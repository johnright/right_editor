use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

use cosmic_text::FontSystem;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::editor::{EditorState, MAIN_GRID};
use crate::input::{self, Mods};
use crate::latency::Latency;
use crate::nvim::NvimCommand;
use crate::nvim::redraw::RedrawEvent;
use crate::preview::layout::{Document, Params};
use crate::preview::{LinkTarget, Preview, PreviewEvent, PreviewMsg};
use crate::renderer::Renderer;
use crate::renderer::font::FontRequest;
use crate::vsync::VSync;
use crate::window_state::WindowState;

const ZOOM_STEP: f32 = 1.1;
const ZOOM_RANGE: (f32, f32) = (0.4, 4.0);

#[derive(Debug)]
pub enum UserEvent {
    /// Everything up to and including a `flush`, plus when the RPC thread got it.
    Redraw(Vec<RedrawEvent>, Instant),
    NvimExited(Result<String, String>),
    /// Reattached after `:restart`. The new server redraws from scratch.
    NvimRestarted,
    VSync,
    Preview(PreviewEvent),
    /// from the preview layout/image threads
    PreviewMsg(PreviewMsg),
}

pub struct App {
    config: Config,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    error: Option<anyhow::Error>,
    state: EditorState,
    flushes: u64,
    fonts: Option<JoinHandle<FontSystem>>,
    nvim: UnboundedSender<NvimCommand>,
    requested_grid: Option<(u64, u64)>,
    mods: Mods,
    mouse: (f64, f64),
    pointer_on_link: bool,
    /// Leftover fractions of a col/row from touchpad pixel deltas.
    scroll_accum: (f64, f64),
    dragging: Option<&'static str>,
    /// Grid the drag started on and its screen origin, in cells.
    drag_grid: Option<(u64, i64, i64)>,
    last_mouse_cell: Option<(u64, u64)>,
    /// Hidden while typing, back as soon as the mouse moves.
    pointer_visible: bool,
    latency: Option<Latency>,
    animating: bool,
    vsync: Option<VSync>,
    refresh: Duration,
    /// Cleared by the next flush.
    key_sent: Option<Instant>,
    zoom: f32,
    /// Last non-maximized, non-fullscreen geometry. Saved on exit.
    geometry: Option<WindowState>,
    /// Starts hidden (no white rectangle), shown with the first real frame.
    hidden_since: Option<Instant>,
    /// Windows: visible but DWM-cloaked (`DWMWA_CLOAK`) instead of hidden,
    /// otherwise the first frame never actually lands.
    cloaked: bool,
    preview: Preview,
}

/// Show it anyway after this, in case the real frame never comes (slow nvim, errors).
const MAX_HIDDEN: Duration = Duration::from_millis(1500);

impl App {
    pub fn new(
        config: Config,
        fonts: JoinHandle<FontSystem>,
        nvim: UnboundedSender<NvimCommand>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Self {
        let latency = config.latency.then(Latency::default);
        let preview_proxy = proxy.clone();
        let preview = Preview::new(Arc::new(move |msg| {
            let _ = preview_proxy.send_event(UserEvent::PreviewMsg(msg));
        }));
        let vsync = VSync::new(proxy)
            .inspect_err(|err| log::warn!("vsync thread not started: {err}"))
            .ok();
        Self {
            config,
            window: None,
            renderer: None,
            error: None,
            state: EditorState::default(),
            flushes: 0,
            fonts: Some(fonts),
            nvim,
            requested_grid: None,
            mods: Mods::default(),
            mouse: (0.0, 0.0),
            pointer_on_link: false,
            scroll_accum: (0.0, 0.0),
            dragging: None,
            drag_grid: None,
            last_mouse_cell: None,
            pointer_visible: true,
            latency,
            animating: false,
            vsync,
            refresh: Duration::from_micros(16_667),
            key_sent: None,
            zoom: 1.0,
            geometry: None,
            hidden_since: Some(Instant::now()),
            cloaked: false,
            preview,
        }
    }

    pub fn exit_result(self) -> Result<()> {
        if let Some(latency) = &self.latency {
            latency.summary();
        }
        self.error.map_or(Ok(()), Err)
    }

    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let mut attrs = Window::default_attributes()
            .with_title("right_editor")
            .with_inner_size(LogicalSize::new(1024.0, 640.0))
            .with_visible(false);
        // skip the saved position if it's off every monitor (second screen unplugged)
        let mut maximized = false;
        if let Some(saved) = WindowState::load() {
            let visible = event_loop.available_monitors().any(|m| {
                let (p, s) = (m.position(), m.size());
                let (x, y) = (saved.x + 50, saved.y + 10);
                x >= p.x && y >= p.y && x < p.x + s.width as i32 && y < p.y + s.height as i32
            });
            // Not with_maximized(): winit does ShowWindow(SW_MAXIMIZE), which shows
            // the window right away, all white, before we get to cloak it.
            attrs = attrs.with_inner_size(PhysicalSize::new(saved.width, saved.height));
            maximized = saved.maximized;
            if visible {
                attrs = attrs.with_position(PhysicalPosition::new(saved.x, saved.y));
            }
            self.geometry = Some(saved);
        }
        let window = Arc::new(event_loop.create_window(with_icon(attrs))?);
        // DXGI drops frames presented to a hidden window, so make it visible
        // but cloaked, and do it before creating the surface.
        self.cloaked = crate::window_state::set_cloaked(&window, true);
        if self.cloaked {
            if maximized {
                window.set_maximized(true);
            }
            window.set_visible(true);
        } else if maximized {
            window.set_maximized(true);
        }
        let fonts = self.fonts.take().context("fonts already taken")?;
        // guifont may already be here if nvim sent it while we were creating the window
        let request = self.font_request();
        let renderer = Renderer::new(window.clone(), &self.config, fonts, request)?;
        window.request_redraw();
        self.refresh = refresh_interval(&window);
        if let Some(vsync) = &self.vsync {
            vsync.set_interval(self.refresh);
        }
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.sync_grid_size();
        Ok(())
    }

    /// Wait for a frame at the real window size (after maximizing) with nvim's
    /// grid at the size we asked for (after guifont). Earlier = stretched frame, then a flash.
    fn show_when_ready(&mut self, presented: bool) {
        let (Some(since), Some(window), Some(renderer)) =
            (self.hidden_since, &self.window, &self.renderer)
        else {
            return;
        };
        let size = window.inner_size();
        let sized = renderer.surface_size() == (size.width, size.height);
        let grid = self
            .state
            .grids
            .get(&MAIN_GRID)
            .map(|g| (g.width() as u64, g.height() as u64));
        let settled = grid.is_some() && grid == self.requested_grid;
        let waited = since.elapsed();
        if !(presented && sized && settled) && waited < MAX_HIDDEN {
            return;
        }
        if !(self.cloaked && crate::window_state::set_cloaked(window, false)) {
            window.set_visible(true);
        }
        if presented && sized && settled {
            log::info!(
                "window shown after {waited:?} (cloaked: {})",
                self.cloaked
            );
        } else {
            log::warn!(
                "window shown after {waited:?} without a final frame \
                 (presented {presented}, sized {sized}, grid {grid:?} / {:?})",
                self.requested_grid
            );
        }
        window.request_redraw();
        self.hidden_since = None;
    }

    fn font_request(&self) -> FontRequest {
        font_request(&self.config, &self.state, self.zoom)
    }

    fn refresh_font(&mut self) {
        let request = self.font_request();
        if let Some(renderer) = &mut self.renderer
            && renderer.set_font(request)
        {
            self.sync_grid_size();
            self.refresh_preview();
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    fn refresh_preview(&mut self) {
        if !self.preview.open {
            return;
        }
        let Some(renderer) = &self.renderer else {
            return;
        };
        let Some([_, _, width, _]) = renderer.panel_rect() else {
            return;
        };
        let scale = renderer.scale() as f32;
        let defaults = self.state.hl.defaults();
        let params = Params {
            width,
            scale: scale * self.zoom,
            size: match self.config.preview_font_size {
                Some(pt) => (pt * 96.0 / 72.0 * scale * self.zoom).round(),
                None => renderer.text_size_px(),
            },
            family: self
                .config
                .preview_font
                .clone()
                .or_else(|| cfg!(windows).then(|| "Segoe UI".to_string())),
            mono: renderer.mono_family(),
            hinting: self.config.hinting,
            theme: self.preview.theme(defaults.fg, defaults.bg),
            images: Default::default(),
        };
        self.preview.request_layout(params, || renderer.font_db());
    }

    fn over_panel(&self) -> bool {
        self.renderer
            .as_ref()
            .and_then(Renderer::panel_rect)
            .is_some_and(|[x, ..]| self.mouse.0 >= x as f64)
    }

    /// Mouse in document coords, scroll included.
    fn preview_point(&self) -> Option<(f32, f32, Arc<Document>)> {
        let renderer = self.renderer.as_ref()?;
        let [px, py, pw, ph] = renderer.panel_rect()?;
        let (x, y) = (self.mouse.0 as f32 - px, self.mouse.1 as f32 - py);
        if x < 0.0 || y < 0.0 || x >= pw || y >= ph {
            return None;
        }
        let doc = self.preview.doc.clone()?;
        Some((x, y + renderer.preview_scroll(), doc))
    }

    /// Follow a link, or else jump nvim's cursor to the source line under the mouse.
    fn click_preview(&mut self) {
        let Some((x, y, doc)) = self.preview_point() else {
            return;
        };
        let Some(url) = doc.link_at(x, y) else {
            let line = doc.line_at(y) + 1;
            self.send(NvimCommand::Lua(
                "_G.right_editor_preview.goto_line(...)".into(),
                vec![line.into()],
            ));
            return;
        };
        match crate::preview::link_target(url) {
            LinkTarget::Anchor(anchor) => {
                if let Some(target) = doc.target(&anchor) {
                    let margin = self.renderer.as_ref().map_or(0.0, Renderer::text_size_px);
                    self.preview.manual_scroll = Some((target - margin).max(0.0));
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }
            LinkTarget::Web(url) => crate::preview::open_url(&url),
            LinkTarget::File(path) => self.send(NvimCommand::Lua(
                "_G.right_editor_preview.open_file(...)".into(),
                vec![path.into()],
            )),
            LinkTarget::Unsupported => log::info!("preview: unsupported link: {url}"),
        }
    }

    fn on_preview(&mut self, event: PreviewEvent) {
        if self.preview.apply(event) {
            let fraction = if self.preview.open {
                self.config.preview_width
            } else {
                0.0
            };
            if let Some(renderer) = &mut self.renderer {
                renderer.set_panel(fraction);
            }
            self.sync_grid_size();
        }
        self.refresh_preview();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn on_preview_msg(&mut self, msg: PreviewMsg) {
        match msg {
            PreviewMsg::Laid(generation, doc) => {
                let max_width = self
                    .renderer
                    .as_ref()
                    .and_then(Renderer::panel_rect)
                    .map_or(0, |[_, _, w, _]| w as u32);
                let scale = self
                    .renderer
                    .as_ref()
                    .map_or(1.0, |r| r.scale() as f32 * self.zoom);
                if !self.preview.laid(generation, doc, max_width, scale) {
                    return;
                }
            }
            PreviewMsg::Image(url, result) => {
                self.preview.image_loaded(url, result);
                self.refresh_preview();
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        self.refresh_font();
    }

    /// GUI-only, never sent to nvim: Ctrl+=/-/0 zoom, Alt+Enter maximize,
    /// Ctrl+Shift+V paste, Ctrl+Shift+M preview.
    fn shortcut(&mut self, key: &KeyEvent) -> bool {
        if key.state != ElementState::Pressed {
            return false;
        }
        let m = self.mods;
        let ctrl_only = m.ctrl && !m.alt && !m.logo;
        // Windows clipboard, any mode incl. :terminal. Physical key so the
        // layout doesn't matter.
        if ctrl_only && m.shift && key.physical_key == PhysicalKey::Code(KeyCode::KeyV) {
            self.paste_clipboard();
            return true;
        }
        // preview state lives in nvim, same toggle as :RightEditorPreview
        if ctrl_only && m.shift && key.physical_key == PhysicalKey::Code(KeyCode::KeyM) {
            if !key.repeat {
                self.send(NvimCommand::Lua(crate::preview::TOGGLE.to_string(), vec![]));
            }
            return true;
        }
        match &key.logical_key {
            Key::Character(c) if ctrl_only && (c == "+" || c == "=") => {
                self.set_zoom(self.zoom * ZOOM_STEP)
            }
            Key::Character(c) if ctrl_only && c == "-" => self.set_zoom(self.zoom / ZOOM_STEP),
            Key::Character(c) if ctrl_only && c == "0" => self.set_zoom(1.0),
            Key::Named(NamedKey::Enter) if m.alt && !m.ctrl && !m.logo && !key.repeat => {
                if let Some(window) = &self.window {
                    if window.fullscreen().is_some() {
                        window.set_fullscreen(None::<Fullscreen>);
                    } else {
                        window.set_maximized(!window.is_maximized());
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// A second present in the same refresh blocks until the first is on screen,
    /// up to a whole refresh on the hybrid GPU (cross-adapter copy). So skip the
    /// tick if a key's frame is coming or we just presented. Animations are
    /// time-based, a skipped tick costs nothing.
    fn animation_frame_due(&self) -> bool {
        let now = Instant::now();
        let key_pending = self
            .key_sent
            .is_some_and(|sent| now.saturating_duration_since(sent) < MAX_KEY_WAIT);
        let just_presented = self
            .renderer
            .as_ref()
            .and_then(Renderer::last_present)
            .is_some_and(|t| now.saturating_duration_since(t) < self.refresh / 2);
        !key_pending && !just_presented
    }

    fn paste_clipboard(&mut self) {
        match crate::clipboard::read_text() {
            Some(text) if !text.is_empty() => {
                self.hide_pointer();
                self.send(NvimCommand::Paste(text));
            }
            _ => log::debug!("no text in the clipboard"),
        }
    }

    /// Only matters off Windows. There exiting() asks the system instead.
    fn track_geometry(&mut self) {
        let Some(window) = &self.window else { return };
        if window.is_maximized()
            || window.fullscreen().is_some()
            || window.is_minimized() == Some(true)
        {
            return;
        }
        if let Ok(pos) = window.outer_position() {
            let size = window.inner_size();
            self.geometry = Some(WindowState {
                x: pos.x,
                y: pos.y,
                width: size.width,
                height: size.height,
                maximized: false,
            });
        }
    }

    fn sync_grid_size(&mut self) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let size = renderer.grid_size();
        if self.requested_grid != Some(size) {
            self.requested_grid = Some(size);
            let (cols, rows) = size;
            let _ = self.nvim.send(NvimCommand::Resize { cols, rows });
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        log::error!("{err:#}");
        self.error = Some(err);
        event_loop.exit();
    }

    fn on_redraw(&mut self, batch: Vec<RedrawEvent>) {
        self.key_sent = None;
        self.log_batch(&batch);
        if !self.state.apply(&batch) {
            return;
        }
        if log::log_enabled!(log::Level::Debug) {
            self.dump_dirty_rows();
        }
        self.refresh_font();
        self.refresh_preview();
        if let Some(window) = &self.window {
            // we `set title` at attach; empty means fall back to ours
            if let Some(title) = self.state.title.take() {
                window.set_title(if title.is_empty() {
                    "right_editor"
                } else {
                    &title
                });
            }
            window.request_redraw();
        }
    }

    fn dump_dirty_rows(&self) {
        let mut ids: Vec<_> = self.state.grids.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let grid = &self.state.grids[&id];
            for (r, row) in grid.rows.iter().enumerate() {
                if row.dirty {
                    log::debug!("g{id} {r:3}│{}│", grid.row_text(r, &self.state.text));
                }
            }
        }
        let c = self.state.cursor;
        log::debug!("cursor: g{} r{} c{}", c.grid, c.row, c.col);
    }

    fn send(&self, cmd: NvimCommand) {
        let _ = self.nvim.send(cmd);
    }

    fn mouse_cell(&self) -> Option<(u64, u64)> {
        let renderer = self.renderer.as_ref()?;
        let (cell_w, cell_h) = renderer.cell_size();
        let (cols, rows) = self.requested_grid?;
        let col = (self.mouse.0 / cell_w as f64).max(0.0) as u64;
        let row = (self.mouse.1 / cell_h as f64).max(0.0) as u64;
        Some((
            row.min(rows.saturating_sub(1)),
            col.min(cols.saturating_sub(1)),
        ))
    }

    fn send_mouse(&mut self, button: &'static str, action: &'static str) {
        let Some((row, col)) = self.mouse_cell() else {
            return;
        };
        let (row, col) = (row as i64, col as i64);
        let (grid, row, col) = if !self.config.multigrid {
            // no multigrid: grid 0, screen cells
            (0, row, col)
        } else if let (Some(_), "drag" | "release", Some((grid, top, left))) =
            (self.dragging, action, self.drag_grid)
        {
            // drags stick to the grid they started on, even outside its window
            (grid, row - top, col - left)
        } else {
            let Some((grid, r, c)) = self.state.grid_at(row, col) else {
                return;
            };
            if action == "press" {
                self.drag_grid = Some((grid, row - r, col - c));
            }
            (grid, r, c)
        };
        self.send(NvimCommand::Mouse {
            button,
            action,
            modifier: self.mods.mouse(),
            grid,
            row: row.max(0) as u64,
            col: col.max(0) as u64,
        });
    }

    fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        let name = match button {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
            MouseButton::Back => "x1",
            MouseButton::Forward => "x2",
            MouseButton::Other(_) => return,
        };
        self.show_pointer();
        match state {
            ElementState::Pressed if self.over_panel() => {
                if button == MouseButton::Left {
                    self.click_preview();
                }
            }
            ElementState::Released if self.dragging != Some(name) && self.over_panel() => {}
            ElementState::Pressed => {
                if let Some(latency) = &mut self.latency {
                    latency.input("mouse", name);
                }
                self.dragging = Some(name);
                self.last_mouse_cell = self.mouse_cell();
                self.send_mouse(name, "press");
            }
            ElementState::Released => {
                if self.dragging == Some(name) {
                    self.dragging = None;
                }
                self.send_mouse(name, "release");
            }
        }
    }

    /// `drag` with a button down, else `move` (for 'mousemoveevent').
    /// Only sent when the cell changes.
    fn on_mouse_move(&mut self) {
        self.show_pointer();
        let on_link = self.dragging.is_none()
            && self
                .preview_point()
                .is_some_and(|(x, y, doc)| doc.link_at(x, y).is_some());
        if on_link != self.pointer_on_link
            && let Some(window) = &self.window
        {
            window.set_cursor(if on_link {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            });
            self.pointer_on_link = on_link;
        }
        if self.dragging.is_none() && self.over_panel() {
            return;
        }
        let cell = self.mouse_cell();
        if cell.is_none() || cell == self.last_mouse_cell {
            return;
        }
        self.last_mouse_cell = cell;
        match self.dragging {
            Some(button) => {
                if let Some(latency) = &mut self.latency {
                    latency.input("mouse", "drag");
                }
                self.send_mouse(button, "drag");
            }
            None => self.send_mouse("move", ""),
        }
    }

    /// One nvim `wheel` event per row/column. Ctrl+wheel zooms.
    fn on_wheel(&mut self, delta: MouseScrollDelta) {
        if self.mods.ctrl && !self.mods.alt {
            let up = match delta {
                MouseScrollDelta::LineDelta(_, y) => y > 0.0,
                MouseScrollDelta::PixelDelta(p) => p.y > 0.0,
            };
            self.set_zoom(if up {
                self.zoom * ZOOM_STEP
            } else {
                self.zoom / ZOOM_STEP
            });
            return;
        }
        let Some(renderer) = &self.renderer else {
            return;
        };
        if let Some([_, _, _, panel_h]) = renderer.panel_rect().filter(|_| self.over_panel()) {
            // the preview scrolls by itself until nvim scrolls the source
            let step = renderer.text_size_px() * 1.5;
            let dy = match delta {
                MouseScrollDelta::LineDelta(_, y) => -y * 3.0 * step,
                MouseScrollDelta::PixelDelta(p) => -p.y as f32,
            };
            let current = renderer.preview_scroll();
            self.preview.scroll_by(current, dy, panel_h);
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            return;
        }
        let (cell_w, cell_h) = renderer.cell_size();
        let (cols, lines) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
            // TODO: touchpads still scroll by whole rows, no pixel scrolling yet
            MouseScrollDelta::PixelDelta(p) => {
                self.scroll_accum.0 += p.x / cell_w as f64;
                self.scroll_accum.1 += p.y / cell_h as f64;
                let whole = (self.scroll_accum.0.trunc(), self.scroll_accum.1.trunc());
                self.scroll_accum.0 -= whole.0;
                self.scroll_accum.1 -= whole.1;
                whole
            }
        };
        if (lines.abs() >= 0.5 || cols.abs() >= 0.5)
            && let Some(latency) = &mut self.latency
        {
            latency.input("wheel", if lines < 0.0 { "down" } else { "up" });
        }
        for _ in 0..lines.abs().round() as u32 {
            self.send_mouse("wheel", if lines > 0.0 { "up" } else { "down" });
        }
        // positive x is left
        for _ in 0..cols.abs().round() as u32 {
            self.send_mouse("wheel", if cols > 0.0 { "left" } else { "right" });
        }
    }

    fn hide_pointer(&mut self) {
        if self.pointer_visible
            && let Some(window) = &self.window
        {
            window.set_cursor_visible(false);
            self.pointer_visible = false;
        }
    }

    fn show_pointer(&mut self) {
        if !self.pointer_visible
            && let Some(window) = &self.window
        {
            window.set_cursor_visible(true);
            self.pointer_visible = true;
        }
    }

    fn log_batch(&mut self, batch: &[RedrawEvent]) {
        self.flushes += 1;
        if !log::log_enabled!(log::Level::Debug) {
            return;
        }
        if log::log_enabled!(log::Level::Trace) {
            for ev in batch {
                log::trace!("  {ev}");
            }
        }
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for ev in batch {
            match counts.iter_mut().find(|(name, _)| *name == ev.name()) {
                Some((_, n)) => *n += 1,
                None => counts.push((ev.name(), 1)),
            }
        }
        let summary: Vec<String> = counts
            .iter()
            .map(|(name, n)| format!("{name}×{n}"))
            .collect();
        log::debug!(
            "flush #{}: {} events [{}]",
            self.flushes,
            batch.len(),
            summary.join(", ")
        );
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none()
            && let Err(err) = self.init(event_loop)
        {
            self.fail(event_loop, err);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Input goes straight to nvim, no waiting on replies or the next frame.
        match &event {
            WindowEvent::ModifiersChanged(m) => {
                self.mods = m.state().into();
                return;
            }
            WindowEvent::KeyboardInput {
                event: key,
                is_synthetic,
                ..
            } => {
                let keys = (key.state == ElementState::Pressed)
                    .then(|| input::translate(&key.logical_key, key.text.as_deref(), self.mods))
                    .flatten();
                // --log right_editor::keys=debug
                log::debug!(
                    target: "right_editor::keys",
                    "{:?}{} {:?} physical={:?} text={:?} mod={:?} → {:?}",
                    key.state,
                    if key.repeat { " (repeat)" } else { "" },
                    key.logical_key,
                    key.physical_key,
                    key.text,
                    self.mods,
                    keys,
                );
                // winit fakes events for keys already held when focus changes (e.g.
                // still holding something after Alt+Tab). Not typed here, drop them.
                if *is_synthetic {
                    log::debug!(target: "right_editor::keys", "  (synthetic event, ignored)");
                    return;
                }
                if self.shortcut(key) {
                    return;
                }
                if let Some(keys) = keys {
                    if let Some(latency) = &mut self.latency {
                        latency.input("key", &keys);
                    }
                    self.hide_pointer();
                    self.key_sent = Some(Instant::now());
                    self.send(NvimCommand::Input(keys));
                }
                return;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x, position.y);
                self.on_mouse_move();
                return;
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse_button(*state, *button);
                return;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.on_wheel(*delta);
                return;
            }
            WindowEvent::Focused(gained) => {
                self.send(NvimCommand::Focus(*gained));
                // frames get skipped while occluded, even mid-animation
                if *gained && let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
            WindowEvent::CloseRequested => {
                // Let nvim decide (quit, or ask about unsaved changes). We close when it exits.
                if self.nvim.send(NvimCommand::Quit).is_err() {
                    event_loop.exit();
                }
                return;
            }
            _ => {}
        }

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        match event {
            WindowEvent::Resized(size) => {
                // eprintln!("resized {}x{}", size.width, size.height);
                renderer.resize(size.width, size.height);
                self.sync_grid_size();
                self.refresh_preview();
                self.track_geometry();
                // Win32's modal resize loop blocks everything while the border is
                // dragged. Redraw now or the content shows up stretched.
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Moved(_) => {
                // could be a monitor with another refresh rate
                if let Some(window) = &self.window {
                    self.refresh = refresh_interval(window);
                    if let Some(vsync) = &self.vsync {
                        vsync.set_interval(self.refresh);
                    }
                }
                self.track_geometry();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                renderer.set_scale(scale_factor);
                self.sync_grid_size();
                self.refresh_preview();
            }
            WindowEvent::RedrawRequested => {
                let result = renderer.render(&mut self.state, &self.preview);
                self.animating = renderer.animating();
                // still hidden: one frame per refresh until the real one lands or MAX_HIDDEN runs out
                let busy = self.animating || self.hidden_since.is_some();
                match &self.vsync {
                    Some(vsync) => vsync.set_active(busy),
                    None if busy => {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                    None => {}
                }
                let presented = matches!(result, Ok(Some(_)));
                match result {
                    Ok(Some(frame)) => {
                        if let Some(latency) = &mut self.latency {
                            latency.presented(frame);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => return self.fail(event_loop, err),
                }
                self.show_when_ready(presented);
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        let Some(window) = &self.window else { return };
        #[cfg(windows)]
        let geometry = crate::window_state::normal_placement(window);
        #[cfg(not(windows))]
        let geometry = self.geometry.map(|g| WindowState {
            maximized: window.is_maximized(),
            ..g
        });
        if let Some(geometry) = geometry {
            geometry.save();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Redraw(batch, flushed) => {
                if let Some(latency) = &mut self.latency {
                    latency.flushed(flushed);
                }
                self.on_redraw(batch)
            }
            UserEvent::NvimExited(Ok(reason)) => {
                log::info!("{reason}");
                event_loop.exit();
            }
            UserEvent::NvimExited(Err(err)) => self.fail(event_loop, anyhow::anyhow!(err)),
            UserEvent::NvimRestarted => {
                log::info!(":restart: attached to the new server");
                self.state = EditorState::default();
                self.preview.reset();
                if let Some(renderer) = &mut self.renderer {
                    renderer.reset_animations();
                    renderer.set_panel(0.0);
                }
                // the old server's size doesn't carry over
                self.requested_grid = None;
                self.sync_grid_size();
            }
            UserEvent::Preview(event) => self.on_preview(event),
            UserEvent::PreviewMsg(msg) => self.on_preview_msg(msg),
            UserEvent::VSync => {
                if self.animating
                    && self.animation_frame_due()
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
        }
    }
}

/// After this long without an answer to a key, animation frames resume.
const MAX_KEY_WAIT: Duration = Duration::from_millis(50);

/// nvim's built-in default, so not something the user picked.
const NVIM_DEFAULT_GUIFONT: &str = "Source Code Pro,DejaVu Sans Mono,Courier New,monospace";

/// CLI / config.toml, then `guifont`, then defaults.
fn font_request(config: &Config, state: &EditorState, zoom: f32) -> FontRequest {
    let guifont = state
        .guifont
        .as_deref()
        .filter(|g| *g != NVIM_DEFAULT_GUIFONT)
        .map(FontRequest::parse_guifont)
        .unwrap_or_default();
    let defaults = FontRequest::default();
    FontRequest {
        families: config.font.clone().map_or(guifont.families, |f| vec![f]),
        size_pt: config
            .font_size
            .or(guifont.size)
            .unwrap_or(defaults.size_pt)
            * zoom,
        linespace: state.linespace as f32,
        weight: config
            .font_weight
            .or(guifont.weight)
            .unwrap_or(defaults.weight),
        hinting: config.hinting,
    }
}

fn refresh_interval(window: &Window) -> Duration {
    window
        .current_monitor()
        .and_then(|m| m.refresh_rate_millihertz())
        .filter(|&mhz| mhz > 0)
        .map_or(Duration::from_micros(16_667), |mhz| {
            Duration::from_nanos(1_000_000_000_000 / mhz as u64)
        })
}

/// Resource from build.rs; Windows picks the size.
#[cfg(windows)]
fn with_icon(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
    use winit::window::Icon;

    match (Icon::from_resource(1, None), Icon::from_resource(1, None)) {
        (Ok(window_icon), Ok(taskbar_icon)) => attrs
            .with_window_icon(Some(window_icon))
            .with_taskbar_icon(Some(taskbar_icon)),
        (Err(err), _) | (_, Err(err)) => {
            log::warn!("icon not loaded: {err}");
            attrs
        }
    }
}

#[cfg(not(windows))]
fn with_icon(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    attrs
}
