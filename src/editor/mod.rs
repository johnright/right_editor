//! Screen state rebuilt from redraw events, owned by the window thread.
//! Updated a whole batch (up to `flush`) at a time so the renderer never sees
//! a half-applied update.

mod grid;
mod highlight;

use std::collections::HashMap;

pub use grid::{Cell, CellText, Grid, Row, TextInterner};
pub use highlight::HlTable;

use crate::nvim::redraw::{GridId, ModeInfo, RedrawEvent};

/// Drained by the renderer to drive the animations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridChange {
    Scroll {
        grid: GridId,
        top: usize,
        bot: usize,
        left: usize,
        right: usize,
        rows: i64,
        /// false for rows shifted by an edit (Enter, `O`, `dd`), which
        /// shouldn't animate
        animate: bool,
    },
    /// resize, clear, destroy
    Reset { grid: GridId },
    /// Cells [start, end) the cursor moved over since the last flush that got
    /// new text.
    Typed {
        row_id: u64,
        start: usize,
        end: usize,
    },
    /// Cells [start, end) used to be `shift` cells to the left (insertion) or,
    /// with a negative shift, to the right (deletion).
    Slide {
        row_id: u64,
        start: usize,
        end: usize,
        shift: i64,
    },
}

// bigger cursor jumps aren't typing
const MAX_TYPED: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub grid: GridId,
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Window,
    Float { zindex: i64, compindex: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub row: i64,
    pub col: i64,
    pub layer: Layer,
    pub hidden: bool,
    /// floats can opt out of mouse events
    pub mouse: bool,
    /// real floats only, not the message grid
    pub shadow: bool,
}

/// Without multigrid this is the whole screen. With it, it's what sits under
/// the windows: tabline, global statusline.
pub const MAIN_GRID: GridId = 1;

#[derive(Debug, Default)]
pub struct EditorState {
    pub grids: HashMap<GridId, Grid>,
    pub placements: HashMap<GridId, Placement>,
    pub hl: HlTable,
    pub text: TextInterner,
    pub cursor: Cursor,
    pub modes: Vec<ModeInfo>,
    pub mode_idx: usize,
    /// In a `:terminal` the cursor belongs to the shell.
    pub terminal_mode: bool,
    pub busy: bool,
    pub guifont: Option<String>,
    pub linespace: i64,
    pub title: Option<String>,
    /// The renderer drains this every frame.
    pub changes: Vec<GridChange>,
    // (row id, col), since the last flush
    changed_cells: Vec<(u64, usize)>,
    flushed_cursor: Cursor,
    // cursor row as of the last flush, to spot text that slid
    flushed_row: Option<(u64, Vec<Cell>)>,
}

impl EditorState {
    /// true if the batch ends in `flush`, i.e. there's a frame to draw.
    pub fn apply(&mut self, batch: &[RedrawEvent]) -> bool {
        let mut flushed = false;
        let mut viewport_scrolled = false;
        let first_change = self.changes.len();
        for event in batch {
            match event {
                RedrawEvent::GridResize {
                    grid,
                    width,
                    height,
                } => {
                    let (w, h) = (*width as usize, *height as usize);
                    self.changes.push(GridChange::Reset { grid: *grid });
                    self.grids
                        .entry(*grid)
                        .and_modify(|g| g.resize(w, h))
                        .or_insert_with(|| Grid::new(w, h));
                }
                RedrawEvent::GridLine {
                    grid,
                    row,
                    col_start,
                    cells,
                } => {
                    if let Some(g) = Self::grid_mut(&mut self.grids, *grid, "grid_line") {
                        g.line(
                            *row as usize,
                            *col_start as usize,
                            cells,
                            &mut self.text,
                            &mut self.changed_cells,
                        );
                    }
                }
                RedrawEvent::GridScroll {
                    grid,
                    top,
                    bot,
                    left,
                    right,
                    rows,
                } => {
                    // dbg!(grid, top, bot, rows);
                    self.changes.push(GridChange::Scroll {
                        grid: *grid,
                        top: *top as usize,
                        bot: *bot as usize,
                        left: *left as usize,
                        right: *right as usize,
                        rows: *rows,
                        animate: false,
                    });
                    if let Some(g) = Self::grid_mut(&mut self.grids, *grid, "grid_scroll") {
                        g.scroll(
                            *top as usize,
                            *bot as usize,
                            *left as usize,
                            *right as usize,
                            *rows,
                        );
                    }
                }
                RedrawEvent::GridClear { grid } => {
                    self.changes.push(GridChange::Reset { grid: *grid });
                    if let Some(g) = Self::grid_mut(&mut self.grids, *grid, "grid_clear") {
                        g.clear();
                    }
                }
                RedrawEvent::GridDestroy { grid } => {
                    self.changes.push(GridChange::Reset { grid: *grid });
                    self.grids.remove(grid);
                    self.placements.remove(grid);
                }
                RedrawEvent::WinPos { grid, row, col, .. } => {
                    self.place(*grid, *row as i64, *col as i64, Layer::Window, true, false);
                }
                RedrawEvent::WinFloatPos {
                    grid,
                    row,
                    col,
                    zindex,
                    compindex,
                    mouse,
                } => {
                    let layer = Layer::Float {
                        zindex: *zindex,
                        compindex: *compindex,
                    };
                    self.place(*grid, *row, *col, layer, *mouse, true);
                }
                RedrawEvent::MsgSetPos {
                    grid,
                    row,
                    zindex,
                    compindex,
                } => {
                    let layer = Layer::Float {
                        zindex: *zindex,
                        compindex: *compindex,
                    };
                    self.place(*grid, *row as i64, 0, layer, true, false);
                }
                RedrawEvent::WinHide { grid } => {
                    if let Some(p) = self.placements.get_mut(grid) {
                        p.hidden = true;
                    }
                }
                RedrawEvent::WinClose { grid } => {
                    self.placements.remove(grid);
                }
                RedrawEvent::GridCursorGoto { grid, row, col } => {
                    self.cursor = Cursor {
                        grid: *grid,
                        row: *row as usize,
                        col: *col as usize,
                    };
                }
                RedrawEvent::HlAttrDefine { id, attr } => {
                    if self.hl.define(*id, attr.clone()) {
                        self.mark_all_dirty();
                    }
                }
                RedrawEvent::DefaultColorsSet { fg, bg, sp } => {
                    self.hl.set_defaults(*fg, *bg, *sp);
                    self.mark_all_dirty();
                }
                RedrawEvent::ModeInfoSet { modes, .. } => self.modes = modes.clone(),
                RedrawEvent::ModeChange { name, mode_idx } => {
                    self.mode_idx = *mode_idx;
                    self.terminal_mode = name == "terminal";
                }
                RedrawEvent::BusyStart => self.busy = true,
                RedrawEvent::BusyStop => self.busy = false,
                RedrawEvent::GuiFont(font) => {
                    self.guifont = (!font.is_empty()).then(|| font.clone())
                }
                RedrawEvent::LineSpace(px) => self.linespace = *px,
                RedrawEvent::SetTitle(title) => self.title = Some(title.clone()),
                RedrawEvent::WinViewport { scroll_delta, .. } => {
                    viewport_scrolled |= *scroll_delta != 0;
                }
                RedrawEvent::Flush => {
                    flushed = true;
                    self.detect_edits();
                }
                RedrawEvent::Restart(_) | RedrawEvent::Unhandled(_) => {}
            }
        }
        // win_viewport comes after the grid_scrolls it belongs to, so we only
        // know here whether they were a real scroll or just rows pushed around
        // by Enter/`O`/`dd`.
        if viewport_scrolled {
            for change in &mut self.changes[first_change..] {
                if let GridChange::Scroll { animate, .. } = change {
                    *animate = true;
                }
            }
        }
        flushed
    }

    /// Runs at flush, cursor row only. Changed cells the cursor moved over are
    /// typed text. Text right of the cursor that moved by the same amount as
    /// the cursor is a slide (insert/delete mid-line).
    fn detect_edits(&mut self) {
        let (prev, cur) = (self.flushed_cursor, self.cursor);
        self.flushed_cursor = cur;
        let mut changed = std::mem::take(&mut self.changed_cells);
        let old = self.flushed_row.take();
        let Some(row) = self.grids.get(&cur.grid).and_then(|g| g.rows.get(cur.row)) else {
            return;
        };
        let same_row = prev.grid == cur.grid && prev.row == cur.row;
        if same_row && cur.col > prev.col && cur.col - prev.col <= MAX_TYPED {
            let cols = changed
                .iter()
                .filter(|&&(id, col)| id == row.id && (prev.col..cur.col).contains(&col))
                .map(|&(_, col)| col);
            if let (Some(first), Some(last)) = (cols.clone().min(), cols.max()) {
                self.changes.push(GridChange::Typed {
                    row_id: row.id,
                    start: first,
                    end: last + 1,
                });
            }
        }
        if same_row
            && changed.iter().any(|&(id, _)| id == row.id)
            && cur.col != prev.col
            && cur.col.abs_diff(prev.col) <= MAX_TYPED
            && let Some((old_id, old_cells)) = &old
            && *old_id == row.id
        {
            // how much of what followed the old cursor now follows the new one
            let len = old_cells[prev.col.min(old_cells.len())..]
                .iter()
                .zip(&row.cells[cur.col.min(row.cells.len())..])
                .take_while(|(a, b)| a.text == b.text)
                .count();
            // trailing spaces don't need to slide
            let tail = &row.cells[cur.col..cur.col + len];
            let len = tail
                .iter()
                .rposition(|c| c.text != CellText::SPACE)
                .map_or(0, |last| last + 1);
            if len > 0 {
                self.changes.push(GridChange::Slide {
                    row_id: row.id,
                    start: cur.col,
                    end: cur.col + len,
                    shift: cur.col as i64 - prev.col as i64,
                });
            }
        }
        self.flushed_row = Some((row.id, row.cells.clone()));
        changed.clear();
        self.changed_cells = changed;
    }

    fn place(&mut self, grid: GridId, row: i64, col: i64, layer: Layer, mouse: bool, shadow: bool) {
        let placement = Placement {
            row,
            col,
            layer,
            hidden: false,
            mouse,
            shadow,
        };
        if self.placements.insert(grid, placement) != Some(placement) {
            // moved or shown again, pending scroll animations are stale
            self.changes.push(GridChange::Reset { grid });
        }
    }

    /// Bottom to top with origins in cells: grid 1, windows, then floats and
    /// messages by (zindex, compindex).
    pub fn draw_order(&self) -> Vec<(GridId, i64, i64)> {
        let mut order = Vec::with_capacity(self.placements.len() + 1);
        if self.grids.contains_key(&MAIN_GRID) {
            order.push((MAIN_GRID, 0, 0));
        }
        let mut layered: Vec<(GridId, &Placement)> = self
            .placements
            .iter()
            .filter(|(id, p)| !p.hidden && **id != MAIN_GRID && self.grids.contains_key(id))
            .map(|(id, p)| (*id, p))
            .collect();
        layered.sort_by_key(|(id, p)| match p.layer {
            Layer::Window => (0, i64::MIN, i64::MIN, *id),
            Layer::Float { zindex, compindex } => (1, zindex, compindex, *id),
        });
        order.extend(layered.into_iter().map(|(id, p)| (id, p.row, p.col)));
        order
    }

    /// Topmost grid taking mouse input at a screen cell, plus the cell in
    /// that grid's coordinates.
    pub fn grid_at(&self, row: i64, col: i64) -> Option<(GridId, i64, i64)> {
        self.draw_order()
            .into_iter()
            .rev()
            .find_map(|(id, top, left)| {
                if self.placements.get(&id).is_some_and(|p| !p.mouse) {
                    return None;
                }
                let grid = &self.grids[&id];
                let (r, c) = (row - top, col - left);
                (r >= 0 && c >= 0 && r < grid.height() as i64 && c < grid.width() as i64)
                    .then_some((id, r, c))
            })
    }

    pub fn cursor_mode(&self) -> ModeInfo {
        self.modes.get(self.mode_idx).copied().unwrap_or_default()
    }

    fn grid_mut<'a>(
        grids: &'a mut HashMap<GridId, Grid>,
        id: GridId,
        event: &str,
    ) -> Option<&'a mut Grid> {
        let grid = grids.get_mut(&id);
        if grid.is_none() {
            log::warn!("{event} on missing grid {id}");
        }
        grid
    }

    pub fn mark_all_dirty(&mut self) {
        self.grids.values_mut().for_each(Grid::mark_all_dirty);
    }

    pub fn clear_dirty(&mut self) {
        for grid in self.grids.values_mut() {
            grid.rows.iter_mut().for_each(|r| r.dirty = false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim::redraw::GridLineCell;

    fn cell(text: &str, hl: Option<u64>, repeat: u64) -> GridLineCell {
        GridLineCell {
            text: text.into(),
            hl_id: hl,
            repeat,
        }
    }

    fn line(row: u64, col: u64, text: &str) -> RedrawEvent {
        RedrawEvent::GridLine {
            grid: 1,
            row,
            col_start: col,
            cells: text
                .chars()
                .map(|c| cell(&c.to_string(), Some(0), 1))
                .collect(),
        }
    }

    fn state(width: u64, rows: &[&str]) -> EditorState {
        let mut s = EditorState::default();
        let mut batch = vec![RedrawEvent::GridResize {
            grid: 1,
            width,
            height: rows.len() as u64,
        }];
        batch.extend(rows.iter().enumerate().map(|(i, r)| line(i as u64, 0, r)));
        batch.push(RedrawEvent::Flush);
        assert!(s.apply(&batch));
        s.clear_dirty();
        s
    }

    fn texts(s: &EditorState) -> Vec<String> {
        let g = &s.grids[&1];
        (0..g.height()).map(|r| g.row_text(r, &s.text)).collect()
    }

    fn dirty(s: &EditorState) -> Vec<bool> {
        s.grids[&1].rows.iter().map(|r| r.dirty).collect()
    }

    fn goto(row: u64, col: u64) -> RedrawEvent {
        RedrawEvent::GridCursorGoto { grid: 1, row, col }
    }

    fn typed(s: &mut EditorState) -> Vec<(usize, usize)> {
        s.changes
            .drain(..)
            .filter_map(|c| match c {
                GridChange::Typed { start, end, .. } => Some((start, end)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn typed_text_is_what_the_cursor_passed_over() {
        let mut s = state(10, &["foo(bar)  "]);
        s.apply(&[goto(0, 4), RedrawEvent::Flush]);
        s.changes.clear();
        // `x` mid-line redraws the rest of the row, only one cell is typed
        s.apply(&[line(0, 4, "xbar)"), goto(0, 5), RedrawEvent::Flush]);
        assert_eq!(typed(&mut s), [(4, 5)]);
        // accepted completion
        s.apply(&[line(0, 5, "yzw"), goto(0, 8), RedrawEvent::Flush]);
        assert_eq!(typed(&mut s), [(5, 8)]);
    }

    fn slides(s: &mut EditorState) -> Vec<(usize, usize, i64)> {
        s.changes
            .drain(..)
            .filter_map(|c| match c {
                GridChange::Slide {
                    start, end, shift, ..
                } => Some((start, end, shift)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_right_of_the_cursor_slides() {
        let mut s = state(10, &["foo(bar)  "]);
        s.apply(&[goto(0, 4), RedrawEvent::Flush]);
        s.changes.clear();
        s.apply(&[line(0, 4, "xbar)"), goto(0, 5), RedrawEvent::Flush]);
        assert_eq!(slides(&mut s), [(5, 9, 1)]);
        // backspace
        s.apply(&[line(0, 4, "bar) "), goto(0, 4), RedrawEvent::Flush]);
        assert_eq!(slides(&mut s), [(4, 8, -1)]);
        // end of line, nothing to slide
        s.apply(&[goto(0, 8), RedrawEvent::Flush]);
        s.apply(&[line(0, 8, "z"), goto(0, 9), RedrawEvent::Flush]);
        assert_eq!(slides(&mut s), []);
    }

    #[test]
    fn terminal_mode_follows_mode_change() {
        let mut s = state(4, &["    "]);
        let mode = |name: &str| RedrawEvent::ModeChange {
            name: name.into(),
            mode_idx: 0,
        };
        s.apply(&[mode("terminal")]);
        assert!(s.terminal_mode);
        s.apply(&[mode("normal")]);
        assert!(!s.terminal_mode);
    }

    #[test]
    fn moves_without_new_text_are_not_typing() {
        let mut s = state(10, &["foo bar   "]);
        s.apply(&[goto(0, 0), RedrawEvent::Flush]);
        // `w`
        s.apply(&[line(0, 0, "foo bar"), goto(0, 4), RedrawEvent::Flush]);
        assert_eq!(typed(&mut s), []);
        s.apply(&[line(0, 0, "fooxbar"), goto(0, 2), RedrawEvent::Flush]);
        assert_eq!(typed(&mut s), []);
        // only changes since the previous flush count
        s.apply(&[goto(0, 6), RedrawEvent::Flush]);
        assert_eq!(typed(&mut s), []);
    }

    #[test]
    fn grid_line_repeat_hl_and_wide_chars() {
        let mut s = state(8, &["        "]);
        s.apply(&[RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            col_start: 1,
            cells: vec![
                cell("a", Some(7), 1),
                cell("b", None, 2),
                cell("界", Some(2), 1),
                cell("", None, 1),
            ],
        }]);
        assert_eq!(texts(&s), [" abb界  "]);
        let cells = &s.grids[&1].rows[0].cells;
        assert_eq!(
            cells.iter().map(|c| c.hl).collect::<Vec<_>>(),
            [0, 7, 7, 7, 2, 2, 0, 0]
        );
        assert_eq!(cells[5].text, CellText::EMPTY);
        assert_eq!(dirty(&s), [true]);
    }

    #[test]
    fn multi_codepoint_clusters_are_interned() {
        let mut s = state(4, &["    "]);
        s.apply(&[RedrawEvent::GridLine {
            grid: 1,
            row: 0,
            col_start: 0,
            cells: vec![cell("e\u{301}", Some(0), 2)],
        }]);
        assert_eq!(texts(&s), ["e\u{301}e\u{301}  "]);
        let cells = &s.grids[&1].rows[0].cells;
        assert_eq!(cells[0].text, cells[1].text);
    }

    #[test]
    fn full_width_scroll_rotates_rows() {
        let mut s = state(3, &["aaa", "bbb", "ccc", "ddd"]);
        let ids: Vec<u64> = s.grids[&1].rows.iter().map(|r| r.id).collect();
        s.apply(&[RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bot: 3,
            left: 0,
            right: 3,
            rows: 1,
        }]);
        assert_eq!(&texts(&s)[..2], ["bbb", "ccc"]);
        assert_eq!(texts(&s)[3], "ddd");
        assert_eq!(s.grids[&1].rows[0].id, ids[1]);
        assert_eq!(dirty(&s), [false, false, true, false]);

        s.apply(&[RedrawEvent::GridScroll {
            grid: 1,
            top: 1,
            bot: 4,
            left: 0,
            right: 3,
            rows: -2,
        }]);
        assert_eq!(texts(&s)[3], "ccc");
        assert_eq!(dirty(&s), [false, true, true, false]);
    }

    #[test]
    fn partial_scroll_copies_columns() {
        let mut s = state(4, &["a1b1", "a2b2", "a3b3"]);
        s.apply(&[RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bot: 3,
            left: 2,
            right: 4,
            rows: 1,
        }]);
        assert_eq!(&texts(&s)[..2], ["a1b2", "a2b3"]);
        s.apply(&[RedrawEvent::GridScroll {
            grid: 1,
            top: 0,
            bot: 3,
            left: 0,
            right: 2,
            rows: -1,
        }]);
        assert_eq!(&texts(&s)[1..], ["a1b3", "a2b3"]);
    }

    #[test]
    fn resize_clear_and_destroy() {
        let mut s = state(3, &["abc", "def"]);
        s.apply(&[RedrawEvent::GridResize {
            grid: 1,
            width: 4,
            height: 3,
        }]);
        assert_eq!(texts(&s), ["abc ", "def ", "    "]);
        s.apply(&[RedrawEvent::GridResize {
            grid: 1,
            width: 2,
            height: 1,
        }]);
        assert_eq!(texts(&s), ["ab"]);
        s.apply(&[RedrawEvent::GridClear { grid: 1 }]);
        assert_eq!(texts(&s), ["  "]);
        s.apply(&[RedrawEvent::GridDestroy { grid: 1 }]);
        assert!(s.grids.is_empty());
        // must not panic on a destroyed grid
        assert!(s.apply(&[line(0, 0, "x"), RedrawEvent::Flush]));
    }

    #[test]
    fn only_viewport_scrolls_are_animated() {
        let mut s = state(3, &["aaa", "bbb", "ccc", "ddd"]);
        let scroll = RedrawEvent::GridScroll {
            grid: 1,
            top: 1,
            bot: 4,
            left: 0,
            right: 3,
            rows: -1,
        };
        // Enter / `O`
        s.apply(&[
            scroll.clone(),
            RedrawEvent::WinViewport {
                grid: 2,
                scroll_delta: 0,
            },
        ]);
        // wheel
        s.apply(&[
            scroll,
            RedrawEvent::WinViewport {
                grid: 2,
                scroll_delta: 1,
            },
        ]);
        let animated: Vec<bool> = s
            .changes
            .iter()
            .filter_map(|c| match c {
                GridChange::Scroll { animate, .. } => Some(*animate),
                _ => None,
            })
            .collect();
        assert_eq!(animated, [false, true]);
    }

    #[test]
    fn multigrid_draw_order_and_hit_testing() {
        let mut s = EditorState::default();
        let resize = |grid, width, height| RedrawEvent::GridResize {
            grid,
            width,
            height,
        };
        s.apply(&[
            resize(1, 80, 24),
            resize(2, 80, 22),
            resize(4, 20, 5),
            resize(5, 80, 1),
            RedrawEvent::WinPos {
                grid: 2,
                row: 1,
                col: 0,
                width: 80,
                height: 22,
            },
            RedrawEvent::WinFloatPos {
                grid: 4,
                row: 3,
                col: 10,
                zindex: 50,
                compindex: 2,
                mouse: true,
            },
            RedrawEvent::MsgSetPos {
                grid: 5,
                row: 23,
                zindex: 200,
                compindex: 3,
            },
        ]);
        assert_eq!(
            s.draw_order(),
            [(1, 0, 0), (2, 1, 0), (4, 3, 10), (5, 23, 0)]
        );
        assert_eq!(s.grid_at(4, 12), Some((4, 1, 2)));
        assert_eq!(s.grid_at(10, 40), Some((2, 9, 40)));
        assert_eq!(s.grid_at(0, 5), Some((1, 0, 5)));
        // hidden floats get neither drawn nor clicked
        s.apply(&[RedrawEvent::WinHide { grid: 4 }]);
        assert_eq!(s.grid_at(4, 12), Some((2, 3, 12)));
        assert_eq!(s.draw_order().len(), 3);
        s.apply(&[RedrawEvent::GridDestroy { grid: 4 }]);
        assert!(!s.placements.contains_key(&4));
    }

    #[test]
    fn hl_redefinition_and_default_colors_dirty_everything() {
        let mut s = state(2, &["ab", "cd"]);
        s.apply(&[RedrawEvent::HlAttrDefine {
            id: 5,
            attr: Default::default(),
        }]);
        assert_eq!(dirty(&s), [false, false]);
        s.apply(&[RedrawEvent::HlAttrDefine {
            id: 5,
            attr: Default::default(),
        }]);
        assert_eq!(dirty(&s), [true, true]);
        s.clear_dirty();
        s.apply(&[RedrawEvent::DefaultColorsSet {
            fg: 1,
            bg: 2,
            sp: 3,
        }]);
        assert_eq!(dirty(&s), [true, true]);
        assert_eq!(s.hl.defaults().bg, 2);
    }
}
