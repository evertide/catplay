use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use catplay_carplay::{
    audio::{AudioPlayerBox, AudioRecorderBox, AudioStreamBasicDescription},
    carplay_rx::{AirPlayReceiverHandleRef, AirPlayReceiverSink},
    modes::AirPlayModeState,
    msg::{AudioFormat, AudioType, Command, ControllerFeature, InfoMessageResponse, StreamType},
    rtsp_frame::{HttpStatus, RtspError, RtspResponse, RtspResult},
    screen::rx::ScreenReceiverSinkBox,
};
use catplay_carplay_rx_gadget::CarPlaySessionIdentity;
use catplay_iap2_client::CsmSessionBox;
use catplay_mfi::MfiDeficeRef;
use catplay_util::{AsyncShutdown, EventReconciler, EventSleeper};

use crate::proxy_rpc::{CarLocationSource, GpsConfig, Iap2LocationBridge, tx_adapter::TxAdapter};

pub struct CarPlayRxSession {
    tx: Option<TxAdapter>,
    handle: Option<AirPlayReceiverHandleRef>,
    reject: bool,
    car_location: CarLocationSource,
    identity: Arc<Mutex<Option<CarPlaySessionIdentity>>>,
    gps: GpsConfig,
    mfi: Option<MfiDeficeRef>,
}

impl CarPlayRxSession {
    pub fn new(
        tx: TxAdapter,
        car_location: CarLocationSource,
        identity: Arc<Mutex<Option<CarPlaySessionIdentity>>>,
        gps: GpsConfig,
        mfi: Option<MfiDeficeRef>,
    ) -> Self {
        Self {
            tx: Some(tx),
            handle: None,
            reject: false,
            car_location,
            identity,
            gps,
            mfi,
        }
    }

    pub fn reject() -> Self {
        Self {
            tx: None,
            handle: None,
            reject: true,
            car_location: Default::default(),
            identity: Default::default(),
            gps: Default::default(),
            mfi: None,
        }
    }

    fn tx(&mut self) -> &mut TxAdapter {
        self.tx.as_mut().expect("missing TX adapter")
    }

    fn reject_if_needed(&mut self) -> RtspResult<()> {
        if self.reject {
            return Err(RtspError::UnexpectedState("no TX session while RX has connected".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl AirPlayReceiverSink for CarPlayRxSession {
    fn init(&mut self, session: AirPlayReceiverHandleRef) -> RtspResult<()> {
        self.reject_if_needed()?;
        self.handle.replace(session.clone());
        self.tx().on_init(session);
        Ok(())
    }

    async fn open_audio(
        &mut self,
        latency: Duration,
        stream_type: StreamType,
        audio_type: AudioType,
        audio_format: AudioFormat,
        pcm_format: AudioStreamBasicDescription,
        duplex: bool,
    ) -> RtspResult<AudioPlayerBox<i16>> {
        self.reject_if_needed()?;
        self.tx()
            .proxy_audio(latency, stream_type, audio_type, audio_format, pcm_format, duplex)
            .await
    }

    async fn open_microphone(
        &mut self,
        stream_type: StreamType,
        audio_type: AudioType,
        pcm_format: AudioStreamBasicDescription,
    ) -> RtspResult<AudioRecorderBox<i16>> {
        self.reject_if_needed()?;
        self.tx()
            .proxy_microphone(stream_type, audio_type, pcm_format)
            .await
    }

    async fn on_record(&mut self) -> RtspResult<()> {
        self.reject_if_needed()?;
        self.tx().proxy_record().await
    }

    async fn open_iap2(&mut self) -> Option<CsmSessionBox> {
        if self.reject || !self.gps.airplay_iap2 {
            return None;
        }
        let identity = self.identity.lock().unwrap().clone()?;
        Some(Iap2LocationBridge::new(self.mfi.clone(), identity, self.car_location.clone()))
    }

    fn iap2_is_optional(&self) -> bool {
        true
    }

    async fn on_info(&mut self, info: &mut InfoMessageResponse) {
        if self.reject {
            return;
        }

        self.tx().proxy_info(info).await
    }

    async fn on_initial_setup(&mut self) -> RtspResult<()> {
        self.reject_if_needed()?;
        // TODO check busy status
        Ok(())
    }

    async fn negotiate_features(&mut self, offered: &[ControllerFeature]) -> Vec<ControllerFeature> {
        if self.reject {
            return vec![];
        }

        self.tx().proxy_features(offered).await
    }

    async fn open_screen(&mut self, latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        self.reject_if_needed()?;
        self.tx().proxy_screen(latency).await
    }

    async fn open_alt_screen(&mut self, latency: Duration, uuid: Option<String>) -> RtspResult<ScreenReceiverSinkBox> {
        self.reject_if_needed()?;
        self.tx().proxy_alt_screen(latency, uuid).await
    }

    async fn on_command_raw(&mut self, command: &Command) -> RtspResult<RtspResponse> {
        self.reject_if_needed()?;
        if self.gps.airplay_iap2 && matches!(command, Command::IApSendMessage(_)) {
            // The opted-in local endpoint owns these frames, including after its
            // failure. Do not also tunnel its authentication/location traffic to USB.
            return Ok(RtspResponse::new(None, HttpStatus::Ok));
        }
        self.tx().proxy_cmd_and_spy(command).await
    }

    async fn on_modes(&mut self, modes: &AirPlayModeState) {
        if let Some(tx) = self.tx.as_mut() {
            tx.on_modes(*modes).await
        }
    }
}

impl EventReconciler for CarPlayRxSession {
    type Error = RtspError;
}

impl EventSleeper for CarPlayRxSession {}

impl AsyncShutdown for CarPlayRxSession {
    async fn shutdown(&mut self) {
        if self.reject {
            return;
        }

        self.tx().on_shutdown().await;
    }
}

impl Drop for CarPlayRxSession {
    fn drop(&mut self) {
        if self.reject {
            return;
        }

        self.tx().on_drop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_carplay_rx_gadget::CarPlayServerSession;
    use catplay_carplay_tx_gadget::carplay_client_session::CarPlayClientSession;
    use catplay_csm::{
        decoder::{CsmFlag, CsmPacketId},
        msg::{
            IdentificationInformation, LocationInformationComponent, StartIdentification, StartLocationInformation, StopLocationInformation,
        },
    };
    use catplay_iap2_client::{CsmSession, test_support::Capture};
    use catplay_util::mpsc;
    use futures::{FutureExt, StreamExt, executor::block_on};

    fn identity() -> CarPlaySessionIdentity {
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

    #[test]
    fn default_config_keeps_airplay_iap2_absent_even_with_an_available_car() {
        let (mut car, control) = CarPlayClientSession::new();
        block_on(car.respond(IdentificationInformation {
            location_information_component: Some(LocationInformationComponent {
                global_positioning_system_fix_data: CsmFlag::Yes,
                ..LocationInformationComponent::default()
            }),
            messages_received_from_device: IdentificationInformation::pack_ids(&[
                StartLocationInformation::PACKET_ID,
                StopLocationInformation::PACKET_ID,
            ]),
            ..IdentificationInformation::default()
        }.into(), Capture::new())).unwrap();
        assert!(control.is_available());
        let source = CarLocationSource::default();
        source.set(control);
        let (tx, _rx) = mpsc::unbounded();
        let mut sink = CarPlayRxSession::new(
            TxAdapter::new(tx),
            source,
            Arc::new(Mutex::new(Some(identity()))),
            GpsConfig::default(),
            None,
        );
        assert!(block_on(sink.open_iap2()).is_none());
        assert!(car.wait_event().now_or_never().is_none());
    }

    #[test]
    fn opted_in_endpoint_uses_the_real_bluetooth_identity() {
        let identity = identity();
        let expected = CarPlayServerSession::id(&identity);
        let (tx, _rx) = mpsc::unbounded();
        let mut sink = CarPlayRxSession::new(
            TxAdapter::new(tx),
            CarLocationSource::default(),
            Arc::new(Mutex::new(Some(identity))),
            GpsConfig { airplay_iap2: true },
            None,
        );
        assert!(sink.iap2_is_optional());
        let mut endpoint = block_on(sink.open_iap2()).unwrap();
        let handle = Capture::new();
        block_on(endpoint.respond(StartIdentification::default().into(), handle.clone())).unwrap();
        let packets = handle.take();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].cast::<IdentificationInformation>(), Some(&expected));
    }

    #[test]
    fn opted_in_endpoint_never_fabricates_a_missing_identity() {
        let (tx, _rx) = mpsc::unbounded();
        let mut sink = CarPlayRxSession::new(
            TxAdapter::new(tx),
            CarLocationSource::default(),
            Default::default(),
            GpsConfig { airplay_iap2: true },
            None,
        );
        assert!(block_on(sink.open_iap2()).is_none());
    }

    #[test]
    fn only_opted_in_iap2_commands_bypass_the_existing_proxy() {
        use crate::proxy_rpc::TxAdapterOp;
        use catplay_carplay::msg::CommandIApSendMessage;
        let cmd = Command::IApSendMessage(CommandIApSendMessage { data: vec![1, 2, 3].into() });
        for enabled in [false, true] {
            let (tx, mut rx) = mpsc::unbounded();
            let mut sink = CarPlayRxSession::new(
                TxAdapter::new(tx),
                CarLocationSource::default(),
                Arc::new(Mutex::new(Some(identity()))),
                GpsConfig { airplay_iap2: enabled },
                None,
            );
            if enabled {
                assert_eq!(block_on(sink.on_command_raw(&cmd)).unwrap().status, HttpStatus::Ok);
                assert!(rx.next().now_or_never().is_none());
            } else {
                // Upstream behavior: raw commands go to the car proxy and await its response.
                let mut future = Box::pin(sink.on_command_raw(&cmd));
                assert!(future.as_mut().now_or_never().is_none());
                let Some(Some(TxAdapterOp::Command { response, .. })) = rx.next().now_or_never() else {
                    panic!("default mode must still proxy raw commands");
                };
                response.send(RtspResponse::new(None, HttpStatus::Ok)).unwrap();
                assert_eq!(block_on(future).unwrap().status, HttpStatus::Ok);
            }
        }
    }
}
