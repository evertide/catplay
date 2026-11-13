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
        messages_sent_by_accessory: IdentificationInformation::pack_ids(&[PowerSourceUpdate::PACKET_ID, StartPowerUpdates::PACKET_ID]),
        ..IdentificationInformation::default()
    }
}

fn ready(car: &mut CarPlayClientSession, handle: &Arc<Capture>) {
    block_on(car.start(handle.clone())).unwrap();
    respond(car, handle, car_id());
    respond(car, handle, AuthenticationResponse::default());
    respond(car, handle, PowerSourceUpdate::default());
    respond(car, handle, StartPowerUpdates::default());
}

fn sample() -> LocationInformation {
    LocationInformation {
        nmea_sentence: vec!["$GPRMC,120000,V,,,,,,,131126,,,N*00\r\n".into(), "$PASCD,0*00\r\n".into()],
    }
}

#[test]
fn early_request_waits_for_authentication_and_both_power_replies() {
    for auth_first in [false, true] {
        for source_first in [false, true] {
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
            respond(&mut car, &car_out, car_id());
            if auth_first {
                respond(&mut car, &car_out, AuthenticationResponse::default());
            }
            let mut power: [CsmPacketBox; 2] = [PowerSourceUpdate::default().into(), StartPowerUpdates::default().into()];
            if !source_first {
                power.reverse();
            }
            let [first, second] = power;
            respond(&mut car, &car_out, first);
            assert!(car_out.location_commands().is_empty());
            respond(&mut car, &car_out, second);
            if !auth_first {
                assert!(car_out.location_commands().is_empty());
                respond(&mut car, &car_out, AuthenticationResponse::default());
            }
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
        }
    }
}

#[test]
fn no_commands_without_subscription_capabilities_or_power() {
    for (subscribe, power, id) in [
        (false, true, car_id()),
        (true, false, car_id()),
        (
            true,
            false,
            IdentificationInformation {
                messages_sent_by_accessory: IdentificationInformation::pack_ids(&[]),
                ..car_id()
            },
        ),
        (
            true,
            true,
            IdentificationInformation {
                location_information_component: None,
                ..car_id()
            },
        ),
        (
            true,
            true,
            IdentificationInformation {
                location_information_component: Some(LocationInformationComponent::default()),
                ..car_id()
            },
        ),
        (
            true,
            true,
            IdentificationInformation {
                messages_received_from_device: IdentificationInformation::pack_ids(&[StartLocationInformation::PACKET_ID]),
                ..car_id()
            },
        ),
        (
            true,
            true,
            IdentificationInformation {
                messages_received_from_device: IdentificationInformation::pack_ids(&[StopLocationInformation::PACKET_ID]),
                ..car_id()
            },
        ),
    ] {
        let location = LocationBridge::default();
        let mut car = CarPlayClientSession::with_location(location.clone());
        let mut phone = phone(location);
        let car_out = Arc::new(Capture::default());
        let phone_out = Arc::new(Capture::default());
        block_on(car.start(car_out.clone())).unwrap();
        respond(&mut car, &car_out, id);
        respond(&mut car, &car_out, AuthenticationResponse::default());
        if power {
            respond(&mut car, &car_out, PowerSourceUpdate::default());
            respond(&mut car, &car_out, StartPowerUpdates::default());
        }
        if subscribe {
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
fn stop_before_power_ready_cancels_deferred_request() {
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
fn replacement_usb_session_repeats_negotiation_for_the_live_bluetooth_request() {
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
