//! The cell model: what one position on a terminal screen holds.

use bitflags::bitflags;

/// The colour of a glyph or its background, as the shell described it.
///
/// Three shapes because the protocol has three: "whatever this theme says"
/// ([`Color::Default`]), one of the sixteen ANSI slots plus their bright
/// halves ([`Color::Indexed`]), and an exact 24-bit value
/// ([`Color::Rgb`]). Indexed stays indexed rather than being resolved to RGB
/// here, because the palette belongs to the theme, and a theme change should
/// not require re-reading scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Default,
    /// An ANSI palette slot: 0–7 normal, 8–15 bright.
    Indexed(u8),
    /// A truecolour value, carried through untouched.
    Rgb(u8, u8, u8),
}

impl Color {
    /// Whether this names a specific colour at all.
    ///
    /// Renderers use it to decide between the theme's default foreground or
    /// background and something that must be drawn literally.
    pub fn is_default(self) -> bool {
        matches!(self, Color::Default)
    }
}

/// Emphasis applied to a cell, independent of its colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs(u8);

bitflags! {
    impl Attrs: u8 {
        /// SGR 1.
        const BOLD       = 1 << 0;
        /// SGR 3.
        const ITALIC     = 1 << 1;
        /// SGR 4.
        const UNDERLINE  = 1 << 2;
        /// SGR 7. Swapped at render time, never folded into the stored
        /// colours: the swap is presentation, and un-reversing (SGR 27) must
        /// be able to put the original colours back.
        const REVERSE    = 1 << 3;
    }
}

/// One position on the screen: a character drawn in `fg` on `bg`, emphasised
/// by `attrs`.
///
/// Copied rather than referenced everywhere, because a cell is eight bytes and
/// a renderer wants to own its snapshot while the parser keeps writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The glyph. Always exactly one `char`; double-width characters occupy
    /// one cell like any other, which is honest about what this grid is even
    /// if CJK output looks tighter than a full-width emulator would draw it.
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Default for Cell {
    /// A blank cell: a space in the theme's default colours. Every clear and
    /// every new row is filled with these, so "nothing was printed there" and
    /// "a space was printed there" look identical, which is what a terminal
    /// user expects.
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::default(),
        }
    }
}
