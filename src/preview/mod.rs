//! Markdown preview panel, drawn by the GUI next to the nvim grid.
//!
//! `preview.lua` runs inside nvim and pushes buffer text, viewport and theme
//! colors as `nvright_md` notifications. Parse + layout happen on one
//! thread and image loading on another; the window thread only ever gets
//! finished documents and pixels.

pub mod layout;
pub mod parse;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use cosmic_text::FontSystem;
use cosmic_text::fontdb::Database;
use rmpv::Value;

use layout::{Document, ImageSize, Params, Theme};
use parse::Highlight;

pub const LUA: &str = include_str!("preview.lua");

pub const TOGGLE: &str = "_G.nvright_preview.toggle()";

#[derive(Debug, Clone, PartialEq)]
pub enum PreviewEvent {
    Open(bool),
    Text {
        /// Linux path when running under WSL.
        path: String,
        distro: String,
        text: Arc<str>,
        highlights: Arc<[Highlight]>,
    },
    View {
        /// 1-based visible range, plus the buffer's line count
        topline: u32,
        botline: u32,
        lines: u32,
    },
    Theme(HashMap<String, u32>),
}

#[derive(Debug)]
pub enum PreviewMsg {
    Laid(u64, Arc<Document>),
    Image(String, Result<Arc<ImageData>, String>),
}

#[derive(Debug)]
pub struct ImageData {
    pub natural: (u32, u32),
    /// straight RGBA, possibly downscaled
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn parse_notification(args: Vec<Value>) -> Option<PreviewEvent> {
    let mut args = args.into_iter();
    let kind = args.next()?;
    let data = args.next().unwrap_or(Value::Nil);
    let get = |key: &str| -> Option<&Value> {
        data.as_map()?
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let num = |key: &str| get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
    let string = |key: &str| get(key).and_then(Value::as_str).unwrap_or("").to_string();
    Some(match kind.as_str()? {
        "open" => PreviewEvent::Open(data.as_bool().unwrap_or(false)),
        "text" => {
            let lines = get("lines").and_then(Value::as_array)?;
            let mut text = String::new();
            for line in lines {
                // invalid UTF-8 comes through as Binary
                match line {
                    Value::String(s) => text.push_str(&String::from_utf8_lossy(s.as_bytes())),
                    Value::Binary(b) => text.push_str(&String::from_utf8_lossy(b)),
                    _ => {}
                }
                text.push('\n');
            }
            PreviewEvent::Text {
                path: string("path"),
                distro: string("distro"),
                text: text.into(),
                highlights: get("hl")
                    .and_then(Value::as_array)
                    .map(|hl| parse_highlights(hl))
                    .unwrap_or_default()
                    .into(),
            }
        }
        "view" => PreviewEvent::View {
            topline: num("topline").max(1),
            botline: num("botline").max(1),
            lines: num("lines").max(1),
        },
        "theme" => PreviewEvent::Theme(
            data.as_map()?
                .iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_u64()? as u32)))
                .collect(),
        ),
        _ => return None,
    })
}

/// nvim sends them flattened, six numbers each: row0, col0, row1, col1,
/// color, flags (1 = bold, 2 = italic).
fn parse_highlights(values: &[Value]) -> Vec<Highlight> {
    let nums: Vec<u32> = values
        .iter()
        .map(|v| v.as_u64().unwrap_or(0) as u32)
        .collect();
    nums.as_chunks::<6>()
        .0
        .iter()
        .map(|&[r0, c0, r1, c1, color, flags]| Highlight {
            start: (r0, c0),
            end: (r1, c1),
            color,
            bold: flags & 1 != 0,
            italic: flags & 2 != 0,
        })
        .collect()
}

struct Job {
    generation: u64,
    text: Arc<str>,
    highlights: Arc<[Highlight]>,
    params: Params,
}

pub type Sink = Arc<dyn Fn(PreviewMsg) + Send + Sync>;

pub struct Preview {
    pub open: bool,
    path: String,
    distro: String,
    text: Option<Arc<str>>,
    highlights: Arc<[Highlight]>,
    colors: HashMap<String, u32>,
    pub view: (u32, u32, u32),
    pub doc: Option<Arc<Document>>,
    /// Set by the wheel over the panel, dropped as soon as nvim scrolls.
    pub manual_scroll: Option<f32>,
    pub images: HashMap<String, Arc<ImageData>>,
    image_sizes: HashMap<String, ImageSize>,
    requested_images: Vec<String>,
    generation: u64,
    last_job: Option<(Arc<str>, Params)>,
    sink: Sink,
    worker: Option<Sender<Job>>,
    loader: Option<Sender<LoadRequest>>,
    /// Editor font db with browser-like generic families, for text in SVGs.
    fonts: Option<Arc<Database>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    File(PathBuf),
    Web(String),
}

struct LoadRequest {
    url: String,
    source: ImageSource,
    max_width: u32,
    /// device px per image px, SVG only
    scale: f32,
    fonts: Option<Arc<Database>>,
}

impl Preview {
    pub fn new(sink: Sink) -> Self {
        Self {
            open: false,
            path: String::new(),
            distro: String::new(),
            text: None,
            highlights: Arc::new([]),
            colors: HashMap::new(),
            view: (1, 1, 1),
            doc: None,
            manual_scroll: None,
            images: HashMap::new(),
            image_sizes: HashMap::new(),
            requested_images: Vec::new(),
            generation: 0,
            last_job: None,
            sink,
            worker: None,
            loader: None,
            fonts: None,
        }
    }

    /// `:restart`: the new server starts with the preview closed. Threads are reused.
    pub fn reset(&mut self) {
        let sink = self.sink.clone();
        let (worker, loader, fonts) = (self.worker.take(), self.loader.take(), self.fonts.take());
        *self = Self::new(sink);
        self.worker = worker;
        self.loader = loader;
        self.fonts = fonts;
    }

    /// Returns true when the panel opened or closed.
    pub fn apply(&mut self, event: PreviewEvent) -> bool {
        match event {
            PreviewEvent::Open(open) => {
                let changed = open != self.open;
                self.open = open;
                changed
            }
            PreviewEvent::Text {
                path,
                distro,
                text,
                highlights,
            } => {
                if path != self.path {
                    // different file, old images and scroll are stale
                    self.images.clear();
                    self.image_sizes.clear();
                    self.requested_images.clear();
                    self.manual_scroll = None;
                }
                self.path = path;
                self.distro = distro;
                self.text = Some(text);
                self.highlights = highlights;
                false
            }
            PreviewEvent::View {
                topline,
                botline,
                lines,
            } => {
                self.view = (topline, botline, lines);
                self.manual_scroll = None;
                false
            }
            PreviewEvent::Theme(colors) => {
                self.colors = colors;
                false
            }
        }
    }

    pub fn theme(&self, default_fg: u32, default_bg: u32) -> Theme {
        let c = |name: &str, fallback: u32| self.colors.get(name).copied().unwrap_or(fallback);
        let fg = c("fg", default_fg);
        let bg = c("bg", default_bg);
        let muted = c("muted", mix(fg, bg, 0.55));
        let heading = c("heading", fg);
        let link = c("link", fg);
        Theme {
            fg,
            bg,
            headings: std::array::from_fn(|i| c(&format!("heading{}", i + 1), heading)),
            link,
            code: c("code", fg),
            code_bg: c("code_bg", mix(fg, bg, 0.08)),
            quote: c("quote", muted),
            muted,
            rule: c("rule", mix(fg, bg, 0.2)),
            alerts: [
                c("note", link),
                c("tip", link),
                c("important", link),
                c("warning", link),
                c("caution", link),
            ],
        }
    }

    /// No-op if neither the text nor the params changed since the last request.
    /// `fonts` is only called once, to seed the layout thread.
    pub fn request_layout(
        &mut self,
        mut params: Params,
        fonts: impl FnOnce() -> (String, Database),
    ) {
        let Some(text) = self.text.clone().filter(|_| self.open) else {
            return;
        };
        params.images = self.image_sizes.clone();
        if self
            .last_job
            .as_ref()
            .is_some_and(|(t, p)| Arc::ptr_eq(t, &text) && *p == params)
        {
            return;
        }
        if self.worker.is_none() {
            let (locale, db) = fonts();
            // usvg resolves generic families (sans-serif etc.), map them like a browser would
            let mut svg_fonts = db.clone();
            if cfg!(windows) {
                svg_fonts.set_sans_serif_family("Arial");
                svg_fonts.set_serif_family("Times New Roman");
                svg_fonts.set_monospace_family("Consolas");
            }
            self.fonts = Some(Arc::new(svg_fonts));
            self.worker = Some(spawn_worker(locale, db, self.sink.clone()));
        }
        self.generation += 1;
        self.last_job = Some((text.clone(), params.clone()));
        if let Some(worker) = &self.worker {
            let _ = worker.send(Job {
                generation: self.generation,
                text,
                highlights: self.highlights.clone(),
                params,
            });
        }
    }

    /// Returns false for a stale generation. Also starts loading whatever images
    /// the layout still has no size for.
    pub fn laid(&mut self, generation: u64, doc: Arc<Document>, max_width: u32, scale: f32) -> bool {
        if generation != self.generation {
            return false;
        }
        for url in &doc.missing_images {
            if self.requested_images.contains(url) {
                continue;
            }
            self.requested_images.push(url.clone());
            match resolve_image(&self.path, url, &self.distro) {
                Some(source) => {
                    let request = LoadRequest {
                        url: url.clone(),
                        source,
                        max_width,
                        scale,
                        fonts: self.fonts.clone(),
                    };
                    if matches!(request.source, ImageSource::Web(_)) {
                        // downloads can be slow, one thread each
                        let sink = self.sink.clone();
                        let spawned = std::thread::Builder::new()
                            .name("download".into())
                            .spawn(move || load(request, &sink));
                        if let Err(err) = spawned {
                            log::error!("preview: download not started: {err}");
                        }
                    } else {
                        let loader = self
                            .loader
                            .get_or_insert_with(|| spawn_loader(self.sink.clone()));
                        let _ = loader.send(request);
                    }
                }
                None => {
                    log::info!("preview: unsupported image: {url}");
                    self.image_sizes.insert(url.clone(), ImageSize::Failed);
                }
            }
        }
        self.doc = Some(doc);
        true
    }

    pub fn image_loaded(&mut self, url: String, result: Result<Arc<ImageData>, String>) {
        match result {
            Ok(data) => {
                let (w, h) = data.natural;
                self.image_sizes.insert(url.clone(), ImageSize::Ready(w, h));
                self.images.insert(url, data);
            }
            Err(err) => {
                log::warn!("preview: image {url} not loaded: {err}");
                self.image_sizes.insert(url, ImageSize::Failed);
            }
        }
    }

    pub fn target_scroll(&self, panel_height: f32) -> f32 {
        let Some(doc) = &self.doc else { return 0.0 };
        let max = (doc.height - panel_height).max(0.0);
        if let Some(manual) = self.manual_scroll {
            return manual.clamp(0.0, max);
        }
        scroll_for_view(doc, self.view, max)
    }

    pub fn scroll_by(&mut self, current: f32, delta: f32, panel_height: f32) {
        let max = self
            .doc
            .as_ref()
            .map_or(0.0, |d| (d.height - panel_height).max(0.0));
        let from = self.manual_scroll.unwrap_or(current);
        self.manual_scroll = Some((from + delta).clamp(0.0, max));
    }
}

/// Keeps nvim's top line at the top of the panel, except over the last
/// screenful of the buffer where it eases toward the end of the document, so
/// both ends line up. The preview is usually taller than the source.
fn scroll_for_view(doc: &Document, (topline, botline, lines): (u32, u32, u32), max: f32) -> f32 {
    let aligned = doc
        .y_of_line(topline.saturating_sub(1) as f32)
        .clamp(0.0, max);
    let visible = botline.saturating_sub(topline) + 1;
    let below = lines.saturating_sub(botline);
    let toward_end = (1.0 - below as f32 / visible as f32).clamp(0.0, 1.0);
    // top of the file: just start from 0
    if topline <= 1 {
        return max * toward_end;
    }
    aligned + (max - aligned) * toward_end
}

/// Linear blend, t = 0 is bg and t = 1 is fg.
pub fn mix(fg: u32, bg: u32, t: f32) -> u32 {
    let c = |shift: u32| {
        let (f, b) = (((fg >> shift) & 0xff) as f32, ((bg >> shift) & 0xff) as f32);
        ((b + (f - b) * t).round() as u32) << shift
    };
    c(16) | c(8) | c(0)
}

fn spawn_worker(locale: String, db: Database, sink: Sink) -> Sender<Job> {
    let (tx, rx) = channel::<Job>();
    let spawned = std::thread::Builder::new()
        .name("markdown".into())
        .spawn(move || worker(locale, db, rx, sink));
    if let Err(err) = spawned {
        log::error!("preview: layout thread not started: {err}");
    }
    tx
}

fn worker(locale: String, db: Database, rx: Receiver<Job>, sink: Sink) {
    let mut fs = FontSystem::new_with_locale_and_db(locale, db);
    while let Ok(mut job) = rx.recv() {
        // skip to the newest job, the rest are stale
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        let started = std::time::Instant::now();
        let parsed = parse::parse(&job.text, &job.highlights);
        let doc = layout::layout(&mut fs, &parsed, &job.params);
        log::debug!(
            "preview: layout of {} bytes in {:?} ({} glyphs)",
            job.text.len(),
            started.elapsed(),
            doc.glyphs.len()
        );
        sink(PreviewMsg::Laid(job.generation, Arc::new(doc)));
    }
}

fn spawn_loader(sink: Sink) -> Sender<LoadRequest> {
    let (tx, rx) = channel::<LoadRequest>();
    let spawned = std::thread::Builder::new()
        .name("images".into())
        .spawn(move || {
            while let Ok(request) = rx.recv() {
                load(request, &sink);
            }
        });
    if let Err(err) = spawned {
        log::error!("preview: image thread not started: {err}");
    }
    tx
}

fn load(request: LoadRequest, sink: &Sink) {
    let started = std::time::Instant::now();
    let result = read_source(&request.source).and_then(|bytes| {
        decode_image(
            &bytes,
            request.max_width,
            request.scale,
            request.fonts.as_ref(),
        )
    });
    match &result {
        Ok(img) => log::debug!(
            "preview: {} loaded in {:?} ({}x{} → {}x{})",
            request.url,
            started.elapsed(),
            img.natural.0,
            img.natural.1,
            img.width,
            img.height
        ),
        Err(err) => log::debug!("preview: {}: {err}", request.url),
    }
    sink(PreviewMsg::Image(request.url, result.map(Arc::new)));
}

const MAX_DOWNLOAD: u64 = 20 << 20;

fn read_source(source: &ImageSource) -> Result<Vec<u8>, String> {
    match source {
        ImageSource::File(path) => {
            std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
        }
        // Shell out to curl (bundled since Windows 10) rather than pull an HTTP/TLS stack into the GUI.
        ImageSource::Web(url) => {
            let mut cmd = std::process::Command::new(if cfg!(windows) {
                "curl.exe"
            } else {
                "curl"
            });
            cmd.args(["-sSfL", "--max-time", "20", "--max-filesize"])
                .arg(MAX_DOWNLOAD.to_string())
                .arg("--")
                .arg(url)
                .stdin(std::process::Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "curl: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(out.stdout)
        }
    }
}

fn is_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let head = String::from_utf8_lossy(head);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with('<') && (head.contains("<svg") || head.starts_with("<?xml"))
}

fn decode_image(
    bytes: &[u8],
    max_width: u32,
    scale: f32,
    fonts: Option<&Arc<Database>>,
) -> Result<ImageData, String> {
    if is_svg(bytes) {
        return decode_svg(bytes, max_width, scale, fonts);
    }
    let image = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let natural = (image.width(), image.height());
    let image = if max_width > 0 && image.width() > max_width {
        let h = (image.height() as u64 * max_width as u64 / image.width() as u64).max(1) as u32;
        image.resize_exact(max_width, h, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    Ok(ImageData {
        natural,
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

/// Rendered at natural size × `scale` so it stays sharp on HiDPI, capped at `max_width`.
fn decode_svg(
    bytes: &[u8],
    max_width: u32,
    scale: f32,
    fonts: Option<&Arc<Database>>,
) -> Result<ImageData, String> {
    use resvg::{tiny_skia, usvg};
    let mut options = usvg::Options::default();
    if let Some(fonts) = fonts {
        options.fontdb = fonts.clone();
    }
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|e| e.to_string())?;
    let size = tree.size();
    let natural = (
        size.width().ceil().max(1.0) as u32,
        size.height().ceil().max(1.0) as u32,
    );
    let mut k = scale.max(0.1);
    if max_width > 0 && size.width() * k > max_width as f32 {
        k = max_width as f32 / size.width();
    }
    let (w, h) = (
        (size.width() * k).ceil().clamp(1.0, 8192.0) as u32,
        (size.height() * k).ceil().clamp(1.0, 8192.0) as u32,
    );
    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("SVG too large")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(k, k),
        &mut pixmap.as_mut(),
    );
    // tiny-skia is premultiplied, the texture upload wants straight alpha
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Ok(ImageData {
        natural,
        width: w,
        height: h,
        rgba,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum LinkTarget {
    Anchor(String),
    Web(String),
    /// Relative to the markdown file, opened in nvim
    File(String),
    Unsupported,
}

pub fn link_target(url: &str) -> LinkTarget {
    let url = url.trim();
    if let Some(anchor) = url.strip_prefix('#') {
        return LinkTarget::Anchor(percent_decode(anchor));
    }
    let lower = url.to_ascii_lowercase();
    if ["http://", "https://", "mailto:"]
        .iter()
        .any(|s| lower.starts_with(s))
    {
        return LinkTarget::Web(url.to_string());
    }
    if url.is_empty() || lower.contains("://") || lower.starts_with("data:") {
        return LinkTarget::Unsupported;
    }
    let path = url.split(['#', '?']).next().unwrap_or(url);
    match path.is_empty() {
        true => LinkTarget::Unsupported,
        false => LinkTarget::File(percent_decode(path)),
    }
}

pub fn open_url(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut cmd = std::process::Command::new("rundll32.exe");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    } else {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    match cmd.spawn() {
        Ok(mut child) => {
            // wait on a side thread so it doesn't linger as a zombie
            std::thread::spawn(move || child.wait());
        }
        Err(err) => log::warn!("preview: {url} not opened: {err}"),
    }
}

/// Image urls are relative to the markdown file. Under WSL (`distro` set) the
/// Linux path maps to `\\wsl.localhost\<distro>\...` on Windows.
pub fn resolve_image(md_path: &str, url: &str, distro: &str) -> Option<ImageSource> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(ImageSource::Web(url.to_string()));
    }
    resolve_file(md_path, url, distro).map(ImageSource::File)
}

fn resolve_file(md_path: &str, url: &str, distro: &str) -> Option<PathBuf> {
    let url = url.strip_prefix("file://").unwrap_or(url);
    if url.is_empty() || url.contains("://") || url.starts_with("data:") {
        return None;
    }
    let url = percent_decode(url.split(['?', '#']).next().unwrap_or(url));
    if distro.is_empty() {
        let path = std::path::Path::new(&url);
        return Some(if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::path::Path::new(md_path).parent()?.join(path)
        });
    }
    // normalize by hand, the path doesn't exist on this side so no canonicalize
    let joined = if url.starts_with('/') {
        url
    } else {
        let dir = md_path.rsplit_once('/').map_or("", |(d, _)| d);
        format!("{dir}/{url}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let linux = format!("/{}", parts.join("/"));
    Some(if cfg!(windows) {
        PathBuf::from(format!(
            r"\\wsl.localhost\{distro}{}",
            linux.replace('/', "\\")
        ))
    } else {
        PathBuf::from(linux)
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_notifications() {
        let text = parse_notification(vec![
            "text".into(),
            Value::Map(vec![
                ("path".into(), "/home/j/a.md".into()),
                ("distro".into(), "cachyos".into()),
                ("lines".into(), Value::Array(vec!["# a".into(), "b".into()])),
            ]),
        ]);
        assert_eq!(
            text,
            Some(PreviewEvent::Text {
                path: "/home/j/a.md".into(),
                distro: "cachyos".into(),
                text: "# a\nb\n".into(),
                highlights: Arc::new([]),
            })
        );
        assert_eq!(
            parse_notification(vec!["open".into(), true.into()]),
            Some(PreviewEvent::Open(true))
        );
        let view = parse_notification(vec![
            "view".into(),
            Value::Map(vec![
                ("topline".into(), 10.into()),
                ("botline".into(), 40.into()),
                ("lines".into(), 100.into()),
            ]),
        ]);
        assert_eq!(
            view,
            Some(PreviewEvent::View {
                topline: 10,
                botline: 40,
                lines: 100
            })
        );
        assert_eq!(parse_notification(vec!["boh".into()]), None);
    }

    #[test]
    fn resolves_image_paths() {
        let r = |md: &str, url: &str, distro: &str| {
            match resolve_image(md, url, distro)? {
                ImageSource::File(p) => Some(p.to_string_lossy().into_owned()),
                ImageSource::Web(u) => Some(u),
            }
        };
        let wsl = |linux: &str| {
            if cfg!(windows) {
                format!(r"\\wsl.localhost\cachyos{}", linux.replace('/', "\\"))
            } else {
                linux.to_string()
            }
        };
        assert_eq!(
            r("/home/j/docs/a.md", "img/b.png", "cachyos"),
            Some(wsl("/home/j/docs/img/b.png"))
        );
        assert_eq!(
            r("/home/j/docs/a.md", "../x%20y.png", "cachyos"),
            Some(wsl("/home/j/x y.png"))
        );
        assert_eq!(
            r("/home/j/docs/a.md", "/tmp/c.png", "cachyos"),
            Some(wsl("/tmp/c.png"))
        );
        assert_eq!(
            r("/home/j/a.md", "https://x.org/a.png", "cachyos"),
            Some("https://x.org/a.png".into())
        );
        assert_eq!(r("/home/j/a.md", "ftp://x.org/a.png", "cachyos"), None);
        assert_eq!(
            r("/home/j/a.md", "pic.png", ""),
            Some("/home/j/pic.png".into())
        );
    }

    #[test]
    fn scroll_follows_the_top_line_then_reaches_the_end() {
        let doc = Document {
            height: 3000.0,
            anchors: vec![(0, 0.0), (100, 1000.0), (200, 2000.0), (300, 3000.0)],
            ..Document::default()
        };
        let max = 2000.0;
        assert_eq!(scroll_for_view(&doc, (101, 150, 300), max), 1000.0);
        assert_eq!(scroll_for_view(&doc, (1, 50, 300), max), 0.0);
        // last line visible -> end of the document
        assert_eq!(scroll_for_view(&doc, (251, 300, 300), max), max);
        let mid = scroll_for_view(&doc, (226, 275, 300), max);
        assert!(mid > 2000.0 * 0.5 && mid <= max, "{mid}");
        // everything fits, nothing to scroll
        assert_eq!(scroll_for_view(&doc, (1, 10, 10), 0.0), 0.0);
    }

    #[test]
    fn link_targets() {
        assert_eq!(link_target("#My%20Title"), LinkTarget::Anchor("My Title".into()));
        assert_eq!(
            link_target("https://x.org/a?b#c"),
            LinkTarget::Web("https://x.org/a?b#c".into())
        );
        assert_eq!(
            link_target("docs/a%20b.md#intro"),
            LinkTarget::File("docs/a b.md".into())
        );
        assert_eq!(link_target("ftp://x"), LinkTarget::Unsupported);
    }

    #[test]
    fn decodes_svg_with_text() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="90" height="20">
            <rect width="90" height="20" fill="#4c1"/>
            <text x="5" y="14" font-family="sans-serif" font-size="11">build</text></svg>"##;
        assert!(is_svg(svg));
        let img = decode_image(svg, 1000, 2.0, None).unwrap();
        assert_eq!(img.natural, (90, 20));
        assert_eq!((img.width, img.height), (180, 40));
        assert_eq!(img.rgba.len(), 180 * 40 * 4);
        // opaque green, not premultiplied
        assert_eq!(&img.rgba[..4], &[0x44, 0xcc, 0x11, 0xff]);
        let small = decode_image(svg, 45, 2.0, None).unwrap();
        assert_eq!((small.width, small.height), (45, 10));
    }

    #[test]
    fn mixes_colors() {
        assert_eq!(mix(0xffffff, 0x000000, 0.5), 0x808080);
        assert_eq!(mix(0x102030, 0x102030, 0.3), 0x102030);
    }
}
