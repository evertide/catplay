use std::sync::Arc;

use catplay_carplay_rx_gadget::{CarPlayServerSession, CarPlayServerSessionEventRx, CarPlayServerSessionEventTx, CarPlaySessionIdentity};
use catplay_carplay_tx_gadget::{
    carplay_client_session::CarPlayClientSession,
    location::{CarPlayLocationControl, CarPlayLocationSession, LocationRequest},
};
use catplay_csm::{
    decoder::{CsmFlag, CsmPacketBox, CsmPacketId},
    msg::*,
};
use catplay_iap2_client::{CsmSession, test_support::Capture};
use catplay_util::mpsc;
use futures::{FutureExt, StreamExt, executor::block_on};

fn respond(session: &mut dyn CsmSession, handle: &Arc<Capture>, packet: impl Into<CsmPacketBox>) {
    block_on(session.respond(packet.into(), handle.clone())).unwrap();
}

fn pump(session: &mut dyn CsmSession, handle: &Arc<Capture>) {
    session
        .wait_event()
        .now_or_never()
        .expect("queued application event must wake the session");
    assert!(session.process_event(handle.clone()).unwrap());
}

fn volvo() -> LocationInformationComponent {
    LocationInformationComponent {
        global_positioning_system_fix_data: CsmFlag::Yes,
        recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
        vehicle_speed_data: CsmFlag::Yes,
        ..LocationInformationComponent::default()
    }
}

fn identify(session: &mut CarPlayClientSession, handle: &Arc<Capture>, component: Option<LocationInformationComponent>) {
    respond(
        session,
        handle,
        IdentificationInformation {
            location_information_component: component,
            messages_received_from_device: IdentificationInformation::pack_ids(&[
                StartLocationInformation::PACKET_ID,
                StopLocationInformation::PACKET_ID,
            ]),
            messages_sent_by_accessory: IdentificationInformation::pack_ids(&[StartPowerUpdates::PACKET_ID, PowerSourceUpdate::PACKET_ID]),
            ..IdentificationInformation::default()
        },
    );
}

fn ready(session: &mut CarPlayClientSession, handle: &Arc<Capture>, component: Option<LocationInformationComponent>) {
    block_on(session.start(handle.clone())).unwrap();
    identify(session, handle, component);
    respond(session, handle, AuthenticationResponse::default());
    respond(session, handle, PowerSourceUpdate::default());
    respond(session, handle, StartPowerUpdates::default());
}

fn connect(
    session: &mut CarPlayClientSession,
    handle: &Arc<Capture>,
    control: &CarPlayLocationControl,
) -> (mpsc::UnboundedSender<LocationRequest>, mpsc::UnboundedReceiver<LocationInformation>) {
    let (peer, requests, updates) = CarPlayLocationSession::new();
    assert!(control.connect(peer));
    pump(session, handle);
    (requests, updates)
}

fn sample() -> LocationInformation {
    // Multiple sentences in one packet must survive unchanged, including invalid fixes.
    LocationInformation {
        nmea_sentence: vec!["$GPRMC,120000,V,,,,,,,131126,,,N*00\r\n".into(), "$PASCD,0*00\r\n".into()],
    }
}

#[test]
fn early_subscription_waits_for_authentication_and_both_power_replies_in_either_order() {
    for source_first in [false, true] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        let (requests, _updates) = connect(&mut session, &handle, &control);
        requests
            .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
            .unwrap();
        pump(&mut session, &handle);
        assert!(handle.location_commands().is_empty());

        block_on(session.start(handle.clone())).unwrap();
        identify(&mut session, &handle, Some(volvo()));
        respond(&mut session, &handle, AuthenticationResponse::default());
        assert!(handle.location_commands().is_empty());

        if source_first {
            respond(&mut session, &handle, PowerSourceUpdate::default());
        } else {
            respond(&mut session, &handle, StartPowerUpdates::default());
        }
        assert!(handle.location_commands().is_empty());
        if source_first {
            respond(&mut session, &handle, StartPowerUpdates::default());
        } else {
            respond(&mut session, &handle, PowerSourceUpdate::default());
        }
        let packets = handle.take();
        let start = packets
            .last()
            .unwrap()
            .cast::<StartLocationInformation>()
            .unwrap();
        assert_eq!(
            start,
            &StartLocationInformation {
                global_positioning_system_fix_data: CsmFlag::Yes,
                recommended_minimum_specific_gps_transit_data: CsmFlag::Yes,
                vehicle_speed_data: CsmFlag::Yes,
                ..StartLocationInformation::default()
            }
        );
        assert_eq!(packets.len(), if source_first { 6 } else { 2 });
        assert!(
            packets[..packets.len() - 1]
                .iter()
                .all(|p| p.cast::<PowerUpdate>().is_some())
        );
        respond(&mut session, &handle, StartPowerUpdates::default());
        assert!(handle.location_commands().is_empty());
    }
}

#[test]
fn a_single_advertised_power_exchange_still_requires_authentication() {
    for power_message in [StartPowerUpdates::PACKET_ID, PowerSourceUpdate::PACKET_ID] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        block_on(session.start(handle.clone())).unwrap();
        respond(
            &mut session,
            &handle,
            IdentificationInformation {
                location_information_component: Some(volvo()),
                messages_received_from_device: IdentificationInformation::pack_ids(&[
                    StartLocationInformation::PACKET_ID,
                    StopLocationInformation::PACKET_ID,
                ]),
                messages_sent_by_accessory: IdentificationInformation::pack_ids(&[power_message]),
                ..IdentificationInformation::default()
            },
        );
        let (requests, _updates) = connect(&mut session, &handle, &control);
        requests
            .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
            .unwrap();
        pump(&mut session, &handle);
        if power_message == StartPowerUpdates::PACKET_ID {
            respond(&mut session, &handle, StartPowerUpdates::default());
        } else {
            respond(&mut session, &handle, PowerSourceUpdate::default());
        }
        assert!(handle.location_commands().is_empty());
        respond(&mut session, &handle, AuthenticationResponse::default());
        assert_eq!(handle.location_commands().len(), 1);
    }
}

#[test]
fn no_subscription_no_component_and_empty_intersection_send_no_location_commands() {
    for component in [None, Some(LocationInformationComponent::default()), Some(volvo())] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        let available = component.as_ref().is_some_and(|component| component.global_positioning_system_fix_data == CsmFlag::Yes);
        ready(&mut session, &handle, component);
        assert_eq!(control.is_available(), available);
        assert!(handle.location_commands().is_empty());
        let (requests, mut updates) = connect(&mut session, &handle, &control);
        // Volvo doesn't advertise gyro data; the other cases advertise nothing.
        requests
            .unbounded_send(LocationRequest::Start(StartLocationInformation {
                vehicle_gyro_data: CsmFlag::Yes,
                ..StartLocationInformation::default()
            }))
            .unwrap();
        pump(&mut session, &handle);
        respond(&mut session, &handle, sample());
        assert!(updates.next().now_or_never().is_none());
        requests.unbounded_send(LocationRequest::Stop).unwrap();
        pump(&mut session, &handle);
        drop(session);
        assert!(handle.location_commands().is_empty());
        assert!(!control.is_available());
    }
}

#[test]
fn stop_or_disconnect_before_power_ready_cancels_deferred_start() {
    for disconnect in [false, true] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        let (requests, _updates) = connect(&mut session, &handle, &control);
        requests
            .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
            .unwrap();
        pump(&mut session, &handle);
        if disconnect {
            drop(requests);
        } else {
            requests.unbounded_send(LocationRequest::Stop).unwrap();
            pump(&mut session, &handle);
            drop(requests);
        }
        pump(&mut session, &handle);
        ready(&mut session, &handle, Some(volvo()));
        assert!(handle.location_commands().is_empty());
    }
}

#[test]
fn queued_stop_is_consumed_before_the_last_power_reply_can_start_the_feed() {
    let (mut session, control) = CarPlayClientSession::new();
    let handle = Capture::new();
    block_on(session.start(handle.clone())).unwrap();
    identify(&mut session, &handle, Some(volvo()));
    respond(&mut session, &handle, AuthenticationResponse::default());
    respond(&mut session, &handle, PowerSourceUpdate::default());
    let (requests, _updates) = connect(&mut session, &handle, &control);
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
        .unwrap();
    pump(&mut session, &handle);
    requests.unbounded_send(LocationRequest::Stop).unwrap();
    // Deliberately do not pump the application channel before the car packet.
    respond(&mut session, &handle, StartPowerUpdates::default());
    assert!(handle.location_commands().is_empty());
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
        .unwrap();
    requests.unbounded_send(LocationRequest::Stop).unwrap();
    pump(&mut session, &handle);
    assert!(handle.location_commands().is_empty());
}

#[test]
fn forwarding_stops_on_stop_and_request_channel_close() {
    let (mut session, control) = CarPlayClientSession::new();
    let handle = Capture::new();
    ready(&mut session, &handle, Some(volvo()));
    let (requests, mut updates) = connect(&mut session, &handle, &control);
    respond(&mut session, &handle, sample());
    assert!(updates.next().now_or_never().is_none());
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
        .unwrap();
    pump(&mut session, &handle);
    assert_eq!(handle.location_commands().len(), 1);
    respond(&mut session, &handle, sample());
    assert_eq!(updates.next().now_or_never(), Some(Some(sample())));
    requests.unbounded_send(LocationRequest::Stop).unwrap();
    pump(&mut session, &handle);
    assert!(
        handle.location_commands()[0]
            .cast::<StopLocationInformation>()
            .is_some()
    );
    respond(&mut session, &handle, sample());
    assert!(updates.next().now_or_never().is_none());
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
        .unwrap();
    pump(&mut session, &handle);
    handle.take();
    drop(requests);
    pump(&mut session, &handle);
    let commands = handle.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
    assert_eq!(updates.next().now_or_never(), Some(None));
}

#[test]
fn repeated_start_is_idempotent_and_changed_request_replaces_the_subscription() {
    let (mut session, control) = CarPlayClientSession::new();
    let handle = Capture::new();
    ready(&mut session, &handle, Some(volvo()));
    let (requests, _updates) = connect(&mut session, &handle, &control);
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
        .unwrap();
    pump(&mut session, &handle);
    handle.take();
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::all()))
        .unwrap();
    pump(&mut session, &handle);
    assert!(handle.location_commands().is_empty());
    let request = StartLocationInformation {
        vehicle_speed_data: CsmFlag::Yes,
        nmea_sentence_pascd_interval_min: Some(500),
        ..StartLocationInformation::default()
    };
    requests
        .unbounded_send(LocationRequest::Start(request.clone()))
        .unwrap();
    pump(&mut session, &handle);
    let commands = handle.location_commands();
    assert_eq!(commands.len(), 2);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
    assert_eq!(commands[1].cast::<StartLocationInformation>(), Some(&request));
    requests
        .unbounded_send(LocationRequest::Start(StartLocationInformation::default()))
        .unwrap();
    pump(&mut session, &handle);
    let commands = handle.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
}

#[test]
fn idle_transport_does_not_steal_and_stopped_transport_resumes_the_previous_subscriber() {
    for disconnect in [false, true] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        ready(&mut session, &handle, Some(volvo()));
        let (old_requests, mut old_updates) = connect(&mut session, &handle, &control);
        old_requests.unbounded_send(LocationRequest::Start(StartLocationInformation::all())).unwrap();
        pump(&mut session, &handle);
        handle.take();
        let (requests, mut updates) = connect(&mut session, &handle, &control);
        assert!(handle.location_commands().is_empty());
        respond(&mut session, &handle, sample());
        assert_eq!(old_updates.next().now_or_never(), Some(Some(sample())));

        // A different request ensures fallback must actually restore the old
        // car request, not merely redirect unchanged updates.
        requests.unbounded_send(LocationRequest::Start(StartLocationInformation {
            vehicle_speed_data: CsmFlag::Yes,
            ..StartLocationInformation::default()
        })).unwrap();
        pump(&mut session, &handle);
        assert_eq!(handle.location_commands().len(), 2);
        respond(&mut session, &handle, sample());
        assert!(old_updates.next().now_or_never().is_none());
        assert_eq!(updates.next().now_or_never(), Some(Some(sample())));

        if disconnect {
            drop(requests);
        } else {
            requests.unbounded_send(LocationRequest::Stop).unwrap();
        }
        pump(&mut session, &handle);
        let commands = handle.location_commands();
        assert_eq!(commands.len(), 2);
        assert!(commands[0].cast::<StopLocationInformation>().is_some());
        assert_eq!(commands[1].cast::<StartLocationInformation>().unwrap().global_positioning_system_fix_data, CsmFlag::Yes);
        respond(&mut session, &handle, sample());
        assert_eq!(old_updates.next().now_or_never(), Some(Some(sample())));
        drop(session);
        // The USB link is being destroyed; no undeliverable Stop is queued.
        assert!(handle.location_commands().is_empty());
        assert_eq!(old_updates.next().now_or_never(), Some(None));
        assert_eq!(updates.next().now_or_never(), Some(None));
        assert!(!control.is_available());
    }
}

#[test]
fn both_start_and_stop_must_be_advertised_before_any_location_command() {
    for commands in [
        vec![],
        vec![StartLocationInformation::PACKET_ID],
        vec![StopLocationInformation::PACKET_ID],
        vec![StartLocationInformation::PACKET_ID, StopLocationInformation::PACKET_ID],
    ] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        block_on(session.start(handle.clone())).unwrap();
        respond(&mut session, &handle, IdentificationInformation {
            location_information_component: Some(volvo()),
            messages_received_from_device: IdentificationInformation::pack_ids(&commands),
            messages_sent_by_accessory: IdentificationInformation::pack_ids(&[StartPowerUpdates::PACKET_ID]),
            ..IdentificationInformation::default()
        });
        respond(&mut session, &handle, AuthenticationResponse::default());
        respond(&mut session, &handle, StartPowerUpdates::default());
        let (requests, _updates) = connect(&mut session, &handle, &control);
        requests.unbounded_send(LocationRequest::Start(StartLocationInformation::all())).unwrap();
        pump(&mut session, &handle);
        assert_eq!(control.is_available(), commands.len() == 2);
        assert_eq!(handle.location_commands().len(), usize::from(commands.len() == 2));
        requests.unbounded_send(LocationRequest::Stop).unwrap();
        pump(&mut session, &handle);
        assert_eq!(handle.location_commands().len(), usize::from(commands.len() == 2));
    }
}

#[test]
fn absent_or_missing_power_exchange_never_starts_location() {
    for messages in [vec![], vec![StartPowerUpdates::PACKET_ID], vec![PowerSourceUpdate::PACKET_ID]] {
        let (mut session, control) = CarPlayClientSession::new();
        let handle = Capture::new();
        block_on(session.start(handle.clone())).unwrap();
        respond(&mut session, &handle, IdentificationInformation {
            location_information_component: Some(volvo()),
            messages_received_from_device: IdentificationInformation::pack_ids(&[
                StartLocationInformation::PACKET_ID,
                StopLocationInformation::PACKET_ID,
            ]),
            messages_sent_by_accessory: IdentificationInformation::pack_ids(&messages),
            ..IdentificationInformation::default()
        });
        respond(&mut session, &handle, AuthenticationResponse::default());
        let (requests, _updates) = connect(&mut session, &handle, &control);
        requests.unbounded_send(LocationRequest::Start(StartLocationInformation::all())).unwrap();
        pump(&mut session, &handle);
        for _ in 0..3 {
            assert!(!session.process_event(handle.clone()).unwrap());
        }
        assert!(handle.location_commands().is_empty());
    }
}

#[test]
fn reidentification_stops_active_request_and_requires_fresh_authentication_and_power() {
    let (mut session, control) = CarPlayClientSession::new();
    let handle = Capture::new();
    ready(&mut session, &handle, Some(volvo()));
    let (requests, _updates) = connect(&mut session, &handle, &control);
    requests.unbounded_send(LocationRequest::Start(StartLocationInformation::all())).unwrap();
    pump(&mut session, &handle);
    handle.take();
    identify(&mut session, &handle, Some(volvo()));
    let commands = handle.location_commands();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].cast::<StopLocationInformation>().is_some());
    respond(&mut session, &handle, AuthenticationResponse::default());
    respond(&mut session, &handle, PowerSourceUpdate::default());
    assert!(handle.location_commands().is_empty());
    respond(&mut session, &handle, StartPowerUpdates::default());
    assert_eq!(handle.location_commands().len(), 1);
    identify(&mut session, &handle, None);
    assert_eq!(handle.location_commands().len(), 1);
    respond(&mut session, &handle, AuthenticationResponse::default());
    respond(&mut session, &handle, PowerSourceUpdate::default());
    respond(&mut session, &handle, StartPowerUpdates::default());
    assert!(handle.location_commands().is_empty());
    assert!(!control.is_available());
}

#[test]
fn iphone_forwards_nmea_only_while_subscribed() {
    let (mut session, _feeds, mut events) = CarPlayServerSession::new(
        None,
        CarPlaySessionIdentity {
            has_gps: true,
            ..CarPlaySessionIdentity::default()
        },
    );
    let handle = Capture::new();
    session.on_event(CarPlayServerSessionEventRx::FeedGpsNmea(sample()), handle.clone()).unwrap();
    assert!(handle.take().is_empty());
    respond(&mut session, &handle, StartLocationInformation::all());
    assert!(matches!(
        events.next().now_or_never(),
        Some(Some(CarPlayServerSessionEventTx::StartLocationInformation(_)))
    ));
    session.on_event(CarPlayServerSessionEventRx::FeedGpsNmea(sample()), handle.clone()).unwrap();
    let packets = handle.take();
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].cast::<LocationInformation>(), Some(&sample()));

    respond(&mut session, &handle, StopLocationInformation::default());
    session.on_event(CarPlayServerSessionEventRx::FeedGpsNmea(sample()), handle.clone()).unwrap();
    assert!(handle.take().is_empty());
    assert!(matches!(
        events.next().now_or_never(),
        Some(Some(CarPlayServerSessionEventTx::StopLocationInformat(_)))
    ));
}
