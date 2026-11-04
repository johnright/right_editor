//! `redraw` notifications (ext_linegrid, rgb) into typed events.
//!
//! Shape is `["redraw", [[name, args1, args2, ...], ...]]`, and one name can
//! carry several arg tuples, each becomes its own event. Values are moved out
//! so strings aren't copied.

use std::fmt;

use rmpv::Value;

pub type GridId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum RedrawEvent {
    GridResize {
        grid: GridId,
        width: u64,
        height: u64,
    },
    GridLine {
        grid: GridId,
        row: u64,
        col_start: u64,
        cells: Vec<GridLineCell>,
    },
    GridScroll {
        grid: GridId,
        top: u64,
        bot: u64,
        left: u64,
        right: u64,
        rows: i64,
    },
    GridClear {
        grid: GridId,
    },
    GridDestroy {
        grid: GridId,
    },
    GridCursorGoto {
        grid: GridId,
        row: u64,
        col: u64,
    },
    HlAttrDefine {
        id: u64,
        attr: HlAttr,
    },
    DefaultColorsSet {
        fg: u32,
        bg: u32,
        sp: u32,
    },
    ModeInfoSet {
        cursor_style_enabled: bool,
        modes: Vec<ModeInfo>,
    },
    ModeChange {
        name: String,
        mode_idx: usize,
    },
    BusyStart,
    BusyStop,
    // from option_set
    GuiFont(String),
    LineSpace(i64),
    WinPos {
        grid: GridId,
        row: u64,
        col: u64,
        width: u64,
        height: u64,
    },
    /// nvim has already resolved the anchor, row/col are screen cells.
    WinFloatPos {
        grid: GridId,
        row: i64,
        col: i64,
        zindex: i64,
        compindex: i64,
        mouse: bool,
    },
    WinHide {
        grid: GridId,
    },
    WinClose {
        grid: GridId,
    },
    /// `scroll_delta` is 0 when rows were inserted/deleted without the view
    /// actually scrolling. Only nonzero deltas get animated.
    WinViewport {
        grid: GridId,
        scroll_delta: i64,
    },
    MsgSetPos {
        grid: GridId,
        row: u64,
        zindex: i64,
        compindex: i64,
    },
    SetTitle(String),
    /// Sent on `:restart` with the new server's address. The current server
    /// exits right after and we have to reattach.
    Restart(String),
    Flush,
    Unhandled(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridLineCell {
    pub text: String,
    /// `None`: reuse the previous cell's hl
    pub hl_id: Option<u64>,
    pub repeat: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeInfo {
    pub cursor_shape: CursorShape,
    pub cell_percentage: u8,
    /// 0 means draw the cell with fg/bg swapped
    pub attr_id: u32,
}

impl Default for ModeInfo {
    fn default() -> Self {
        Self {
            cursor_shape: CursorShape::Block,
            cell_percentage: 100,
            attr_id: 0,
        }
    }
}

/// `rgb_attr` from hl_attr_define. `None` colors fall back to the defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HlAttr {
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub special: Option<u32>,
    pub reverse: bool,
    pub italic: bool,
    pub bold: bool,
    pub strikethrough: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub underdouble: bool,
    pub underdotted: bool,
    pub underdashed: bool,
    pub blend: u8,
}

#[derive(Debug)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, ParseError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ParseError(msg.into()))
}

/// Appends to `out`. Malformed events get logged and skipped, the rest of the
/// batch still goes through.
pub fn parse_redraw(args: Vec<Value>, out: &mut Vec<RedrawEvent>) {
    for batch in args {
        let Value::Array(batch) = batch else {
            log::warn!("redraw: batch is not an array: {batch}");
            continue;
        };
        let mut items = batch.into_iter();
        let Some(Value::String(name)) = items.next() else {
            log::warn!("redraw: event without a name");
            continue;
        };
        let name = name.into_str().unwrap_or_default();
        for tuple in items {
            match parse_event(&name, tuple) {
                Ok(ev) => out.push(ev),
                Err(e) => log::warn!("redraw: {name}: {e}"),
            }
        }
    }
}

fn parse_event(name: &str, tuple: Value) -> Result<RedrawEvent> {
    let Value::Array(args) = tuple else {
        return err("arguments not in an array");
    };
    let mut a = Args(args.into_iter());
    Ok(match name {
        "grid_resize" => RedrawEvent::GridResize {
            grid: a.u64()?,
            width: a.u64()?,
            height: a.u64()?,
        },
        "grid_line" => RedrawEvent::GridLine {
            grid: a.u64()?,
            row: a.u64()?,
            col_start: a.u64()?,
            cells: parse_cells(a.array()?)?,
            // ignoring `wrap` (5th arg, nvim 0.10+)
        },
        // trailing `cols` is always 0 per the docs
        "grid_scroll" => RedrawEvent::GridScroll {
            grid: a.u64()?,
            top: a.u64()?,
            bot: a.u64()?,
            left: a.u64()?,
            right: a.u64()?,
            rows: a.i64()?,
        },
        "grid_clear" => RedrawEvent::GridClear { grid: a.u64()? },
        "grid_destroy" => RedrawEvent::GridDestroy { grid: a.u64()? },
        "grid_cursor_goto" => RedrawEvent::GridCursorGoto {
            grid: a.u64()?,
            row: a.u64()?,
            col: a.u64()?,
        },
        "hl_attr_define" => RedrawEvent::HlAttrDefine {
            id: a.u64()?,
            attr: parse_hl_attr(a.next()?)?,
        },
        "default_colors_set" => RedrawEvent::DefaultColorsSet {
            fg: a.color()?,
            bg: a.color()?,
            sp: a.color()?,
        },
        "mode_info_set" => RedrawEvent::ModeInfoSet {
            cursor_style_enabled: a.next()?.as_bool().unwrap_or(true),
            modes: a.array()?.into_iter().map(parse_mode_info).collect(),
        },
        "mode_change" => RedrawEvent::ModeChange {
            name: a.string()?,
            mode_idx: a.u64()? as usize,
        },
        "busy_start" => RedrawEvent::BusyStart,
        "busy_stop" => RedrawEvent::BusyStop,
        "option_set" => {
            let name = a.string()?;
            let value = a.next()?;
            match name.as_str() {
                "guifont" => RedrawEvent::GuiFont(value.as_str().unwrap_or_default().to_string()),
                "linespace" => RedrawEvent::LineSpace(value.as_i64().unwrap_or(0)),
                _ => RedrawEvent::Unhandled("option_set".to_string()),
            }
        }
        "set_title" => RedrawEvent::SetTitle(a.string()?),
        "win_pos" => {
            let grid = a.u64()?;
            a.next()?; // window handle
            RedrawEvent::WinPos {
                grid,
                row: a.u64()?,
                col: a.u64()?,
                width: a.u64()?,
                height: a.u64()?,
            }
        }
        "win_float_pos" => {
            // [grid, win, anchor, anchor_grid, anchor_row, anchor_col,
            //  mouse_enabled, zindex, compindex, screen_row, screen_col]
            let grid = a.u64()?;
            for _ in 0..4 {
                a.next()?;
            }
            a.next()?; // anchor_col
            let mouse = a.next()?.as_bool().unwrap_or(true);
            let zindex = a.i64()?;
            let compindex = a.i64()?;
            RedrawEvent::WinFloatPos {
                grid,
                row: a.number()?,
                col: a.number()?,
                zindex,
                compindex,
                mouse,
            }
        }
        "win_hide" => RedrawEvent::WinHide { grid: a.u64()? },
        "win_close" => RedrawEvent::WinClose { grid: a.u64()? },
        "win_viewport" => {
            // [grid, win, topline, botline, curline, curcol, line_count, scroll_delta]
            let grid = a.u64()?;
            for _ in 0..6 {
                a.next()?;
            }
            RedrawEvent::WinViewport {
                grid,
                // older nvim doesn't send it, assume a real scroll
                scroll_delta: a.next().ok().and_then(|v| v.as_i64()).unwrap_or(1),
            }
        }
        "msg_set_pos" => {
            let grid = a.u64()?;
            let row = a.u64()?;
            a.next()?; // scrolled
            a.next()?; // sep_char
            RedrawEvent::MsgSetPos {
                grid,
                row,
                zindex: a.i64()?,
                compindex: a.i64()?,
            }
        }
        "flush" => RedrawEvent::Flush,
        "restart" => RedrawEvent::Restart(a.string()?),
        other => RedrawEvent::Unhandled(other.to_string()),
    })
}

fn parse_cells(cells: Vec<Value>) -> Result<Vec<GridLineCell>> {
    cells
        .into_iter()
        .map(|cell| {
            let Value::Array(cell) = cell else {
                return err("cell not in an array");
            };
            let mut c = Args(cell.into_iter());
            let text = c.string()?;
            let hl_id = c.opt_u64()?;
            let repeat = c.opt_u64()?.unwrap_or(1);
            Ok(GridLineCell {
                text,
                hl_id,
                repeat,
            })
        })
        .collect()
}

fn parse_mode_info(map: Value) -> ModeInfo {
    let mut info = ModeInfo::default();
    let Value::Map(map) = map else { return info };
    for (key, value) in map {
        match key.as_str() {
            Some("cursor_shape") => {
                info.cursor_shape = match value.as_str() {
                    Some("horizontal") => CursorShape::Horizontal,
                    Some("vertical") => CursorShape::Vertical,
                    _ => CursorShape::Block,
                }
            }
            Some("cell_percentage") => {
                info.cell_percentage = value.as_u64().unwrap_or(100).clamp(1, 100) as u8
            }
            Some("attr_id") => info.attr_id = value.as_u64().unwrap_or(0) as u32,
            _ => {}
        }
    }
    info
}

fn parse_hl_attr(map: Value) -> Result<HlAttr> {
    let Value::Map(map) = map else {
        return err("rgb_attr is not a map");
    };
    let mut attr = HlAttr::default();
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        let color = || value.as_u64().map(|v| v as u32);
        let flag = || value.as_bool().unwrap_or(false);
        match key {
            "foreground" => attr.foreground = color(),
            "background" => attr.background = color(),
            "special" => attr.special = color(),
            "reverse" => attr.reverse = flag(),
            "italic" => attr.italic = flag(),
            "bold" => attr.bold = flag(),
            "strikethrough" => attr.strikethrough = flag(),
            "underline" => attr.underline = flag(),
            "undercurl" => attr.undercurl = flag(),
            "underdouble" => attr.underdouble = flag(),
            "underdotted" => attr.underdotted = flag(),
            "underdashed" => attr.underdashed = flag(),
            "blend" => attr.blend = value.as_u64().unwrap_or(0).min(100) as u8,
            _ => {}
        }
    }
    Ok(attr)
}

struct Args(std::vec::IntoIter<Value>);

impl Args {
    fn next(&mut self) -> Result<Value> {
        self.0.next().map_or_else(|| err("missing argument"), Ok)
    }

    fn u64(&mut self) -> Result<u64> {
        let v = self.next()?;
        v.as_u64()
            .map_or_else(|| err(format!("expected integer ≥ 0, found {v}")), Ok)
    }

    fn i64(&mut self) -> Result<i64> {
        let v = self.next()?;
        v.as_i64()
            .map_or_else(|| err(format!("expected integer, found {v}")), Ok)
    }

    // -1 (unset) maps to u32::MAX
    fn color(&mut self) -> Result<u32> {
        let v = self.i64()?;
        Ok(if v < 0 { u32::MAX } else { v as u32 })
    }

    // some positions come in as floats
    fn number(&mut self) -> Result<i64> {
        let v = self.next()?;
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f.round() as i64))
            .map_or_else(|| err(format!("expected a number, found {v}")), Ok)
    }

    fn opt_u64(&mut self) -> Result<Option<u64>> {
        match self.0.next() {
            None => Ok(None),
            Some(v) => v.as_u64().map_or_else(
                || err(format!("expected integer ≥ 0, found {v}")),
                |n| Ok(Some(n)),
            ),
        }
    }

    fn string(&mut self) -> Result<String> {
        match self.next()? {
            Value::String(s) => s.into_str().map_or_else(|| err("non-UTF-8 string"), Ok),
            v => err(format!("expected string, found {v}")),
        }
    }

    fn array(&mut self) -> Result<Vec<Value>> {
        match self.next()? {
            Value::Array(a) => Ok(a),
            v => err(format!("expected array, found {v}")),
        }
    }
}

impl fmt::Display for RedrawEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RedrawEvent::GridLine {
                grid,
                row,
                col_start,
                cells,
            } => {
                let mut text = String::new();
                for c in cells {
                    for _ in 0..c.repeat.min(200) {
                        text.push_str(&c.text);
                    }
                }
                let text: String = text.chars().take(120).collect();
                write!(
                    f,
                    "grid_line g{grid} r{row} c{col_start} ({} cells) {text:?}",
                    cells.len()
                )
            }
            RedrawEvent::HlAttrDefine { id, attr } => write!(
                f,
                "hl_attr_define {id} fg={} bg={}{}{}{}",
                fmt_color(attr.foreground),
                fmt_color(attr.background),
                if attr.bold { " bold" } else { "" },
                if attr.italic { " italic" } else { "" },
                if attr.reverse { " reverse" } else { "" },
            ),
            RedrawEvent::DefaultColorsSet { fg, bg, sp } => write!(
                f,
                "default_colors_set fg={} bg={} sp={}",
                fmt_color(Some(*fg)),
                fmt_color(Some(*bg)),
                fmt_color(Some(*sp))
            ),
            other => write!(f, "{other:?}"),
        }
    }
}

fn fmt_color(c: Option<u32>) -> String {
    match c {
        Some(u32::MAX) | None => "-".to_string(),
        Some(c) => format!("#{c:06x}"),
    }
}

impl RedrawEvent {
    pub fn name(&self) -> &str {
        match self {
            RedrawEvent::GridResize { .. } => "grid_resize",
            RedrawEvent::GridLine { .. } => "grid_line",
            RedrawEvent::GridScroll { .. } => "grid_scroll",
            RedrawEvent::GridClear { .. } => "grid_clear",
            RedrawEvent::GridDestroy { .. } => "grid_destroy",
            RedrawEvent::GridCursorGoto { .. } => "grid_cursor_goto",
            RedrawEvent::HlAttrDefine { .. } => "hl_attr_define",
            RedrawEvent::DefaultColorsSet { .. } => "default_colors_set",
            RedrawEvent::ModeInfoSet { .. } => "mode_info_set",
            RedrawEvent::ModeChange { .. } => "mode_change",
            RedrawEvent::BusyStart => "busy_start",
            RedrawEvent::BusyStop => "busy_stop",
            RedrawEvent::GuiFont(_) => "guifont",
            RedrawEvent::LineSpace(_) => "linespace",
            RedrawEvent::SetTitle(_) => "set_title",
            RedrawEvent::WinPos { .. } => "win_pos",
            RedrawEvent::WinFloatPos { .. } => "win_float_pos",
            RedrawEvent::WinHide { .. } => "win_hide",
            RedrawEvent::WinClose { .. } => "win_close",
            RedrawEvent::WinViewport { .. } => "win_viewport",
            RedrawEvent::MsgSetPos { .. } => "msg_set_pos",
            RedrawEvent::Flush => "flush",
            RedrawEvent::Restart(_) => "restart",
            RedrawEvent::Unhandled(name) => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        Value::from(s)
    }

    #[test]
    fn parses_restart() {
        let mut out = Vec::new();
        parse_redraw(
            vec![Value::Array(vec![
                v("restart"),
                Value::Array(vec![v("/run/user/1000/nvim.1.0")]),
            ])],
            &mut out,
        );
        assert_eq!(
            out,
            [RedrawEvent::Restart("/run/user/1000/nvim.1.0".into())]
        );
    }

    #[test]
    fn parses_multi_tuple_events() {
        let args = vec![
            Value::Array(vec![
                v("grid_resize"),
                Value::Array(vec![1.into(), 80.into(), 24.into()]),
            ]),
            Value::Array(vec![
                v("grid_line"),
                Value::Array(vec![
                    1.into(),
                    0.into(),
                    2.into(),
                    Value::Array(vec![
                        Value::Array(vec![v("a"), 3.into()]),
                        Value::Array(vec![v(" "), 3.into(), 4.into()]),
                    ]),
                    false.into(),
                ]),
                Value::Array(vec![
                    1.into(),
                    1.into(),
                    0.into(),
                    Value::Array(vec![Value::Array(vec![v("b")])]),
                    false.into(),
                ]),
            ]),
            Value::Array(vec![v("flush"), Value::Array(vec![])]),
        ];
        let mut out = Vec::new();
        parse_redraw(args, &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(
            out[0],
            RedrawEvent::GridResize {
                grid: 1,
                width: 80,
                height: 24
            }
        );
        let RedrawEvent::GridLine { cells, .. } = &out[1] else {
            panic!()
        };
        assert_eq!(
            cells[0],
            GridLineCell {
                text: "a".into(),
                hl_id: Some(3),
                repeat: 1
            }
        );
        assert_eq!(
            cells[1],
            GridLineCell {
                text: " ".into(),
                hl_id: Some(3),
                repeat: 4
            }
        );
        assert!(matches!(out[2], RedrawEvent::GridLine { row: 1, .. }));
        assert_eq!(out[3], RedrawEvent::Flush);
    }

    #[test]
    fn parses_multigrid_events() {
        let win = Value::Ext(1, vec![0]);
        let args = vec![
            Value::Array(vec![
                v("win_pos"),
                Value::Array(vec![
                    2.into(),
                    win.clone(),
                    0.into(),
                    0.into(),
                    80.into(),
                    24.into(),
                ]),
            ]),
            Value::Array(vec![
                v("win_float_pos"),
                Value::Array(vec![
                    4.into(),
                    win,
                    v("NW"),
                    2.into(),
                    3.0.into(),
                    5.0.into(),
                    true.into(),
                    50.into(),
                    7.into(),
                    3.into(),
                    5.into(),
                ]),
            ]),
            Value::Array(vec![
                v("msg_set_pos"),
                Value::Array(vec![
                    3.into(),
                    20.into(),
                    false.into(),
                    v(""),
                    200.into(),
                    9.into(),
                ]),
            ]),
        ];
        let mut out = Vec::new();
        parse_redraw(args, &mut out);
        assert_eq!(
            out[0],
            RedrawEvent::WinPos {
                grid: 2,
                row: 0,
                col: 0,
                width: 80,
                height: 24
            }
        );
        assert_eq!(
            out[1],
            RedrawEvent::WinFloatPos {
                grid: 4,
                row: 3,
                col: 5,
                zindex: 50,
                compindex: 7,
                mouse: true
            }
        );
        assert_eq!(
            out[2],
            RedrawEvent::MsgSetPos {
                grid: 3,
                row: 20,
                zindex: 200,
                compindex: 9
            }
        );
    }

    #[test]
    fn parses_hl_and_colors() {
        let attr = Value::Map(vec![
            (v("foreground"), 0xff0000.into()),
            (v("bold"), true.into()),
        ]);
        let args = vec![
            Value::Array(vec![
                v("hl_attr_define"),
                Value::Array(vec![
                    5.into(),
                    attr,
                    Value::Map(vec![]),
                    Value::Array(vec![]),
                ]),
            ]),
            Value::Array(vec![
                v("default_colors_set"),
                Value::Array(vec![
                    0xffffff.into(),
                    0.into(),
                    (-1).into(),
                    0.into(),
                    0.into(),
                ]),
            ]),
        ];
        let mut out = Vec::new();
        parse_redraw(args, &mut out);
        let RedrawEvent::HlAttrDefine { id: 5, attr } = &out[0] else {
            panic!()
        };
        assert_eq!(attr.foreground, Some(0xff0000));
        assert!(attr.bold);
        assert_eq!(
            out[1],
            RedrawEvent::DefaultColorsSet {
                fg: 0xffffff,
                bg: 0,
                sp: u32::MAX
            }
        );
    }
}
