use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlayServerSessionEventRx, CarPlayServerSessionEventTx, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{LocationBridge, LocationSubscription};
use catplay_csm::decoder::CsmPacketBox;
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::{event_select, mpsc};
use futures::{FutureExt, StreamExt};

/// Forwards GPS over the AirPlay iAP2 session that lasts for the whole drive.
/// Bluetooth is used only for the existing pairing/Wi-Fi setup.
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    events: mpsc::UnboundedReceiver<CarPlayServerSessionEventTx>,
    location: LocationBridge,
    subscription: Option<LocationSubscription>,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, location: LocationBridge) -> CsmSessionBox {
        let (session, events) = CarPlayServerSession::new(mfi, identity);
        Box::new(Self {
            session,
            events,
            location,
            subscription: None,
        })
    }
}

#[async_trait]
impl CsmSession for Iap2LocationBridge {
    async fn start(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.start(handle).await
    }

    async fn respond(&mut self, packet: CsmPacketBox, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.respond(packet, handle).await
    }

    async fn on_file_event(&mut self, id: u8, event: CsmFileTransferEvent, handle: CsmClientHandleRef) -> CsmSessionResult<()> {
        self.session.on_file_event(id, event, handle).await
    }

    async fn wait_event(&mut self) {
        event_select!(self.events, self.subscription);
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        while let Some(Some(event)) = self.events.next().now_or_never() {
            match event {
                CarPlayServerSessionEventTx::StartLocationInformation(request) => {
                    self.subscription = Some(self.location.subscribe(request));
                }
                CarPlayServerSessionEventTx::StopLocationInformat(_) => self.subscription = None,
                _ => {}
            }
            consumed = true;
        }
        if let Some(update) = self
            .subscription
            .as_mut()
            .and_then(LocationSubscription::take)
        {
            self.session
                .on_event(CarPlayServerSessionEventRx::FeedGpsNmea(update), handle)?;
            consumed = true;
        }
        Ok(consumed)
    }
}

#[cfg(test)]
mod tests;
