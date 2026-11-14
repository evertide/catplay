use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{LocationBridge, LocationSubscription};
use catplay_csm::{
    decoder::CsmPacketBox,
    msg::{AuthenticationSucceeded, IdentificationAccepted, StartIdentification, StartLocationInformation, StopLocationInformation},
};
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::event_select;
use log::info;

/// Both transports control one iPhone request; only AirPlay delivers GPS data.
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    location: LocationBridge,
    subscription: Option<LocationSubscription>,
    transport: &'static str,
    waiting_for_transport: bool,
    identified: bool,
    authenticated: bool,
    forwarded_location: bool,
    log_first_location: bool,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, location: LocationBridge, airplay: bool) -> CsmSessionBox {
        let transport = if airplay { "AirPlay" } else { "Bluetooth" };
        info!(
            "{transport} GPS bridge created: gps_advertised={}, mfi_present={}",
            identity.has_gps,
            mfi.is_some()
        );
        if !airplay {
            // A new Bluetooth connection begins a new phone session. Its later
            // disconnect must preserve the request for the CarPlay handoff.
            location.set_request(None);
        }
        Box::new(Self {
            session: CarPlayServerSession::new(mfi, identity).0,
            subscription: airplay.then(|| location.subscribe()),
            location,
            transport,
            waiting_for_transport: false,
            identified: false,
            authenticated: false,
            forwarded_location: false,
            log_first_location: false,
        })
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
            self.location.set_request(Some(request.clone()));
            return Ok(());
        }
        if packet.cast::<StopLocationInformation>().is_some() {
            info!("iPhone StopLocationInformation: transport={}", self.transport);
            self.location.set_request(None);
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
        if self.waiting_for_transport || !self.identified || !self.authenticated {
            // Link input/completion still wakes the client, including Stop.
            std::future::pending::<()>().await;
        } else {
            event_select!(self.subscription);
        }
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        self.waiting_for_transport = handle.has_pending_send();
        if self.log_first_location && !self.waiting_for_transport {
            // On AirPlay the transmit permit lasts until the successful RTSP reply.
            info!("GPS first LocationInformation sent to iPhone: transport={}", self.transport);
            self.log_first_location = false;
        }
        if !self.waiting_for_transport
            && self.identified
            && self.authenticated
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
