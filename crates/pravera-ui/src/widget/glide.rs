//! Lets a control's hover arrive and leave instead of switching.
//!
//! An iced button is styled from a status: hovered or not, and nothing in
//! between. That is fine for a control that is only ever hovered for good, but
//! every button in the interface changed colour in a single frame, while the
//! rows and the rail around them eased. Tracking each button's hover in its
//! screen's state would have meant a slot, a message and a branch per button.
//!
//! This wraps a control and keeps the hover itself, in the widget tree, where
//! the control already lives: it notices when the pointer comes and goes,
//! ramps a number from 0 to 1 over [`MICRO`], and hands the number to the
//! control's style through a shared cell that the style reads as it paints.
//! While the ramp runs it asks for redraws; once it is over it asks for
//! nothing, so a screen at rest still costs nothing.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer;
use iced::advanced::widget::{self, tree, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{mouse, window, Element, Event, Length, Rectangle, Size, Vector};

use crate::motion::{EASE_MICRO, MICRO};

/// How hovered the wrapped control is, from 0 to 1, as the widget last
/// worked it out. The control's style reads it while it paints.
pub type Hover = Rc<Cell<f32>>;

/// A fresh, unhovered [`Hover`] for a control to be built with.
pub fn hover() -> Hover {
    Rc::new(Cell::new(0.0))
}

pub struct Glide<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    hover: Hover,
}

impl<'a, Message, Theme, Renderer> Glide<'a, Message, Theme, Renderer> {
    /// Wraps `content`, whose style reads `hover`.
    pub fn new(hover: Hover, content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
        Glide {
            content: content.into(),
            hover,
        }
    }
}

/// Where the ramp was going and where it started.
#[derive(Debug)]
struct State {
    /// Whether the pointer is over the control now.
    over: bool,
    /// How hovered it was drawn the moment `over` last changed.
    from: f32,
    /// When `over` last changed.
    since: Instant,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        State {
            over: false,
            from: 0.0,
            // Long enough ago that a control that has never been touched is
            // not thought to be mid-ramp.
            since: now.checked_sub(MICRO * 2).unwrap_or(now),
        }
    }
}

impl State {
    fn amount(&self, now: Instant) -> f32 {
        ramp(self.from, self.over, now.saturating_duration_since(self.since))
    }

    fn is_animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) < MICRO
    }
}

/// How hovered a control is `elapsed` after its hover last changed: from
/// where it was drawn then, towards 1 if the pointer is over it and 0 if not.
pub fn ramp(from: f32, hovered: bool, elapsed: Duration) -> f32 {
    let to = if hovered { 1.0 } else { 0.0 };
    let progress = (elapsed.as_secs_f32() / MICRO.as_secs_f32()).clamp(0.0, 1.0);
    from + (to - from) * EASE_MICRO.value(progress)
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Glide<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let now = match event {
            Event::Window(window::Event::RedrawRequested(now)) => *now,
            _ => Instant::now(),
        };

        let state = tree.state.downcast_mut::<State>();
        let over = cursor.is_over(layout.bounds());
        if over != state.over {
            state.from = state.amount(now);
            state.since = now;
            state.over = over;
            shell.request_redraw();
        }
        self.hover.set(state.amount(now));
        if state.is_animating(now) {
            shell.request_redraw();
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        self.hover.set(state.amount(Instant::now()));
        self.content
            .as_widget()
            .draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}

impl<'a, Message, Theme, Renderer> From<Glide<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(glide: Glide<'a, Message, Theme, Renderer>) -> Self {
        Element::new(glide)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hover_starts_where_it_was_and_arrives_over_the_micro_tier() {
        assert_eq!(ramp(0.0, true, Duration::ZERO), 0.0);
        let part = ramp(0.0, true, MICRO / 4);
        assert!(part > 0.0 && part < 1.0, "{part}");
        assert_eq!(ramp(0.0, true, MICRO), 1.0);
        assert_eq!(ramp(0.0, true, MICRO * 5), 1.0);
    }

    #[test]
    fn a_hover_that_ends_early_leaves_from_where_it_was_drawn() {
        let drawn = ramp(0.0, true, MICRO / 3);
        assert_eq!(ramp(drawn, false, Duration::ZERO), drawn);
        assert_eq!(ramp(drawn, false, MICRO), 0.0);
        let part = ramp(drawn, false, MICRO / 2);
        assert!(part > 0.0 && part < drawn);
    }

    #[test]
    fn a_control_never_touched_is_not_mid_ramp() {
        let state = State::default();
        assert!(!state.is_animating(Instant::now()));
        assert_eq!(state.amount(Instant::now()), 0.0);
    }

    #[test]
    fn a_hover_that_just_changed_is_animating_until_it_has_landed() {
        let now = Instant::now();
        let state = State {
            over: true,
            from: 0.0,
            since: now,
        };
        assert!(state.is_animating(now));
        assert!(state.is_animating(now + MICRO / 2));
        assert!(!state.is_animating(now + MICRO));
        assert_eq!(state.amount(now + MICRO), 1.0);
    }

    // ----------------------------------------------- what it does to a click

    use crate::headless::{arrive, click, press_and_release, Screen};
    use iced::{Point, Rectangle, Size};

    fn small_button() -> Element<'static, &'static str> {
        crate::components::small_button(None, "Press me", Some("pressed"))
    }

    #[test]
    fn a_press_and_release_over_a_glided_button_publishes_its_message_exactly_once() {
        let mut screen = Screen::new(small_button(), Size::new(300.0, 100.0));
        assert_eq!(screen.feed(click(Point::new(20.0, 12.0))), vec!["pressed"]);
    }

    #[test]
    fn the_first_click_is_a_click_not_a_hover() {
        // A press with no pointer movement before it, as a touch screen or a
        // remote-controlled pointer gives: the wrapper must not need to have
        // seen the pointer arrive before the button under it will fire.
        let mut screen = Screen::new(small_button(), Size::new(300.0, 100.0));
        assert_eq!(
            screen.feed(press_and_release(Point::new(20.0, 12.0))),
            vec!["pressed"]
        );
    }

    #[test]
    fn hovering_first_and_then_clicking_publishes_once_and_never_twice() {
        let mut screen = Screen::new(small_button(), Size::new(300.0, 100.0));
        let at = Point::new(20.0, 12.0);
        let mut published = screen.feed(arrive(at));
        published.extend(screen.feed(press_and_release(at)));
        published.extend(screen.feed(press_and_release(at)));
        assert_eq!(published, vec!["pressed", "pressed"]);
    }

    #[test]
    fn pressing_off_the_button_publishes_nothing() {
        let mut screen = Screen::new(small_button(), Size::new(300.0, 100.0));
        assert!(screen.feed(click(Point::new(250.0, 90.0))).is_empty());
    }

    #[test]
    fn every_point_of_the_button_answers_and_no_point_answers_twice() {
        let mut screen = Screen::new(small_button(), Size::new(300.0, 100.0));
        let clicks = screen.sweep(Rectangle::new(Point::new(0.0, 0.0), Size::new(300.0, 100.0)), 2.0);
        let mut answering = 0;
        for (at, published) in &clicks {
            assert!(published.len() <= 1, "{at:?} published {published:?}");
            answering += published.len();
        }
        assert!(answering > 50, "the button answered only {answering} points");
    }
}
