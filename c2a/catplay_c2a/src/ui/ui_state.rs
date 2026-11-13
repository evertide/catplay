use catplay_fb::{Anchor, AssetType, RenderOp, TextAlign, UiRect, Unit};
use std::fmt::{Display, Formatter};

const ASSET_LOGO: &[u8] = include_bytes!("../../assets/logo.jpg");
const ASSET_FONT: &[u8] = include_bytes!("../../assets/Exo2-Light.otf");
const CACHE_VERSION: &str = "ui-v1";

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

    pub fn cache_hash(&self) -> String {
        format!("{CACHE_VERSION}:{self}")
    }

    pub fn as_ops(&self) -> Vec<RenderOp> {
        let mut ops = ASSET_OPS.to_vec();
        ops.extend_from_slice(&match self {
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
}

impl Display for UiState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
