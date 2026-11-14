use super::{proxy_screen::ScreenProxyOp, xc90_diag::Xc90Diag};
use catplay_carplay::{
    carplay_tx::TeardownGuard,
    screen::tx::{ScreenTransmitError, ScreenTransmitProxy, ScreenTransmitSink},
    video::{AvccConfigExtended, EncodedVideoFrame},
};
use catplay_util::{EventSleeper, EventToken, event_select, mpsc, oneshot};
use log::{debug, error, info, warn};
use std::time::{Duration, Instant};

/// Proxies the alt screen stream (for example, an instrument cluster) from the iPhone to the car.
///
/// Unlike the main screen, the alt screen is not arbitrated using modes and never shows an overlay. The stream towards the car is setup
/// when the iPhone sets up its alt screen stream, and torn down when the iPhone tears it down (or disconnects).
#[derive(Default)]
pub struct AltScreenManager {
    iphone_screen_rx: Option<mpsc::UnboundedReceiver<ScreenProxyOp>>,
    /// UUID of the display the alt screen stream is meant for, used to request keyframes for this display only.
    uuid: Option<String>,
    car_screen: Option<TeardownGuard<ScreenTransmitProxy>>,
    pending_setup: Option<oneshot::Receiver<TeardownGuard<ScreenTransmitProxy>>>,
    /// Last iPhone keyframe received while the car SETUP was pending, so the car stream does not start with delta frames only.
    pending_keyframe: Option<EncodedVideoFrame>,
    /// Diagnostics: frame capture after the last showUI (see [Capture]).
    capture: Option<Capture>,
    /// XC90 diagnostics: last config sent to the car stream, and whether a re-SETUP is due (see [Xc90Diag::alt_resetup]).
    sent_config: Option<AvccConfigExtended>,
    resetup: bool,
    /// XC90 diagnostics ([Xc90Diag::alt_lazy_setup]): the car SETUP waits for the iPhone's first keyframe, like the stock firmware.
    lazy_setup: bool,
    /// Whether the keyframe held during a lazy SETUP was followed by frames the car never got, so a fresh one is needed.
    lazy_keyframe: Option<catplay_carplay::carplay_rx::AirPlayReceiverHandleRef>,
    /// XC90 diagnostics ([Xc90Diag::alt_showui_resetup]): ask the iPhone for a keyframe once the new car stream is ready.
    keyframe_after_setup: bool,
}

/// Diagnostics: captures started by this process (a new [AltScreenManager] is created per car session).
static CAPTURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Diagnostics (XC90 build only): after each showUI, the alt screen frames of the next few seconds are logged and written to
/// `/tmp/altscreen-<n>.h264` (Annex-B with SPS/PPS, starting at a keyframe), so a car log shows whether the iPhone draws anything.
/// Bounded: at most [CAPTURE_MAX_COUNT] captures of at most [CAPTURE_MAX_BYTES] each per process.
const CAPTURE_WINDOW: Duration = Duration::from_secs(5);
const CAPTURE_MAX_BYTES: usize = 1 << 20;
const CAPTURE_MAX_COUNT: u32 = 4;

struct Capture {
    n: u32,
    until: Instant,
    frames: u32,
    buf: Vec<u8>,
}

impl Capture {
    /// Log and record one submitted frame; returns false once the capture is complete (the frame is then not recorded).
    fn add(&mut self, frame: &EncodedVideoFrame) -> bool {
        if Instant::now() >= self.until {
            return false;
        }
        let keyframe = frame.is_known_keyframe();
        info!(
            "Alt screen capture {}: frame {} {} bytes keyframe={keyframe}",
            self.n,
            self.frames,
            frame.data.len()
        );
        self.frames += 1;
        // Start at a keyframe so the file decodes; repeat SPS/PPS before every keyframe.
        if !keyframe && self.buf.is_empty() {
            return true;
        }
        let config = frame
            .config
            .as_ref()
            .filter(|_| keyframe)
            .map(|c| c.avcc.sps_pps.as_slice())
            .unwrap_or_default();
        if self.buf.len() + config.len() + frame.data.len() > CAPTURE_MAX_BYTES {
            return false;
        }
        self.buf.extend_from_slice(config);
        self.buf.extend_from_slice(&frame.data);
        true
    }

    /// Write the capture on a background thread so the frame path never waits on I/O.
    fn finish(self) {
        let path = format!("/tmp/altscreen-{}.h264", self.n);
        info!(
            "Alt screen capture {} complete: {} frames, {} bytes",
            self.n,
            self.frames,
            self.buf.len()
        );
        if self.buf.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            if let Err(err) = std::fs::write(&path, &self.buf) {
                warn!("Could not write {path}: {err}");
            }
        });
    }
}

impl AltScreenManager {
    /// Start proxying a new iPhone alt screen stream, which completes once the car SETUP (see [Self::set_pending_setup]) completes.
    ///
    /// Returns the previous car stream, if any, which should be torn down.
    pub fn set_iphone_screen(
        &mut self,
        channel: mpsc::UnboundedReceiver<ScreenProxyOp>,
        uuid: Option<String>,
    ) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        self.iphone_screen_rx.replace(channel);
        self.uuid = uuid;
        self.pending_keyframe.take();
        self.sent_config = None;
        self.resetup = false;
        self.lazy_setup = false;
        self.lazy_keyframe = None;
        self.keyframe_after_setup = false;
        self.car_screen.take()
    }

    /// XC90 diagnostics: do not SETUP the car stream yet; [Self::take_resetup] asks for it at the iPhone's first keyframe.
    pub fn defer_setup(&mut self) {
        self.lazy_setup = true;
    }

    pub fn set_pending_setup(&mut self, pending_setup: oneshot::Receiver<TeardownGuard<ScreenTransmitProxy>>) {
        self.pending_setup.replace(pending_setup);
    }

    /// Stop proxying, for example because the iPhone has disconnected.
    ///
    /// Returns the car stream, if any, which should be torn down.
    pub fn reset(&mut self) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        self.iphone_screen_rx.take();
        self.uuid.take();
        self.pending_setup.take();
        self.pending_keyframe.take();
        self.sent_config = None;
        self.resetup = false;
        self.lazy_setup = false;
        self.lazy_keyframe = None;
        self.keyframe_after_setup = false;
        if let Some(capture) = self.capture.take() {
            capture.finish();
        }
        self.car_screen.take()
    }

    /// Diagnostics: capture the frames of the next few seconds, called when the car sends showUI.
    pub fn capture_after_show_ui(&mut self) {
        if let Some(previous) = self.capture.take() {
            previous.finish();
        }
        let n = CAPTURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n > CAPTURE_MAX_COUNT {
            return;
        }
        info!("Alt screen capture {n} started (showUI)");
        self.capture = Some(Capture {
            n,
            until: Instant::now() + CAPTURE_WINDOW,
            frames: 0,
            buf: Vec::new(),
        });
    }

    fn trace(capture: &mut Option<Capture>, frame: &EncodedVideoFrame) {
        if let Some(active) = capture.as_mut()
            && !active.add(frame)
            && let Some(done) = capture.take()
        {
            done.finish();
        }
    }

    /// XC90 diagnostics ([Xc90Diag::alt_showui_resetup]): start a fresh car stream at showUI, so the car gets SETUP, VideoConfig and
    /// a keyframe right after it. Returns the live car stream, if any, which should be torn down.
    pub fn resetup_for_show_ui(&mut self) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        let screen = self.car_screen.take()?;
        info!("XC90: showUI, re-SETUP of the car's alt screen stream");
        self.sent_config = None;
        self.resetup = true;
        self.keyframe_after_setup = true;
        Some(screen)
    }

    /// XC90 diagnostics: the display UUID, if a re-SETUP of the car stream is due (cleared by this call).
    pub fn take_resetup(&mut self) -> Option<Option<String>> {
        std::mem::take(&mut self.resetup).then(|| self.uuid.clone())
    }

    /// XC90 diagnostics: apply [Xc90Diag::alt_config], and decide whether `config` needs a new car stream.
    fn diag_config(&mut self, diag: Xc90Diag, config: &mut AvccConfigExtended) -> bool {
        Xc90Diag::apply_config(diag.alt_config, config);
        let changed = self.sent_config.as_ref().is_some_and(|sent| sent != config);
        if changed && diag.alt_resetup && self.car_screen.is_some() {
            info!("XC90: alt screen config changed, re-SETUP of the car stream: {config:?}");
            self.sent_config = None;
            self.resetup = true;
            return true;
        }
        self.sent_config = Some(config.clone());
        false
    }

    /// Returns a car stream that is no longer needed, if any, which should be torn down.
    pub async fn reconcile(&mut self) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        if let Some(os) = self.pending_setup.as_mut()
            && let Some(result) = os.take()
        {
            self.pending_setup.take();

            match result {
                Ok(mut screen) if self.iphone_screen_rx.is_some() => {
                    info!("Alt screen is ready to receive iPhone frames");
                    self.sent_config = None;
                    self.keyframe_after_setup = false;

                    if let Some(mut keyframe) = self.pending_keyframe.take() {
                        warn!("Repeating iPhone alt screen keyframe received during SETUP");
                        keyframe.pts = (Instant::now() + Duration::from_millis(75)).into();
                        Self::trace(&mut self.capture, &keyframe);

                        if let Err((err, _frame)) = screen.push_frame(keyframe).await {
                            warn!("Sink rejected iPhone alt screen keyframe?: {err}");
                        }
                    }
                    // Frames after the held keyframe were dropped during the lazy SETUP: get a fresh keyframe as well.
                    if let (Some(handle), Some(uuid)) = (self.lazy_keyframe.take(), self.uuid.as_deref()) {
                        handle.request_display_keyframe(uuid);
                    }

                    self.car_screen.replace(screen);
                }
                Ok(screen) => {
                    // The iPhone has torn down its alt screen while the car SETUP was pending.
                    return Some(screen);
                }
                Err(_) => error!("Alt screen SETUP did not complete, dropping iPhone alt screen frames"),
            }
        }

        let op = self.iphone_screen_rx.as_mut().and_then(|rx| rx.take())?;

        match op {
            ScreenProxyOp::IphoneFrame {
                mut frame,
                handle,
                consumed,
            } => {
                if let Some(config) = frame.config.as_mut()
                    && self.diag_config(Xc90Diag::get(), config)
                {
                    let car_screen = self.car_screen.take();
                    if frame.is_known_keyframe() {
                        self.pending_keyframe.replace(frame);
                    }
                    let _ = consumed.send(());
                    return car_screen;
                }
                match self.car_screen.as_mut() {
                    None if frame.is_known_keyframe() => {
                        debug!("Holding iPhone alt screen keyframe until car alt screen is ready");
                        self.pending_keyframe.replace(frame);
                        if std::mem::take(&mut self.lazy_setup) {
                            info!("XC90: first iPhone alt screen keyframe, SETUP of the car stream now");
                            self.resetup = true;
                            self.lazy_keyframe = Some(handle);
                        }
                    }
                    None => {
                        debug!("Dropping iPhone alt screen frame, no car alt screen (yet)");
                        if self.keyframe_after_setup {
                            self.lazy_keyframe = Some(handle);
                        }
                    }
                    Some(car_screen) => {
                        Self::trace(&mut self.capture, &frame);
                        match car_screen.push_frame(frame).await {
                            Err((ScreenTransmitError::NeedsKeyframe, _frame)) => {
                                warn!("Dropping iPhone alt screen delta frame, requesting a keyframe ASAP");
                                match self.uuid.as_deref() {
                                    Some(uuid) => handle.request_display_keyframe(uuid),
                                    None => warn!("Cannot request alt screen keyframe without display UUID"),
                                }
                            }
                            Err((err, _frame)) => warn!("Sink rejected iPhone alt screen frame?: {err}"),
                            Ok(()) => {}
                        }
                    }
                }
                let _ = consumed.send(());
            }
            // A config update without a frame (for example the view switch on showUI) is passed on right away, or kept with the
            // held keyframe while the car SETUP is pending.
            ScreenProxyOp::IphoneConfig { mut config } => {
                if self.diag_config(Xc90Diag::get(), &mut config) {
                    return self.car_screen.take();
                }
                match self.car_screen.as_mut() {
                    Some(car_screen) => {
                        if Xc90Diag::get().alt_config_repeat {
                            car_screen.repeat_next_config();
                        }
                        if let Err(err) = car_screen.push_avcc_config(config) {
                            warn!("Sink rejected iPhone alt screen config?: {err}");
                        }
                    }
                    None => {
                        // A view change keeps the held keyframe decodable; any other change needs a new keyframe from the iPhone.
                        let held = self.pending_keyframe.take();
                        self.pending_keyframe = held.filter(|keyframe| {
                            keyframe
                                .config
                                .as_ref()
                                .is_some_and(|c| c.same_except_view(&config))
                        });
                        if let Some(keyframe) = self.pending_keyframe.as_mut() {
                            keyframe.config = Some(config);
                        }
                    }
                }
            }
            ScreenProxyOp::IphoneStreamFinish => {
                info!("iPhone has finished its alt screen stream");
                if let Some(capture) = self.capture.take() {
                    capture.finish();
                }
                self.iphone_screen_rx.take();
                return self.car_screen.take();
            }
            _ => {}
        }

        None
    }
}

impl Drop for AltScreenManager {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.finish();
        }
    }
}

impl EventSleeper for AltScreenManager {
    async fn sleep(&mut self) -> Option<EventToken> {
        event_select!(self.iphone_screen_rx, self.pending_setup)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catplay_util::notify::Notify;
    use futures::executor::block_on;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    const UUID: &str = "b7e6c5a0-2222-4000-8000-000000000002";

    /// Car stream that sets `torn_down` once it is dropped, which is what triggers the TEARDOWN towards the car.
    fn car_screen(torn_down: &Arc<AtomicBool>) -> TeardownGuard<ScreenTransmitProxy> {
        let proxy = ScreenTransmitProxy::new(
            Duration::ZERO,
            Default::default(),
            Default::default(),
            Notify::new(),
            Notify::new(),
            1,
        );
        let torn_down = torn_down.clone();
        TeardownGuard::new(proxy, move || torn_down.store(true, Ordering::SeqCst))
    }

    /// Wait for the next event and reconcile it, like the event loop of the car manager does.
    fn step(manager: &mut AltScreenManager) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        block_on(async {
            manager.sleep().await;
            manager.reconcile().await
        })
    }

    #[test]
    fn completed_setup_is_kept_until_iphone_finishes() {
        let torn_down = Arc::new(AtomicBool::new(false));
        let mut manager = AltScreenManager::default();
        let (iphone_tx, iphone_rx) = mpsc::unbounded();
        let (setup_tx, setup_rx) = oneshot::channel();

        assert!(
            manager
                .set_iphone_screen(iphone_rx, Some(UUID.into()))
                .is_none()
        );
        manager.set_pending_setup(setup_rx);

        let _ = setup_tx.send(car_screen(&torn_down));
        assert!(step(&mut manager).is_none());
        assert!(!torn_down.load(Ordering::SeqCst));

        let _ = iphone_tx.unbounded_send(ScreenProxyOp::IphoneStreamFinish);
        let screen = step(&mut manager).expect("car screen should be returned for teardown");
        drop(screen);
        assert!(torn_down.load(Ordering::SeqCst));
    }

    #[test]
    fn iphone_finishing_during_setup_returns_car_screen_for_teardown() {
        let torn_down = Arc::new(AtomicBool::new(false));
        let mut manager = AltScreenManager::default();
        let (iphone_tx, iphone_rx) = mpsc::unbounded();
        let (setup_tx, setup_rx) = oneshot::channel();

        manager.set_iphone_screen(iphone_rx, Some(UUID.into()));
        manager.set_pending_setup(setup_rx);

        // The iPhone tears down its alt screen before the car SETUP has completed.
        let _ = iphone_tx.unbounded_send(ScreenProxyOp::IphoneStreamFinish);
        assert!(step(&mut manager).is_none());

        let _ = setup_tx.send(car_screen(&torn_down));
        let screen = step(&mut manager).expect("late car screen should be returned for teardown");
        drop(screen);
        assert!(torn_down.load(Ordering::SeqCst));
    }

    #[test]
    fn failed_setup_is_ignored() {
        let mut manager = AltScreenManager::default();
        let (iphone_tx, iphone_rx) = mpsc::unbounded();
        let (setup_tx, setup_rx) = oneshot::channel::<TeardownGuard<ScreenTransmitProxy>>();

        manager.set_iphone_screen(iphone_rx, Some(UUID.into()));
        manager.set_pending_setup(setup_rx);

        // The car manager drops the sender when the car SETUP fails.
        drop(setup_tx);
        assert!(step(&mut manager).is_none());

        let _ = iphone_tx.unbounded_send(ScreenProxyOp::IphoneStreamFinish);
        assert!(step(&mut manager).is_none());
    }

    #[test]
    fn new_iphone_stream_returns_previous_car_screen() {
        let torn_down = Arc::new(AtomicBool::new(false));
        let mut manager = AltScreenManager::default();
        let (_iphone_tx, iphone_rx) = mpsc::unbounded();
        let (setup_tx, setup_rx) = oneshot::channel();

        manager.set_iphone_screen(iphone_rx, Some(UUID.into()));
        manager.set_pending_setup(setup_rx);
        let _ = setup_tx.send(car_screen(&torn_down));
        assert!(step(&mut manager).is_none());

        let (_iphone_tx, iphone_rx) = mpsc::unbounded();
        assert!(
            manager
                .set_iphone_screen(iphone_rx, Some(UUID.into()))
                .is_some()
        );
    }

    #[test]
    fn reset_returns_car_screen_once() {
        let torn_down = Arc::new(AtomicBool::new(false));
        let mut manager = AltScreenManager::default();
        let (_iphone_tx, iphone_rx) = mpsc::unbounded();
        let (setup_tx, setup_rx) = oneshot::channel();

        manager.set_iphone_screen(iphone_rx, Some(UUID.into()));
        manager.set_pending_setup(setup_rx);
        let _ = setup_tx.send(car_screen(&torn_down));
        assert!(step(&mut manager).is_none());

        assert!(manager.reset().is_some());
        assert!(manager.reset().is_none());
    }

    fn frame(len: usize, keyframe: bool) -> EncodedVideoFrame {
        EncodedVideoFrame {
            pts: Instant::now().into(),
            width: 1920,
            height: 720,
            data: vec![0u8; len].as_slice().into(),
            config: None,
            nal_offsets: None,
            is_keyframe: Some(keyframe),
            header_buf: Default::default(),
            chacha_tag_buf: Default::default(),
        }
    }

    fn capture() -> Capture {
        Capture {
            n: 1,
            until: Instant::now() + CAPTURE_WINDOW,
            frames: 0,
            buf: Vec::new(),
        }
    }

    #[test]
    fn capture_starts_at_a_keyframe_and_stops_at_its_byte_cap() {
        let mut c = capture();
        assert!(c.add(&frame(10, false)));
        assert!(c.buf.is_empty(), "delta frames before the first keyframe are not decodable");
        assert!(c.add(&frame(10, true)));
        assert!(c.add(&frame(10, false)));
        assert_eq!(c.buf.len(), 20);
        assert!(!c.add(&frame(CAPTURE_MAX_BYTES, true)));
        assert_eq!(c.buf.len(), 20, "a frame over the cap is not recorded");
    }

    #[test]
    fn capture_ends_after_its_window() {
        let mut c = capture();
        c.until = Instant::now();
        assert!(!c.add(&frame(10, true)));
        assert!(c.buf.is_empty());
    }

    #[test]
    fn captures_per_process_are_bounded() {
        let mut manager = AltScreenManager::default();
        for _ in 0..CAPTURE_MAX_COUNT + 2 {
            manager.capture_after_show_ui();
        }
        assert!(manager.capture.is_none(), "no capture beyond the per-process limit");
        // A new manager (next car session) shares the process-wide limit.
        let mut next_session = AltScreenManager::default();
        next_session.capture_after_show_ui();
        assert!(next_session.capture.is_none());
    }

    #[test]
    fn config_update_during_setup_is_kept_with_the_held_keyframe() {
        use catplay_carplay::video::{AvccConfig, AvccConfigExtended};

        let config = |view: u8| AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: vec![1, 2],
            },
            video_latency: Duration::ZERO,
            width: 1920,
            height: 720,
            respect_timestamps: true,
            view: [[0; 8], [view; 8], [0; 8]],
        };
        let mut manager = AltScreenManager::default();
        let (iphone_tx, iphone_rx) = mpsc::unbounded();
        let (_setup_tx, setup_rx) = oneshot::channel();
        manager.set_iphone_screen(iphone_rx, Some(UUID.into()));
        manager.set_pending_setup(setup_rx);
        manager.pending_keyframe = Some(EncodedVideoFrame {
            pts: Instant::now().into(),
            width: 1920,
            height: 720,
            data: [0u8; 4].as_slice().into(),
            config: Some(config(1)),
            nal_offsets: None,
            is_keyframe: Some(true),
            header_buf: Default::default(),
            chacha_tag_buf: Default::default(),
        });

        let _ = iphone_tx.unbounded_send(ScreenProxyOp::IphoneConfig { config: config(2) });
        assert!(step(&mut manager).is_none());
        let held = manager.pending_keyframe.as_ref().unwrap();
        assert_eq!(held.config, Some(config(2)), "the car stream must start with the latest view");

        let new_codec = AvccConfigExtended { width: 1280, ..config(2) };
        let _ = iphone_tx.unbounded_send(ScreenProxyOp::IphoneConfig { config: new_codec });
        assert!(step(&mut manager).is_none());
        assert!(manager.pending_keyframe.is_none(), "a keyframe for another codec config is dropped");
    }

    #[test]
    fn diag_config_requests_a_resetup_only_for_a_changed_config_on_a_live_stream() {
        use catplay_carplay::video::AvccConfig;

        let config = |view: u8| AvccConfigExtended {
            hevc: false,
            avcc: AvccConfig {
                nal_size_len: 4,
                sps_pps: vec![1, 2],
            },
            video_latency: Duration::ZERO,
            width: 1920,
            height: 720,
            respect_timestamps: true,
            view: [[0; 8], [view; 8], [0; 8]],
        };
        let torn_down = Arc::new(AtomicBool::new(false));
        let mut manager = AltScreenManager::default();
        manager.car_screen = Some(car_screen(&torn_down));
        let diag = Xc90Diag {
            alt_resetup: true,
            ..Default::default()
        };
        assert!(!manager.diag_config(diag, &mut config(1)), "the first config is just recorded");
        assert!(!manager.diag_config(diag, &mut config(1)), "an unchanged config needs nothing");
        assert!(
            !manager.diag_config(Default::default(), &mut config(2)),
            "no re-SETUP unless switched on"
        );
        assert!(manager.diag_config(diag, &mut config(3)));
        assert!(manager.take_resetup().is_some());
        assert!(
            !manager.diag_config(diag, &mut config(4)),
            "after a re-SETUP the next config starts the new stream"
        );
        let mut stripped = config(5);
        manager.diag_config(
            Xc90Diag {
                alt_config: 0,
                ..Default::default()
            },
            &mut stripped,
        );
        assert_eq!(stripped.view, [[0; 8]; 3], "mode 0 (build 13) sends no view");
        let mut build14 = config(5);
        manager.diag_config(Default::default(), &mut build14);
        assert_eq!(build14.view, [[0; 8], [5; 8], [0; 8]], "mode 1 (build 14) sends params 3 and 4 only");
        assert!(manager.take_resetup().is_none());
    }
}
