use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use bytes::BytesMut;
use catplay_carplay::video::{AvccConfig, AvccConfigExtended, EncodedVideoFrame, NalChunk, Pts};
use catplay_fb::{
    Canvas, H264FrameBuffer, H264FrameBufferError, KeyframeCacheEntry, KeyframeCacheFile, KeyframeCacheFileError, KeyframeCacheKey,
    RenderError, Renderer,
};
use catplay_tracing_macro::trace_time;
use log::{info, warn};

use crate::ui::{UiState, UiTheme};

pub struct UiRenderer {
    width: i32,
    height: i32,
    fps: u32,
    dpi: f32,
    fb: Renderer,
    h264: Option<H264FrameBuffer>,
    headers: BytesMut,
    last_state: UiState,
    persist_dir: Option<PathBuf>,
    theme: UiTheme,
}

#[derive(thiserror::Error, Debug)]
pub enum UiError {
    #[error("H264 error: {0}")]
    H264(#[from] H264FrameBufferError),
    #[error("Rendering error: {0}")]
    Render(#[from] RenderError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type UiResult<T> = Result<T, UiError>;

impl UiRenderer {
    const RELEASE_ENCODER_MEMORY: bool = true;

    pub fn new(width: i32, height: i32, fps: u32, dpi: f32, persist_dir: Option<PathBuf>) -> UiResult<Self> {
        Ok(Self {
            width,
            height,
            fps,
            dpi,
            fb: Renderer::new(width, height, dpi),
            h264: None,
            last_state: UiState::Hidden,
            headers: BytesMut::new(),
            theme: UiTheme::load(persist_dir.as_deref()),
            persist_dir,
        })
    }

    fn release_encoder(&mut self) {
        if Self::RELEASE_ENCODER_MEMORY {
            self.h264.take();
        }
    }

    fn render_framebuffer(&mut self, state: UiState, reset: bool) -> UiResult<()> {
        if state == UiState::Hidden || (!reset && self.last_state == state) {
            return Ok(());
        }

        if reset {
            self.fb.reset(self.width, self.height, self.dpi);
        }
        if let Err(err) = self.fb.execute(&state.as_ops(&self.theme)) {
            if self.theme.background.is_none() {
                return Err(err.into());
            }
            // The background was validated at load time, but the file may have changed since.
            warn!("Rendering the UI theme background failed ({err}); falling back to the default background");
            self.theme = self.theme.without_background();
            self.fb.reset(self.width, self.height, self.dpi);
            self.fb.execute(&state.as_ops(&self.theme))?;
        }
        self.last_state = state;
        Ok(())
    }

    #[trace_time(warn)]
    pub fn update(&mut self, state: UiState) -> UiResult<()> {
        self.render_framebuffer(state, false)
    }

    pub fn avcc_config(&mut self) -> UiResult<AvccConfig> {
        Ok(AvccConfig {
            nal_size_len: 4,
            sps_pps: self.headers.as_mut().into(),
        })
    }

    fn encode(&mut self) -> UiResult<BytesMut> {
        let mut out = BytesMut::new();
        // Lazy-init encoder
        if self.h264.is_none() {
            self.h264
                .replace(H264FrameBuffer::new(self.width, self.height, self.fps as _)?);
        }
        let h264 = self.h264.as_mut().unwrap();
        h264.update_rgba(Canvas::as_rgba(self.fb.framebuffer()), &mut out)?;

        self.headers.clear();
        h264.get_headers(&mut self.headers)?;
        // Release memory taken by encoder (X264 + YUV shadow buffer)
        self.release_encoder();
        Ok(out)
    }

    fn cache_file(&self, state: &UiState) -> Option<KeyframeCacheFile> {
        Some(KeyframeCacheFile::new(self.persist_dir.as_ref()?.join(state.cache_file_name()?)))
    }

    fn keyframe_cache_key(&self, state: &UiState) -> KeyframeCacheKey {
        KeyframeCacheKey {
            hash: state.cache_hash(&self.theme),
            width: self.width as u32,
            height: self.height as u32,
            dpi: self.dpi.round() as u32,
        }
    }

    fn frame_from_keyframe_entry(&self, entry: KeyframeCacheEntry, latency: Duration) -> EncodedVideoFrame {
        let frame_len = entry.keyframe.len();
        EncodedVideoFrame {
            pts: Pts(Instant::now() + latency),
            width: self.width as _,
            height: self.height as _,
            data: BytesMut::from(entry.keyframe.as_slice()),
            config: Some(AvccConfigExtended {
                hevc: false,
                avcc: AvccConfig {
                    nal_size_len: 4,
                    sps_pps: BytesMut::from(entry.sps_pps.as_slice()).into(),
                },
                video_latency: Duration::ZERO,
                width: self.width as _,
                height: self.height as _,
                respect_timestamps: true,
            }),
            nal_offsets: Some(vec![NalChunk {
                prefix_start: 0,
                prefix_len: 4,
                data_size: frame_len.saturating_sub(4),
            }]),
            is_keyframe: Some(true),
            chacha_tag_buf: BytesMut::new(),
            header_buf: BytesMut::new(),
        }
    }

    fn try_load_keyframe_cache(&self, state: &UiState, latency: Duration) -> Option<EncodedVideoFrame> {
        let Some(_cache_file_name) = state.cache_file_name() else {
            return None;
        };
        let Some(cache) = self.cache_file(state) else {
            info!("{state} keyframe cache disabled: persist_dir is not configured");
            return None;
        };

        match cache.load(&self.keyframe_cache_key(state)) {
            Ok(entry) => {
                info!("Loaded {state} keyframe cache from {}", cache.path().display());
                Some(self.frame_from_keyframe_entry(entry, latency))
            }
            Err(KeyframeCacheFileError::NotFound) => {
                info!("{state} keyframe cache not found at {}", cache.path().display());
                None
            }
            Err(KeyframeCacheFileError::OutOfDate) => {
                info!("{state} keyframe cache is out of date at {}", cache.path().display());
                None
            }
            Err(err) => {
                warn!("Failed to load {state} keyframe cache from {}: {err}", cache.path().display());
                None
            }
        }
    }

    fn store_keyframe_cache(&self, state: &UiState, frame: &BytesMut) {
        let Some(_cache_file_name) = state.cache_file_name() else {
            return;
        };
        let Some(cache) = self.cache_file(state) else {
            return;
        };

        let entry = KeyframeCacheEntry {
            sps_pps: self.headers.to_vec(),
            keyframe: frame.to_vec(),
        };
        match cache.store(&self.keyframe_cache_key(state), &entry) {
            Ok(()) => info!("Stored {state} keyframe cache at {}", cache.path().display()),
            Err(err) => warn!("Failed to store {state} keyframe cache at {}: {err}", cache.path().display()),
        }
    }

    #[trace_time(warn)]
    pub fn render_and_encode_frame(&mut self, state: UiState, latency: Duration, reset: bool) -> UiResult<EncodedVideoFrame> {
        if state.cache_file_name().is_some() {
            info!("UI renderer requested cacheable state {state}");
        }

        if !reset && let Some(frame) = self.try_load_keyframe_cache(&state, latency) {
            self.last_state = state;
            return Ok(frame);
        }

        self.render_framebuffer(state.clone(), reset)?;
        let frame = self.encode()?;
        let frame_len = frame.len();
        let config = self.avcc_config()?;

        if state.cache_file_name().is_some() {
            info!("Generated {state} keyframe live");
        }
        self.store_keyframe_cache(&state, &frame);

        Ok(EncodedVideoFrame {
            pts: Pts(Instant::now() + latency),
            width: self.width as _,
            height: self.height as _,
            data: frame,
            config: Some(AvccConfigExtended {
                hevc: false,
                avcc: config,
                video_latency: Duration::ZERO, // Ignored
                width: self.width as _,
                height: self.height as _,
                respect_timestamps: true,
            }),
            nal_offsets: Some(vec![NalChunk {
                prefix_start: 0,
                prefix_len: 4,
                data_size: frame_len.saturating_sub(4),
            }]),
            is_keyframe: Some(true),
            chacha_tag_buf: BytesMut::new(),
            header_buf: BytesMut::new(),
        })
    }

    #[trace_time(warn)]
    pub fn encode_frame(&mut self, latency: Duration) -> UiResult<EncodedVideoFrame> {
        self.render_and_encode_frame(self.last_state.clone(), latency, false)
    }

    pub fn export_img_to_disk(&mut self, path: PathBuf) -> UiResult<()> {
        self.fb.export_to_disk(path)?;
        Ok(())
    }

    pub fn export_h264_to_disk(&mut self, path: PathBuf) -> UiResult<()> {
        let mut out = BytesMut::new();
        let encoded = self.encode()?;
        out.extend_from_slice(&self.headers);
        out.extend_from_slice(&encoded);
        std::fs::write(path, out)?;

        Ok(())
    }
}

#[test]
fn test_render() {
    use catplay_tracing::logger::setup_test_logger;
    setup_test_logger(true);
    let mut renderer = UiRenderer::new(1920, 720, 60, 168.0, None).unwrap();
    renderer
        .update(UiState::Connecting {
            device: "Test".into(),
            ticks: 2,
        })
        .unwrap();
    renderer.export_img_to_disk("test.png".into()).unwrap();
    renderer.export_h264_to_disk("test.h264".into()).unwrap();
}

#[test]
fn test_rendered_frame_can_be_serialized_by_transmitter() {
    use catplay_carplay::{clock::MediaClockSession, screen::ScreenFrame};

    let mut renderer = UiRenderer::new(800, 480, 60, 160.0, None).unwrap();
    let frame = renderer
        .render_and_encode_frame(
            UiState::Connecting {
                device: "Test".into(),
                ticks: 0,
            },
            Duration::ZERO,
            false,
        )
        .unwrap();

    assert!(!frame.data.is_empty(), "x264 must emit the first frame without requiring a flush");

    ScreenFrame::video_proxied(frame, 4, &MediaClockSession::new())
        .expect("renderer output must be accepted by the transmitter NAL serializer");
}

#[test]
fn test_waiting_for_connection_keyframe_cache_cycle() {
    use catplay_tracing::logger::setup_test_logger;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    setup_test_logger(true);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let persist_dir = std::env::temp_dir().join(format!("catplay-ui-cache-test-{}-{unique}", std::process::id()));
    std::fs::create_dir(&persist_dir).unwrap();

    let mut renderer = UiRenderer::new(1920, 720, 60, 168.0, Some(persist_dir.clone())).unwrap();
    renderer
        .render_and_encode_frame(UiState::WaitingForConnection, Duration::ZERO, false)
        .unwrap();

    let cache_file = persist_dir.join("catplay_welcome.h264");
    assert!(cache_file.exists());

    renderer
        .render_and_encode_frame(UiState::WaitingForConnection, Duration::ZERO, false)
        .unwrap();

    let _ = std::fs::remove_file(cache_file);
    let _ = std::fs::remove_dir(persist_dir);
}

#[test]
fn test_broken_theme_background_falls_back_to_default() {
    use catplay_tracing::logger::setup_test_logger;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    setup_test_logger(true);

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let persist_dir = std::env::temp_dir().join(format!("catplay-ui-fallback-test-{}-{unique}", std::process::id()));
    std::fs::create_dir(&persist_dir).unwrap();

    // A valid 2x2 PNG, accepted when the theme is loaded...
    const SMALL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 17,
        73, 68, 65, 84, 120, 156, 99, 16, 80, 48, 248, 15, 194, 12, 48, 6, 0, 43, 180, 5, 125, 93, 178, 111, 66, 0, 0, 0, 0, 73, 69, 78,
        68, 174, 66, 96, 130,
    ];
    let background = persist_dir.join("bg.png");
    std::fs::write(&background, SMALL_PNG).unwrap();
    std::fs::write(
        persist_dir.join(UiTheme::FILE_NAME),
        format!("background = {:?}\n", background.display().to_string()),
    )
    .unwrap();

    let mut renderer = UiRenderer::new(800, 480, 30, 160.0, Some(persist_dir.clone())).unwrap();
    assert!(renderer.theme.background.is_some());

    // ...but corrupted before the first render.
    std::fs::write(&background, b"not an image").unwrap();
    renderer
        .render_and_encode_frame(UiState::WaitingForConnection, Duration::ZERO, false)
        .expect("a broken background must not suppress the screen");
    assert!(renderer.theme.background.is_none());

    let _ = std::fs::remove_dir_all(persist_dir);
}
