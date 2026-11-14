use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{LocationBridge, LocationSubscription};
use catplay_csm::{
    decoder::CsmPacketBox,
    msg::{AuthenticationSucceeded, IdentificationAccepted, StartIdentification, StartLocationInformation, StopLocationInformation},
};
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionError, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::event_select;
use log::info;
use std::sync::atomic::{AtomicU32, Ordering};

/// XC90 diagnostics (`xc90_bt_fast_close`): bumped on the iPhone's disableBluetooth; Bluetooth bridges created before end their
/// iAP2 link right away instead of waiting for BlueZ to drop it (~2 s, overlapping the AirPlay iAP2 identification).
static BT_CLOSE_GENERATION: AtomicU32 = AtomicU32::new(0);
static BT_CLOSE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Ask every current Bluetooth iAP2 bridge to end its link (XC90 diagnostics).
pub fn request_bt_close() {
    BT_CLOSE_GENERATION.fetch_add(1, Ordering::SeqCst);
    BT_CLOSE.notify_waiters();
}

/// Both transports control one iPhone request; only AirPlay delivers GPS data.
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    location: LocationBridge,
    subscription: Option<LocationSubscription>,
    owner: u64,
    transport: &'static str,
    waiting_for_transport: bool,
    identified: bool,
    authenticated: bool,
    forwarded_location: bool,
    log_first_location: bool,
    /// XC90 diagnostics (`xc90_gps_bt_location=0`): only send GPS after the iPhone asked on this link.
    own_request_only: bool,
    requested_here: bool,
    /// Bluetooth only: [BT_CLOSE_GENERATION] at creation.
    bt_close_generation: Option<u32>,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, location: LocationBridge, airplay: bool) -> CsmSessionBox {
        Self::new_gated(mfi, identity, location, airplay, false)
    }

    /// Like [Self::new]; with `own_request_only` GPS goes out only after a StartLocationInformation on this same link.
    pub fn new_gated(
        mfi: Option<MfiDeficeRef>,
        identity: CarPlaySessionIdentity,
        location: LocationBridge,
        airplay: bool,
        own_request_only: bool,
    ) -> CsmSessionBox {
        let transport = if airplay { "AirPlay" } else { "Bluetooth" };
        info!(
            "{transport} GPS bridge created: gps_advertised={}, mfi_present={}",
            identity.has_gps,
            mfi.is_some()
        );
        Box::new(Self {
            session: CarPlayServerSession::new(mfi, identity).0,
            subscription: airplay.then(|| location.subscribe()),
            owner: if airplay { 0 } else { location.new_owner() },
            location,
            transport,
            waiting_for_transport: false,
            identified: false,
            authenticated: false,
            forwarded_location: false,
            log_first_location: false,
            own_request_only,
            requested_here: false,
            bt_close_generation: (!airplay).then(|| BT_CLOSE_GENERATION.load(Ordering::SeqCst)),
        })
    }

    fn set_request(&self, request: Option<StartLocationInformation>) {
        if let Some(subscription) = &self.subscription {
            subscription.set_request(request);
        } else {
            self.location.set_request(self.owner, request);
        }
    }
}

#[async_trait]
impl CsmSession for Iap2LocationBridge {
    async fn start(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        info!("{} GPS iAP2 link ready", self.transport);
        self.session.start(handle).await
    }

    async fn respond(&mut self, packet: CsmPacketBox, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        // Handle these directly, without the server session's separate GPS gate
        // or an event-channel round trip back into this adapter.
        if let Some(request) = packet.cast::<StartLocationInformation>() {
            info!("iPhone StartLocationInformation: transport={}, request={request:?}", self.transport);
            self.requested_here = true;
            self.set_request(Some(request.clone()));
            return Ok(());
        }
        if packet.cast::<StopLocationInformation>().is_some() {
            info!("iPhone StopLocationInformation: transport={}", self.transport);
            self.requested_here = false;
            self.set_request(None);
            return Ok(());
        }
        if packet.cast::<StartIdentification>().is_some() {
            self.identified = false;
            self.authenticated = false;
        }
        if packet.cast::<IdentificationAccepted>().is_some() && !self.identified {
            self.identified = true;
            info!("{} GPS identification complete", self.transport);
        }
        if packet.cast::<AuthenticationSucceeded>().is_some() && !self.authenticated {
            self.authenticated = true;
            info!(
                "{} GPS authentication complete: iPhone_location_requested={}",
                self.transport,
                self.location.is_requested()
            );
        }
        self.session.respond(packet, handle).await
    }

    async fn on_file_event(&mut self, id: u8, event: CsmFileTransferEvent, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.on_file_event(id, event, handle).await
    }

    async fn wait_event(&mut self) {
        if let Some(generation) = self.bt_close_generation {
            // Bluetooth carries no GPS data; only wake up when disableBluetooth asks to end the link.
            loop {
                // Created before the check, so a close request in between still wakes it.
                let closed = BT_CLOSE.notified();
                if BT_CLOSE_GENERATION.load(Ordering::SeqCst) != generation {
                    return;
                }
                closed.await;
            }
        }
        if self.waiting_for_transport || !self.identified || !self.authenticated {
            // Link input/completion still wakes the client, including Stop.
            std::future::pending::<()>().await;
        } else {
            event_select!(self.subscription);
        }
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        if self
            .bt_close_generation
            .is_some_and(|generation| BT_CLOSE_GENERATION.load(Ordering::SeqCst) != generation)
        {
            info!("XC90: ending the Bluetooth iAP2 link after disableBluetooth");
            return Err(CsmSessionError::Session("disableBluetooth".into()));
        }
        self.waiting_for_transport = handle.has_pending_send();
        if self.log_first_location && !self.waiting_for_transport {
            // On AirPlay the transmit permit lasts until the successful RTSP reply.
            info!("GPS first LocationInformation sent to iPhone: transport={}", self.transport);
            self.log_first_location = false;
        }
        if !self.waiting_for_transport
            && self.identified
            && self.authenticated
            && (!self.own_request_only || self.requested_here)
            && let Some(update) = self
                .subscription
                .as_mut()
                .and_then(|subscription| subscription.take(handle.max_csm_payload_len()))
        {
            handle.send(&update)?;
            self.waiting_for_transport = handle.has_pending_send();
            if !self.forwarded_location {
                self.forwarded_location = true;
                self.log_first_location = true;
            }
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
