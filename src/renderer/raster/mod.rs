//! Glyph rasterization. On Windows, monochrome glyphs go through DirectWrite
//! (same as Zed and Windows Terminal) so we get the system's ClearType or
//! grayscale settings. Color emoji, and everything on Linux, use swash.

#[cfg(windows)]
mod dwrite;

use cosmic_text::{CacheKey, SwashContent};

use super::font::Fonts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    Mask,
    Subpixel,
    /// emoji etc., straight alpha
    Color,
}

pub struct RasterGlyph {
    pub width: u32,
    pub height: u32,
    /// offset from the glyph origin; `top` grows upward
    pub left: i32,
    pub top: i32,
    pub coverage: Coverage,
    pub rgba: Vec<u8>,
}

/// DirectWrite-style gamma and contrast, applied when blending in the shader.
#[derive(Debug, Clone, Copy)]
pub struct TextParams {
    pub gamma_ratios: [f32; 4],
    pub enhanced_contrast: f32,
    pub grayscale_enhanced_contrast: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Antialias {
    Subpixel,
    Grayscale,
}

pub struct Rasterizer {
    #[cfg(windows)]
    dwrite: Option<dwrite::DWrite>,
    #[cfg_attr(not(windows), allow(dead_code))]
    subpixel: bool,
    params: TextParams,
}

fn default_params() -> TextParams {
    TextParams {
        gamma_ratios: gamma_ratios(1.8),
        enhanced_contrast: 1.0,
        grayscale_enhanced_contrast: 1.0,
    }
}

impl Rasterizer {
    /// Subpixel AA also needs `dual_source` blending on the GPU.
    #[cfg(windows)]
    pub fn new(wanted: Antialias, dual_source: bool) -> Self {
        let (dwrite, params, subpixel) = match dwrite::DWrite::new() {
            Ok(dw) => {
                let s = dw.settings;
                log::info!(
                    "DirectWrite: gamma {:.2}, contrast {:.2}/{:.2}, ClearType {:.2}, geometry {}",
                    s.gamma,
                    s.enhanced_contrast,
                    s.grayscale_enhanced_contrast,
                    s.cleartype_level,
                    if s.flat {
                        "flat"
                    } else if s.bgr {
                        "BGR"
                    } else {
                        "RGB"
                    },
                );
                let params = TextParams {
                    gamma_ratios: gamma_ratios(s.gamma),
                    enhanced_contrast: s.enhanced_contrast,
                    grayscale_enhanced_contrast: s.grayscale_enhanced_contrast,
                };
                let subpixel = wanted == Antialias::Subpixel
                    && dual_source
                    && s.cleartype_level > 0.0
                    && !s.flat;
                (Some(dw), params, subpixel)
            }
            Err(err) => {
                log::warn!("DirectWrite unavailable, using swash: {err}");
                (None, default_params(), false)
            }
        };
        log::info!(
            "text antialiasing: {}",
            if subpixel {
                "subpixel (ClearType)"
            } else {
                "grayscale"
            }
        );
        Self {
            dwrite,
            subpixel,
            params,
        }
    }

    #[cfg(not(windows))]
    pub fn new(_wanted: Antialias, _dual_source: bool) -> Self {
        Self {
            subpixel: false,
            params: default_params(),
        }
    }

    pub fn params(&self) -> TextParams {
        self.params
    }

    // TODO: new glyphs get rasterized on the render thread. Probably what's
    // behind the odd isolated 20-46 ms frame.
    pub fn rasterize(&mut self, fonts: &mut Fonts, key: CacheKey) -> Option<RasterGlyph> {
        #[cfg(windows)]
        if let Some(dw) = &mut self.dwrite {
            match dw.rasterize(&fonts.system, key, self.subpixel) {
                Ok(Some(glyph)) => return glyph,
                // color font, or not backed by a file: leave it to swash
                Ok(None) => {}
                Err(err) => log::debug!(
                    "DirectWrite: glyph {} not rasterized: {err}",
                    key.glyph_id
                ),
            }
        }
        rasterize_swash(fonts, key)
    }
}

fn rasterize_swash(fonts: &mut Fonts, key: CacheKey) -> Option<RasterGlyph> {
    let image = fonts.swash.get_image_uncached(&mut fonts.system, key)?;
    let (width, height) = (image.placement.width, image.placement.height);
    if width == 0 || height == 0 {
        return None;
    }
    let (coverage, rgba) = match image.content {
        SwashContent::Mask => (
            Coverage::Mask,
            image
                .data
                .iter()
                .flat_map(|&a| [255, 255, 255, a])
                .collect(),
        ),
        SwashContent::SubpixelMask => (Coverage::Subpixel, image.data),
        SwashContent::Color => (Coverage::Color, image.data),
    };
    Some(RasterGlyph {
        width,
        height,
        left: image.placement.left,
        top: image.placement.top,
        coverage,
        rgba,
    })
}

/// Alpha correction table from Direct2D/DirectWrite, via Windows Terminal's
/// dwrite.hlsl (MIT).
pub fn gamma_ratios(gamma: f32) -> [f32; 4] {
    const RATIOS: [[f32; 4]; 13] = [
        [0.0000, 0.0000, 0.0000, 0.0000], // 1.0
        [0.0166, -0.0807, 0.2227, -0.0751],
        [0.0350, -0.1760, 0.4325, -0.1370],
        [0.0543, -0.2821, 0.6302, -0.1876],
        [0.0739, -0.3963, 0.8167, -0.2287],
        [0.0933, -0.5161, 0.9926, -0.2616],
        [0.1121, -0.6395, 1.1588, -0.2877],
        [0.1300, -0.7649, 1.3159, -0.3080],
        [0.1469, -0.8911, 1.4644, -0.3234], // 1.8
        [0.1627, -1.0170, 1.6051, -0.3347],
        [0.1773, -1.1420, 1.7385, -0.3426],
        [0.1908, -1.2652, 1.8650, -0.3476],
        [0.2031, -1.3864, 1.9851, -0.3501], // 2.2
    ];
    let index = ((gamma * 10.0).round() as i32).clamp(10, 22) - 10;
    RATIOS[index as usize].map(|r| r / 4.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_ratio_lookup_clamps() {
        assert_eq!(gamma_ratios(1.0), [0.0; 4]);
        assert_eq!(gamma_ratios(1.8)[0], 0.1469 / 4.0);
        assert_eq!(gamma_ratios(5.0), gamma_ratios(2.2));
        assert_eq!(gamma_ratios(0.5), gamma_ratios(1.0));
    }
}
