//! A terminal emulator's front half: parse a VT stream into a drawable grid.
//!
//! This is what renders the remote shell. Bytes arrive from a
//! [`TerminalOut`](pravera_proto::TerminalOut) stream — raw output from a real
//! pseudoconsole, escape sequences and all — and [`Screen::feed`] turns them
//! into cells a UI can draw:
//!
//! ```no_run
//! # let bytes: Vec<u8> = vec![];
//! # let (cols, rows) = (80u16, 24u16);
//! use pravera_term::Screen;
//!
//! let mut screen = Screen::new(cols, rows);
//! screen.feed(&bytes);
//! for row in 0..screen.rows() {
//!     for cell in screen.line(row) {
//!         let _ = (&cell.ch, &cell.fg, &cell.bg, &cell.attrs);
//!     }
//! }
//! ```
//!
//! ## What is supported, precisely
//!
//! Cursor movement (CUP, CUU/CUD/CUF/CUB and relatives), erasing (EL/ED),
//! scrolling, line and character insertion and deletion, SGR including 24-bit
//! colour (`38;2;r;g;b`), 256-colour, bold, italic, underline and reverse,
//! OSC titles, carriage and newline handling, deferred autowrap, and bounded
//! scrollback. Sequences that only matter to full-screen multiplexers — scroll
//! regions, alternate screens — are ignored rather than half-implemented; see
//! `crate::screen` for why.
//!
//! Colour is carried exactly as received: truecolour values survive parsing
//! untouched because nothing between the shell and this grid is allowed to
//! have an opinion about them.

mod cell;
mod parser;
mod screen;

pub use cell::{Attrs, Cell, Color};
pub use screen::{Cursor, SCROLLBACK_LINES};

use vte::Parser;

// Re-exported so the parser module can name it without leaking internals.
use screen::Grid;

/// One terminal pane's parsed state.
///
/// Feed it, then read it. The parser runs inside `feed`, so every accessor
/// sees a consistent grid even while output arrives faster than frames are
/// drawn — a renderer snapshots whatever is there now and never blocks.
pub struct Screen {
    /// The tokenizer. Kept alongside the state it drives; `feed` splits the
    /// borrow so the two can be passed separately to `advance`.
    parser: Parser,
    state: Grid,
}

impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The tokenizer has nothing to show that the grid does not already.
        f.debug_struct("Screen")
            .field("cols", &self.state.cols())
            .field("rows", &self.state.rows())
            .field("cursor", &self.state.cursor())
            .finish_non_exhaustive()
    }
}

impl Screen {
    /// An empty screen of the given size, cursor at home, defaults everywhere.
    ///
    /// Sizes are clamped to at least one column and one row rather than
    /// panicking: a zero-sized pane is a caller bug worth surviving.
    pub fn new(cols: u16, rows: u16) -> Screen {
        Screen {
            parser: Parser::new(),
            state: Grid::new(cols, rows),
        }
    }

    /// Consume more shell output.
    ///
    /// Chunks may cut anywhere — mid-escape-sequence included. The parser
    /// holds partial sequences across calls until they complete, which is why
    /// callers must feed chunks in order and must not skip any: the wire
    /// guarantees both, and this API leans on that guarantee.
    pub fn feed(&mut self, bytes: &[u8]) {
        let Screen { parser, state } = self;
        parser.advance(state, bytes);
    }

    /// Change the pane size, keeping content anchored at the top left.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.state.resize(cols, rows);
    }

    pub fn cols(&self) -> u16 {
        self.state.cols()
    }

    pub fn rows(&self) -> u16 {
        self.state.rows()
    }

    pub fn cursor(&self) -> Cursor {
        self.state.cursor()
    }

    pub fn title(&self) -> &str {
        self.state.title()
    }

    /// The cell at `col, row`. Out of range means a renderer bug, so this
    /// asserts rather than returning a blank nobody would notice.
    pub fn cell(&self, col: u16, row: u16) -> Cell {
        assert!(col < self.cols(), "column {col} outside {}", self.cols());
        assert!(row < self.rows(), "row {row} outside {}", self.rows());
        self.state.cell(col, row)
    }

    /// One whole row of the visible screen.
    pub fn line(&self, row: u16) -> &[Cell] {
        assert!(row < self.rows(), "row {row} outside {}", self.rows());
        self.state.line(row)
    }

    pub fn scrollback_len(&self) -> usize {
        self.state.scrollback_len()
    }

    /// A scrolled-off line, oldest first, at the width it had when it
    /// scrolled off.
    pub fn scrollback_line(&self, index: usize) -> Option<&[Cell]> {
        self.state.scrollback_line(index)
    }
}
