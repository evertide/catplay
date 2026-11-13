use crate::ui::UiTheme;
use catplay_fb::{Anchor, AssetType, RenderOp, TextAlign, UiRect, Unit};
use std::fmt::{Display, Formatter};

const ASSET_LOGO: &[u8] = include_bytes!("../../assets/logo.jpg");
const ASSET_FONT: &[u8] = include_bytes!("../../assets/Exo2-Light.otf");
const CACHE_VERSION: &str = "ui-v1";
/// Pen colours use the framebuffer's native 0xAABBGGRR layout.
const SUBTITLE_COLOR: u32 = 0xFFAAA096;
const PHOTO_BACKDROP: u32 = 0xFF000000;

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
            // Leave the photo visible; text sits low on the (darkened) bottom of the image.
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
                0.88
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

        let text_size = theme.text_size.unwrap_or(30.0);
        ops.push(Self::text_op(text, text_y, text_size, 0xFFFFFFFF, None));
        if let Some(subtitle) = subtitle {
            // About 1.3 main-text lines below the main baseline (font px = pt * dpi / 72, Dp px = dp * dpi / 160).
            let line_dp = 1.3 * text_size * 160.0 / 72.0;
            ops.push(Self::text_op(subtitle, text_y, text_size * 0.6, SUBTITLE_COLOR, Some(line_dp)));
        }
        ops.extend([
            RenderOp::ReleaseAsset { id: "logo".into() },
            RenderOp::ReleaseFont { id: "font".into() },
        ]);
        ops
    }

    /// Centred text with its baseline at `y`, or `below_dp` further down when given.
    fn text_op(text: String, y: f32, font_size_pt: f32, color: u32, below_dp: Option<f32>) -> RenderOp {
        let (h, anchor) = match below_dp {
            // Non-baseline anchors put the baseline at the vertical centre of the rect.
            Some(dp) => (Unit::Dp(2.0 * dp), Anchor::TopCenter),
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

    #[test]
    fn default_theme_keeps_the_original_screens() {
        let theme = UiTheme::default();
        let ops = UiState::WaitingForConnection.as_ops(&theme);
        assert!(ops.contains(&RenderOp::SetBackground { rgba: BG_COLOR }));
        assert!(
            ops.iter()
                .any(|op| matches!(op, RenderOp::BlitAsset { id, .. } if id == "logo"))
        );
        assert!(
            !ops.iter()
                .any(|op| matches!(op, RenderOp::BlitAssetCover { .. }))
        );
        assert_eq!(texts(&ops), ["Waiting for connection..."]);

        let connecting = UiState::Connecting {
            device: "iPhone".into(),
            ticks: 1,
        };
        assert_eq!(texts(&connecting.as_ops(&theme)), ["Connecting to device: iPhone.."]);
        let pairing = UiState::Pairing {
            device: "iPhone".into(),
            pin: "1234".into(),
        };
        assert_eq!(texts(&pairing.as_ops(&theme)), ["Press any key to pair: iPhone | 1234"]);
        assert!(
            UiState::Hidden
                .as_ops(&theme)
                .iter()
                .all(|op| matches!(op, RenderOp::LoadAssetConst { .. }))
        );

        assert_eq!(UiState::WaitingForConnection.cache_hash(&theme), "ui-v1:WaitingForConnection");
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
