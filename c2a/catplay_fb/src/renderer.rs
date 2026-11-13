use core::fmt;
use std::{path::PathBuf, time::Instant};

use ab_glyph::InvalidFont;
use image::ImageError;
use log::{debug, warn};

use crate::{Anchor, AssetCache, AssetCacheError, Canvas, DirtyRect, Surface, TextAlign, UiRect, resolve_rect};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetType {
    Image,
    Font,
}

#[derive(Clone, PartialEq)]
pub enum RenderOp {
    SetBackground {
        rgba: u32,
    },

    LoadAsset {
        id: String,
        path: PathBuf,
        asset_type: AssetType,
    },
    ReleaseAsset {
        id: String,
    },
    ReleaseFont {
        id: String,
    },

    LoadAssetConst {
        id: &'static str,
        buf: &'static [u8],
        asset_type: AssetType,
    },

    BlitAsset {
        id: String,
        rect: UiRect,
    },

    /// Fills the whole frame with an image, keeping its aspect ratio and cropping the overflow evenly.
    BlitAssetCover {
        id: String,
    },

    SetTextColor {
        argb: u32,
    },

    DrawText {
        text: String,
        rect: UiRect,

        align: TextAlign,
        font_id: String,
        font_size_pt: f32,
        color: u32, // 0xRRGGBBAA
    },
}

impl fmt::Debug for RenderOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SetBackground { rgba } => f.debug_struct("SetBackground").field("rgba", rgba).finish(),
            Self::LoadAsset { id, path, asset_type } => f
                .debug_struct("LoadAsset")
                .field("id", id)
                .field("path", path)
                .field("asset_type", asset_type)
                .finish(),
            Self::LoadAssetConst { id, asset_type, .. } => f
                .debug_struct("LoadAssetConst")
                .field("id", id)
                .field("asset_type", asset_type)
                .finish(),
            Self::BlitAsset { id, rect } => f
                .debug_struct("BlitAsset")
                .field("id", id)
                .field("rect", rect)
                .finish(),
            Self::BlitAssetCover { id } => f.debug_struct("BlitAssetCover").field("id", id).finish(),
            Self::SetTextColor { argb } => f.debug_struct("SetTextColor").field("argb", argb).finish(),
            Self::DrawText {
                text,
                rect,
                align,
                font_id,
                font_size_pt,
                color,
            } => f
                .debug_struct("DrawText")
                .field("text", text)
                .field("rect", rect)
                .field("align", align)
                .field("font_id", font_id)
                .field("font_size_pt", font_size_pt)
                .field("color", color)
                .finish(),
            Self::ReleaseAsset { id } => f.debug_struct("ReleaseAsset").field("id", id).finish(),
            Self::ReleaseFont { id } => f.debug_struct("ReleaseFont").field("id", id).finish(),
        }
    }
}
pub struct Renderer {
    fb: Surface,
    dpi: f32,
    assets: AssetCache,

    current_text_rgb: u32,
}

#[derive(thiserror::Error, Debug)]
pub enum RenderError {
    #[error("Invalid font: {0}")]
    InvalidFont(#[from] InvalidFont),
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unknown font: {0}")]
    UnknownFont(String),
    #[error("Unknown asset: {0}")]
    UnknownAsset(String),
    #[error("Assets caching error: {0}")]
    AssetsCache(#[from] AssetCacheError),
    #[error("No font selected")]
    NoFontSelected,

    #[error("Failed to export image: {0}")]
    ImageExportError(ImageError),
}

impl Renderer {
    pub fn new(width: i32, height: i32, dpi: f32) -> Self {
        Self {
            fb: Surface::new(width, height),
            dpi,
            assets: AssetCache::new(),
            current_text_rgb: 0,
        }
    }

    pub fn reset(&mut self, width: i32, height: i32, dpi: f32) {
        self.dpi = dpi;
        self.fb = Surface::new(width, height);
    }

    pub fn export_to_disk(&mut self, path: PathBuf) -> Result<(), RenderError> {
        self.fb
            .save_surface(path.to_str().unwrap())
            .map_err(RenderError::ImageExportError)
    }

    pub fn execute(&mut self, ops: &[RenderOp]) -> Result<(), RenderError> {
        for op in ops {
            let start = Instant::now();
            match op {
                RenderOp::SetBackground { rgba } => {
                    Canvas::fill(&mut self.fb, *rgba);

                    let dirty = DirtyRect {
                        x: 0,
                        y: 0,
                        w: self.fb.width as _,
                        h: self.fb.height as _,
                    };
                    debug!("Background dirty: {dirty:?}");
                    self.fb.add_dirty(dirty);
                }
                RenderOp::LoadAsset { id, path, asset_type } => match asset_type {
                    AssetType::Image => {
                        self.assets.load(id.clone(), path)?;
                    }
                    AssetType::Font => {
                        self.assets.load_font(id.clone(), std::fs::read(path)?)?;
                    }
                },
                RenderOp::LoadAssetConst { id, buf, asset_type } => match asset_type {
                    AssetType::Image => {
                        self.assets.load_buf((*id).into(), buf)?;
                    }
                    AssetType::Font => {
                        self.assets.load_font((*id).into(), (*buf).into())?;
                    }
                },
                RenderOp::BlitAsset { id, rect } => {
                    let rect = resolve_rect(*rect, self.fb.width, self.fb.height, self.dpi);
                    let asset = self.assets.get_scaled(id.clone(), rect.w, rect.h)?;
                    Canvas::blit(&mut self.fb, asset, rect.x, rect.y, rect.w, rect.h);

                    debug!("Blit asset dirty: {rect:?}");
                    self.fb.add_dirty(rect);
                }
                RenderOp::BlitAssetCover { id } => {
                    let (fw, fh) = (self.fb.width.max(0) as u32, self.fb.height.max(0) as u32);
                    let asset = self.assets.get_cover(id.clone(), fw, fh)?;
                    Canvas::blit(&mut self.fb, asset, 0, 0, fw, fh);

                    let rect = DirtyRect {
                        x: 0,
                        y: 0,
                        w: fw as _,
                        h: fh as _,
                    };
                    debug!("Blit asset cover dirty: {rect:?}");
                    self.fb.add_dirty(rect);
                }
                RenderOp::SetTextColor { argb } => {
                    self.current_text_rgb = *argb;
                    self.fb.set_pen_color(*argb);
                }
                RenderOp::DrawText {
                    text,
                    rect,
                    align,
                    font_id,
                    font_size_pt,
                    color,
                } => {
                    let r = resolve_rect(*rect, self.fb.width, self.fb.height, self.dpi);
                    let (rx, ry, rw, rh) = (r.x, r.y, r.w, r.h);
                    let px = font_size_pt * self.dpi / 72.0;

                    let font_renderer = self
                        .assets
                        .load_font_scale((*font_id).clone(), px.round() as _)?;

                    self.fb.set_pen_color(*color);

                    let (mut pen_x, baseline_y) = match rect.anchor {
                        Anchor::BaselineLeft => (rx as f32, ry as f32),
                        Anchor::BaselineCenter => (rx as f32 + rw as f32 / 2.0, ry as f32),
                        Anchor::BaselineRight => (rx as f32 + rw as f32, ry as f32),
                        _ => (rx as f32 + rw as f32 / 2.0, ry as f32 + rh as f32 / 2.0),
                    };

                    match align {
                        TextAlign::Left => {}
                        TextAlign::Center => {
                            let tw = font_renderer.measure_text_width(text);
                            pen_x -= tw / 2.0;
                        }
                        TextAlign::Right => {
                            let tw = font_renderer.measure_text_width(text);
                            pen_x -= tw;
                        }
                    }

                    let dirty = font_renderer.draw_text(&mut self.fb, pen_x, baseline_y, text);
                    if let Some(dirty) = dirty {
                        debug!("Text dirty: {dirty:?}");
                        self.fb.add_dirty(dirty);
                    }
                }
                RenderOp::ReleaseAsset { id } => self.assets.release_img(id),
                RenderOp::ReleaseFont { id } => self.assets.release_font(id),
            }

            warn!("Finished OP {op:?} in {:?}", Instant::now() - start);
        }

        Ok(())
    }

    pub fn framebuffer(&mut self) -> &mut Surface {
        &mut self.fb
    }
}

#[cfg(test)]
mod cover_tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    /// Encodes a 40x10 (or 10x40) PNG whose first half is `a` and second half `b`.
    fn two_tone(horizontal: bool, a: [u8; 4], b: [u8; 4]) -> &'static [u8] {
        let (w, h) = if horizontal { (40, 10) } else { (10, 40) };
        let img = RgbaImage::from_fn(w, h, |x, y| {
            let first = if horizontal { x < w / 2 } else { y < h / 2 };
            Rgba(if first { a } else { b })
        });
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, ImageFormat::Png).unwrap();
        Box::leak(png.into_inner().into_boxed_slice())
    }

    fn render(width: i32, height: i32, buf: &'static [u8]) -> Renderer {
        let mut r = Renderer::new(width, height, 160.0);
        r.execute(&[
            RenderOp::LoadAssetConst {
                id: "bg",
                buf,
                asset_type: AssetType::Image,
            },
            RenderOp::BlitAssetCover { id: "bg".into() },
        ])
        .unwrap();
        r
    }

    fn px(r: &Renderer, x: i32, y: i32) -> u32 {
        r.fb.rgba_u32()[(y * r.fb.width + x) as usize]
    }

    #[test]
    fn cover_fills_frame_and_crops_evenly() {
        const RED: [u8; 4] = [255, 0, 0, 255];
        const BLUE: [u8; 4] = [0, 0, 255, 255];

        // Wide image in a square frame: scaled to the frame height, left/right halves visible.
        let r = render(10, 10, two_tone(true, RED, BLUE));
        assert_ne!(px(&r, 1, 5), px(&r, 8, 5));
        assert_eq!(px(&r, 0, 0), px(&r, 1, 9));
        assert_eq!(px(&r, 9, 0), px(&r, 8, 9));

        // Tall image in a wide frame: scaled to the frame width, top/bottom halves visible.
        let r = render(10, 4, two_tone(false, RED, BLUE));
        assert_ne!(px(&r, 5, 0), px(&r, 5, 3));
        assert_eq!(px(&r, 0, 0), px(&r, 9, 1));
    }
}
