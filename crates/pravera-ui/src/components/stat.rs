//! Inline figures for a status line.
//!
//! Deliberately not tiles. A row of bordered boxes each holding one number is
//! the reflex layout for a screen like Devices, and it spends a whole band of
//! vertical space on four facts that fit comfortably on one line. Setting the
//! number in mono against a plain-language label in the interface face gives
//! the same reading speed at a fraction of the height, which is what a dense
//! screen needs.

use iced::widget::{row, text};
use iced::{Alignment, Color, Element};

use crate::theme::{self, tokens as t};

/// One figure and what it counts, on a single line.
///
/// The number carries the tint and the label stays muted: colour on a status
/// word would compete with the route colours, which are the only place in the
/// app where hue means something.
pub fn figure<'a, Message: 'a>(
    value: usize,
    label: &'static str,
    tint: Color,
) -> Element<'a, Message> {
    row![
        text(value.to_string())
            .size(t::TEXT_SM)
            .font(t::FONT_MONO_STRONG)
            .style(theme::tinted(tint)),
        text(label).size(t::TEXT_XS).style(theme::muted),
    ]
    .spacing(t::SPACE_1)
    .align_y(Alignment::Center)
    .into()
}

/// The divider between two figures.
pub fn separator<'a, Message: 'a>() -> Element<'a, Message> {
    text("\u{00b7}")
        .size(t::TEXT_XS)
        .style(theme::subtle)
        .into()
}
