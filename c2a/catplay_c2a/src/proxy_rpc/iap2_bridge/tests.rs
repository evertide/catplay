use std::sync::{Arc, Mutex};

use catplay_carplay_tx_gadget::carplay_client_session::CarPlayClientSession;
use catplay_csm::{
    decoder::{AsCsmPacket, CsmFlag, CsmPacketId, CsmPacketRegistry},
    msg::*,
};
use catplay_iap2_client::{CsmClientHandle, CsmRemote, CsmSessionStatus};
use futures::executor::block_on;

use super::*;

#[derive(Default)]
struct Capture(Mutex<Vec<CsmPacketBox>>);

impl Capture {
    fn take(&self) -> Vec<CsmPacketBox> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }

    fn location_commands(&self) -> Vec<CsmPacketBox> {
        self.take()
            .into_iter()
            .filter(|packet| packet.cast::<StartLocationInformation>().is_some() || packet.cast::<StopLocationInformation>().is_some())
            .collect()
    }
}

impl CsmClientHandle for Capture {
    fn disconnect(&self) {}
    fn is_server(&self) -> bool {
        true
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
        &CsmRemote::UsbGadget
    }
    fn status(&self) -> CsmSessionStatus {
        CsmSessionStatus::Writable
    }
    fn send(&self, packet: &dyn AsCsmPacket) -> CsmSessionResult<()> {
        let registry = CsmPacketRegistry::static_registry();
        let bytes = registry.encode(packet).unwrap();
        self.0.lock().unwrap().push(registry.decode(&bytes).unwrap());
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
    session
        .wait_event()
        .now_or_never()
        .expect("queued event must wake the session");
    session.process_event(handle.clone()).unwrap();
}

fn phone(location: LocationBridge) -> CsmSessionBox {
    Iap2LocationBridge::new(
        None,
        CarPlaySessionIdentity {
            has_gps: true,
            ..CarPlaySessionIdentity::default()
        },
        location,
    )
}

fn car_id() -> IdentificationInformation {
    IdentificationInformation {
        location_information_component: Some(LocationInformationComponent {
            global_positioning_system_fix_data: CsmFlag::Yes,
            recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
            vehicle_speed_data: CsmFlag::Yes,
            ..LocationInformationComponent::default()
        }),
        messages_received_from_device: IdentificationInformation::pack_ids(&[
            StartLocationInformation::PACKET_ID,
            StopLocationInformation::PACKET_ID,
        ]),
        messages_sent_by_accessory: IdentificationInformation::pack_ids(&[
            LocationInformation::PACKET_ID,
            PowerSourceUpdate::PACKET_ID,
            StartPowerUpdates::PACKET_ID,
        ]),
        ..IdentificationInformation::default()
    }
}

fn ready(car: &mut CarPlayClientSession, handle: &Arc<Capture>) {
    block_on(car.start(handle.clone())).unwrap();
    respond(car, handle, car_id());
    respond(car, handle, AuthenticationResponse::default());
}

fn sample() -> LocationInformation {
    LocationInformation {
        nmea_sentence: vec!["$GPRMC,120000,V,,,,,,,131126,,,N*00\r\n".into(), "$PASCD,0*00\r\n".into()],
    }
}

#[test]
fn early_request_waits_for_identification_and_authentication_but_not_power() {
    for power_ids in [
        vec![],
        vec![PowerSourceUpdate::PACKET_ID],
        vec![StartPowerUpdates::PACKET_ID],
        vec![PowerSourceUpdate::PACKET_ID, StartPowerUpdates::PACKET_ID],
    ] {
        let location = LocationBridge::default();
        let mut car = CarPlayClientSession::with_location(location.clone());
        let mut phone = phone(location);
        let car_out = Arc::new(Capture::default());
        let phone_out = Arc::new(Capture::default());
        respond(&mut phone, &phone_out, StartLocationInformation::all());
        pump(&mut phone, &phone_out);
        pump(&mut car, &car_out);
        assert!(car_out.take().is_empty());

        block_on(car.start(car_out.clone())).unwrap();
        let mut sent = power_ids;
        sent.push(LocationInformation::PACKET_ID);
        respond(
            &mut car,
            &car_out,
            IdentificationInformation {
                messages_sent_by_accessory: IdentificationInformation::pack_ids(&sent),
                ..car_id()
            },
        );
        assert!(car_out.location_commands().is_empty());
        respond(&mut car, &car_out, AuthenticationResponse::default());
        let commands = car_out.location_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0].cast::<StartLocationInformation>(),
            Some(&StartLocationInformation {
                global_positioning_system_fix_data: CsmFlag::Yes,
                recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
                vehicle_speed_data: CsmFlag::Yes,
                ..StartLocationInformation::default()
            })
        );

        // Existing power replies still work if they arrive later, without restarting GPS.
        respond(&mut car, &car_out, PowerSourceUpdate::default());
        respond(&mut car, &car_out, StartPowerUpdates::default());
        let power = car_out.take();
        assert_eq!(power.len(), 6);
        assert!(power.iter().all(|packet| packet.cast::<PowerUpdate>().is_some()));
    }
}

fn unsupported_cars() -> Vec<IdentificationInformation> {
    vec![
        IdentificationInformation {
            location_information_component: None,
            ..car_id()
        },
        IdentificationInformation {
            location_information_component: Some(LocationInformationComponent::default()),
            ..car_id()
        },
        IdentificationInformation {
            messages_received_from_device: IdentificationInformation::pack_ids(&[StartLocationInformation::PACKET_ID]),
            ..car_id()
        },
        IdentificationInformation {
            messages_received_from_device: IdentificationInformation::pack_ids(&[StopLocationInformation::PACKET_ID]),
            ..car_id()
        },
        IdentificationInformation {
            messages_sent_by_accessory: IdentificationInformation::pack_ids(&[PowerSourceUpdate::PACKET_ID, StartPowerUpdates::PACKET_ID]),
            ..car_id()
        },
    ]
}

#[test]
fn no_commands_without_subscription_or_full_location_capabilities() {
    let cases = std::iter::once((false, car_id())).chain(unsupported_cars().into_iter().map(|id| (true, id)));
    for (subscribe, id) in cases {
        let location = LocationBridge::default();
        let mut car = CarPlayClientSession::with_location(location.clone());
        let mut phone = phone(location.clone());
        let car_out = Arc::new(Capture::default());
        let phone_out = Arc::new(Capture::default());
        block_on(car.start(car_out.clone())).unwrap();
        respond(&mut car, &car_out, id);
        respond(&mut car, &car_out, AuthenticationResponse::default());
        if subscribe {
            assert!(!location.is_available());
            respond(&mut phone, &phone_out, StartLocationInformation::all());
            pump(&mut phone, &phone_out);
            pump(&mut car, &car_out);
        }
        respond(&mut car, &car_out, sample());
        assert!(car_out.location_commands().is_empty());
        assert!(phone.wait_event().now_or_never().is_none());
        assert!(phone_out.take().is_empty());
    }
}

#[test]
fn forwards_only_while_subscribed_and_stops_on_unsubscribe_or_disconnect() {
    for disconnect in [false, true] {
        let location = LocationBridge::default();
        let mut car = CarPlayClientSession::with_location(location.clone());
        let mut phone = phone(location);
        let car_out = Arc::new(Capture::default());
        let phone_out = Arc::new(Capture::default());
        ready(&mut car, &car_out);
        respond(&mut car, &car_out, sample());
        assert!(phone.wait_event().now_or_never().is_none());
        respond(&mut phone, &phone_out, StartLocationInformation::all());
        pump(&mut phone, &phone_out);
        pump(&mut car, &car_out);
        assert_eq!(car_out.location_commands().len(), 1);
        respond(&mut car, &car_out, sample());
        pump(&mut phone, &phone_out);
        let packets = phone_out.take();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].cast::<LocationInformation>(), Some(&sample()));

        respond(&mut car, &car_out, sample()); // Queue data just before Stop/disconnect.
        phone.wait_event().now_or_never().unwrap();
        if disconnect {
            drop(phone);
        } else {
            respond(&mut phone, &phone_out, StopLocationInformation::default());
            pump(&mut phone, &phone_out);
        }
        pump(&mut car, &car_out);
        let commands = car_out.location_commands();
        assert_eq!(commands.len(), 1);
        assert!(commands[0].cast::<StopLocationInformation>().is_some());
        respond(&mut car, &car_out, sample());
        assert!(phone_out.take().is_empty());
        assert!(car_out.location_commands().is_empty());
    }
}

#[test]
fn stop_before_authentication_cancels_deferred_request() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let mut phone = phone(location);
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    respond(&mut phone, &phone_out, StartLocationInformation::all());
    pump(&mut phone, &phone_out);
    respond(&mut phone, &phone_out, StopLocationInformation::default());
    pump(&mut phone, &phone_out);
    ready(&mut car, &car_out);
    assert!(car_out.location_commands().is_empty());
}

#[test]
fn replacement_usb_session_repeats_negotiation_for_the_live_iphone_request() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let mut phone = phone(location.clone());
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    respond(&mut phone, &phone_out, StartLocationInformation::all());
    pump(&mut phone, &phone_out);
    pump(&mut car, &car_out);
    car_out.take();
    drop(car);
    assert!(car_out.take().is_empty()); // No Stop queued into a destroyed USB link.

    let mut car = CarPlayClientSession::with_location(location);
    car.process_event(car_out.clone()).unwrap();
    assert!(car_out.take().is_empty());
    ready(&mut car, &car_out);
    assert_eq!(car_out.location_commands().len(), 1);
    respond(&mut car, &car_out, sample());
    pump(&mut phone, &phone_out);
    assert_eq!(phone_out.take()[0].cast::<LocationInformation>(), Some(&sample()));
}

#[test]
fn slow_phone_receives_only_the_latest_fix_even_after_a_cancelled_wait() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let mut phone = phone(location);
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    respond(&mut phone, &phone_out, StartLocationInformation::all());
    pump(&mut phone, &phone_out);
    pump(&mut car, &car_out);
    assert!(phone.wait_event().now_or_never().is_none());
    respond(&mut car, &car_out, sample());
    phone.wait_event().now_or_never().unwrap();
    let mut latest = sample();
    for counter in 0..1000 {
        latest.nmea_sentence = vec![format!("$PASCD,{counter}*00\r\n").into()];
        respond(&mut car, &car_out, latest.clone());
    }
    pump(&mut phone, &phone_out);
    let packets = phone_out.take();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].cast::<LocationInformation>(), Some(&latest));
    assert!(!phone.process_event(phone_out.clone()).unwrap());
    assert!(phone.wait_event().now_or_never().is_none());
}

#[test]
fn overlapping_usb_sessions_both_wake_for_start_and_stop() {
    use futures::task::{ArcWake, waker};
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Context,
    };

    #[derive(Default)]
    struct WakeCount(AtomicUsize);
    impl ArcWake for WakeCount {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let location = LocationBridge::default();
    let mut first = CarPlayClientSession::with_location(location.clone());
    let mut second = CarPlayClientSession::with_location(location.clone());
    let mut phone = phone(location);
    let first_out = Arc::new(Capture::default());
    let second_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut first, &first_out);
    ready(&mut second, &second_out);
    first_out.take();
    second_out.take();

    for start in [true, false] {
        let first_wakes = Arc::new(WakeCount::default());
        let second_wakes = Arc::new(WakeCount::default());
        let first_waker = waker(first_wakes.clone());
        let second_waker = waker(second_wakes.clone());
        let mut first_context = Context::from_waker(&first_waker);
        let mut second_context = Context::from_waker(&second_waker);
        let mut first_wait = first.wait_event();
        let mut second_wait = second.wait_event();
        assert!(first_wait.as_mut().poll(&mut first_context).is_pending());
        assert!(second_wait.as_mut().poll(&mut second_context).is_pending());
        if start {
            respond(&mut phone, &phone_out, StartLocationInformation::all());
        } else {
            respond(&mut phone, &phone_out, StopLocationInformation::default());
        }
        pump(&mut phone, &phone_out);
        assert!(first_wakes.0.load(Ordering::Relaxed) > 0);
        assert!(second_wakes.0.load(Ordering::Relaxed) > 0);
        assert!(first_wait.as_mut().poll(&mut first_context).is_ready());
        assert!(second_wait.as_mut().poll(&mut second_context).is_ready());
        drop(first_wait);
        drop(second_wait);
        for (car, out) in [(&mut first, &first_out), (&mut second, &second_out)] {
            assert!(car.process_event(out.clone()).unwrap());
            let commands = out.location_commands();
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].cast::<StartLocationInformation>().is_some(), start);
            assert_eq!(commands[0].cast::<StopLocationInformation>().is_some(), !start);
        }
    }
}

#[test]
fn availability_tracks_the_newest_live_usb_session() {
    let location = LocationBridge::default();
    let out = Arc::new(Capture::default());
    let mut first = CarPlayClientSession::with_location(location.clone());
    assert!(!location.is_available());
    ready(&mut first, &out);
    assert!(location.is_available());
    let mut second = CarPlayClientSession::with_location(location.clone());
    assert!(!location.is_available());
    ready(&mut second, &out);
    drop(first);
    assert!(location.is_available());
    drop(second);
    assert!(!location.is_available());
}

#[test]
fn reidentification_stops_gps_until_authentication_is_repeated() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let mut phone = phone(location);
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    respond(&mut phone, &phone_out, StartLocationInformation::all());
    pump(&mut phone, &phone_out);
    pump(&mut car, &car_out);
    car_out.take();
    respond(&mut car, &car_out, car_id());
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
    respond(&mut car, &car_out, sample());
    assert!(phone.wait_event().now_or_never().is_none());
    respond(&mut car, &car_out, AuthenticationResponse::default());
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StartLocationInformation>().is_some());
}

#[test]
fn replacing_a_request_discards_old_fixes_without_cancelling_the_new_subscription() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let first = location.subscribe(StartLocationInformation::all());
    pump(&mut car, &car_out);
    car_out.take();
    respond(&mut car, &car_out, sample());
    let mut second = location.subscribe(StartLocationInformation::all());
    drop(first);
    pump(&mut car, &car_out);
    assert!(car_out.take().is_empty());
    assert!(second.take().is_none());
    respond(&mut car, &car_out, sample());
    assert_eq!(second.take(), Some(sample()));
    drop(second);
    pump(&mut car, &car_out);
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
}
