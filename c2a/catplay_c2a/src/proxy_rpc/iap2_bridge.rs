use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{LocationBridge, LocationSubscription};
use catplay_csm::{
    decoder::CsmPacketBox,
    msg::{AuthenticationSucceeded, IdentificationAccepted, StartIdentification, StartLocationInformation, StopLocationInformation},
};
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::EventSleeper;
use log::info;

/// The iAP2 session on the AirPlay link: offers Location to the iPhone and passes the car's GPS on to it.
///
/// Location is offered on this link only, never in the Bluetooth identification. iOS binds Location to the first iAP2
/// endpoint that offers it; the Bluetooth endpoint is gone after disableBluetooth, and LocationInformation arriving on
/// another endpoint is then rejected by the iPhone.
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    subscription: LocationSubscription,
    waiting_for_transport: bool,
    identified: bool,
    authenticated: bool,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, location: &LocationBridge) -> CsmSessionBox {
        Box::new(Self {
            session: CarPlayServerSession::new(mfi, CarPlaySessionIdentity { has_gps: true, ..identity }).0,
            subscription: location.subscribe(),
            waiting_for_transport: false,
            identified: false,
            authenticated: false,
        })
    }
}

#[async_trait]
impl CsmSession for Iap2LocationBridge {
    async fn start(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.start(handle).await
    }

    async fn respond(&mut self, packet: CsmPacketBox, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        if let Some(request) = packet.cast::<StartLocationInformation>() {
            info!("iPhone requested location: {request:?}");
            self.subscription.set_request(Some(request.clone()));
            return Ok(());
        }
        if packet.cast::<StopLocationInformation>().is_some() {
            info!("iPhone stopped location");
            self.subscription.set_request(None);
            return Ok(());
        }
        if packet.cast::<StartIdentification>().is_some() {
            self.identified = false;
            self.authenticated = false;
        }
        self.identified |= packet.cast::<IdentificationAccepted>().is_some();
        self.authenticated |= packet.cast::<AuthenticationSucceeded>().is_some();
        self.session.respond(packet, handle).await
    }

    async fn on_file_event(&mut self, id: u8, event: CsmFileTransferEvent, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.on_file_event(id, event, handle).await
    }

    async fn wait_event(&mut self) {
        // While the transport is busy or the link is not ready, link input and send completion wake the client.
        if self.waiting_for_transport || !self.identified || !self.authenticated || self.subscription.sleep().await.is_none() {
            std::future::pending::<()>().await;
        }
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        // One message at a time: on AirPlay the transmit permit lasts until the RTSP reply, so a slow iPhone slows us down
        // and the subscription keeps only the latest fix per sentence type meanwhile.
        self.waiting_for_transport = handle.has_pending_send();
        if !self.waiting_for_transport
            && self.identified
            && self.authenticated
            && let Some(update) = self.subscription.take(handle.max_csm_payload_len())
        {
            handle.send(&update)?;
            self.waiting_for_transport = handle.has_pending_send();
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
