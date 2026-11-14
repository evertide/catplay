use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use catplay_csm::{
    decoder::{CsmFlag, CsmPacketId},
    msg::{
        IdentificationInformation, LocationInformation, LocationInformationComponent, StartLocationInformation, StopLocationInformation,
    },
};
use catplay_iap2_client::{CsmClientHandleRef, CsmSessionResult};
use catplay_util::{EventSleeper, EventToken};
use log::info;
use tokio::sync::watch;

type PendingLocation = VecDeque<(String, VecDeque<String>)>;

// Covers the seven advertised types and alternate talker IDs without an unbounded backlog.
const MAX_PENDING_SENTENCE_TYPES: usize = 16;
// Leave room below the 65,529-byte CSM payload limit and for iAP2 framing.
const MAX_LOCATION_PAYLOAD_BYTES: usize = 60 * 1024;

#[derive(Clone)]
struct LocationUpdates {
    changed: watch::Sender<()>,
    buffer: Arc<Mutex<PendingLocation>>,
}

impl LocationUpdates {
    fn push(&self, packet: &LocationInformation, request: &StartLocationInformation) {
        let mut incoming = BTreeMap::<&str, VecDeque<String>>::new();
        for sentence in &packet.nmea_sentence {
            let kind = sentence.split_once(',').map_or("", |(kind, _)| kind);
            if requested_sentence(request, kind) && sentence.len() + 5 <= MAX_LOCATION_PAYLOAD_BYTES {
                incoming
                    .entry(kind)
                    .or_default()
                    .push_back(sentence.clone());
            }
        }
        if incoming.is_empty() {
            return;
        }

        let mut buffer = self.buffer.lock().unwrap();
        for (kind, sentences) in incoming {
            // Replace multipart data together, but transmit each sentence separately.
            if sentences
                .iter()
                .map(|sentence| sentence.len() + 5)
                .sum::<usize>()
                > MAX_LOCATION_PAYLOAD_BYTES
            {
                continue;
            }
            if let Some((_, pending)) = buffer.iter_mut().find(|(pending_kind, _)| pending_kind == kind) {
                *pending = sentences;
            } else if buffer.len() < MAX_PENDING_SENTENCE_TYPES {
                buffer.push_back((kind.into(), sentences));
            }
        }
        // Publish under the buffer lock so take() cannot mark unseen data as read.
        self.changed.send_replace(());
    }
}

#[derive(Default)]
struct LocationState {
    next_owner: u64,
    owner: u64,
    updates: Option<LocationUpdates>,
}

/// The current iPhone request, shared with each successive USB iAP2 session.
/// Negotiation and the active car request remain local to the USB session.
#[derive(Clone)]
pub struct LocationBridge {
    request: watch::Sender<Option<StartLocationInformation>>,
    state: Arc<Mutex<LocationState>>,
    car: Arc<Mutex<Weak<AtomicBool>>>,
}

impl Default for LocationBridge {
    fn default() -> Self {
        Self {
            request: watch::channel(None).0,
            state: Default::default(),
            car: Default::default(),
        }
    }
}

impl LocationBridge {
    /// Only the newest USB session determines whether to open the AirPlay endpoint.
    pub fn is_available(&self) -> bool {
        self.car
            .lock()
            .unwrap()
            .upgrade()
            .is_some_and(|car| car.load(Ordering::Relaxed))
    }

    /// Opening Bluetooth reserves an ID without changing the selected request.
    pub fn new_owner(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.next_owner += 1;
        state.next_owner
    }

    pub fn set_request(&self, owner: u64, request: Option<StartLocationInformation>) {
        self.update_request(&mut self.state.lock().unwrap(), owner, request);
    }

    fn update_request(&self, state: &mut LocationState, owner: u64, request: Option<StartLocationInformation>) {
        // Only Start can select a newer Bluetooth session. Stale controls are inert.
        if owner < state.owner || (request.is_none() && owner != state.owner) {
            return;
        }
        if let Some(updates) = state.updates.as_ref() {
            updates.buffer.lock().unwrap().clear();
        }
        if owner != state.owner {
            state.updates = None;
        }
        state.owner = owner;
        self.request.send_replace(request);
    }

    pub fn is_requested(&self) -> bool {
        self.request.borrow().is_some()
    }

    /// AirPlay owns delivery; a Bluetooth disconnect must not cancel the request.
    pub fn subscribe(&self) -> LocationSubscription {
        let mut state = self.state.lock().unwrap();
        if state.owner == 0 {
            // AirPlay can open before the first Start, or without Bluetooth.
            state.next_owner = state.next_owner.max(1);
            state.owner = state.next_owner;
        }
        let (changed, updates) = watch::channel(());
        let buffer = Arc::new(Mutex::new(PendingLocation::new()));
        if let Some(previous) = state.updates.take() {
            previous.buffer.lock().unwrap().clear();
        }
        state.updates = Some(LocationUpdates {
            changed,
            buffer: buffer.clone(),
        });
        LocationSubscription {
            updates,
            buffer,
            pending: false,
            bridge: self.clone(),
            owner: state.owner,
        }
    }
}

/// Keeps the latest unconsumed message per sentence type. Dropping the current
/// owner's receiver cancels its request and wakes every USB session.
pub struct LocationSubscription {
    updates: watch::Receiver<()>,
    buffer: Arc<Mutex<PendingLocation>>,
    pending: bool,
    bridge: LocationBridge,
    owner: u64,
}

impl EventSleeper for LocationSubscription {
    async fn sleep(&mut self) -> Option<EventToken> {
        let updates = &mut self.updates;
        // Stop/replacement can clear a buffer whose notification was already seen.
        while !self.pending || self.buffer.lock().unwrap().is_empty() {
            updates.changed().await.ok()?;
            self.pending = true;
        }
        Some(EventToken(1))
    }
}

impl LocationSubscription {
    pub fn set_request(&self, request: Option<StartLocationInformation>) {
        let mut state = self.bridge.state.lock().unwrap();
        if self.is_current(&state) {
            self.bridge.update_request(&mut state, self.owner, request);
        }
    }

    fn is_current(&self, state: &LocationState) -> bool {
        state.owner == self.owner
            && state
                .updates
                .as_ref()
                .is_some_and(|updates| Arc::ptr_eq(&updates.buffer, &self.buffer))
    }

    pub fn take(&mut self, max_payload_bytes: usize) -> Option<LocationInformation> {
        let max_payload_bytes = max_payload_bytes.min(MAX_LOCATION_PAYLOAD_BYTES);
        let updates = &mut self.updates;
        let mut buffer = self.buffer.lock().unwrap();
        if !self.pending && !updates.has_changed().unwrap_or(false) {
            return None;
        }
        updates.borrow_and_update();
        self.pending = false;
        while let Some((kind, mut sentences)) = buffer.pop_front() {
            let sentence = sentences.pop_front().unwrap();
            if !sentences.is_empty() {
                // Rotate pending types, including multipart data, after every send.
                buffer.push_back((kind, sentences));
            }
            // Four-byte TLV header plus NUL; never split a sentence across packets.
            if sentence.len() + 5 <= max_payload_bytes {
                self.pending = !buffer.is_empty();
                return Some(LocationInformation {
                    nmea_sentence: vec![sentence],
                });
            }
        }
        None
    }
}

impl Drop for LocationSubscription {
    fn drop(&mut self) {
        let mut state = self.bridge.state.lock().unwrap();
        // A newer BT Start may own the request before its AirPlay endpoint exists.
        if self.is_current(&state) {
            state.updates = None;
            self.bridge.request.send_replace(None);
        }
    }
}

pub(crate) struct CarLocation {
    bridge: LocationBridge,
    requests: watch::Receiver<Option<StartLocationInformation>>,
    supported: Arc<AtomicBool>,
    started: bool,
    authenticated: bool,
    active: Option<StartLocationInformation>,
    rejected: Option<StartLocationInformation>,
    received_location: bool,
}

impl Default for CarLocation {
    fn default() -> Self {
        Self::new(LocationBridge::default())
    }
}

impl CarLocation {
    pub fn new(bridge: LocationBridge) -> Self {
        let supported = Arc::new(AtomicBool::new(false));
        *bridge.car.lock().unwrap() = Arc::downgrade(&supported);
        Self {
            requests: bridge.request.subscribe(),
            bridge,
            supported,
            started: false,
            authenticated: false,
            active: None,
            rejected: None,
            received_location: false,
        }
    }

    pub fn start(&mut self, handle: &CsmClientHandleRef) {
        self.started = !handle.is_downgrade();
        info!("Car GPS bridge started: iAP2={}", self.started);
    }

    pub fn authenticate(&mut self) {
        if !self.authenticated {
            info!("Car GPS authentication complete");
        }
        self.authenticated = true;
    }

    pub async fn wait_event(&mut self) {
        let _ = self.requests.changed().await;
    }

    pub fn identify(&mut self, id: &IdentificationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        self.stop(handle)?;
        self.authenticated = false;
        self.supported
            .store(self.started && supports_location(id), Ordering::Relaxed);
        info!(
            "Car GPS identification complete: component={:?}, wants_rx(StartLocationInformation)={}, \
             wants_rx(StopLocationInformation)={}, wants_tx(LocationInformation)={}, available={}",
            id.location_information_component,
            id.wants_rx(StartLocationInformation::PACKET_ID),
            id.wants_rx(StopLocationInformation::PACKET_ID),
            id.wants_tx(LocationInformation::PACKET_ID),
            self.supported.load(Ordering::Relaxed),
        );
        Ok(())
    }

    pub fn reconcile(&mut self, id: &IdentificationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        let request = self.requests.borrow_and_update().clone();
        // Authentication and explicit location capabilities are sufficient. Power
        // messages are independent and some cars advertise them without sending any.
        let wanted = request
            .as_ref()
            .filter(|_| self.authenticated && self.supported.load(Ordering::Relaxed))
            .and_then(|request| intersect(request, id.location_information_component.as_ref()));
        let rejected = request.filter(|_| self.authenticated && wanted.is_none());
        if rejected != self.rejected {
            if let Some(request) = &rejected {
                info!(
                    "Car GPS request rejected: empty or unsupported subscription, request={request:?}, component={:?}, available={}",
                    id.location_information_component,
                    self.supported.load(Ordering::Relaxed),
                );
            }
            self.rejected = rejected;
        }
        if wanted == self.active {
            return Ok(false);
        }
        self.stop(handle)?;
        if let Some(request) = wanted {
            handle.send(&request)?;
            info!("Car GPS StartLocationInformation sent: {request:?}");
            self.active = Some(request);
        }
        Ok(true)
    }

    pub fn forward(&mut self, packet: &LocationInformation) {
        if !self.received_location {
            info!(
                "Car GPS first LocationInformation received: sentences={}, active={}",
                packet.nmea_sentence.len(),
                self.active.is_some(),
            );
            self.received_location = true;
        }
        if self.active.is_some()
            && let Some(updates) = self.bridge.state.lock().unwrap().updates.as_ref()
            && let Some(request) = self.bridge.request.borrow().as_ref()
        {
            updates.push(packet, request);
        }
    }

    fn stop(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        if self.active.take().is_some() {
            handle.send(&StopLocationInformation::default())?;
            info!("Requested car location stop");
        }
        Ok(())
    }
}

fn requested_sentence(request: &StartLocationInformation, kind: &str) -> bool {
    let flag = match kind {
        "$PASCD" => request.vehicle_speed_data,
        "$PAGCD" => request.vehicle_gyro_data,
        "$PAACD" => request.vehicle_accelerometer_data,
        _ => match kind.strip_prefix('$').and_then(|kind| kind.get(2..)) {
            Some("GGA") => request.global_positioning_system_fix_data,
            Some("RMC") => request.recommended_minimum_specific_gps_transit_data,
            Some("GSV") => request.gps_satellites_in_view,
            Some("HDT") => request.vehicle_heading_data,
            _ => CsmFlag::No,
        },
    };
    flag == CsmFlag::Yes
}

fn supports_location(id: &IdentificationInformation) -> bool {
    id.wants_rx(StartLocationInformation::PACKET_ID)
        && id.wants_rx(StopLocationInformation::PACKET_ID)
        && id.wants_tx(LocationInformation::PACKET_ID)
        && intersect(&StartLocationInformation::all(), id.location_information_component.as_ref()).is_some()
}

fn intersect(request: &StartLocationInformation, component: Option<&LocationInformationComponent>) -> Option<StartLocationInformation> {
    let component = component?;
    let mut result = request.clone();
    macro_rules! filter {
        ($flag:ident, $interval:ident) => {
            if component.$flag != CsmFlag::Yes || request.$flag != CsmFlag::Yes {
                result.$flag = CsmFlag::No;
                result.$interval = None;
            }
        };
    }
    filter!(global_positioning_system_fix_data, nmea_sentence_gpgga_interval_min);
    filter!(recommended_minimum_specific_gps_transit_data, nmea_sentence_gprmc_interval_min);
    filter!(gps_satellites_in_view, nmea_sentence_gpgsv_interval_min);
    filter!(vehicle_speed_data, nmea_sentence_pascd_interval_min);
    filter!(vehicle_gyro_data, nmea_sentence_pagcd_interval_min);
    filter!(vehicle_accelerometer_data, nmea_sentence_paacd_interval_min);
    filter!(vehicle_heading_data, nmea_sentence_gphdt_interval_min);
    // No advertised type corresponds to the reserved interval.
    result.nmea_sentence_rsvd_interval_min = None;

    [
        result.global_positioning_system_fix_data,
        result.recommended_minimum_specific_gps_transit_data,
        result.gps_satellites_in_view,
        result.vehicle_speed_data,
        result.vehicle_gyro_data,
        result.vehicle_accelerometer_data,
        result.vehicle_heading_data,
    ]
    .contains(&CsmFlag::Yes)
    .then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_csm::decoder::CsmPacketRegistry;
    use futures::FutureExt;

    fn subscription() -> (LocationSubscription, LocationUpdates) {
        let bridge = LocationBridge::default();
        let subscription = bridge.subscribe();
        subscription.set_request(Some(StartLocationInformation::all()));
        let updates = bridge.state.lock().unwrap().updates.as_ref().unwrap().clone();
        (subscription, updates)
    }

    fn sentences(sentences: &[&str]) -> LocationInformation {
        LocationInformation {
            nmea_sentence: sentences
                .iter()
                .map(|sentence| (*sentence).into())
                .collect(),
        }
    }

    #[test]
    fn consumed_types_are_not_replayed_but_identical_updates_are_delivered() {
        let (mut subscription, updates) = subscription();
        let first = sentences(&["$GPGGA,1*00\r\n", "$GPRMC,1*00\r\n"]);
        updates.push(&first, &StartLocationInformation::all());
        for sentence in first.nmea_sentence {
            assert_eq!(subscription.take(usize::MAX), Some(sentences(&[&sentence])));
        }
        assert!(subscription.sleep().now_or_never().is_none());

        let rmc = sentences(&["$GPRMC,1*00\r\n"]);
        for _ in 0..2 {
            updates.push(&rmc, &StartLocationInformation::all());
            assert!(subscription.sleep().now_or_never().flatten().is_some());
            assert_eq!(subscription.take(usize::MAX), Some(rmc.clone()));
            assert!(subscription.take(usize::MAX).is_none());
            assert!(subscription.sleep().now_or_never().is_none());
        }
    }

    #[test]
    fn pending_sentence_types_are_bounded_and_existing_types_still_update() {
        let (mut subscription, updates) = subscription();
        updates.push(&sentences(&["$GPGGA,1*00\r\n"]), &StartLocationInformation::all());
        for kind in 0..1000 {
            let talker = format!(
                "{}{}",
                char::from(b'A' + (kind / 26 % 26) as u8),
                char::from(b'A' + (kind % 26) as u8)
            );
            updates.push(&sentences(&[&format!("${talker}GGA,1*00\r\n")]), &StartLocationInformation::all());
        }
        updates.push(&sentences(&["$GPGGA,2*00\r\n"]), &StartLocationInformation::all());
        let mut buffered = Vec::new();
        while let Some(packet) = subscription.take(usize::MAX) {
            assert_eq!(packet.nmea_sentence.len(), 1);
            buffered.extend(packet.nmea_sentence);
        }
        assert_eq!(buffered.len(), MAX_PENDING_SENTENCE_TYPES);
        assert!(buffered.contains(&"$GPGGA,2*00\r\n".into()));

        // Taking an update frees all slots, including for previously unseen types.
        let rmc = sentences(&["$GPRMC,1*00\r\n"]);
        updates.push(&rmc, &StartLocationInformation::all());
        assert_eq!(subscription.take(usize::MAX), Some(rmc));
    }

    #[test]
    fn same_type_sentences_in_one_message_are_replaced_together() {
        let (mut subscription, updates) = subscription();
        updates.push(
            &sentences(&["$GPGSV,2,1,old*00\r\n", "$GPGSV,2,2,old*00\r\n", "$GPRMC,1*00\r\n"]),
            &StartLocationInformation::all(),
        );
        updates.push(
            &sentences(&["$GPGSV,2,1,new*00\r\n", "$GPGSV,2,2,new*00\r\n"]),
            &StartLocationInformation::all(),
        );
        for sentence in ["$GPGSV,2,1,new*00\r\n", "$GPRMC,1*00\r\n", "$GPGSV,2,2,new*00\r\n"] {
            assert_eq!(subscription.take(usize::MAX), Some(sentences(&[sentence])));
        }
        assert!(subscription.take(usize::MAX).is_none());
    }

    #[test]
    fn multipart_sentences_fit_individually_even_when_the_group_exceeds_the_peer_limit() {
        let (mut subscription, updates) = subscription();
        let packet = sentences(&["$GPGSV,2,1,part1", "$GPGSV,2,2,part2"]);
        let budget = packet.nmea_sentence[0].len() + 5;
        updates.push(&packet, &StartLocationInformation::all());
        for sentence in packet.nmea_sentence {
            assert_eq!(subscription.take(budget), Some(sentences(&[&sentence])));
        }
        assert!(subscription.sleep().now_or_never().is_none());
    }

    #[test]
    fn one_sentence_per_message_even_when_the_combined_payload_is_too_large() {
        let (mut subscription, updates) = subscription();
        let gsv = sentences(&[
            &format!("$GPGSV,2,1,{}", "x".repeat(20_000)),
            &format!("$GPGSV,2,2,{}", "x".repeat(20_000)),
        ]);
        let rmc = sentences(&[&format!("$GPRMC,{}", "x".repeat(40_000))]);
        let registry = CsmPacketRegistry::static_registry();
        assert!(registry.encode(&gsv).is_ok());
        assert!(registry.encode(&rmc).is_ok());
        let mut oversized = gsv.clone();
        oversized.nmea_sentence.extend(rmc.nmea_sentence.clone());
        assert!(registry.encode(&oversized).is_err());
        updates.push(&gsv, &StartLocationInformation::all());
        updates.push(&rmc, &StartLocationInformation::all());

        for sentence in [&gsv.nmea_sentence[0], &rmc.nmea_sentence[0], &gsv.nmea_sentence[1]] {
            assert!(subscription.sleep().now_or_never().flatten().is_some());
            let batch = subscription.take(usize::MAX).unwrap();
            assert_eq!(batch.nmea_sentence, vec![sentence.clone()]);
            let encoded = registry.encode(&batch).unwrap();
            assert!(encoded.len() <= MAX_LOCATION_PAYLOAD_BYTES + 6);
            assert_eq!(
                registry
                    .decode(&encoded)
                    .unwrap()
                    .cast::<LocationInformation>(),
                Some(&batch)
            );
        }
        assert!(subscription.take(usize::MAX).is_none());
        assert!(subscription.sleep().now_or_never().is_none());
    }

    #[test]
    fn payload_budget_counts_tlv_headers_nul_terminators_and_utf8_bytes() {
        for budget in [2048 - 16, MAX_LOCATION_PAYLOAD_BYTES] {
            let (mut subscription, updates) = subscription();
            let prefix = "$GPGGA,\u{00e9}";
            let padding = "x".repeat(budget - prefix.len() - 5);
            let exact = sentences(&[&format!("{prefix}{padding}")]);
            let next = sentences(&["$GPRMC,next"]);
            updates.push(&exact, &StartLocationInformation::all());
            updates.push(&next, &StartLocationInformation::all());
            let batch = subscription.take(budget).unwrap();
            assert_eq!(batch, exact);
            assert_eq!(
                CsmPacketRegistry::static_registry()
                    .encode(&batch)
                    .unwrap()
                    .len(),
                budget + 6
            );
            assert_eq!(subscription.take(budget), Some(next));
        }
    }

    #[test]
    fn oversized_sentence_is_dropped_without_blocking_later_updates() {
        for budget in [2048 - 16, MAX_LOCATION_PAYLOAD_BYTES] {
            let (mut subscription, updates) = subscription();
            let prefix = "$GPGSV,";
            let padding = "x".repeat(budget - prefix.len() - 4);
            let oversized = sentences(&[&format!("{prefix}{padding}")]);
            updates.push(&oversized, &StartLocationInformation::all());
            assert!(subscription.take(budget).is_none());
            assert!(subscription.sleep().now_or_never().is_none());
            let next = sentences(&["$GPRMC,new"]);
            updates.push(&oversized, &StartLocationInformation::all());
            updates.push(&next, &StartLocationInformation::all());
            assert_eq!(subscription.take(budget), Some(next));
            assert!(subscription.sleep().now_or_never().is_none());
        }
    }

    #[test]
    fn each_type_requires_both_flags_and_preserves_only_its_interval() {
        macro_rules! check {
            ($flag:ident, $interval:ident) => {
                for requested in [CsmFlag::No, CsmFlag::Yes] {
                    for advertised in [CsmFlag::No, CsmFlag::Yes] {
                        let request = StartLocationInformation {
                            $flag: requested,
                            $interval: Some(1000),
                            nmea_sentence_rsvd_interval_min: Some(50),
                            ..StartLocationInformation::default()
                        };
                        let component = LocationInformationComponent {
                            $flag: advertised,
                            ..LocationInformationComponent::default()
                        };
                        let result = intersect(&request, Some(&component));
                        if requested == CsmFlag::Yes && advertised == CsmFlag::Yes {
                            assert_eq!(
                                result,
                                Some(StartLocationInformation {
                                    nmea_sentence_rsvd_interval_min: None,
                                    ..request
                                })
                            );
                        } else {
                            assert_eq!(result, None);
                        }
                    }
                }
            };
        }
        check!(global_positioning_system_fix_data, nmea_sentence_gpgga_interval_min);
        check!(recommended_minimum_specific_gps_transit_data, nmea_sentence_gprmc_interval_min);
        check!(gps_satellites_in_view, nmea_sentence_gpgsv_interval_min);
        check!(vehicle_speed_data, nmea_sentence_pascd_interval_min);
        check!(vehicle_gyro_data, nmea_sentence_pagcd_interval_min);
        check!(vehicle_accelerometer_data, nmea_sentence_paacd_interval_min);
        check!(vehicle_heading_data, nmea_sentence_gphdt_interval_min);
    }

    #[test]
    fn missing_component_has_no_intersection() {
        assert_eq!(intersect(&StartLocationInformation::all(), None), None);
    }
}
