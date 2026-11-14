use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
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

type LocationRequest = (StartLocationInformation, watch::Sender<Option<LocationInformation>>);

/// The current iPhone request, shared with each successive USB iAP2 session.
/// Negotiation and the active car request remain local to the USB session.
#[derive(Clone)]
pub struct LocationBridge {
    request: watch::Sender<Option<LocationRequest>>,
    car: Arc<Mutex<Weak<AtomicBool>>>,
}

impl Default for LocationBridge {
    fn default() -> Self {
        Self {
            request: watch::channel(None).0,
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

    pub fn subscribe(&self, request: StartLocationInformation) -> LocationSubscription {
        let (tx, updates) = watch::channel(None);
        self.request.send_replace(Some((request, tx)));
        LocationSubscription {
            updates: Some(updates),
            pending: false,
            request: self.request.clone(),
        }
    }
}

/// Keeps only the latest fix. Closing the receiver cancels its request and wakes
/// every USB session, including briefly overlapping sessions during reconnects.
pub struct LocationSubscription {
    updates: Option<watch::Receiver<Option<LocationInformation>>>,
    pending: bool,
    request: watch::Sender<Option<LocationRequest>>,
}

impl EventSleeper for LocationSubscription {
    async fn sleep(&mut self) -> Option<EventToken> {
        let updates = self.updates.as_mut()?;
        if !self.pending {
            updates.changed().await.ok()?;
            self.pending = true;
        }
        Some(EventToken(1))
    }
}

impl LocationSubscription {
    pub fn take(&mut self) -> Option<LocationInformation> {
        let updates = self.updates.as_mut()?;
        if !self.pending && !updates.has_changed().unwrap_or(false) {
            return None;
        }
        self.pending = false;
        updates.borrow_and_update().clone()
    }
}

impl Drop for LocationSubscription {
    fn drop(&mut self) {
        // Close before publishing cancellation. Dropping an old subscription must
        // not clear a replacement request that already has its own live receiver.
        self.updates.take();
        self.request.send_if_modified(|request| {
            if request
                .as_ref()
                .is_some_and(|(_, updates)| updates.is_closed())
            {
                *request = None;
                true
            } else {
                false
            }
        });
    }
}

pub(crate) struct CarLocation {
    bridge: LocationBridge,
    requests: watch::Receiver<Option<LocationRequest>>,
    supported: Arc<AtomicBool>,
    started: bool,
    authenticated: bool,
    active: Option<StartLocationInformation>,
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
        }
    }

    pub fn start(&mut self, handle: &CsmClientHandleRef) {
        self.started = !handle.is_downgrade();
    }

    pub fn authenticate(&mut self) {
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
        Ok(())
    }

    pub fn reconcile(&mut self, id: &IdentificationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        let request = self.requests.borrow_and_update().clone();
        // Authentication and explicit location capabilities are sufficient. Power
        // messages are independent and some cars advertise them without sending any.
        let wanted = request
            .filter(|(_, updates)| !updates.is_closed() && self.authenticated && self.supported.load(Ordering::Relaxed))
            .and_then(|(request, _)| intersect(&request, id.location_information_component.as_ref()));
        if wanted == self.active {
            return Ok(false);
        }
        self.stop(handle)?;
        if let Some(request) = wanted {
            handle.send(&request)?;
            info!("Requested car location updates: {request:?}");
            self.active = Some(request);
        }
        Ok(true)
    }

    pub fn forward(&self, packet: &LocationInformation) {
        if self.active.is_some()
            && let Some((_, updates)) = self.bridge.request.borrow().as_ref()
        {
            updates.send_replace(Some(packet.clone()));
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
