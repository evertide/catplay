use crate::ui::UiState;
use catplay_fb::{Anchor, AssetCache, AssetType, RenderOp, TextAlign, UiRect, Unit};
use log::{info, warn};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Limits that keep a user-provided background well within the dongle's RAM.
const MAX_BACKGROUND_BYTES: u64 = 1 << 20;
const MAX_BACKGROUND_SIDE: u32 = 2048;
const MAX_BACKGROUND_PIXELS: u64 = 1_000_000;
const MAX_TEXT_CHARS: usize = 120;
const TEXT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=72.0;
const DEFAULT_TEXT_SIZE: f32 = 30.0;

/// Text (pen) colours use the framebuffer's native 0xAABBGGRR layout.
const TEXT_COLOR: u32 = 0xFFFFFFFF;
const SUBTITLE_COLOR: u32 = 0xFFAAA096;
/// `SetBackground` takes 0xRRGGBBAA.
const PHOTO_BACKDROP: u32 = 0x000000FF;

/// Optional, user-provided look of the built-in screens, read from `<persist_dir>/ui.toml`.
///
/// Every field is optional; without the file the upstream screens are rendered unchanged.
/// Invalid values are ignored with a warning. The background must be a JPEG of at most 1 MiB,
/// 2048 px per side and 1 megapixel, ideally already the screen's size; a relative path is
/// resolved against the persist directory. Text is only shrunk to fit vertically, so keep it
/// short or lower `text_size` on narrow screens.
///
/// ```toml
/// background = "ui/background.jpg"
/// waiting_text = "Waiting for connection..."
/// subtitle = "My car"
/// text_size = 20.0
/// connecting_text = "Connecting to device: {device}"
/// pairing_text = "Press any key to pair: {device} | {pin}"
/// ```
#[derive(Clone, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct UiTheme {
    /// Full-screen background image (JPEG), scaled to cover the screen.
    pub background: Option<PathBuf>,
    pub waiting_text: Option<String>,
    /// Smaller second line under the waiting text.
    pub subtitle: Option<String>,
    /// Main text size in points (default 30).
    pub text_size: Option<f32>,
    /// `{device}` is replaced by the phone name.
    pub connecting_text: Option<String>,
    /// `{device}` and `{pin}` are replaced.
    pub pairing_text: Option<String>,
    /// The background bytes checked at load; rendering uses these, never the file again.
    #[serde(skip)]
    pub background_bytes: Option<Arc<[u8]>>,
    /// CRC-32 of `background_bytes`, part of the keyframe cache key.
    #[serde(skip)]
    pub background_crc: Option<u32>,
}

impl std::fmt::Debug for UiTheme {
    /// Like the derived impl, but shows only the size of the background bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiTheme")
            .field("background", &self.background)
            .field("background_len", &self.background_bytes.as_ref().map(|bytes| bytes.len()))
            .field("background_crc", &self.background_crc)
            .field("waiting_text", &self.waiting_text)
            .field("subtitle", &self.subtitle)
            .field("text_size", &self.text_size)
            .field("connecting_text", &self.connecting_text)
            .field("pairing_text", &self.pairing_text)
            .finish()
    }
}

/// Size of the frame the UI is rendered into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiScreen {
    pub height: i32,
    pub dpi: f32,
}

impl UiTheme {
    pub const FILE_NAME: &'static str = "ui.toml";

    /// Loads the theme from the persist directory; any problem falls back to the default look.
    ///
    /// Reads at most 1 MiB and parses only the JPEG header; the image is decoded when rendered.
    pub fn load(persist_dir: Option<&Path>) -> Self {
        let Some(dir) = persist_dir else {
            return Self::default();
        };
        let path = dir.join(Self::FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                warn!("Ignoring UI theme {path:?}: {err}");
                return Self::default();
            }
        };
        let mut theme: Self = match toml::from_str(&text) {
            Ok(theme) => theme,
            Err(err) => {
                warn!("Ignoring UI theme {path:?}: {err}");
                return Self::default();
            }
        };
        theme.sanitize(dir);
        info!("Loaded UI theme from {path:?}: {theme:?}");
        theme
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Drops or clamps values that could break rendering, keeping the rest.
    fn sanitize(&mut self, persist_dir: &Path) {
        if let Some(size) = self.text_size
            && !(size.is_finite() && TEXT_SIZE_RANGE.contains(&size))
        {
            warn!("UI theme text_size {size} is outside {TEXT_SIZE_RANGE:?}; using the default");
            self.text_size = None;
        }
        for text in [
            &mut self.waiting_text,
            &mut self.subtitle,
            &mut self.connecting_text,
            &mut self.pairing_text,
        ]
        .into_iter()
        .flatten()
        {
            if text.chars().count() > MAX_TEXT_CHARS {
                warn!("UI theme text is longer than {MAX_TEXT_CHARS} characters; truncating");
                *text = text.chars().take(MAX_TEXT_CHARS).collect();
            }
        }
        self.background_bytes = None;
        self.background_crc = None;
        if let Some(background) = self.background.take() {
            let background = persist_dir.join(background);
            match Self::check_background(&background) {
                Ok(bytes) => {
                    self.background_crc = Some(crc32fast::hash(&bytes));
                    self.background_bytes = Some(bytes);
                    self.background = Some(background);
                }
                Err(err) => warn!("UI theme background {background:?} not used: {err}"),
            }
        }
    }

    /// Bounded read plus a header-only check of format and dimensions.
    fn check_background(path: &Path) -> Result<Arc<[u8]>, String> {
        use std::io::Read;
        // Read at most one byte past the limit, so neither a growing file nor a device that
        // reports no size can exceed it.
        let mut buf = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_BACKGROUND_BYTES + 1).read_to_end(&mut buf))
            .map_err(|err| err.to_string())?;
        if buf.len() as u64 > MAX_BACKGROUND_BYTES {
            return Err(format!("file is larger than {MAX_BACKGROUND_BYTES} bytes"));
        }
        let (w, h) = AssetCache::jpeg_dimensions(&buf).map_err(|err| err.to_string())?;
        if w == 0 || h == 0 || w > MAX_BACKGROUND_SIDE || h > MAX_BACKGROUND_SIDE || w as u64 * h as u64 > MAX_BACKGROUND_PIXELS {
            return Err(format!(
                "image is {w}x{h}, limit {MAX_BACKGROUND_SIDE} px per side and {MAX_BACKGROUND_PIXELS} pixels"
            ));
        }
        Ok(buf.into())
    }

    /// The same theme without its background, used when the image cannot be rendered.
    pub fn without_background(&self) -> Self {
        Self {
            background: None,
            background_bytes: None,
            background_crc: None,
            ..self.clone()
        }
    }

    /// Keyframe cache key of `state` rendered with this theme.
    pub fn cache_hash(&self, state: &UiState) -> String {
        format!(
            "{}:theme:{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            state.cache_hash(),
            self.background,
            self.background_crc,
            self.waiting_text,
            self.subtitle,
            self.text_size,
            self.connecting_text,
            self.pairing_text
        )
    }

    /// The upstream screen of `state` with this theme's background and text swapped in.
    pub fn ops(&self, state: &UiState, screen: UiScreen) -> Vec<RenderOp> {
        let upstream = state.as_ops();
        let Some(text) = self.text(state, &upstream) else {
            return upstream;
        };
        let subtitle = matches!(state, UiState::WaitingForConnection)
            .then(|| self.subtitle.clone())
            .flatten();

        let mut ops = Vec::with_capacity(upstream.len() + 4);
        for op in upstream {
            match (op, &self.background_bytes) {
                (RenderOp::SetBackground { .. }, Some(bytes)) => ops.extend([
                    RenderOp::SetBackground { rgba: PHOTO_BACKDROP },
                    RenderOp::LoadAssetShared {
                        id: "background".into(),
                        buf: bytes.clone(),
                        asset_type: AssetType::Image,
                    },
                    RenderOp::BlitAssetCover { id: "background".into() },
                    RenderOp::ReleaseAsset { id: "background".into() },
                ]),
                (RenderOp::BlitAsset { .. }, Some(_)) => {}
                (RenderOp::DrawText { .. }, _) => ops.extend(self.text_ops(text.clone(), subtitle.clone(), screen)),
                (op, _) => ops.push(op),
            }
        }
        ops
    }

    /// The themed main text for `state`, or `None` for states without text.
    fn text(&self, state: &UiState, upstream: &[RenderOp]) -> Option<String> {
        let upstream_text = upstream.iter().find_map(|op| match op {
            RenderOp::DrawText { text, .. } => Some(text.clone()),
            _ => None,
        })?;
        let custom = match state {
            UiState::Hidden => None,
            UiState::WaitingForConnection => self.waiting_text.clone(),
            UiState::Connecting { device, ticks } => self
                .connecting_text
                .as_ref()
                .map(|text| text.replace("{device}", device) + &".".repeat(ticks % 3 + 1)),
            // The PIN is digits only, so replacing it first keeps the device name literal.
            UiState::Pairing { device, pin } => self
                .pairing_text
                .as_ref()
                .map(|text| text.replace("{pin}", pin).replace("{device}", device)),
        };
        Some(custom.unwrap_or(upstream_text))
    }

    /// Bottom-aligned text block in pixels: the main line, then an optional smaller subtitle.
    ///
    /// The block is shrunk to at most a third of the screen height and keeps a bottom margin,
    /// so it stays on screen for any text size. `DrawText` treats `y` as the top of the line
    /// (the font renderer adds the ascent).
    fn text_ops(&self, text: String, subtitle: Option<String>, screen: UiScreen) -> Vec<RenderOp> {
        const LINE: f32 = 1.25;
        const SUBTITLE_SCALE: f32 = 0.6;
        let height = screen.height.max(1) as f32;
        let px_per_pt = screen.dpi.max(1.0) / 72.0;
        let lines = 1.0 + if subtitle.is_some() { SUBTITLE_SCALE } else { 0.0 };

        let mut size = self.text_size.unwrap_or(DEFAULT_TEXT_SIZE);
        let max_block = height / 3.0;
        let block = size * px_per_pt * LINE * lines;
        if block > max_block {
            size *= max_block / block;
        }
        let main_h = size * px_per_pt * LINE;
        let total = main_h * lines;
        let top = (height * 0.94 - total).max(0.0);

        let line = |text: String, y: f32, font_size_pt: f32, color: u32| RenderOp::DrawText {
            text,
            rect: UiRect {
                x: Unit::Percent(0.5),
                y: Unit::Px(y),
                w: Unit::Px(0.0),
                h: Unit::Px(0.0),
                anchor: Anchor::BaselineCenter,
            },
            align: TextAlign::Center,
            font_id: "font".into(),
            font_size_pt,
            color,
        };
        let mut ops = vec![line(text, top, size, TEXT_COLOR)];
        if let Some(subtitle) = subtitle {
            ops.push(line(subtitle, top + main_h, size * SUBTITLE_SCALE, SUBTITLE_COLOR));
        }
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: UiScreen = UiScreen { height: 480, dpi: 160.0 };
    const SMALL_JPEG: &[u8] = include_bytes!("testdata/small.jpg");

    fn texts(ops: &[RenderOp]) -> Vec<String> {
        ops.iter()
            .filter_map(|op| match op {
                RenderOp::DrawText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// (top y, font size) of each text line, in order.
    fn text_lines(ops: &[RenderOp]) -> Vec<(f32, f32)> {
        ops.iter()
            .filter_map(|op| match op {
                RenderOp::DrawText { rect, font_size_pt, .. } => match rect.y {
                    Unit::Px(y) => Some((y, *font_size_pt)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The test JPEG with its frame header (SOF0) claiming `w`x`h` pixels.
    fn jpeg_claiming(w: u16, h: u16) -> Vec<u8> {
        let mut jpeg = SMALL_JPEG.to_vec();
        let sof = jpeg
            .windows(2)
            .position(|m| m == [0xFF, 0xC0])
            .expect("SOF0");
        jpeg[sof + 5..sof + 7].copy_from_slice(&h.to_be_bytes());
        jpeg[sof + 7..sof + 9].copy_from_slice(&w.to_be_bytes());
        jpeg
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("catplay-ui-theme-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn load_with_background(dir: &Path, image: &[u8], extra: &str) -> UiTheme {
        std::fs::write(dir.join("bg.jpg"), image).unwrap();
        std::fs::write(dir.join(UiTheme::FILE_NAME), format!("background = \"bg.jpg\"\n{extra}")).unwrap();
        UiTheme::load(Some(dir))
    }

    fn all_states() -> Vec<UiState> {
        vec![
            UiState::Hidden,
            UiState::WaitingForConnection,
            UiState::Connecting {
                device: "iPhone".into(),
                ticks: 4,
            },
            UiState::Pairing {
                device: "iPhone".into(),
                pin: "1234".into(),
            },
        ]
    }

    #[test]
    fn theme_without_changes_renders_the_upstream_screens() {
        let theme = UiTheme {
            text_size: None,
            ..Default::default()
        };
        assert!(theme.is_default());
        for state in all_states() {
            // No custom text or background: the only change is the text layout.
            assert_eq!(texts(&theme.ops(&state, SCREEN)), texts(&state.as_ops()), "{state}");
        }
        assert_eq!(UiState::Hidden.as_ops(), theme.ops(&UiState::Hidden, SCREEN));
    }

    #[test]
    fn theme_swaps_background_and_texts() {
        let theme = UiTheme {
            background_bytes: Some(SMALL_JPEG.into()),
            waiting_text: Some("Wachten op iPhone…".into()),
            subtitle: Some("Volvo XC90".into()),
            text_size: Some(20.0),
            connecting_text: Some("Verbinden met {device}".into()),
            pairing_text: Some("Koppelen met {device}: {pin}".into()),
            ..Default::default()
        };

        let ops = theme.ops(&UiState::WaitingForConnection, SCREEN);
        assert!(
            ops.iter()
                .any(|op| matches!(op, RenderOp::LoadAssetShared { id, .. } if id == "background"))
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RenderOp::BlitAssetCover { id } if id == "background"))
        );
        assert!(
            !ops.iter()
                .any(|op| matches!(op, RenderOp::BlitAsset { .. }))
        );
        assert_eq!(texts(&ops), ["Wachten op iPhone…", "Volvo XC90"]);

        let connecting = UiState::Connecting {
            device: "iPhone".into(),
            ticks: 0,
        };
        assert_eq!(texts(&theme.ops(&connecting, SCREEN)), ["Verbinden met iPhone."]);
        let pairing = UiState::Pairing {
            device: "Phone {pin}".into(),
            pin: "1234".into(),
        };
        assert_eq!(texts(&theme.ops(&pairing, SCREEN)), ["Koppelen met Phone {pin}: 1234"]);

        let state = UiState::WaitingForConnection;
        assert_ne!(theme.cache_hash(&state), state.cache_hash());
        let changed = UiTheme {
            background_crc: Some(1),
            ..theme.clone()
        };
        assert_ne!(
            theme.cache_hash(&state),
            changed.cache_hash(&state),
            "a different image changes the key"
        );
    }

    #[test]
    fn text_block_stays_on_screen() {
        for (screen, size) in [
            (SCREEN, 72.0),
            (SCREEN, 30.0),
            (UiScreen { height: 880, dpi: 160.0 }, 20.0),
            (UiScreen { height: 720, dpi: 240.0 }, 72.0),
            (UiScreen { height: 240, dpi: 160.0 }, 72.0),
        ] {
            for subtitle in [None, Some("sub".to_string())] {
                let theme = UiTheme {
                    subtitle: subtitle.clone(),
                    text_size: Some(size),
                    ..Default::default()
                };
                let lines = text_lines(&theme.ops(&UiState::WaitingForConnection, screen));
                assert_eq!(lines.len(), 1 + subtitle.is_some() as usize);
                let (last_top, last_size) = *lines.last().unwrap();
                let bottom = last_top + last_size * screen.dpi / 72.0 * 1.25;
                assert!(lines.iter().all(|(y, _)| *y >= 0.0), "{screen:?} {size} {lines:?}");
                assert!(bottom <= screen.height as f32, "{screen:?} {size}: text ends at {bottom}");
                if lines.len() == 2 {
                    assert!(lines[0].0 < lines[1].0, "subtitle is below the main text");
                }
            }
        }
    }

    #[test]
    fn theme_file_is_optional_and_validated() {
        let dir = temp_dir("file");
        assert!(UiTheme::load(Some(&dir)).is_default());
        assert!(UiTheme::load(None).is_default());

        std::fs::write(
            dir.join(UiTheme::FILE_NAME),
            "waiting_text = \"Wachten\"\nbackground = \"/does/not/exist.jpg\"\n",
        )
        .unwrap();
        let theme = UiTheme::load(Some(&dir));
        assert_eq!(theme.waiting_text.as_deref(), Some("Wachten"));
        assert_eq!(theme.background_bytes, None, "a missing background falls back to the default");

        std::fs::write(dir.join(UiTheme::FILE_NAME), "unknown_key = 1\n").unwrap();
        assert!(UiTheme::load(Some(&dir)).is_default(), "invalid files are ignored");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn background_is_checked_before_use() {
        let dir = temp_dir("bg");

        let ok = load_with_background(&dir, SMALL_JPEG, "");
        assert_eq!(ok.background_bytes.as_deref(), Some(SMALL_JPEG), "rendering uses the checked bytes");
        assert_eq!(
            ok.background,
            Some(dir.join("bg.jpg")),
            "relative paths resolve against persist_dir"
        );
        assert!(format!("{ok:?}").len() < 500, "Debug must not dump the image bytes");

        let no_header = load_with_background(&dir, &SMALL_JPEG[..100], "waiting_text = \"Wachten\"");
        assert_eq!(no_header.background_bytes, None, "images without a readable header fall back");
        assert_eq!(no_header.waiting_text.as_deref(), Some("Wachten"), "other settings are kept");

        let png = load_with_background(&dir, b"\x89PNG\r\n\x1a\n", "");
        assert_eq!(png.background_bytes, None, "only JPEG is accepted");

        let huge = load_with_background(&dir, &jpeg_claiming(4000, 4000), "");
        assert_eq!(huge.background_bytes, None, "oversized images are rejected from the header");
        let too_many_pixels = load_with_background(&dir, &jpeg_claiming(1200, 1000), "");
        assert_eq!(too_many_pixels.background_bytes, None);

        // A valid JPEG padded past the limit (decoders ignore trailing data).
        let mut padded = SMALL_JPEG.to_vec();
        padded.resize((1 << 20) + 1, 0);
        let big_file = load_with_background(&dir, &padded, "");
        assert_eq!(big_file.background_bytes, None, "files over the size limit are rejected");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn text_settings_are_sanitized() {
        let dir = temp_dir("text");
        for bad in ["0.0", "-5.0", "1000.0", "inf", "nan"] {
            std::fs::write(dir.join(UiTheme::FILE_NAME), format!("text_size = {bad}\n")).unwrap();
            assert_eq!(UiTheme::load(Some(&dir)).text_size, None, "text_size = {bad}");
        }
        std::fs::write(
            dir.join(UiTheme::FILE_NAME),
            format!("text_size = 20.0\nwaiting_text = \"{}\"\n", "x".repeat(500)),
        )
        .unwrap();
        let theme = UiTheme::load(Some(&dir));
        assert_eq!(theme.text_size, Some(20.0));
        assert_eq!(theme.waiting_text.unwrap().chars().count(), 120);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
