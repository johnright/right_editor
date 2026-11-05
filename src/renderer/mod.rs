mod animation;
mod atlas;
mod cells;
pub mod font;
mod gpu;
mod image_pipeline;
mod pipeline;
pub mod raster;

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Result;
use cosmic_text::FontSystem;
use winit::window::Window;

use crate::config::Config;
use crate::editor::{CellText, EditorState, GridChange, Layer, MAIN_GRID};
use crate::latency::FrameTimings;
use crate::nvim::redraw::CursorShape;
use crate::nvim::redraw::GridId;
use crate::preview::{ImageData, Preview};
use animation::{CursorAnimation, Region, Scrolling};
use atlas::{Atlas, AtlasFull};
use cells::{RowBuilder, RowContext, RowInstances};
use font::{FontRequest, Fonts};
use gpu::{Frame, Gpu};
use image_pipeline::{ImagePipeline, ImageQuad, ImageTexture};
use pipeline::{
    CellPipeline, CursorPipeline, Instance, KIND_SHADOW, KIND_SOLID, Segment, pack_rgb, pack_rgba,
};

/// grid id + on-screen origin, in cells
type Placed = (GridId, i64, i64);
use raster::Rasterizer;

pub struct Renderer {
    gpu: Gpu,
    fonts: Fonts,
    font_request: FontRequest,
    scale: f64,
    raster: Rasterizer,
    atlas: Atlas,
    pipeline: CellPipeline,
    builder: RowBuilder,
    /// row id -> (last frame used, instances); the Arc is shared with scroll snapshots
    rows: HashMap<u64, (u64, Arc<RowInstances>)>,
    frame: u64,
    instances: Vec<Instance>,
    segments: Vec<Segment>,
    background: u32,
    /// of the default background only, < 1 means a translucent window
    opacity: f32,
    cursor: CursorAnimation,
    cursor_duration: Duration,
    cursor_trail: f32,
    // as of the previous frame
    cursor_in_terminal: bool,
    cursor_pipeline: CursorPipeline,
    cursor_quad: Option<([[f32; 2]; 4], u32)>,
    /// text under a block cursor, redrawn on top of it
    cursor_text: std::ops::Range<u32>,
    scrolling: HashMap<GridId, Scrolling>,
    scroll_duration: Duration,
    /// want another frame even if nvim doesn't flush
    animating: bool,
    floating_shadow: bool,
    /// dimming behind a focused float, 0 = off
    backdrop: f32,
    backdrop_since: Option<Instant>,
    last_present: Option<Instant>,
    text_fade: Duration,
    fades: Vec<TextFade>,
    text_slide: Duration,
    slides: Vec<TextSlide>,
    /// fraction of the window, 0 = closed
    panel_fraction: f32,
    /// bumped on every atlas clear, invalidates cached glyph instances
    atlas_epoch: u64,
    preview: PreviewPaint,
    images: ImagePipeline,
}

/// Preview panel instances, reused until the doc, scroll or atlas change.
#[derive(Default)]
struct PreviewPaint {
    key: Option<(usize, i64, [u32; 4], u64)>,
    scroll: f32,
    tick: Option<Instant>,
    animating: bool,
    rects: Vec<Instance>,
    glyphs: Vec<Instance>,
    quads: Vec<(String, ImageQuad)>,
    textures: HashMap<String, (Arc<ImageData>, ImageTexture)>,
    rect_range: std::ops::Range<u32>,
    glyph_range: std::ops::Range<u32>,
    panel: Option<[u32; 4]>,
}

/// Cells [start, end) that got pushed by `from` cells at `since` (positive =
/// they started further left) and are sliding back into place.
#[derive(Debug, Clone, Copy)]
struct TextSlide {
    row_id: u64,
    start: usize,
    end: usize,
    from: f32,
    since: Instant,
}

#[derive(Debug, Clone, Copy)]
struct TextFade {
    row_id: u64,
    start: usize,
    end: usize,
    since: Instant,
}

impl Renderer {
    pub fn new(
        window: Arc<Window>,
        config: &Config,
        font_system: JoinHandle<FontSystem>,
        font_request: FontRequest,
    ) -> Result<Self> {
        let scale = window.scale_factor();
        // bring up the GPU while the font thread is still loading
        let transparent = config.opacity < 1.0;
        let gpu = Gpu::new(window, config.present_mode, transparent)?;
        let font_system = font_system
            .join()
            .map_err(|_| anyhow::anyhow!("loading the fonts failed"))?;
        let fonts = Fonts::new(font_system, &font_request, scale);
        // ClearType needs to know the color under the text
        let antialias = if transparent {
            raster::Antialias::Grayscale
        } else {
            config.antialias
        };
        let raster = Rasterizer::new(antialias, gpu.dual_source);
        let atlas = Atlas::new(&gpu.device);
        let pipeline = CellPipeline::new(
            &gpu.device,
            gpu.surface_config.format,
            &atlas.view,
            gpu.dual_source,
            raster.params(),
        );
        let cursor_pipeline = CursorPipeline::new(&gpu.device, gpu.surface_config.format);
        let images = ImagePipeline::new(&gpu.device, gpu.surface_config.format);
        let [r, g, b] = config.background;
        Ok(Self {
            gpu,
            fonts,
            font_request,
            scale,
            raster,
            atlas,
            pipeline,
            builder: RowBuilder::default(),
            rows: HashMap::new(),
            frame: 0,
            instances: Vec::new(),
            segments: Vec::new(),
            background: (r as u32) << 16 | (g as u32) << 8 | b as u32,
            opacity: config.opacity,
            cursor: CursorAnimation::new(),
            cursor_duration: Duration::from_millis(config.cursor_animation_ms),
            cursor_trail: config.cursor_trail,
            cursor_in_terminal: false,
            cursor_pipeline,
            cursor_quad: None,
            cursor_text: 0..0,
            scrolling: HashMap::new(),
            scroll_duration: Duration::from_millis(config.scroll_animation_ms),
            animating: false,
            floating_shadow: config.floating_shadow,
            backdrop: config.backdrop,
            backdrop_since: None,
            last_present: None,
            text_fade: Duration::from_millis(config.text_fade_ms),
            fades: Vec::new(),
            text_slide: Duration::from_millis(config.text_slide_ms),
            slides: Vec::new(),
            panel_fraction: 0.0,
            atlas_epoch: 0,
            preview: PreviewPaint::default(),
            images,
        })
    }

    // scales with the row height, so it follows zoom
    fn shadow_blur(&self) -> f32 {
        (self.fonts.metrics.height * 0.8).round()
    }

    pub fn last_present(&self) -> Option<Instant> {
        self.last_present
    }

    pub fn animating(&self) -> bool {
        self.animating
    }

    pub fn cell_size(&self) -> (f32, f32) {
        (self.fonts.metrics.width, self.fonts.metrics.height)
    }

    pub fn surface_size(&self) -> (u32, u32) {
        (
            self.gpu.surface_config.width,
            self.gpu.surface_config.height,
        )
    }

    /// Cols/rows that fit next to the preview panel.
    pub fn grid_size(&self) -> (u64, u64) {
        let m = self.fonts.metrics;
        let width = self.gpu.surface_config.width as f32 * (1.0 - self.panel_fraction);
        let cols = (width / m.width).floor().max(1.0);
        let rows = (self.gpu.surface_config.height as f32 / m.height)
            .floor()
            .max(1.0);
        (cols as u64, rows as u64)
    }

    pub fn set_panel(&mut self, fraction: f32) {
        if fraction != self.panel_fraction {
            self.panel_fraction = fraction;
            self.preview.key = None;
            self.preview.tick = None;
        }
    }

    /// From the last grid column to the right edge of the window.
    pub fn panel_rect(&self) -> Option<[f32; 4]> {
        if self.panel_fraction <= 0.0 {
            return None;
        }
        let (cols, _) = self.grid_size();
        let x = cols as f32 * self.fonts.metrics.width;
        let (w, h) = self.surface_size();
        Some([x, 0.0, (w as f32 - x).max(0.0), h as f32])
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn text_size_px(&self) -> f32 {
        self.fonts.size_px()
    }

    pub fn mono_family(&self) -> Option<String> {
        self.fonts.family_name().map(str::to_string)
    }

    pub fn font_db(&self) -> (String, cosmic_text::fontdb::Database) {
        (
            self.fonts.system.locale().to_string(),
            self.fonts.system.db().clone(),
        )
    }

    pub fn preview_scroll(&self) -> f32 {
        self.preview.scroll
    }

    /// true if the font changed, which also means a new grid size
    pub fn set_font(&mut self, request: FontRequest) -> bool {
        if request == self.font_request {
            return false;
        }
        self.font_request = request;
        self.reload_font();
        true
    }

    pub fn set_scale(&mut self, scale: f64) {
        if scale != self.scale {
            self.scale = scale;
            self.reload_font();
        }
    }

    fn reload_font(&mut self) {
        self.fonts.set(&self.font_request, self.scale);
        self.clear_atlas();
        self.scrolling.clear();
    }

    fn clear_atlas(&mut self) {
        self.atlas.clear();
        self.rows.clear();
        self.atlas_epoch += 1;
        self.preview.key = None;
    }

    /// After `:restart`, running animations point at grids/rows that are gone.
    pub fn reset_animations(&mut self) {
        self.scrolling.clear();
        self.fades.clear();
        self.slides.clear();
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.gpu.resize(width, height);
        self.scrolling.clear();
    }

    fn process_changes(&mut self, state: &mut EditorState, now: Instant) {
        for change in state.changes.drain(..) {
            match change {
                GridChange::Scroll {
                    grid,
                    top,
                    bot,
                    left,
                    right,
                    rows,
                    animate,
                } => {
                    let region = Region {
                        top,
                        bot,
                        left,
                        right,
                    };
                    // zero duration still rotates the snapshot, just no animation
                    let duration = if animate {
                        self.scroll_duration
                    } else {
                        Duration::ZERO
                    };
                    self.scrolling
                        .entry(grid)
                        .or_default()
                        .scroll(region, rows, duration, now);
                }
                GridChange::Reset { grid } => {
                    self.scrolling.remove(&grid);
                }
                GridChange::Slide {
                    row_id,
                    start,
                    end,
                    shift,
                } => {
                    if self.text_slide.is_zero() {
                        continue;
                    }
                    // already sliding: continue from where the text is on screen now
                    let mut from = shift as f32;
                    let duration = self.text_slide;
                    if let Some(i) = self.slides.iter().position(|s| s.row_id == row_id) {
                        let old = self.slides.swap_remove(i);
                        from += slide_offset(
                            old.from,
                            now.saturating_duration_since(old.since),
                            duration,
                        );
                    }
                    self.slides.push(TextSlide {
                        row_id,
                        start,
                        end,
                        from,
                        since: now,
                    });
                }
                GridChange::Typed { row_id, start, end } => {
                    if !self.text_fade.is_zero() {
                        self.fades.push(TextFade {
                            row_id,
                            start,
                            end,
                            since: now,
                        });
                    }
                }
            }
        }
    }

    /// None if the frame was skipped.
    pub fn render(
        &mut self,
        state: &mut EditorState,
        preview: &Preview,
    ) -> Result<Option<FrameTimings>> {
        let start = Instant::now();
        let Frame::Ready(frame) = self.gpu.acquire()? else {
            // skipped (minimized etc.), drop all animation state
            state.changes.clear();
            self.scrolling.clear();
            self.fades.clear();
            self.slides.clear();
            self.animating = false;
            return Ok(None);
        };
        let acquired = Instant::now();
        self.frame += 1;
        log::trace!("frame {} (animation: {})", self.frame, self.animating);
        self.instances.clear();
        self.segments.clear();
        self.cursor_quad = None;
        self.cursor_text = 0..0;
        self.process_changes(state, acquired);
        if !state.grids.is_empty() {
            self.background = state.hl.defaults().bg;
        }

        let order = state.draw_order();
        if self.prepare_preview(preview, acquired).is_err()
            || self.update_rows(&order, state).is_err()
        {
            // atlas full: clear it and rebuild everything, but only try once
            log::info!("atlas full, rebuilding");
            self.clear_atlas();
            if self.prepare_preview(preview, acquired).is_err()
                || self.update_rows(&order, state).is_err()
            {
                log::error!("the visible glyphs do not fit in the atlas");
            }
        }
        self.scrolling.retain(|id, _| state.grids.contains_key(id));
        let mut scrolling = false;
        for s in self.scrolling.values_mut() {
            scrolling |= s.prune(acquired);
        }
        // prune before drawing so the last frame shows fully opaque text
        let fade = self.text_fade;
        self.fades
            .retain(|f| acquired.saturating_duration_since(f.since) < fade);
        let slide = self.text_slide;
        self.slides
            .retain(|s| acquired.saturating_duration_since(s.since) < slide);
        let cursor_moving = self.assemble(&order, state, acquired);
        self.assemble_preview();
        self.animating = scrolling
            || cursor_moving
            || !self.fades.is_empty()
            || !self.slides.is_empty()
            || self.preview.animating;
        let rows = &self.rows;
        for &(id, _, _) in &order {
            self.scrolling.entry(id).or_default().set_snapshot(
                state.grids[&id]
                    .rows
                    .iter()
                    .map(|row| rows[&row.id].1.clone()),
            );
        }
        let frame_id = self.frame;
        self.rows.retain(|_, (used, _)| *used == frame_id);
        state.clear_dirty();

        let screen = [
            self.gpu.surface_config.width as f32,
            self.gpu.surface_config.height as f32,
        ];
        self.pipeline.upload(
            &self.gpu.device,
            &self.gpu.queue,
            &self.instances,
            screen,
            self.shadow_blur(),
        );
        if let Some((corners, rgb)) = self.cursor_quad {
            self.cursor_pipeline
                .upload(&self.gpu.queue, corners, rgb, screen);
        }
        let image_textures = self.upload_preview_images(preview, screen);

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(to_wgpu_color(self.background, self.opacity)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            let size = [
                self.gpu.surface_config.width,
                self.gpu.surface_config.height,
            ];
            self.pipeline.draw(&mut pass, &self.segments, size);
            if self.cursor_quad.is_some() {
                self.cursor_pipeline.draw(&mut pass, size);
                let text = Segment {
                    range: self.cursor_text.clone(),
                    scissor: None,
                    replace: false,
                };
                self.pipeline.draw(&mut pass, &[text], size);
            }
            if let Some(panel) = self.preview.panel {
                // bg, images, text
                let rects = Segment {
                    range: self.preview.rect_range.clone(),
                    scissor: Some(panel),
                    replace: false,
                };
                self.pipeline.draw(&mut pass, &[rects], size);
                let textures: Vec<&ImageTexture> = image_textures
                    .iter()
                    .filter_map(|url| self.preview.textures.get(url).map(|(_, t)| t))
                    .collect();
                self.images.draw(&mut pass, &textures, panel);
                let glyphs = Segment {
                    range: self.preview.glyph_range.clone(),
                    scissor: Some(panel),
                    replace: false,
                };
                self.pipeline.draw(&mut pass, &[glyphs], size);
            }
        }
        self.gpu.queue.submit([encoder.finish()]);
        self.gpu.queue.present(frame);
        let presented = Instant::now();
        self.last_present = Some(presented);
        Ok(Some(FrameTimings {
            start,
            acquire: acquired - start,
            render: presented - acquired,
            presented,
        }))
    }

    fn update_rows(&mut self, order: &[Placed], state: &EditorState) -> Result<(), AtlasFull> {
        for &(id, _, _) in order {
            let floating = state
                .placements
                .get(&id)
                .is_some_and(|p| matches!(p.layer, Layer::Float { .. }));
            let ctx = RowContext {
                hl: &state.hl,
                text: &state.text,
                metrics: self.fonts.metrics,
                queue: &self.gpu.queue,
                // above grid 1 there's other content underneath, not the clear
                // color. Windows get a base in the window color, floats don't
                paint_default_bg: id != MAIN_GRID && (self.opacity >= 1.0 || floating),
            };
            for row in &state.grids[&id].rows {
                let (used, instances) = self.rows.entry(row.id).or_default();
                let cached = *used != 0;
                *used = self.frame;
                if row.dirty || !cached || instances.default_bg != ctx.paint_default_bg {
                    // make_mut clones if a scroll snapshot still holds the old version
                    self.builder.build(
                        row,
                        &ctx,
                        &mut self.fonts,
                        &mut self.raster,
                        &mut self.atlas,
                        Arc::make_mut(instances),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Grids bottom to top, then regions mid-scroll redrawn shifted, cursor last.
    /// Returns true while the cursor or the veil is still animating.
    fn assemble(&mut self, order: &[Placed], state: &EditorState, now: Instant) -> bool {
        let m = self.fonts.metrics;
        let cursor = state.cursor;
        let mut cursor_at: Option<(f32, f32, f32, Vec<Instance>)> = None;

        // Dim veil behind floats. A screen-sized float with no mouse and no focus
        // is a plugin's own backdrop (snacks does this): we draw it as our veil
        // instead, with our opacity, and skip adding another.
        let main_size = state.grids.get(&MAIN_GRID).map(|g| (g.width(), g.height()));
        let is_plugin_backdrop = |id: GridId| {
            id != cursor.grid
                && state
                    .placements
                    .get(&id)
                    .is_some_and(|p| !p.mouse && matches!(p.layer, Layer::Float { .. }))
                && main_size == state.grids.get(&id).map(|g| (g.width(), g.height()))
        };
        let plugin_backdrop = order.iter().any(|&(id, _, _)| is_plugin_backdrop(id));
        let in_float = state
            .placements
            .get(&cursor.grid)
            .is_some_and(|p| p.shadow && !p.hidden);
        let (veil, fading) = self.backdrop_level(in_float && !plugin_backdrop, now);
        log::trace!(
            "backdrop: cursor g{} in_float={in_float} plugin={plugin_backdrop} veil={veil:.2} positions={:?}",
            cursor.grid,
            state.placements
        );
        let screen = [
            self.gpu.surface_config.width as f32,
            self.gpu.surface_config.height as f32,
        ];
        let veil_quad = |alpha: f32| Instance {
            color: pack_rgba(0x000000, (alpha * 255.0).round() as u8),
            ..Instance::solid(0.0, 0.0, screen[0], screen[1], 0)
        };
        let mut veil_drawn = false;

        // no shadow for non-mouse floats (backdrops, decorations)
        let shadowed: Vec<(GridId, CellRect)> = if self.floating_shadow {
            order
                .iter()
                .filter(|&&(id, _, _)| {
                    state
                        .placements
                        .get(&id)
                        .is_some_and(|p| p.shadow && p.mouse && !is_plugin_backdrop(id))
                })
                .map(|&(id, row, col)| {
                    let g = &state.grids[&id];
                    let (width, height) = (g.width() as i64, g.height() as i64);
                    (
                        id,
                        CellRect {
                            row,
                            col,
                            width,
                            height,
                        },
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let shadow_groups = group_touching(&shadowed);

        for &(id, top, left) in order {
            let grid = &state.grids[&id];
            let floating = state
                .placements
                .get(&id)
                .is_some_and(|p| matches!(p.layer, Layer::Float { .. }));
            if floating && !veil_drawn && veil > 0.0 {
                // goes under the first float, over the normal windows
                let start = self.instances.len() as u32;
                self.instances.push(veil_quad(veil));
                self.segments.push(Segment {
                    range: start..self.instances.len() as u32,
                    scissor: None,
                    replace: false,
                });
                veil_drawn = true;
            }
            if is_plugin_backdrop(id) {
                if self.backdrop > 0.0 {
                    let start = self.instances.len() as u32;
                    self.instances.push(veil_quad(self.backdrop));
                    self.segments.push(Segment {
                        range: start..self.instances.len() as u32,
                        scissor: None,
                        replace: false,
                    });
                }
                continue;
            }
            let (ox, oy) = (left as f32 * m.width, top as f32 * m.height);
            let (gw, gh) = (
                grid.width() as f32 * m.width,
                grid.height() as f32 * m.height,
            );
            let rows: Vec<&RowInstances> = grid.rows.iter().map(|r| &*self.rows[&r.id].1).collect();
            // Shadows sit a bit lower than the float (light from above), blurred in
            // the shader. Floats that touch form a group and the whole group's
            // shadows go down at its first float, otherwise a picker's input would
            // cast a shadow on its list.
            if let Some(group) = shadow_groups.iter().find(|g| g[0].0 == id) {
                let blur = self.shadow_blur();
                let color = pack_rgba(0x000000, shadow_alpha(self.background));
                let start = self.instances.len() as u32;
                for &(_, r) in group {
                    let (x, y) = (r.col as f32 * m.width, r.row as f32 * m.height);
                    let (w, h) = (r.width as f32 * m.width, r.height as f32 * m.height);
                    self.instances.push(Instance {
                        pos: [x - blur, y - blur + blur * 0.4],
                        size: [w + 2.0 * blur, h + 2.0 * blur],
                        uv: [0.0; 2],
                        color,
                        kind: KIND_SHADOW,
                    });
                }
                self.segments.push(Segment {
                    range: start..self.instances.len() as u32,
                    scissor: None,
                    replace: false,
                });
            }
            // normal windows need a base over grid 1. Floats don't, they paint
            // every cell already and blend should let grid 1 show through
            if state
                .placements
                .get(&id)
                .is_some_and(|p| p.layer == Layer::Window)
            {
                push_base(
                    &mut self.instances,
                    &mut self.segments,
                    self.background,
                    self.opacity,
                    [ox, oy, gw, gh],
                );
            }
            let start = self.instances.len() as u32;
            let placed: Vec<(f32, &RowInstances)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| (oy + i as f32 * m.height, *r))
                .collect();
            push_rows(&mut self.instances, ox, &placed);
            if !self.fades.is_empty() || !self.slides.is_empty() {
                // push_rows put all the glyphs at the end, row by row
                let total: usize = rows.iter().map(|r| r.fg.len()).sum();
                let mut at = self.instances.len() - total;
                for (row, instances) in grid.rows.iter().zip(&rows) {
                    let glyphs = &mut self.instances[at..at + instances.fg.len()];
                    at += instances.fg.len();
                    let cells = |start: usize, end: usize| {
                        ox + start as f32 * m.width..ox + end as f32 * m.width
                    };
                    let fades: Vec<_> = self
                        .fades
                        .iter()
                        .filter(|f| f.row_id == row.id)
                        .map(|f| {
                            let alpha =
                                fade_alpha(now.saturating_duration_since(f.since), self.text_fade);
                            (cells(f.start, f.end), alpha)
                        })
                        .collect();
                    let slides: Vec<_> = self
                        .slides
                        .iter()
                        .filter(|s| s.row_id == row.id)
                        .map(|s| {
                            let elapsed = now.saturating_duration_since(s.since);
                            let offset = slide_offset(s.from, elapsed, self.text_slide);
                            (cells(s.start, s.end), -(offset * m.width).round())
                        })
                        .collect();
                    if fades.is_empty() && slides.is_empty() {
                        continue;
                    }
                    // test against the unshifted position
                    for g in glyphs.iter_mut() {
                        let center = g.pos[0] + g.size[0] / 2.0;
                        for (range, alpha) in &fades {
                            if range.contains(&center) {
                                g.color = scale_alpha(g.color, *alpha);
                            }
                        }
                        for (range, dx) in &slides {
                            if range.contains(&center) {
                                g.pos[0] += dx;
                            }
                        }
                    }
                }
            }
            self.segments.push(Segment {
                range: start..self.instances.len() as u32,
                scissor: (id != MAIN_GRID).then(|| scissor(ox, oy, gw, gh)),
                replace: false,
            });

            // mid-scroll: redraw shifted, rows that went off screen fill the gap
            let mut row_offset = vec![0.0f32; grid.height()];
            for anim in self.scrolling.get(&id).map_or(&[][..], |s| &s.animations) {
                let offset = (anim.offset(now) * m.height).round();
                let Region {
                    top,
                    bot,
                    left,
                    right,
                } = anim.region;
                let (bot, right) = (bot.min(grid.height()), right.min(grid.width()));
                if offset == 0.0 || top >= bot || left >= right {
                    continue;
                }
                let (x0, x1) = (ox + left as f32 * m.width, ox + right as f32 * m.width);
                let (y0, y1) = (oy + top as f32 * m.height, oy + bot as f32 * m.height);
                let mut placed: Vec<(f32, &RowInstances)> = (top..bot)
                    .map(|i| (oy + i as f32 * m.height + offset, rows[i]))
                    .collect();
                for (k, row) in anim.above.iter().rev().enumerate() {
                    let y = y0 + offset - (k + 1) as f32 * m.height;
                    if y + m.height <= y0 {
                        break;
                    }
                    placed.push((y, row));
                }
                for (k, row) in anim.below.iter().enumerate() {
                    let y = y1 + offset + k as f32 * m.height;
                    if y >= y1 {
                        break;
                    }
                    placed.push((y, row));
                }
                push_base(
                    &mut self.instances,
                    &mut self.segments,
                    self.background,
                    self.opacity,
                    [x0, y0, x1 - x0, y1 - y0],
                );
                let start = self.instances.len() as u32;
                push_rows(&mut self.instances, ox, &placed);
                self.segments.push(Segment {
                    range: start..self.instances.len() as u32,
                    scissor: Some(scissor(x0, y0, x1 - x0, y1 - y0)),
                    replace: false,
                });
                row_offset[top..bot].fill(offset);
            }

            if id == cursor.grid && cursor.row < grid.height() && cursor.col < grid.width() {
                let text = rows[cursor.row]
                    .fg
                    .iter()
                    .map(|i| shifted(i, ox, 0.0))
                    .collect();
                cursor_at = Some((ox, oy, row_offset[cursor.row], text));
            }
        }

        let Some((ox, oy, dy, cursor_row_text)) = cursor_at.filter(|_| !state.busy) else {
            return fading;
        };
        let row = &state.grids[&cursor.grid].rows[cursor.row];
        let cell = row.cells[cursor.col];
        let wide = row
            .cells
            .get(cursor.col + 1)
            .is_some_and(|c| c.text == CellText::EMPTY);
        let mode = state.cursor_mode();
        let attr = state.hl.attr(mode.attr_id);
        // attr_id 0 or a group with no colors of its own: invert the cell
        let (fg, bg) =
            if mode.attr_id == 0 || (attr.foreground.is_none() && attr.background.is_none()) {
                let c = state.hl.colors(cell.hl);
                (c.bg, c.fg)
            } else {
                let c = state.hl.colors(mode.attr_id);
                (c.fg, c.bg)
            };
        let x = ox + cursor.col as f32 * m.width;
        let y = oy + cursor.row as f32 * m.height;
        let w = m.width * if wide { 2.0 } else { 1.0 };
        let fraction = mode.cell_percentage as f32 / 100.0;
        let rect = match mode.cursor_shape {
            CursorShape::Block => [x, y, w, m.height],
            CursorShape::Vertical => [x, y, (m.width * fraction).round().max(1.0), m.height],
            CursorShape::Horizontal => {
                let ch = (m.height * fraction).round().max(1.0);
                [x, y + m.height - ch, w, ch]
            }
        };
        // In terminal mode the cursor tracks the shell, and entering/leaving it
        // jumps between the shell's cursor and nvim's. Animating that looks bad.
        let snap = state.terminal_mode || state.terminal_mode != self.cursor_in_terminal;
        self.cursor_in_terminal = state.terminal_mode;
        self.cursor.retarget(
            rect,
            [m.width, m.height],
            if snap {
                Duration::ZERO
            } else {
                self.cursor_duration
            },
            self.cursor_trail,
            now,
        );
        let (corners, settled) = self.cursor.update(now);
        // ride along with the row's scroll
        self.cursor_quad = Some((corners.map(|[cx, cy]| [cx, cy + dy]), bg));

        // at rest, redraw the glyphs under the cell in the cursor fg, clipped to
        // the cell, so a ligature only changes color inside the cursor
        let start = self.instances.len() as u32;
        if settled && mode.cursor_shape == CursorShape::Block {
            let y = y + dy;
            let cell_rect = [x, y, x + w, y + m.height];
            for inst in &cursor_row_text {
                if inst.kind == KIND_SOLID {
                    continue;
                }
                if let Some(clipped) = clip(&shifted(inst, 0.0, y), cell_rect) {
                    self.instances.push(Instance {
                        color: pack_rgb(fg),
                        ..clipped
                    });
                }
            }
        }
        self.cursor_text = start..self.instances.len() as u32;
        !settled || fading
    }

    fn prepare_preview(&mut self, preview: &Preview, now: Instant) -> Result<(), AtlasFull> {
        let panel = self.panel_rect();
        let p = &mut self.preview;
        p.animating = false;
        let (Some(panel), Some(doc)) = (panel, preview.doc.as_ref().filter(|_| preview.open))
        else {
            p.key = None;
            p.rects.clear();
            p.glyphs.clear();
            p.quads.clear();
            p.tick = None;
            return Ok(());
        };
        let [px, py, pw, ph] = panel;

        // exponential approach, tuned to feel about as fast as the grid scroll
        let target = preview.target_scroll(ph);
        let tau = self.scroll_duration.as_secs_f32() / 3.0;
        match p.tick {
            Some(tick) if tau > 0.0 => {
                let dt = now.saturating_duration_since(tick).as_secs_f32();
                p.scroll += (target - p.scroll) * (1.0 - (-dt / tau).exp());
                if (target - p.scroll).abs() < 0.5 {
                    p.scroll = target;
                } else {
                    p.animating = true;
                }
            }
            _ => p.scroll = target,
        }
        p.tick = Some(now);
        let scroll = p.scroll.round();

        let key = (
            Arc::as_ptr(doc) as usize,
            scroll as i64,
            [px as u32, py as u32, pw as u32, ph as u32],
            self.atlas_epoch,
        );
        if p.key == Some(key) {
            return Ok(());
        }
        p.key = None;
        p.rects.clear();
        p.glyphs.clear();
        p.quads.clear();
        let (top, bottom) = (scroll, scroll + ph);
        p.rects
            .push(Instance::solid(px, py, pw, ph, doc.background));
        for r in &doc.rects {
            if r.y + r.h >= top && r.y <= bottom && r.w > 0.0 && r.h > 0.0 {
                p.rects.push(Instance::solid(
                    px + r.x,
                    py + r.y - scroll,
                    r.w,
                    r.h,
                    r.color,
                ));
            }
        }
        let line = (self.scale as f32).round().max(1.0);
        p.rects.push(Instance::solid(px, py, line, ph, doc.divider));
        for img in &doc.images {
            if img.y + img.h >= top && img.y <= bottom {
                p.quads.push((
                    img.url.clone(),
                    ImageQuad {
                        pos: [px + img.x, py + img.y - scroll],
                        size: [img.w, img.h],
                    },
                ));
            }
        }
        // pad so glyphs with a baseline just off screen still get drawn
        let margin = self.fonts.size_px() * 3.0;
        for g in &doc.glyphs[doc.glyph_range(top - margin, bottom + margin)] {
            let Some(ag) =
                self.atlas
                    .get(g.key, &mut self.fonts, &mut self.raster, &self.gpu.queue)?
            else {
                continue;
            };
            self.preview.glyphs.push(Instance {
                pos: [px + g.x + ag.left as f32, py + g.y - scroll - ag.top as f32],
                size: [ag.size[0] as f32, ag.size[1] as f32],
                uv: [ag.uv[0] as f32, ag.uv[1] as f32],
                color: pack_rgb(g.color),
                kind: match ag.coverage {
                    raster::Coverage::Mask => pipeline::KIND_MASK,
                    raster::Coverage::Subpixel => pipeline::KIND_SUBPIXEL,
                    raster::Coverage::Color => pipeline::KIND_COLOR,
                },
            });
        }
        self.preview.key = Some(key);
        Ok(())
    }

    fn assemble_preview(&mut self) {
        let panel = self.panel_rect();
        let p = &mut self.preview;
        p.panel = None;
        if p.key.is_none() {
            return;
        }
        let start = self.instances.len() as u32;
        self.instances.extend_from_slice(&p.rects);
        let mid = self.instances.len() as u32;
        self.instances.extend_from_slice(&p.glyphs);
        let end = self.instances.len() as u32;
        p.rect_range = start..mid;
        p.glyph_range = mid..end;
        p.panel = panel.map(|[x, y, w, h]| scissor(x, y, w, h));
    }

    fn upload_preview_images(&mut self, preview: &Preview, screen: [f32; 2]) -> Vec<String> {
        let p = &mut self.preview;
        p.textures.retain(|url, (data, _)| {
            preview
                .images
                .get(url)
                .is_some_and(|d| Arc::ptr_eq(d, data))
        });
        let mut urls = Vec::new();
        let mut quads = Vec::new();
        for (url, quad) in &p.quads {
            let Some(data) = preview.images.get(url) else {
                continue;
            };
            if !p.textures.contains_key(url) {
                let texture = self.images.create_texture(
                    &self.gpu.device,
                    &self.gpu.queue,
                    data.width,
                    data.height,
                    &data.rgba,
                );
                p.textures.insert(url.clone(), (data.clone(), texture));
            }
            urls.push(url.clone());
            quads.push(*quad);
        }
        self.images
            .upload(&self.gpu.device, &self.gpu.queue, &quads, screen);
        urls
    }

    /// (opacity, still fading in)
    fn backdrop_level(&mut self, active: bool, now: Instant) -> (f32, bool) {
        if !active || self.backdrop <= 0.0 {
            self.backdrop_since = None;
            return (0.0, false);
        }
        let since = *self.backdrop_since.get_or_insert(now);
        let t = now.saturating_duration_since(since).as_secs_f32() / 0.12;
        (self.backdrop * t.min(1.0), t < 1.0)
    }
}

/// Starts at 0.3, not 0, so typing doesn't feel laggy.
fn fade_alpha(elapsed: Duration, duration: Duration) -> f32 {
    const START: f32 = 0.3;
    let t = (elapsed.as_secs_f32() / duration.as_secs_f32().max(f32::EPSILON)).clamp(0.0, 1.0);
    let ease = 1.0 - (1.0 - t) * (1.0 - t);
    START + (1.0 - START) * ease
}

fn slide_offset(from: f32, elapsed: Duration, duration: Duration) -> f32 {
    let t = (elapsed.as_secs_f32() / duration.as_secs_f32().max(f32::EPSILON)).clamp(0.0, 1.0);
    // Quadratic, not cubic: with cubic a 1-cell slide did nearly all its moving
    // in the first two frames and you couldn't see it.
    from * (1.0 - t).powi(2)
}

fn scale_alpha(color: u32, factor: f32) -> u32 {
    let alpha = ((color >> 24) as f32 * factor).round() as u32;
    (color & 0x00ff_ffff) | (alpha.min(255) << 24)
}

fn shifted(inst: &Instance, dx: f32, dy: f32) -> Instance {
    Instance {
        pos: [inst.pos[0] + dx, inst.pos[1] + dy],
        ..*inst
    }
}

/// All backgrounds first, then all text, so a row's bg never covers its neighbor's glyphs.
fn push_rows(out: &mut Vec<Instance>, dx: f32, rows: &[(f32, &RowInstances)]) {
    for (y, row) in rows {
        out.extend(row.bg.iter().map(|inst| shifted(inst, dx, *y)));
    }
    for (y, row) in rows {
        out.extend(row.fg.iter().map(|inst| shifted(inst, dx, *y)));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellRect {
    row: i64,
    col: i64,
    width: i64,
    height: i64,
}

impl CellRect {
    // sharing an edge or just a corner counts
    fn touches(&self, other: &CellRect) -> bool {
        self.row <= other.row + other.height
            && other.row <= self.row + self.height
            && self.col <= other.col + other.width
            && other.col <= self.col + self.width
    }
}

/// Transitive groups of touching floats, each in draw order.
fn group_touching(rects: &[(GridId, CellRect)]) -> Vec<Vec<(GridId, CellRect)>> {
    let mut groups: Vec<Vec<(GridId, CellRect)>> = Vec::new();
    for &item in rects {
        let hits: Vec<usize> = (0..groups.len())
            .filter(|&i| groups[i].iter().any(|(_, r)| r.touches(&item.1)))
            .collect();
        match hits.split_first() {
            None => groups.push(vec![item]),
            Some((&first, rest)) => {
                for &i in rest.iter().rev() {
                    let moved = groups.remove(i);
                    groups[first].extend(moved);
                }
                groups[first].push(item);
            }
        }
    }
    let position = |id: GridId| rects.iter().position(|(r, _)| *r == id);
    for g in &mut groups {
        g.sort_by_key(|(id, _)| position(*id));
    }
    groups
}

// dark backgrounds need a much darker shadow to show at all
fn shadow_alpha(background: u32) -> u8 {
    let c = |shift: u32| ((background >> shift) & 0xff) as f32 / 255.0;
    let lightness = 0.30 * c(16) + 0.59 * c(8) + 0.11 * c(0);
    if lightness < 0.35 { 190 } else { 110 }
}

/// Default background that hides what's under it, even when translucent.
fn push_base(
    instances: &mut Vec<Instance>,
    segments: &mut Vec<Segment>,
    background: u32,
    opacity: f32,
    [x, y, w, h]: [f32; 4],
) {
    let start = instances.len() as u32;
    instances.push(Instance {
        color: pack_rgba(background, (opacity * 255.0).round() as u8),
        ..Instance::solid(x, y, w, h, background)
    });
    segments.push(Segment {
        range: start..instances.len() as u32,
        scissor: Some(scissor(x, y, w, h)),
        replace: true,
    });
}

fn scissor(x: f32, y: f32, w: f32, h: f32) -> [u32; 4] {
    let (x0, y0) = (x.max(0.0), y.max(0.0));
    let (x1, y1) = ((x + w).max(x0), (y + h).max(y0));
    [x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32]
}

/// Atlas texels are 1:1 with screen pixels, so uv shifts with pos.
fn clip(inst: &Instance, [x0, y0, x1, y1]: [f32; 4]) -> Option<Instance> {
    let left = inst.pos[0].max(x0);
    let top = inst.pos[1].max(y0);
    let right = (inst.pos[0] + inst.size[0]).min(x1);
    let bottom = (inst.pos[1] + inst.size[1]).min(y1);
    (right > left && bottom > top).then(|| Instance {
        pos: [left, top],
        size: [right - left, bottom - top],
        uv: [
            inst.uv[0] + left - inst.pos[0],
            inst.uv[1] + top - inst.pos[1],
        ],
        ..*inst
    })
}

/// premultiplied
fn to_wgpu_color(rgb: u32, alpha: f32) -> wgpu::Color {
    let a = alpha as f64;
    let c = |shift: u32| ((rgb >> shift) & 0xff) as f64 / 255.0 * a;
    wgpu::Color {
        r: c(16),
        g: c(8),
        b: c(0),
        a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(row: i64, col: i64, width: i64, height: i64) -> CellRect {
        CellRect {
            row,
            col,
            width,
            height,
        }
    }

    #[test]
    fn fade_starts_visible_and_ends_full() {
        let d = Duration::from_millis(70);
        assert_eq!(fade_alpha(Duration::ZERO, d), 0.3);
        assert_eq!(fade_alpha(d, d), 1.0);
        assert_eq!(fade_alpha(Duration::from_secs(1), d), 1.0);
        let mid = fade_alpha(Duration::from_millis(35), d);
        assert!(mid > 0.65 && mid < 1.0, "{mid}");
        assert_eq!(scale_alpha(0xff12_3456, 0.5), 0x8012_3456);
        assert_eq!(scale_alpha(0x8012_3456, 1.0), 0x8012_3456);
    }

    #[test]
    fn slide_goes_from_shift_to_zero() {
        let d = Duration::from_millis(120);
        assert_eq!(slide_offset(2.0, Duration::ZERO, d), 2.0);
        assert_eq!(slide_offset(2.0, d, d), 0.0);
        assert_eq!(slide_offset(-1.0, Duration::from_secs(1), d), 0.0);
        let mid = slide_offset(1.0, Duration::from_millis(60), d);
        assert!(mid > 0.0 && mid < 0.5, "{mid}");
    }

    #[test]
    fn touching_floats_share_shadows() {
        // picker: input stacked on list, preview next to them, plus one far away
        let input = (2, rect(5, 10, 40, 3));
        let list = (3, rect(8, 10, 40, 20));
        let preview = (4, rect(5, 50, 40, 23));
        let far = (5, rect(40, 0, 10, 2));
        let groups = group_touching(&[input, list, far, preview]);
        assert_eq!(groups, vec![vec![input, list, preview], vec![far]]);
        assert!(!rect(0, 0, 2, 2).touches(&rect(3, 0, 2, 2)));
    }

    #[test]
    fn chain_merges_groups_in_draw_order() {
        let a = (2, rect(0, 0, 5, 5));
        let b = (3, rect(0, 10, 5, 5));
        let bridge = (4, rect(0, 5, 5, 5));
        let groups = group_touching(&[a, b, bridge]);
        assert_eq!(groups, vec![vec![a, b, bridge]]);
    }

    #[test]
    fn clip_moves_uv_with_position() {
        let inst = Instance {
            pos: [8.0, 2.0],
            size: [20.0, 10.0],
            uv: [100.0, 50.0],
            color: 0,
            kind: 1,
        };
        let c = clip(&inst, [11.0, 0.0, 22.0, 21.0]).unwrap();
        assert_eq!(
            (c.pos, c.size, c.uv),
            ([11.0, 2.0], [11.0, 10.0], [103.0, 50.0])
        );
        assert!(clip(&inst, [30.0, 0.0, 41.0, 21.0]).is_none());
    }
}
