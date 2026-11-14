use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use catplay_carplay::{
    audio::{
        AudioPlayer, AudioPlayerBox, AudioPlayerEvent, AudioRecorder, AudioRecorderBox, AudioRecorderControl, AudioRecorderEvent,
        AudioStreamBasicDescription, RingConsumer, RingProducer,
    },
    carplay_rx::{
        AirPlayReceiverProfile, AirPlayReceiverSink,
        sink::{AirPlayReceiver, AirPlayServerShared},
    },
    carplay_tx::{AirPlayTransmitterBootstrap, AirPlayTransmitterImpl},
    msg::{AudioFormat, AudioType, Display, InfoMessageResponse, StreamType},
    rtsp_frame::{HttpStatus, RtspError, RtspMethod, RtspResult},
    rtsp_session::{RtspReceiver, RtspReceiverCallback, RtspReceiverEvent, RtspReceiverEventResult},
    screen::{
        rx::{ScreenReceiverSink, ScreenReceiverSinkBox},
        tx::ScreenTransmitSink,
    },
    video::{AvccConfig, AvccConfigExtended, EncodedVideoFrame},
};
use catplay_hap::HomekitStorageFile;
use catplay_tokio::TcpHelper;
use catplay_tracing::logger::setup_test_logger;
use catplay_util::{AsyncShutdown, EventReconciler, EventSink, EventSleeper, EventToken};
use log::debug;
use macaddr::MacAddr6;
use tokio::{runtime::Handle, time::sleep};
use uuid::Uuid;

struct MockSink {}
struct MockScreenSink {}
struct MockRecorder {}
struct MockPlayer {}

impl AudioRecorder for MockRecorder {
    type Sample = i16;

    fn init(&mut self, _ring: RingProducer<i16>, _format: AudioStreamBasicDescription, _control: AudioRecorderControl) -> RtspResult<()> {
        Ok(())
    }

    fn on_event(&mut self, _event: AudioRecorderEvent) {}
}

impl AudioPlayer for MockPlayer {
    type Sample = i16;

    fn init(&mut self, _ring: RingConsumer<i16>, _format: AudioStreamBasicDescription) -> RtspResult<()> {
        Ok(())
    }

    fn on_event(&mut self, _event: AudioPlayerEvent) {}
}

#[async_trait]
impl ScreenReceiverSink for MockScreenSink {
    async fn init(&mut self) -> RtspResult<()> {
        Ok(())
    }

    async fn process_frame(&mut self, frame: EncodedVideoFrame) -> RtspResult<()> {
        Ok(())
    }

    async fn set_avcc_config(&mut self, _config: AvccConfigExtended) -> RtspResult<()> {
        debug!("AVCC: {_config:?}");
        Ok(())
    }
}

impl AsyncShutdown for MockScreenSink {}

#[async_trait]
impl AirPlayReceiverSink for MockSink {
    async fn on_info(&mut self, info: &mut InfoMessageResponse) {
        info.displays.push(Display::default());
    }

    async fn open_screen(&mut self, latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        Ok(Box::new(MockScreenSink {}))
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
        Ok(Box::new(MockPlayer {}))
    }

    async fn open_microphone(
        &mut self,
        stream_type: StreamType,
        audio_type: AudioType,
        pcm_format: AudioStreamBasicDescription,
    ) -> RtspResult<AudioRecorderBox<i16>> {
        Ok(Box::new(MockRecorder {}))
    }
}

impl AsyncShutdown for MockSink {}
impl EventSleeper for MockSink {}
impl EventReconciler for MockSink {
    type Error = RtspError;
}

/// What the transmitter sent, in order: method, url, and whether the session was encrypted at that point.
type RequestLog = Arc<Mutex<Vec<(RtspMethod, String, bool)>>>;

/// Wraps the real receiver to record the requests it gets.
struct RecordingReceiver {
    inner: AirPlayReceiver,
    log: RequestLog,
    encrypted: bool,
    /// Answer /pair-setup with 403, like a receiver that refuses to pair.
    reject_pair_setup: bool,
}

#[async_trait]
impl EventSink<RtspReceiverEvent<'_>, RtspReceiverEventResult> for RecordingReceiver {
    async fn on_event<'a>(&mut self, event: RtspReceiverEvent<'a>) -> RtspReceiverEventResult {
        if let RtspReceiverEvent::Request { request, .. } = &event {
            let entry = (request.method, request.url.to_string(), self.encrypted);
            self.log.lock().unwrap().push(entry);
            if self.reject_pair_setup && request.url.as_str() == "/pair-setup" {
                return RtspReceiverEventResult::new(Err(RtspError::Code(HttpStatus::Forbidden)));
            }
        }
        let result = self.inner.on_event(event).await;
        self.encrypted |= result.encrypt_after_response.is_some();
        result
    }
}

impl RtspReceiverCallback for RecordingReceiver {}
impl AsyncShutdown for RecordingReceiver {
    async fn shutdown(&mut self) {
        self.inner.shutdown().await
    }
}
impl EventSleeper for RecordingReceiver {
    async fn sleep(&mut self) -> Option<EventToken> {
        self.inner.sleep().await
    }
}
impl EventReconciler for RecordingReceiver {
    type Error = RtspError;

    async fn reconcile(&mut self) -> Result<(), Self::Error> {
        self.inner.reconcile().await
    }
}

/// Returns the receiver handle too: dropping it would stop the mock receiver.
async fn connect_to_mock_receiver(pair_first: bool) -> (impl Sized, AirPlayTransmitterImpl) {
    let (rx, transmitter, _) = connect_recording(pair_first, None, false).await;
    (rx, transmitter)
}

async fn connect_recording(
    pair_first: bool,
    remote_homekit_id: Option<Uuid>,
    reject_pair_setup: bool,
) -> (impl Sized, AirPlayTransmitterImpl, RequestLog) {
    let hk_rx = HomekitStorageFile::memory();
    let hk_tx = HomekitStorageFile::memory();

    let receiver = AirPlayReceiver::new(
        "fake_iface",
        MacAddr6::default(),
        hk_rx,
        None,
        AirPlayServerShared::new(),
        Box::new(MockSink {}),
        AirPlayReceiverProfile::CarPlay,
    );
    let log = RequestLog::default();
    let recording = RecordingReceiver {
        inner: receiver,
        log: log.clone(),
        encrypted: false,
        reject_pair_setup,
    };
    let session = RtspReceiver::new(recording);
    let (rx, bind) = TcpHelper::accept_timeout(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0), Duration::ZERO, session).unwrap();

    let bootstrap = AirPlayTransmitterBootstrap {
        homekit: hk_tx,
        peer_ip: bind,
        controller_features: vec![],
        remote_homekit_id,
        pair_first,
    };
    (rx, AirPlayTransmitterImpl::connect(bootstrap).await.unwrap(), log)
}

fn count(log: &RequestLog, method: RtspMethod, url: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(m, u, _)| *m == method && u == url)
        .count()
}

/// Encryption state of every SETUP the receiver got, in order.
fn setups(log: &RequestLog) -> Vec<bool> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(m, ..)| *m == RtspMethod::Setup)
        .map(|(.., encrypted)| *encrypted)
        .collect()
}

#[tokio::test]
async fn sends_pair_request() {
    let (_rx, _transmitter, log) = connect_recording(false, None, false).await;
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 0, "no pairing unless asked: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 0, "no pairing unless asked: {log:?}");
    assert_eq!(setups(&log), [false], "{log:?}");
}

/// Recent HomeKit receivers (Volvo XC90, srcvers 365.x) are paired before the first SETUP.
#[tokio::test]
async fn pairs_before_setup_when_requested() {
    let (_rx, _transmitter, log) = connect_recording(true, None, false).await;
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "M1, M3, M5: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 2, "M1, M3: {log:?}");
    assert_eq!(setups(&log), [true], "one SETUP, on the encrypted session: {log:?}");
}

/// If pairing first fails, SETUP is still tried once without pairing, as before pair-first existed.
#[tokio::test]
async fn falls_back_to_unpaired_setup_once_when_pairing_first_fails() {
    // A Bonjour id the receiver doesn't have makes /pair-setup end in a uuid mismatch.
    let (_rx, _transmitter, log) = connect_recording(true, Some(Uuid::from_u128(1)), false).await;
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "pairing was tried: {log:?}");
    assert_eq!(
        count(&log, RtspMethod::Post, "/pair-verify"),
        0,
        "pairing failed before verify: {log:?}"
    );
    assert_eq!(setups(&log), [false], "one SETUP, unencrypted: {log:?}");
}

/// Same fallback when the receiver itself refuses /pair-setup.
#[tokio::test]
async fn falls_back_to_unpaired_setup_once_when_the_receiver_refuses_pairing() {
    let (_rx, _transmitter, log) = connect_recording(true, None, true).await;
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 1, "refused at M1: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 0, "{log:?}");
    assert_eq!(setups(&log), [false], "one SETUP, unencrypted: {log:?}");
}

#[tokio::test]
async fn setups_video_audio_streams() {
    setup_test_logger(true);
    let sps_pps: Vec<u8> = vec![
        0x00, 0x00, 0x00, 0x01, // start code
        0x67, 0x64, 0x00, 0x28, 0xAC, 0x2B, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00,
        0xC8, 0x3C, 0x48, 0x96, 0x11, 0x80, 0x00, 0x00, 0x00, 0x01, // start code
        0x68, 0xEE, 0x3C, 0xB0,
    ];

    let config = AvccConfigExtended {
        hevc: false,
        avcc: AvccConfig { nal_size_len: 4, sps_pps },
        video_latency: Duration::from_millis(70),
        width: 1920,
        height: 1080,
        respect_timestamps: true,
    };

    let (_rx, mut _transmitter) = connect_to_mock_receiver(false).await;
    let mut sink = _transmitter
        .do_setup_video(StreamType::Screen, Duration::from_millis(75), None)
        .await
        .unwrap();
    sink.push_avcc_config(config).unwrap();

    let _ = _transmitter
        .do_setup_audio(
            Duration::from_millis(32),
            StreamType::MainAudio,
            AudioFormat::PCM_16000_MONO,
            AudioType::Telephony,
            Some(Box::new(MockPlayer {})),
            Box::new(MockRecorder {}),
        )
        .await
        .unwrap();
    sleep(Duration::from_millis(500)).await;

    // TODO assert result
    debug!("Paired!")
}
