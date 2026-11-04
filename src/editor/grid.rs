use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::nvim::redraw::GridLineCell;

/// A single codepoint is stored inline; multi-codepoint clusters (emoji,
/// combining marks) go through `TextInterner`. Keeps `Cell` at 8 bytes so
/// scrolling and row copies are cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellText(u32);

const CLUSTER_BASE: u32 = 0x11_0000;

impl CellText {
    /// Right half of a wide char, nvim sends "" for it.
    pub const EMPTY: Self = Self(u32::MAX);
    pub const SPACE: Self = Self(' ' as u32);
}

/// Never cleared, there just aren't many distinct clusters in a session.
#[derive(Debug, Default)]
pub struct TextInterner {
    strings: Vec<Box<str>>,
    index: HashMap<Box<str>, u32>,
}

impl TextInterner {
    pub fn intern(&mut self, s: &str) -> CellText {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (None, _) => CellText::EMPTY,
            (Some(c), None) => CellText(c as u32),
            _ => {
                if let Some(&i) = self.index.get(s) {
                    return CellText(CLUSTER_BASE + i);
                }
                let i = self.strings.len() as u32;
                self.strings.push(s.into());
                self.index.insert(s.into(), i);
                CellText(CLUSTER_BASE + i)
            }
        }
    }

    pub fn push_to(&self, text: CellText, out: &mut String) {
        match text.0 {
            u32::MAX => {}
            c if c < CLUSTER_BASE => out.push(char::from_u32(c).unwrap_or('\u{fffd}')),
            i => out.push_str(&self.strings[(i - CLUSTER_BASE) as usize]),
        }
    }
}

// More than this in one flush is a full redraw, not typing. Stop tracking.
const MAX_CHANGED: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub text: CellText,
    pub hl: u32,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: CellText::SPACE,
            hl: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    /// Survives grid_scroll (rows get rotated, not copied), so the renderer
    /// caches per id rather than per position.
    pub id: u64,
    pub cells: Vec<Cell>,
    pub dirty: bool,
}

impl Row {
    fn new(width: usize) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            cells: vec![Cell::default(); width],
            dirty: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Grid {
    width: usize,
    height: usize,
    pub rows: Vec<Row>,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            rows: (0..height).map(|_| Row::new(width)).collect(),
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.rows.resize_with(height, || Row::new(width));
        for row in &mut self.rows {
            row.cells.resize(width, Cell::default());
            row.dirty = true;
        }
    }

    pub fn clear(&mut self) {
        for row in &mut self.rows {
            row.cells.fill(Cell::default());
            row.dirty = true;
        }
    }

    pub fn mark_all_dirty(&mut self) {
        for row in &mut self.rows {
            row.dirty = true;
        }
    }

    /// Pushes (row id, col) of every cell whose text changed into `changed`.
    pub fn line(
        &mut self,
        row: usize,
        col_start: usize,
        cells: &[GridLineCell],
        text: &mut TextInterner,
        changed: &mut Vec<(u64, usize)>,
    ) {
        let Some(row) = self.rows.get_mut(row) else {
            log::warn!("grid_line outside the grid: row {row} ≥ {}", self.height);
            return;
        };
        row.dirty = true;
        let mut col = col_start;
        let mut hl = 0;
        for cell in cells {
            if let Some(id) = cell.hl_id {
                hl = id as u32;
            }
            let value = Cell {
                text: text.intern(&cell.text),
                hl,
            };
            let end = (col + cell.repeat as usize).min(row.cells.len());
            if col < end {
                for (i, c) in row.cells[col..end].iter_mut().enumerate() {
                    if c.text != value.text && changed.len() < MAX_CHANGED {
                        changed.push((row.id, col + i));
                    }
                    *c = value;
                }
            }
            col = end;
        }
    }

    /// `rows > 0` moves the region up. Rows that scroll in are left as
    /// garbage, nvim sends grid_line for them in the same flush.
    pub fn scroll(&mut self, top: usize, bot: usize, left: usize, right: usize, rows: i64) {
        let bot = bot.min(self.height);
        let right = right.min(self.width);
        if rows == 0 || top >= bot || left >= right {
            return;
        }
        let n = rows.unsigned_abs() as usize;
        let region = bot - top;
        if n >= region {
            self.rows[top..bot].iter_mut().for_each(|r| r.dirty = true);
            return;
        }

        if left == 0 && right == self.width {
            // full width: just rotate, rows keep their content and dirty flag
            let slice = &mut self.rows[top..bot];
            if rows > 0 {
                slice.rotate_left(n);
                slice[region - n..].iter_mut().for_each(|r| r.dirty = true);
            } else {
                slice.rotate_right(n);
                slice[..n].iter_mut().for_each(|r| r.dirty = true);
            }
        } else {
            // e.g. vsplits without multigrid, copy just the column range
            if rows > 0 {
                for dst in top..bot - n {
                    self.copy_cols(dst + n, dst, left, right);
                }
                (bot - n..bot).for_each(|r| self.rows[r].dirty = true);
            } else {
                for dst in (top + n..bot).rev() {
                    self.copy_cols(dst - n, dst, left, right);
                }
                (top..top + n).for_each(|r| self.rows[r].dirty = true);
            }
        }
    }

    fn copy_cols(&mut self, src: usize, dst: usize, left: usize, right: usize) {
        let (src_row, dst_row) = if src < dst {
            let (a, b) = self.rows.split_at_mut(dst);
            (&a[src], &mut b[0])
        } else {
            let (a, b) = self.rows.split_at_mut(src);
            (&b[0], &mut a[dst])
        };
        dst_row.cells[left..right].copy_from_slice(&src_row.cells[left..right]);
        dst_row.dirty = true;
    }

    pub fn row_text(&self, row: usize, text: &TextInterner) -> String {
        let mut out = String::with_capacity(self.width);
        for cell in &self.rows[row].cells {
            text.push_to(cell.text, &mut out);
        }
        out
    }
}
