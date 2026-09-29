//! The grid: what a shell's output looks like once it has been parsed.
//!
//! A [`Screen`] is a rectangular grid of [`Cell`]s plus a cursor, a bounded
//! scrollback of lines that have scrolled off the top, and the current title.
//! It is the thing a renderer draws: feed it the bytes from a terminal stream
//! with [`Screen::feed`], then read rows out whenever it is time to draw.
//!
//! ## What is deliberately not here
//!
//! Scroll regions (DECSTBM) are ignored, so a full-screen application that
//! repaints through them still works but scrolls whole-screen rather than
//! within its margins. Alternate-screen modes are ignored for the same reason
//! — the grid always reflects the last thing written. Both matter for `vim`
//! or `tmux`, neither matters for a shell, and half-supporting either would
//! produce screens that lie about where their content went.

use std::collections::VecDeque;

use crate::cell::{Attrs, Cell, Color};

/// How many scrolled-off lines are kept before the oldest start falling off.
///
/// Ten thousand lines is several sessions of heavy use and about a megabyte
/// per terminal at most — bounded on purpose, because an unbounded scrollback
/// turns a runaway `yes` into a memory leak the person has to notice.
pub const SCROLLBACK_LINES: usize = 10_000;

/// Where the cursor is and whether it should be drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
}

/// The mutable state one VT stream paints onto.
#[derive(Debug)]
pub(crate) struct Grid {
    cols: u16,
    rows: u16,
    /// Row-major, `rows * cols`. A flat vector rather than nested vectors so
    /// scrolling is one `copy_within` instead of a drain per line.
    cells: Vec<Cell>,
    cursor_col: u16,
    cursor_row: u16,
    /// Set when output reached the right edge exactly, cleared by anything
    /// that moves the cursor. This is the xterm deferred-wrap behaviour: a
    /// character printed in the last column does not wrap immediately, so a
    /// program that writes exactly `cols` characters then a newline does not
    /// produce a blank line.
    pending_wrap: bool,
    cursor_visible: bool,
    saved_cursor: Option<(u16, u16)>,
    scrollback: VecDeque<Box<[Cell]>>,
    title: String,
    /// Current SGR state, applied to everything printed until changed again.
    fg: Color,
    bg: Color,
    attrs: Attrs,
}

impl Grid {
    pub(crate) fn new(cols: u16, rows: u16) -> Grid {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cursor_col: 0,
            cursor_row: 0,
            pending_wrap: false,
            cursor_visible: true,
            saved_cursor: None,
            scrollback: VecDeque::new(),
            title: String::new(),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::default(),
        }
    }

    // ------------------------------------------------------------- reading

    pub(crate) fn cols(&self) -> u16 {
        self.cols
    }

    pub(crate) fn rows(&self) -> u16 {
        self.rows
    }

    pub(crate) fn cursor(&self) -> Cursor {
        Cursor {
            col: self.cursor_col,
            row: self.cursor_row,
            visible: self.cursor_visible,
        }
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn cell(&self, col: u16, row: u16) -> Cell {
        self.row_slice(row)[col as usize]
    }

    pub(crate) fn line(&self, row: u16) -> &[Cell] {
        self.row_slice(row)
    }

    pub(crate) fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// A scrolled-off line, oldest first. Lines keep the width they had when
    /// they scrolled, which may differ from the current width after a resize.
    pub(crate) fn scrollback_line(&self, index: usize) -> Option<&[Cell]> {
        self.scrollback.get(index).map(|line| &**line)
    }

    fn row_slice(&self, row: u16) -> &[Cell] {
        let start = row as usize * self.cols as usize;
        &self.cells[start..start + self.cols as usize]
    }

    fn row_mut(&mut self, row: u16) -> &mut [Cell] {
        let cols = self.cols as usize;
        let start = row as usize * cols;
        &mut self.cells[start..start + cols]
    }

    // ------------------------------------------------------------- writing

    /// One printable glyph at the cursor, honouring deferred wrap.
    pub(crate) fn print(&mut self, ch: char) {
        if self.pending_wrap {
            self.pending_wrap = false;
            self.cursor_col = 0;
            self.index();
        }
        let cell = Cell {
            ch,
            fg: self.fg,
            bg: self.bg,
            attrs: self.attrs,
        };
        let col = self.cursor_col as usize;
        self.row_mut(self.cursor_row)[col] = cell;
        if self.cursor_col + 1 == self.cols {
            self.pending_wrap = true;
        } else {
            self.cursor_col += 1;
        }
    }

    /// C0 controls that move the cursor or scroll. Everything else arrives
    /// already filtered by the parser.
    pub(crate) fn execute(&mut self, byte: u8) {
        match byte {
            // Backspace never wraps upward; terminals that did were hated.
            0x08 => {
                self.pending_wrap = false;
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
            // Tab stops every eight columns, clamped to the edge rather than
            // allowed to park the cursor outside the grid.
            0x09 => {
                self.pending_wrap = false;
                let next = (self.cursor_col / 8 + 1) * 8;
                self.cursor_col = next.min(self.cols - 1);
            }
            // VT and FF mean newline to every terminal anyone has shipped.
            0x0A | 0x0B | 0x0C => {
                self.index();
                self.pending_wrap = false;
            }
            0x0D => {
                self.cursor_col = 0;
                self.pending_wrap = false;
            }
            _ => {}
        }
    }

    /// Move down one row, scrolling when already on the bottom.
    pub(crate) fn index(&mut self) {
        if self.cursor_row == self.rows - 1 {
            self.scroll_up(1);
        } else {
            self.cursor_row += 1;
        }
    }

    /// Move up one row, scrolling the other way when already on the top.
    pub(crate) fn reverse_index(&mut self) {
        if self.cursor_row == 0 {
            self.scroll_down(1);
        } else {
            self.cursor_row -= 1;
        }
    }

    /// Drop `n` lines off the top: they go to scrollback, everything moves
    /// up, blank lines arrive at the bottom.
    pub(crate) fn scroll_up(&mut self, n: u16) {
        for _ in 0..n {
            let top: Box<[Cell]> = self.row_slice(0).into();
            self.scrollback.push_back(top);
            while self.scrollback.len() > SCROLLBACK_LINES {
                self.scrollback.pop_front();
            }
            let cols = self.cols as usize;
            self.cells.copy_within(cols.., 0);
            let tail = self.cells.len() - cols;
            self.cells[tail..].fill(Cell::default());
        }
    }

    /// Push everything down `n` lines, blanking at the top. Nothing goes to
    /// scrollback: content moving *down* the screen was already visible.
    pub(crate) fn scroll_down(&mut self, n: u16) {
        for _ in 0..n {
            let cols = self.cols as usize;
            let len = self.cells.len();
            self.cells.copy_within(..len - cols, cols);
            self.cells[..cols].fill(Cell::default());
        }
    }

    pub(crate) fn move_to(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols - 1);
        self.cursor_row = row.min(self.rows - 1);
        self.pending_wrap = false;
    }

    pub(crate) fn move_col_to(&mut self, col: u16) {
        self.cursor_col = col.min(self.cols - 1);
        self.pending_wrap = false;
    }

    pub(crate) fn move_row_to(&mut self, row: u16) {
        self.cursor_row = row.min(self.rows - 1);
        self.pending_wrap = false;
    }

    pub(crate) fn move_by(&mut self, dcol: i32, drow: i32) {
        let col = self.cursor_col as i32 + dcol;
        let row = self.cursor_row as i32 + drow;
        self.cursor_col = col.clamp(0, self.cols as i32 - 1) as u16;
        self.cursor_row = row.clamp(0, self.rows as i32 - 1) as u16;
        self.pending_wrap = false;
    }

    pub(crate) fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_col, self.cursor_row));
    }

    pub(crate) fn restore_cursor(&mut self) {
        // Restoring before anything was saved lands in the top left, which is
        // what real terminals do rather than what a Rust default would do.
        let (col, row) = self.saved_cursor.unwrap_or((0, 0));
        self.move_to(col, row);
    }

    pub(crate) fn set_title(&mut self, title: String) {
        self.title = title;
    }

    pub(crate) fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    pub(crate) fn set_fg(&mut self, color: Color) {
        self.fg = color;
    }

    pub(crate) fn set_bg(&mut self, color: Color) {
        self.bg = color;
    }

    pub(crate) fn set_attrs(&mut self, attrs: Attrs) {
        self.attrs = attrs;
    }

    pub(crate) fn attrs(&self) -> Attrs {
        self.attrs
    }

    /// Back to the theme defaults for everything printed next.
    pub(crate) fn reset_pen(&mut self) {
        self.fg = Color::Default;
        self.bg = Color::Default;
        self.attrs = Attrs::default();
    }

    /// Erase part or all of the screen (SGR-independent `ED`, parameter
    /// `mode`): 0 below the cursor, 1 above it, 2 everything, 3 everything
    /// plus scrollback.
    pub(crate) fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                let row = self.cursor_row;
                let col = self.cursor_col;
                self.erase_in_row(row, col, self.cols);
                self.erase_rows(row + 1, self.rows - 1);
            }
            1 => {
                let row = self.cursor_row;
                self.erase_in_row(row, 0, self.cursor_col.saturating_add(1));
                if row > 0 {
                    self.erase_rows(0, row - 1);
                }
            }
            2 | 3 => {
                self.cells.fill(Cell::default());
                if mode == 3 {
                    self.clear_scrollback();
                }
            }
            _ => {}
        }
    }

    /// Erase part of the cursor's row (`EL`): 0 rightward, 1 leftward
    /// including the cursor, 2 the whole row.
    pub(crate) fn erase_line(&mut self, mode: u16) {
        match mode {
            0 => {
                let col = self.cursor_col;
                self.erase_in_row(self.cursor_row, col, self.cols);
            }
            1 => {
                self.erase_in_row(self.cursor_row, 0, self.cursor_col.saturating_add(1));
            }
            2 => self.erase_in_row(self.cursor_row, 0, self.cols),
            _ => {}
        }
    }

    /// Erase columns `start..end` of one row without moving the cursor.
    pub(crate) fn erase_in_row(&mut self, row: u16, start: u16, end: u16) {
        let start = start.min(self.cols);
        let end = end.clamp(start, self.cols);
        self.row_mut(row)[start as usize..end as usize].fill(Cell::default());
    }

    /// Erase whole rows, inclusive, without touching the cursor.
    pub(crate) fn erase_rows(&mut self, from: u16, to: u16) {
        for row in from..=to.min(self.rows - 1) {
            self.row_mut(row).fill(Cell::default());
        }
    }

    /// Delete `n` characters under and after the cursor; the rest of the row
    /// shifts left and blanks appear at the end.
    pub(crate) fn delete_chars(&mut self, n: u16) {
        let col = self.cursor_col as usize;
        let row = self.row_mut(self.cursor_row);
        let n = (n as usize).min(row.len().saturating_sub(col));
        row.copy_within(col + n.., col);
        let fill_from = row.len() - n;
        row[fill_from..].fill(Cell::default());
    }

    /// Insert `n` blanks at the cursor; the rest of the row shifts right and
    /// whatever falls off the end is gone.
    pub(crate) fn insert_chars(&mut self, n: u16) {
        let row_index = self.cursor_row;
        let col = self.cursor_col as usize;
        let n = (n as usize).min(self.cols as usize - col);
        let mut row: Vec<Cell> = self.row_slice(row_index).to_vec();
        row.splice(col..col, std::iter::repeat(Cell::default()).take(n));
        let width = self.cols as usize;
        self.row_mut(row_index).clone_from_slice(&row[..width]);
    }

    /// Insert `n` blank lines at the cursor row; lines below shift down and
    /// the ones pushed past the bottom are gone. The column resets, matching
    /// every real terminal's IL/DL behaviour.
    pub(crate) fn insert_lines(&mut self, n: u16) {
        for _ in 0..n {
            let bottom = self.rows - 1;
            for row in (self.cursor_row..bottom).rev() {
                let (a, b) = (row as usize * self.cols as usize, (row + 1) as usize * self.cols as usize);
                self.cells.copy_within(a..b, b);
            }
            self.row_mut(self.cursor_row).fill(Cell::default());
        }
        self.cursor_col = 0;
        self.pending_wrap = false;
    }

    /// Delete `n` lines at the cursor row; lines below shift up and blanks
    /// arrive at the bottom.
    pub(crate) fn delete_lines(&mut self, n: u16) {
        for _ in 0..n {
            let bottom = self.rows - 1;
            for row in self.cursor_row..bottom {
                let (a, b) = (row as usize * self.cols as usize, (row + 1) as usize * self.cols as usize);
                self.cells.copy_within(b..b + self.cols as usize, a);
            }
            self.row_mut(bottom).fill(Cell::default());
        }
        self.cursor_col = 0;
        self.pending_wrap = false;
    }

    /// Forget every line in scrollback. SGR 3 (`ED` with parameter 3), which
    /// programs use for "clear including history".
    pub(crate) fn clear_scrollback(&mut self) {
        self.scrollback.clear();
    }

    /// Grow or shrink the pane, keeping content anchored at the top left.
    ///
    /// No reflow: shrinking drops the right-hand ends of lines rather than
    /// wrapping them, because reflowing would move text the user's eye is on
    /// and there is no way to do that without also inventing scroll positions.
    /// Shells redraw themselves on SIGWINCH anyway.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut cells = vec![Cell::default(); cols as usize * rows as usize];
        let copy_cols = cols.min(self.cols) as usize;
        let copy_rows = rows.min(self.rows) as usize;
        for row in 0..copy_rows {
            let src = row * self.cols as usize;
            let dst = row * cols as usize;
            cells[dst..dst + copy_cols].clone_from_slice(&self.cells[src..src + copy_cols]);
        }
        self.cells = cells;
        self.cols = cols;
        self.rows = rows;
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.pending_wrap = false;
    }

    /// Back to how a fresh screen started: blank grid, empty history, home
    /// cursor. What RIS promises.
    pub(crate) fn reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        *self = Grid::new(cols, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Screen;
    use crate::cell::Color;

    /// A fresh 80x24 screen, the size every terminal test assumes.
    fn screen() -> Screen {
        Screen::new(80, 24)
    }

    fn text(screen: &Screen, row: u16) -> String {
        let mut out = String::new();
        for cell in screen.line(row) {
            out.push(cell.ch);
        }
        out.trim_end().to_owned()
    }

    #[test]
    fn truecolour_survives_parsing_untouched() {
        // The reason this crate exists as more than a cursor mover: a shell
        // that says (255, 100, 5) must be drawn (255, 100, 5), not the nearest
        // palette entry some intermediate stage thought was close enough.
        let mut screen = screen();
        screen.feed(b"\x1b[38;2;255;100;5mX");

        let cell = screen.cell(0, 0);
        assert_eq!(cell.fg, Color::Rgb(255, 100, 5));
        assert_eq!(cell.bg, Color::Default);
        assert_eq!(cell.ch, 'X');
    }

    #[test]
    fn truecolour_backgrounds_and_the_colon_spelling_land_in_the_same_place() {
        let mut screen = screen();
        screen.feed(b"\x1b[48;2;1;2;3;38:2:9:8:7mY");

        let cell = screen.cell(0, 0);
        assert_eq!(cell.bg, Color::Rgb(1, 2, 3));
        assert_eq!(cell.fg, Color::Rgb(9, 8, 7));
        assert_eq!(cell.ch, 'Y');
    }

    #[test]
    fn a_chunk_cut_mid_sequence_is_completed_by_the_next_one() {
        // The wire cuts chunks wherever a read lands. This cut is inside the
        // SGR parameters, which is the common case for fast output.
        let mut screen = screen();
        screen.feed(b"\x1b[38;2;250;10");
        screen.feed(b"0;200mZ");
        assert_eq!(screen.cell(0, 0).fg, Color::Rgb(250, 100, 200));
    }

    #[test]
    fn indexed_colour_names_palette_slots() {
        let mut screen = screen();
        screen.feed(b"\x1b[31m\x1b[44mA\x1b[91mB");
        assert_eq!(screen.cell(0, 0).fg, Color::Indexed(1));
        assert_eq!(screen.cell(0, 0).bg, Color::Indexed(4));

        // Bright red is slot 8 of the bright half, not slot 1 again.
        assert_eq!(screen.cell(1, 0).fg, Color::Indexed(9));
    }

    #[test]
    fn attributes_arrive_and_leave_individually() {
        let mut screen = screen();
        screen.feed(b"\x1b[1;3;4;7mW");
        let cell = screen.cell(0, 0);
        assert_eq!(
            cell.attrs,
            Attrs::BOLD | Attrs::ITALIC | Attrs::UNDERLINE | Attrs::REVERSE
        );

        // Turning bold off leaves the rest alone.
        screen.feed(b"\x1b[22mX");
        let cell = screen.cell(1, 0);
        assert_eq!(cell.attrs, Attrs::ITALIC | Attrs::UNDERLINE | Attrs::REVERSE);

        screen.feed(b"\x1b[mY");
        assert_eq!(screen.cell(2, 0).attrs, Attrs::default());
        assert_eq!(screen.cell(2, 0).fg, Color::Default);
    }

    #[test]
    fn the_cursor_moves_where_cup_points_and_no_further() {
        let mut screen = screen();
        screen.feed(b"\x1b[10;20H");
        assert_eq!(screen.cursor(), Cursor { col: 19, row: 9, visible: true });

        // Off-screen requests clamp to the edge rather than corrupting the
        // grid or panicking mid-render.
        screen.feed(b"\x1b[999;999H");
        assert_eq!(screen.cursor().col, 79);
        assert_eq!(screen.cursor().row, 23);
    }

    #[test]
    fn relative_movement_stops_at_the_edges() {
        let mut screen = screen();
        screen.feed(b"\x1b[2;2H");
        screen.feed(b"\x1b[5A"); // up five from row 1 clamps to row 0
        screen.feed(b"\x1b[3D"); // left three from column 1 clamps to column 0
        assert_eq!(screen.cursor(), Cursor { col: 0, row: 0, visible: true });

        screen.feed(b"\x1b[500C"); // right five hundred clamps to the edge
        assert_eq!(screen.cursor().col, 79);
    }

    #[test]
    fn newline_at_the_bottom_scrolls_instead_of_moving() {
        let mut screen = screen();
        screen.feed(b"top\r\nbottom");
        assert_eq!(text(&screen, 0), "top");
        assert_eq!(text(&screen, 1), "bottom");

        // Now fill to the bottom and push one more line through. Twenty-three
        // newlines from row 1: twenty-two to reach the last row, the
        // twenty-third to scroll "top" into history.
        screen.feed(b"\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\nmore");
        assert_eq!(text(&screen, 23), "more");
        assert_eq!(screen.scrollback_len(), 1);
        assert_eq!(
            screen
                .scrollback_line(0)
                .unwrap()
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end(),
            "top"
        );
    }

    #[test]
    fn printing_past_the_right_edge_defers_then_wraps() {
        let mut screen = Screen::new(4, 2);
        screen.feed(b"abcd");
        // Nothing has wrapped yet: the last character sits in the last column.
        assert_eq!(screen.cursor().col, 3);
        screen.feed(b"\r\nnext");
        // The deferred wrap means `abcd` never produced a blank line.
        assert_eq!(text(&screen, 0), "abcd");
        assert_eq!(text(&screen, 1), "next");
    }

    #[test]
    fn erasing_to_the_end_of_the_line_leaves_the_rest() {
        let mut screen = screen();
        screen.feed(b"hello world");
        screen.feed(b"\x1b[1;6H\x1b[K");
        assert_eq!(text(&screen, 0), "hello");

        // Erase leftward including the cursor: the caret sits on the third
        // cell, so three characters go.
        screen.feed(b"\x1b[1;3H\x1b[1K");
        let row = text(&screen, 0);
        assert!(row.starts_with("   lo"), "{row}");
    }

    #[test]
    fn clearing_the_screen_keeps_the_cursor_and_the_history() {
        let mut screen = screen();
        screen.feed(b"before\r\nafter\x1b[2J");
        assert_eq!(text(&screen, 0), "");
        assert_eq!(screen.scrollback_len(), 0, "ED 2 clears the pane only");
        assert_eq!(screen.cursor().row, 1, "the cursor does not move on ED 2");

        // Mode 3 is the one that means history too.
        screen.feed(b"x\r\ny\x1b[3J");
        assert_eq!(screen.scrollback_len(), 0);
    }

    #[test]
    fn scrollback_is_bounded_so_a_runaway_shell_is_not_a_memory_leak() {
        let mut screen = Screen::new(10, 2);
        let lines = SCROLLBACK_LINES as u32 + 50;
        for _ in 0..lines {
            screen.feed(b"\r\n");
        }
        assert_eq!(
            screen.scrollback_len(),
            SCROLLBACK_LINES,
            "scrollback grew past its bound"
        );
        // The oldest lines fell off the front, not the back.
        assert_ne!(
            screen.scrollback_line(0).unwrap()[0].ch,
            '\u{0}',
            "a blank placeholder replaced real history"
        );
    }

    #[test]
    fn the_title_arrives_through_osc_and_other_codes_are_ignored() {
        let mut screen = screen();
        screen.feed(b"\x1b]0;my machine\x07");
        assert_eq!(screen.title(), "my machine");

        // OSC 2 also titles. Everything else — clipboard, hyperlinks — must
        // not disturb the title or the grid.
        screen.feed(b"\x1b]52;c;base64junk\x1b\\");
        screen.feed(b"\x1b]2;renamed\x1b\\");
        assert_eq!(screen.title(), "renamed");
    }

    #[test]
    fn utf8_lands_as_single_cells_even_across_chunk_boundaries() {
        let mut screen = screen();
        let mut bytes = b"\xc3\xa9\xc3\xa8".to_vec();
        let split = bytes.split_off(1); // cuts the second byte of é in half
        screen.feed(&bytes);
        screen.feed(&split);
        assert_eq!(screen.cell(0, 0).ch, 'é');
        assert_eq!(screen.cell(1, 0).ch, 'è');
    }

    #[test]
    fn resizing_keeps_the_top_left_content_both_ways() {
        let mut screen = Screen::new(20, 5);
        screen.feed(b"hello\r\nworld");

        screen.resize(10, 3);
        assert_eq!((screen.cols(), screen.rows()), (10, 3));
        assert_eq!(text(&screen, 0), "hello");
        assert_eq!(text(&screen, 1), "world");

        // Growing back fills new space with blanks rather than garbage.
        screen.resize(30, 6);
        assert_eq!(text(&screen, 0), "hello");
        assert_eq!(text(&screen, 5), "");
    }

    #[test]
    fn deleting_characters_shifts_the_rest_of_the_row_left() {
        let mut screen = screen();
        screen.feed(b"abcdefgh");
        screen.feed(b"\x1b[1;2H\x1b[2P"); // delete the two chars at column 2
        assert_eq!(text(&screen, 0), "adefgh");
    }

    #[test]
    fn deleting_lines_pulls_everything_below_up() {
        let mut screen = screen();
        screen.feed(b"one\r\ntwo\r\nthree");
        screen.feed(b"\x1b[1;1H\x1b[M"); // delete the line the cursor is on
        assert_eq!(text(&screen, 0), "two");
        assert_eq!(text(&screen, 1), "three");
        assert_eq!(text(&screen, 2), "", "a blank line arrived at the bottom");
    }

    #[test]
    fn reverse_index_at_the_top_pushes_the_screen_down() {
        let mut screen = screen();
        screen.feed(b"first");
        screen.feed(b"\x1bM"); // RI at row 0
        screen.feed(b"second");
        // RI keeps the column, so "second" continues from where "first"
        // stopped rather than starting over at the margin.
        assert_eq!(text(&screen, 0), "     second");
        assert_eq!(text(&screen, 1), "first");
    }

    #[test]
    fn full_reset_returns_everything_to_how_it_started() {
        let mut screen = screen();
        screen.feed(b"\x1b[41;1mcoloured\x1b[5;5H\x1b]2;t\x07");
        screen.feed(b"\x1bc");
        assert_eq!(text(&screen, 0), "");
        assert_eq!(screen.cursor(), Cursor { col: 0, row: 0, visible: true });
        assert_eq!(screen.title(), "");
    }

    #[test]
    fn cursor_visibility_is_a_mode_not_an_escape() {
        let mut screen = screen();
        screen.feed(b"\x1b[?25l");
        assert!(!screen.cursor().visible);
        screen.feed(b"\x1b[?25h");
        assert!(screen.cursor().visible);
    }
}
