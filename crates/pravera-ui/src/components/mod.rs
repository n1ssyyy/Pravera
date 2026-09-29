//! The component kit: every composed surface a screen is built from.
//!
//! Styles in [`crate::theme`] colour a single widget. What lives here is the
//! next level up — a bevelled panel, a page header, a section, a switch, a
//! pill — so that screens assemble the same handful of pieces instead of each
//! one re-deriving a card from containers and hoping it matches the next one.
//!
//! ## The page
//!
//! Every page is one sheet: a single bevelled panel filling the window to the
//! right of the rail. The header is the top region of the sheet, a hairline
//! splits it from the body, and the body scrolls inside itself. A second pane
//! or a footer is another region of the same sheet, split by a hairline, never
//! a card set inside it. The window never scrolls, so the header is always
//! where the eye left it.

pub mod route_meter;
pub mod sources;
pub mod stat;
pub mod titlebar;

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{button, column, container, mouse_area, row, scrollable, text, Space};
use std::time::Instant;

use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Theme};

use crate::icon;
use crate::motion;
use crate::theme::{self, tokens as t};
use crate::widget::glide::{self, Glide};

/// A one-pixel horizontal hairline.
///
/// iced borders apply to all four sides, so a single-edge rule has to be its
/// own element rather than a border on the thing above it.
pub fn hairline<'a, Message: 'a>() -> Element<'a, Message> {
    container(Space::new().height(Length::Fixed(1.0)))
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(t::BORDER)),
            ..Default::default()
        })
        .into()
}

/// A vertical hairline, for splitting a row.
pub fn vrule<'a, Message: 'a>(height: impl Into<Length>) -> Element<'a, Message> {
    container(Space::new().width(Length::Fixed(1.0)))
        .height(height)
        .style(|_| container::Style {
            background: Some(Background::Color(t::BORDER)),
            ..Default::default()
        })
        .into()
}

// ---------------------------------------------------------------------------
// Panel — the bevelled card
// ---------------------------------------------------------------------------

/// A bevelled surface: DigiClip's card.
///
/// iced draws one border colour on all four sides, so the lit top edge and the
/// shadowed bottom edge are built from nested fills: the outermost is the
/// bottom colour and stops a pixel short at the bottom, the next is the top
/// colour and starts a pixel down, the next the side colour a pixel in from
/// each side, and the innermost is the fill. At one pixel the seams at the
/// rounded corners blend the way CSS blends per-side border colours.
pub struct Panel<'a, Message> {
    content: Element<'a, Message>,
    edge: t::Bevel,
    fill: Color,
    radius: f32,
    padding: Padding,
    width: Length,
    height: Length,
    opacity: f32,
    shadow: iced::Shadow,
    align_x: Horizontal,
    align_y: Vertical,
}

/// A card on the window floor.
pub fn panel<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Panel<'a, Message> {
    Panel {
        content: content.into(),
        edge: t::BEVEL_CARD,
        fill: t::CARD,
        radius: t::RADIUS,
        padding: Padding::new(t::SPACE_2),
        width: Length::Shrink,
        height: Length::Shrink,
        opacity: 1.0,
        shadow: theme::NO_SHADOW,
        align_x: Horizontal::Left,
        align_y: Vertical::Top,
    }
}

/// A raised tile: a row inside a card, an input-like surface, a dialog.
pub fn tile<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Panel<'a, Message> {
    panel(content).edge(t::BEVEL_RAISED).fill(t::ROW)
}

impl<'a, Message: 'a> Panel<'a, Message> {
    pub fn edge(mut self, edge: t::Bevel) -> Self {
        self.edge = edge;
        self
    }

    pub fn fill(mut self, fill: Color) -> Self {
        self.fill = fill;
        self
    }

    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    pub fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.padding = padding.into();
        self
    }

    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Centres the content vertically in whatever height the panel has. A
    /// fixed-height band whose words sit at its top edge is the single most
    /// common way a dark interface looks unfinished.
    pub fn center_y(mut self) -> Self {
        self.align_y = Vertical::Center;
        self
    }

    /// Centres the content both ways.
    pub fn center(mut self) -> Self {
        self.align_x = Horizontal::Center;
        self.align_y = Vertical::Center;
        self
    }

    /// Fades the whole surface, edge and fill, for a panel that is arriving
    /// over something that is not flat. Its content fades separately.
    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// A drop shadow under the whole surface, for things that float.
    pub fn shadow(mut self, shadow: iced::Shadow) -> Self {
        self.shadow = shadow;
        self
    }

    /// Brightens the edge towards [`t::BEVEL_HOVER`] by `amount`.
    pub fn lit(mut self, amount: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        self.edge = t::Bevel {
            top: theme::blend(self.edge.top, t::BEVEL_HOVER.top, amount),
            sides: theme::blend(self.edge.sides, t::BEVEL_HOVER.sides, amount),
            bottom: self.edge.bottom,
        };
        self
    }
}

#[allow(clippy::too_many_arguments)]
fn layer<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    color: Color,
    radius: f32,
    padding: Padding,
    width: Length,
    height: Length,
    align_x: Horizontal,
    align_y: Vertical,
) -> Element<'a, Message> {
    container(content)
        .padding(padding)
        .width(width)
        .height(height)
        .align_x(align_x)
        .align_y(align_y)
        .style(move |_| container::Style {
            background: Some(Background::Color(color)),
            border: Border {
                radius: radius.into(),
                ..Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

impl<'a, Message: 'a> From<Panel<'a, Message>> for Element<'a, Message> {
    fn from(panel: Panel<'a, Message>) -> Self {
        let Panel {
            content,
            edge,
            fill,
            radius,
            padding,
            width,
            height,
            opacity,
            shadow,
            align_x,
            align_y,
        } = panel;
        let o = |c: Color| theme::faded(c, opacity);
        let inner_radius = (radius - 1.0).max(0.0);

        let inner = layer(content, o(fill), inner_radius, padding, width, height, align_x, align_y);
        let edges = Horizontal::Left;
        let top_align = Vertical::Top;
        let sides = layer(
            inner,
            o(edge.sides),
            radius,
            Padding {
                left: 1.0,
                right: 1.0,
                ..Padding::ZERO
            },
            width,
            height,
            edges,
            top_align,
        );
        let top = layer(
            sides,
            o(edge.top),
            radius,
            Padding {
                top: 1.0,
                ..Padding::ZERO
            },
            width,
            height,
            edges,
            top_align,
        );
        let shadow = iced::Shadow {
            color: o(shadow.color),
            ..shadow
        };
        let bottom = o(edge.bottom);
        container(top)
            .padding(Padding {
                bottom: 1.0,
                ..Padding::ZERO
            })
            .width(width)
            .height(height)
            .style(move |_| container::Style {
                background: Some(Background::Color(bottom)),
                border: Border {
                    radius: radius.into(),
                    ..Border::default()
                },
                shadow,
                ..container::Style::default()
            })
            .into()
    }
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

/// The one surface a page is drawn on: a single bevelled sheet that fills the
/// window to the right of the rail. Its header, its body and any footer or
/// second pane are regions of it, split by hairlines, never cards of their own.
///
/// The sheet is chrome, not content: the shell draws it once, round whatever
/// page is in front, so changing page changes what is on the sheet and never
/// the sheet. The rule under the header is the sheet's too, for the same
/// reason. The page functions below therefore make the sheet's contents only,
/// leaving a one pixel gap where the rule falls.
pub fn sheet<'a, Message: 'a>(contents: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let rule = container(hairline())
        .padding(Padding {
            top: t::HEADER_HEIGHT,
            ..Padding::ZERO
        })
        .width(Length::Fill)
        .height(Length::Fill);
    panel(
        iced::widget::Stack::with_children([contents.into(), rule.into()])
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .radius(t::RADIUS_LG)
    .padding(0)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The gap the sheet's own header rule is drawn in.
fn header_rule<'a, Message: 'a>() -> Element<'a, Message> {
    Space::new().height(Length::Fixed(1.0)).into()
}

/// A page: the header across the top of the sheet, and the body filling the
/// rest and scrolling inside itself.
pub fn page<'a, Message: 'a>(
    header: impl Into<Element<'a, Message>>,
    body: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![header.into(), header_rule(), body.into()].into()
}

/// A page with a strip along the bottom of its sheet, under a hairline of its
/// own: a status readout, a ledger, anything that belongs to the whole page
/// rather than to what scrolls above it.
pub fn page_footed<'a, Message: 'a>(
    header: impl Into<Element<'a, Message>>,
    body: impl Into<Element<'a, Message>>,
    footer: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![header.into(), header_rule(), body.into(), hairline(), footer.into()].into()
}

/// A page of two panes: `left` at a fixed width, a vertical hairline, and
/// `right` taking the rest. Each pane brings its own scrolling.
pub fn page_split<'a, Message: 'a>(
    header: impl Into<Element<'a, Message>>,
    left: impl Into<Element<'a, Message>>,
    left_width: f32,
    right: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let panes = row![
        container(left.into())
            .width(Length::Fixed(left_width))
            .height(Length::Fill),
        vrule(Length::Fill),
        container(right.into()).width(Length::Fill).height(Length::Fill),
    ]
    .height(Length::Fill);
    column![header.into(), header_rule(), panes].into()
}

/// A page's header: the title, what the page is showing at a glance, and the
/// page's actions on the right. It is the top region of the sheet.
pub struct Header<'a, Message> {
    title: &'a str,
    meta: Vec<Element<'a, Message>>,
    actions: Vec<Element<'a, Message>>,
}

pub fn header<'a, Message: 'a>(title: &'a str) -> Header<'a, Message> {
    Header {
        title,
        meta: Vec::new(),
        actions: Vec::new(),
    }
}

impl<'a, Message: 'a> Header<'a, Message> {
    /// Something that sits beside the title: a pill, a figure, a status.
    pub fn meta(mut self, element: impl Into<Element<'a, Message>>) -> Self {
        self.meta.push(element.into());
        self
    }

    /// A control on the right.
    pub fn action(mut self, element: impl Into<Element<'a, Message>>) -> Self {
        self.actions.push(element.into());
        self
    }
}

impl<'a, Message: 'a> From<Header<'a, Message>> for Element<'a, Message> {
    fn from(header: Header<'a, Message>) -> Self {
        let mut line = row![text(header.title)
            .size(t::TEXT_LG)
            .font(t::FONT_UI_STRONG)
            .wrapping(text::Wrapping::None)
            .style(theme::heading)]
        .spacing(t::SPACE_3)
        .align_y(Alignment::Center);
        for meta in header.meta {
            line = line.push(meta);
        }
        line = line.push(Space::new().width(Length::Fill));
        line = line.push(
            row(header.actions)
                .spacing(t::SPACE_2)
                .align_y(Alignment::Center),
        );

        container(line)
            .padding([0.0, t::SPACE_6])
            .width(Length::Fill)
            .height(Length::Fixed(t::HEADER_HEIGHT))
            .center_y(Length::Fixed(t::HEADER_HEIGHT))
            .into()
    }
}

/// What a page's body holds, scrolling inside the sheet.
pub fn body<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    body_with(content, [t::SPACE_5, t::SPACE_6])
}

/// The same with its own inset: a list whose rows run nearly to the sheet's
/// edges wants less than a column of prose does.
pub fn body_with<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    padding: impl Into<Padding>,
) -> Element<'a, Message> {
    scroll(container(content).padding(padding).width(Length::Fill))
}

/// The same body with its content held to a reading width and centred, for
/// pages that are a column of settings rather than a list.
pub fn reading_body<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    body(
        container(container(content).max_width(t::READING_WIDTH).width(Length::Fill))
            .center_x(Length::Fill),
    )
}

/// The same body held to a reading width but set against the left edge, for
/// a pane that sits beside a list it belongs to: centred, it would leave a
/// gutter between the two. It starts [`t::SPACE_8`] from what is to its left.
pub fn leading_body<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    scroll(
        container(container(content).max_width(t::READING_WIDTH).width(Length::Fill))
            .padding(Padding {
                top: t::SPACE_5,
                right: t::SPACE_8,
                bottom: t::SPACE_5,
                left: t::SPACE_8,
            })
            .width(Length::Fill),
    )
}

/// A vertical scroll with the thin, nearly invisible thumb every list uses.
pub fn scroll<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    scrollable(content)
        .direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::new().width(6).scroller_width(4).margin(2),
        ))
        .style(theme::scrollbar)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

// ---------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------

/// DigiClip's section label: 10px, upper-case. Heads a group of rows.
pub fn section_label<'a, Message: 'a>(label: &str) -> Element<'a, Message> {
    text(t::tracked(label))
        .size(t::TEXT_2XS)
        .font(t::FONT_UI_MEDIUM)
        .wrapping(text::Wrapping::None)
        .style(theme::muted)
        .into()
}

/// A section label with something on its right: a count, a small action.
pub fn section_head<'a, Message: 'a>(
    label: &str,
    trailing: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        section_label(label),
        Space::new().width(Length::Fill),
        trailing.into()
    ]
    .align_y(Alignment::Center)
    .height(Length::Fixed(t::SPACE_5))
    .into()
}

/// A labelled group inside a page: the section label, then its body.
pub fn section<'a, Message: 'a>(
    label: &str,
    body: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![section_label(label), body.into()]
        .spacing(t::SPACE_3)
        .width(Length::Fill)
        .into()
}

/// A line of body text in the muted tone, for what something is for.
pub fn note<'a, Message: 'a>(words: impl text::IntoFragment<'a>) -> Element<'a, Message> {
    text(words)
        .size(t::TEXT_XS)
        .style(theme::muted)
        .width(Length::Fill)
        .into()
}

/// A state and what it means: the dot, the headline, and the sentence under it.
pub fn status<'a, Message: 'a>(
    tint: Color,
    headline: impl text::IntoFragment<'a>,
    detail: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let line = row![
        container(dot(tint, 8.0))
            .center_x(Length::Fixed(t::ICON))
            .center_y(Length::Fixed(t::ICON)),
        text(headline)
            .size(t::TEXT_SM)
            .font(t::FONT_UI_STRONG)
            .style(theme::heading),
    ]
    .spacing(t::SPACE_2)
    .align_y(Alignment::Center);

    match detail {
        Some(detail) => column![
            line,
            row![Space::new().width(Length::Fixed(t::ICON + t::SPACE_2)), detail]
        ]
        .spacing(t::SPACE_1)
        .width(Length::Fill)
        .into(),
        None => line.into(),
    }
}

/// A fact: its name in a fixed column, then its value.
pub fn fact<'a, Message: 'a>(
    label: &'a str,
    value: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        container(text(label).size(t::TEXT_XS).style(theme::muted))
            .width(Length::Fixed(132.0)),
        container(value.into()).width(Length::Fill),
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center)
    .into()
}

/// One centred block for a list with nothing in it: a mark, what is missing,
/// and what to do about it.
pub fn empty<'a, Message: 'a>(
    glyph: &'static str,
    headline: &'a str,
    detail: &'a str,
) -> iced::widget::Column<'a, Message> {
    column![
        glyph_tile(glyph, t::MUTED_FOREGROUND, 40.0),
        Space::new().height(t::SPACE_2),
        text(headline)
            .size(t::TEXT_SM)
            .font(t::FONT_UI_STRONG)
            .align_x(Horizontal::Center)
            .style(theme::heading),
        container(
            text(detail)
                .size(t::TEXT_XS)
                .align_x(Horizontal::Center)
                .style(theme::muted)
        )
        .max_width(380.0),
    ]
    .spacing(t::SPACE_1_5)
    .align_x(Alignment::Center)
}

/// One centred muted line. For a list that is merely empty, not a problem.
pub fn empty_state<'a, Message: 'a>(line: &'a str) -> Element<'a, Message> {
    container(text(line).size(t::TEXT_SM).style(theme::muted))
        .center_x(Length::Fill)
        .padding([t::SPACE_8, t::SPACE_4])
        .into()
}

// ---------------------------------------------------------------------------
// Marks
// ---------------------------------------------------------------------------

/// A mark with a small dot at its corner: something waiting there. `ring` is
/// the surface the mark sits on, which the dot is cut out of.
pub fn badged<'a, Message: 'a>(mark: Element<'a, Message>, tint: Color, ring: Color) -> Element<'a, Message> {
    const BOX: f32 = t::ICON + 6.0;
    iced::widget::Stack::with_children([
        container(mark)
            .center_x(Length::Fixed(BOX))
            .center_y(Length::Fixed(BOX))
            .into(),
        container(
            container(Space::new().width(Length::Fixed(7.0)).height(Length::Fixed(7.0))).style(move |_| {
                container::Style {
                    background: Some(Background::Color(tint)),
                    border: Border {
                        color: ring,
                        width: 1.5,
                        radius: t::RADIUS_FULL.into(),
                    },
                    ..container::Style::default()
                }
            }),
        )
        .align_right(Length::Fill)
        .align_top(Length::Fill)
        .into(),
    ])
    .into()
}

/// A filled status dot.
pub fn dot<'a, Message: 'a>(color: Color, size: f32) -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(theme::dot(color))
        .into()
}

/// What a pill is saying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Lime, black text: "ready", "live", "hosting". Rationed.
    Success,
    /// A filled grey: a count, a version, a state that is not news.
    Neutral,
    /// A hairline and nothing else.
    Outline,
    Warning,
    Danger,
    Info,
}

impl Tone {
    /// The colour the tone means, for marks that sit beside a pill.
    pub fn tint(self) -> Color {
        match self {
            Tone::Success => t::LIME,
            Tone::Neutral | Tone::Outline => t::MUTED_FOREGROUND,
            Tone::Warning => t::WARNING,
            Tone::Danger => t::DESTRUCTIVE_TEXT,
            Tone::Info => t::INFO,
        }
    }
}

/// DigiClip's badge: 11px, medium, four-pixel corners.
pub fn pill<'a, Message: 'a>(label: impl text::IntoFragment<'a>, tone: Tone) -> Element<'a, Message> {
    let (fill, ink, edge) = match tone {
        Tone::Success => (t::LIME, t::LIME_FOREGROUND, t::LIME),
        Tone::Neutral => (t::SECONDARY, t::NEUTRAL_300, t::BEVEL_RAISED.sides),
        Tone::Outline => (Color::TRANSPARENT, t::NEUTRAL_300, t::BORDER),
        Tone::Warning => (t::with_alpha(t::WARNING, 0.14), t::WARNING, t::with_alpha(t::WARNING, 0.35)),
        Tone::Danger => (
            t::with_alpha(t::DESTRUCTIVE, 0.18),
            t::DESTRUCTIVE_TEXT,
            t::with_alpha(t::DESTRUCTIVE, 0.45),
        ),
        Tone::Info => (t::with_alpha(t::INFO, 0.14), t::INFO, t::with_alpha(t::INFO, 0.35)),
    };
    container(
        text(label)
            .size(t::TEXT_XS)
            .font(t::FONT_UI_MEDIUM)
            .wrapping(text::Wrapping::None)
            .style(theme::tinted(ink)),
    )
    .padding([1.0, t::SPACE_2])
    .style(move |_| container::Style {
        background: Some(Background::Color(fill)),
        border: Border {
            color: edge,
            width: 1.0,
            radius: t::RADIUS_SM.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// A small tinted badge: status carried by colour, never shouting.
pub fn badge<'a, Message: 'a>(label: impl text::IntoFragment<'a>, tint: Color) -> Element<'a, Message> {
    container(
        text(label)
            .size(t::TEXT_XS)
            .font(t::FONT_UI_MEDIUM)
            .wrapping(text::Wrapping::None),
    )
    .padding([1.0, t::SPACE_1_5])
    .style(theme::badge(tint))
    .into()
}

/// A neutral tag for machine metadata.
pub fn tag<'a, Message: 'a>(label: impl text::IntoFragment<'a>) -> Element<'a, Message> {
    pill(label, Tone::Neutral)
}

/// The lime pill. Rationed: "live", "ready", "hosting".
pub fn lime_pill<'a, Message: 'a>(label: &'a str) -> Element<'a, Message> {
    pill(label, Tone::Success)
}

/// A keycap: a shortcut shown inline.
pub fn keycap<'a, Message: 'a>(key: &'a str) -> Element<'a, Message> {
    container(text(key).size(t::TEXT_2XS).font(t::FONT_UI_MEDIUM).style(theme::muted))
        .padding([0.0, t::SPACE_1])
        .style(|_| container::Style {
            background: Some(Background::Color(t::NEUTRAL_825)),
            border: Border {
                color: t::BEVEL_RAISED.sides,
                width: 1.0,
                radius: t::RADIUS_SM.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A warning, a refusal or a note that must not be missed: a soft wash of the
/// tone's colour, its mark, and the words in body contrast.
pub fn callout<'a, Message: 'a>(
    glyph: &'static str,
    message: impl text::IntoFragment<'a>,
    tone: Tone,
) -> Element<'a, Message> {
    let tint = tone.tint();
    container(
        row![
            container(icon::stroked(glyph, t::ICON_SM, tint)).padding([1.0, 0.0]),
            text(message)
                .size(t::TEXT_XS)
                .width(Length::Fill)
                .style(theme::tinted(t::NEUTRAL_200)),
        ]
        .spacing(t::SPACE_2 + 2.0)
        .align_y(Alignment::Start),
    )
    .padding([t::SPACE_2 + 2.0, t::SPACE_3])
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(t::with_alpha(tint, 0.07))),
        border: Border {
            color: t::with_alpha(tint, 0.32),
            width: t::BORDER_WIDTH,
            radius: t::RADIUS.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// A block of machine text — an address, a snippet, a code — sunk into the
/// card like an input, with room for a control on its right.
pub fn code<'a, Message: 'a>(
    words: impl text::IntoFragment<'a>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut line = row![text(words)
        .size(t::TEXT_XS)
        .font(t::FONT_MONO)
        .width(Length::Fill)
        .style(theme::tinted(t::NEUTRAL_200))]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center);
    if let Some(trailing) = trailing {
        line = line.push(trailing);
    }
    container(line)
        .padding(Padding {
            top: t::SPACE_1,
            bottom: t::SPACE_1,
            left: t::SPACE_3,
            right: t::SPACE_1,
        })
        .width(Length::Fill)
        .center_y(Length::Fixed(t::CONTROL_HEIGHT + 4.0))
        .style(|_| container::Style {
            background: Some(Background::Color(t::BACKGROUND)),
            border: Border {
                color: t::BEVEL_RAISED.sides,
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A few lines of machine text — a config snippet, say — sunk into the card
/// like [`code`], with a control pinned to its top-right corner.
pub fn code_block<'a, Message: 'a>(
    words: impl text::IntoFragment<'a>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    // The first line is dropped to the middle of a compact control, so the
    // words and the button beside them start on one line.
    let drop = (26.0 - t::TEXT_XS * 1.3) / 2.0;
    let mut line = row![container(
        text(words)
            .size(t::TEXT_XS)
            .font(t::FONT_MONO)
            .style(theme::tinted(t::NEUTRAL_200)),
    )
    .padding(Padding {
        top: drop,
        ..Padding::ZERO
    })
    .width(Length::Fill)]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Start);
    if let Some(trailing) = trailing {
        line = line.push(trailing);
    }
    container(line)
        .padding(Padding {
            top: t::SPACE_1_5,
            right: t::SPACE_1_5,
            bottom: t::SPACE_3,
            left: t::SPACE_3,
        })
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(t::BACKGROUND)),
            border: Border {
                color: t::BEVEL_RAISED.sides,
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A square tile holding one mark: the lead of a status block or an empty
/// state. `tint` colours the mark; the tile itself stays neutral.
pub fn glyph_tile<'a, Message: 'a>(glyph: &'static str, tint: Color, size: f32) -> Element<'a, Message> {
    container(icon::stroked(glyph, t::ICON, tint))
        .center_x(Length::Fixed(size))
        .center_y(Length::Fixed(size))
        .style(|_| container::Style {
            background: Some(Background::Color(t::ROW)),
            border: Border {
                color: t::BEVEL_RAISED.top,
                width: 1.0,
                radius: t::RADIUS_LG.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// The lead of a card: a mark in its tile, a headline, and one line under it
/// saying what the headline means.
pub fn hero<'a, Message: 'a>(
    glyph: &'static str,
    tint: Color,
    headline: impl text::IntoFragment<'a>,
    detail: impl text::IntoFragment<'a>,
) -> Element<'a, Message> {
    row![
        glyph_tile(glyph, tint, 40.0),
        column![
            text(headline)
                .size(t::TEXT_BASE)
                .font(t::FONT_UI_STRONG)
                .style(theme::heading),
            text(detail).size(t::TEXT_XS).style(theme::muted),
        ]
        .spacing(2.0)
        .width(Length::Fill),
    ]
    .spacing(t::SPACE_3)
    .align_y(Alignment::Center)
    .into()
}

/// Rows in one bevelled tile, split by hairlines: DigiClip's `divide-y` list.
pub fn rows<'a, Message: 'a>(items: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    let mut list = column![].width(Length::Fill);
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            list = list.push(hairline());
        }
        list = list.push(item);
    }
    tile(list).padding(0).width(Length::Fill).into()
}

/// OpenCode's project avatar colours, dark variant: fill, then edge.
pub const AVATAR_TINTS: [(Color, Color); 8] = [
    (rgb(0x72, 0x3d, 0x22), rgb(0xff, 0x86, 0x48)),
    (rgb(0x68, 0x55, 0x2b), rgb(0xe7, 0xaf, 0x36)),
    (rgb(0x00, 0x5a, 0x6e), rgb(0x00, 0x96, 0xb8)),
    (rgb(0x19, 0x61, 0x30), rgb(0x49, 0xc9, 0x70)),
    (rgb(0x7a, 0x1f, 0x23), rgb(0xd9, 0x2e, 0x3c)),
    (rgb(0x8c, 0x2d, 0x61), rgb(0xe4, 0x42, 0x9e)),
    (rgb(0x26, 0x3f, 0xa9), rgb(0x76, 0x98, 0xfd)),
    (rgb(0x36, 0x1f, 0x83), rgb(0x71, 0x52, 0xf4)),
];

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// Which avatar tint a name gets. Stable for a name, so a machine keeps its
/// colour from one run to the next.
pub fn avatar_tint(name: &str) -> (Color, Color) {
    let hash = name
        .bytes()
        .fold(0x811c_9dc5_u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193));
    AVATAR_TINTS[hash as usize % AVATAR_TINTS.len()]
}

/// A machine's initial on its own colour: OpenCode's project avatar.
pub fn avatar<'a, Message: 'a>(name: &str, size: f32) -> Element<'a, Message> {
    avatar_faded(name, size, 1.0)
}

/// The same at an opacity, for an avatar inside something fading in.
pub fn avatar_faded<'a, Message: 'a>(name: &str, size: f32, opacity: f32) -> Element<'a, Message> {
    let (fill, edge) = avatar_tint(name);
    let (fill, edge) = (theme::faded(fill, opacity), theme::faded(edge, opacity));
    let initial = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".to_string());
    container(
        text(initial)
            .size((size * 0.62).round())
            .font(t::FONT_UI_STRONG)
            .style(theme::tinted(theme::faded(t::NEUTRAL_50, opacity))),
    )
    .center_x(Length::Fixed(size))
    .center_y(Length::Fixed(size))
    .style(move |_| container::Style {
        background: Some(Background::Color(fill)),
        border: Border {
            color: edge,
            width: 1.0,
            radius: t::RADIUS_SM.into(),
        },
        ..container::Style::default()
    })
    .into()
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

/// A square ghost button holding one icon: muted at rest, lit on hover.
pub fn icon_button<'a, Message: Clone + 'a>(glyph: &'static str, on_press: Option<Message>) -> Element<'a, Message> {
    let muted = on_press.is_none();
    glide(|hover| {
        button(
            container(icon::stroked(
                glyph,
                t::ICON_SM,
                if muted {
                    t::with_alpha(t::MUTED_FOREGROUND, 0.5)
                } else {
                    t::MUTED_FOREGROUND
                },
            ))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
        )
        .width(Length::Fixed(t::CONTROL_HEIGHT_SM))
        .height(Length::Fixed(t::CONTROL_HEIGHT_SM))
        .padding(0)
        .style(theme::gliding(hover, theme::ghost_button))
        .on_press_maybe(on_press)
        .into()
    })
}

/// A button's label: an icon and a word, spaced the way every button is.
pub fn label<'a, Message: 'a>(glyph: Option<&'static str>, words: &'a str, tint: Color) -> Element<'a, Message> {
    let mut content = row![].spacing(t::SPACE_2).align_y(Alignment::Center);
    if let Some(glyph) = glyph {
        content = content.push(icon::stroked(glyph, t::ICON_SM, tint));
    }
    content
        .push(
            text(words)
                .size(t::TEXT_SM)
                .font(t::FONT_UI_MEDIUM)
                .wrapping(text::Wrapping::None),
        )
        .into()
}

/// A control whose hover eases in and out. `build` is given the hover the
/// control's style should read (see [`theme::gliding`]) and returns the control.
///
/// This is the way to have a button that does not switch colour in a single
/// frame; the buttons below are all made with it.
pub fn glide<'a, Message: 'a>(build: impl FnOnce(glide::Hover) -> Element<'a, Message>) -> Element<'a, Message> {
    let hover = glide::hover();
    Glide::new(hover.clone(), build(hover)).into()
}

/// The compact button every header and card uses: an icon and a word on the
/// quiet filled style. `None` draws it disabled.
pub fn small_button<'a, Message: Clone + 'a>(
    glyph: Option<&'static str>,
    words: &'a str,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    let tint = if on_press.is_some() {
        t::FOREGROUND
    } else {
        t::with_alpha(t::FOREGROUND, 0.5)
    };
    glide(|hover| {
        button(label(glyph, words, tint))
            .padding(BUTTON_PADDING_SM)
            .style(theme::gliding(hover, theme::secondary_button))
            .on_press_maybe(on_press)
            .into()
    })
}

/// The one action a view exists for: white, with near-black words. At most
/// one per view, or it stops meaning that. `None` draws it disabled.
pub fn primary_button<'a, Message: Clone + 'a>(
    glyph: Option<&'static str>,
    words: &'a str,
    on_press: Option<Message>,
) -> Element<'a, Message> {
    glide(|hover| {
        button(label(glyph, words, t::PRIMARY_FOREGROUND))
            .padding(BUTTON_PADDING_SM)
            .style(theme::gliding(hover, theme::primary_button))
            .on_press_maybe(on_press)
            .into()
    })
}

/// The copy control: turns into a confirmation for as long as it is true.
pub fn copy_button<'a, Message: Clone + 'a>(copied: bool, on_press: Message) -> Element<'a, Message> {
    let tint = if copied { t::LIME } else { t::FOREGROUND };
    glide(|hover| {
        button(
            row![
                icon::stroked(if copied { icon::CHECK } else { icon::COPY }, 12.0, tint),
                text(if copied { "Copied" } else { "Copy" })
                    .size(t::TEXT_XS)
                    .font(t::FONT_UI_MEDIUM)
                    .wrapping(text::Wrapping::None)
                    .style(theme::tinted(tint)),
            ]
            .spacing(t::SPACE_1_5)
            .align_y(Alignment::Center),
        )
        .padding([t::SPACE_1 + 1.0, t::SPACE_2 + 2.0])
        .style(theme::gliding(hover, theme::secondary_button))
        .on_press(on_press)
        .into()
    })
}

/// A switch, drawn: DigiClip's 36 by 20 track with the knob that travels.
///
/// `travel` is how far on it is, 0 to 1, so the caller's animation moves the
/// knob and crossfades the track on one clock. On is the primary white — the
/// same inversion as the primary button — and off is a dark well with a light
/// knob, so both states read at a glance on a near-black card.
pub fn switch<'a, Message: 'a>(travel: f32, hover: f32) -> Element<'a, Message> {
    let travel = travel.clamp(0.0, 1.0);
    let track = theme::blend(
        theme::blend(t::NEUTRAL_750, t::NEUTRAL_700, hover),
        theme::blend(t::PRIMARY, t::PRIMARY_HOVER, hover),
        travel,
    );
    let edge = theme::blend(t::BEVEL_RAISED.top, t::PRIMARY, travel);
    let knob = theme::blend(t::NEUTRAL_300, t::BACKGROUND, travel);

    let shift = 16.0 * travel;
    container(
        row![
            Space::new().width(Length::Fixed(1.0 + shift)),
            container(Space::new().width(16.0).height(16.0)).style(theme::dot(knob)),
            Space::new().width(Length::Fixed(17.0 - shift)),
        ]
        .align_y(Alignment::Center),
    )
    .width(Length::Fixed(36.0))
    .height(Length::Fixed(20.0))
    .center_y(Length::Fixed(20.0))
    .style(move |_| container::Style {
        background: Some(Background::Color(track)),
        border: Border {
            color: edge,
            width: 1.0,
            radius: t::RADIUS_FULL.into(),
        },
        ..container::Style::default()
    })
    .into()
}

/// A whole row that flips a switch: what it is and what it does on the left,
/// the switch on the right, and the entire row the target.
#[allow(clippy::too_many_arguments)]
pub fn switch_row<'a, Message: Clone + 'a>(
    title: &'a str,
    detail: &'a str,
    travel: f32,
    hover: f32,
    on_press: Message,
    on_enter: Message,
    on_exit: Message,
) -> Element<'a, Message> {
    let body = row![
        column![
            text(title)
                .size(t::TEXT_SM)
                .font(t::FONT_UI_MEDIUM)
                .style(theme::heading),
            text(detail).size(t::TEXT_XS).style(theme::muted),
        ]
        .spacing(2.0)
        .width(Length::Fill),
        switch(travel, hover),
    ]
    .spacing(t::SPACE_4)
    .align_y(Alignment::Center);

    mouse_area(
        button(body)
            .width(Length::Fill)
            .padding([t::SPACE_2 + 2.0, t::SPACE_3])
            .style(move |_, status| button::Style {
                background: Some(Background::Color(match status {
                    button::Status::Pressed => t::NEUTRAL_825,
                    _ => t::with_alpha(t::NEUTRAL_850, hover),
                })),
                text_color: t::FOREGROUND,
                border: Border {
                    color: t::with_alpha(t::BEVEL_RAISED.sides, hover),
                    width: 1.0,
                    radius: t::RADIUS.into(),
                },
                ..button::Style::default()
            })
            .on_press(on_press),
    )
    .on_enter(on_enter)
    .on_exit(on_exit)
    .into()
}

/// A list row: held when selected, lifted under the pointer.
pub fn list_row<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    selected: bool,
    hover: f32,
    height: f32,
) -> Element<'a, Message> {
    container(content)
        .padding([0.0, t::SPACE_2 + 2.0])
        .height(Length::Fixed(height))
        .center_y(Length::Fixed(height))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(if selected {
                theme::blend(t::SELECTED, t::SECONDARY, 0.6 + 0.4 * hover)
            } else {
                t::with_alpha(t::SECONDARY, 0.75 * hover)
            })),
            border: Border {
                color: if selected {
                    t::BEVEL_RAISED.sides
                } else {
                    Color::TRANSPARENT
                },
                width: if selected { 1.0 } else { 0.0 },
                radius: t::RADIUS.into(),
            },
            ..Default::default()
        })
        .into()
}

/// A segmented control: one choice of a few, DigiClip's filter chips joined
/// into one bevelled bar. The chosen cell is a tile that slides under it, and
/// its label lights as the tile arrives; `thumb` is the screen's record of
/// where the tile is.
pub fn segmented<'a, Message: Clone + 'a>(
    choices: Vec<(String, Message)>,
    thumb: &motion::Thumb,
    now: Instant,
) -> Element<'a, Message> {
    let count = choices.len();
    let mut bar = row![].height(Length::Fill);
    for (index, (words, message)) in choices.into_iter().enumerate() {
        // How much of the thumb is under this cell. The label is lit by it, and
        // the cell stops drawing a fill of its own once the thumb is there.
        let held = thumb.amount(index, now);
        let chosen = thumb.chosen() == index;
        bar = bar.push(
            container(glide(move |hover| {
                button(
                    container(
                        text(words)
                            .size(t::TEXT_XS + 1.0)
                            .font(if chosen { t::FONT_UI_MEDIUM } else { t::FONT_UI })
                            .wrapping(text::Wrapping::None),
                    )
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .padding([0.0, t::SPACE_3])
                .style(move |_, status| {
                    // Under the pointer a cell lifts, and stops lifting as the
                    // tile arrives under it.
                    let lift = hover.get() * (1.0 - held);
                    let fill = match status {
                        button::Status::Pressed => t::NEUTRAL_750,
                        _ => t::with_alpha(t::NEUTRAL_800, lift),
                    };
                    button::Style {
                        background: Some(Background::Color(fill)),
                        text_color: theme::blend(
                            theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, hover.get()),
                            t::FOREGROUND,
                            held,
                        ),
                        border: Border {
                            radius: t::RADIUS_SM.into(),
                            ..Border::default()
                        },
                        ..button::Style::default()
                    }
                })
                .on_press(message)
                .into()
            }))
            .padding([0.0, 1.0])
            .width(Length::Fill)
            .height(Length::Fill),
        );
    }

    let layers = iced::widget::Stack::with_children([Element::from(bar)])
        .width(Length::Fill)
        .height(Length::Fill)
        .push_under(thumb_layer(thumb.position(now), count, |_| container::Style {
            background: Some(Background::Color(t::NEUTRAL_700)),
            border: Border {
                color: t::BEVEL_HOVER.sides,
                width: 1.0,
                radius: t::RADIUS_SM.into(),
            },
            ..container::Style::default()
        }));

    container(layers)
        .padding(2.0)
        .width(Length::Fill)
        .height(Length::Fixed(t::CONTROL_HEIGHT))
        .style(|_| container::Style {
            background: Some(Background::Color(t::BACKGROUND)),
            border: Border {
                color: t::BEVEL_RAISED.sides,
                width: 1.0,
                radius: t::RADIUS.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// The shares of a segmented control's width that fall before, under and after
/// its thumb when the thumb is `position` segments from the first, out of
/// `count` equal ones. A row of three portions laid out with these puts the
/// thumb exactly over that fraction of the control; integer portions of a
/// thousand a segment make the slide smooth to a fraction of a pixel.
pub fn thumb_portions(position: f32, count: usize) -> [u16; 3] {
    const UNIT: f32 = 1000.0;
    let last = count.saturating_sub(1) as f32;
    let position = position.clamp(0.0, last);
    [
        (position * UNIT).round() as u16,
        UNIT as u16,
        ((last - position) * UNIT).round() as u16,
    ]
}

/// The tile that slides under the chosen cell of a segmented control: a layer
/// as wide and tall as the control, holding the tile at `position`. Each cell
/// of the control sits a pixel in from its share of the width on both sides,
/// and so does the tile, which keeps the two lined up and leaves a gap between
/// neighbours.
pub fn thumb_layer<'a, Message: 'a>(
    position: f32,
    count: usize,
    style: impl Fn(&Theme) -> container::Style + 'a,
) -> Element<'a, Message> {
    let share = |portion: u16| {
        if portion == 0 {
            Length::Fixed(0.0)
        } else {
            Length::FillPortion(portion)
        }
    };
    let [before, tile, after] = thumb_portions(position, count);
    row![
        Space::new().width(share(before)),
        container(container(Space::new().width(Length::Fill).height(Length::Fill)).style(style))
            .padding([0.0, 1.0])
            .width(share(tile))
            .height(Length::Fill),
        Space::new().width(share(after)),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Standard button padding: 32px tall with the body size.
pub const BUTTON_PADDING: Padding = Padding {
    top: 7.5,
    right: t::SPACE_3 + 2.0,
    bottom: 7.5,
    left: t::SPACE_3 + 2.0,
};

/// Compact button padding: 28px tall, for buttons inside a dense row.
pub const BUTTON_PADDING_SM: Padding = Padding {
    top: 5.5,
    right: t::SPACE_2 + 2.0,
    bottom: 5.5,
    left: t::SPACE_2 + 2.0,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_keeps_its_avatar_colour() {
        assert_eq!(avatar_tint("EVERCORE"), avatar_tint("EVERCORE"));
    }

    #[test]
    fn avatar_colours_spread_across_the_palette() {
        let names = ["EVERCORE", "desktop", "laptop", "nas", "studio", "pi", "tower", "den"];
        let distinct: std::collections::HashSet<_> = names
            .iter()
            .map(|name| {
                let (fill, _) = avatar_tint(name);
                (fill.r.to_bits(), fill.g.to_bits(), fill.b.to_bits())
            })
            .collect();
        assert!(distinct.len() >= 4, "eight names landed on {} colours", distinct.len());
    }

    #[test]
    fn button_padding_lands_on_the_control_heights() {
        // Body text in iced's default 1.3 line box, plus the vertical padding.
        // A button's edge is drawn inside its bounds, so it adds nothing.
        let line = t::TEXT_SM * 1.3;
        let tall = BUTTON_PADDING.top + BUTTON_PADDING.bottom + line;
        let short = BUTTON_PADDING_SM.top + BUTTON_PADDING_SM.bottom + line;
        assert!((tall - t::CONTROL_HEIGHT).abs() < 0.5, "{tall}");
        assert!((short - t::CONTROL_HEIGHT_SM).abs() < 0.5, "{short}");
    }

    #[test]
    fn the_thumb_of_a_segmented_control_sits_over_its_share_of_the_width() {
        // Three segments: the thumb is a third wide wherever it is, and what
        // is before and after it always adds up to the other two thirds.
        for position in [0.0, 0.25, 1.0, 1.5, 2.0] {
            let [before, tile, after] = thumb_portions(position, 3);
            assert_eq!(tile, 1000);
            assert_eq!(before + after, 2000, "{position}");
        }
        // On a segment it is exactly whole segments in from the left.
        assert_eq!(thumb_portions(0.0, 3), [0, 1000, 2000]);
        assert_eq!(thumb_portions(1.0, 3), [1000, 1000, 1000]);
        assert_eq!(thumb_portions(2.0, 3), [2000, 1000, 0]);
        // Between two it is a proportion of the way.
        assert_eq!(thumb_portions(0.5, 3), [500, 1000, 1500]);
        // And never off either end, whatever a slide's easing overshoots to.
        assert_eq!(thumb_portions(-1.0, 3), thumb_portions(0.0, 3));
        assert_eq!(thumb_portions(9.0, 3), thumb_portions(2.0, 3));
        // A control of one segment has nowhere to slide.
        assert_eq!(thumb_portions(0.0, 1), [0, 1000, 0]);
    }

    #[test]
    fn the_thumb_follows_a_thumb_through_a_whole_slide() {
        let now = Instant::now();
        let mut thumb = motion::Thumb::at(0);
        thumb.select(2, now);
        // Every frame of the slide puts the tile somewhere on the control,
        // and it never runs back on itself.
        let mut last = 0;
        for step in 0..=20 {
            let at = now + motion::STANDARD * step / 20;
            let [before, _, after] = thumb_portions(thumb.position(at), 3);
            assert!(before >= last);
            assert_eq!(before + after, 2000);
            last = before;
        }
        assert_eq!(last, 2000);
    }

    #[test]
    fn a_header_has_room_for_a_control_above_and_below() {
        let room = (t::HEADER_HEIGHT - t::CONTROL_HEIGHT_SM) / 2.0;
        assert!(room >= t::SPACE_3, "{room}");
    }
}
