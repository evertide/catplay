use catplay_fb::AssetCache;
use log::{info, warn};
use serde::Deserialize;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Limits that keep a user-provided background well within the dongle's RAM.
const MAX_BACKGROUND_BYTES: u64 = 1 << 20;
const MAX_BACKGROUND_SIDE: u32 = 2048;
const MAX_BACKGROUND_PIXELS: u64 = 1_000_000;
const MAX_TEXT_CHARS: usize = 120;
const TEXT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 8.0..=72.0;

/// Optional, user-provided look of the built-in screens, read from `<persist_dir>/ui.toml`.
///
/// Every field is optional; without the file the screens look exactly as before. Invalid values
/// are ignored with a warning. The background must be a JPEG of at most 1 MiB, 2048 px per side
/// and 1 megapixel; ideally it already has the screen's size.
///
/// ```toml
/// background = "/persist/ui/background.jpg"
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
    /// Smaller second line under the main text.
    pub subtitle: Option<String>,
    /// Main text size in points (default 30); narrow or portrait screens may need less.
    pub text_size: Option<f32>,
    /// `{device}` is replaced by the phone name.
    pub connecting_text: Option<String>,
    /// `{device}` and `{pin}` are replaced.
    pub pairing_text: Option<String>,
    /// The validated background bytes; rendering uses these, never the file again.
    #[serde(skip)]
    pub background_bytes: Option<Arc<[u8]>>,
    /// Content digest of `background_bytes`, part of the keyframe cache key.
    #[serde(skip)]
    pub background_digest: Option<u64>,
}

impl std::fmt::Debug for UiTheme {
    /// Like the derived impl, but shows only the size of the background bytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiTheme")
            .field("background", &self.background)
            .field("background_len", &self.background_bytes.as_ref().map(|bytes| bytes.len()))
            .field("waiting_text", &self.waiting_text)
            .field("subtitle", &self.subtitle)
            .field("text_size", &self.text_size)
            .field("connecting_text", &self.connecting_text)
            .field("pairing_text", &self.pairing_text)
            .field("background_digest", &self.background_digest)
            .finish()
    }
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
        theme.sanitize();
        info!("Loaded UI theme from {path:?}: {theme:?}");
        theme
    }

    /// Drops or clamps values that could break rendering, keeping the rest.
    fn sanitize(&mut self) {
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
        self.background_digest = None;
        if let Some(background) = self.background.take() {
            match Self::check_background(&background) {
                Ok((bytes, digest)) => {
                    self.background = Some(background);
                    self.background_bytes = Some(bytes);
                    self.background_digest = Some(digest);
                }
                Err(err) => warn!("UI theme background {background:?} not used: {err}"),
            }
        }
    }

    /// Validates size, format, dimensions and decodability; returns the bytes and their digest.
    fn check_background(path: &Path) -> Result<(Arc<[u8]>, u64), String> {
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
        AssetCache::validate_image(&buf).map_err(|err| err.to_string())?;

        let mut hasher = DefaultHasher::new();
        buf.hash(&mut hasher);
        Ok((buf.into(), hasher.finish()))
    }

    /// Changes whenever the rendered result may change, so cached frames are not reused.
    pub fn fingerprint(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.background,
            self.background_digest,
            self.waiting_text,
            self.subtitle,
            self.text_size,
            self.connecting_text,
            self.pairing_text
        )
    }

    /// The same theme without its background, used when the image cannot be rendered.
    pub fn without_background(&self) -> Self {
        Self {
            background: None,
            background_bytes: None,
            background_digest: None,
            ..self.clone()
        }
    }
}
