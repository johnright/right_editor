//! Per-row instance building. y is relative to the row so the result survives
//! scrolling; the renderer caches it by row id.

use super::atlas::{Atlas, AtlasFull};
use super::font::{CellMetrics, FontStyle, Fonts};
use super::pipeline::{
    Instance, KIND_COLOR, KIND_DASHED, KIND_DOTTED, KIND_MASK, KIND_SOLID, KIND_SUBPIXEL,
    KIND_UNDERCURL, pack_rgb, pack_rgba,
};
use super::raster::{Coverage, Rasterizer};
use crate::editor::{CellText, HlTable, Row, TextInterner};

#[derive(Debug, Default, Clone)]
pub struct RowInstances {
    pub bg: Vec<Instance>,
    pub fg: Vec<Instance>,
    /// built with `paint_default_bg`
    pub default_bg: bool,
}

#[derive(Default)]
pub struct RowBuilder {
    seg_text: String,
    // (byte offset, column) per cell
    seg_map: Vec<(u32, u16)>,
}

pub struct RowContext<'a> {
    pub hl: &'a HlTable,
    pub text: &'a TextInterner,
    pub metrics: CellMetrics,
    pub queue: &'a wgpu::Queue,
    /// for grids drawn over other grids
    pub paint_default_bg: bool,
}

fn font_style(hl: &HlTable, id: u32) -> FontStyle {
    let attr = hl.attr(id);
    FontStyle {
        bold: attr.bold,
        italic: attr.italic,
    }
}

impl RowBuilder {
    pub fn build(
        &mut self,
        row: &Row,
        ctx: &RowContext<'_>,
        fonts: &mut Fonts,
        raster: &mut Rasterizer,
        atlas: &mut Atlas,
        out: &mut RowInstances,
    ) -> Result<(), AtlasFull> {
        out.bg.clear();
        out.fg.clear();
        out.default_bg = ctx.paint_default_bg;
        let cells = &row.cells;
        let m = ctx.metrics;
        let default_bg = ctx.hl.defaults().bg;

        // One quad per run of equal bg + blend. Grid 1 skips the default bg, the
        // clear already covers it. blend comes from winblend/pumblend.
        let bg_of = |hl: u32| (ctx.hl.colors(hl).bg, ctx.hl.attr(hl).blend);
        let mut col = 0;
        while col < cells.len() {
            let (bg, blend) = bg_of(cells[col].hl);
            let start = col;
            while col < cells.len() && bg_of(cells[col].hl) == (bg, blend) {
                col += 1;
            }
            if bg != default_bg || ctx.paint_default_bg {
                let alpha = (255 * (100 - blend.min(100) as u32) / 100) as u8;
                out.bg.push(Instance {
                    color: pack_rgba(bg, alpha),
                    ..Instance::solid(
                        start as f32 * m.width,
                        0.0,
                        (col - start) as f32 * m.width,
                        m.height,
                        bg,
                    )
                });
            }
        }

        let mut col = 0;
        while col < cells.len() {
            let first = cells[col];
            if first.text == CellText::SPACE || first.text == CellText::EMPTY {
                col += 1;
                continue;
            }
            let style = font_style(ctx.hl, first.hl);
            self.seg_text.clear();
            self.seg_map.clear();
            while col < cells.len() {
                let cell = cells[col];
                if cell.text == CellText::SPACE {
                    break;
                }
                // EMPTY is the right half of a wide char: no text, segment goes on
                if cell.text != CellText::EMPTY {
                    if font_style(ctx.hl, cell.hl) != style {
                        break;
                    }
                    self.seg_map.push((self.seg_text.len() as u32, col as u16));
                    ctx.text.push_to(cell.text, &mut self.seg_text);
                }
                col += 1;
            }

            let glyphs = fonts.shape(&self.seg_text, style);
            for g in glyphs.iter() {
                let i = self
                    .seg_map
                    .partition_point(|&(byte, _)| byte <= g.byte)
                    .saturating_sub(1);
                let gcol = self.seg_map[i].1;
                let Some(ag) = atlas.get(g.key, fonts, raster, ctx.queue)? else {
                    continue;
                };
                let fg = ctx.hl.colors(cells[gcol as usize].hl).fg;
                out.fg.push(Instance {
                    pos: [
                        gcol as f32 * m.width + (g.x + ag.left as i32) as f32,
                        m.baseline + (g.y - ag.top as i32) as f32,
                    ],
                    size: [ag.size[0] as f32, ag.size[1] as f32],
                    uv: [ag.uv[0] as f32, ag.uv[1] as f32],
                    color: pack_rgb(fg),
                    kind: match ag.coverage {
                        Coverage::Mask => KIND_MASK,
                        Coverage::Subpixel => KIND_SUBPIXEL,
                        Coverage::Color => KIND_COLOR,
                    },
                });
            }
        }

        let mut col = 0;
        while col < cells.len() {
            let id = cells[col].hl;
            let start = col;
            while col < cells.len() && cells[col].hl == id {
                col += 1;
            }
            let attr = ctx.hl.attr(id);
            let (x, w) = (start as f32 * m.width, (col - start) as f32 * m.width);
            // uv.x is the absolute x so waves/dashes line up across runs
            let mut deco = |kind: u32, y: f32, h: f32, rgb: u32| {
                out.fg.push(Instance {
                    pos: [x, y],
                    size: [w, h],
                    uv: [x, 0.0],
                    color: pack_rgb(rgb),
                    kind,
                })
            };
            let colors = ctx.hl.colors(id);
            if attr.underline {
                deco(KIND_SOLID, m.underline_y, m.stroke, colors.sp);
            }
            if attr.underdouble {
                deco(KIND_SOLID, m.underline_y, m.stroke, colors.sp);
                deco(
                    KIND_SOLID,
                    (m.underline_y - 2.0 * m.stroke).max(0.0),
                    m.stroke,
                    colors.sp,
                );
            }
            if attr.undercurl {
                let h = (4.0 * m.stroke).max(4.0);
                let y = (m.underline_y + m.stroke / 2.0 - h / 2.0).clamp(0.0, m.height - h);
                deco(KIND_UNDERCURL, y, h, colors.sp);
            }
            if attr.underdotted {
                deco(KIND_DOTTED, m.underline_y, m.stroke, colors.sp);
            }
            if attr.underdashed {
                deco(KIND_DASHED, m.underline_y, m.stroke, colors.sp);
            }
            if attr.strikethrough {
                deco(KIND_SOLID, m.strikeout_y, m.stroke, colors.fg);
            }
        }
        Ok(())
    }
}
