//! Font selection, cell metrics and cached shaping.
//!
//! Rows are shaped one segment at a time (non-blank cells, same style) so
//! ligatures work, but every glyph is pinned back to the column of its source
//! char so the grid never drifts.

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::fontdb::{Family, Query, Stretch, Style, Weight};
use cosmic_text::{
    Attrs, Buffer, CacheKey, CacheKeyFlags, FontSystem, Metrics, Shaping, SwashCache, Wrap,
};

const FALLBACK_FAMILIES: &[&str] = &["Cascadia Mono", "Consolas"];
const DEFAULT_SIZE_PT: f32 = 12.0;
const SHAPE_CACHE_LIMIT: usize = 50_000;

/// Physical pixels, always whole numbers so cells land on pixel boundaries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
    pub underline_y: f32,
    pub strikeout_y: f32,
    pub stroke: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FontStyle {
    pub bold: bool,
    pub italic: bool,
}

impl FontStyle {
    fn tag(self) -> char {
        char::from(b'0' + self.bold as u8 + 2 * self.italic as u8)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShapedGlyph {
    /// cluster's byte offset in the segment, maps the glyph back to its cell
    pub byte: u32,
    /// px from the left edge of the cluster's cell
    pub x: i32,
    pub y: i32,
    pub key: CacheKey,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontRequest {
    pub families: Vec<String>,
    pub size_pt: f32,
    pub linespace: f32,
    pub weight: u16,
    /// off = softer, fuller shapes closer to the design
    pub hinting: bool,
}

impl Default for FontRequest {
    fn default() -> Self {
        Self {
            families: Vec::new(),
            size_pt: DEFAULT_SIZE_PT,
            linespace: 0.0,
            weight: 400,
            hinting: false,
        }
    }
}

impl FontRequest {
    /// Neovide/gvim style `guifont`: `Fam\ One,Fam2:h13.5:b...`.
    pub fn parse_guifont(guifont: &str) -> GuiFont {
        let mut parts = guifont.split(':');
        let families = parts
            .next()
            .unwrap_or_default()
            .split(',')
            .map(|f| f.replace("\\ ", " ").replace('_', " ").trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
        let options: Vec<&str> = parts.collect();
        let size = options
            .iter()
            .filter_map(|p| p.strip_prefix('h'))
            .find_map(|h| h.parse::<f32>().ok())
            .filter(|s| *s > 0.0);
        // `:w500` is our own extension, Neovide ignores it
        let weight = options
            .iter()
            .filter_map(|p| p.strip_prefix('w'))
            .find_map(|w| w.parse::<u16>().ok())
            .filter(|w| (100..=900).contains(w));
        GuiFont {
            families,
            size,
            weight,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct GuiFont {
    pub families: Vec<String>,
    pub size: Option<f32>,
    pub weight: Option<u16>,
}

pub struct Fonts {
    pub system: FontSystem,
    pub swash: SwashCache,
    family: Option<String>,
    size_px: f32,
    weight: u16,
    hinting: bool,
    pub metrics: CellMetrics,
    buffer: Buffer,
    shape_cache: HashMap<Box<str>, Arc<[ShapedGlyph]>>,
    key_scratch: String,
}

impl Fonts {
    pub fn new(mut system: FontSystem, request: &FontRequest, scale: f64) -> Self {
        let buffer = Buffer::new(&mut system, Metrics::new(16.0, 16.0));
        let mut fonts = Self {
            system,
            swash: SwashCache::new(),
            family: None,
            size_px: 0.0,
            weight: 400,
            hinting: false,
            metrics: CellMetrics {
                width: 1.0,
                height: 1.0,
                baseline: 1.0,
                underline_y: 1.0,
                strikeout_y: 1.0,
                stroke: 1.0,
            },
            buffer,
            shape_cache: HashMap::new(),
            key_scratch: String::new(),
        };
        fonts.set(request, scale);
        fonts
    }

    pub fn size_px(&self) -> f32 {
        self.size_px
    }

    pub fn family_name(&self) -> Option<&str> {
        self.family.as_deref()
    }

    /// Caller has to clear the atlas and the row caches afterwards.
    pub fn set(&mut self, request: &FontRequest, scale: f64) {
        self.family = self.resolve_family(&request.families);
        self.weight = request.weight.clamp(100, 900);
        self.hinting = request.hinting;
        // pt -> px, 96 DPI at scale 1
        self.size_px = (request.size_pt * 96.0 / 72.0 * scale as f32).max(1.0);
        self.metrics = self.compute_metrics(request.linespace * scale as f32);
        self.buffer
            .set_metrics(Metrics::new(self.size_px, self.metrics.height));
        self.buffer.set_wrap(Wrap::None);
        self.buffer.set_size(None, None);
        self.shape_cache.clear();
        log::info!(
            "font: {} {:.1}px, cell {}x{}",
            self.family.as_deref().unwrap_or("<system monospace>"),
            self.size_px,
            self.metrics.width,
            self.metrics.height
        );
    }

    fn resolve_family(&self, requested: &[String]) -> Option<String> {
        let db = self.system.db();
        let wanted = requested
            .iter()
            .map(String::as_str)
            .chain(FALLBACK_FAMILIES.iter().copied());
        for name in wanted {
            let found = db.faces().find_map(|face| {
                face.families
                    .iter()
                    .find(|(fam, _)| fam.eq_ignore_ascii_case(name))
                    .map(|(fam, _)| fam.clone())
            });
            match found {
                Some(fam) => return Some(fam),
                None if requested.iter().any(|r| r == name) => {
                    log::warn!("font not found: {name}")
                }
                None => {}
            }
        }
        None
    }

    fn family(&self) -> Family<'_> {
        self.family
            .as_deref()
            .map_or(Family::Monospace, Family::Name)
    }

    fn compute_metrics(&mut self, linespace: f32) -> CellMetrics {
        let query = Query {
            families: &[self.family()],
            weight: Weight(self.weight),
            stretch: Stretch::Normal,
            style: Style::Normal,
        };
        let font = self
            .system
            .db()
            .query(&query)
            .and_then(|id| self.system.get_font(id, Weight(self.weight)));
        let Some(font) = font else {
            log::error!("no monospace font available");
            let s = self.size_px;
            return CellMetrics {
                width: (s * 0.6).round(),
                height: (s * 1.2).round(),
                baseline: s.round(),
                underline_y: s.round() + 1.0,
                strikeout_y: (s * 0.6).round(),
                stroke: 1.0,
            };
        };
        let swash = font.as_swash();
        let m = swash.metrics(&[]).scale(self.size_px);
        let advance = swash
            .glyph_metrics(&[])
            .scale(self.size_px)
            .advance_width(swash.charmap().map('M'));
        let linespace = linespace.round();
        let height = (m.ascent + m.descent + m.leading).round() + linespace;
        let baseline = (m.ascent + (linespace / 2.0).floor()).round();
        let stroke = m.stroke_size.round().max(1.0);
        CellMetrics {
            width: advance.round().max(1.0),
            height: height.max(1.0),
            baseline,
            // swash offsets point up
            underline_y: (baseline - m.underline_offset).round().min(height - stroke),
            strikeout_y: (baseline - m.strikeout_offset).round(),
            stroke,
        }
    }

    pub fn shape(&mut self, text: &str, style: FontStyle) -> Arc<[ShapedGlyph]> {
        self.key_scratch.clear();
        self.key_scratch.push(style.tag());
        self.key_scratch.push_str(text);
        if let Some(glyphs) = self.shape_cache.get(self.key_scratch.as_str()) {
            return glyphs.clone();
        }

        let attrs = Attrs::new()
            .family(
                self.family
                    .as_deref()
                    .map_or(Family::Monospace, Family::Name),
            )
            .weight(if style.bold {
                Weight(self.weight.saturating_add(300).clamp(700, 900))
            } else {
                Weight(self.weight)
            })
            // ends up in each glyph's CacheKey, which is how the rasterizer sees it
            .cache_key_flags(if self.hinting {
                CacheKeyFlags::empty()
            } else {
                CacheKeyFlags::DISABLE_HINTING
            })
            .style(if style.italic {
                Style::Italic
            } else {
                Style::Normal
            });
        self.buffer.set_text(text, &attrs, Shaping::Advanced, None);
        self.buffer.shape_until_scroll(&mut self.system, false);

        let mut glyphs = Vec::with_capacity(text.len());
        for run in self.buffer.layout_runs() {
            // later glyphs of the same cluster (combining marks etc.) are placed
            // relative to the cluster's first glyph
            let mut cluster: Option<(usize, f32)> = None;
            for g in run.glyphs {
                let origin = match cluster {
                    Some((start, x)) if start == g.start => x,
                    _ => {
                        cluster = Some((g.start, g.x));
                        g.x
                    }
                };
                let phys = g.physical((-origin, 0.0), 1.0);
                glyphs.push(ShapedGlyph {
                    byte: g.start as u32,
                    x: phys.x,
                    y: phys.y,
                    key: phys.cache_key,
                });
            }
        }

        if self.shape_cache.len() >= SHAPE_CACHE_LIMIT {
            self.shape_cache.clear();
        }
        let glyphs: Arc<[ShapedGlyph]> = glyphs.into();
        self.shape_cache
            .insert(self.key_scratch.as_str().into(), glyphs.clone());
        glyphs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_guifont() {
        let g = FontRequest::parse_guifont(
            r"CaskaydiaCove\ Nerd\ Font:h13.5:#e-subpixelantialias:#h-none",
        );
        assert_eq!(g.families, ["CaskaydiaCove Nerd Font"]);
        assert_eq!((g.size, g.weight), (Some(13.5), None));
        let g = FontRequest::parse_guifont("JetBrains_Mono,Consolas:b:w500");
        assert_eq!(g.families, ["JetBrains Mono", "Consolas"]);
        assert_eq!((g.size, g.weight), (None, Some(500)));
        assert_eq!(FontRequest::parse_guifont(""), GuiFont::default());
    }
}
