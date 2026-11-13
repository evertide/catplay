use std::{
    future::poll_fn,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use catplay_csm::{
    decoder::{CsmFlag, CsmPacketId},
    msg::{
        IdentificationInformation, LocationInformation, LocationInformationComponent, PowerSourceUpdate, StartLocationInformation,
        StartPowerUpdates, StopLocationInformation,
    },
};
use catplay_iap2_client::{CsmClientHandleRef, CsmSessionResult};
use catplay_util::{event_select, mpsc, sleeper};
use futures::{FutureExt, Stream, StreamExt};
use log::info;

pub enum LocationRequest {
    Start(StartLocationInformation),
    Stop,
}

#[derive(Clone)]
pub struct CarPlayLocationControl {
    sessions: mpsc::UnboundedSender<CarPlayLocationSession>,
    supported: Arc<AtomicBool>,
}

impl CarPlayLocationControl {
    pub fn is_available(&self) -> bool {
        self.supported.load(Ordering::Relaxed) && !self.sessions.is_closed()
    }

    pub fn connect(&self, session: CarPlayLocationSession) -> bool {
        self.sessions.unbounded_send(session).is_ok()
    }
}

/// One iPhone transport's location subscription. Closing either channel detaches it.
/// The newest subscribed transport is preferred; older live subscriptions are retained
/// so they can resume if that transport stops or disconnects during handover.
pub struct CarPlayLocationSession {
    requests: mpsc::UnboundedReceiver<LocationRequest>,
    updates: mpsc::UnboundedSender<LocationInformation>,
    requested: Option<StartLocationInformation>,
    // The inner None is EOF, distinct from having no pending request.
    pending_request: Option<Option<LocationRequest>>,
    closed: bool,
}

impl CarPlayLocationSession {
    pub fn new() -> (
        Self,
        mpsc::UnboundedSender<LocationRequest>,
        mpsc::UnboundedReceiver<LocationInformation>,
    ) {
        let (requests_tx, requests) = mpsc::unbounded();
        let (updates, updates_rx) = mpsc::unbounded();
        (
            Self {
                requests,
                updates,
                requested: None,
                pending_request: None,
                closed: false,
            },
            requests_tx,
            updates_rx,
        )
    }
}

#[derive(Default)]
pub(crate) struct CarLocation {
    sessions: Option<mpsc::UnboundedReceiver<CarPlayLocationSession>>,
    peers: Vec<CarPlayLocationSession>,
    active: Option<StartLocationInformation>,
    active_peer: Option<usize>,
    component: Option<LocationInformationComponent>,
    accepts_location: bool,
    supported: Arc<AtomicBool>,
    started: bool,
    pub authenticated: bool,
    needs_power_source: bool,
    needs_power_updates: bool,
    pub power_source_replied: bool,
    pub power_updates_replied: bool,
    last_blocked_reason: Option<&'static str>,
}

impl CarLocation {
    pub fn new() -> (Self, CarPlayLocationControl) {
        let (tx, rx) = mpsc::unbounded();
        let location = Self {
            sessions: Some(rx),
            ..Self::default()
        };
        let control = CarPlayLocationControl {
            sessions: tx,
            supported: location.supported.clone(),
        };
        (location, control)
    }

    pub fn start(&mut self, handle: CsmClientHandleRef) {
        self.started = !handle.is_downgrade();
    }

    pub fn identify(&mut self, id: &IdentificationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        // A new identification invalidates the old negotiation, including an active
        // request. Stop under the old negotiated capabilities before resetting them.
        self.stop(handle)?;
        self.active_peer = None;
        self.component = id.location_information_component.clone();
        self.accepts_location = id.wants_rx(StartLocationInformation::PACKET_ID) && id.wants_rx(StopLocationInformation::PACKET_ID);
        self.supported.store(
            self.accepts_location && intersect(&StartLocationInformation::all(), self.component.as_ref()).is_some(),
            Ordering::Relaxed,
        );
        self.needs_power_source = id.wants_tx(PowerSourceUpdate::PACKET_ID);
        self.needs_power_updates = id.wants_tx(StartPowerUpdates::PACKET_ID);
        self.authenticated = false;
        self.power_source_replied = false;
        self.power_updates_replied = false;
        self.last_blocked_reason = None;
        Ok(())
    }

    pub async fn wait_event(&mut self) {
        // Buffer input only: the iAP2 client's reconcile loop serializes state changes
        // with authentication, power replies and received location packets.
        let peers = &mut self.peers;
        event_select!(
            self.sessions,
            sleeper(poll_fn(|cx| {
                for peer in peers.iter_mut() {
                    if peer.pending_request.is_some() {
                        return Poll::Ready(());
                    }
                    if let Poll::Ready(request) = Pin::new(&mut peer.requests).poll_next(cx) {
                        peer.pending_request = Some(request);
                        return Poll::Ready(());
                    }
                }
                Poll::Pending
            }))
        );
    }

    pub fn process_event(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        self.reconcile(handle)
    }

    fn blocked_reason(&self) -> Option<&'static str> {
        if !self.started {
            Some("waiting for the car iAP2 session")
        } else if self.component.is_none() {
            Some("the car has not advertised a location component")
        } else if !self.accepts_location {
            Some("the car has not advertised support for both StartLocationInformation and StopLocationInformation")
        } else if !self.authenticated {
            Some("waiting for car iAP2 authentication")
        } else if !self.power_source_replied && !self.power_updates_replied {
            if !self.needs_power_source && !self.needs_power_updates {
                Some("the car advertised neither power message; GPS remains disabled until a power exchange completes")
            } else {
                Some("waiting for the car's advertised power negotiation; GPS remains disabled if it never arrives")
            }
        } else if (self.needs_power_source && !self.power_source_replied) || (self.needs_power_updates && !self.power_updates_replied) {
            Some("waiting for the remaining advertised power exchange before starting GPS")
        } else {
            None
        }
    }

    pub fn reconcile(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        if let Some(sessions) = self.sessions.as_mut() {
            while let Some(Some(peer)) = sessions.next().now_or_never() {
                self.peers.push(peer);
                consumed = true;
            }
        }

        // Coalesce queued requests before opening the feed, including a Stop/EOF
        // that arrived while identification or power negotiation was in progress.
        for peer in &mut self.peers {
            while let Some(request) = peer.pending_request.take().or_else(|| peer.requests.next().now_or_never()) {
                consumed = true;
                match request {
                    Some(LocationRequest::Start(request)) => peer.requested = Some(request),
                    Some(LocationRequest::Stop) => peer.requested = None,
                    None => {
                        peer.closed = true;
                        break;
                    }
                }
            }
        }
        self.peers
            .retain(|peer| !peer.closed && !peer.updates.is_closed());

        let subscribed = self.peers.iter().any(|peer| peer.requested.is_some());
        let candidate = self.peers.iter().enumerate().rev().find_map(|(index, peer)| {
            peer.requested
                .as_ref()
                .and_then(|request| intersect(request, self.component.as_ref()))
                .map(|request| (index, request))
        });
        // Never infer power readiness from authentication or AirPlay alone.
        // If both power messages were advertised, both replies must have been sent.
        let reason = self.blocked_reason().or_else(|| {
            candidate.is_none().then_some("no requested location types are supported by the car")
        });
        if subscribed
            && reason != self.last_blocked_reason
            && let Some(reason) = reason
        {
            info!("Car GPS request deferred: {reason}");
        }
        self.last_blocked_reason = if subscribed { reason } else { None };
        let (peer, wanted) = match candidate.filter(|_| reason.is_none()) {
            Some((peer, request)) => (Some(peer), Some(request)),
            None => (None, None),
        };

        if wanted != self.active {
            self.stop(handle)?;
            if let Some(request) = wanted {
                handle.send(&request)?;
                info!("Requested car location updates: {request:?}");
                self.active = Some(request);
            }
        }
        self.active_peer = peer;
        Ok(consumed)
    }

    pub fn forward(&mut self, packet: &LocationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        if self.active.is_some()
            && let Some(index) = self.active_peer
            && self.peers[index].updates.unbounded_send(packet.clone()).is_err()
        {
            self.peers[index].closed = true;
            self.reconcile(handle)?;
        }
        Ok(())
    }

    fn stop(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        if self.active.take().is_some() {
            handle.send(&StopLocationInformation::default())?;
            info!("Requested car location stop");
        }
        Ok(())
    }
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
    fn unsupported_intervals_are_removed_from_a_mixed_request() {
        let request = StartLocationInformation {
            nmea_sentence_gpgga_interval_min: Some(1000),
            nmea_sentence_gprmc_interval_min: Some(2000),
            nmea_sentence_gpgsv_interval_min: Some(3000),
            nmea_sentence_pascd_interval_min: Some(4000),
            nmea_sentence_pagcd_interval_min: Some(5000),
            nmea_sentence_paacd_interval_min: Some(6000),
            nmea_sentence_gphdt_interval_min: Some(7000),
            ..StartLocationInformation::all()
        };
        let component = LocationInformationComponent {
            global_positioning_system_fix_data: CsmFlag::Yes,
            ..LocationInformationComponent::default()
        };
        assert_eq!(
            intersect(&request, Some(&component)),
            Some(StartLocationInformation {
                global_positioning_system_fix_data: CsmFlag::Yes,
                nmea_sentence_gpgga_interval_min: Some(1000),
                ..StartLocationInformation::default()
            })
        );
        assert_eq!(intersect(&request, None), None);
    }
}
