use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlayServerSessionEventRx, CarPlayServerSessionEventTx, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{LocationBridge, LocationSubscription};
use catplay_csm::{
    decoder::CsmPacketBox,
    msg::{AuthenticationSucceeded, IdentificationAccepted, StartIdentification},
};
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::{event_select, mpsc};
use futures::{FutureExt, StreamExt};
use log::info;

/// Forwards GPS over the AirPlay iAP2 session that lasts for the whole drive.
/// Bluetooth is used only for the existing pairing/Wi-Fi setup.
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    events: mpsc::UnboundedReceiver<CarPlayServerSessionEventTx>,
    location: LocationBridge,
    subscription: Option<LocationSubscription>,
    waiting_for_transport: bool,
    identified: bool,
    authenticated: bool,
    received_start: bool,
    forwarded_location: bool,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, location: LocationBridge) -> CsmSessionBox {
        info!(
            "AirPlay GPS bridge created: gps_advertised={}, mfi_present={}",
            identity.has_gps,
            mfi.is_some()
        );
        let (session, events) = CarPlayServerSession::new(mfi, identity);
        Box::new(Self {
            session,
            events,
            location,
            subscription: None,
            waiting_for_transport: false,
            identified: false,
            authenticated: false,
            received_start: false,
            forwarded_location: false,
        })
    }
}

#[async_trait]
impl CsmSession for Iap2LocationBridge {
    async fn start(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        info!("AirPlay GPS iAP2 link ready");
        self.session.start(handle).await
    }

    async fn respond(&mut self, packet: CsmPacketBox, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        if packet.cast::<StartIdentification>().is_some() {
            self.identified = false;
            self.authenticated = false;
        }
        if packet.cast::<IdentificationAccepted>().is_some() && !self.identified {
            self.identified = true;
            info!("AirPlay GPS identification complete");
        }
        if packet.cast::<AuthenticationSucceeded>().is_some() && !self.authenticated {
            self.authenticated = true;
            info!(
                "AirPlay GPS authentication complete: iPhone_location_requested={}",
                self.received_start
            );
        }
        self.session.respond(packet, handle).await
    }

    async fn on_file_event(&mut self, id: u8, event: CsmFileTransferEvent, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.on_file_event(id, event, handle).await
    }

    async fn wait_event(&mut self) {
        if self.waiting_for_transport {
            // The client wakes on transmit completion. Keep accepting Stop/replacement
            // requests without spinning on accumulated location updates meanwhile.
            event_select!(self.events);
        } else {
            event_select!(self.events, self.subscription);
        }
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        while let Some(Some(event)) = self.events.next().now_or_never() {
            match event {
                CarPlayServerSessionEventTx::StartLocationInformation(request) => {
                    if !self.received_start {
                        info!("AirPlay GPS first StartLocationInformation from iPhone: {request:?}");
                        self.received_start = true;
                    }
                    self.subscription = Some(self.location.subscribe(request));
                }
                CarPlayServerSessionEventTx::StopLocationInformat(_) => self.subscription = None,
                _ => {}
            }
            consumed = true;
        }
        self.waiting_for_transport = handle.has_pending_send();
        if !self.waiting_for_transport
            && let Some(update) = self
                .subscription
                .as_mut()
                .and_then(|subscription| subscription.take(handle.max_csm_payload_len()))
        {
            self.session
                .on_event(CarPlayServerSessionEventRx::FeedGpsNmea(update), handle.clone())?;
            self.waiting_for_transport = handle.has_pending_send();
            if !self.forwarded_location {
                info!("AirPlay GPS first LocationInformation forwarded to iPhone");
                self.forwarded_location = true;
            }
            consumed = true;
        }
        Ok(consumed)
    }
}

#[cfg(test)]
mod tests;
