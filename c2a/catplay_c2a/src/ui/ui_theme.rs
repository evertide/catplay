use log::{info, warn};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Optional, user-provided look of the built-in screens, read from `<persist_dir>/ui.toml`.
///
/// Every field is optional; without the file the screens look exactly as before.
///
/// ```toml
/// background = "/persist/ui/background.jpg"
/// waiting_text = "Waiting for connection..."
/// subtitle = "My car"
/// text_size = 20.0
/// connecting_text = "Connecting to device: {device}"
/// pairing_text = "Press any key to pair: {device} | {pin}"
/// ```
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct UiTheme {
    /// Full-screen background image (JPEG/PNG), scaled to cover the screen.
    pub background: Option<PathBuf>,
    pub waiting_text: Option<String>,
    /// Smaller second line under the main text.
    pub subtitle: Option<String>,
    /// Main text size in points (default 30); narrow or portrait screens may need less.
    pub text_size: Option<f32>,
    /// `{device}` is replaced by the phone name.
    pub connecting_text: Option<String>,
    /// `{device}` and `{pin}` are replaced.
    pub pairing_text: Option<String>,
}

impl UiTheme {
    pub const FILE_NAME: &'static str = "ui.toml";

    /// Loads the theme from the persist directory; any problem falls back to the default look.
    pub fn load(persist_dir: Option<&Path>) -> Self {
        let Some(path) = persist_dir.map(|dir| dir.join(Self::FILE_NAME)) else {
            return Self::default();
        };
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
        if let Some(background) = &theme.background
            && !background.is_file()
        {
            warn!("UI theme background {background:?} does not exist; using the default background");
            theme.background = None;
        }
        info!("Loaded UI theme from {path:?}: {theme:?}");
        theme
    }

    /// Changes whenever the rendered result may change, so cached frames are not reused.
    pub fn fingerprint(&self) -> String {
        let background = self.background.as_ref().map(|path| {
            let meta = std::fs::metadata(path).ok();
            let len = meta.as_ref().map(|m| m.len()).unwrap_or_default();
            let modified = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or_default();
            format!("{}:{len}:{modified}", path.display())
        });
        format!(
            "{background:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.waiting_text, self.subtitle, self.text_size, self.connecting_text, self.pairing_text
        )
    }
}
