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

    pub fn as_ops(&self, theme: &UiTheme) -> Vec<RenderOp> {
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
                    Some(text) => text.replace("{device}", device).replace("{pin}", pin),
                    None => format!("Press any key to pair: {device} | {pin}"),
                },
                None,
            ),
        };

        let text_y = match &theme.background {
            // Leave the photo visible; the text sits near the bottom of the image.
            Some(background) => {
                ops.extend([
                    RenderOp::SetBackground { rgba: PHOTO_BACKDROP },
                    RenderOp::LoadAsset {
                        id: "background".into(),
                        path: background.clone(),
                        asset_type: AssetType::Image,
                    },
                    RenderOp::BlitAssetCover { id: "background".into() },
                    RenderOp::ReleaseAsset { id: "background".into() },
                ]);
                0.92
            }
            None => {
                ops.extend([
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
                ]);
                0.8
            }
        };

        // `text_y` is the baseline of the last line, so a subtitle lifts the main text instead
        // of pushing the block down (and possibly off a short screen).
        let text_size = theme.text_size.unwrap_or(30.0);
        match subtitle {
            None => ops.push(Self::text_op(text, text_y, text_size, 0xFFFFFFFF, None)),
            Some(subtitle) => {
                // About 1.3 main-text lines (font px = pt * dpi / 72, Dp px = dp * dpi / 160).
                let line_dp = 1.3 * text_size * 160.0 / 72.0;
                ops.push(Self::text_op(text, text_y, text_size, 0xFFFFFFFF, Some(line_dp)));
                ops.push(Self::text_op(subtitle, text_y, text_size * 0.6, SUBTITLE_COLOR, None));
            }
        }
        ops.extend([
            RenderOp::ReleaseAsset { id: "logo".into() },
            RenderOp::ReleaseFont { id: "font".into() },
        ]);
        ops
    }

    /// Centred text with its baseline at `y`, or `lift_dp` higher when given.
    fn text_op(text: String, y: f32, font_size_pt: f32, color: u32, lift_dp: Option<f32>) -> RenderOp {
        let (h, anchor) = match lift_dp {
            // Non-baseline anchors put the baseline at the vertical centre of the rect; a
            // bottom-anchored rect of height 2*lift therefore has its baseline `lift` above `y`.
            Some(dp) => (Unit::Dp(2.0 * dp), Anchor::BottomCenter),
            None => (Unit::Dp(0.0), Anchor::BaselineCenter),
        };
        RenderOp::DrawText {
            text,
            rect: UiRect {
                x: Unit::Percent(0.5),
                y: Unit::Percent(y),
                w: Unit::Dp(700.0),
                h,
                anchor,
            },
            align: TextAlign::Center,
            font_id: "font".into(),
            font_size_pt,
            color,
        }
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
            assert_eq!(state.as_ops(&theme), upstream_ops(&state), "{state}");
            assert_eq!(state.cache_hash(&theme), format!("ui-v1:{state}"));
        }
    }

    #[test]
    fn custom_theme_uses_background_and_texts() {
        let theme = UiTheme {
            background: Some(PathBuf::from("/persist/ui/background.jpg")),
            waiting_text: Some("Wachten op iPhone…".into()),
            subtitle: Some("Volvo XC90".into()),
            text_size: Some(20.0),
            connecting_text: Some("Verbinden met {device}".into()),
            pairing_text: Some("Koppelen met {device}: {pin}".into()),
            ..Default::default()
        };

        let ops = UiState::WaitingForConnection.as_ops(&theme);
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
        assert_eq!(texts(&connecting.as_ops(&theme)), ["Verbinden met iPhone."]);
        let pairing = UiState::Pairing {
            device: "iPhone".into(),
            pin: "1234".into(),
        };
        assert_eq!(texts(&pairing.as_ops(&theme)), ["Koppelen met iPhone: 1234"]);

        assert_ne!(
            UiState::WaitingForConnection.cache_hash(&theme),
            UiState::WaitingForConnection.cache_hash(&UiTheme::default())
        );
    }

    const SMALL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 17,
        73, 68, 65, 84, 120, 156, 99, 16, 80, 48, 248, 15, 194, 12, 48, 6, 0, 43, 180, 5, 125, 93, 178, 111, 66, 0, 0, 0, 0, 73, 69, 78,
        68, 174, 66, 96, 130,
    ];
    /// Valid header claiming 4000x4000 pixels, with truncated image data.
    const HUGE_HEADER_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 15, 160, 0, 0, 15, 160, 8, 6, 0, 0, 0, 238, 127, 227, 2, 0, 0,
        0, 9, 73, 68, 65, 84, 120, 156, 99, 0, 0, 0, 1, 0, 1, 94, 255, 125, 249, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];

    fn theme_with_background(dir: &std::path::Path, image: &[u8], extra: &str) -> UiTheme {
        let bg = dir.join("bg.png");
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
        let dir = std::env::temp_dir().join(format!("catplay-ui-bg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let ok = theme_with_background(&dir, SMALL_PNG, "");
        assert!(ok.background.is_some());
        assert!(ok.background_digest.is_some());

        let corrupt = theme_with_background(&dir, b"\x89PNG not really", "waiting_text = \"Wachten\"");
        assert_eq!(corrupt.background, None, "undecodable images fall back");
        assert_eq!(corrupt.waiting_text.as_deref(), Some("Wachten"), "other settings are kept");

        let huge = theme_with_background(&dir, HUGE_HEADER_PNG, "");
        assert_eq!(huge.background, None, "oversized images are rejected from the header");

        // A different image at the same path changes the cache key.
        let mut changed = ok.clone();
        changed.background_digest = Some(ok.background_digest.unwrap() ^ 1);
        assert_ne!(
            UiState::WaitingForConnection.cache_hash(&ok),
            UiState::WaitingForConnection.cache_hash(&changed)
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn text_settings_are_sanitized() {
        let dir = std::env::temp_dir().join(format!("catplay-ui-text-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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

    #[test]
    fn subtitle_lifts_the_main_text() {
        let theme = UiTheme {
            subtitle: Some("sub".into()),
            ..Default::default()
        };
        let ops = UiState::WaitingForConnection.as_ops(&theme);
        let anchors: Vec<_> = ops
            .iter()
            .filter_map(|op| match op {
                RenderOp::DrawText { rect, .. } => Some(rect.anchor),
                _ => None,
            })
            .collect();
        assert_eq!(anchors, [Anchor::BottomCenter, Anchor::BaselineCenter]);
    }

    #[test]
    fn theme_file_is_optional_and_validated() {
        let dir = std::env::temp_dir().join(format!("catplay-ui-theme-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

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
