use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlayServerSessionEventRx, CarPlayServerSessionEventTx, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{CarPlayLocationControl, CarPlayLocationSession, LocationRequest};
use catplay_csm::{decoder::CsmPacketBox, msg::LocationInformation};
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::{event_select, mpsc};
use futures::{FutureExt, StreamExt};

/// Owns one iPhone iAP2 session and its channels to one car iAP2 session.
/// Dropping it closes requests, causing the car to stop location updates.
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    events: mpsc::UnboundedReceiver<CarPlayServerSessionEventTx>,
    car: CarPlayLocationControl,
    peer: Option<CarPlayLocationSession>,
    requests: mpsc::UnboundedSender<LocationRequest>,
    updates: mpsc::UnboundedReceiver<LocationInformation>,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, car: CarPlayLocationControl) -> CsmSessionBox {
        let (session, _feeds, events) = CarPlayServerSession::new(mfi, identity);
        let (peer, requests, updates) = CarPlayLocationSession::new();
        Box::new(Self {
            session,
            events,
            car,
            peer: Some(peer),
            requests,
            updates,
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
        event_select!(self.events, self.updates);
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        while let Some(Some(event)) = self.events.next().now_or_never() {
            match event {
                CarPlayServerSessionEventTx::StartLocationInformation(request) => {
                    // Bluetooth and AirPlay can overlap during handover. An idle
                    // session must not replace one that is actively requesting GPS.
                    if self.requests.is_closed() {
                        let (peer, requests, updates) = CarPlayLocationSession::new();
                        self.peer = Some(peer);
                        self.requests = requests;
                        self.updates = updates;
                    }
                    if let Some(peer) = self.peer.take() {
                        self.car.connect(peer);
                    }
                    let _ = self.requests.unbounded_send(LocationRequest::Start(request));
                }
                CarPlayServerSessionEventTx::StopLocationInformat(_) => {
                    let _ = self.requests.unbounded_send(LocationRequest::Stop);
                }
                _ => {}
            }
            consumed = true;
        }
        if let Some(update) = self.updates.take() {
            // Drop queued data if the car has disconnected. The server's gps_sub
            // check also rejects data queued before the iPhone's Stop request.
            if self.car.is_available() {
                self.session
                    .on_event(CarPlayServerSessionEventRx::FeedGpsNmea(update), handle)?;
            }
            consumed = true;
        }
        Ok(consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_carplay_tx_gadget::carplay_client_session::CarPlayClientSession;
    use catplay_csm::{
        decoder::{AsCsmPacket, CsmFlag, CsmPacketId, CsmPacketRegistry},
        msg::{
            AuthenticationResponse, IdentificationInformation, LocationInformationComponent, StartLocationInformation,
            StartPowerUpdates, StopLocationInformation,
        },
    };
    use catplay_iap2_client::{CsmClientHandle, CsmRemote, CsmSessionStatus};
    use futures::executor::block_on;
    use std::sync::{Arc, Mutex};

    struct Capture {
        remote: CsmRemote,
        packets: Mutex<Vec<CsmPacketBox>>,
    }

    impl Capture {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                remote: CsmRemote::airplay(),
                packets: Mutex::new(Vec::new()),
            })
        }

        fn take(&self) -> Vec<CsmPacketBox> {
            std::mem::take(&mut *self.packets.lock().unwrap())
        }
    }

    impl CsmClientHandle for Capture {
        fn disconnect(&self) {}
        fn is_server(&self) -> bool {
            false
        }
        fn is_closed(&self) -> bool {
            false
        }
        fn is_downgrade(&self) -> bool {
            false
        }
        fn is_writable(&self) -> bool {
            true
        }
        fn remote(&self) -> &CsmRemote {
            &self.remote
        }
        fn status(&self) -> CsmSessionStatus {
            CsmSessionStatus::Writable
        }
        fn send(&self, packet: &dyn AsCsmPacket) -> CsmSessionResult<()> {
            let registry = CsmPacketRegistry::static_registry();
            let bytes = registry.encode(packet).unwrap();
            self.packets.lock().unwrap().push(registry.decode(&bytes).unwrap());
            Ok(())
        }
        fn send_all(&self, packets: &[CsmPacketBox]) -> CsmSessionResult<()> {
            for packet in packets {
                self.send(packet)?;
            }
            Ok(())
        }
        fn send_file_reserve(&self) -> Option<u8> {
            None
        }
        fn send_file(&self, _: u8, _: u16, _: &[u8], _: Vec<u8>) {
            panic!("GPS must not use FileTransfer");
        }
    }

    fn respond(session: &mut dyn CsmSession, handle: &Arc<Capture>, packet: impl Into<CsmPacketBox>) {
        block_on(session.respond(packet.into(), handle.clone())).unwrap();
    }

    fn pump(session: &mut dyn CsmSession, handle: &Arc<Capture>) {
        session.wait_event().now_or_never().expect("queued bridge event must wake the session");
        assert!(session.process_event(handle.clone()).unwrap());
    }

    fn ready(car: &mut CarPlayClientSession, handle: &Arc<Capture>) {
        block_on(car.start(handle.clone())).unwrap();
        respond(car, handle, IdentificationInformation {
            location_information_component: Some(LocationInformationComponent {
                recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
                ..LocationInformationComponent::default()
            }),
            messages_sent_by_accessory: IdentificationInformation::pack_ids(&[StartPowerUpdates::PACKET_ID]),
            ..IdentificationInformation::default()
        });
        respond(car, handle, AuthenticationResponse::default());
        respond(car, handle, StartPowerUpdates::default());
    }

    fn phone(car: CarPlayLocationControl) -> CsmSessionBox {
        Iap2LocationBridge::new(None, CarPlaySessionIdentity {
            has_gps: true,
            ..CarPlaySessionIdentity::default()
        }, car)
    }

    #[test]
    fn bridge_defers_early_start_then_forwards_and_stops_on_phone_teardown() {
        let (mut car, control) = CarPlayClientSession::new();
        let mut phone = phone(control);
        let car_handle = Capture::new();
        let phone_handle = Capture::new();
        // An idle phone has not even attached a subscription to the car.
        assert!(car.wait_event().now_or_never().is_none());
        respond(&mut phone, &phone_handle, StartLocationInformation::all());
        pump(&mut phone, &phone_handle);
        pump(&mut car, &car_handle); // Attach and receive the request before the link starts.
        assert!(car_handle.take().is_empty());
        ready(&mut car, &car_handle);
        let packets = car_handle.take();
        assert!(packets.last().unwrap().cast::<StartLocationInformation>().is_some());

        let update = LocationInformation { nmea_sentence: vec!["$GPRMC,120000,V,,,,,,,131126,,,N*00\r\n".into()] };
        respond(&mut car, &car_handle, update.clone());
        pump(&mut phone, &phone_handle);
        let packets = phone_handle.take();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].cast::<LocationInformation>(), Some(&update));

        respond(&mut phone, &phone_handle, StopLocationInformation::default());
        pump(&mut phone, &phone_handle);
        pump(&mut car, &car_handle);
        let packets = car_handle.take();
        assert_eq!(packets.len(), 1);
        assert!(packets[0].cast::<StopLocationInformation>().is_some());
        respond(&mut phone, &phone_handle, StartLocationInformation::all());
        pump(&mut phone, &phone_handle);
        pump(&mut car, &car_handle);
        car_handle.take();
        drop(phone);
        pump(&mut car, &car_handle);
        let packets = car_handle.take();
        assert_eq!(packets.len(), 1);
        assert!(packets[0].cast::<StopLocationInformation>().is_some());
    }

    #[test]
    fn transport_handover_ignores_old_stop_and_car_teardown_discards_queued_data() {
        let (mut car, control) = CarPlayClientSession::new();
        let car_handle = Capture::new();
        ready(&mut car, &car_handle);
        car_handle.take();
        let mut bluetooth = phone(control.clone());
        let phone_handle = Capture::new();
        respond(&mut bluetooth, &phone_handle, StartLocationInformation::all());
        pump(&mut bluetooth, &phone_handle);
        pump(&mut car, &car_handle);
        car_handle.take();

        let mut airplay = phone(control);
        assert!(car.wait_event().now_or_never().is_none());
        respond(&mut airplay, &phone_handle, StartLocationInformation::all());
        pump(&mut airplay, &phone_handle);
        pump(&mut car, &car_handle);
        let packets = car_handle.take();
        assert_eq!(packets.len(), 2);
        assert!(packets[0].cast::<StopLocationInformation>().is_some());
        assert!(packets[1].cast::<StartLocationInformation>().is_some());

        respond(&mut bluetooth, &phone_handle, StopLocationInformation::default());
        pump(&mut bluetooth, &phone_handle);
        drop(bluetooth);
        assert!(car.wait_event().now_or_never().is_none());
        respond(&mut car, &car_handle, LocationInformation { nmea_sentence: vec!["$PASCD,0*00\r\n".into()] });
        drop(car);
        pump(&mut airplay, &phone_handle);
        assert!(phone_handle.take().is_empty());
        let packets = car_handle.take();
        assert_eq!(packets.len(), 1);
        assert!(packets[0].cast::<StopLocationInformation>().is_some());
    }
}
