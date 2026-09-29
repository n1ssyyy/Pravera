//! Turning the shell's byte stream into grid operations.
//!
//! The tokenising half is `vte`'s; this file owns the meanings. Every CSI and
//! escape sequence a real shell emits lands in one of the handlers below,
//! which translate it into cursor moves, erases, scrolls and attribute changes
//! on the [`Grid`](crate::screen::Grid). Sequences this emulator has decided
//! not to model — scroll regions, alternate screens, DCS frames — are ignored
//! here, on purpose and in one place.

use vte::Perform;

use crate::cell::{Attrs, Color};
use crate::screen::Grid;

/// A CSI parameter with the sequence's own default filled in.
///
/// Missing means the default, not zero — `CUU` without a parameter moves one,
/// not none — and an explicit 0 means the same thing to every terminal that
/// ever implemented this. Entries holding several colon-separated values take
/// their first number.
fn param(params: &[&[u16]], index: usize, default: u16) -> u16 {
    match params.get(index) {
        Some([value]) if *value != 0 => *value,
        Some([]) | None => default,
        Some(values) => {
            let value = values[0];
            if value == 0 { default } else { value }
        }
    }
}

impl Perform for Grid {
    fn print(&mut self, ch: char) {
        self.print(ch);
    }

    fn execute(&mut self, byte: u8) {
        self.execute(byte);
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let params: Vec<&[u16]> = params.iter().collect();

        // Private modes. `vte` collects the `?` prefix into the intermediates,
        // so `CSI ? 25 l` arrives as intermediates [`?`], params [25]. This is
        // the only place DECSET/DECRST are interpreted; unknown private modes
        // are ignored rather than guessed at.
        if intermediates == [b'?'] {
            if matches!(action, 'h' | 'l') {
                let on = action == 'h';
                match param(&params, 0, 0) {
                    // DECTCEM. Cursor visibility is a mode, not a one-off
                    // hide: shells flip it around painting full-screen apps,
                    // and the renderer owes the user a blinking caret between
                    // them.
                    25 => self.set_cursor_visible(on),
                    _ => {}
                }
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }

        match action {
            'A' => self.move_by(0, -(param(&params, 0, 1) as i32)),
            'B' | 'e' => self.move_by(0, param(&params, 0, 1) as i32),
            'C' | 'a' => self.move_by(param(&params, 0, 1) as i32, 0),
            'D' => self.move_by(-(param(&params, 0, 1) as i32), 0),
            // CNL / CPL: cursor down or up, then carriage return.
            'E' => {
                self.move_by(0, param(&params, 0, 1) as i32);
                self.move_col_to(0);
            }
            'F' => {
                self.move_by(0, -(param(&params, 0, 1) as i32));
                self.move_col_to(0);
            }
            'G' | '`' => self.move_col_to(param(&params, 0, 1).saturating_sub(1)),
            'H' | 'f' => self.move_to(
                param(&params, 1, 1).saturating_sub(1),
                param(&params, 0, 1).saturating_sub(1),
            ),
            'd' => self.move_row_to(param(&params, 0, 1).saturating_sub(1)),
            'J' => self.erase_display(param(&params, 0, 0)),
            'K' => self.erase_line(param(&params, 0, 0)),
            'L' => self.insert_lines(param(&params, 0, 1)),
            'M' => self.delete_lines(param(&params, 0, 1)),
            'P' => self.delete_chars(param(&params, 0, 1)),
            '@' => self.insert_chars(param(&params, 0, 1)),
            'S' => self.scroll_up(param(&params, 0, 1)),
            'T' => self.scroll_down(param(&params, 0, 1)),
            'm' => self.select_graphic_rendition(&params),
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),

            // DECSTBM (r), device status reports (n), key reassignment and
            // every other action without a meaning in this grid: ignored,
            // deliberately, because they arrive from real shells all day.
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            // IND / NEL: line feeds that also work at the bottom margin.
            ([], b'D') => self.index(),
            ([], b'E') => {
                self.index();
                self.move_col_to(0);
            }
            ([], b'M') => self.reverse_index(),
            // DECSC / DECRC, alongside their ANSI.SYS cousins s and u.
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            // RIS: full reset. Charset designations and the rest: no.
            ([], b'c') => self.reset(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0 and OSC 2 both set the window title. Everything else —
        // clipboard requests, hyperlinks, palette redefinition — is skipped.
        let Some(code) = params.first() else {
            return;
        };
        if matches!(*code, b"0" | b"2") {
            if let Some(title) = params.get(1) {
                let title = String::from_utf8_lossy(title).into_owned();
                self.set_title(title);
            }
        }
    }

    /// DCS, SOS, PM and APC frames. No program a terminal user cares about
    /// needs them, so the payload is discarded rather than buffered.
    fn hook(
        &mut self,
        _params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}
}

impl Grid {
    /// SGR: select graphic rendition.
    ///
    /// Parameters may be separated by semicolons (`38;2;255;100;5`) or by
    /// colons (`38:2:255:100:5`). `vte` hands over the colon form packed into
    /// one entry of sub-parameters and the semicolon form as separate
    /// entries, so both spellings get walked explicitly below.
    fn select_graphic_rendition(&mut self, params: &[&[u16]]) {
        // A bare ESC[m resets, exactly like ESC[0m.
        if params.is_empty() {
            self.reset_pen();
            return;
        }

        let mut index = 0;
        while index < params.len() {
            let entry = params[index];
            match entry.first().copied() {
                None | Some(0) => self.reset_pen(),
                Some(1) => self.set_attrs(self.attrs() | Attrs::BOLD),
                Some(3) => self.set_attrs(self.attrs() | Attrs::ITALIC),
                Some(4) => self.set_attrs(self.attrs() | Attrs::UNDERLINE),
                Some(7) => self.set_attrs(self.attrs() | Attrs::REVERSE),
                Some(22) => self.set_attrs(self.attrs() - Attrs::BOLD),
                Some(23) => self.set_attrs(self.attrs() - Attrs::ITALIC),
                Some(24) => self.set_attrs(self.attrs() - Attrs::UNDERLINE),
                Some(27) => self.set_attrs(self.attrs() - Attrs::REVERSE),
                Some(30..=37) => self.set_fg(Color::Indexed(entry[0] as u8 - 30)),
                Some(39) => self.set_fg(Color::Default),
                Some(40..=47) => self.set_bg(Color::Indexed(entry[0] as u8 - 40)),
                Some(49) => self.set_bg(Color::Default),
                Some(90..=97) => self.set_fg(Color::Indexed(entry[0] as u8 - 90 + 8)),
                Some(100..=107) => self.set_bg(Color::Indexed(entry[0] as u8 - 100 + 8)),

                // Extended colour for foreground (38) or background (48).
                Some(code @ (38 | 48)) => {
                    let consumed = if entry.len() > 1 {
                        // Colon form: the values live inside this entry.
                        match self.extended_colour(&entry[1..], code == 48) {
                            Some(()) => 1,
                            None => return,
                        }
                    } else {
                        // Semicolon form: the values are the next entries.
                        match self.extended_colour_flat(&params[index + 1..], code == 48) {
                            Some(consumed) => consumed + 1,
                            None => return,
                        }
                    };
                    index += consumed - 1;
                }

                // Bold-off aliases, dim, blink, font selection, and anything
                // else this grid has no opinion on: acknowledged by being
                // ignored, so the sequence still consumes its parameters.
                _ => {}
            }
            index += 1;
        }
    }

    /// Apply one colon-form extended colour: `[5, index]` or `[2, r, g, b]`.
    ///
    /// Returns nothing on failure, because a truncated specification is a
    /// peer bug and guessing would paint the wrong colour with confidence.
    fn extended_colour(&mut self, rest: &[u16], background: bool) -> Option<()> {
        let color = match rest {
            [5, n, ..] => Color::Indexed(*n as u8),
            [2, r, g, b, ..] => Color::Rgb(*r as u8, *g as u8, *b as u8),
            _ => return None,
        };
        self.apply_colour(color, background);
        Some(())
    }

    /// Apply one semicolon-form extended colour, returning how many entries
    /// it consumed: two for indexed, five for truecolour including the `38`.
    fn extended_colour_flat(&mut self, rest: &[&[u16]], background: bool) -> Option<usize> {
        let value_at = |index: usize| -> Option<u16> { rest.get(index)?.first().copied() };
        match rest.first()?.first()? {
            5 => {
                let color = Color::Indexed(value_at(1)? as u8);
                self.apply_colour(color, background);
                Some(2)
            }
            2 => {
                let color = Color::Rgb(
                    value_at(1)? as u8,
                    value_at(2)? as u8,
                    value_at(3)? as u8,
                );
                self.apply_colour(color, background);
                Some(4)
            }
            _ => None,
        }
    }

    fn apply_colour(&mut self, color: Color, background: bool) {
        if background {
            self.set_bg(color);
        } else {
            self.set_fg(color);
        }
    }
}
