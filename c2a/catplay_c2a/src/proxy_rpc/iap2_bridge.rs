use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlayServerSessionEventRx, CarPlayServerSessionEventTx, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::location::{CarPlayLocationControl, CarPlayLocationSession, LocationRequest};
use catplay_csm::{
    decoder::CsmPacketBox,
    msg::{LocationInformation, StartLocationInformation},
};
use catplay_iap2_client::{CsmClientHandleRef, CsmFileTransferEvent, CsmSession, CsmSessionBox, CsmSessionResult};
use catplay_mfi::MfiDeficeRef;
use catplay_util::{EventSleeper, EventToken, event_select, filling_slot_value, mpsc};
use futures::{FutureExt, StreamExt};
use log::warn;

/// Shared across USB iAP2 recreations. Each phone transport has its own wakeup
/// channel, so a replacement wakes every live subscriber, including idle Bluetooth.
#[derive(Clone, Default)]
pub struct CarLocationSource(Arc<Mutex<CarLocationSourceState>>);

#[derive(Default)]
struct CarLocationSourceState {
    generation: u64,
    current: Option<CarPlayLocationControl>,
    listeners: Vec<mpsc::UnboundedSender<()>>,
}

impl CarLocationSource {
    pub fn set(&self, car: CarPlayLocationControl) {
        let mut state = self.0.lock().unwrap();
        state.generation += 1;
        state.current = Some(car);
        state.listeners.retain(|listener| listener.unbounded_send(()).is_ok());
    }

    fn current(&self) -> Option<(u64, CarPlayLocationControl)> {
        let state = self.0.lock().unwrap();
        state.current.clone().map(|car| (state.generation, car))
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded();
        let mut state = self.0.lock().unwrap();
        state.listeners.retain(|listener| !listener.is_closed());
        state.listeners.push(tx);
        rx
    }
}

struct LocationConnection {
    generation: u64,
    car: CarPlayLocationControl,
    requests: mpsc::UnboundedSender<LocationRequest>,
    updates: mpsc::UnboundedReceiver<LocationInformation>,
    pending_update: Option<Option<LocationInformation>>,
    sent: StartLocationInformation,
}

impl EventSleeper for LocationConnection {
    async fn sleep(&mut self) -> Option<EventToken> {
        // Cache EOF too: it must wake the bridge even when no data was queued.
        filling_slot_value(&mut self.pending_update, self.updates.next())
            .sleep()
            .await
    }
}

/// Owns one iPhone iAP2 session and follows replacements of the car iAP2 session.
/// Dropping it closes requests, causing the car to stop (or resume another subscriber).
pub struct Iap2LocationBridge {
    session: CarPlayServerSession,
    events: mpsc::UnboundedReceiver<CarPlayServerSessionEventTx>,
    source: CarLocationSource,
    changes: mpsc::UnboundedReceiver<()>,
    requested: Option<StartLocationInformation>,
    connection: Option<LocationConnection>,
    failed_generation: Option<u64>,
}

impl Iap2LocationBridge {
    pub fn new(mfi: Option<MfiDeficeRef>, identity: CarPlaySessionIdentity, source: CarLocationSource) -> CsmSessionBox {
        let (session, _feeds, events) = CarPlayServerSession::new(mfi, identity);
        let changes = source.subscribe();
        Box::new(Self {
            session,
            events,
            source,
            changes,
            requested: None,
            connection: None,
            failed_generation: None,
        })
    }

    fn sync_connection(&mut self) -> bool {
        let Some(request) = self.requested.as_ref() else {
            if let Some(connection) = self.connection.take() {
                let _ = connection.requests.unbounded_send(LocationRequest::Stop);
                return true;
            }
            return false;
        };
        let Some((generation, car)) = self.source.current() else {
            return false;
        };
        if self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.generation != generation)
        {
            self.connection = None;
        }
        if let Some(connection) = self.connection.as_mut() {
            if connection.sent == *request {
                return false;
            }
            if connection
                .requests
                .unbounded_send(LocationRequest::Start(request.clone()))
                .is_ok()
            {
                connection.sent = request.clone();
                return true;
            }
        } else if self.failed_generation != Some(generation) {
            let (peer, requests, updates) = CarPlayLocationSession::new();
            if car.connect(peer) && requests.unbounded_send(LocationRequest::Start(request.clone())).is_ok() {
                self.connection = Some(LocationConnection {
                    generation,
                    car,
                    requests,
                    updates,
                    pending_update: None,
                    sent: request.clone(),
                });
                return true;
            }
        } else {
            return false;
        }

        warn!("Could not attach GPS subscription to car iAP2 session {generation}; waiting for a replacement");
        self.connection = None;
        self.failed_generation = Some(generation);
        true
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
        event_select!(self.events, self.changes, self.connection);
    }

    fn process_event(&mut self, handle: CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        while let Some(Some(event)) = self.events.next().now_or_never() {
            match event {
                CarPlayServerSessionEventTx::StartLocationInformation(request) => self.requested = Some(request),
                CarPlayServerSessionEventTx::StopLocationInformat(_) => {
                    self.requested = None;
                    self.failed_generation = None;
                }
                _ => {}
            }
            consumed = true;
        }
        while let Some(Some(())) = self.changes.next().now_or_never() {
            consumed = true;
        }
        consumed |= self.sync_connection();

        if let Some(connection) = self.connection.as_mut()
            && let Some(update) = connection.pending_update.take()
        {
            match update {
                Some(update) if self.requested.is_some() && connection.car.is_available() => {
                    self.session
                        .on_event(CarPlayServerSessionEventRx::FeedGpsNmea(update), handle)?;
                }
                Some(_) => {}
                None => {
                    // Keep the subscription, but never spin reconnecting to a dead
                    // generation. Source notifications attach us to the next one.
                    self.failed_generation = Some(connection.generation);
                    self.connection = None;
                }
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
        decoder::{CsmFlag, CsmPacketId, CsmPacketRegistry},
        msg::{
            AuthenticationResponse, IdentificationInformation, LocationInformationComponent, StartLocationInformation, StartPowerUpdates,
            StopLocationInformation,
        },
    };
    use catplay_iap2_client::{CsmRemote, test_support::Capture};
    use futures::executor::block_on;
    use std::sync::Arc;

    fn respond(session: &mut dyn CsmSession, handle: &Arc<Capture>, packet: impl Into<CsmPacketBox>) {
        block_on(session.respond(packet.into(), handle.clone())).unwrap();
    }

    fn pump(session: &mut dyn CsmSession, handle: &Arc<Capture>) {
        session
            .wait_event()
            .now_or_never()
            .expect("queued bridge event must wake the session");
        assert!(session.process_event(handle.clone()).unwrap());
    }

    fn ready(car: &mut CarPlayClientSession, handle: &Arc<Capture>) {
        block_on(car.start(handle.clone())).unwrap();
        respond(
            car,
            handle,
            IdentificationInformation {
                location_information_component: Some(LocationInformationComponent {
                    recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
                    ..LocationInformationComponent::default()
                }),
                messages_received_from_device: IdentificationInformation::pack_ids(&[
                    StartLocationInformation::PACKET_ID,
                    StopLocationInformation::PACKET_ID,
                ]),
                messages_sent_by_accessory: IdentificationInformation::pack_ids(&[StartPowerUpdates::PACKET_ID]),
                ..IdentificationInformation::default()
            },
        );
        respond(car, handle, AuthenticationResponse::default());
        respond(car, handle, StartPowerUpdates::default());
    }

    fn phone(car: CarLocationSource) -> CsmSessionBox {
        Iap2LocationBridge::new(
            None,
            CarPlaySessionIdentity {
                has_gps: true,
                ..CarPlaySessionIdentity::default()
            },
            car,
        )
    }

    #[test]
    fn bridge_defers_early_start_then_forwards_and_stops_on_phone_teardown() {
        let (mut car, control) = CarPlayClientSession::new();
        let source = CarLocationSource::default();
        source.set(control);
        let mut phone = phone(source);
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
        assert!(
            packets
                .last()
                .unwrap()
                .cast::<StartLocationInformation>()
                .is_some()
        );

        let update = LocationInformation {
            nmea_sentence: vec!["$GPRMC,120000,V,,,,,,,131126,,,N*00\r\n".into()],
        };
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

    fn start_phone(phone: &mut CsmSessionBox, phone_handle: &Arc<Capture>, car: &mut CarPlayClientSession, car_handle: &Arc<Capture>) {
        respond(phone, phone_handle, StartLocationInformation::all());
        pump(phone, phone_handle);
        pump(car, car_handle);
    }

    fn sample() -> LocationInformation {
        LocationInformation {
            nmea_sentence: vec!["$GPRMC,120000,V,,,,,,,131126,,,N*00\r\n".into()],
        }
    }

    #[test]
    fn live_transport_recovers_when_newer_subscriber_stops_or_drops() {
        for disconnect in [false, true] {
            let (mut car, control) = CarPlayClientSession::new();
            let source = CarLocationSource::default();
            source.set(control);
            let car_handle = Capture::new();
            ready(&mut car, &car_handle);
            car_handle.take();
            let mut airplay = phone(source.clone());
            let airplay_handle = Capture::new();
            start_phone(&mut airplay, &airplay_handle, &mut car, &car_handle);
            car_handle.take();

            let mut bluetooth = phone(source);
            let bt_handle = Capture::new();
            // Idle Bluetooth must not disturb the AirPlay subscriber.
            assert!(car.wait_event().now_or_never().is_none());
            start_phone(&mut bluetooth, &bt_handle, &mut car, &car_handle);
            assert!(car_handle.location_commands().is_empty()); // same effective request
            respond(&mut car, &car_handle, sample());
            pump(&mut bluetooth, &bt_handle);
            assert_eq!(bt_handle.take()[0].cast::<LocationInformation>(), Some(&sample()));
            assert!(airplay.wait_event().now_or_never().is_none());
            if disconnect {
                drop(bluetooth);
            } else {
                respond(&mut bluetooth, &bt_handle, StopLocationInformation::default());
                pump(&mut bluetooth, &bt_handle);
            }
            pump(&mut car, &car_handle);
            assert!(car_handle.location_commands().is_empty()); // earlier subscription still active
            respond(&mut car, &car_handle, sample());
            pump(&mut airplay, &airplay_handle);
            assert_eq!(airplay_handle.take()[0].cast::<LocationInformation>(), Some(&sample()));
            drop(airplay);
            pump(&mut car, &car_handle);
            let commands = car_handle.location_commands();
            assert_eq!(commands.len(), 1);
            assert!(commands[0].cast::<StopLocationInformation>().is_some());
        }
    }

    #[test]
    fn old_transport_stop_cannot_stop_the_newer_subscriber() {
        let (mut car, control) = CarPlayClientSession::new();
        let source = CarLocationSource::default();
        source.set(control);
        let car_handle = Capture::new();
        ready(&mut car, &car_handle);
        let mut bluetooth = phone(source.clone());
        let mut airplay = phone(source);
        let phone_handle = Capture::new();
        start_phone(&mut bluetooth, &phone_handle, &mut car, &car_handle);
        start_phone(&mut airplay, &phone_handle, &mut car, &car_handle);
        car_handle.take();
        respond(&mut bluetooth, &phone_handle, StopLocationInformation::default());
        pump(&mut bluetooth, &phone_handle);
        pump(&mut car, &car_handle);
        drop(bluetooth);
        assert!(car_handle.location_commands().is_empty());
        respond(&mut car, &car_handle, sample());
        pump(&mut airplay, &phone_handle);
        assert_eq!(phone_handle.take()[0].cast::<LocationInformation>(), Some(&sample()));
    }

    #[test]
    fn subscribed_phone_follows_usb_replacement_and_discards_old_queued_data() {
        for replace_before_eof in [false, true] {
            let source = CarLocationSource::default();
            let (mut car, control) = CarPlayClientSession::new();
            source.set(control);
            let car_handle = Capture::new();
            ready(&mut car, &car_handle);
            let mut phone = phone(source.clone());
            let phone_handle = Capture::new();
            start_phone(&mut phone, &phone_handle, &mut car, &car_handle);
            car_handle.take();
            respond(&mut car, &car_handle, sample());
            // Exercise a cached update as well as one still queued in the channel.
            phone.wait_event().now_or_never().unwrap();
            drop(car);
            assert!(car_handle.take().is_empty());
            if !replace_before_eof {
                assert!(phone.process_event(phone_handle.clone()).unwrap());
                pump(&mut phone, &phone_handle); // EOF; subscription remains remembered
                assert!(phone.wait_event().now_or_never().is_none());
            }
            let (mut next_car, next_control) = CarPlayClientSession::new();
            source.set(next_control);
            pump(&mut phone, &phone_handle);
            pump(&mut next_car, &car_handle);
            assert!(phone_handle.take().is_empty());
            assert!(car_handle.take().is_empty()); // new car still not ready
            ready(&mut next_car, &car_handle);
            assert_eq!(car_handle.location_commands().len(), 1);
            respond(&mut next_car, &car_handle, sample());
            pump(&mut phone, &phone_handle);
            assert_eq!(phone_handle.take()[0].cast::<LocationInformation>(), Some(&sample()));
        }
    }

    #[test]
    fn early_subscriber_attaches_when_first_usb_session_is_created() {
        let source = CarLocationSource::default();
        let mut phone = phone(source.clone());
        let phone_handle = Capture::new();
        respond(&mut phone, &phone_handle, StartLocationInformation::all());
        pump(&mut phone, &phone_handle);
        assert!(phone.wait_event().now_or_never().is_none());
        let (mut car, control) = CarPlayClientSession::new();
        source.set(control);
        let car_handle = Capture::new();
        pump(&mut phone, &phone_handle);
        pump(&mut car, &car_handle);
        assert!(car_handle.take().is_empty());
        ready(&mut car, &car_handle);
        assert_eq!(car_handle.location_commands().len(), 1);
    }

    #[test]
    fn new_usb_session_wakes_every_subscribed_transport() {
        let source = CarLocationSource::default();
        let mut first = phone(source.clone());
        let mut second = phone(source.clone());
        let phone_handle = Capture::new();
        respond(&mut first, &phone_handle, StartLocationInformation::all());
        pump(&mut first, &phone_handle);
        respond(&mut second, &phone_handle, StartLocationInformation::all());
        pump(&mut second, &phone_handle);
        let (mut car, control) = CarPlayClientSession::new();
        source.set(control);
        let car_handle = Capture::new();
        pump(&mut first, &phone_handle);
        pump(&mut second, &phone_handle);
        pump(&mut car, &car_handle);
        ready(&mut car, &car_handle);
        assert_eq!(car_handle.location_commands().len(), 1);
        drop(second);
        pump(&mut car, &car_handle);
        respond(&mut car, &car_handle, sample());
        pump(&mut first, &phone_handle);
        assert_eq!(phone_handle.take()[0].cast::<LocationInformation>(), Some(&sample()));
    }

    #[test]
    fn failed_attach_waits_for_next_generation_without_spinning() {
        let source = CarLocationSource::default();
        let (car, control) = CarPlayClientSession::new();
        source.set(control);
        drop(car);
        let mut phone = phone(source.clone());
        let phone_handle = Capture::new();
        respond(&mut phone, &phone_handle, StartLocationInformation::all());
        pump(&mut phone, &phone_handle); // connect returns false
        assert!(phone.wait_event().now_or_never().is_none());
        assert!(!phone.process_event(phone_handle.clone()).unwrap());
        let (mut car, control) = CarPlayClientSession::new();
        source.set(control);
        let car_handle = Capture::new();
        ready(&mut car, &car_handle);
        pump(&mut phone, &phone_handle);
        pump(&mut car, &car_handle);
        assert_eq!(car_handle.location_commands().len(), 1);
    }

    #[test]
    fn bluetooth_non_gps_responses_match_the_unwrapped_session() {
        use catplay_csm::msg::{
            AuthenticationSucceeded, IdentificationAccepted, RequestAccessoryWiFiConfigurationInformation,
            RequestAuthenticationCertificate, RequestAuthenticationChallengeResponse, StartIdentification,
        };
        let identity = CarPlaySessionIdentity {
            display_name: "Volvo CatPlay".into(),
            bt_mac: Some("12:34:56:78:9a:bc".parse().unwrap()),
            wifi_ssid: Some("CatPlay-Volvo".into()),
            wifi_passphrase: Some("test-passphrase".into()),
            wifi_is_wpa: true,
            wifi_channel: Some(36),
            is_usb_transport: false,
            has_gps: true,
            wants_now_playing: false,
            ..CarPlaySessionIdentity::default()
        };
        let (mut plain, _feeds, _events) = CarPlayServerSession::new(None, identity.clone());
        let source = CarLocationSource::default();
        let (mut car, control) = CarPlayClientSession::new();
        source.set(control);
        let mut bridged = Iap2LocationBridge::new(None, identity, source);
        let remote = CsmRemote::bluetooth("aa:bb:cc:dd:ee:ff".parse().unwrap(), "12:34:56:78:9a:bc".parse().unwrap());
        let plain_handle = Capture::with_remote(remote, false);
        let bridge_handle = Capture::with_remote(remote, false);
        block_on(plain.start(plain_handle.clone())).unwrap();
        block_on(bridged.start(bridge_handle.clone())).unwrap();
        for packet in [
            CsmPacketBox::from(StartIdentification::default()),
            IdentificationAccepted::default().into(),
            RequestAuthenticationCertificate::default().into(),
            RequestAuthenticationChallengeResponse::default().into(),
            AuthenticationSucceeded::default().into(),
            RequestAccessoryWiFiConfigurationInformation::default().into(),
        ] {
            let registry = CsmPacketRegistry::static_registry();
            let bytes = registry.encode(&packet).unwrap();
            respond(&mut plain, &plain_handle, registry.decode(&bytes).unwrap());
            respond(&mut bridged, &bridge_handle, packet);
            assert!(!bridged.process_event(bridge_handle.clone()).unwrap());
            let encode = |packets: Vec<CsmPacketBox>| {
                packets.into_iter().map(|packet| registry.encode(&packet).unwrap()).collect::<Vec<_>>()
            };
            assert_eq!(encode(plain_handle.take()), encode(bridge_handle.take()));
            assert!(car.wait_event().now_or_never().is_none());
        }
    }
}
