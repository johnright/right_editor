//! Glyph atlas, one RGBA texture with shelf packing. Glyphs are stored as the
//! rasterizer gives them: alpha mask, per-channel ClearType coverage or color.

use std::collections::HashMap;

use cosmic_text::CacheKey;

use super::font::Fonts;
use super::raster::{Coverage, RasterGlyph, Rasterizer};

const SIZE: u32 = 2048;
const PADDING: u32 = 1;

#[derive(Debug, Clone, Copy)]
pub struct AtlasGlyph {
    pub uv: [u16; 2],
    pub size: [u16; 2],
    pub left: i16,
    pub top: i16,
    pub coverage: Coverage,
}

/// Caller has to clear the atlas and rebuild every row.
#[derive(Debug)]
pub struct AtlasFull;

pub struct Atlas {
    texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    // None for glyphs with no pixels, so we don't rasterize them again
    glyphs: HashMap<CacheKey, Option<AtlasGlyph>>,
    cursor_x: u32,
    cursor_y: u32,
    shelf_height: u32,
}

impl Atlas {
    pub fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            glyphs: HashMap::new(),
            cursor_x: 0,
            cursor_y: 0,
            shelf_height: 0,
        }
    }

    pub fn clear(&mut self) {
        self.glyphs.clear();
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.shelf_height = 0;
    }

    pub fn get(
        &mut self,
        key: CacheKey,
        fonts: &mut Fonts,
        raster: &mut Rasterizer,
        queue: &wgpu::Queue,
    ) -> Result<Option<AtlasGlyph>, AtlasFull> {
        if let Some(glyph) = self.glyphs.get(&key) {
            return Ok(*glyph);
        }
        let glyph = match raster.rasterize(fonts, key) {
            Some(image) => Some(self.upload(&image, queue)?),
            None => None,
        };
        self.glyphs.insert(key, glyph);
        Ok(glyph)
    }

    fn upload(
        &mut self,
        image: &RasterGlyph,
        queue: &wgpu::Queue,
    ) -> Result<AtlasGlyph, AtlasFull> {
        let (w, h) = (image.width, image.height);
        let (x, y) = self.allocate(w, h)?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &image.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        Ok(AtlasGlyph {
            uv: [x as u16, y as u16],
            size: [w as u16, h as u16],
            left: image.left as i16,
            top: image.top as i16,
            coverage: image.coverage,
        })
    }

    fn allocate(&mut self, w: u32, h: u32) -> Result<(u32, u32), AtlasFull> {
        if w + PADDING > SIZE || h + PADDING > SIZE {
            log::warn!("glyph too large for the atlas: {w}x{h}");
            return Err(AtlasFull);
        }
        if self.cursor_x + w + PADDING > SIZE {
            self.cursor_x = 0;
            self.cursor_y += self.shelf_height;
            self.shelf_height = 0;
        }
        if self.cursor_y + h + PADDING > SIZE {
            return Err(AtlasFull);
        }
        let pos = (self.cursor_x, self.cursor_y);
        self.cursor_x += w + PADDING;
        self.shelf_height = self.shelf_height.max(h + PADDING);
        Ok(pos)
    }
}
