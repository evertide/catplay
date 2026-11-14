use super::proxy_screen::ScreenProxyOp;
use crate::{
    proxy_rpc::OverlayPolicyDefault,
    ui::{ThemeFlag, UiRenderer, UiResult, UiState},
};
use catplay_carplay::{
    carplay_rx::AirPlayReceiverHandleRef,
    carplay_tx::TeardownGuard,
    screen::tx::{ScreenTransmitError, ScreenTransmitProxy, ScreenTransmitSink},
    video::EncodedVideoFrame,
};
use catplay_util::{EventReconciler, EventSleeper, EventToken, deadline, event_select, futures_xordered::FuturesOrdered, mpsc};
use futures::{FutureExt, future::BoxFuture};
use log::{debug, error, info, warn};
use std::{
    mem,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicUsize},
    time::{Duration, Instant},
};
use tokio::task::spawn_blocking;

pub struct OverlayManager {
    width: u32,
    height: u32,
    fps: u32,
    dpi: f32,
    persist_dir: Option<PathBuf>,
    lazy_screen: Option<TeardownGuard<ScreenTransmitProxy>>,

    overlay_render_queue: FuturesOrdered<BoxFuture<'static, UiResult<EncodedVideoFrame>>>,
    overlay_current: Option<UiState>,
    overlay_last_keyframe: Option<EncodedVideoFrame>,

    iphone_peer: Option<AirPlayReceiverHandleRef>,

    iphone_screen_rx: Option<mpsc::UnboundedReceiver<ScreenProxyOp>>,
    iphone_screen_active: bool,

    renderer: Option<Arc<Mutex<UiRenderer>>>,
    // Whether the current renderer draws a UI theme (read without locking the renderer)
    renderer_theme: Option<ThemeFlag>,

    policy: OverlayPolicyDefault,
    // stream_state: ResourceTransferType,
}

impl OverlayManager {
    pub fn new(width: u32, height: u32, fps: u32, dpi: f32, persist_dir: Option<PathBuf>, iphone_bt_links: Arc<AtomicUsize>) -> Self {
        Self {
            width,
            height,
            fps,
            dpi,
            persist_dir,

            overlay_render_queue: Default::default(),
            overlay_current: None,
            overlay_last_keyframe: None,

            iphone_peer: None,

            iphone_screen_rx: None,
            iphone_screen_active: false,

            renderer: None,
            renderer_theme: None,

            policy: OverlayPolicyDefault::new(iphone_bt_links),
            lazy_screen: None,
        }
    }

    pub fn set_car_peer(&mut self, lazy_screen: TeardownGuard<ScreenTransmitProxy>) {
        self.lazy_screen.replace(lazy_screen);
    }

    pub fn drop_car_peer(&mut self) -> Option<TeardownGuard<ScreenTransmitProxy>> {
        self.lazy_screen.take()
    }

    pub fn has_car_peer(&mut self) -> bool {
        self.lazy_screen.is_some()
    }

    pub fn set_iphone_peer(&mut self, iphone: Option<AirPlayReceiverHandleRef>) {
        self.iphone_peer = iphone;
    }

    pub fn set_iphone_peer_screen(&mut self, screen: Option<mpsc::UnboundedReceiver<ScreenProxyOp>>) {
        self.iphone_screen_rx = screen;
        self.iphone_screen_active = false;
        if let Some(peer) = self.iphone_peer.as_mut() {
            peer.request_keyframe();
        }
    }

    pub fn on_hid_interact(&mut self) -> bool {
        self.policy.on_hid_interact()
    }

    fn clear_render_queue(&mut self) {
        std::mem::take(&mut self.overlay_render_queue);
        self.release_renderer();
    }

    fn reset_overlay(&mut self) {
        self.overlay_current.take();
        self.overlay_last_keyframe.take();

        if let Some(iphone) = self.iphone_peer.as_mut() {
            iphone.request_keyframe();
        }
    }

    async fn reconcile_screen_iphone(&mut self) {
        let Some(op) = self.iphone_screen_rx.as_mut().and_then(|rx| rx.take()) else {
            return;
        };

        match op {
            ScreenProxyOp::IphoneFrame { frame, handle, consumed } => {
                if self.overlay_current.is_some() {
                    debug!("Dropping iPhone frame because overlay is active");
                    let _ = consumed.send(());
                    return;
                }

                let Some(lazy_screen) = self.lazy_screen.as_mut() else {
                    warn!("Dropping frame, no screen");
                    let _ = consumed.send(());
                    return;
                };

                let pts = frame.pts;
                match lazy_screen.push_frame(frame).await {
                    Err((ScreenTransmitError::NeedsKeyframe, _frame)) => {
                        warn!("Dropping iPhone delta frame, requesting a keyframe ASAP");
                        handle.request_keyframe();
                    }
                    Err((err, _frame)) => {
                        warn!("Sink rejected iPhone frame?: {err}");
                    }
                    Ok(()) => {
                        debug!("Proxied iPhone frame at pts = {}", pts);
                    }
                }
                let _ = consumed.send(());
            }
            ScreenProxyOp::IphoneStreamStart { .. } => self.iphone_screen_active = true,
            ScreenProxyOp::IphoneStreamFinish => self.iphone_screen_active = false,
            _ => {}
        }
    }

    async fn reconcile_overlay_frame(&mut self) {
        // If last overlay was rendered before SETUP, unglitch by sending a copy of the overlay keyframe
        // Check if we have a FIFO rendered frame to publish
        let Some(lazy_screen) = self.lazy_screen.as_mut() else {
            return;
        };

        if self.overlay_current.is_some()
            && let Some(mut keyframe) = self.overlay_last_keyframe.clone()
            && lazy_screen.needs_keyframe()
        {
            warn!("Repeating overlay keyframe");
            keyframe.pts = (Instant::now() + Duration::from_millis(75)).into();

            if let Err((err, _)) = lazy_screen.push_frame(keyframe).await {
                warn!("Sink rejected UI frame?: {err}");
                // TODO: critical
            }
        }

        // TODO dont use push_frame.await; queue instead
        match self.overlay_render_queue.take() {
            Some(Ok(mut frame)) => match self.overlay_current.is_some() {
                false => {
                    warn!("Dropped rendered overlay frame, because overlay is already disabled by now");
                }
                true => {
                    frame.pts = (Instant::now() + Duration::from_millis(75)).into();
                    self.overlay_last_keyframe.replace(frame.clone());

                    if let Err((err, _)) = lazy_screen.push_frame(frame).await {
                        warn!("Sink rejected UI frame?: {err}");
                        // TODO: critical
                    }
                }
            },
            Some(Err(err)) => error!("Failed to render overlay frame: {err}"),
            _ => {}
        }
    }

    async fn reconcile_overlay_next(&mut self) {
        self.policy.set_peer_state(self.peer_state());
        // Unknown until the renderer drew its first screen: show the plain waiting screen till then.
        let themed = self.renderer_theme.as_ref().and_then(ThemeFlag::get);
        self.policy.set_themed(themed.unwrap_or(false));
        let _ = self.policy.reconcile().await;

        let overlay_next = self.policy.overlay();

        debug!("reconcile_overlay_next");
        match (self.overlay_current.clone(), overlay_next.clone()) {
            (None, None) => {
                // Overlay stays inactive
            }
            (Some(_), None) => {
                // Deactivate overlay and request iPhone keyframe
                self.clear_render_queue();
                self.reset_overlay();
            }
            (Some(old), Some(current)) if old == current => {
                // Overlay stays the same.
                // Consider repeating the keyframe or ticking the UI animation if needed.
                // self.overlay_current.unwrap().tick();
            }
            (_, Some(next)) => {
                // Something new to render
                self.push_overlay_queue(next);
            }
        }

        self.overlay_current = overlay_next.clone();
    }

    fn peer_state(&self) -> OverlayPeerState {
        match (self.iphone_peer.is_some(), self.iphone_screen_active) {
            (false, _) => OverlayPeerState::Unconnected,
            (true, false) => OverlayPeerState::ConnectedWithoutScreen,
            (true, true) => OverlayPeerState::Connected,
        }
    }

    fn push_overlay_queue(&mut self, next: UiState) {
        let renderer = self.init_renderer();
        let latency = Duration::from_millis(25);

        let task = async move {
            spawn_blocking(move || {
                let mut renderer = renderer.lock().unwrap();
                renderer.render_and_encode_frame(next, latency, false)
            })
            .await
            .unwrap()
        };

        if self.overlay_render_queue.len() >= 5 {
            error!("Overlay render queue overflowing, system too slow?");
            let _ = mem::take(&mut self.overlay_render_queue);
        }

        self.overlay_render_queue.push_back(task.boxed());
    }

    pub fn init_renderer(&mut self) -> Arc<Mutex<UiRenderer>> {
        if self.renderer.is_none() {
            let renderer = UiRenderer::new(self.width as _, self.height as _, self.fps, self.dpi, self.persist_dir.clone())
                .expect("failed to init renderer");
            self.renderer_theme.replace(renderer.theme_flag());
            self.renderer.replace(Arc::new(Mutex::new(renderer)));
            info!("Initialized UI renderer");
        }

        self.renderer.clone().unwrap()
    }

    pub fn release_renderer(&mut self) {
        self.renderer_theme.take();
        if self.renderer.take().is_some() {
            info!("Released UI renderer");
        }
    }

    pub async fn reconcile(&mut self) {
        self.reconcile_overlay_frame().await;
        self.reconcile_overlay_next().await;
        self.reconcile_screen_iphone().await;
    }

    pub fn force_keyframe(&mut self) {
        if let Some(iphone) = self.iphone_peer.as_ref() {
            iphone.request_keyframe();
        } else if let Some(a) = self.lazy_screen.as_mut()
            && self.overlay_current.is_some()
            && let Some(mut last) = self.overlay_last_keyframe.clone()
        {
            warn!("Force keyframe: overlay frame");
            last.pts = (Instant::now() + Duration::from_millis(75)).into();
            let _ = a.push_frame(last.clone()).now_or_never();
        }
    }
}

impl EventSleeper for OverlayManager {
    async fn sleep(&mut self) -> Option<EventToken> {
        event_select!(
            self.policy,
            self.overlay_render_queue,
            self.iphone_screen_rx,
            deadline(Instant::now() + Duration::from_millis(200))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayPeerState {
    Unconnected,
    Connected,
    ConnectedWithoutScreen,
}
pub trait OverlayPolicy: EventReconciler<Error = ()> + EventSleeper {
    fn set_peer_state(&mut self, state: OverlayPeerState);

    /// Whether the waiting screens are drawn with a UI theme (the connecting phase needs one).
    fn set_themed(&mut self, themed: bool);

    fn overlay(&mut self) -> Option<UiState>;

    fn on_hid_interact(&mut self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiTheme;
    use std::sync::atomic::Ordering;

    fn render_first_screen(renderer: &Arc<Mutex<UiRenderer>>) {
        renderer
            .lock()
            .unwrap()
            .render_and_encode_frame(UiState::WaitingForConnection, Duration::ZERO, false)
            .unwrap();
    }

    #[tokio::test]
    async fn connecting_phase_follows_the_current_renderers_theme() {
        let dir = std::env::temp_dir().join(format!("catplay-overlay-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let theme_file = dir.join(UiTheme::FILE_NAME);
        let links = Arc::new(AtomicUsize::new(0));
        let mut manager = OverlayManager::new(800, 480, 30, 160.0, Some(dir.clone()), links.clone());

        // No theme: a found iPhone changes nothing and queues no extra render.
        render_first_screen(&manager.init_renderer());
        manager.reconcile_overlay_next().await;
        assert_eq!(manager.overlay_current, Some(UiState::WaitingForConnection));
        let queued = manager.overlay_render_queue.len();
        links.store(1, Ordering::Relaxed);
        manager.reconcile_overlay_next().await;
        assert_eq!(manager.overlay_current, Some(UiState::WaitingForConnection));
        assert_eq!(manager.overlay_render_queue.len(), queued, "no extra render without a theme");

        // A theme added while the renderer was released is used by the next one, with the phase.
        manager.release_renderer();
        std::fs::write(&theme_file, "connecting_text = \"Verbinden met {device}\"\n").unwrap();
        let themed = manager.init_renderer();
        manager.reconcile_overlay_next().await;
        assert_eq!(
            manager.overlay_current,
            Some(UiState::WaitingForConnection),
            "unknown until the renderer drew its first screen"
        );
        render_first_screen(&themed);
        manager.reconcile_overlay_next().await;
        assert!(matches!(manager.overlay_current, Some(UiState::Connecting { .. })));

        // Removed again: the next renderer has no theme, so neither does the phase.
        manager.release_renderer();
        std::fs::remove_file(&theme_file).unwrap();
        manager.reconcile_overlay_next().await;
        assert_eq!(
            manager.overlay_current,
            Some(UiState::WaitingForConnection),
            "a released renderer's theme no longer counts"
        );
        let stale = manager.init_renderer();
        manager.release_renderer();
        render_first_screen(&manager.init_renderer());
        manager.reconcile_overlay_next().await;
        assert_eq!(manager.overlay_current, Some(UiState::WaitingForConnection));

        // A job of a released renderer that still loads a theme cannot flip the current one.
        std::fs::write(&theme_file, "connecting_text = \"Verbinden met {device}\"\n").unwrap();
        render_first_screen(&stale);
        assert_eq!(stale.lock().unwrap().theme_flag().get(), Some(true));
        manager.reconcile_overlay_next().await;
        assert_eq!(manager.overlay_current, Some(UiState::WaitingForConnection));

        std::fs::remove_dir_all(dir).unwrap();
    }
}
