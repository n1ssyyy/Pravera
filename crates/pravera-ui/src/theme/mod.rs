//! Pravera's visual language, expressed as iced styles.
//!
//! Every style here reads from [`tokens`] and nowhere else. Widgets in the
//! screens layer never write a colour or a radius directly; they pick one of
//! these, or one of the composed surfaces in [`crate::components`].

// The design system defines its full token set and style vocabulary up front so
// that later screens select an existing token rather than inventing a value.
// Tokens not yet consumed are intentional, not oversights.
#![allow(dead_code)]

pub mod tokens;

use iced::widget::{button, container, scrollable, text_input};
use iced::{Background, Border, Color, Shadow, Theme, Vector};

use tokens as t;

/// The application theme.
///
/// iced generates its own extended palette from these six roles, which is used
/// only by stock widgets we have not styled explicitly. Everything Pravera
/// draws goes through the functions below instead, because six roles cannot
/// express a full neutral scale.
pub fn theme() -> Theme {
    Theme::custom(
        "Pravera".to_string(),
        iced::theme::Palette {
            background: t::BACKGROUND,
            text: t::FOREGROUND,
            primary: t::PRIMARY,
            success: t::SUCCESS,
            warning: t::WARNING,
            danger: t::DESTRUCTIVE,
        },
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn border(color: Color, radius: f32) -> Border {
    Border {
        color,
        width: t::BORDER_WIDTH,
        radius: radius.into(),
    }
}

fn no_border(radius: f32) -> Border {
    Border {
        color: Color::TRANSPARENT,
        width: 0.0,
        radius: radius.into(),
    }
}

/// Linear blend between two colours; `amount` is clamped to 0..=1.
pub fn blend(from: Color, to: Color, amount: f32) -> Color {
    let k = amount.clamp(0.0, 1.0);
    Color {
        r: from.r + (to.r - from.r) * k,
        g: from.g + (to.g - from.g) * k,
        b: from.b + (to.b - from.b) * k,
        a: from.a + (to.a - from.a) * k,
    }
}

/// Multiplies a colour's alpha: how a fading element fades everything it
/// paints by the same amount.
pub fn faded(color: Color, opacity: f32) -> Color {
    Color {
        a: color.a * opacity.clamp(0.0, 1.0),
        ..color
    }
}

/// On a near-black floor a shadow is mostly invisible, so the system barely
/// uses them. The exceptions are things that genuinely float — menus, dialogs,
/// toasts — where a soft dark falloff is what separates them from what they
/// cover.
pub const NO_SHADOW: Shadow = Shadow {
    color: Color::TRANSPARENT,
    offset: Vector::new(0.0, 0.0),
    blur_radius: 0.0,
};

/// A menu, tooltip or toast.
pub const SHADOW_FLOAT: Shadow = Shadow {
    color: Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.45,
    },
    offset: Vector::new(0.0, 8.0),
    blur_radius: 20.0,
};

/// A dialog over the scrim.
pub const SHADOW_OVERLAY: Shadow = Shadow {
    color: Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.55,
    },
    offset: Vector::new(0.0, 18.0),
    blur_radius: 44.0,
};

// ---------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------

/// The page background.
pub fn root(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::BACKGROUND)),
        text_color: Some(t::FOREGROUND),
        ..container::Style::default()
    }
}

/// A raised panel with a uniform edge. Prefer [`crate::components::panel`],
/// which draws the bevel; this is for surfaces that are animated through a
/// style closure and cannot be wrapped.
pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::CARD)),
        text_color: Some(t::FOREGROUND),
        border: border(t::BEVEL_CARD.sides, t::RADIUS),
        shadow: NO_SHADOW,
        ..container::Style::default()
    }
}

/// A card under the pointer. One tone brighter, nothing else moves.
pub fn card_hovered(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::ROW)),
        text_color: Some(t::FOREGROUND),
        border: border(t::BEVEL_HOVER.sides, t::RADIUS),
        shadow: NO_SHADOW,
        ..container::Style::default()
    }
}

/// A row or tile inside a card.
pub fn row_surface(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::ROW)),
        text_color: Some(t::FOREGROUND),
        border: border(t::BORDER, t::RADIUS),
        ..container::Style::default()
    }
}

/// A floating surface: menu or dialog.
pub fn popover(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::POPOVER)),
        text_color: Some(t::FOREGROUND),
        border: border(t::BEVEL_RAISED.sides, t::RADIUS),
        shadow: SHADOW_FLOAT,
        ..container::Style::default()
    }
}

/// A tooltip: DigiClip's, a #1a1a1a slip with a hairline.
pub fn tooltip(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::TOAST)),
        text_color: Some(t::FOREGROUND),
        border: border(t::BORDER, t::RADIUS),
        shadow: SHADOW_FLOAT,
        ..container::Style::default()
    }
}

/// A small status pill. `tint` carries the meaning; the fill is the same colour
/// at low alpha so the pill never shouts.
pub fn badge(tint: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(t::with_alpha(tint, 0.12))),
        text_color: Some(tint),
        border: border(t::with_alpha(tint, 0.28), t::RADIUS_SM),
        shadow: NO_SHADOW,
        ..container::Style::default()
    }
}

/// A neutral tag: machine metadata that is not a status.
pub fn tag(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::NEUTRAL_825)),
        text_color: Some(t::MUTED_FOREGROUND),
        border: border(t::BORDER, t::RADIUS_SM),
        ..container::Style::default()
    }
}

/// The lime pill: "ready", "live". Black on lime, fully rounded.
pub fn lime_pill(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::LIME)),
        text_color: Some(t::LIME_FOREGROUND),
        border: no_border(t::RADIUS_FULL),
        ..container::Style::default()
    }
}

/// A hairline separator.
pub fn divider(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(t::BORDER)),
        ..container::Style::default()
    }
}

/// A filled dot of one colour.
pub fn dot(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: no_border(t::RADIUS_FULL),
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------
//
// DigiClip's set: primary is inverted white, secondary a filled grey, outline a
// hairline that fills on hover, ghost nothing until hovered, destructive red.
// Every variant dims to half strength when disabled rather than changing hue,
// so a disabled control still says what it would do.

fn disabled(style: button::Style) -> button::Style {
    button::Style {
        background: style.background.map(|background| match background {
            Background::Color(color) => Background::Color(faded(color, 0.5)),
            other => other,
        }),
        text_color: faded(style.text_color, 0.5),
        border: Border {
            color: faded(style.border.color, 0.5),
            ..style.border
        },
        ..style
    }
}

/// The one important action on a screen.
pub fn primary_button(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => t::PRIMARY_HOVER,
        button::Status::Pressed => t::NEUTRAL_300,
        _ => t::PRIMARY,
    };
    let style = button::Style {
        background: Some(Background::Color(bg)),
        text_color: t::PRIMARY_FOREGROUND,
        border: no_border(t::RADIUS),
        shadow: NO_SHADOW,
        ..button::Style::default()
    };
    if matches!(status, button::Status::Disabled) {
        disabled(style)
    } else {
        style
    }
}

/// A quiet filled action, for everything that is not the primary one.
///
/// Carries the raised hairline as its edge: on a near-black card a flat grey
/// fill reads as a smudge, and the edge is what makes it read as something
/// that can be pressed.
pub fn secondary_button(_: &Theme, status: button::Status) -> button::Style {
    let (bg, edge) = match status {
        button::Status::Hovered => (t::NEUTRAL_700, t::BEVEL_HOVER.sides),
        button::Status::Pressed => (t::NEUTRAL_750, t::BEVEL_RAISED.sides),
        _ => (t::SECONDARY, t::BEVEL_RAISED.sides),
    };
    let style = button::Style {
        background: Some(Background::Color(bg)),
        text_color: t::FOREGROUND,
        border: border(edge, t::RADIUS),
        shadow: NO_SHADOW,
        ..button::Style::default()
    };
    if matches!(status, button::Status::Disabled) {
        disabled(style)
    } else {
        style
    }
}

/// An outlined action: visible at rest, fills on hover.
pub fn outline_button(_: &Theme, status: button::Status) -> button::Style {
    let (bg, bc) = match status {
        button::Status::Hovered => (t::SECONDARY, t::NEUTRAL_700),
        button::Status::Pressed => (t::ACCENT, t::NEUTRAL_700),
        _ => (t::BACKGROUND, t::BORDER),
    };
    let style = button::Style {
        background: Some(Background::Color(bg)),
        text_color: t::FOREGROUND,
        border: border(bc, t::RADIUS),
        shadow: NO_SHADOW,
        ..button::Style::default()
    };
    if matches!(status, button::Status::Disabled) {
        disabled(style)
    } else {
        style
    }
}

/// No chrome until hovered. For toolbar icons and low-stakes actions.
pub fn ghost_button(_: &Theme, status: button::Status) -> button::Style {
    let (bg, fg) = match status {
        button::Status::Hovered => (t::SECONDARY, t::FOREGROUND),
        button::Status::Pressed => (t::ACCENT, t::FOREGROUND),
        button::Status::Disabled => (Color::TRANSPARENT, t::with_alpha(t::MUTED_FOREGROUND, 0.5)),
        button::Status::Active => (Color::TRANSPARENT, t::MUTED_FOREGROUND),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: fg,
        border: no_border(t::RADIUS),
        shadow: NO_SHADOW,
        ..button::Style::default()
    }
}

/// An action that destroys something. The only button allowed to be red.
pub fn destructive_button(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => t::DESTRUCTIVE_HOVER,
        button::Status::Pressed => t::with_alpha(t::DESTRUCTIVE_HOVER, 0.8),
        _ => t::DESTRUCTIVE,
    };
    let style = button::Style {
        background: Some(Background::Color(bg)),
        text_color: t::DESTRUCTIVE_FOREGROUND,
        border: no_border(t::RADIUS),
        shadow: NO_SHADOW,
        ..button::Style::default()
    };
    if matches!(status, button::Status::Disabled) {
        disabled(style)
    } else {
        style
    }
}

/// A destructive action that is not the point of the screen: red text on a
/// ghost, filling with a soft red on hover. "Remove", "Forget", "End".
pub fn danger_ghost_button(_: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => t::DESTRUCTIVE_SOFT,
        button::Status::Pressed => t::with_alpha(t::DESTRUCTIVE, 0.25),
        _ => Color::TRANSPARENT,
    };
    let style = button::Style {
        background: Some(Background::Color(bg)),
        text_color: t::DESTRUCTIVE_TEXT,
        border: no_border(t::RADIUS),
        shadow: NO_SHADOW,
        ..button::Style::default()
    };
    if matches!(status, button::Status::Disabled) {
        disabled(style)
    } else {
        style
    }
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// A text field: sunk into the card it sits on, one step below it, with the
/// raised hairline as its edge — DigiClip's input, which reads as a well to
/// type into rather than as another panel.
pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let (bg, bc, width) = match status {
        text_input::Status::Active => (t::BACKGROUND, t::BEVEL_RAISED.sides, t::BORDER_WIDTH),
        text_input::Status::Hovered => (t::BACKGROUND, t::NEUTRAL_600, t::BORDER_WIDTH),
        // A visible ring is the only affordance telling a keyboard user where
        // they are, so it is deliberately the brightest border in the system.
        text_input::Status::Focused { .. } => (t::BACKGROUND, t::RING, t::BORDER_WIDTH),
        text_input::Status::Disabled => (t::BACKGROUND, t::NEUTRAL_825, t::BORDER_WIDTH),
    };
    text_input::Style {
        background: Background::Color(bg),
        border: Border {
            color: bc,
            width,
            radius: t::RADIUS.into(),
        },
        icon: t::MUTED_FOREGROUND,
        placeholder: t::SUBTLE_FOREGROUND,
        value: t::FOREGROUND,
        selection: t::with_alpha(t::PRIMARY, 0.22),
    }
}

// ---------------------------------------------------------------------------
// Scrollbars
// ---------------------------------------------------------------------------

/// A 4px thumb that barely exists until it is needed: OpenCode's, lifted to
/// DigiClip's greys. The rail itself is never drawn.
pub fn scrollbar(_: &Theme, status: scrollable::Status) -> scrollable::Style {
    let (hovered, dragged) = match status {
        scrollable::Status::Active { .. } => (false, false),
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered,
            ..
        } => (is_vertical_scrollbar_hovered, false),
        scrollable::Status::Dragged {
            is_vertical_scrollbar_dragged,
            ..
        } => (false, is_vertical_scrollbar_dragged),
    };
    let thumb = if dragged {
        t::NEUTRAL_500
    } else if hovered {
        t::NEUTRAL_600
    } else {
        t::NEUTRAL_750
    };
    let rail = scrollable::Rail {
        background: None,
        border: no_border(t::RADIUS_FULL),
        scroller: scrollable::Scroller {
            background: Background::Color(thumb),
            border: no_border(t::RADIUS_FULL),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(t::TOAST),
            border: border(t::BORDER, t::RADIUS_FULL),
            shadow: SHADOW_FLOAT,
            icon: t::FOREGROUND,
        },
    }
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

pub fn heading(_: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(t::FOREGROUND),
    }
}

pub fn muted(_: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(t::MUTED_FOREGROUND),
    }
}

pub fn subtle(_: &Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(t::SUBTLE_FOREGROUND),
    }
}

pub fn tinted(color: Color) -> impl Fn(&Theme) -> iced::widget::text::Style {
    move |_| iced::widget::text::Style { color: Some(color) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_button_keeps_its_hue_at_half_strength() {
        let style = primary_button(&theme(), button::Status::Disabled);
        let Some(Background::Color(fill)) = style.background else {
            panic!("primary always has a fill");
        };
        assert_eq!((fill.r, fill.g, fill.b), (t::PRIMARY.r, t::PRIMARY.g, t::PRIMARY.b));
        assert!((fill.a - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn blend_lands_on_both_ends() {
        assert_eq!(blend(t::BACKGROUND, t::FOREGROUND, 0.0), t::BACKGROUND);
        assert_eq!(blend(t::BACKGROUND, t::FOREGROUND, 1.0), t::FOREGROUND);
        assert_eq!(blend(t::BACKGROUND, t::FOREGROUND, 7.0), t::FOREGROUND);
    }

    #[test]
    fn fading_multiplies_rather_than_replaces() {
        let half = faded(t::with_alpha(t::FOREGROUND, 0.5), 0.5);
        assert!((half.a - 0.25).abs() < f32::EPSILON);
    }
}
