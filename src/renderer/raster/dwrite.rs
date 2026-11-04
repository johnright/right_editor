//! DirectWrite rasterizer. We open the same font file cosmic-text shaped
//! with, so glyph ids line up and DirectWrite only has to draw them.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;

use cosmic_text::fontdb::{ID, Source};
use cosmic_text::{CacheKey, CacheKeyFlags, FontSystem};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::{BOOL, Interface, PCWSTR, Result};

use super::{Coverage, RasterGlyph};

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub gamma: f32,
    pub enhanced_contrast: f32,
    pub grayscale_enhanced_contrast: f32,
    pub cleartype_level: f32,
    pub bgr: bool,
    pub flat: bool,
}

struct Face {
    face: IDWriteFontFace,
    color: bool,
}

pub struct DWrite {
    factory: IDWriteFactory2,
    pub settings: Settings,
    // keyed by (font, simulations); None = leave it to swash
    faces: HashMap<(ID, i32), Option<Face>>,
    buf: Vec<u8>,
}

impl DWrite {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: IDWriteFactory2 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            // the defaults reflect whatever the user picked in the ClearType
            // Text Tuner
            let params = factory.CreateRenderingParams()?;
            let grayscale_enhanced_contrast = params
                .cast::<IDWriteRenderingParams1>()
                .map(|p| p.GetGrayscaleEnhancedContrast())
                .unwrap_or(1.0);
            let geometry = params.GetPixelGeometry();
            let settings = Settings {
                gamma: params.GetGamma(),
                enhanced_contrast: params.GetEnhancedContrast(),
                grayscale_enhanced_contrast,
                cleartype_level: params.GetClearTypeLevel(),
                bgr: geometry == DWRITE_PIXEL_GEOMETRY_BGR,
                flat: geometry == DWRITE_PIXEL_GEOMETRY_FLAT,
            };
            Ok(Self {
                factory,
                settings,
                faces: HashMap::new(),
                buf: Vec::new(),
            })
        }
    }

    /// `Ok(None)` means swash should handle it (color font, or not file-based).
    /// `Ok(Some(None))` is a glyph with no pixels.
    pub fn rasterize(
        &mut self,
        system: &FontSystem,
        key: CacheKey,
        subpixel: bool,
    ) -> Result<Option<Option<RasterGlyph>>> {
        let simulations = simulations(system, key);
        let face = match self.faces.get(&(key.font_id, simulations.0)) {
            Some(face) => face,
            None => {
                let face = self
                    .load_face(system, key.font_id, simulations)
                    .unwrap_or_else(|err| {
                        log::warn!("DirectWrite: font {:?} cannot be loaded: {err}", key.font_id);
                        None
                    });
                self.faces
                    .entry((key.font_id, simulations.0))
                    .or_insert(face)
            }
        };
        let Some(face) = face else { return Ok(None) };
        if face.color {
            return Ok(None);
        }

        let glyph_id = key.glyph_id;
        let advance = 0.0f32;
        let offset = DWRITE_GLYPH_OFFSET::default();
        let run = DWRITE_GLYPH_RUN {
            fontFace: ManuallyDrop::new(Some(face.face.clone())),
            fontEmSize: f32::from_bits(key.font_size_bits),
            glyphCount: 1,
            glyphIndices: &glyph_id,
            glyphAdvances: &advance,
            glyphOffsets: &offset,
            isSideways: BOOL(0),
            bidiLevel: 0,
        };
        let (antialias, texture) = if subpixel {
            (
                DWRITE_TEXT_ANTIALIAS_MODE_CLEARTYPE,
                DWRITE_TEXTURE_CLEARTYPE_3x1,
            )
        } else {
            (
                DWRITE_TEXT_ANTIALIAS_MODE_GRAYSCALE,
                DWRITE_TEXTURE_ALIASED_1x1,
            )
        };
        let analysis = unsafe {
            self.factory.CreateGlyphRunAnalysis(
                &run,
                None,
                // symmetric = AA vertically as well. Grid fit follows the hinting option.
                DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC,
                DWRITE_MEASURING_MODE_NATURAL,
                if key.flags.contains(CacheKeyFlags::DISABLE_HINTING) {
                    DWRITE_GRID_FIT_MODE_DISABLED
                } else {
                    DWRITE_GRID_FIT_MODE_ENABLED
                },
                antialias,
                key.x_bin.as_float(),
                key.y_bin.as_float(),
            )
        };
        drop(ManuallyDrop::into_inner(run.fontFace));
        let analysis = analysis?;

        let rect = unsafe { analysis.GetAlphaTextureBounds(texture)? };
        let (width, height) = (
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        );
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return Ok(Some(None));
        }
        let channels = if subpixel { 3 } else { 1 };
        self.buf.clear();
        self.buf.resize((width * height * channels) as usize, 0);
        unsafe { analysis.CreateAlphaTexture(texture, &rect, &mut self.buf)? };

        let rgba = if subpixel {
            let bgr = self.settings.bgr;
            self.buf
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|&[a, b, c]| {
                    let [r, g, b] = if bgr { [c, b, a] } else { [a, b, c] };
                    [r, g, b, r.max(g).max(b)]
                })
                .collect()
        } else {
            self.buf.iter().flat_map(|&a| [255, 255, 255, a]).collect()
        };
        Ok(Some(Some(RasterGlyph {
            width,
            height,
            left: rect.left,
            // DirectWrite y goes down from the baseline
            top: -rect.top,
            coverage: if subpixel {
                Coverage::Subpixel
            } else {
                Coverage::Mask
            },
            rgba,
        })))
    }

    fn load_face(
        &self,
        system: &FontSystem,
        id: ID,
        simulations: DWRITE_FONT_SIMULATIONS,
    ) -> Result<Option<Face>> {
        let Some((source, index)) = system.db().face_source(id) else {
            return Ok(None);
        };
        let path = match source {
            Source::File(path) | Source::SharedFile(path, _) => path,
            Source::Binary(_) => return Ok(None),
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let file = self
                .factory
                .CreateFontFileReference(PCWSTR(wide.as_ptr()), None)?;
            let mut supported = BOOL(0);
            let mut file_type = DWRITE_FONT_FILE_TYPE::default();
            let mut face_type = DWRITE_FONT_FACE_TYPE::default();
            let mut faces = 0;
            file.Analyze(
                &mut supported,
                &mut file_type,
                Some(&mut face_type),
                &mut faces,
            )?;
            if !supported.as_bool() {
                return Ok(None);
            }
            let face = self
                .factory
                .CreateFontFace(face_type, &[Some(file)], index, simulations)?;
            let color = face
                .cast::<IDWriteFontFace2>()
                .is_ok_and(|f| f.IsColorFont().as_bool());
            Ok(Some(Face { face, color }))
        }
    }
}

// fake bold/italic when cosmic-text fell back to a face without that variant
fn simulations(system: &FontSystem, key: CacheKey) -> DWRITE_FONT_SIMULATIONS {
    let mut sim = DWRITE_FONT_SIMULATIONS_NONE.0;
    if key.flags.contains(CacheKeyFlags::FAKE_ITALIC) {
        sim |= DWRITE_FONT_SIMULATIONS_OBLIQUE.0;
    }
    let face_weight = system.db().face(key.font_id).map_or(400, |f| f.weight.0);
    if key.font_weight.0 >= 600 && face_weight < 600 {
        sim |= DWRITE_FONT_SIMULATIONS_BOLD.0;
    }
    DWRITE_FONT_SIMULATIONS(sim)
}
