//! A remote shell, drawn.
//!
//! This screen is three jobs that barely touch:
//!
//! - **Decoding.** Output bytes go into [`pravera_term::Screen::feed`], which
//!   is a complete VT emulator with its own test suite. This file holds none
//!   of that logic and wants none of it.
//! - **Encoding.** A key press becomes the bytes a shell expects. This is the
//!   screen's own opinion, and [`encode`] below is written to be read — every
//!   sequence a real shell distinguishes is here, spelled out.
//! - **Drawing.** The grid becomes text on a canvas: runs of same-coloured
//!   cells drawn as one string, backgrounds painted under them, the caret as
//!   a block. The picture is cached and redrawn only when the grid changes.
//!
//! The keyboard comes from the application's event listener rather than a
//! widget: a terminal owns every key the window receives while it is in
//! front, and capturing at the widget level would let a stray click on a
//! button hand the keyboard back without the person saying so.

use std::cell::Cell;

use iced::widget::{canvas, column, container, row, text};
use iced::{Alignment, Background, Element, Length, Point, Rectangle, Size};

use crate::components;
use crate::theme::{self, tokens as t};
use pravera_client::{Terminal, TerminalEvent};
use pravera_term::Color as CellColor;
use tokio::sync::mpsc;

/// The pane a terminal opens with, before the first real layout corrects it.
pub const START_COLS: u16 = 120;
pub const START_ROWS: u16 = 30;

/// One key press, in the shape iced reported it.
#[derive(Debug, Clone)]
pub struct Press {
    pub key: iced::keyboard::Key,
    pub text: Option<String>,
    pub modifiers: iced::keyboard::Modifiers,
}

impl Press {
    /// Ctrl+Shift+V and Shift+Insert: the two spellings of paste a terminal
    /// honours, because plain Ctrl+V is a control character the shell owns.
    pub fn is_paste(&self) -> bool {
        use iced::keyboard::{Key, key::Named};
        let m = self.modifiers;
        match &self.key {
            Key::Character(c) => m.control() && m.shift() && c.eq_ignore_ascii_case("v"),
            Key::Named(Named::Insert) => m.shift() && !m.control(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Key(Press),
    /// The pane's size in cells, recomputed from the canvas bounds. Sent only
    /// when it changed.
    Resized { cols: u16, rows: u16 },
    /// The ended banner's close.
    Close,
}

/// How a terminal stopped being a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The shell exited, with what it reported.
    Exited(i32),
    /// The connection went before the shell did.
    Closed,
}

pub struct State {
    /// What the shell has drawn so far.
    screen: pravera_term::Screen,
    /// Where keystrokes go. `None` while the connection is still being made,
    /// and after the shell ended — a dead terminal stays on screen until its
    /// tab is closed, but it is a picture now, not a conversation.
    terminal: Option<Terminal>,
    ended: Option<Ended>,
    /// The pane size the host currently believes. The canvas is the truth;
    /// this is what has been told so far.
    sent: Cell<(u16, u16)>,
    /// The drawn grid. Cleared whenever the grid or the pane changes, so a
    /// still shell costs nothing to redraw.
    cache: canvas::Cache,
}

impl State {
    pub fn new() -> State {
        State {
            screen: pravera_term::Screen::new(START_COLS, START_ROWS),
            terminal: None,
            ended: None,
            sent: Cell::new((START_COLS, START_ROWS)),
            cache: canvas::Cache::new(),
        }
    }

    /// Wire the shell in, once the connection is up.
    pub fn connect(&mut self, terminal: Terminal) {
        self.terminal = Some(terminal);
    }

    /// Output from the shell.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.screen.feed(bytes);
        self.cache.clear();
    }

    /// The conversation is over. Keys stop going anywhere, the caret goes,
    /// and the banner says why.
    pub fn end(&mut self, how: Ended) {
        if self.ended.is_none() {
            self.ended = Some(how);
            self.terminal = None;
            self.cache.clear();
        }
    }

    pub fn ended(&self) -> Option<Ended> {
        self.ended
    }

    pub fn is_ended(&self) -> bool {
        self.ended.is_some()
    }

    /// What the shell calls its window, if it said.
    pub fn title(&self) -> &str {
        self.screen.title()
    }

    /// The pane size the host has been told about.
    pub fn pane(&self) -> (u16, u16) {
        self.sent.get()
    }

    /// The pane changed, because the window did. Resizes the emulator and,
    /// where there is still a shell to tell, tells it.
    pub fn resized(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.sent.get() == (cols, rows) {
            return;
        }
        self.sent.set((cols, rows));
        self.screen.resize(cols, rows);
        self.cache.clear();
        if let Some(terminal) = &self.terminal {
            terminal.resize(cols, rows);
        }
    }

    /// A key was pressed. Encoded and sent; nothing is echoed locally, because
    /// echoing is the shell's job and a terminal that echoes twice is a
    /// terminal nobody trusts.
    pub fn key(&mut self, press: Press) {
        let bytes = encode(&press);
        if bytes.is_empty() {
            return;
        }
        if let Some(terminal) = &self.terminal {
            terminal.input(bytes);
        }
    }

    /// Text from the clipboard. Line endings become carriage returns, which
    /// is what Enter sends, and a long paste travels in pieces the protocol
    /// carries rather than being refused whole.
    pub fn paste(&mut self, text: &str) {
        let Some(terminal) = &self.terminal else {
            return;
        };
        let normalised = text.replace("\r\n", "\r").replace('\n', "\r");
        for chunk in chunks(&normalised, pravera_proto::MAX_INPUT_CHUNK) {
            terminal.input(chunk.as_bytes().to_vec());
        }
    }
}

impl Default for State {
    fn default() -> Self {
        State::new()
    }
}

/// Split on character boundaries into pieces of at most `limit` bytes.
fn chunks(text: &str, limit: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(limit);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(&rest[..end]);
        rest = &rest[end..];
    }
    pieces
}

/// Drain whatever the terminal's pump has sent. Returns whether anything
/// arrived, so the caller knows a redraw is worth asking for.
///
/// A receiver whose sender has gone without a word is a connection that
/// dropped, and says so: a shell that silently stops answering looks exactly
/// like a shell that is thinking.
pub fn update(state: &mut State, events: &mut mpsc::UnboundedReceiver<TerminalEvent>) -> bool {
    use mpsc::error::TryRecvError;

    let mut changed = false;
    loop {
        match events.try_recv() {
            Ok(TerminalEvent::Output(bytes)) => {
                state.feed(&bytes);
                changed = true;
            }
            Ok(TerminalEvent::Exited(code)) => {
                state.end(Ended::Exited(code));
                changed = true;
            }
            Ok(TerminalEvent::Closed) => {
                state.end(Ended::Closed);
                changed = true;
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                if !state.is_ended() {
                    state.end(Ended::Closed);
                    changed = true;
                }
                break;
            }
        }
    }
    changed
}

/// A key press, in the bytes a shell would want. Empty means this key is not
/// part of a terminal conversation and is dropped.
///
/// The character case leans on the bytes iced already produced: the person's
/// layout and their Shift key have already been applied by the operating
/// system, and re-deriving either from physical key codes is how emulators
/// end up typing the wrong alphabet on half the keyboards in Europe.
fn encode(press: &Press) -> Vec<u8> {
    use iced::keyboard::{Key, key::Named};

    let ctrl = press.modifiers.control();
    let alt = press.modifiers.alt();
    let shift = press.modifiers.shift();
    // Windows reports AltGr as Ctrl+Alt. What it typed is a character — the
    // `@` on a German keyboard, the `€` on most of Europe — and must arrive
    // as that character, not as a control code or an escape sequence.
    let altgr = ctrl && alt;

    match &press.key {
        Key::Character(key) => {
            // Ctrl with a letter is a control character, spelled by position
            // on the alphabet rather than by layout — Ctrl+I is a tab on every
            // keyboard that has ever had one. The punctuation row keeps its
            // C0 meanings too: Ctrl+@ is NUL, Ctrl+? is DEL, and readline
            // binds half its shortcuts to those spellings.
            if ctrl && !altgr {
                if let Some(mapped) = control_byte(key) {
                    return mapped;
                }
            }
            let typed = press
                .text
                .as_deref()
                .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
                .unwrap_or(key.as_str());
            let mut bytes = typed.as_bytes().to_vec();
            // Alt is the terminal's Meta: a prefix escape before whatever was
            // typed. Before, not instead — Alt+X has meant ESC X since the
            // PDP-11 and every readline still listens for it.
            if alt && !altgr {
                bytes.insert(0, 0x1b);
            }
            bytes
        }
        Key::Named(named) => {
            // Sequences that already carry a modifier parameter (the cursor
            // family) have Alt folded into their number; everything else
            // takes the Meta prefix.
            let (sequence, modifier_encoded): (Vec<u8>, bool) = match named {
                Named::Enter => (b"\r".to_vec(), false),
                Named::Backspace => (vec![0x7f], false),
                Named::Tab => {
                    if shift {
                        (b"\x1b[Z".to_vec(), false)
                    } else {
                        (b"\t".to_vec(), false)
                    }
                }
                Named::Escape => (b"\x1b".to_vec(), true),
                // Space is a named key, not a character, and it is the most
                // typed key there is.
                Named::Space if ctrl && !altgr => (vec![0x00], false),
                Named::Space => (b" ".to_vec(), altgr),
                Named::ArrowUp => (cursor_sequence('A', press.modifiers), true),
                Named::ArrowDown => (cursor_sequence('B', press.modifiers), true),
                Named::ArrowRight => (cursor_sequence('C', press.modifiers), true),
                Named::ArrowLeft => (cursor_sequence('D', press.modifiers), true),
                Named::Home => (cursor_sequence('H', press.modifiers), true),
                Named::End => (cursor_sequence('F', press.modifiers), true),
                Named::PageUp => (b"\x1b[5~".to_vec(), false),
                Named::PageDown => (b"\x1b[6~".to_vec(), false),
                Named::Insert => (b"\x1b[2~".to_vec(), false),
                Named::Delete => (b"\x1b[3~".to_vec(), false),
                Named::F1 => (b"\x1bOP".to_vec(), false),
                Named::F2 => (b"\x1bOQ".to_vec(), false),
                Named::F3 => (b"\x1bOR".to_vec(), false),
                Named::F4 => (b"\x1bOS".to_vec(), false),
                Named::F5 => (b"\x1b[15~".to_vec(), false),
                Named::F6 => (b"\x1b[17~".to_vec(), false),
                Named::F7 => (b"\x1b[18~".to_vec(), false),
                Named::F8 => (b"\x1b[19~".to_vec(), false),
                Named::F9 => (b"\x1b[20~".to_vec(), false),
                Named::F10 => (b"\x1b[21~".to_vec(), false),
                Named::F11 => (b"\x1b[23~".to_vec(), false),
                Named::F12 => (b"\x1b[24~".to_vec(), false),
                // Everything else — CapsLock, media keys, the browser rows —
                // is not part of any shell conversation.
                _ => return Vec::new(),
            };
            if alt && !modifier_encoded {
                return [vec![0x1b], sequence].concat();
            }
            sequence
        }
        Key::Unidentified => Vec::new(),
    }
}

/// What Ctrl plus a character means, when it means something: letters map by
/// alphabet position, and the few punctuation keys with historic C0 roles keep
/// them. `None` for everything else — Ctrl+ö has no terminal meaning, and
/// inventing one would be worse than sending the character itself.
fn control_byte(text: &str) -> Option<Vec<u8>> {
    let ch = text.chars().next()?;
    if text.chars().count() != 1 {
        return None;
    }
    match ch {
        'a'..='z' => Some(vec![ch as u8 - b'a' + 1]),
        'A'..='Z' => Some(vec![ch as u8 - b'A' + 1]),
        '@' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' | '?' => Some(vec![0x7f]),
        ' ' => Some(vec![0x00]),
        _ => None,
    }
}

/// Arrows and Home/End carry their modifiers in the sequence itself: bare is
/// `ESC [ <final>`, and any of Shift, Alt or Ctrl upgrade it to the
/// parameterised form, numbered the way xterm has numbered them for decades —
/// one more than the sum of shift(1), alt(2) and ctrl(4). Programs on the far
/// side tell the difference — a Ctrl+Right that arrived as a bare Right would
/// jump a word for a character.
fn cursor_sequence(final_byte: char, modifiers: iced::keyboard::Modifiers) -> Vec<u8> {
    use iced::keyboard::Modifiers;

    let mut modifier = 1u8;
    if modifiers.contains(Modifiers::SHIFT) {
        modifier += 1;
    }
    if modifiers.contains(Modifiers::ALT) {
        modifier += 2;
    }
    if modifiers.contains(Modifiers::CTRL) {
        modifier += 4;
    }

    if modifier == 1 {
        format!("\x1b[{final_byte}").into_bytes()
    } else {
        format!("\x1b[1;{modifier}{final_byte}").into_bytes()
    }
}

// ------------------------------------------------------------------- drawing

/// Cell geometry for JetBrains Mono at 13 px: every glyph advances exactly
/// 0.6 em, and 18 px of line leaves room for descenders and box drawing to
/// meet. Measured once here rather than per cell, because a thousand cells do
/// not make a font metric more true.
const TEXT_SIZE: f32 = 13.0;
const CELL_W: f32 = TEXT_SIZE * 0.6;
const CELL_H: f32 = 18.0;

/// The caret, and what is drawn on it.
const CARET: iced::Color = t::NEUTRAL_200;
const ON_CARET: iced::Color = t::BACKGROUND;

pub fn view<'a>(state: &'a State) -> Element<'a, Message> {
    let surface = container(
        canvas(Surface {
            screen: &state.screen,
            caret: (!state.is_ended()).then(|| state.screen.cursor()),
            sent: &state.sent,
            cache: &state.cache,
        })
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .padding([t::SPACE_2, t::SPACE_3])
    .width(Length::Fill)
    .height(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(t::BACKGROUND)),
        ..container::Style::default()
    });

    match state.ended {
        None => surface.into(),
        Some(how) => column![surface, ended_banner(how)].into(),
    }
}

/// The strip that replaces the conversation once it is over.
fn ended_banner<'a>(how: Ended) -> Element<'a, Message> {
    let (tint, words) = match how {
        Ended::Exited(0) => (t::MUTED_FOREGROUND, "The shell exited.".to_string()),
        Ended::Exited(code) => (t::WARNING, format!("The shell exited with code {code}.")),
        Ended::Closed => (t::DESTRUCTIVE_TEXT, "The connection closed.".to_string()),
    };
    container(
        row![
            components::dot(tint, 6.0),
            text(words).size(t::TEXT_XS).style(theme::muted).width(Length::Fill),
            iced::widget::button(text("Close tab").size(t::TEXT_XS).font(t::FONT_UI_MEDIUM))
                .padding(components::BUTTON_PADDING_SM)
                .style(theme::secondary_button)
                .on_press(Message::Close),
        ]
        .spacing(t::SPACE_2)
        .align_y(Alignment::Center),
    )
    .padding([t::SPACE_1_5, t::SPACE_3])
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(t::CARD)),
        border: iced::Border {
            color: t::BORDER,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    })
    .into()
}

struct Surface<'a> {
    screen: &'a pravera_term::Screen,
    caret: Option<pravera_term::Cursor>,
    sent: &'a Cell<(u16, u16)>,
    cache: &'a canvas::Cache,
}

impl<'a> canvas::Program<Message> for Surface<'a> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: &iced::Event,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        // The canvas is where the true pane size is knowable: the window's
        // pixel size becomes a cell count only against real cell metrics. Any
        // window event is a chance to re-derive it — a terminal that just
        // opened corrects its guessed size on the first frame, without
        // waiting for a resize. The comparison is what makes this cheap: the
        // message goes out only when the cell count actually moved.
        if let iced::Event::Window(_) = event {
            let (cols, rows) = pane_for(bounds.size());
            if self.sent.get() != (cols, rows) {
                return Some(canvas::Action::publish(Message::Resized { cols, rows }));
            }
        }
        None
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: iced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            paint(frame, self.screen, self.caret, bounds.size());
        });
        vec![geometry]
    }
}

/// The whole grid, in frame coordinates: the frame's origin is the canvas's
/// top-left corner, whatever the canvas's place in the window.
fn paint(
    frame: &mut canvas::Frame,
    screen: &pravera_term::Screen,
    caret: Option<pravera_term::Cursor>,
    size: Size,
) {
    frame.fill_rectangle(Point::ORIGIN, size, t::BACKGROUND);

    let (cols, rows) = pane_for(size);
    let show_cols = cols.min(screen.cols()) as usize;
    let show_rows = rows.min(screen.rows());
    let caret = caret.filter(|c| c.visible);

    for row in 0..show_rows {
        let line = screen.line(row);
        let line = &line[..show_cols.min(line.len())];
        let y = row as f32 * CELL_H;
        let caret_col = caret
            .filter(|c| c.row == row)
            .map(|c| c.col as usize);

        // Backgrounds first, in runs, including under spaces: a reversed
        // status bar is nothing but coloured spaces.
        let mut col = 0;
        while col < line.len() {
            let bg = colours(&line[col]).1;
            let start = col;
            while col < line.len() && colours(&line[col]).1 == bg {
                col += 1;
            }
            if bg != t::BACKGROUND {
                frame.fill_rectangle(
                    Point::new(start as f32 * CELL_W, y),
                    Size::new((col - start) as f32 * CELL_W, CELL_H),
                    bg,
                );
            }
        }

        if let Some(at) = caret_col.filter(|&at| at < show_cols) {
            frame.fill_rectangle(Point::new(at as f32 * CELL_W, y), Size::new(CELL_W, CELL_H), CARET);
        }

        // Glyphs, in runs of one colour and weight. Only ASCII joins a run:
        // everything else is drawn in its own cell, so a glyph from a
        // fallback face cannot push the rest of the line out of the grid.
        let mut col = 0;
        while col < line.len() {
            let cell = &line[col];
            if cell.ch == ' ' || cell.ch == '\0' {
                col += 1;
                continue;
            }
            let at_caret = caret_col == Some(col);
            let fg = if at_caret { ON_CARET } else { colours(cell).0 };
            let bold = cell.attrs.contains(pravera_term::Attrs::BOLD);
            let start = col;
            let mut run = String::new();

            if cell.ch.is_ascii() && !at_caret {
                while col < line.len() {
                    let next = &line[col];
                    if !next.ch.is_ascii() || next.ch == '\0' || caret_col == Some(col) {
                        break;
                    }
                    if next.ch != ' '
                        && (colours(next).0 != fg
                            || next.attrs.contains(pravera_term::Attrs::BOLD) != bold)
                    {
                        break;
                    }
                    run.push(next.ch);
                    col += 1;
                }
            } else {
                run.push(cell.ch);
                col += 1;
            }

            let content = run.trim_end().to_string();
            if content.is_empty() {
                continue;
            }
            frame.fill_text(canvas::Text {
                content,
                position: Point::new(start as f32 * CELL_W, y),
                color: fg,
                size: iced::Pixels(TEXT_SIZE),
                line_height: iced::widget::text::LineHeight::Absolute(iced::Pixels(CELL_H)),
                font: if bold { t::FONT_MONO_STRONG } else { t::FONT_MONO },
                shaping: if cell.ch.is_ascii() {
                    iced::widget::text::Shaping::Basic
                } else {
                    iced::widget::text::Shaping::Advanced
                },
                ..canvas::Text::default()
            });
        }

        // Underlines last, in runs, one pixel above the cell's floor.
        let mut col = 0;
        while col < line.len() {
            if !line[col].attrs.contains(pravera_term::Attrs::UNDERLINE) {
                col += 1;
                continue;
            }
            let fg = colours(&line[col]).0;
            let start = col;
            while col < line.len()
                && line[col].attrs.contains(pravera_term::Attrs::UNDERLINE)
                && colours(&line[col]).0 == fg
            {
                col += 1;
            }
            frame.fill_rectangle(
                Point::new(start as f32 * CELL_W, y + CELL_H - 3.0),
                Size::new((col - start) as f32 * CELL_W, 1.0),
                fg,
            );
        }
    }
}

/// How many cells of each kind fit. At least one of each: a pane that cannot
/// answer that question has no answer, but it must still be drawable.
fn pane_for(size: Size) -> (u16, u16) {
    let cols = (size.width / CELL_W).floor().clamp(1.0, u16::MAX as f32) as u16;
    let rows = (size.height / CELL_H).floor().clamp(1.0, u16::MAX as f32) as u16;
    (cols, rows)
}

/// Foreground and background for one cell, with the theme standing in for the
/// terminal's own idea of "default".
fn colours(cell: &pravera_term::Cell) -> (iced::Color, iced::Color) {
    let mut fg = colour(cell.fg, t::NEUTRAL_200);
    let mut bg = colour(cell.bg, t::BACKGROUND);

    if cell.attrs.contains(pravera_term::Attrs::REVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

fn colour(which: CellColor, default: iced::Color) -> iced::Color {
    match which {
        CellColor::Default => default,
        CellColor::Indexed(index) => palette(index),
        CellColor::Rgb(r, g, b) => iced::Color::from_rgb8(r, g, b),
    }
}

/// The xterm 256-colour palette: the sixteen ANSI slots, then a 6×6×6 colour
/// cube, then twenty-four steps of grey. The sixteen are tuned to sit on the
/// app's #121212 floor rather than on xterm's black; the cube and the greys
/// are xterm's own, because programs that use them ask for exact values.
fn palette(index: u8) -> iced::Color {
    const BASE: [[u8; 3]; 16] = [
        [0x26, 0x26, 0x26],
        [0xe5, 0x53, 0x4b],
        [0x49, 0xc9, 0x70],
        [0xe7, 0xaf, 0x36],
        [0x57, 0x86, 0xf5],
        [0xc2, 0x6e, 0xe0],
        [0x2a, 0xb8, 0xc6],
        [0xd4, 0xd4, 0xd4],
        [0x73, 0x73, 0x73],
        [0xff, 0x7b, 0x72],
        [0xa3, 0xe6, 0x35],
        [0xfa, 0xcc, 0x15],
        [0x76, 0x98, 0xfd],
        [0xe4, 0x8a, 0xf6],
        [0x5e, 0xea, 0xd4],
        [0xfa, 0xfa, 0xfa],
    ];

    match index {
        0..=15 => {
            let [r, g, b] = BASE[index as usize];
            iced::Color::from_rgb8(r, g, b)
        }
        16..=231 => {
            let i = index - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            iced::Color::from_rgb8(step(i / 36), step((i % 36) / 6), step(i % 6))
        }
        _ => {
            let grey = 8 + (index - 232) * 10;
            iced::Color::from_rgb8(grey, grey, grey)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::{Key, Modifiers, key::Named};

    fn press(key: Key) -> Press {
        Press {
            key,
            text: None,
            modifiers: Modifiers::empty(),
        }
    }

    fn named(named: Named) -> Press {
        press(Key::Named(named))
    }

    #[test]
    fn plain_characters_travel_as_their_own_bytes() {
        assert_eq!(
            encode(&Press {
                key: Key::Character("x".into()),
                text: Some("x".into()),
                modifiers: Modifiers::empty(),
            }),
            b"x"
        );
        // Layout and Shift have already been applied by the operating system:
        // what iced reported is what a shell should receive.
        assert_eq!(
            encode(&Press {
                key: Key::Character("Ü".into()),
                text: Some("Ü".into()),
                modifiers: Modifiers::empty(),
            }),
            "Ü".as_bytes()
        );
    }

    #[test]
    fn space_is_typed() {
        // Space arrives as a named key, and a terminal that dropped it could
        // not run a single command with an argument.
        assert_eq!(encode(&named(Named::Space)), b" ");
        let ctrl_space = Press {
            key: Key::Named(Named::Space),
            text: None,
            modifiers: Modifiers::CTRL,
        };
        assert_eq!(encode(&ctrl_space), &[0x00]);
    }

    #[test]
    fn altgr_types_its_character_rather_than_a_control_code() {
        // German layout: AltGr+Q is `@`, reported as Ctrl+Alt with text "@".
        let at = Press {
            key: Key::Character("q".into()),
            text: Some("@".into()),
            modifiers: Modifiers::CTRL | Modifiers::ALT,
        };
        assert_eq!(encode(&at), b"@");
    }

    #[test]
    fn ctrl_letters_are_control_codes() {
        let ctrl_c = Press {
            key: Key::Character("c".into()),
            text: Some("\u{3}".into()),
            modifiers: Modifiers::CTRL,
        };
        assert_eq!(encode(&ctrl_c), &[0x03]);
    }

    #[test]
    fn the_keys_every_shell_agrees_on() {
        assert_eq!(encode(&named(Named::Enter)), b"\r");
        assert_eq!(encode(&named(Named::Backspace)), &[0x7f]);
        assert_eq!(encode(&named(Named::Escape)), b"\x1b");
        assert_eq!(encode(&named(Named::Tab)), b"\t");
    }

    #[test]
    fn shift_tab_is_backwards_through_the_stops() {
        assert_eq!(
            encode(&Press {
                key: Key::Named(Named::Tab),
                text: None,
                modifiers: Modifiers::SHIFT,
            }),
            b"\x1b[Z"
        );
    }

    #[test]
    fn arrows_are_bare_until_a_modifier_joins() {
        assert_eq!(encode(&named(Named::ArrowUp)), b"\x1b[A");
        assert_eq!(encode(&named(Named::ArrowLeft)), b"\x1b[D");

        // Ctrl+Right is word-right in every shell that matters, and the
        // parameterised form is what tells it apart from a bare Right.
        let ctrl_right = Press {
            key: Key::Named(Named::ArrowRight),
            text: None,
            modifiers: Modifiers::CTRL,
        };
        assert_eq!(encode(&ctrl_right), b"\x1b[1;5C");

        let shifted = Press {
            key: Key::Named(Named::ArrowDown),
            text: None,
            modifiers: Modifiers::SHIFT | Modifiers::ALT,
        };
        assert_eq!(encode(&shifted), b"\x1b[1;4B");
    }

    #[test]
    fn alt_is_meta_a_prefix_not_a_replacement() {
        let alt_x = Press {
            key: Key::Character("x".into()),
            text: Some("x".into()),
            modifiers: Modifiers::ALT,
        };
        assert_eq!(encode(&alt_x), b"\x1bx");
    }

    #[test]
    fn function_keys_use_the_sequences_programs_look_for() {
        assert_eq!(encode(&named(Named::F1)), b"\x1bOP");
        assert_eq!(encode(&named(Named::F5)), b"\x1b[15~");
        assert_eq!(encode(&named(Named::F12)), b"\x1b[24~");
    }

    #[test]
    fn keys_that_mean_nothing_to_a_shell_mean_nothing() {
        assert!(encode(&press(Key::Unidentified)).is_empty());
        assert!(encode(&named(Named::CapsLock)).is_empty());
        assert!(encode(&named(Named::MediaPlayPause)).is_empty());
    }

    #[test]
    fn paste_is_recognised_in_both_spellings_and_ctrl_v_is_left_to_the_shell() {
        let paste = Press {
            key: Key::Character("V".into()),
            text: None,
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
        };
        assert!(paste.is_paste());
        let insert = Press {
            key: Key::Named(Named::Insert),
            text: None,
            modifiers: Modifiers::SHIFT,
        };
        assert!(insert.is_paste());
        let ctrl_v = Press {
            key: Key::Character("v".into()),
            text: None,
            modifiers: Modifiers::CTRL,
        };
        assert!(!ctrl_v.is_paste());
    }

    #[test]
    fn a_long_paste_is_cut_on_character_boundaries() {
        let text = "é".repeat(3000);
        let pieces = chunks(&text, pravera_proto::MAX_INPUT_CHUNK);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| p.len() <= pravera_proto::MAX_INPUT_CHUNK));
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn a_dropped_connection_ends_the_terminal() {
        let mut state = State::new();
        let (sender, mut events) = mpsc::unbounded_channel();
        sender.send(TerminalEvent::Output(b"hi".to_vec())).unwrap();
        drop(sender);
        assert!(update(&mut state, &mut events));
        assert_eq!(state.ended(), Some(Ended::Closed));
    }

    #[test]
    fn an_exit_is_reported_once_and_kept() {
        let mut state = State::new();
        let (sender, mut events) = mpsc::unbounded_channel();
        sender.send(TerminalEvent::Exited(3)).unwrap();
        sender.send(TerminalEvent::Closed).unwrap();
        update(&mut state, &mut events);
        assert_eq!(state.ended(), Some(Ended::Exited(3)));
    }

    #[test]
    fn the_palette_covers_every_index_without_panicking() {
        for index in 0..=255u8 {
            let colour = palette(index);
            assert!(colour.r <= 1.0 && colour.b <= 1.0);
        }
        // Cube corner: index 16 is pure black, 231 is white.
        assert_eq!(palette(16), iced::Color::from_rgb8(0, 0, 0));
        assert_eq!(palette(231), iced::Color::from_rgb8(255, 255, 255));
    }

    #[test]
    fn a_pane_never_computes_to_zero() {
        assert_eq!(pane_for(Size::new(0.0, 0.0)), (1, 1));
        assert_eq!(pane_for(Size::new(CELL_W * 4.0 + 0.1, CELL_H * 2.0 + 0.1)), (4, 2));
    }
}
