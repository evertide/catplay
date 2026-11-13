use crate::ui::UiTheme;
use catplay_fb::{Anchor, AssetType, RenderOp, TextAlign, UiRect, Unit};
use std::fmt::{Display, Formatter};

const ASSET_LOGO: &[u8] = include_bytes!("../../assets/logo.jpg");
const ASSET_FONT: &[u8] = include_bytes!("../../assets/Exo2-Light.otf");
const CACHE_VERSION: &str = "ui-v1";
/// Text (pen) colours use the framebuffer's native 0xAABBGGRR layout.
const SUBTITLE_COLOR: u32 = 0xFFAAA096;
/// SetBackground takes 0xRRGGBBAA.
const PHOTO_BACKDROP: u32 = 0x000000FF;

const ASSET_OPS: [RenderOp; 2] = [
    RenderOp::LoadAssetConst {
        id: "logo",
        buf: ASSET_LOGO,
        asset_type: AssetType::Image,
    },
    RenderOp::LoadAssetConst {
        id: "font",
        buf: ASSET_FONT,
        asset_type: AssetType::Font,
    },
];

#[cfg(not(debug_assertions))]
const BG_COLOR: u32 = 0x000055FF;
#[cfg(debug_assertions)]
const BG_COLOR: u32 = 0xFF0000FF;

/// Size of the frame the UI is rendered into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub width: i32,
    pub height: i32,
    pub dpi: f32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UiState {
    Hidden,
    WaitingForConnection,
    Connecting { device: String, ticks: usize },
    Pairing { device: String, pin: String },
}

impl UiState {
    pub fn cache_file_name(&self) -> Option<String> {
        match self {
            UiState::WaitingForConnection => Some("catplay_welcome.h264".to_string()),
            _ => None,
        }
    }

    pub fn cache_hash(&self, theme: &UiTheme) -> String {
        if *theme == UiTheme::default() {
            format!("{CACHE_VERSION}:{self}")
        } else {
            format!("{CACHE_VERSION}:{self}:{}", theme.fingerprint())
        }
    }

    pub fn as_ops(&self, theme: &UiTheme, screen: Screen) -> Vec<RenderOp> {
        let mut ops = ASSET_OPS.to_vec();
        let (text, subtitle) = match self {
            UiState::Hidden => return ops,
            UiState::WaitingForConnection => (
                theme
                    .waiting_text
                    .clone()
                    .unwrap_or_else(|| "Waiting for connection...".into()),
                theme.subtitle.clone(),
            ),
            UiState::Connecting { device, ticks } => (
                match &theme.connecting_text {
                    Some(text) => text.replace("{device}", device),
                    None => format!("Connecting to device: {device}"),
                } + &".".repeat(ticks % 3 + 1),
                None,
            ),
            UiState::Pairing { device, pin } => (
                match &theme.pairing_text {
                    // The PIN is digits only, so replacing it first keeps the device name literal.
                    Some(text) => text.replace("{pin}", pin).replace("{device}", device),
                    None => format!("Press any key to pair: {device} | {pin}"),
                },
                None,
            ),
        };

        match &theme.background_bytes {
            // Leave the photo visible; the text sits near the bottom of the image.
            Some(bytes) => ops.extend([
                RenderOp::SetBackground { rgba: PHOTO_BACKDROP },
                RenderOp::LoadAssetShared {
                    id: "background".into(),
                    buf: bytes.clone(),
                    asset_type: AssetType::Image,
                },
                RenderOp::BlitAssetCover { id: "background".into() },
                RenderOp::ReleaseAsset { id: "background".into() },
            ]),
            None => ops.extend([
                RenderOp::SetBackground { rgba: BG_COLOR },
                RenderOp::BlitAsset {
                    id: "logo".into(),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.5),
                        w: Unit::Dp(300.0),
                        h: Unit::Dp(300.0),
                        anchor: Anchor::Center,
                    },
                },
            ]),
        }

        if *theme == UiTheme::default() {
            ops.push(RenderOp::DrawText {
                text,
                rect: UiRect {
                    x: Unit::Percent(0.5),
                    y: Unit::Percent(0.8),
                    w: Unit::Dp(700.0),
                    h: Unit::Dp(0.0),
                    anchor: Anchor::BaselineCenter,
                },
                align: TextAlign::Center,
                font_id: "font".into(),
                font_size_pt: 30.0,
                color: 0xFFFFFFFF,
            });
        } else {
            ops.extend(Self::themed_text_ops(text, subtitle, theme.text_size, screen));
        }
        ops.extend([
            RenderOp::ReleaseAsset { id: "logo".into() },
            RenderOp::ReleaseFont { id: "font".into() },
        ]);
        ops
    }

    /// Bottom-aligned text block in pixels: the main line, then an optional smaller subtitle.
    ///
    /// The block is shrunk to at most a third of the screen height and keeps a bottom margin,
    /// so it stays on screen for any text size. `DrawText` treats `y` as the top of the line
    /// (the font renderer adds the ascent).
    fn themed_text_ops(text: String, subtitle: Option<String>, text_size: Option<f32>, screen: Screen) -> Vec<RenderOp> {
        const LINE: f32 = 1.25;
        const SUBTITLE_SCALE: f32 = 0.6;
        let height = screen.height.max(1) as f32;
        let px_per_pt = screen.dpi.max(1.0) / 72.0;
        let lines = 1.0 + if subtitle.is_some() { SUBTITLE_SCALE } else { 0.0 };

        let mut size = text_size.unwrap_or(30.0);
        let max_block = height / 3.0;
        let block = size * px_per_pt * LINE * lines;
        if block > max_block {
            size *= max_block / block;
        }
        let main_h = size * px_per_pt * LINE;
        let sub_h = main_h * SUBTITLE_SCALE;
        let total = main_h + if subtitle.is_some() { sub_h } else { 0.0 };
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
        let mut ops = vec![line(text, top, size, 0xFFFFFFFF)];
        if let Some(subtitle) = subtitle {
            ops.push(line(subtitle, top + main_h, size * SUBTITLE_SCALE, SUBTITLE_COLOR));
        }
        ops
    }
}

impl Display for UiState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn texts(ops: &[RenderOp]) -> Vec<String> {
        ops.iter()
            .filter_map(|op| match op {
                RenderOp::DrawText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Verbatim copy of `UiState::as_ops` from v0.29.0, the compatibility reference.
    fn upstream_ops(state: &UiState) -> Vec<RenderOp> {
        let mut ops = ASSET_OPS.to_vec();
        ops.extend_from_slice(&match state {
            UiState::Hidden => vec![],
            UiState::WaitingForConnection => vec![
                RenderOp::SetBackground { rgba: BG_COLOR },
                RenderOp::BlitAsset {
                    id: "logo".into(),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.5),
                        w: Unit::Dp(300.0),
                        h: Unit::Dp(300.0),
                        anchor: Anchor::Center,
                    },
                },
                RenderOp::DrawText {
                    text: format!("Waiting for connection..."),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.8),
                        w: Unit::Dp(700.0),
                        h: Unit::Dp(0.0),
                        anchor: Anchor::BaselineCenter,
                    },
                    align: TextAlign::Center,
                    font_id: "font".into(),
                    font_size_pt: 30.0,
                    color: 0xFFFFFFFF,
                },
                RenderOp::ReleaseAsset { id: "logo".into() },
                RenderOp::ReleaseFont { id: "font".into() },
            ],

            UiState::Connecting { device, ticks } => vec![
                RenderOp::SetBackground { rgba: BG_COLOR },
                RenderOp::BlitAsset {
                    id: "logo".into(),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.5),
                        w: Unit::Dp(300.0),
                        h: Unit::Dp(300.0),
                        anchor: Anchor::Center,
                    },
                },
                RenderOp::DrawText {
                    text: format!("Connecting to device: {device}{}", ".".repeat(ticks % 3 + 1)),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.8),
                        w: Unit::Dp(700.0),
                        h: Unit::Dp(0.0),
                        anchor: Anchor::BaselineCenter,
                    },
                    align: TextAlign::Center,
                    font_id: "font".into(),
                    font_size_pt: 30.0,
                    color: 0xFFFFFFFF,
                },
                RenderOp::ReleaseAsset { id: "logo".into() },
                RenderOp::ReleaseFont { id: "font".into() },
            ],

            UiState::Pairing { device, pin } => vec![
                RenderOp::SetBackground { rgba: BG_COLOR },
                RenderOp::BlitAsset {
                    id: "logo".into(),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.5),
                        w: Unit::Dp(300.0),
                        h: Unit::Dp(300.0),
                        anchor: Anchor::Center,
                    },
                },
                RenderOp::DrawText {
                    text: format!("Press any key to pair: {device} | {pin}"),
                    rect: UiRect {
                        x: Unit::Percent(0.5),
                        y: Unit::Percent(0.8),
                        w: Unit::Dp(700.0),
                        h: Unit::Dp(0.0),
                        anchor: Anchor::BaselineCenter,
                    },
                    align: TextAlign::Center,
                    font_id: "font".into(),
                    font_size_pt: 30.0,
                    color: 0xFFFFFFFF,
                },
                RenderOp::ReleaseAsset { id: "logo".into() },
                RenderOp::ReleaseFont { id: "font".into() },
            ],
        });
        ops
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
    fn default_theme_keeps_the_original_screens() {
        let theme = UiTheme::default();
        for state in all_states() {
            assert_eq!(state.as_ops(&theme, SCREEN), upstream_ops(&state), "{state}");
            assert_eq!(state.cache_hash(&theme), format!("ui-v1:{state}"));
        }
    }

    #[test]
    fn custom_theme_uses_background_and_texts() {
        let theme = UiTheme {
            background: Some(PathBuf::from("/persist/ui/background.jpg")),
            background_bytes: Some(SMALL_JPEG.into()),
            waiting_text: Some("Wachten op iPhone…".into()),
            subtitle: Some("Volvo XC90".into()),
            text_size: Some(20.0),
            connecting_text: Some("Verbinden met {device}".into()),
            pairing_text: Some("Koppelen met {device}: {pin}".into()),
            ..Default::default()
        };

        let ops = UiState::WaitingForConnection.as_ops(&theme, SCREEN);
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
        assert_eq!(texts(&connecting.as_ops(&theme, SCREEN)), ["Verbinden met iPhone."]);
        let pairing = UiState::Pairing {
            device: "iPhone".into(),
            pin: "1234".into(),
        };
        assert_eq!(texts(&pairing.as_ops(&theme, SCREEN)), ["Koppelen met iPhone: 1234"]);
        let tricky = UiState::Pairing {
            device: "Phone {pin}".into(),
            pin: "1234".into(),
        };
        assert_eq!(texts(&tricky.as_ops(&theme, SCREEN)), ["Koppelen met Phone {pin}: 1234"]);

        assert_ne!(
            UiState::WaitingForConnection.cache_hash(&theme),
            UiState::WaitingForConnection.cache_hash(&UiTheme::default())
        );
    }

    const SCREEN: Screen = Screen {
        width: 800,
        height: 480,
        dpi: 160.0,
    };
    const SMALL_JPEG: &[u8] = include_bytes!("testdata/small.jpg");

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
        let dir = std::env::temp_dir().join(format!("catplay-ui-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn theme_with_background(dir: &std::path::Path, image: &[u8], extra: &str) -> UiTheme {
        let bg = dir.join("bg.jpg");
        std::fs::write(&bg, image).unwrap();
        std::fs::write(
            dir.join(UiTheme::FILE_NAME),
            format!("background = {:?}\n{extra}", bg.display().to_string()),
        )
        .unwrap();
        UiTheme::load(Some(dir))
    }

    #[test]
    fn background_is_validated_before_use() {
        let dir = temp_dir("bg");

        let ok = theme_with_background(&dir, SMALL_JPEG, "");
        assert_eq!(
            ok.background_bytes.as_deref(),
            Some(SMALL_JPEG),
            "rendering uses the validated bytes"
        );
        assert!(ok.background_digest.is_some());

        let corrupt = theme_with_background(&dir, &SMALL_JPEG[..200], "waiting_text = \"Wachten\"");
        assert_eq!(corrupt.background_bytes, None, "undecodable images fall back");
        assert_eq!(corrupt.waiting_text.as_deref(), Some("Wachten"), "other settings are kept");

        let png = theme_with_background(&dir, b"\x89PNG\r\n\x1a\n", "");
        assert_eq!(png.background_bytes, None, "only JPEG is accepted");

        let huge = theme_with_background(&dir, &jpeg_claiming(4000, 4000), "");
        assert_eq!(huge.background_bytes, None, "oversized images are rejected from the header");
        let too_many_pixels = theme_with_background(&dir, &jpeg_claiming(1200, 1000), "");
        assert_eq!(too_many_pixels.background_bytes, None);

        // A valid JPEG padded past the limit (decoders ignore trailing data).
        let mut padded = SMALL_JPEG.to_vec();
        padded.resize((1 << 20) + 1, 0);
        let big_file = theme_with_background(&dir, &padded, "");
        assert_eq!(big_file.background_bytes, None, "files over the size limit are rejected");
        assert!(format!("{ok:?}").len() < 500, "Debug must not dump the image bytes");

        let mut changed = ok.clone();
        changed.background_digest = Some(ok.background_digest.unwrap() ^ 1);
        assert_ne!(
            UiState::WaitingForConnection.cache_hash(&ok),
            UiState::WaitingForConnection.cache_hash(&changed),
            "a different image changes the cache key"
        );

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

    #[test]
    fn themed_text_block_stays_on_screen() {
        for (screen, size) in [
            (SCREEN, 72.0),
            (SCREEN, 30.0),
            (
                Screen {
                    width: 768,
                    height: 880,
                    dpi: 160.0,
                },
                20.0,
            ),
            (
                Screen {
                    width: 1920,
                    height: 720,
                    dpi: 240.0,
                },
                72.0,
            ),
            (
                Screen {
                    width: 400,
                    height: 240,
                    dpi: 160.0,
                },
                72.0,
            ),
        ] {
            for subtitle in [None, Some("sub".to_string())] {
                let theme = UiTheme {
                    subtitle: subtitle.clone(),
                    text_size: Some(size),
                    waiting_text: Some("Wachten".into()),
                    ..Default::default()
                };
                let lines = text_lines(&UiState::WaitingForConnection.as_ops(&theme, screen));
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
        let dir = temp_dir("theme");

        assert_eq!(UiTheme::load(Some(&dir)), UiTheme::default());
        assert_eq!(UiTheme::load(None), UiTheme::default());

        std::fs::write(
            dir.join(UiTheme::FILE_NAME),
            "waiting_text = \"Wachten\"\nbackground = \"/does/not/exist.jpg\"\n",
        )
        .unwrap();
        let theme = UiTheme::load(Some(&dir));
        assert_eq!(theme.waiting_text.as_deref(), Some("Wachten"));
        assert_eq!(theme.background, None, "a missing background must fall back to the default");

        std::fs::write(dir.join(UiTheme::FILE_NAME), "unknown_key = 1\n").unwrap();
        assert_eq!(UiTheme::load(Some(&dir)), UiTheme::default(), "invalid files are ignored");

        std::fs::remove_dir_all(dir).unwrap();
    }
}
