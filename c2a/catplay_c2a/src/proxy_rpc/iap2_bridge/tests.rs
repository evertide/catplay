use std::sync::{Arc, Mutex};

use catplay_carplay::{
    carplay_rx::AirPlayReceiverSink,
    events::CommandsClient,
    msg::{Command, CommandIApSendMessage},
    rtsp_frame::{HttpStatus, RtspQueue, RtspResponse},
};
use catplay_carplay_tx_gadget::carplay_client_session::CarPlayClientSession;
use catplay_csm::{
    decoder::{AsCsmPacket, CsmFlag, CsmPacketId, CsmPacketRegistry},
    msg::*,
};
use catplay_iap2_client::{
    CsmClient, CsmClientHandle, CsmRemote, CsmSessionCallbacks, CsmSessionStatus,
    tokio::{AsyncClient, AsyncClientDrain},
};
use catplay_util::{EventReconciler, EventSleeper, mpsc};
use futures::{FutureExt, StreamExt, executor::block_on};

use super::*;
use crate::proxy_rpc::{CarPlayRxSession, TxAdapter, TxAdapterOp};

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
        self.0
            .lock()
            .unwrap()
            .push(registry.decode(&bytes).unwrap());
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
    while session.process_event(handle.clone()).unwrap() {}
}

fn unready_phone(location: LocationBridge) -> CsmSessionBox {
    Iap2LocationBridge::new(
        None,
        CarPlaySessionIdentity {
            has_gps: true,
            ..CarPlaySessionIdentity::default()
        },
        location,
        true,
    )
}

fn authenticate_phone(session: &mut dyn CsmSession, out: &Arc<Capture>) {
    block_on(session.start(out.clone())).unwrap();
    respond(session, out, IdentificationAccepted::default());
    respond(session, out, AuthenticationSucceeded::default());
}

fn phone(location: LocationBridge) -> CsmSessionBox {
    let mut session = unready_phone(location);
    authenticate_phone(&mut session, &Arc::new(Capture::default()));
    session
}

fn take_location(out: &Arc<Capture>) -> LocationInformation {
    let nmea_sentence = out
        .take()
        .into_iter()
        .flat_map(|packet| {
            let packet = packet.cast::<LocationInformation>().expect("only location data");
            assert_eq!(packet.nmea_sentence.len(), 1, "one sentence per message");
            packet.nmea_sentence.clone()
        })
        .collect();
    LocationInformation { nmea_sentence }
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
        assert!(
            power
                .iter()
                .all(|packet| packet.cast::<PowerUpdate>().is_some())
        );
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
        pump(&mut car, &car_out);
        assert_eq!(car_out.location_commands().len(), 1);
        respond(&mut car, &car_out, sample());
        pump(&mut phone, &phone_out);
        assert_eq!(take_location(&phone_out), sample());

        respond(&mut car, &car_out, sample()); // Queue data just before Stop/disconnect.
        phone.wait_event().now_or_never().unwrap();
        if disconnect {
            drop(phone);
        } else {
            respond(&mut phone, &phone_out, StopLocationInformation::default());
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
    respond(&mut phone, &phone_out, StopLocationInformation::default());
    ready(&mut car, &car_out);
    assert!(car_out.location_commands().is_empty());
}

#[test]
fn empty_or_unsupported_replacements_stop_gps_until_a_usable_request() {
    for rejected in [
        StartLocationInformation::default(),
        StartLocationInformation {
            vehicle_gyro_data: CsmFlag::Yes,
            ..StartLocationInformation::default()
        },
    ] {
        let location = LocationBridge::default();
        let mut car = CarPlayClientSession::with_location(location.clone());
        let mut phone = phone(location);
        let car_out = Arc::new(Capture::default());
        let phone_out = Arc::new(Capture::default());
        ready(&mut car, &car_out);
        respond(&mut phone, &phone_out, StartLocationInformation::all());
        pump(&mut car, &car_out);
        assert_eq!(car_out.location_commands().len(), 1);
        respond(&mut car, &car_out, sample());

        for expected_stops in [1, 0] {
            respond(&mut phone, &phone_out, rejected.clone());
            pump(&mut car, &car_out);
            let commands = car_out.location_commands();
            assert_eq!(commands.len(), expected_stops);
            assert!(
                commands
                    .iter()
                    .all(|packet| packet.cast::<StopLocationInformation>().is_some())
            );
            respond(&mut car, &car_out, sample());
            assert!(phone.wait_event().now_or_never().is_none());
            assert!(phone_out.take().is_empty());
        }

        respond(&mut phone, &phone_out, StartLocationInformation::all());
        pump(&mut car, &car_out);
        assert_eq!(car_out.location_commands().len(), 1);
        respond(&mut car, &car_out, sample());
        pump(&mut phone, &phone_out);
        assert_eq!(take_location(&phone_out), sample());
    }
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
    assert_eq!(take_location(&phone_out), sample());
}

#[test]
fn slow_phone_keeps_each_sentence_type_after_a_cancelled_wait() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let mut phone = phone(location);
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    respond(&mut phone, &phone_out, StartLocationInformation::all());
    pump(&mut car, &car_out);
    assert!(phone.wait_event().now_or_never().is_none());
    respond(&mut car, &car_out, sample());
    phone.wait_event().now_or_never().unwrap();
    let mut latest = sample();
    for counter in 0..1000 {
        latest.nmea_sentence = vec![
            format!("$GPGGA,{counter}*00\r\n"),
            format!("$GPRMC,{counter}*00\r\n"),
            format!("$PASCD,{counter}*00\r\n"),
        ];
        // Cars can send each sentence type in a separate LocationInformation.
        for sentence in &latest.nmea_sentence {
            respond(
                &mut car,
                &car_out,
                LocationInformation {
                    nmea_sentence: vec![sentence.clone()],
                },
            );
        }
    }
    pump(&mut phone, &phone_out);
    assert_eq!(take_location(&phone_out), latest);
    assert!(!phone.process_event(phone_out.clone()).unwrap());
    assert!(phone.wait_event().now_or_never().is_none());
}

async fn tick(client: &mut AsyncClient) {
    let _ = client.sleep().now_or_never();
    EventReconciler::reconcile(client).await.unwrap();
}

fn relay(drain: &mut AsyncClientDrain, client: &mut AsyncClient) {
    let _ = drain.sleep().now_or_never();
    if let Some(frame) = drain.take() {
        assert!(frame.as_ref().len() <= 2048);
        client.read_frame(&frame);
    }
}

#[tokio::test]
async fn single_sentence_messages_fit_the_default_2048_byte_link() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let phone_out = Arc::new(Capture::default());
    let captured = phone_out.clone();
    let peer_session = CsmSessionCallbacks::new(
        async |handle| {
            handle.send(&IdentificationAccepted::default())?;
            handle.send(&AuthenticationSucceeded::default())?;
            handle.send(&StartLocationInformation::all())
        },
        move |packet, _| {
            captured.0.lock().unwrap().push(packet);
            async { Ok(()) }
        },
    );
    let (mut client, mut outgoing) = AsyncClient::new(false, CsmRemote::airplay(), unready_phone(location));
    let (mut peer, mut incoming) = AsyncClient::new(true, CsmRemote::airplay(), Box::new(peer_session));
    for _ in 0..100 {
        tick(&mut client).await;
        relay(&mut outgoing, &mut peer);
        tick(&mut peer).await;
        relay(&mut incoming, &mut client);
    }
    pump(&mut car, &car_out);
    assert_eq!(car_out.location_commands().len(), 1);
    assert_eq!(client.handle().max_csm_payload_len(), 2048 - 10 - 6);
    phone_out.take();

    // Each sentence fits, but combining GGA and RMC exceeds the peer limit.
    let gga = LocationInformation {
        nmea_sentence: vec![format!("$GPGGA,{}", "x".repeat(1188))],
    };
    let rmc = LocationInformation {
        nmea_sentence: vec![format!("$GPRMC,{}", "x".repeat(1188))],
    };
    let registry = CsmPacketRegistry::static_registry();
    for group in [&gga, &rmc] {
        assert_eq!(registry.encode(group).unwrap().len(), 1206);
    }
    let mut merged = gga.clone();
    merged.nmea_sentence.extend(rmc.nmea_sentence.clone());
    assert_eq!(registry.encode(&merged).unwrap().len() + 10, 2416);

    for expected in [vec![gga, rmc], vec![sample()]] {
        for group in &expected {
            respond(&mut car, &car_out, group.clone());
        }
        for _ in 0..100 {
            tick(&mut client).await;
            relay(&mut outgoing, &mut peer);
            tick(&mut peer).await;
            relay(&mut incoming, &mut client);
        }
        let expected = LocationInformation {
            nmea_sentence: expected.into_iter().flat_map(|packet| packet.nmea_sentence).collect(),
        };
        assert_eq!(take_location(&phone_out), expected);
        assert!(!client.handle().has_pending_send());
    }
}

#[tokio::test]
async fn delayed_rtsp_response_keeps_new_fixes_in_the_accumulator() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let phone_out = Arc::new(Capture::default());
    let captured = phone_out.clone();
    let peer_session = CsmSessionCallbacks::new(
        async |handle| {
            handle.send(&IdentificationAccepted::default())?;
            handle.send(&AuthenticationSucceeded::default())?;
            handle.send(&StartLocationInformation::all())
        },
        move |packet, _| {
            captured.0.lock().unwrap().push(packet);
            async { Ok(()) }
        },
    );
    let (mut client, mut outgoing) = AsyncClient::new(false, CsmRemote::airplay(), unready_phone(location));
    let (mut peer, mut incoming) = AsyncClient::new(true, CsmRemote::airplay(), Box::new(peer_session));
    // Complete real iAP2 link negotiation and deliver the phone's subscription.
    for _ in 0..100 {
        tick(&mut client).await;
        relay(&mut outgoing, &mut peer);
        tick(&mut peer).await;
        relay(&mut incoming, &mut client);
    }
    pump(&mut car, &car_out);
    assert_eq!(car_out.location_commands().len(), 1);
    assert!(!client.handle().has_pending_send());
    phone_out.take();

    respond(&mut car, &car_out, sample());
    for _ in 0..10 {
        tick(&mut client).await;
        let _ = outgoing.sleep().now_or_never();
    }
    let (frame, permit) = outgoing.take_with_permit().expect("first location frame");
    peer.read_frame(&frame);
    let (queue, mut responses) = RtspQueue::fifo();
    let mut response = Box::pin(
        queue
            .send_command(
                &Command::IApSendMessage(CommandIApSendMessage {
                    data: frame.into_inner().into(),
                }),
                std::time::Duration::from_secs(30),
            )
            .unwrap(),
    );
    assert_eq!(responses.drain().len(), 1);

    let mut latest = sample();
    for counter in 0..1000 {
        latest.nmea_sentence = vec![
            format!("$GPGGA,{counter}"),
            format!("$GPRMC,{counter}"),
            format!("$PASCD,{counter}"),
        ];
        for sentence in &latest.nmea_sentence {
            respond(
                &mut car,
                &car_out,
                LocationInformation {
                    nmea_sentence: vec![sentence.clone()],
                },
            );
        }
        tick(&mut client).await;
        tick(&mut peer).await;
        relay(&mut incoming, &mut client);
        assert!(response.as_mut().now_or_never().is_none());
        assert!(client.handle().has_pending_send());
        assert!(outgoing.sleep().now_or_never().is_none());
        assert!(outgoing.take().is_none());
    }
    assert_eq!(take_location(&phone_out).nmea_sentence, vec![sample().nmea_sentence[0].clone()]);
    responses.feed(RtspResponse::new(None, HttpStatus::Ok));
    response.await.unwrap();
    drop(permit);

    for _ in 0..100 {
        tick(&mut client).await;
        relay(&mut outgoing, &mut peer);
        tick(&mut peer).await;
        relay(&mut incoming, &mut client);
    }
    assert_eq!(take_location(&phone_out), latest);
    assert!(!client.handle().has_pending_send());
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
    location.set_request(Some(StartLocationInformation::all()));
    let first = location.subscribe();
    pump(&mut car, &car_out);
    car_out.take();
    respond(&mut car, &car_out, sample());
    location.set_request(Some(StartLocationInformation::all()));
    let mut second = location.subscribe();
    drop(first);
    pump(&mut car, &car_out);
    assert!(car_out.take().is_empty());
    assert!(second.take(2048 - 16).is_none());
    respond(&mut car, &car_out, sample());
    for sentence in sample().nmea_sentence {
        assert_eq!(second.take(2048 - 16), Some(LocationInformation { nmea_sentence: vec![sentence] }));
    }
    drop(second);
    pump(&mut car, &car_out);
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
}

fn wireless_identity() -> CarPlaySessionIdentity {
    CarPlaySessionIdentity {
        name: "CatPlay GPS test".into(),
        display_name: "Volvo".into(),
        bt_mac: Some("12:34:56:78:9a:bc".parse().unwrap()),
        wifi_ssid: Some("CatPlay-Volvo".into()),
        wifi_passphrase: Some("test-passphrase".into()),
        wifi_is_wpa: true,
        wifi_channel: Some(36),
        is_usb_transport: false,
        has_gps: true,
        ..CarPlaySessionIdentity::default()
    }
}

fn airplay_sink(
    location: LocationBridge,
    identity: Option<CarPlaySessionIdentity>,
) -> (CarPlayRxSession, mpsc::UnboundedReceiver<TxAdapterOp>) {
    let (tx, rx) = mpsc::unbounded();
    (
        CarPlayRxSession::new(TxAdapter::new(tx), location, Arc::new(Mutex::new(identity)), None),
        rx,
    )
}

fn iap2_command() -> Command {
    Command::IApSendMessage(CommandIApSendMessage {
        data: vec![1, 2, 3].into(),
    })
}

#[test]
fn volvo_negotiation_waits_for_an_iphone_location_request() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    block_on(car.start(car_out.clone())).unwrap();
    // Message lists from the XC90 build 10 identification, including 0xFFFA/B/C.
    respond(
        &mut car,
        &car_out,
        IdentificationInformation {
            messages_sent_by_accessory: IdentificationInformation::pack_ids(&[
                16724, 16726, 16731, 16732, 43521, 43523, 43526, 44544, 44546, 44547, 65531, 16736, 20480, 20482, 16727, 16729,
            ]),
            messages_received_from_device: IdentificationInformation::pack_ids(&[
                16725, 19982, 43520, 43525, 43524, 43522, 44545, 65530, 65532, 19979, 20481, 16728,
            ]),
            ..car_id()
        },
    );
    respond(&mut car, &car_out, AuthenticationResponse::default());
    assert!(location.is_available());

    let (mut sink, _proxy) = airplay_sink(location, Some(wireless_identity()));
    let mut endpoint = sink.open_iap2().now_or_never().unwrap().unwrap();
    let phone_out = Arc::new(Capture::default());
    respond(&mut endpoint, &phone_out, StartIdentification::default());
    let packets = phone_out.take();
    let id = packets[0].cast::<IdentificationInformation>().unwrap();
    assert!(id.location_information_component.is_some());
    assert!(id.wants_tx(LocationInformation::PACKET_ID));
    assert!(id.wants_rx(StartLocationInformation::PACKET_ID));
    assert!(id.wants_rx(StopLocationInformation::PACKET_ID));
    respond(&mut endpoint, &phone_out, IdentificationAccepted::default());
    respond(&mut endpoint, &phone_out, AuthenticationSucceeded::default());
    assert!(!car.process_event(car_out.clone()).unwrap());
    assert!(car_out.location_commands().is_empty());

    respond(&mut endpoint, &phone_out, StartLocationInformation::all());
    pump(&mut car, &car_out);
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StartLocationInformation>().is_some());
}

fn assert_proxied(sink: &mut CarPlayRxSession, proxy: &mut mpsc::UnboundedReceiver<TxAdapterOp>) {
    let command = iap2_command();
    let mut future = sink.on_command_raw(&command);
    assert!(future.as_mut().now_or_never().is_none());
    let Some(Some(TxAdapterOp::Command { response, .. })) = proxy.next().now_or_never() else {
        panic!("iAP2 commands must still reach the car proxy");
    };
    response
        .send(RtspResponse::new(None, HttpStatus::Ok))
        .unwrap();
    assert_eq!(block_on(future).unwrap().status, HttpStatus::Ok);
}

#[test]
fn cars_without_location_keep_the_existing_airplay_command_proxy() {
    for id in unsupported_cars() {
        let location = LocationBridge::default();
        let mut car = CarPlayClientSession::with_location(location.clone());
        let car_out = Arc::new(Capture::default());
        block_on(car.start(car_out.clone())).unwrap();
        respond(&mut car, &car_out, id);
        respond(&mut car, &car_out, AuthenticationResponse::default());
        let (mut sink, mut proxy) = airplay_sink(location, Some(wireless_identity()));
        assert!(block_on(sink.open_iap2()).is_none());
        assert_proxied(&mut sink, &mut proxy);
        assert!(car_out.location_commands().is_empty());
        assert!(car.wait_event().now_or_never().is_none());
    }
}

#[test]
fn airplay_without_a_car_or_wireless_identity_does_not_wait_or_claim_iap2() {
    let location = LocationBridge::default();
    let (mut no_car, mut proxy) = airplay_sink(location.clone(), Some(wireless_identity()));
    assert!(no_car.open_iap2().now_or_never().unwrap().is_none());
    assert_proxied(&mut no_car, &mut proxy);

    let mut car = CarPlayClientSession::with_location(location.clone());
    ready(&mut car, &Arc::new(Capture::default()));
    let (mut no_identity, mut proxy) = airplay_sink(location, None);
    assert!(no_identity.open_iap2().now_or_never().unwrap().is_none());
    assert_proxied(&mut no_identity, &mut proxy);
    assert!(
        CarPlayRxSession::reject()
            .open_iap2()
            .now_or_never()
            .unwrap()
            .is_none()
    );
}

#[test]
fn airplay_uses_the_wireless_identity_and_keeps_ownership_after_endpoint_failure() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    ready(&mut car, &Arc::new(Capture::default()));
    let identity = wireless_identity();
    let expected = CarPlayServerSession::id(&identity);
    let (mut sink, mut proxy) = airplay_sink(location, Some(identity));
    assert!(sink.iap2_is_optional());
    // Creating the endpoint is immediate; authentication runs in the event loop.
    let mut endpoint = sink.open_iap2().now_or_never().unwrap().unwrap();
    let out = Arc::new(Capture::default());
    respond(&mut endpoint, &out, StartIdentification::default());
    let packets = out.take();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].cast::<IdentificationInformation>(), Some(&expected));
    assert_eq!(
        block_on(sink.on_command_raw(&iap2_command()))
            .unwrap()
            .status,
        HttpStatus::Ok
    );
    assert!(proxy.next().now_or_never().is_none());
    // Optional endpoint failure must not reroute its remaining frames to USB.
    drop(endpoint);
    assert_eq!(
        block_on(sink.on_command_raw(&iap2_command()))
            .unwrap()
            .status,
        HttpStatus::Ok
    );
    assert!(proxy.next().now_or_never().is_none());
}

fn bluetooth(location: LocationBridge) -> CsmSessionBox {
    // The production Bluetooth factory uses this adapter and the same bridge.
    Iap2LocationBridge::new(None, wireless_identity(), location, false)
}

fn build_12_request() -> StartLocationInformation {
    StartLocationInformation {
        global_positioning_system_fix_data: CsmFlag::Yes,
        recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
        vehicle_speed_data: CsmFlag::Yes,
        nmea_sentence_gpgga_interval_min: Some(250),
        nmea_sentence_gprmc_interval_min: Some(250),
        nmea_sentence_pascd_interval_min: Some(20),
        ..StartLocationInformation::default()
    }
}

#[test]
fn bluetooth_start_delivers_over_airplay_after_authentication_without_another_start() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    let bt_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let mut bluetooth = bluetooth(location.clone());
    respond(&mut bluetooth, &bt_out, StartIdentification::default());
    let bt_id = bt_out.take();
    let bt_id = bt_id[0].cast::<IdentificationInformation>().unwrap();
    respond(&mut bluetooth, &bt_out, build_12_request());
    pump(&mut car, &car_out);
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].cast::<StartLocationInformation>(), Some(&build_12_request()));

    let (mut sink, _proxy) = airplay_sink(location.clone(), Some(wireless_identity()));
    let mut airplay = block_on(sink.open_iap2()).unwrap();
    block_on(airplay.start(phone_out.clone())).unwrap();
    respond(&mut airplay, &phone_out, StartIdentification::default());
    let packets = phone_out.take();
    assert_eq!(packets[0].cast::<IdentificationInformation>(), Some(bt_id));
    let mut update = sample();
    update.nmea_sentence.insert(0, "$GNGGA,1*00\r\n".into());
    let mut unfiltered = update.clone();
    unfiltered
        .nmea_sentence
        .extend(["$GPGSV,unrequested".into(), "$UNKNOWN,ignored".into()]);
    respond(&mut car, &car_out, unfiltered);
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    assert!(airplay.wait_event().now_or_never().is_none());
    respond(&mut airplay, &phone_out, IdentificationAccepted::default());
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    assert!(phone_out.take().is_empty());
    respond(&mut airplay, &phone_out, AuthenticationSucceeded::default());
    pump(&mut airplay, &phone_out);
    assert_eq!(take_location(&phone_out), update);
    assert!(bt_out.take().is_empty());

    // disableBluetooth destroys the BT session, but is not StopLocationInformation.
    drop(bluetooth);
    assert!(location.is_requested());
    assert!(!car.process_event(car_out.clone()).unwrap());
    respond(&mut car, &car_out, sample());
    pump(&mut airplay, &phone_out);
    assert_eq!(take_location(&phone_out), sample());

    drop(airplay);
    pump(&mut car, &car_out);
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
    assert!(!location.is_requested());
}

fn stop_after_bluetooth_start(over_bluetooth: bool) {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let mut bluetooth = bluetooth(location.clone());
    let mut airplay = phone(location.clone());
    respond(&mut bluetooth, &phone_out, build_12_request());
    pump(&mut car, &car_out);
    assert_eq!(car_out.location_commands().len(), 1);
    respond(&mut car, &car_out, sample());
    pump(&mut airplay, &phone_out);
    assert_eq!(take_location(&phone_out), sample());
    respond(&mut car, &car_out, sample());
    airplay
        .wait_event()
        .now_or_never()
        .expect("pending data before Stop");

    let stopping_transport = if over_bluetooth { &mut bluetooth } else { &mut airplay };
    respond(stopping_transport, &phone_out, StopLocationInformation::default());
    assert!(!location.is_requested());
    // Buffered data must not escape before the car processes Stop.
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    // A late fix also reconciles Stop, consuming the request notification.
    respond(&mut car, &car_out, sample());
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    assert!(airplay.wait_event().now_or_never().is_none());
    assert!(phone_out.take().is_empty());
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
}

#[test]
fn airplay_stop_cancels_a_bluetooth_subscription() {
    stop_after_bluetooth_start(false);
}

#[test]
fn bluetooth_stop_cancels_airplay_delivery() {
    stop_after_bluetooth_start(true);
}

#[test]
fn no_location_delivery_without_start_on_either_transport() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let mut bluetooth = bluetooth(location.clone());
    let mut airplay = phone(location);
    respond(&mut car, &car_out, sample());
    assert!(!car.process_event(car_out.clone()).unwrap());
    assert!(car_out.location_commands().is_empty());
    assert!(!bluetooth.process_event(phone_out.clone()).unwrap());
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    assert!(airplay.wait_event().now_or_never().is_none());
    assert!(phone_out.take().is_empty());
}

#[test]
fn bluetooth_stop_before_airplay_creation_cancels_the_handoff() {
    let location = LocationBridge::default();
    let phone_out = Arc::new(Capture::default());
    let mut bluetooth = bluetooth(location.clone());
    respond(&mut bluetooth, &phone_out, build_12_request());
    respond(&mut bluetooth, &phone_out, StopLocationInformation::default());
    let mut airplay = phone(location.clone());
    let mut car = CarPlayClientSession::with_location(location);
    let car_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    respond(&mut car, &car_out, sample());
    assert!(car_out.location_commands().is_empty());
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    assert!(phone_out.take().is_empty());
}

#[test]
fn replacement_bluetooth_request_discards_fixes_and_delivers_only_requested_speed() {
    let location = LocationBridge::default();
    let mut car = CarPlayClientSession::with_location(location.clone());
    let car_out = Arc::new(Capture::default());
    let phone_out = Arc::new(Capture::default());
    ready(&mut car, &car_out);
    let mut bluetooth = bluetooth(location.clone());
    let mut airplay = phone(location);
    respond(&mut bluetooth, &phone_out, build_12_request());
    pump(&mut car, &car_out);
    car_out.take();
    respond(&mut car, &car_out, sample());
    airplay.wait_event().now_or_never().unwrap();

    let speed_only = StartLocationInformation {
        vehicle_speed_data: CsmFlag::Yes,
        nmea_sentence_pascd_interval_min: Some(20),
        ..StartLocationInformation::default()
    };
    respond(&mut bluetooth, &phone_out, speed_only.clone());
    assert!(!airplay.process_event(phone_out.clone()).unwrap());
    assert!(airplay.wait_event().now_or_never().is_none());
    pump(&mut car, &car_out);
    let commands = car_out.location_commands();
    assert_eq!(commands.len(), 2);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
    assert_eq!(commands[1].cast::<StartLocationInformation>(), Some(&speed_only));
    respond(&mut car, &car_out, sample());
    pump(&mut airplay, &phone_out);
    assert_eq!(take_location(&phone_out).nmea_sentence, vec![sample().nmea_sentence[1].clone()]);
}

#[test]
fn a_new_bluetooth_connection_does_not_inherit_an_unfinished_handoff() {
    let location = LocationBridge::default();
    let phone_out = Arc::new(Capture::default());
    let mut first = bluetooth(location.clone());
    respond(&mut first, &phone_out, build_12_request());
    drop(first);
    assert!(location.is_requested()); // Still available for this phone's AirPlay handoff.
    let _next = bluetooth(location.clone());
    assert!(!location.is_requested());
}
