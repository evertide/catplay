use std::{
    net::{Ipv4Addr, SocketAddrV4},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::BytesMut;
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
    cipher::AirPlayStreamEncryption,
    common::AirPlayEncryptionType,
    msg::{AudioFormat, AudioType, ControllerFeature, Display, InfoMessage, InfoMessageResponse, InitialSetup, StreamType},
    rtsp_frame::{HttpStatus, RtspError, RtspMethod, RtspResult},
    rtsp_session::{RtspReceiver, RtspReceiverCallback, RtspReceiverEvent, RtspReceiverEventResult},
    screen::{
        rx::{ScreenReceiverSink, ScreenReceiverSinkBox},
        tx::ScreenTransmitSink,
    },
    video::{AvccConfig, AvccConfigExtended, EncodedVideoFrame, Pts},
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

struct MockSink {
    log: RequestLog,
    /// Controller features the receiver enables, if offered.
    enables: Vec<ControllerFeature>,
}
struct MockScreenSink {
    log: RequestLog,
}
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
        self.log.lock().unwrap().frames.push(frame.data.to_vec());
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

    async fn negotiate_features(&mut self, offered: &[ControllerFeature]) -> Vec<ControllerFeature> {
        offered
            .iter()
            .filter(|f| self.enables.contains(f))
            .cloned()
            .collect()
    }

    async fn open_screen(&mut self, latency: Duration) -> RtspResult<ScreenReceiverSinkBox> {
        Ok(Box::new(MockScreenSink { log: self.log.clone() }))
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

#[derive(Debug, Default)]
struct Recorded {
    /// What the transmitter sent, in order.
    requests: Vec<Seen>,
    /// The session key the receiver switched to after /pair-verify.
    shared_secret: Option<[u8; 32]>,
    /// Payloads of the video frames the receiver decoded, in order.
    frames: Vec<Vec<u8>>,
    /// The `altScreenURLs` of every GET /info the receiver got, in order.
    info_alt_screen_urls: Vec<Option<Vec<String>>>,
    /// Whether each GET /info body literally contains the key `altScreenURLs` (independent of the Rust type's spelling).
    info_raw_key: Vec<bool>,
}

type RequestLog = Arc<Mutex<Recorded>>;

/// Which requests the receiver answers with 403.
type Refuse = fn(&Seen) -> bool;

fn refuse_nothing(_: &Seen) -> bool {
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
            if request.method == RtspMethod::Get && request.url == "/info" {
                let urls = request
                    .get_plist::<InfoMessage>()
                    .ok()
                    .and_then(|info| info.alt_screen_urls);
                let raw_key = request.payload.windows(13).any(|w| w == b"altScreenURLs");
                let mut log = self.log.lock().unwrap();
                log.info_alt_screen_urls.push(urls);
                log.info_raw_key.push(raw_key);
            }
            let et = match request.method {
                RtspMethod::Setup => request
                    .get_plist::<InitialSetup>()
                    .ok()
                    .and_then(|setup| setup.et),
                _ => None,
            };
            let seen = Seen {
                method: request.method,
                url: request.url.to_string(),
                encrypted: self.encrypted,
                et,
            };
            let refused = (self.refuse)(&seen);
            self.log.lock().unwrap().requests.push(seen);
            if refused {
                return RtspReceiverEventResult::new(Err(RtspError::Code(HttpStatus::Forbidden)));
            }
        }
        let result = self.inner.on_event(event).await;
        if let Some(shared_secret) = result.encrypt_after_response {
            self.encrypted = true;
            self.log.lock().unwrap().shared_secret = Some(shared_secret);
        }
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
    connect_with_features(vec![], vec![], pair_first, remote_homekit_id, refuse, identities).await
}

/// `offered` are the transmitter's controller features, `enabled` the ones the receiver accepts.
async fn connect_with_features(
    controller_features: Vec<ControllerFeature>,
    enabled: Vec<ControllerFeature>,
    pair_first: bool,
    remote_homekit_id: Option<Uuid>,
    refuse: Refuse,
    identities: Identities,
) -> (
    impl Sized,
    Result<AirPlayTransmitterImpl, AirPlayTransmitterBootstrapError>,
    RequestLog,
) {
    let log = RequestLog::default();
    let receiver = AirPlayReceiver::new(
        "fake_iface",
        MacAddr6::default(),
        identities.rx.clone(),
        None,
        AirPlayServerShared::new(),
        Box::new(MockSink {
            log: log.clone(),
            enables: enabled,
        }),
        AirPlayReceiverProfile::CarPlay,
    );
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
        controller_features,
        remote_homekit_id,
        pair_first,
    };
    // A bootstrap that never reports its result would hang the test instead of failing it.
    // Generous, because a full SRP pair-setup in a debug build is slow on loaded CI runners.
    let result = timeout(Duration::from_secs(30), AirPlayTransmitterImpl::connect(bootstrap))
        .await
        .expect("connect() must return");
    (rx, result, log)
}

fn count(log: &RequestLog, method: RtspMethod, url: &str) -> usize {
    log.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|seen| seen.method == method && seen.url == url)
        .count()
}

/// Encryption state of every SETUP the receiver got, in order.
fn setups(log: &RequestLog) -> Vec<bool> {
    log.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|seen| seen.method == RtspMethod::Setup)
        .map(|seen| seen.encrypted)
        .collect()
}

/// The `et` key of every SETUP the receiver got, in order.
fn setup_ets(log: &RequestLog) -> Vec<Option<AirPlayEncryptionType>> {
    log.lock()
        .unwrap()
        .requests
        .iter()
        .filter(|seen| seen.method == RtspMethod::Setup)
        .map(|seen| seen.et)
        .collect()
}

/// The transmitter's media streams use the session key the receiver got from pairing.
fn assert_streams_use_pairing_secret(transmitter: &mut AirPlayTransmitterImpl, log: &RequestLog) {
    let receiver_secret = log
        .lock()
        .unwrap()
        .shared_secret
        .expect("receiver was paired");
    let cipher = transmitter.raw_streams().cipher;
    assert!(
        matches!(cipher, AirPlayStreamEncryption::ChaCha { shared_secret } if shared_secret == receiver_secret),
        "streams must use the pairing secret, got {cipher:?}"
    );
}

fn is_setup(seen: &Seen) -> bool {
    seen.method == RtspMethod::Setup
}

fn is_unpaired_setup(seen: &Seen) -> bool {
    is_setup(seen) && !seen.encrypted
}

fn is_paired_setup(seen: &Seen) -> bool {
    is_setup(seen) && seen.encrypted
}

fn is_pair_setup_or_setup(seen: &Seen) -> bool {
    seen.url == "/pair-setup" || is_setup(seen)
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
    let mut transmitter = transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "M1, M3, M5: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 2, "M1, M3: {log:?}");
    assert_eq!(setups(&log), [true], "one SETUP, on the encrypted session: {log:?}");
    assert_eq!(setup_ets(&log), [None], "no et after pairing: {log:?}");
    assert_streams_use_pairing_secret(&mut transmitter, &log);
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
    let mut transmitter = transmitter.unwrap();
    assert_streams_use_pairing_secret(&mut transmitter, &log);
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
    let (_rx, transmitter, log) = connect_recording(true, None, |seen| seen.url == "/pair-setup", Identities::new()).await;
    transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 1, "refused at M1: {log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-verify"), 0, "{log:?}");
    assert_eq!(setups(&log), [false], "one SETUP, unencrypted: {log:?}");
}

/// Same fallback when the receiver refuses /pair-verify after a successful /pair-setup.
#[tokio::test]
async fn falls_back_to_unpaired_setup_once_when_the_receiver_refuses_pair_verify() {
    let (_rx, transmitter, log) = connect_recording(true, None, |seen| seen.url == "/pair-verify", Identities::new()).await;
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

/// Without pair-first, a refused unpaired SETUP is retried on a paired session.
#[tokio::test]
async fn upgrades_to_pairing_when_the_unpaired_setup_is_refused() {
    let (_rx, transmitter, log) = connect_recording(false, None, is_unpaired_setup, Identities::new()).await;
    let mut transmitter = transmitter.unwrap();
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "{log:?}");
    assert_eq!(setups(&log), [false, true], "unpaired first, then paired: {log:?}");
    assert_streams_use_pairing_secret(&mut transmitter, &log);
}

/// The pairing upgrade happens once: if the paired SETUP is refused too, connect() fails.
#[tokio::test]
async fn gives_up_when_the_paired_setup_after_the_upgrade_is_refused() {
    let (_rx, transmitter, log) = connect_recording(false, None, is_setup, Identities::new()).await;
    assert!(transmitter.is_err(), "{log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "{log:?}");
    assert_eq!(setups(&log), [false, true], "{log:?}");
}

/// With pair-first, a refused SETUP after successful pairing fails connect(): there is no unpaired fallback then.
#[tokio::test]
async fn gives_up_when_the_setup_after_pairing_first_is_refused() {
    let (_rx, transmitter, log) = connect_recording(true, None, is_paired_setup, Identities::new()).await;
    assert!(transmitter.is_err(), "{log:?}");
    assert_eq!(count(&log, RtspMethod::Post, "/pair-setup"), 3, "{log:?}");
    assert_eq!(setups(&log), [true], "no unpaired retry after pairing worked: {log:?}");
}

/// Sets up screen and audio streams and checks that a video frame reaches the receiver intact,
/// which proves both ends use the same stream key (the pairing secret when paired).
async fn streams_video_and_audio(pair_first: bool) {
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
        view: Default::default(),
    };

    let (_rx, transmitter, log) = connect_recording(pair_first, None, refuse_nothing, Identities::new()).await;
    let mut transmitter = transmitter.unwrap();
    let mut sink = transmitter
        .do_setup_video(StreamType::Screen, Duration::from_millis(75), None)
        .await
        .unwrap();
    sink.push_avcc_config(config).unwrap();

    // An IDR slice in Annex-B; the receiver hands it back in Annex-B after decrypting.
    let keyframe: Vec<u8> = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x33, 0xFF, 0x12, 0x34, 0x56, 0x78];
    let frame = EncodedVideoFrame {
        pts: Pts(Instant::now()),
        width: 1920,
        height: 1080,
        data: BytesMut::from(keyframe.as_slice()),
        config: None,
        nal_offsets: None,
        is_keyframe: Some(true),
        header_buf: BytesMut::new(),
        chacha_tag_buf: BytesMut::new(),
    };
    sink.push_frame(frame)
        .await
        .map_err(|(err, _)| err)
        .unwrap();

    transmitter
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

    let received = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(frame) = log.lock().unwrap().frames.first().cloned() {
                return frame;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the receiver never decoded the video frame: {log:?}"));
    assert_eq!(received, keyframe, "the frame must arrive intact");
    if pair_first {
        assert_streams_use_pairing_secret(&mut transmitter, &log);
    }
}

#[tokio::test]
async fn setups_video_audio_streams() {
    streams_video_and_audio(false).await;
}

/// The Volvo black screen: after pairing, the screen stream must be encrypted with the pairing secret.
#[tokio::test]
async fn setups_video_audio_streams_when_paired() {
    streams_video_and_audio(true).await;
}

const CLUSTER_URLS: [&str; 3] = [
    "maps:/car/instrumentcluster/map",
    "maps:/car/instrumentcluster/instructioncard",
    "maps:/car/instrumentcluster",
];

#[tokio::test]
async fn announces_cluster_urls_only_with_alt_screen() {
    use ControllerFeature::{AltScreen, ViewAreas};
    let urls = CLUSTER_URLS.map(String::from).to_vec();
    // (offered by the transmitter, enabled by the receiver, expected altScreenURLs)
    let cases = [
        (vec![AltScreen], vec![AltScreen], Some(urls)),
        (vec![AltScreen], vec![], None),
        (vec![ViewAreas], vec![AltScreen, ViewAreas], None),
    ];
    for (offered, enabled, expected) in cases {
        let (_rx, tx, log) = connect_with_features(offered.clone(), enabled.clone(), false, None, refuse_nothing, Identities::new()).await;
        tx.unwrap();
        let log = log.lock().unwrap();
        assert_eq!(log.info_raw_key, [expected.is_some()], "offered {offered:?}, enabled {enabled:?}");
        assert_eq!(log.info_alt_screen_urls, [expected], "offered {offered:?}, enabled {enabled:?}");
    }
}
