use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use catplay_csm::{
    decoder::{CsmFlag, CsmPacketId},
    msg::{
        IdentificationInformation, LocationInformation, LocationInformationComponent, PowerSourceUpdate, StartLocationInformation,
        StartPowerUpdates, StopLocationInformation,
    },
};
use catplay_iap2_client::{CsmClientHandleRef, CsmSessionResult};
use catplay_util::mpsc;
use futures::{FutureExt, StreamExt};
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

/// One iPhone's location subscription. Closing the request channel stops updates.
/// A replacement owns new channels, so an old iPhone cannot stop its successor.
pub struct CarPlayLocationSession {
    requests: mpsc::UnboundedReceiver<LocationRequest>,
    updates: mpsc::UnboundedSender<LocationInformation>,
}

impl CarPlayLocationSession {
    pub fn new() -> (
        Self,
        mpsc::UnboundedSender<LocationRequest>,
        mpsc::UnboundedReceiver<LocationInformation>,
    ) {
        let (requests_tx, requests) = mpsc::unbounded();
        let (updates, updates_rx) = mpsc::unbounded();
        (Self { requests, updates }, requests_tx, updates_rx)
    }
}

#[derive(Default)]
pub(crate) struct CarLocation {
    sessions: Option<mpsc::UnboundedReceiver<CarPlayLocationSession>>,
    pending_session: Option<CarPlayLocationSession>,
    peer: Option<CarPlayLocationSession>,
    // The inner None is EOF, distinct from having no pending request.
    pending_request: Option<Option<LocationRequest>>,
    requested: Option<StartLocationInformation>,
    active: Option<StartLocationInformation>,
    component: Option<LocationInformationComponent>,
    supported: Arc<AtomicBool>,
    pub authenticated: bool,
    needs_power_source: bool,
    needs_power_updates: bool,
    pub power_source_replied: bool,
    pub power_updates_replied: bool,
    handle: Option<CsmClientHandleRef>,
}

impl CarLocation {
    pub fn new() -> (Self, CarPlayLocationControl) {
        let (tx, rx) = mpsc::unbounded();
        let mut location = Self::default();
        location.sessions = Some(rx);
        let control = CarPlayLocationControl {
            sessions: tx,
            supported: location.supported.clone(),
        };
        (location, control)
    }

    pub fn start(&mut self, handle: CsmClientHandleRef) {
        if !handle.is_downgrade() {
            self.handle = Some(handle);
        }
    }

    pub fn identify(&mut self, id: &IdentificationInformation) {
        self.component = id.location_information_component.clone();
        self.supported
            .store(self.component.is_some(), Ordering::Relaxed);
        self.needs_power_source = id.wants_tx(PowerSourceUpdate::PACKET_ID);
        self.needs_power_updates = id.wants_tx(StartPowerUpdates::PACKET_ID);
        self.authenticated = false;
        self.power_source_replied = false;
        self.power_updates_replied = false;
    }

    pub async fn wait_event(&mut self) {
        // Buffer input only: the iAP2 client's reconcile loop serializes state changes
        // with authentication, power replies and received location packets.
        loop {
            tokio::select! {
                session = async {
                    match self.sessions.as_mut() {
                        Some(sessions) => sessions.next().await,
                        None => std::future::pending().await,
                    }
                }, if self.pending_session.is_none() => {
                    if let Some(session) = session {
                        self.pending_session = Some(session);
                        return;
                    }
                    self.sessions = None;
                }
                request = async {
                    match self.peer.as_mut() {
                        Some(peer) => peer.requests.next().await,
                        None => std::future::pending().await,
                    }
                }, if self.pending_request.is_none() => {
                    self.pending_request = Some(request);
                    return;
                }
                else => std::future::pending::<()>().await,
            }
        }
    }

    pub fn process_event(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        if let Some(peer) = self.pending_session.take() {
            self.stop(handle)?;
            self.requested = None;
            self.pending_request = None;
            self.peer = Some(peer);
            consumed = true;
        }
        Ok(self.reconcile(handle)? || consumed)
    }

    pub fn reconcile(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut consumed = false;
        // Coalesce queued requests before opening the feed, including a Stop/EOF
        // that arrived while identification or power negotiation was in progress.
        while let Some(request) = self.pending_request.take().or_else(|| {
            self.peer
                .as_mut()
                .and_then(|peer| peer.requests.next().now_or_never())
        }) {
            match request {
                Some(LocationRequest::Start(request)) => self.requested = Some(request),
                Some(LocationRequest::Stop) => self.requested = None,
                None => {
                    self.requested = None;
                    self.peer = None;
                }
            }
            consumed = true;
        }
        if self
            .peer
            .as_ref()
            .is_some_and(|peer| peer.updates.is_closed())
        {
            self.requested = None;
            self.peer = None;
        }

        // Wait for the existing power replies, including the entire StartPowerUpdates
        // burst. If both messages were advertised, both exchanges must have occurred.
        // Do not infer readiness from authentication or the AirPlay connection alone.
        let power_ready = (self.power_source_replied || self.power_updates_replied)
            && (!self.needs_power_source || self.power_source_replied)
            && (!self.needs_power_updates || self.power_updates_replied);
        let wanted = if self.handle.is_some() && self.authenticated && power_ready {
            self.requested
                .as_ref()
                .and_then(|request| intersect(request, self.component.as_ref()))
        } else {
            None
        };

        if wanted != self.active {
            self.stop(handle)?;
            if let Some(request) = wanted {
                handle.send(&request)?;
                info!("Requested car location updates: {request:?}");
                self.active = Some(request);
            }
        }
        Ok(consumed)
    }

    pub fn forward(&mut self, packet: &LocationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        if self.active.is_some()
            && let Some(peer) = self.peer.as_mut()
            && peer.updates.unbounded_send(packet.clone()).is_err()
        {
            self.requested = None;
            self.peer = None;
            self.stop(handle)?;
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

impl Drop for CarLocation {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.clone() {
            // Best effort: a physically disconnected USB link cannot deliver this.
            let _ = self.stop(&handle);
        }
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
