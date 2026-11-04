//! Markdown to a small block tree for the layout.
//!
//! Blocks keep their source line range (0-based, end exclusive) so the
//! preview can follow nvim's scroll. Also copes with the bit of HTML READMEs
//! tend to use: images, links, headings, `align`, `<kbd>`.

use std::collections::HashMap;
use std::ops::Range;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, Options, Parser, Tag, TagEnd,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<u32>,
    /// 0xRRGGBB, code blocks only
    pub color: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Span {
    Text(String, Style),
    /// Drawn as an image only when the paragraph holds nothing but images,
    /// otherwise we fall back to the alt text.
    Image {
        url: String,
        alt: String,
        link: Option<u32>,
        /// from HTML `width`/`height`, px
        width: Option<u32>,
        height: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// `> [!NOTE]` and friends
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind {
    Paragraph(Vec<Span>),
    Heading(u8, Vec<Span>),
    Code {
        lang: String,
        spans: Vec<Span>,
    },
    Quote(Option<Alert>, Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    Table {
        aligns: Vec<Align>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
    },
    Rule,
    Footnote(String, Vec<Block>),
    /// body of a definition list entry
    Indent(Vec<Block>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub kind: BlockKind,
    pub lines: Range<u32>,
    pub center: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub blocks: Vec<Block>,
    pub links: Vec<String>,
}

/// From nvim. 0-based rows, byte columns, end exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub color: u32,
    pub bold: bool,
    pub italic: bool,
}

pub fn footnote_anchor(label: &str) -> String {
    format!("fn-{}", slug(label))
}

/// Same anchors GitHub generates: lowercased, spaces to dashes, punctuation dropped.
pub fn slug(text: &str) -> String {
    text.trim()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn parse(text: &str, highlights: &[Highlight]) -> Parsed {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_GFM;
    let mut b = Builder {
        line_starts: std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect(),
        highlights: index_highlights(highlights),
        ..Builder::default()
    };
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        b.event(event, range);
    }
    b.flush_spans();
    Parsed {
        blocks: b.root,
        links: b.links,
    }
}

/// Per row: (col0, col1, highlight). Multi-line ranges get split per row.
type RowHighlights = HashMap<u32, Vec<(u32, u32, Highlight)>>;

fn index_highlights(highlights: &[Highlight]) -> RowHighlights {
    let mut rows: RowHighlights = HashMap::new();
    for h in highlights {
        for row in h.start.0..=h.end.0 {
            let c0 = if row == h.start.0 { h.start.1 } else { 0 };
            let c1 = if row == h.end.0 { h.end.1 } else { u32::MAX };
            if c1 > c0 {
                rows.entry(row).or_default().push((c0, c1, *h));
            }
        }
    }
    rows
}

enum Frame {
    Quote(Option<Alert>, Vec<Block>, Range<u32>),
    List(Option<u64>, Vec<ListItem>, Range<u32>),
    Item(ListItem),
    Footnote(String, Vec<Block>, Range<u32>),
    Indent(Vec<Block>, Range<u32>),
    Table {
        aligns: Vec<Align>,
        head: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
        lines: Range<u32>,
        in_head: bool,
    },
}

struct CodeAcc {
    lang: String,
    chunks: Vec<(String, u32, u32)>,
    lines: Range<u32>,
}

#[derive(Default)]
struct Builder {
    line_starts: Vec<usize>,
    highlights: RowHighlights,
    root: Vec<Block>,
    links: Vec<String>,
    stack: Vec<Frame>,
    spans: Vec<Span>,
    span_lines: Option<Range<u32>>,
    /// nesting depth of bold, italic, strike, code
    depth: [u32; 4],
    link_stack: Vec<u32>,
    code: Option<CodeAcc>,
    image: Option<(String, String)>,
    in_metadata: bool,
    /// whitespace collapses inside HTML blocks
    in_html: bool,
    html_heading: Option<u8>,
    /// open `<p>`/`<div>`/`<center>`/`<hN>` and whether each one centers
    center: Vec<(String, bool)>,
}

impl Builder {
    fn line_of(&self, byte: usize) -> u32 {
        (self.line_starts.partition_point(|&s| s <= byte) - 1) as u32
    }

    fn lines(&self, r: &Range<usize>) -> Range<u32> {
        let start = self.line_of(r.start);
        // end is exclusive and usually includes the trailing newline
        let end = self.line_of(r.end.saturating_sub(1).max(r.start)) + 1;
        start..end
    }

    fn centered(&self) -> bool {
        self.center.iter().any(|(_, c)| *c)
    }

    fn push_block(&mut self, kind: BlockKind, lines: Range<u32>) {
        let block = Block {
            kind,
            lines,
            center: self.centered(),
        };
        match self.stack.last_mut() {
            Some(
                Frame::Quote(_, blocks, _) | Frame::Footnote(_, blocks, _) | Frame::Indent(blocks, _),
            ) => blocks.push(block),
            Some(Frame::Item(item)) => item.blocks.push(block),
            _ => self.root.push(block),
        }
    }

    /// Loose text (tight list items, HTML) ends up as its own paragraph.
    fn flush_spans(&mut self) {
        if self.in_html
            && let Some(Span::Text(last, _)) = self.spans.last_mut()
        {
            let trimmed = last.trim_end().len();
            last.truncate(trimmed);
        }
        let empty = self.spans.iter().all(|s| match s {
            Span::Text(t, _) => t.is_empty(),
            Span::Image { .. } => false,
        });
        let spans = std::mem::take(&mut self.spans);
        let lines = self.span_lines.take().unwrap_or(0..0);
        if empty {
            return;
        }
        let kind = match self.html_heading.take() {
            Some(level) => BlockKind::Heading(level, spans),
            None => BlockKind::Paragraph(spans),
        };
        self.push_block(kind, lines);
    }

    fn style(&self, code: bool) -> Style {
        let d = self.depth;
        Style {
            bold: d[0] > 0,
            italic: d[1] > 0,
            strike: d[2] > 0,
            code: code || d[3] > 0,
            link: self.link_stack.last().copied().filter(|&l| l != u32::MAX),
            color: None,
        }
    }

    fn set_depth(&mut self, i: usize, delta: i32) {
        self.depth[i] = self.depth[i].saturating_add_signed(delta);
    }

    fn push_link(&mut self, url: &str) {
        self.links.push(url.to_string());
        self.link_stack.push(self.links.len() as u32 - 1);
    }

    fn extend_lines(&mut self, lines: Range<u32>) {
        let span_lines = self.span_lines.get_or_insert(lines.clone());
        span_lines.start = span_lines.start.min(lines.start);
        span_lines.end = span_lines.end.max(lines.end);
    }

    fn text(&mut self, text: &str, code: bool, lines: Range<u32>) {
        if let Some((_, alt)) = &mut self.image {
            alt.push_str(text);
            return;
        }
        let style = self.style(code);
        self.extend_lines(lines);
        match self.spans.last_mut() {
            Some(Span::Text(last, s)) if *s == style => last.push_str(text),
            _ => self.spans.push(Span::Text(text.to_string(), style)),
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        let lines = self.lines(&range);
        if self.in_metadata {
            if let Event::End(TagEnd::MetadataBlock(_)) = event {
                self.in_metadata = false;
            }
            return;
        }
        if self.code.is_some() {
            match event {
                Event::Text(t) => {
                    let row = self.line_of(range.start);
                    let col = (range.start - self.line_starts[row as usize]) as u32;
                    if let Some(code) = &mut self.code {
                        code.chunks.push((t.into_string(), row, col));
                    }
                }
                Event::End(TagEnd::CodeBlock) => {
                    let code = self.code.take().unwrap();
                    let spans = self.color_code(&code.chunks);
                    self.push_block(
                        BlockKind::Code {
                            lang: code.lang,
                            spans,
                        },
                        code.lines,
                    );
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, lines),
            Event::End(tag) => self.end(tag, lines),
            Event::Text(t) if self.in_html => self.html_text(&t, lines),
            Event::Text(t) => self.text(&t, false, lines),
            Event::Code(t) => self.text(&t, true, lines),
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t, true, lines),
            Event::SoftBreak => self.text(" ", false, lines),
            Event::HardBreak => self.text("\n", false, lines),
            Event::Html(html) | Event::InlineHtml(html) => self.html(&html, lines),
            Event::FootnoteReference(name) => {
                self.push_link(&format!("#{}", footnote_anchor(&name)));
                self.text(&format!("[{name}]"), false, lines);
                self.link_stack.pop();
            }
            Event::TaskListMarker(checked) => {
                if let Some(Frame::Item(item)) = self.stack.last_mut() {
                    item.task = Some(checked);
                }
            }
            Event::Rule => {
                self.flush_spans();
                self.push_block(BlockKind::Rule, lines);
            }
        }
    }

    fn color_code(&self, chunks: &[(String, u32, u32)]) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        let mut push = |text: &str, style: Style| {
            if text.is_empty() {
                return;
            }
            match spans.last_mut() {
                Some(Span::Text(last, s)) if *s == style => last.push_str(text),
                _ => spans.push(Span::Text(text.to_string(), style)),
            }
        };
        for (text, row, col) in chunks {
            let (mut row, mut col) = (*row, *col);
            let mut run_start = 0;
            let mut run_style = Style::default();
            for (i, ch) in text.char_indices() {
                // newlines keep the current style, fewer and longer spans
                if ch != '\n' {
                    let style = self.code_style(row, col);
                    if style != run_style {
                        push(&text[run_start..i], run_style);
                        run_start = i;
                        run_style = style;
                    }
                }
                if ch == '\n' {
                    row += 1;
                    col = 0;
                } else {
                    col += ch.len_utf8() as u32;
                }
            }
            push(&text[run_start..], run_style);
        }
        if let Some(Span::Text(last, _)) = spans.last_mut()
            && last.ends_with('\n')
        {
            last.pop();
        }
        spans.retain(|s| !matches!(s, Span::Text(t, _) if t.is_empty()));
        spans
    }

    /// Last covering highlight wins: treesitter captures children after their
    /// parents, so that's the innermost node.
    fn code_style(&self, row: u32, col: u32) -> Style {
        let mut style = Style::default();
        if let Some(hls) = self.highlights.get(&row) {
            for (c0, c1, h) in hls {
                if (*c0..*c1).contains(&col) {
                    style.color = Some(h.color);
                    style.bold = h.bold;
                    style.italic = h.italic;
                }
            }
        }
        style
    }

    fn start(&mut self, tag: Tag<'_>, lines: Range<u32>) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => {
                self.flush_spans();
                self.span_lines = Some(lines);
            }
            Tag::BlockQuote(kind) => {
                self.flush_spans();
                let alert = kind.map(|k| match k {
                    BlockQuoteKind::Note => Alert::Note,
                    BlockQuoteKind::Tip => Alert::Tip,
                    BlockQuoteKind::Important => Alert::Important,
                    BlockQuoteKind::Warning => Alert::Warning,
                    BlockQuoteKind::Caution => Alert::Caution,
                });
                self.stack.push(Frame::Quote(alert, Vec::new(), lines));
            }
            Tag::CodeBlock(kind) => {
                self.flush_spans();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(CodeAcc {
                    lang,
                    chunks: Vec::new(),
                    lines,
                });
            }
            Tag::List(start) => {
                self.flush_spans();
                self.stack.push(Frame::List(start, Vec::new(), lines));
            }
            Tag::Item => {
                self.flush_spans();
                self.stack.push(Frame::Item(ListItem {
                    task: None,
                    blocks: Vec::new(),
                }));
            }
            Tag::FootnoteDefinition(label) => {
                self.flush_spans();
                self.stack
                    .push(Frame::Footnote(label.to_string(), Vec::new(), lines));
            }
            Tag::DefinitionListTitle => {
                self.flush_spans();
                self.set_depth(0, 1);
            }
            Tag::DefinitionListDefinition => {
                self.flush_spans();
                self.stack.push(Frame::Indent(Vec::new(), lines));
            }
            Tag::Table(aligns) => {
                self.flush_spans();
                self.stack.push(Frame::Table {
                    aligns: aligns
                        .into_iter()
                        .map(|a| match a {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            Alignment::Left | Alignment::None => Align::Left,
                        })
                        .collect(),
                    head: Vec::new(),
                    rows: Vec::new(),
                    lines,
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(Frame::Table { in_head, .. }) = self.stack.last_mut() {
                    *in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(Frame::Table { rows, .. }) = self.stack.last_mut() {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.spans.clear(),
            Tag::Strong => self.set_depth(0, 1),
            Tag::Emphasis => self.set_depth(1, 1),
            Tag::Strikethrough => self.set_depth(2, 1),
            Tag::Link { dest_url, .. } => self.push_link(&dest_url),
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            Tag::MetadataBlock(_) => self.in_metadata = true,
            Tag::HtmlBlock => {
                self.flush_spans();
                self.in_html = true;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, lines: Range<u32>) {
        match tag {
            TagEnd::Paragraph => self.flush_spans(),
            TagEnd::Heading(level) => {
                let spans = std::mem::take(&mut self.spans);
                let lines = self.span_lines.take().unwrap_or(lines);
                self.push_block(BlockKind::Heading(level as u8, spans), lines);
            }
            TagEnd::BlockQuote(_) => {
                self.flush_spans();
                if let Some(Frame::Quote(alert, blocks, lines)) = self.stack.pop() {
                    self.push_block(BlockKind::Quote(alert, blocks), lines);
                }
            }
            TagEnd::List(_) => {
                self.flush_spans();
                if let Some(Frame::List(start, items, lines)) = self.stack.pop() {
                    self.push_block(BlockKind::List { start, items }, lines);
                }
            }
            TagEnd::Item => {
                self.flush_spans();
                if let Some(Frame::Item(item)) = self.stack.pop()
                    && let Some(Frame::List(_, items, _)) = self.stack.last_mut()
                {
                    items.push(item);
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_spans();
                if let Some(Frame::Footnote(label, blocks, lines)) = self.stack.pop() {
                    self.push_block(BlockKind::Footnote(label, blocks), lines);
                }
            }
            TagEnd::DefinitionListTitle => {
                self.flush_spans();
                self.set_depth(0, -1);
            }
            TagEnd::DefinitionListDefinition => {
                self.flush_spans();
                if let Some(Frame::Indent(blocks, lines)) = self.stack.pop() {
                    self.push_block(BlockKind::Indent(blocks), lines);
                }
            }
            TagEnd::TableHead => {
                if let Some(Frame::Table { in_head, .. }) = self.stack.last_mut() {
                    *in_head = false;
                }
            }
            TagEnd::TableCell => {
                let spans = std::mem::take(&mut self.spans);
                self.span_lines = None;
                if let Some(Frame::Table {
                    head,
                    rows,
                    in_head,
                    ..
                }) = self.stack.last_mut()
                {
                    if *in_head {
                        head.push(spans);
                    } else if let Some(row) = rows.last_mut() {
                        row.push(spans);
                    }
                }
            }
            TagEnd::Table => {
                if let Some(Frame::Table {
                    aligns,
                    head,
                    rows,
                    lines,
                    ..
                }) = self.stack.pop()
                {
                    self.push_block(BlockKind::Table { aligns, head, rows }, lines);
                }
            }
            TagEnd::Strong => self.set_depth(0, -1),
            TagEnd::Emphasis => self.set_depth(1, -1),
            TagEnd::Strikethrough => self.set_depth(2, -1),
            TagEnd::Link => {
                self.link_stack.pop();
            }
            TagEnd::Image => {
                if let Some((url, alt)) = self.image.take() {
                    self.extend_lines(lines);
                    self.spans.push(Span::Image {
                        url,
                        alt,
                        link: self.style(false).link,
                        width: None,
                        height: None,
                    });
                }
            }
            TagEnd::HtmlBlock => {
                self.flush_spans();
                self.in_html = false;
            }
            _ => {}
        }
    }

    fn html_text(&mut self, text: &str, lines: Range<u32>) {
        let mut out = String::with_capacity(text.len());
        let mut space = match self.spans.last() {
            None => true,
            Some(Span::Text(t, _)) => t.ends_with([' ', '\n']),
            Some(Span::Image { .. }) => false,
        };
        for c in text.chars() {
            if c.is_whitespace() {
                if !space {
                    out.push(' ');
                    space = true;
                }
            } else {
                out.push(c);
                space = false;
            }
        }
        // whitespace alone between two images doesn't separate them
        if !out.is_empty() && !(out == " " && matches!(self.spans.last(), Some(Span::Image { .. })))
        {
            self.text(&out, false, lines);
        }
    }

    fn html(&mut self, html: &str, lines: Range<u32>) {
        for token in html_tokens(html) {
            match token {
                HtmlToken::Text(t) => {
                    let t = decode_entities(&t);
                    if self.in_html {
                        self.html_text(&t, lines.clone());
                    } else {
                        self.text(&t, false, lines.clone());
                    }
                }
                HtmlToken::Open(name, attrs) => self.html_open(&name, &attrs, lines.clone()),
                HtmlToken::Close(name) => self.html_close(&name),
            }
        }
    }

    fn html_open(&mut self, name: &str, attrs: &[(String, String)], lines: Range<u32>) {
        let attr = |key: &str| {
            attrs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let size =
            |key: &str| attr(key).and_then(|v| v.trim().trim_end_matches("px").parse::<u32>().ok());
        let centered = attr("align").is_some_and(|a| a.eq_ignore_ascii_case("center"));
        match name {
            "br" => self.text("\n", false, lines),
            "img" => {
                self.extend_lines(lines);
                self.spans.push(Span::Image {
                    url: decode_entities(attr("src").unwrap_or("")),
                    alt: decode_entities(attr("alt").unwrap_or("")),
                    link: self.style(false).link,
                    width: size("width"),
                    height: size("height"),
                });
            }
            "a" => match attr("href") {
                Some(href) => self.push_link(&decode_entities(href)),
                // push a dummy so `</a>` still pops the right entry
                None => self.link_stack.push(u32::MAX),
            },
            "b" | "strong" => self.set_depth(0, 1),
            "i" | "em" => self.set_depth(1, 1),
            "s" | "del" | "strike" => self.set_depth(2, 1),
            "code" | "kbd" | "tt" | "samp" => self.set_depth(3, 1),
            "p" | "div" | "center" => {
                self.flush_spans();
                self.center
                    .push((name.to_string(), centered || name == "center"));
            }
            "details" | "table" | "tr" | "ul" | "ol" => self.flush_spans(),
            "li" => {
                self.flush_spans();
                self.text("• ", false, lines);
            }
            "summary" => {
                self.flush_spans();
                self.set_depth(0, 1);
                self.text("▸ ", false, lines);
            }
            "hr" => {
                self.flush_spans();
                self.push_block(BlockKind::Rule, lines);
            }
            "td" | "th" => {
                if !self.spans.is_empty() {
                    self.text("   ", false, lines);
                }
            }
            _ => {
                if let Some(level) = heading_level(name) {
                    self.flush_spans();
                    self.html_heading = Some(level);
                    self.center.push((name.to_string(), centered));
                }
            }
        }
    }

    fn html_close(&mut self, name: &str) {
        match name {
            "a" => {
                self.link_stack.pop();
            }
            "b" | "strong" => self.set_depth(0, -1),
            "i" | "em" => self.set_depth(1, -1),
            "s" | "del" | "strike" => self.set_depth(2, -1),
            "code" | "kbd" | "tt" | "samp" => self.set_depth(3, -1),
            "summary" => {
                self.flush_spans();
                self.set_depth(0, -1);
            }
            "details" | "table" | "tr" | "ul" | "ol" | "li" => self.flush_spans(),
            _ => {
                if matches!(name, "p" | "div" | "center") || heading_level(name).is_some() {
                    self.flush_spans();
                    if let Some(i) = self.center.iter().rposition(|(n, _)| n == name) {
                        self.center.truncate(i);
                    }
                }
            }
        }
    }
}

fn heading_level(name: &str) -> Option<u8> {
    match name.strip_prefix('h')?.parse::<u8>().ok()? {
        level @ 1..=6 => Some(level),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
enum HtmlToken {
    Text(String),
    Open(String, Vec<(String, String)>),
    Close(String),
}

/// Forgiving on purpose. Skips comments and doctypes, anything malformed
/// stays text.
fn html_tokens(s: &str) -> Vec<HtmlToken> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    while let Some(lt) = rest.find('<') {
        text.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |i| &after[i + 3..]);
            continue;
        }
        let next = rest[1..].chars().next();
        if next.is_some_and(|c| c == '!' || c == '?') {
            rest = rest.find('>').map_or("", |i| &rest[i + 1..]);
            continue;
        }
        if let Some(after) = rest.strip_prefix("</") {
            let Some(gt) = after.find('>') else { break };
            if !text.is_empty() {
                tokens.push(HtmlToken::Text(std::mem::take(&mut text)));
            }
            tokens.push(HtmlToken::Close(after[..gt].trim().to_ascii_lowercase()));
            rest = &after[gt + 1..];
            continue;
        }
        if !next.is_some_and(|c| c.is_ascii_alphabetic()) {
            text.push('<');
            rest = &rest[1..];
            continue;
        }
        // '>' can show up inside attribute values, so actually parse the tag
        let (name, attrs, len) = parse_tag(&rest[1..]);
        if !text.is_empty() {
            tokens.push(HtmlToken::Text(std::mem::take(&mut text)));
        }
        tokens.push(HtmlToken::Open(name, attrs));
        rest = rest.get(1 + len..).unwrap_or("");
    }
    text.push_str(rest);
    if !text.is_empty() {
        tokens.push(HtmlToken::Text(text));
    }
    tokens
}

/// Starts after the '<'. Returns (name, attrs, length through the '>').
fn parse_tag(s: &str) -> (String, Vec<(String, String)>, usize) {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-') {
        i += 1;
    }
    let name = s[..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        if i >= b.len() || b[i] == b'>' {
            return (name, attrs, i + 1);
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
            i += 1;
        }
        let key = s[start..i].to_ascii_lowercase();
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let quote = b[i];
                let start = i + 1;
                i = start;
                while i < b.len() && b[i] != quote {
                    i += 1;
                }
                value = s[start..i].to_string();
                i += 1;
            } else {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = s[start..i].to_string();
            }
        }
        if key.is_empty() {
            i += 1;
        } else {
            attrs.push((key, value));
        }
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(';').filter(|&e| e <= 10).and_then(|e| {
            let c = match &rest[1..e] {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => '\u{a0}',
                "copy" => '©',
                "mdash" => '—',
                "ndash" => '–',
                name => {
                    let num = name.strip_prefix('#')?;
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => num.parse().ok()?,
                    };
                    char::from_u32(code)?
                }
            };
            Some((c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Span {
        Span::Text(s.into(), Style::default())
    }

    fn blocks(md: &str) -> Vec<Block> {
        parse(md, &[]).blocks
    }

    #[test]
    fn blocks_keep_their_source_lines() {
        let md = "# Title\n\nSome *text*\nmore.\n\n```rust\nfn main() {}\n```\n\n---\n";
        let blocks = blocks(md);
        let lines: Vec<_> = blocks.iter().map(|b| b.lines.clone()).collect();
        assert_eq!(lines, vec![0..1, 2..4, 5..8, 9..10]);
        assert_eq!(blocks[0].kind, BlockKind::Heading(1, vec![text("Title")]));
        let BlockKind::Paragraph(spans) = &blocks[1].kind else {
            panic!()
        };
        assert_eq!(
            spans,
            &vec![
                text("Some "),
                Span::Text(
                    "text".into(),
                    Style {
                        italic: true,
                        ..Style::default()
                    }
                ),
                text(" more."),
            ]
        );
        assert_eq!(
            blocks[2].kind,
            BlockKind::Code {
                lang: "rust".into(),
                spans: vec![text("fn main() {}")]
            }
        );
        assert_eq!(blocks[3].kind, BlockKind::Rule);
    }

    #[test]
    fn tight_lists_and_tasks() {
        let blocks = blocks("- [x] done\n- todo\n  1. nested\n");
        let BlockKind::List { start, items } = &blocks[0].kind else {
            panic!("{blocks:?}")
        };
        assert_eq!(*start, None);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].task, Some(true));
        assert_eq!(
            items[0].blocks[0].kind,
            BlockKind::Paragraph(vec![text("done")])
        );
        assert_eq!(items[1].task, None);
        let BlockKind::List { start, items: sub } = &items[1].blocks[1].kind else {
            panic!("{items:?}")
        };
        assert_eq!(*start, Some(1));
        assert_eq!(sub[0].blocks[0].lines, 2..3);
    }

    #[test]
    fn quotes_tables_images_and_frontmatter() {
        let md = "---\ntitle: x\n---\n> quoted\n\n| a | b |\n|---|--:|\n| 1 | **2** |\n\n![alt text](img/a.png)\n";
        let blocks = blocks(md);
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        let BlockKind::Quote(None, inner) = &blocks[0].kind else {
            panic!()
        };
        assert_eq!(inner[0].kind, BlockKind::Paragraph(vec![text("quoted")]));
        assert_eq!(blocks[0].lines, 3..4);
        let BlockKind::Table { aligns, head, rows } = &blocks[1].kind else {
            panic!()
        };
        assert_eq!(aligns, &vec![Align::Left, Align::Right]);
        assert_eq!(head, &vec![vec![text("a")], vec![text("b")]]);
        assert_eq!(rows.len(), 1);
        assert_eq!(blocks[2].lines, 9..10);
        assert_eq!(
            blocks[2].kind,
            BlockKind::Paragraph(vec![Span::Image {
                url: "img/a.png".into(),
                alt: "alt text".into(),
                link: None,
                width: None,
                height: None,
            }])
        );
    }

    #[test]
    fn code_is_colored_by_the_highlights() {
        let md = "text\n\n```lua\nlocal x = 1\n```\n";
        let hl = |r, c0, c1, color| Highlight {
            start: (r, c0),
            end: (r, c1),
            color,
            bold: false,
            italic: false,
        };
        // the parent capture over the whole line comes first, like treesitter emits it
        let parsed = parse(md, &[hl(3, 0, 11, 1), hl(3, 0, 5, 2), hl(3, 10, 11, 3)]);
        let BlockKind::Code { spans, .. } = &parsed.blocks[1].kind else {
            panic!()
        };
        let colored = |s: &str, c| {
            Span::Text(
                s.into(),
                Style {
                    color: Some(c),
                    ..Style::default()
                },
            )
        };
        assert_eq!(
            spans,
            &vec![colored("local", 2), colored(" x = ", 1), colored("1", 3)]
        );
    }

    #[test]
    fn alerts_footnotes_and_links() {
        let md = "> [!WARNING]\n> careful\n\nSee [docs](https://x.org)[^1].\n\n[^1]: The note.\n";
        let parsed = parse(md, &[]);
        assert!(matches!(
            parsed.blocks[0].kind,
            BlockKind::Quote(Some(Alert::Warning), _)
        ));
        assert_eq!(parsed.links, vec!["https://x.org", "#fn-1"]);
        let BlockKind::Paragraph(spans) = &parsed.blocks[1].kind else {
            panic!()
        };
        assert!(matches!(&spans[1], Span::Text(t, s) if t == "docs" && s.link == Some(0)));
        assert!(matches!(&spans[2], Span::Text(t, s) if t == "[1]" && s.link == Some(1)));
        let BlockKind::Footnote(label, inner) = &parsed.blocks[2].kind else {
            panic!("{:?}", parsed.blocks)
        };
        assert_eq!(label, "1");
        assert_eq!(inner[0].kind, BlockKind::Paragraph(vec![text("The note.")]));
    }

    #[test]
    fn readme_html() {
        let md = "<p align=\"center\">\n  <a href=\"https://x.org\"><img src=\"logo.png\" alt=\"Logo\" width=\"120px\"></a>\n</p>\n\n<h1 align=\"center\">My &amp; tool</h1>\n\nPress <kbd>Ctrl</kbd>.\n";
        let parsed = parse(md, &[]);
        let b = &parsed.blocks;
        assert_eq!(b.len(), 3, "{b:?}");
        assert!(b[0].center && b[1].center && !b[2].center);
        assert_eq!(
            b[0].kind,
            BlockKind::Paragraph(vec![Span::Image {
                url: "logo.png".into(),
                alt: "Logo".into(),
                link: Some(0),
                width: Some(120),
                height: None,
            }])
        );
        assert_eq!(b[1].kind, BlockKind::Heading(1, vec![text("My & tool")]));
        let BlockKind::Paragraph(spans) = &b[2].kind else {
            panic!()
        };
        assert!(matches!(&spans[1], Span::Text(t, s) if t == "Ctrl" && s.code));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("API v2 (beta)"), "api-v2-beta");
    }
}
