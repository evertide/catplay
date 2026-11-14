use crate::{
    proxy_rpc::{OverlayPeerState, OverlayPolicy},
    ui::UiState,
};
use catplay_bt::BluezManager;
use catplay_util::{EventReconciler, EventSleeper, EventToken, LazyAsync};
use log::{debug, warn};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

pub struct OverlayPolicyDefault {
    bluez: LazyAsync<BluezManager>,
    peer: OverlayPeerState,
    overlay: Option<UiState>,
    iphone_bt_links: Arc<AtomicUsize>,
    themed: Arc<OnceLock<bool>>,
}

/// The waiting screen while no iPhone session runs: a pairing request wins, then a found iPhone
/// (a live Bluetooth iAP2 link, kept by iOS until the AirPlay session runs), else still searching.
/// The connecting phase is part of the UI theme; without one the upstream waiting screen stays.
fn waiting_screen(pairing: Option<UiState>, iphone_found: bool, themed: bool) -> UiState {
    match (pairing, iphone_found && themed) {
        (Some(pairing), _) => pairing,
        (None, true) => UiState::Connecting {
            // The Bluetooth link carries no name; the AirPlay name only arrives with the session.
            device: "iPhone".into(),
            // Static dots: the screen is redrawn only when the phase changes.
            ticks: 2,
        },
        (None, false) => UiState::WaitingForConnection,
    }
}

impl OverlayPolicyDefault {
    pub fn new(iphone_bt_links: Arc<AtomicUsize>, themed: Arc<OnceLock<bool>>) -> Self {
        Self {
            bluez: LazyAsync::new(Self::start_pin_agent),
            peer: OverlayPeerState::Unconnected,
            overlay: None,
            iphone_bt_links,
            themed,
        }
    }
    async fn start_pin_agent() -> BluezManager {
        loop {
            let mut mgr = BluezManager::new();
            match mgr.register_pin_agent().await {
                Ok(()) => {
                    if let Err(err) = mgr.set_discoverable("hci0", true).await {
                        warn!("Failed to set BlueZ adapter discoverable: {err}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    if let Err(err) = mgr.set_pairable("hci0", true).await {
                        warn!("Failed to set BlueZ adapter pairable: {err}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return mgr;
                }
                Err(err) => {
                    debug!("BlueZ agent not available: {err}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

impl OverlayPolicy for OverlayPolicyDefault {
    fn overlay(&mut self) -> Option<UiState> {
        if self.peer != OverlayPeerState::Unconnected {
            return None;
        }

        let pairing = self
            .bluez
            .as_ref()
            .as_ref()
            .and_then(|bluez| bluez.get_pairing_request())
            .map(|req| UiState::Pairing {
                device: req.remote_name.unwrap_or("unknown".into()),
                pin: format!("{}", req.passkey),
            });
        let iphone_found = self.iphone_bt_links.load(Ordering::Relaxed) > 0;
        // Until the first screen loaded the theme, treat it as absent.
        let themed = self.themed.get().copied().unwrap_or(false);

        Some(waiting_screen(pairing, iphone_found, themed))
    }

    fn on_hid_interact(&mut self) -> bool {
        if let Some(req) = self
            .bluez
            .as_ref()
            .as_ref()
            .and_then(|bluez| bluez.get_pairing_request())
        {
            req.accept();
            return false;
        }

        true
    }

    fn set_peer_state(&mut self, state: OverlayPeerState) {
        self.peer = state;
    }
}

impl EventReconciler for OverlayPolicyDefault {
    type Error = ();

    async fn reconcile(&mut self) -> Result<(), Self::Error> {
        self.overlay = self.overlay();
        Ok(())
    }
}

impl EventSleeper for OverlayPolicyDefault {
    async fn sleep(&mut self) -> Option<EventToken> {
        self.bluez.sleep().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_screen_follows_the_connection_phase() {
        assert_eq!(waiting_screen(None, false, true), UiState::WaitingForConnection);
        let connecting = waiting_screen(None, true, true);
        assert!(matches!(connecting, UiState::Connecting { .. }), "{connecting}");
        assert_eq!(connecting, waiting_screen(None, true, true), "a phase renders the same state, so it is not redrawn");
        assert_eq!(
            waiting_screen(None, true, false),
            UiState::WaitingForConnection,
            "without a theme the upstream waiting screen stays"
        );

        let pairing = UiState::Pairing {
            device: "iPhone".into(),
            pin: "1234".into(),
        };
        for (found, themed) in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(waiting_screen(Some(pairing.clone()), found, themed), pairing, "pairing wins");
        }
    }

    #[test]
    fn found_iphone_switches_to_connecting_until_the_link_drops() {
        let links = Arc::new(AtomicUsize::new(0));
        let themed = Arc::new(OnceLock::new());
        let mut policy = OverlayPolicyDefault::new(links.clone(), themed.clone());
        assert_eq!(policy.overlay(), Some(UiState::WaitingForConnection));
        links.store(1, Ordering::Relaxed);
        assert_eq!(
            policy.overlay(),
            Some(UiState::WaitingForConnection),
            "the theme is not known before the first screen"
        );
        themed.set(true).unwrap();
        assert!(matches!(policy.overlay(), Some(UiState::Connecting { .. })));
        policy.set_peer_state(OverlayPeerState::ConnectedWithoutScreen);
        assert_eq!(policy.overlay(), None, "the session hides the waiting screen");
        policy.set_peer_state(OverlayPeerState::Unconnected);
        links.store(0, Ordering::Relaxed);
        assert_eq!(policy.overlay(), Some(UiState::WaitingForConnection), "back to searching");
    }
}
