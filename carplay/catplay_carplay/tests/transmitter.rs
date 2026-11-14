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
    carplay_tx::{AirPlayTransmitterBootstrap, AirPlayTransmitterBootstrapError, AirPlayTransmitterImpl},
    common::AirPlayEncryptionType,
    msg::{AudioFormat, AudioType, Display, InfoMessageResponse, InitialSetup, StreamType},
    rtsp_frame::{HttpStatus, RtspError, RtspMethod, RtspResult},
    rtsp_session::{RtspReceiver, RtspReceiverCallback, RtspReceiverEvent, RtspReceiverEventResult},
    screen::{
        rx::{ScreenReceiverSink, ScreenReceiverSinkBox},
        tx::ScreenTransmitSink,
    },
    video::{AvccConfig, AvccConfigExtended, EncodedVideoFrame},
};
use catplay_hap::{HomekitStorageFile, HomekitStorageRef};
use catplay_tokio::TcpHelper;
use catplay_tracing::logger::setup_test_logger;
use catplay_util::{AsyncShutdown, EventReconciler, EventSink, EventSleeper, EventToken};
use log::debug;
use macaddr::MacAddr6;
use tokio::{
    runtime::Handle,
    time::{sleep, timeout},
};
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

/// One request the receiver got.
#[derive(Debug)]
struct Seen {
    method: RtspMethod,
    url: String,
    /// The RTSP session was encrypted (paired) when the request arrived.
    encrypted: bool,
    /// The `et` key of an initial SETUP. Absent after pairing: the streams then use the pairing secret.
    et: Option<AirPlayEncryptionType>,
}

/// What the transmitter sent, in order.
type RequestLog = Arc<Mutex<Vec<Seen>>>;

/// Which requests the receiver answers with 403.
type Refuse = fn(RtspMethod, &str) -> bool;

fn refuse_nothing(_: RtspMethod, _: &str) -> bool {
    false
}

/// Wraps the real receiver to record the requests it gets.
struct RecordingReceiver {
    inner: AirPlayReceiver,
    log: RequestLog,
    encrypted: bool,
    refuse: Refuse,
}

#[async_trait]
impl EventSink<RtspReceiverEvent<'_>, RtspReceiverEventResult> for RecordingReceiver {
    async fn on_event<'a>(&mut self, event: RtspReceiverEvent<'a>) -> RtspReceiverEventResult {
        if let RtspReceiverEvent::Request { request, .. } = &event {
            let et = match request.method {
                RtspMethod::Setup => request
                    .get_plist::<InitialSetup>()
                    .ok()
                    .and_then(|setup| setup.et),
                _ => None,
            };
            let url = request.url.to_string();
            let refused = (self.refuse)(request.method, &url);
            self.log.lock().unwrap().push(Seen {
                method: request.method,
                url,
                encrypted: self.encrypted,
                et,
            });
            if refused {
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

/// HomeKit identities of both ends; reuse them to reconnect with a known pairing.
#[derive(Clone)]
struct Identities {
    rx: HomekitStorageRef,
    tx: HomekitStorageRef,
}

impl Identities {
    fn new() -> Self {
        Self {
            rx: HomekitStorageFile::memory(),
            tx: HomekitStorageFile::memory(),
        }
    }
}

/// Returns the receiver handle too: dropping it would stop the mock receiver.
async fn connect_to_mock_receiver(pair_first: bool) -> (impl Sized, AirPlayTransmitterImpl) {
    let (rx, transmitter, _) = connect_recording(pair_first, None, refuse_nothing, Identities::new()).await;
    (rx, transmitter.unwrap())
}

async fn connect_recording(
    pair_first: bool,
    remote_homekit_id: Option<Uuid>,
    refuse: Refuse,
    identities: Identities,
) -> (
    impl Sized,
    Result<AirPlayTransmitterImpl, AirPlayTransmitterBootstrapError>,
    RequestLog,
) {
    let receiver = AirPlayReceiver::new(
        "fake_iface",
        MacAddr6::default(),
        identities.rx.clone(),
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
        refuse,
    };
    let session = RtspReceiver::new(recording);
    let (rx, bind) = TcpHelper::accept_timeout(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0), Duration::ZERO, session).unwrap();

    let bootstrap = AirPlayTransmitterBootstrap {
        homekit: identities.tx.clone(),
        peer_ip: bind,
        controller_features: vec![],
        remote_homekit_id,
        pair_first,
    };
    // A bootstrap that never reports its result would hang the test instead of failing it.
    let result = timeout(Duration::from_secs(10), AirPlayTransmitterImpl::connect(bootstrap))
        .await
        .expect("connect() must return");
    (rx, result, log)
}

fn count(log: &RequestLog, method: RtspMethod, url: &str) -> usize {
    log.lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.method == method && seen.url == url)
        .count()
}

/// Encryption state of every SETUP the receiver got, in order.
fn setups(log: &RequestLog) -> Vec<bool> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.method == RtspMethod::Setup)
        .map(|seen| seen.encrypted)
        .collect()
}

/// The `et` key of every SETUP the receiver got, in order.
fn setup_ets(log: &RequestLog) -> Vec<Option<AirPlayEncryptionType>> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.method == RtspMethod::Setup)
        .map(|seen| seen.et)
        .collect()
}

fn is_setup(method: RtspMethod, _: &str) -> bool {
    method == RtspMethod::Setup
}

fn is_pair_setup_or_setup(method: RtspMethod, url: &str) -> bool {
    url == "/pair-setup" || method == RtspMethod::Setup
}

#[tokio::test]
async fn skips_pairing_unless_requested() {
    let (_rx, transmitter, log) = connect_recording(false, None, refuse_nothing, Identities::new()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 0, "no pairing unless asked: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 0, "no pairing unless asked: {log:?}");
    assert_eq!(setups(&log), [false], "{log:?}");
    assert_eq!(setup_ets(&log), [Some(AirPlayEncryptionType::NONE)], "{log:?}");
}

/// Recent HomeKit receivers (Volvo XC90, srcvers 365.x) are paired before the first SETUP.
#[tokio::test]
async fn pairs_before_setup_when_requested() {
    let (_rx, transmitter, log) = connect_recording(true, None, refuse_nothing, Identities::new()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "M1, M3, M5: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 2, "M1, M3: {log:?}");
    assert_eq!(setups(&log), [true], "one SETUP, on the encrypted session: {log:?}");
    assert_eq!(setup_ets(&log), [None], "no et: the streams use the pairing secret: {log:?}");
}

/// The next connection to a known receiver skips /pair-setup and goes straight to /pair-verify.
#[tokio::test]
async fn reconnects_with_pair_verify_only_when_already_paired() {
    let identities = Identities::new();
    let receiver_id = Some(identities.rx.device_id());

    let (_rx, transmitter, log) = connect_recording(true, receiver_id, refuse_nothing, identities.clone()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "first connection pairs: {log:?}");

    let (_rx, transmitter, log) = connect_recording(true, receiver_id, refuse_nothing, identities.clone()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 0, "known pairing: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 2, "M1, M3: {log:?}");
    assert_eq!(setups(&log), [true], "{log:?}");
    assert_eq!(setup_ets(&log), [None], "{log:?}");
}

/// If pairing first fails, SETUP is still tried once without pairing, as before pair-first existed.
#[tokio::test]
async fn falls_back_to_unpaired_setup_once_when_pairing_first_fails() {
    // A Bonjour id the receiver doesn't have makes /pair-setup end in a uuid mismatch.
    let (_rx, transmitter, log) = connect_recording(true, Some(Uuid::from_u128(1)), refuse_nothing, Identities::new()).await;
    transmitter.unwrap();
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
    let (_rx, transmitter, log) = connect_recording(true, None, |_, url| url == "/pair-setup", Identities::new()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 1, "refused at M1: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 0, "{log:?}");
    assert_eq!(setups(&log), [false], "one SETUP, unencrypted: {log:?}");
}

/// Same fallback when the receiver refuses /pair-verify after a successful /pair-setup.
#[tokio::test]
async fn falls_back_to_unpaired_setup_once_when_the_receiver_refuses_pair_verify() {
    let (_rx, transmitter, log) = connect_recording(true, None, |_, url| url == "/pair-verify", Identities::new()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "{log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 1, "refused at M1: {log:?}");
    assert_eq!(setups(&log), [false], "one SETUP, unencrypted: {log:?}");
}

/// A receiver that refuses both pairing and SETUP gets one try of each, then connect() fails.
#[tokio::test]
async fn gives_up_when_pairing_first_and_the_fallback_setup_both_fail() {
    let (_rx, transmitter, log) = connect_recording(true, None, is_pair_setup_or_setup, Identities::new()).await;
    assert!(transmitter.is_err(), "{log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 1, "no second pairing round: {log:?}");
    assert_eq!(setups(&log), [false], "one unpaired SETUP, no retry: {log:?}");
}

/// Without pair-first, a refused SETUP leads to one pairing attempt; when that fails too, connect() fails
/// without another unpaired SETUP.
#[tokio::test]
async fn gives_up_when_setup_and_the_pairing_upgrade_both_fail() {
    let (_rx, transmitter, log) = connect_recording(false, None, is_pair_setup_or_setup, Identities::new()).await;
    assert!(transmitter.is_err(), "{log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 1, "one pairing upgrade: {log:?}");
    assert_eq!(setups(&log), [false], "no second unpaired SETUP: {log:?}");
}

/// Without pair-first, a refused SETUP is retried once on a paired session.
#[tokio::test]
async fn upgrades_to_pairing_once_when_the_unpaired_setup_is_refused() {
    // The mock refuses the paired SETUP too, so this checks the order of attempts and that it stops there.
    let (_rx, transmitter, log) = connect_recording(false, None, is_setup, Identities::new()).await;
    assert!(transmitter.is_err(), "{log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "{log:?}");
    assert_eq!(setups(&log), [false, true], "unpaired first, then paired: {log:?}");
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
