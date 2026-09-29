//! Custom window chrome, with the open sessions as tabs.
//!
//! The native title bar is switched off (`.decorations(false)`), so this row is
//! the only thing standing between the user and an undraggable, unclosable
//! window. Every affordance the OS used to provide is rebuilt here: drag,
//! double-click to maximise, minimise, maximise, close.
//!
//! ## Tabs belong in the title bar
//!
//! OpenCode puts open work where a browser does, as tabs across the top, and
//! that is exactly what a Pravera session or shell is. It also fixes the dead
//! end the old strip had: that strip only existed on the session screen and
//! only with two or more tabs, so a single terminal had no close, no name and
//! no way back. Here every open tab is always visible, always closable, and
//! the home toggle beside them goes back to the rest of the app.
//!
//! ## Look
//!
//! DigiClip's: 42px, the same floor colour as the sidebar so the two read as
//! one frame, mark and wordmark 16px in, window buttons 32px square with 6px
//! corners. The close button is the only control that turns red, because a
//! warning that appears everywhere stops being read.

use std::time::Instant;

use iced::widget::{button, container, mouse_area, row, text, Space};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use crate::components;
use crate::icon;
use crate::motion::HoverTracker;
use crate::theme::{self, tokens as t};
use crate::Message;

/// Height of the bar.
pub const HEIGHT: f32 = t::TITLEBAR_HEIGHT;

/// Each window control is square.
const CONTROL: f32 = 32.0;

/// Rendered size of a chrome glyph.
const GLYPH: f32 = 10.0;

/// Widest a tab grows before its title is cut.
const TAB_MAX: f32 = 200.0;

/// Which window control is under the pointer, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Minimize,
    Maximize,
    Close,
    /// The home toggle, left of the tabs.
    Home,
}

impl Control {
    pub const ALL: [Control; 4] = [
        Control::Minimize,
        Control::Maximize,
        Control::Close,
        Control::Home,
    ];

    /// Slot in the shared [`HoverTracker`].
    pub const fn index(self) -> usize {
        match self {
            Control::Minimize => 0,
            Control::Maximize => 1,
            Control::Close => 2,
            Control::Home => 3,
        }
    }

    fn message(self) -> Message {
        match self {
            Control::Minimize => Message::Minimize,
            Control::Maximize => Message::ToggleMaximize,
            Control::Close => Message::CloseWindow,
            Control::Home => Message::ToggleHome,
        }
    }
}

/// One open tab, as the bar draws it.
pub struct TabInfo<'a> {
    pub title: &'a str,
    /// A shell rather than a picture.
    pub terminal: bool,
    /// The far end has gone; the tab stays until it is closed so the reason
    /// can be read.
    pub ended: bool,
}

/// Everything the bar shows.
pub struct Bar<'a> {
    pub maximized: bool,
    pub controls: &'a HoverTracker,
    pub tabs: Vec<TabInfo<'a>>,
    /// The tab in front, if the session screen is showing it.
    pub active: Option<usize>,
    pub tab_hover: &'a HoverTracker,
    /// Whether a tab's surface is what fills the window right now.
    pub on_session: bool,
    pub now: Instant,
}

pub fn view<'a>(bar: Bar<'a>) -> Element<'a, Message> {
    let now = bar.now;

    // A title bar's job is to say what window this is.
    let identity = mouse_area(
        container(
            row![
                icon::stroked(icon::LOGO, 16.0, t::FOREGROUND),
                text("Pravera")
                    .size(t::TEXT_SM)
                    .font(t::FONT_UI_STRONG)
                    .style(theme::heading),
            ]
            .spacing(t::SPACE_2)
            .align_y(Alignment::Center),
        )
        // The mark sits on the rail's centre line, so the column of icons
        // under it starts with the app's own.
        .padding(Padding {
            top: 0.0,
            right: t::SPACE_4,
            bottom: 0.0,
            left: (t::SIDEBAR_RAIL - 16.0) / 2.0,
        })
        .center_y(Length::Fill),
    )
    .on_press(Message::DragWindow)
    .on_double_click(Message::ToggleMaximize);

    let mut left = row![identity].align_y(Alignment::Center);

    if !bar.tabs.is_empty() {
        left = left.push(home_toggle(bar.controls, !bar.on_session, now));
        left = left.push(Space::new().width(Length::Fixed(t::SPACE_2)));
        let tabs = bar
            .tabs
            .iter()
            .enumerate()
            .fold(row![].spacing(2.0), |strip, (index, tab)| {
                strip.push(tab_view(
                    index,
                    tab,
                    bar.active == Some(index),
                    bar.tab_hover.amount(index, now),
                ))
            });
        left = left.push(tabs);
    }

    // The drag region fills whatever the tabs leave. It deliberately stops
    // short of every button: a press that lands on one must not also start a
    // window drag.
    let filler = mouse_area(
        container(Space::new())
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Message::DragWindow)
    .on_double_click(Message::ToggleMaximize);

    let controls = row(
        [Control::Minimize, Control::Maximize, Control::Close]
            .map(|which| control(which, bar.maximized, bar.controls, now)),
    )
    .spacing(t::SPACE_1)
    .align_y(Alignment::Center);

    container(
        row![left, filler, controls]
            .align_y(Alignment::Center)
            .height(Length::Fill),
    )
    .padding(iced::Padding {
        right: t::GAP,
        ..iced::Padding::ZERO
    })
    .width(Length::Fill)
    .height(Length::Fixed(HEIGHT))
    .style(|_| container::Style {
        background: Some(Background::Color(t::BACKGROUND)),
        ..Default::default()
    })
    .into()
}

/// OpenCode's home toggle: the grid that goes back to the rest of the app,
/// drawn pressed while the rest of the app is what is showing.
fn home_toggle<'a>(hover: &HoverTracker, pressed: bool, now: Instant) -> Element<'a, Message> {
    let amount = hover.amount(Control::Home.index(), now);
    let background = if pressed {
        t::SECONDARY
    } else {
        t::with_alpha(t::NEUTRAL_850, amount)
    };
    let tint = if pressed {
        t::FOREGROUND
    } else {
        theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, amount)
    };

    mouse_area(
        button(
            container(icon::stroked(icon::GRID, t::ICON_SM, tint))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(t::ROW_HEIGHT))
        .height(Length::Fixed(t::ROW_HEIGHT))
        .padding(0)
        .style(move |_, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Pressed => t::ACCENT,
                _ => background,
            })),
            border: Border {
                radius: t::RADIUS.into(),
                ..Border::default()
            },
            ..button::Style::default()
        })
        .on_press(Control::Home.message()),
    )
    .on_enter(Message::HoverControl(Control::Home, true))
    .on_exit(Message::HoverControl(Control::Home, false))
    .into()
}

/// One tab: avatar, title, and a close that appears with the pointer.
fn tab_view<'a>(index: usize, tab: &TabInfo<'a>, active: bool, hover: f32) -> Element<'a, Message> {
    let tint = if active {
        t::FOREGROUND
    } else {
        theme::blend(t::SUBTLE_FOREGROUND, t::NEUTRAL_300, hover)
    };
    let background = if active {
        t::SECONDARY
    } else {
        t::with_alpha(t::NEUTRAL_850, hover)
    };

    let leading: Element<'a, Message> = if tab.terminal {
        let (_, edge) = components::avatar_tint(tab.title);
        container(icon::stroked(icon::TERMINAL, 12.0, if tab.ended { t::SUBTLE_FOREGROUND } else { edge }))
            .center_x(Length::Fixed(16.0))
            .center_y(Length::Fixed(16.0))
            .into()
    } else {
        components::avatar_faded(tab.title, 16.0, if tab.ended { 0.5 } else { 1.0 })
    };

    // Present on the tab in front, and fading in on the one being pointed at:
    // a close on every tab at rest is a row of ways to lose something.
    let close_alpha = if active { 1.0 } else { hover };
    let close = button(
        container(icon::stroked(
            icon::CLOSE,
            9.0,
            t::with_alpha(t::MUTED_FOREGROUND, close_alpha),
        ))
        .center_x(Length::Fill)
        .center_y(Length::Fill),
    )
    .width(Length::Fixed(18.0))
    .height(Length::Fixed(18.0))
    .padding(0)
    .style(|_, status| button::Style {
        background: Some(Background::Color(match status {
            button::Status::Hovered => t::NEUTRAL_700,
            button::Status::Pressed => t::NEUTRAL_600,
            _ => Color::TRANSPARENT,
        })),
        border: Border {
            radius: t::RADIUS_SM.into(),
            ..Border::default()
        },
        ..button::Style::default()
    })
    .on_press(Message::CloseTab(index));

    let title = text(tab.title)
        .size(t::TEXT_XS + 1.0)
        .font(if active { t::FONT_UI_MEDIUM } else { t::FONT_UI })
        .wrapping(text::Wrapping::None)
        .style(theme::tinted(tint));

    let label = row![
        leading,
        container(title).clip(true).max_width(TAB_MAX - 56.0),
        close
    ]
    .spacing(t::SPACE_1_5)
    .align_y(Alignment::Center);

    mouse_area(
        button(label)
            .height(Length::Fixed(t::ROW_HEIGHT))
            .padding(iced::Padding {
                top: 0.0,
                right: 5.0,
                bottom: 0.0,
                left: t::SPACE_1_5,
            })
            .style(move |_, status| button::Style {
                background: Some(Background::Color(match status {
                    button::Status::Pressed => t::ACCENT,
                    _ => background,
                })),
                text_color: tint,
                border: Border {
                    radius: t::RADIUS.into(),
                    ..Border::default()
                },
                ..button::Style::default()
            })
            .on_press(Message::SwitchTab(index)),
    )
    .on_enter(Message::HoverTab(index, true))
    .on_exit(Message::HoverTab(index, false))
    .into()
}

fn control(
    which: Control,
    maximized: bool,
    hover: &HoverTracker,
    now: Instant,
) -> Element<'static, Message> {
    let amount = hover.amount(which.index(), now);

    // Close fades to red, everything else to the hover surface. Both
    // interpolate rather than switch, which is the whole difference between
    // chrome that feels built in and chrome that feels bolted on.
    let fill = match which {
        Control::Close => t::DESTRUCTIVE,
        _ => t::SECONDARY,
    };
    let background = t::with_alpha(fill, amount);
    let foreground = theme::blend(t::MUTED_FOREGROUND, t::FOREGROUND, amount);

    let glyph = match which {
        Control::Minimize => icon::WIN_MINIMIZE,
        Control::Maximize if maximized => icon::WIN_RESTORE,
        Control::Maximize => icon::WIN_MAXIMIZE,
        Control::Close | Control::Home => icon::WIN_CLOSE,
    };

    mouse_area(
        button(
            container(icon::stroked(glyph, GLYPH, foreground))
                .center_x(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(CONTROL))
        .height(Length::Fixed(CONTROL))
        .padding(0)
        .style(move |_, status| {
            // Press deepens whatever the hover animation has arrived at, so the
            // two states compose instead of one overriding the other.
            let background = match status {
                button::Status::Pressed => match which {
                    Control::Close => t::DESTRUCTIVE_HOVER,
                    _ => t::ACCENT,
                },
                _ => background,
            };
            button::Style {
                background: Some(Background::Color(background)),
                text_color: foreground,
                border: Border {
                    radius: t::RADIUS.into(),
                    ..Border::default()
                },
                ..Default::default()
            }
        })
        .on_press(which.message()),
    )
    // Both halves name the control. An exit that cannot say where it came from
    // is what lets a stale one blank a fresh enter; see `motion::HoverTracker`.
    .on_enter(Message::HoverControl(which, true))
    .on_exit(Message::HoverControl(which, false))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_has_its_own_slot() {
        let indices: Vec<usize> = Control::ALL.iter().map(|c| c.index()).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);
    }

    #[test]
    fn hovering_one_control_leaves_its_neighbours_alone() {
        let now = std::time::Instant::now();
        let mut hover = HoverTracker::new(Control::ALL.len());
        hover.enter(Control::Maximize.index(), now);

        let settled = now + crate::motion::MICRO + std::time::Duration::from_millis(1);
        assert_eq!(hover.amount(Control::Maximize.index(), settled), 1.0);
        assert_eq!(hover.amount(Control::Minimize.index(), settled), 0.0);
        assert_eq!(hover.amount(Control::Close.index(), settled), 0.0);
    }

    /// The regression the user actually hit: sweeping right worked, sweeping
    /// left did not, because the exit fired after the enter and said nothing
    /// about which control it belonged to.
    #[test]
    fn sweeping_across_the_controls_in_either_direction_keeps_the_hover() {
        let now = std::time::Instant::now();
        let mut hover = HoverTracker::new(Control::ALL.len());

        hover.enter(Control::Minimize.index(), now);
        hover.exit(Control::Minimize.index(), now);
        hover.enter(Control::Maximize.index(), now);
        assert_eq!(hover.current(), Some(Control::Maximize.index()));

        hover.enter(Control::Minimize.index(), now);
        hover.exit(Control::Maximize.index(), now);
        assert_eq!(hover.current(), Some(Control::Minimize.index()));
    }

    #[test]
    fn a_maximised_window_offers_to_restore_itself() {
        assert_ne!(icon::WIN_RESTORE, icon::WIN_MAXIMIZE);
    }
}
