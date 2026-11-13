use std::sync::{Arc, Mutex};

use catplay_csm::{
    decoder::{CsmFlag, CsmPacketId},
    msg::{
        IdentificationInformation, LocationInformation, LocationInformationComponent, PowerSourceUpdate, StartLocationInformation,
        StartPowerUpdates, StopLocationInformation,
    },
};
use catplay_iap2_client::{CsmClientHandleRef, CsmSessionResult};
use catplay_util::{EventSleeper, mpsc, notify::Notify};
use log::info;

/// The current Bluetooth request, shared with each successive USB iAP2 session.
/// Negotiation and the active car request remain local to the USB session.
#[derive(Clone, Default)]
pub struct LocationBridge {
    current: Arc<Mutex<Option<(StartLocationInformation, mpsc::UnboundedSender<LocationInformation>)>>>,
    changed: Notify,
}

impl LocationBridge {
    pub fn subscribe(&self, request: StartLocationInformation) -> LocationSubscription {
        let (tx, updates) = mpsc::unbounded();
        *self.current.lock().unwrap() = Some((request, tx));
        self.changed.notify();
        LocationSubscription {
            updates,
            changed: self.changed.clone(),
        }
    }
}

/// Closing this receiver cancels its request and wakes the car to send Stop.
#[derive(EventSleeper)]
pub struct LocationSubscription {
    #[sleep]
    updates: mpsc::UnboundedReceiver<LocationInformation>,
    changed: Notify,
}

impl LocationSubscription {
    pub fn take(&mut self) -> Option<LocationInformation> {
        mpsc::PeekReceiver::take(&mut self.updates)
    }
}

impl Drop for LocationSubscription {
    fn drop(&mut self) {
        self.updates.close();
        self.changed.notify();
    }
}

#[derive(Default)]
pub(crate) struct CarLocation {
    pub bridge: LocationBridge,
    pub started: bool,
    pub authenticated: bool,
    pub power_source_replied: bool,
    pub power_updates_replied: bool,
    active: Option<StartLocationInformation>,
    power_wait_logged: bool,
}

impl CarLocation {
    pub async fn wait_event(&mut self) {
        let _ = self.bridge.changed.sleep().await;
    }

    pub fn reset(&mut self, handle: &CsmClientHandleRef) -> CsmSessionResult<()> {
        self.stop(handle)?;
        self.authenticated = false;
        self.power_source_replied = false;
        self.power_updates_replied = false;
        self.power_wait_logged = false;
        Ok(())
    }

    pub fn reconcile(&mut self, id: &IdentificationInformation, handle: &CsmClientHandleRef) -> CsmSessionResult<bool> {
        let mut current = self.bridge.current.lock().unwrap();
        if current.as_ref().is_some_and(|(_, updates)| updates.is_closed()) {
            *current = None;
        }
        let request = current
            .as_ref()
            .and_then(|(request, _)| intersect(request, id.location_information_component.as_ref()));
        drop(current);

        // At least one real power exchange is required. If both were advertised,
        // both replies (including the complete StartPowerUpdates burst) must be sent.
        let power_ready = (self.power_source_replied || self.power_updates_replied)
            && (!id.wants_tx(PowerSourceUpdate::PACKET_ID) || self.power_source_replied)
            && (!id.wants_tx(StartPowerUpdates::PACKET_ID) || self.power_updates_replied);
        let allowed = self.started
            && self.authenticated
            && id.wants_rx(StartLocationInformation::PACKET_ID)
            && id.wants_rx(StopLocationInformation::PACKET_ID);
        if request.is_some() && allowed && !power_ready {
            if !self.power_wait_logged {
                info!("Car GPS deferred: waiting for a power exchange and all advertised power replies");
                self.power_wait_logged = true;
            }
        } else {
            self.power_wait_logged = false;
        }
        let wanted = request.filter(|_| allowed && power_ready);
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
            && let Some((_, updates)) = self.bridge.current.lock().unwrap().as_ref()
        {
            // A closed receiver is handled by the subscription's wakeup above.
            let _ = updates.unbounded_send(packet.clone());
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
