use super::proxy_screen::ScreenProxyOp;
use catplay_carplay::{
    carplay_tx::TeardownGuard,
    screen::tx::{ScreenTransmitError, ScreenTransmitProxy, ScreenTransmitSink},
    video::EncodedVideoFrame,
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
        self.car_screen.take()
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
        self.car_screen.take()
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

                    if let Some(mut keyframe) = self.pending_keyframe.take() {
                        warn!("Repeating iPhone alt screen keyframe received during SETUP");
                        keyframe.pts = (Instant::now() + Duration::from_millis(75)).into();

                        if let Err((err, _frame)) = screen.push_frame(keyframe).await {
                            warn!("Sink rejected iPhone alt screen keyframe?: {err}");
                        }
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
            ScreenProxyOp::IphoneFrame { frame, handle, consumed } => {
                match self.car_screen.as_mut() {
                    None if frame.is_known_keyframe() => {
                        debug!("Holding iPhone alt screen keyframe until car alt screen is ready");
                        self.pending_keyframe.replace(frame);
                    }
                    None => debug!("Dropping iPhone alt screen frame, no car alt screen (yet)"),
                    Some(car_screen) => match car_screen.push_frame(frame).await {
                        Err((ScreenTransmitError::NeedsKeyframe, _frame)) => {
                            warn!("Dropping iPhone alt screen delta frame, requesting a keyframe ASAP");
                            match self.uuid.as_deref() {
                                Some(uuid) => handle.request_display_keyframe(uuid),
                                None => warn!("Cannot request alt screen keyframe without display UUID"),
                            }
                        }
                        Err((err, _frame)) => warn!("Sink rejected iPhone alt screen frame?: {err}"),
                        Ok(()) => {}
                    },
                }
                let _ = consumed.send(());
            }
            // A config update without a frame (for example the view switch on showUI) is passed on right away, or kept with the
            // held keyframe while the car SETUP is pending.
            ScreenProxyOp::IphoneConfig { config } => match self.car_screen.as_mut() {
                Some(car_screen) => {
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
            },
            ScreenProxyOp::IphoneStreamFinish => {
                info!("iPhone has finished its alt screen stream");
                self.iphone_screen_rx.take();
                return self.car_screen.take();
            }
            _ => {}
        }

        None
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
            view: [[0; 8], [view; 8]],
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
}
