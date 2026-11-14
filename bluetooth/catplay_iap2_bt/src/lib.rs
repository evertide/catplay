use std::{io, str::FromStr};
use std::{net::TcpStream, sync::Mutex};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use catplay_bt::{BluezError, BluezManager, CARPLAY_EIR_ACCESSORY, CARPLAY_EIR_PHONE, CARPLAY_HCI_CLASS_ACCESSORY_MGMT_BYTES, HciSocket};
use catplay_iap2_client::{
    CsmRemote, CsmSession, CsmSessionCallback,
    tokio::{AsyncClient, AsyncClientStream},
};
use catplay_util::{EventReconciler, event_select, sleep};
use tokio::{runtime::Handle, task::AbortHandle};
use log::{error, info, warn};
use macaddr::MacAddr6;

#[derive(thiserror::Error, Debug)]
pub enum BluetoothError {
    #[error("Invalid adapter name: {0}")]
    InvalidAdapter(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("BlueZ error: {0}")]
    Bluez(#[from] BluezError),
}

pub type BluetoothResult<T> = Result<T, BluetoothError>;

pub struct BluetoothManager {
    hcihacks: bool,
    carplay: bool,
    server: bool,

    enabled: bool,
    mac_addr: Option<String>,
    adapter: String,
    bluez: Option<BluezManager>,
    csm: CsmSessionCallback,

    last_connect: Arc<Mutex<Option<MacAddr6>>>,
    links: Arc<LinkTasks>,
}

/// The iAP2 session tasks of a manager: counts the live ones in a shareable counter, and aborts
/// them when the manager is dropped, so no session (and no count) outlives a Bluetooth restart.
struct LinkTasks {
    live: Arc<AtomicUsize>,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl LinkTasks {
    fn new(live: Arc<AtomicUsize>) -> Self {
        Self {
            live,
            tasks: Default::default(),
        }
    }

    fn spawn(&self, handle: &Handle, session: impl Future<Output = ()> + Send + 'static) {
        let live_link = LiveLink::new(self.live.clone());
        let task = handle.spawn(async move {
            let _live_link = live_link;
            session.await
        });
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task.abort_handle());
    }
}

impl Drop for LinkTasks {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap().drain(..) {
            task.abort();
        }
    }
}

/// Counts one live iAP2 link for as long as its session task runs.
struct LiveLink(Arc<AtomicUsize>);

impl LiveLink {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for LiveLink {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl BluetoothManager {
    pub fn new<T: CsmSession, F: Fn() -> T + Send + Sync + 'static>(
        server: bool,
        hcihacks: bool,
        carplay: bool,
        adapter: &str,
        csm: F,
    ) -> Self {
        let csm: CsmSessionCallback = Arc::new(move || Box::new(csm()));

        Self {
            server,
            hcihacks,
            carplay,

            enabled: false,
            mac_addr: None,
            adapter: adapter.into(),
            bluez: None,
            csm,
            last_connect: Default::default(),
            links: Arc::new(LinkTasks::new(Default::default())),
        }
    }

    /// Counts live iAP2 links in `counter` instead of a private one, so the count can be shared
    /// before the manager starts and stays valid across manager restarts.
    pub fn with_live_links(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.links = Arc::new(LinkTasks::new(counter));
        self
    }

    /// Sets Powered = true on Bluetooth adapter.
    pub async fn start_power(adapter: &str, alias: &str) -> BluetoothResult<()> {
        let bluez = BluezManager::new();
        bluez.set_alias(adapter, alias).await?;
        bluez.set_powered(adapter, true).await?;
        Ok(())
    }

    /// Invites trusted iPhone to connect to iAP2 profile.
    pub async fn invite_iphone(adapter: &str, iphone_bt_mac: &MacAddr6) -> BluetoothResult<()> {
        let bluez = BluezManager::new();
        bluez
            .iap2_connect(adapter, &iphone_bt_mac.to_string())
            .await?;
        Ok(())
    }

    /// Query local MAC address of Bluetooth device.
    ///
    /// Early in the boot process this function may fail, and can be externally retried again in intervals.
    pub async fn query_mac(adapter: &str) -> BluetoothResult<MacAddr6> {
        let bluez = BluezManager::new();
        let addr = bluez.get_address(adapter).await?;
        let mac = MacAddr6::from_str(&addr).map_err(|_| io::Error::other("invalid MAC address"))?;
        Ok(mac)
    }

    /// Starts Bluetooth iAP2 server - requires BlueZ to be running and given HCI device to be active.
    ///
    /// Early in the boot process this function may fail, and can be externally retried again in intervals.
    pub async fn start(&mut self) -> BluetoothResult<()> {
        let bluez = BluezManager::new();
        let adapter = &self.adapter;

        fn parse_hci_number(iface: &str) -> Option<u16> {
            let hci = iface.strip_prefix("hci")?;
            hci.parse::<u16>().ok()
        }

        let adapter_index = match parse_hci_number(&self.adapter) {
            Some(num) => num,
            None => {
                return Err(BluetoothError::InvalidAdapter(self.adapter.clone()));
            }
        };

        info!("Querying Bluez for adapters...");
        let addr = bluez.get_address(adapter).await?;
        info!("Found BT addr = {} at adapter {}", addr, adapter);

        self.mac_addr.replace(addr);
        self.enabled = true;
        self.bluez.replace(bluez);

        // TODO: detect BlueZ restart and re-register
        self.start_iap2().await?;

        const HCIHACK_ATTEMPTS: usize = 40;
        const HCIHACKS_DELAY: Duration = Duration::from_millis(50);

        for attempt in 1..=HCIHACK_ATTEMPTS {
            // Sometimes we get MGMT status "Busy" while bluetoothd is still settling.
            match self.do_hcihacks(adapter_index).await {
                Ok(()) => {
                    if self.hcihacks && self.carplay {
                        if attempt == 1 {
                            info!("Applied CarPlay HCI tweaks");
                        } else {
                            info!("Applied CarPlay HCI tweaks after {} attempts", attempt);
                        }
                    }
                    return Ok(());
                }
                Err(_) if attempt < HCIHACK_ATTEMPTS => {
                    sleep(HCIHACKS_DELAY).await;
                }
                Err(_err) => {
                    return Ok(());
                    // return Err(err.into());
                }
            }
        }

        Ok(())
    }

    async fn do_hcihacks(&self, adapter_index: u16) -> io::Result<()> {
        let hci = HciSocket::new(adapter_index)?;

        if self.hcihacks && self.carplay && !self.server {
            hci.add_uuid(CARPLAY_EIR_ACCESSORY)
                .map_err(|err| io::Error::new(err.kind(), format!("failed to register CarPlay BT Accessory EIR: {}", err)))?;
        }
        if self.hcihacks && self.carplay && self.server {
            hci.add_uuid(CARPLAY_EIR_PHONE)
                .map_err(|err| io::Error::new(err.kind(), format!("failed to register CarPlay BT iPhone EIR: {}", err)))?;
        }

        if self.hcihacks && self.carplay && !self.server {
            hci.set_class(CARPLAY_HCI_CLASS_ACCESSORY_MGMT_BYTES)
                .map_err(|err| io::Error::new(err.kind(), format!("failed to update HCI class to match CarPlay receiver: {}", err)))?;
        }

        Ok(())
    }

    pub fn get_last_connected(&mut self) -> Option<MacAddr6> {
        *self.last_connect.lock().unwrap()
    }

    async fn start_iap2(&mut self) -> BluetoothResult<()> {
        let handle = Handle::current();
        let csm = self.csm.clone();
        let server = self.server;

        let last_connect = self.last_connect.clone();
        let links = Arc::downgrade(&self.links);
        self.bluez
            .as_mut()
            .unwrap()
            .register_iap2(server, move |conn| {
                info!("New Bluetooth iAP2 conn from peer: {}", conn.peer);

                last_connect.lock().unwrap().replace(conn.peer);
                let socket = tokio::net::TcpStream::from_std(TcpStream::from(conn.socket));
                let session = (csm)();

                match socket {
                    Err(err) => {
                        error!("Failed to wrap Bluetooth socket: {}", err);
                    }
                    Ok(socket) => {
                        let Some(links) = links.upgrade() else {
                            warn!("Dropping Bluetooth iAP2 conn from {}: the manager is gone", conn.peer);
                            return;
                        };
                        links.spawn(&handle, async move {
                            let task = AsyncClient::new(server, CsmRemote::bluetooth(conn.peer, conn.local), session);
                            let mut t = AsyncClientStream::new(task.0, task.1, socket).end_on_eof();

                            loop {
                                if let Err(err) = t.reconcile().await {
                                    warn!("Reached final/error state for Bluetooth iAP2 session with {}: {}", conn.peer, err);
                                    return;
                                }

                                event_select!(t);
                            }
                        });
                    }
                }
            })
            .await?;

        info!("Started iAP2 Bluetooth server");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_link_counts_while_held() {
        let counter = Arc::new(AtomicUsize::new(0));
        let first = LiveLink::new(counter.clone());
        let second = LiveLink::new(counter.clone());
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        drop(first);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        drop(second);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn session_tasks_are_counted_until_they_end_or_the_manager_drops() {
        let counter = Arc::new(AtomicUsize::new(0));
        let links = LinkTasks::new(counter.clone());
        let handle = Handle::current();

        let (end, ended) = futures::channel::oneshot::channel::<()>();
        links.spawn(&handle, async move {
            let _ = ended.await;
        });
        links.spawn(&handle, std::future::pending());
        assert_eq!(counter.load(Ordering::Relaxed), 2, "counted from the spawn on");

        end.send(()).unwrap();
        wait_for(&counter, 1).await;
        drop(links);
        wait_for(&counter, 0).await;
    }

    async fn wait_for(counter: &AtomicUsize, expected: usize) {
        for _ in 0..100 {
            if counter.load(Ordering::Relaxed) == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("live links stayed at {}, expected {expected}", counter.load(Ordering::Relaxed));
    }
}
