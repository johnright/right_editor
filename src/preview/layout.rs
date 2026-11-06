//! Lays out parsed blocks into a flat display list (glyphs, rects, images) in
//! pixels, y going down from the top of the document.
//!
//! Runs on the preview thread with its own `FontSystem`, but built from the
//! editor's font db. That's what keeps the `CacheKey`s valid for the
//! renderer's rasterizer and atlas.

use std::collections::HashMap;

use cosmic_text::fontdb::{Family, Style as FontStyle, Weight};
use cosmic_text::{
    Align as TextAlign, Attrs, Buffer, CacheKey, CacheKeyFlags, Color, FontSystem, Metrics,
    Shaping, Wrap,
};

use super::parse::{
    Alert, Align, Block, BlockKind, ListItem, Parsed, Span, Style, footnote_anchor, slug,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub fg: u32,
    pub bg: u32,
    pub headings: [u32; 6],
    pub link: u32,
    pub code: u32,
    pub code_bg: u32,
    pub quote: u32,
    pub muted: u32,
    pub rule: u32,
    /// note, tip, important, warning, caution
    pub alerts: [u32; 5],
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            fg: 0xcdd6f4,
            bg: 0x1e1e2e,
            headings: [0x89b4fa; 6],
            link: 0x89dceb,
            code: 0xa6e3a1,
            code_bg: 0x313244,
            quote: 0x9399b2,
            muted: 0x7f849c,
            rule: 0x45475a,
            alerts: [0x89b4fa, 0xa6e3a1, 0xcba6f7, 0xf9e2af, 0xf38ba8],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSize {
    Ready(u32, u32),
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub width: f32,
    /// device scale; image sizes are in logical px
    pub scale: f32,
    pub size: f32,
    pub family: Option<String>,
    pub mono: Option<String>,
    pub hinting: bool,
    pub theme: Theme,
    pub images: HashMap<String, ImageSize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    /// pen position on the baseline, whole pixels
    pub x: f32,
    pub y: f32,
    pub key: CacheKey,
    pub color: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub url: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Document {
    pub background: u32,
    pub divider: u32,
    pub height: f32,
    /// sorted by y so `glyph_range` can bisect
    pub glyphs: Vec<Glyph>,
    pub rects: Vec<Rect>,
    pub images: Vec<Image>,
    pub links: Vec<Link>,
    pub targets: Vec<(String, f32)>,
    /// (source line, y), both non-decreasing
    pub anchors: Vec<(u32, f32)>,
    /// Referenced but not in `Params::images` yet. The caller loads them.
    pub missing_images: Vec<String>,
}

impl Document {
    /// Linear interpolation between anchors; `line` may be fractional.
    pub fn y_of_line(&self, line: f32) -> f32 {
        let a = &self.anchors;
        let Some(&(first_line, first_y)) = a.first() else {
            return 0.0;
        };
        if line <= first_line as f32 {
            return first_y * (line / first_line.max(1) as f32).clamp(0.0, 1.0);
        }
        let i = a.partition_point(|&(l, _)| l as f32 <= line);
        if i >= a.len() {
            return a.last().unwrap().1;
        }
        let (l0, y0) = a[i - 1];
        let (l1, y1) = a[i];
        let t = (line - l0 as f32) / (l1 - l0).max(1) as f32;
        y0 + (y1 - y0) * t.clamp(0.0, 1.0)
    }

    /// Inverse of `y_of_line`, 0-based.
    pub fn line_at(&self, y: f32) -> u32 {
        let a = &self.anchors;
        let i = a.partition_point(|&(_, ay)| ay <= y);
        if i == 0 {
            return 0;
        }
        if i >= a.len() {
            return a.last().map_or(0, |&(l, _)| l.saturating_sub(1));
        }
        let (l0, y0) = a[i - 1];
        let (l1, y1) = a[i];
        let t = if y1 > y0 { (y - y0) / (y1 - y0) } else { 0.0 };
        // a block's end anchor is exclusive
        (l0 + ((l1 - l0) as f32 * t) as u32).min(l1.saturating_sub(1).max(l0))
    }

    pub fn link_at(&self, x: f32, y: f32) -> Option<&str> {
        self.links
            .iter()
            .find(|l| x >= l.x && x < l.x + l.w && y >= l.y && y < l.y + l.h)
            .map(|l| l.url.as_str())
    }

    pub fn target(&self, anchor: &str) -> Option<f32> {
        let anchor = anchor.to_lowercase();
        self.targets
            .iter()
            .find(|(name, _)| *name == anchor)
            .map(|&(_, y)| y)
    }

    pub fn glyph_range(&self, top: f32, bottom: f32) -> std::ops::Range<usize> {
        let start = self.glyphs.partition_point(|g| g.y < top);
        let end = self.glyphs.partition_point(|g| g.y < bottom);
        start..end.max(start)
    }
}

/// Glyph metadata bits. The link index + 1 sits above `META_LINK_SHIFT`,
/// 0 means no link.
const META_CODE: usize = 1;
const META_STRIKE: usize = 2;
const META_LINK_SHIFT: usize = 2;

struct Layout<'a> {
    fs: &'a mut FontSystem,
    buffer: Buffer,
    p: &'a Params,
    links: &'a [String],
    doc: Document,
    y: f32,
    em: f32,
}

#[derive(Clone, Copy)]
struct TextStyle {
    size: f32,
    line_height: f32,
    color: u32,
    bold: bool,
    italic: bool,
    mono: bool,
    align: Align,
}

struct RowImage<'s> {
    url: &'s str,
    alt: &'s str,
    link: Option<u32>,
    w: f32,
    h: f32,
    ready: bool,
}

pub fn layout(fs: &mut FontSystem, parsed: &Parsed, p: &Params) -> Document {
    let em = p.size;
    let buffer = Buffer::new(fs, Metrics::new(em, em * 1.5));
    let mut l = Layout {
        fs,
        buffer,
        p,
        links: &parsed.links,
        doc: Document::default(),
        y: (em * 1.0).round(),
        em,
    };
    let pad = (em * 1.5).round();
    l.blocks(&parsed.blocks, pad, (p.width - 2.0 * pad).max(em * 4.0));
    l.doc.height = l.y + em * 2.0;
    l.doc.background = p.theme.bg;
    l.doc.divider = p.theme.rule;
    l.doc.glyphs.sort_by(|a, b| a.y.total_cmp(&b.y));
    // nested blocks can repeat a line, force both coordinates monotonic
    l.doc.anchors.sort_by_key(|&(line, _)| line);
    let mut max_y = 0.0f32;
    for (_, y) in &mut l.doc.anchors {
        max_y = max_y.max(*y);
        *y = max_y;
    }
    l.doc.anchors.dedup_by_key(|&mut (line, _)| line);
    l.doc
}

fn plain_text(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|s| match s {
            Span::Text(t, _) => t.as_str(),
            Span::Image { alt, .. } => alt.as_str(),
        })
        .collect()
}

impl Layout<'_> {
    fn body(&self) -> TextStyle {
        TextStyle {
            size: self.em,
            line_height: (self.em * 1.5).round(),
            color: self.p.theme.fg,
            bold: false,
            italic: false,
            mono: false,
            align: Align::Left,
        }
    }

    fn anchor(&mut self, line: u32) {
        self.doc.anchors.push((line, self.y));
    }

    fn blocks(&mut self, blocks: &[Block], x: f32, width: f32) {
        self.blocks_styled(blocks, x, width, self.body(), false);
    }

    /// `tight` = list item contents, smaller gaps between blocks
    fn blocks_styled(
        &mut self,
        blocks: &[Block],
        x: f32,
        width: f32,
        style: TextStyle,
        tight: bool,
    ) {
        for (i, block) in blocks.iter().enumerate() {
            if i > 0 {
                self.y += if tight {
                    (self.em * 0.25).round()
                } else {
                    self.gap_before(block)
                };
            }
            self.anchor(block.lines.start);
            let style = TextStyle {
                align: if block.center {
                    Align::Center
                } else {
                    style.align
                },
                ..style
            };
            self.block(block, x, width, style);
            self.anchor(block.lines.end);
        }
    }

    fn gap_before(&self, block: &Block) -> f32 {
        let em = self.em;
        match block.kind {
            BlockKind::Heading(level, _) if level <= 2 => (em * 1.5).round(),
            BlockKind::Heading(..) => (em * 1.25).round(),
            _ => (em * 0.85).round(),
        }
    }

    fn block(&mut self, block: &Block, x: f32, width: f32, style: TextStyle) {
        let em = self.em;
        let theme = self.p.theme;
        match &block.kind {
            BlockKind::Paragraph(spans) => {
                if let Some(images) = only_images(spans) {
                    self.image_rows(&images, x, width, style.align == Align::Center);
                    return;
                }
                // "**Title**<br>\n<img ...>" is common in READMEs: lines holding only
                // images get drawn as images, the rest as text
                let lines = split_lines(spans);
                if lines.len() < 2 || lines.iter().all(|l| only_images(l).is_none()) {
                    self.y += self.text(spans, x, self.y, width, style);
                    return;
                }
                let gap = (em * 0.25).round();
                let mut text: Vec<Span> = Vec::new();
                let mut first = true;
                let flush = |this: &mut Self, text: &mut Vec<Span>, first: &mut bool| {
                    if text.is_empty() {
                        return;
                    }
                    if !*first {
                        this.y += gap;
                    }
                    *first = false;
                    this.y += this.text(text, x, this.y, width, style);
                    text.clear();
                };
                for line in &lines {
                    if let Some(images) = only_images(line) {
                        flush(self, &mut text, &mut first);
                        if !first {
                            self.y += gap;
                        }
                        first = false;
                        self.image_rows(&images, x, width, style.align == Align::Center);
                    } else {
                        if !text.is_empty() {
                            text.push(Span::Text("\n".into(), Style::default()));
                        }
                        text.extend(line.iter().cloned());
                    }
                }
                flush(self, &mut text, &mut first);
            }
            BlockKind::Heading(level, spans) => {
                let factor = match level {
                    1 => 2.0,
                    2 => 1.5,
                    3 => 1.25,
                    4 => 1.0,
                    5 => 0.875,
                    _ => 0.85,
                };
                let size = (em * factor).round();
                let heading = TextStyle {
                    size,
                    line_height: (size * 1.25).round(),
                    color: theme.headings[(*level as usize).clamp(1, 6) - 1],
                    bold: true,
                    ..style
                };
                self.doc.targets.push((slug(&plain_text(spans)), self.y));
                self.y += self.text(spans, x, self.y, width, heading);
                if *level <= 2 {
                    self.y += (em * 0.3).round();
                    self.rect(
                        x,
                        self.y,
                        width,
                        1.0f32.max((em / 16.0).round()),
                        theme.rule,
                    );
                    self.y += 1.0;
                }
            }
            BlockKind::Code { spans, .. } => {
                let pad = (em * 0.75).round();
                let top = self.y;
                let code = TextStyle {
                    size: (em * 0.875).round(),
                    line_height: (em * 0.875 * 1.45).round(),
                    color: theme.fg,
                    bold: false,
                    italic: false,
                    mono: true,
                    align: Align::Left,
                };
                let rect_index = self.doc.rects.len();
                // push the background now, patch its height once the text is laid out
                self.rect(x, top, width, 0.0, theme.code_bg);
                let h = if spans.is_empty() {
                    code.line_height
                } else {
                    self.text(spans, x + pad, top + pad, width - 2.0 * pad, code)
                };
                let total = h + 2.0 * pad;
                self.doc.rects[rect_index].h = total;
                self.y = top + total;
            }
            BlockKind::Quote(alert, blocks) => {
                let top = self.y;
                let bar = (em * 0.25).round();
                let indent = (em * 1.0).round();
                let (bar_color, text_color) = match alert {
                    Some(a) => (theme.alerts[*a as usize], style.color),
                    None => (theme.rule, theme.quote),
                };
                if let Some(alert) = alert {
                    let title = match alert {
                        Alert::Note => "Note",
                        Alert::Tip => "Tip",
                        Alert::Important => "Important",
                        Alert::Warning => "Warning",
                        Alert::Caution => "Caution",
                    };
                    let title_style = TextStyle {
                        color: bar_color,
                        bold: true,
                        ..style
                    };
                    let span = [Span::Text(title.to_string(), Style::default())];
                    self.y += self.text(&span, x + bar + indent, self.y, width, title_style);
                    if !blocks.is_empty() {
                        self.y += (em * 0.25).round();
                    }
                }
                let quote = TextStyle {
                    color: text_color,
                    ..style
                };
                self.blocks_styled(blocks, x + bar + indent, width - bar - indent, quote, false);
                self.rect(x, top, bar, self.y - top, bar_color);
            }
            BlockKind::List { start, items } => self.list(*start, items, x, width, style),
            BlockKind::Table { aligns, head, rows } => {
                self.table(aligns, head, rows, x, width, style)
            }
            BlockKind::Rule => {
                let h = (em / 8.0).round().max(2.0);
                self.y += (em * 0.5).round();
                self.rect(x, self.y, width, h, theme.rule);
                self.y += h + (em * 0.5).round();
            }
            BlockKind::Footnote(label, blocks) => {
                self.doc.targets.push((footnote_anchor(label), self.y));
                let size = (em * 0.875).round();
                let small = TextStyle {
                    size,
                    line_height: (size * 1.5).round(),
                    ..style
                };
                let marker = TextStyle {
                    color: theme.muted,
                    ..small
                };
                let label = format!("{label}.");
                let label_w = self.measure(&[Span::Text(label.clone(), Style::default())], marker);
                let gap = (em * 0.5).round();
                let indent = (em * 2.0).round().max(label_w + gap);
                self.marker(&label, x + indent - gap, self.y, marker);
                self.blocks_styled(blocks, x + indent, width - indent, small, true);
            }
            BlockKind::Indent(blocks) => {
                let indent = (em * 1.6).round();
                self.blocks_styled(blocks, x + indent, width - indent, style, true);
            }
        }
    }

    fn list(
        &mut self,
        start: Option<u64>,
        items: &[ListItem],
        x: f32,
        width: f32,
        style: TextStyle,
    ) {
        let em = self.em;
        let indent = (em * 1.6).round();
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.y += (em * 0.25).round();
            }
            let top = self.y;
            let line_height = style.line_height;
            if let Some(checked) = item.task {
                let side = (em * 0.85).round();
                let bx = (x + (indent - side) / 2.0 - em * 0.2).round();
                let by = top + ((line_height - side) / 2.0).round();
                let stroke = (em / 12.0).round().max(1.0);
                let theme = self.p.theme;
                if checked {
                    self.rect(bx, by, side, side, theme.link);
                    let check = TextStyle {
                        size: (side * 0.85).round(),
                        line_height: side,
                        color: theme.bg,
                        bold: true,
                        align: Align::Center,
                        ..style
                    };
                    let span = [Span::Text("✓".into(), Style::default())];
                    self.text(&span, bx, by, side, check);
                } else {
                    let color = theme.muted;
                    self.rect(bx, by, side, stroke, color);
                    self.rect(bx, by + side - stroke, side, stroke, color);
                    self.rect(bx, by, stroke, side, color);
                    self.rect(bx + side - stroke, by, stroke, side, color);
                }
            } else {
                let marker = match start {
                    Some(n) => format!("{}.", n + i as u64),
                    None => "•".to_string(),
                };
                let marker_style = TextStyle {
                    color: self.p.theme.muted,
                    align: Align::Left,
                    ..style
                };
                self.marker(&marker, x + indent - (em * 0.5).round(), top, marker_style);
            }
            let item_style = TextStyle {
                color: if item.task == Some(true) {
                    self.p.theme.muted
                } else {
                    style.color
                },
                ..style
            };
            self.blocks_styled(&item.blocks, x + indent, width - indent, item_style, true);
            if self.y < top + line_height {
                self.y = top + line_height;
            }
        }
    }

    fn table(
        &mut self,
        aligns: &[Align],
        head: &[Vec<Span>],
        rows: &[Vec<Vec<Span>>],
        x: f32,
        width: f32,
        style: TextStyle,
    ) {
        let em = self.em;
        let theme = self.p.theme;
        let cols = aligns.len().max(head.len()).max(1);
        let pad = (em * 0.6).round();
        let border = 1.0f32.max((em / 16.0).round());
        let style = TextStyle {
            align: Align::Left,
            ..style
        };
        let head_style = TextStyle {
            bold: true,
            ..style
        };
        // natural widths, shrunk proportionally if the table doesn't fit
        let mut natural = vec![em; cols];
        let all_rows =
            std::iter::once((head, head_style)).chain(rows.iter().map(|r| (&r[..], style)));
        for (row, st) in all_rows.clone() {
            for (c, cell) in row.iter().enumerate().take(cols) {
                natural[c] = natural[c].max(self.measure(cell, st) + 2.0 * pad);
            }
        }
        let total: f32 = natural.iter().sum();
        let widths: Vec<f32> = if total > width {
            natural
                .iter()
                .map(|w| (w * width / total).floor())
                .collect()
        } else {
            natural
        };
        let table_w: f32 = widths.iter().sum();
        let top = self.y;
        self.rect(x, top, table_w, border, theme.rule);
        self.y += border;
        for (r, (row, st)) in all_rows.enumerate() {
            let row_top = self.y;
            let bg_index = self.doc.rects.len();
            if r == 0 {
                self.rect(x, row_top, table_w, 0.0, theme.code_bg);
            }
            let mut height = 0.0f32;
            let mut cx = x;
            for (c, w) in widths.iter().enumerate() {
                if let Some(cell) = row.get(c) {
                    let cell_style = TextStyle {
                        align: aligns.get(c).copied().unwrap_or(Align::Left),
                        ..st
                    };
                    let h = self.text(
                        cell,
                        cx + pad,
                        row_top + pad * 0.6,
                        w - 2.0 * pad,
                        cell_style,
                    );
                    height = height.max(h);
                }
                cx += w;
            }
            let row_h = (height + 2.0 * pad * 0.6).round().max(st.line_height);
            if r == 0 {
                self.doc.rects[bg_index].h = row_h;
            }
            self.y = row_top + row_h;
            self.rect(x, self.y, table_w, border, theme.rule);
            self.y += border;
        }
        let mut cx = x;
        for w in std::iter::once(0.0).chain(widths.iter().copied()) {
            cx += w;
            let lx = if w == 0.0 { x } else { cx - border };
            self.rect(lx, top, border, self.y - top, theme.rule);
        }
    }

    fn image_rows(&mut self, spans: &[&Span], x: f32, width: f32, center: bool) {
        let em = self.em;
        let scale = self.p.scale;
        let gap = (em * 0.25).round();
        let pad = (em * 0.5).round();
        let placeholder = TextStyle {
            color: self.p.theme.muted,
            italic: true,
            ..self.body()
        };
        let mut images = Vec::new();
        for span in spans {
            let Span::Image {
                url,
                alt,
                link,
                width: want_w,
                height: want_h,
            } = span
            else {
                continue;
            };
            let state = self.p.images.get(url.as_str()).copied();
            let (w, h, ready) = match state {
                Some(ImageSize::Ready(nw, nh)) => {
                    let (nw, nh) = (nw.max(1) as f32, nh.max(1) as f32);
                    let (w, h) = match (want_w, want_h) {
                        (Some(w), Some(h)) => (*w as f32, *h as f32),
                        (Some(w), None) => (*w as f32, *w as f32 * nh / nw),
                        (None, Some(h)) => (*h as f32 * nw / nh, *h as f32),
                        (None, None) => (nw, nh),
                    };
                    let (w, h) = (w * scale, h * scale);
                    let k = (width / w).min(1.0);
                    ((w * k).round().max(1.0), (h * k).round().max(1.0), true)
                }
                _ => {
                    if state.is_none() && !self.doc.missing_images.iter().any(|u| u == url) {
                        self.doc.missing_images.push(url.clone());
                    }
                    let label = placeholder_label(url, alt, state.is_some());
                    let span = [Span::Text(label, Style::default())];
                    let w = (self.measure(&span, placeholder) + 2.0 * pad).min(width);
                    let h = self.height_of(&span, w - 2.0 * pad, placeholder) + 2.0 * pad;
                    (w, h, false)
                }
            };
            images.push(RowImage {
                url,
                alt,
                link: *link,
                w,
                h,
                ready,
            });
        }
        let mut rows: Vec<std::ops::Range<usize>> = Vec::new();
        let mut start = 0;
        let mut row_w = 0.0;
        for (i, img) in images.iter().enumerate() {
            if i > start && row_w + gap + img.w > width {
                rows.push(start..i);
                start = i;
                row_w = 0.0;
            }
            row_w += if i > start { gap } else { 0.0 } + img.w;
        }
        if start < images.len() {
            rows.push(start..images.len());
        }
        for (r, range) in rows.into_iter().enumerate() {
            if r > 0 {
                self.y += gap;
            }
            let row = &images[range];
            let row_w: f32 = row.iter().map(|i| i.w).sum::<f32>() + gap * (row.len() - 1) as f32;
            let row_h = row.iter().map(|i| i.h).fold(0.0, f32::max);
            let mut ix = if center {
                x + ((width - row_w) / 2.0).max(0.0).round()
            } else {
                x
            };
            for img in row {
                // bottom-aligned within the row, like inline images
                let iy = self.y + row_h - img.h;
                if img.ready {
                    self.doc.images.push(Image {
                        x: ix,
                        y: iy,
                        w: img.w,
                        h: img.h,
                        url: img.url.to_string(),
                    });
                } else {
                    self.rect(ix, iy, img.w, img.h, self.p.theme.code_bg);
                    let failed = self.p.images.contains_key(img.url);
                    let label = placeholder_label(img.url, img.alt, failed);
                    let span = [Span::Text(label, Style::default())];
                    self.text(&span, ix + pad, iy + pad, img.w - 2.0 * pad, placeholder);
                }
                if let Some(url) = img.link.and_then(|l| self.links.get(l as usize)) {
                    self.doc.links.push(Link {
                        x: ix,
                        y: iy,
                        w: img.w,
                        h: img.h,
                        url: url.clone(),
                    });
                }
                ix += img.w + gap;
            }
            self.y += row_h;
        }
    }

    fn marker(&mut self, text: &str, right: f32, top: f32, st: TextStyle) {
        let span = [Span::Text(text.to_string(), Style::default())];
        let w = self.measure(&span, st);
        self.text(&span, (right - w).round(), top, w + 1.0, st);
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: u32) {
        self.doc.rects.push(Rect { x, y, w, h, color });
    }

    fn attrs<'f>(
        &self,
        family: &'f Option<String>,
        mono: bool,
        st: TextStyle,
        s: Style,
    ) -> Attrs<'f> {
        let family = match family {
            Some(name) => Family::Name(name),
            None if mono => Family::Monospace,
            None => Family::SansSerif,
        };
        let theme = self.p.theme;
        let color = if let Some(color) = s.color {
            color
        } else if s.link.is_some() {
            theme.link
        } else if s.code && !st.mono {
            theme.code
        } else {
            st.color
        };
        let meta = (if s.code { META_CODE } else { 0 })
            | (if s.strike { META_STRIKE } else { 0 })
            | (s.link.map_or(0, |l| l as usize + 1) << META_LINK_SHIFT);
        let size = if s.code && !st.mono {
            (st.size * 0.9).round()
        } else {
            st.size
        };
        Attrs::new()
            .family(family)
            .weight(if st.bold || s.bold {
                Weight(700)
            } else {
                Weight(400)
            })
            .style(if st.italic || s.italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            })
            .color(Color(0xff00_0000 | color))
            .metadata(meta)
            .metrics(Metrics::new(size, st.line_height))
            .cache_key_flags(if self.p.hinting {
                CacheKeyFlags::empty()
            } else {
                CacheKeyFlags::DISABLE_HINTING
            })
    }

    fn prepare(&mut self, spans: &[Span], width: Option<f32>, st: TextStyle) {
        let p = self.p;
        let family = if st.mono { &p.mono } else { &p.family };
        let base = self.attrs(family, st.mono, st, Style::default());
        let mut rich: Vec<(String, Attrs)> = Vec::with_capacity(spans.len());
        for span in spans {
            match span {
                Span::Text(text, s) => {
                    let fam = if s.code || st.mono {
                        &p.mono
                    } else {
                        &p.family
                    };
                    rich.push((text.clone(), self.attrs(fam, s.code || st.mono, st, *s)));
                }
                Span::Image { url, alt, link, .. } => {
                    let label = if alt.is_empty() { url } else { alt };
                    let s = Style {
                        italic: true,
                        link: *link,
                        ..Style::default()
                    };
                    let muted = TextStyle {
                        color: p.theme.muted,
                        ..st
                    };
                    rich.push((format!("[{label}]"), self.attrs(family, st.mono, muted, s)));
                }
            }
        }
        let align = match st.align {
            Align::Left => None,
            Align::Center => Some(TextAlign::Center),
            Align::Right => Some(TextAlign::Right),
        };
        self.buffer
            .set_metrics(Metrics::new(st.size, st.line_height));
        self.buffer.set_wrap(if width.is_some() {
            Wrap::WordOrGlyph
        } else {
            Wrap::None
        });
        self.buffer.set_size(width, None);
        self.buffer.set_rich_text(
            rich.iter().map(|(t, a)| (t.as_str(), a.clone())),
            &base,
            Shaping::Advanced,
            align,
        );
        self.buffer.shape_until_scroll(self.fs, false);
    }

    fn measure(&mut self, spans: &[Span], st: TextStyle) -> f32 {
        self.prepare(spans, None, st);
        self.buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max)
            .ceil()
    }

    fn height_of(&mut self, spans: &[Span], width: f32, st: TextStyle) -> f32 {
        self.prepare(spans, Some(width.max(1.0)), st);
        self.buffer
            .layout_runs()
            .map(|run| run.line_top + run.line_height)
            .fold(0.0, f32::max)
            .ceil()
    }

    /// Wrapped text with its top-left at (x, top). Returns the height.
    fn text(&mut self, spans: &[Span], x: f32, top: f32, width: f32, st: TextStyle) -> f32 {
        self.prepare(spans, Some(width.max(1.0)), st);
        let mut height = 0.0f32;
        let mut glyphs = Vec::new();
        let mut rects = Vec::new();
        let mut links = Vec::new();
        let code_bg = self.p.theme.code_bg;
        for run in self.buffer.layout_runs() {
            let baseline = (top + run.line_y).round();
            height = height.max(run.line_top + run.line_height);
            // underline / strike / inline code bg, per run of glyphs with the same metadata
            let mut i = 0;
            while i < run.glyphs.len() {
                let meta = run.glyphs[i].metadata;
                let mut j = i;
                while j < run.glyphs.len() && run.glyphs[j].metadata == meta {
                    j += 1;
                }
                if meta != 0 {
                    let first = &run.glyphs[i];
                    let last = &run.glyphs[j - 1];
                    let (x0, x1) = (x + first.x, x + last.x + last.w);
                    let size = first.font_size;
                    let stroke = (size / 14.0).round().max(1.0);
                    let color = first.color_opt.map_or(st.color, |c| c.0 & 0xff_ffff);
                    if meta & META_CODE != 0 && !st.mono {
                        let pad = (size * 0.2).round();
                        rects.push(Rect {
                            x: (x0 - pad).round(),
                            y: (top + run.line_top + (run.line_height - size * 1.3) / 2.0).round(),
                            w: (x1 - x0 + 2.0 * pad).round(),
                            h: (size * 1.3).round(),
                            color: code_bg,
                        });
                    }
                    let link = meta >> META_LINK_SHIFT;
                    if link != 0 {
                        rects.push(Rect {
                            x: x0.round(),
                            y: baseline + (size * 0.12).round(),
                            w: (x1 - x0).round(),
                            h: stroke,
                            color,
                        });
                        if let Some(url) = self.links.get(link - 1) {
                            links.push(Link {
                                x: x0,
                                y: top + run.line_top,
                                w: x1 - x0,
                                h: run.line_height,
                                url: url.clone(),
                            });
                        }
                    }
                    if meta & META_STRIKE != 0 {
                        rects.push(Rect {
                            x: x0.round(),
                            y: baseline - (size * 0.3).round(),
                            w: (x1 - x0).round(),
                            h: stroke,
                            color,
                        });
                    }
                }
                i = j;
            }
            for g in run.glyphs {
                let phys = g.physical((x, baseline), 1.0);
                glyphs.push(Glyph {
                    x: phys.x as f32,
                    y: phys.y as f32,
                    key: phys.cache_key,
                    color: g.color_opt.map_or(st.color, |c| c.0 & 0xff_ffff),
                });
            }
        }
        self.doc.rects.extend(rects);
        self.doc.glyphs.extend(glyphs);
        self.doc.links.extend(links);
        height.ceil()
    }
}

fn placeholder_label(url: &str, alt: &str, failed: bool) -> String {
    let label = if alt.is_empty() { url } else { alt };
    if failed {
        format!("⚠ {label}")
    } else {
        label.to_string()
    }
}

fn split_lines(spans: &[Span]) -> Vec<Vec<Span>> {
    let mut lines = vec![Vec::new()];
    for span in spans {
        match span {
            Span::Text(text, style) => {
                for (i, piece) in text.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Vec::new());
                    }
                    let line = lines.last_mut().expect("never empty");
                    let piece = if line.is_empty() { piece.trim_start() } else { piece };
                    if !piece.is_empty() {
                        line.push(Span::Text(piece.to_string(), *style));
                    }
                }
            }
            Span::Image { .. } => lines.last_mut().expect("never empty").push(span.clone()),
        }
    }
    for line in &mut lines {
        if let Some(Span::Text(text, _)) = line.last_mut() {
            let len = text.trim_end().len();
            text.truncate(len);
        }
    }
    lines.retain(|l| !l.is_empty());
    lines
}

fn only_images(spans: &[Span]) -> Option<Vec<&Span>> {
    let mut images = Vec::new();
    for span in spans {
        match span {
            Span::Image { .. } => images.push(span),
            Span::Text(t, _) if t.trim().is_empty() => {}
            Span::Text(..) => return None,
        }
    }
    (!images.is_empty()).then_some(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::parse::parse;

    fn params(images: HashMap<String, ImageSize>) -> Params {
        Params {
            width: 600.0,
            scale: 1.0,
            size: 16.0,
            family: None,
            mono: None,
            hinting: false,
            theme: Theme::default(),
            images,
        }
    }

    fn sample() -> &'static str {
        "# Title\n\nA paragraph with **bold**, `code` and a [link](x).\n\n\
         - one\n- two\n  - nested\n\n> quote\n\n```\ncode\nblock\n```\n\n\
         | a | b |\n|---|---|\n| 1 | 2 |\n\n![pic](img.png)\n\n---\n\nend\n"
    }

    fn lay(md: &str, images: HashMap<String, ImageSize>) -> Document {
        let mut fs = FontSystem::new();
        layout(&mut fs, &parse(md, &[]), &params(images))
    }

    #[test]
    fn anchors_and_glyphs_are_ordered() {
        let doc = lay(sample(), HashMap::new());
        assert!(doc.height > 200.0, "{}", doc.height);
        assert!(
            doc.anchors
                .windows(2)
                .all(|w| w[0].0 < w[1].0 && w[0].1 <= w[1].1)
        );
        assert!(doc.glyphs.windows(2).all(|w| w[0].y <= w[1].y));
        assert_eq!(doc.missing_images, vec!["img.png".to_string()]);
        let ys: Vec<f32> = (0..30).map(|l| doc.y_of_line(l as f32)).collect();
        assert!(ys.windows(2).all(|w| w[0] <= w[1]), "{ys:?}");
        assert!(*ys.last().unwrap() <= doc.height);
        for w in doc.anchors.windows(2) {
            let ((line, y), (_, next_y)) = (w[0], w[1]);
            if next_y > y {
                assert_eq!(doc.line_at(y + 0.5), line, "{:?}", doc.anchors);
            }
        }
        assert_eq!(doc.target("title"), Some(16.0));
    }

    #[test]
    fn text_wraps_inside_the_panel() {
        let long = "word ".repeat(200);
        let doc = lay(&long, HashMap::new());
        if doc.glyphs.is_empty() {
            return; // no fonts on this machine
        }
        let pad = 24.0;
        assert!(
            doc.glyphs
                .iter()
                .all(|g| g.x >= pad - 1.0 && g.x <= 600.0 - pad)
        );
        assert!(doc.height > 100.0);
    }

    #[test]
    fn loaded_images_are_scaled_to_the_width() {
        let images = HashMap::from([("img.png".to_string(), ImageSize::Ready(2000, 1000))]);
        let doc = lay("![pic](img.png)\n", images);
        assert!(doc.missing_images.is_empty());
        let img = &doc.images[0];
        assert_eq!((img.w, img.h), (552.0, 276.0));
        let small = HashMap::from([("img.png".to_string(), ImageSize::Ready(100, 50))]);
        let doc = lay("![pic](img.png)\n", small);
        assert_eq!((doc.images[0].w, doc.images[0].h), (100.0, 50.0));
    }

    #[test]
    fn images_after_a_line_break_are_drawn_as_images() {
        let text = "**Languages**<br>\n<img src=\"a.svg\" alt=\"A, B\">\n";
        let doc = lay(text, HashMap::from([("a.svg".to_string(), ImageSize::Ready(100, 20))]));
        assert_eq!(doc.images.len(), 1);
        assert!(doc.images[0].y > 0.0, "the image goes below the title");
    }

    #[test]
    fn badges_share_a_row_and_are_links() {
        let images = HashMap::from([
            ("a.svg".to_string(), ImageSize::Ready(80, 20)),
            ("b.svg".to_string(), ImageSize::Ready(100, 20)),
        ]);
        let md = "[![a](a.svg)](https://a.org) ![b](b.svg)\n";
        let doc = lay(md, images);
        assert_eq!(doc.images.len(), 2);
        assert_eq!(doc.images[0].y, doc.images[1].y);
        assert_eq!(doc.images[1].x, doc.images[0].x + 80.0 + 4.0);
        let (x, y) = (doc.images[0].x + 5.0, doc.images[0].y + 5.0);
        assert_eq!(doc.link_at(x, y), Some("https://a.org"));
        assert_eq!(doc.link_at(doc.images[1].x + 5.0, y), None);
    }

    #[test]
    fn text_links_are_clickable() {
        let doc = lay("Go [there](#title) now.\n\n# Title\n", HashMap::new());
        if doc.glyphs.is_empty() {
            return;
        }
        let link = &doc.links[0];
        assert_eq!(link.url, "#title");
        assert_eq!(doc.link_at(link.x + 1.0, link.y + 1.0), Some("#title"));
        assert!(doc.target("Title").unwrap() > link.y);
    }
}
