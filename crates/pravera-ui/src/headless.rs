//! Driving a real interface without a window, for tests.
//!
//! `iced` builds a `UserInterface` from an element, feeds it events and hands
//! back the messages the widgets published. Nothing in that needs a window or a
//! GPU: the software renderer measures text and lays widgets out the same way,
//! and the picture layer's shader is simply not drawn. That makes it possible
//! to press a button in a test and see what the button did, which is the only
//! way to know that a control works rather than that its handler does.
//!
//! Events go in the order a real pointer produces them: it moves, the window
//! redraws, the button goes down, the window redraws, the button comes up. A
//! test that skipped the redraws would miss anything that depends on the
//! widgets having been asked for a frame, such as a hover that has to be
//! registered before the press arrives.

use std::time::Instant;

use iced::advanced::clipboard;
use iced::{mouse, window, Element, Event, Font, Pixels, Point, Size, Theme};
use iced_runtime::user_interface::{Cache, UserInterface};

/// A software renderer, which measures text the way the real one does.
pub fn renderer() -> iced::Renderer {
    iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(Font::DEFAULT, Pixels(16.0)))
}

/// One event, and where the pointer is when it happens.
pub type Step = (Event, mouse::Cursor);

/// The pointer arriving somewhere, and the window drawing a frame with it there.
pub fn arrive(at: Point) -> Vec<Step> {
    let cursor = mouse::Cursor::Available(at);
    vec![
        (Event::Mouse(mouse::Event::CursorMoved { position: at }), cursor),
        (Event::Window(window::Event::RedrawRequested(Instant::now())), cursor),
    ]
}

/// A press and a release with the pointer where it is, and a frame after each.
pub fn press_and_release(at: Point) -> Vec<Step> {
    let cursor = mouse::Cursor::Available(at);
    let button = mouse::Button::Left;
    vec![
        (Event::Mouse(mouse::Event::ButtonPressed(button)), cursor),
        (Event::Window(window::Event::RedrawRequested(Instant::now())), cursor),
        (Event::Mouse(mouse::Event::ButtonReleased(button)), cursor),
        (Event::Window(window::Event::RedrawRequested(Instant::now())), cursor),
    ]
}

/// A whole click: arrive, then press and release.
pub fn click(at: Point) -> Vec<Step> {
    let mut steps = arrive(at);
    steps.extend(press_and_release(at));
    steps
}

/// A built interface that keeps its widget state between the events fed to it,
/// as the running application does between frames.
pub struct Screen<'a, Message> {
    ui: Option<UserInterface<'a, Message, Theme, iced::Renderer>>,
    renderer: iced::Renderer,
}

impl<'a, Message> Screen<'a, Message> {
    pub fn new(element: impl Into<Element<'a, Message>>, size: Size) -> Self {
        let mut renderer = renderer();
        let ui = UserInterface::build(element, size, Cache::default(), &mut renderer);
        Screen {
            ui: Some(ui),
            renderer,
        }
    }

    /// Feed `steps` in order and return every message published, in order.
    pub fn feed(&mut self, steps: impl IntoIterator<Item = Step>) -> Vec<Message> {
        let mut published = Vec::new();
        let ui = self.ui.as_mut().expect("the interface is built");
        for (event, cursor) in steps {
            ui.update(
                std::slice::from_ref(&event),
                cursor,
                &mut self.renderer,
                &mut clipboard::Null,
                &mut published,
            );
        }
        published
    }

    /// Click every `step` pixels across `area`, and report what each click
    /// published.
    ///
    /// Buttons carry no names, so a test finds them by pressing where they
    /// must be and reading what comes back. It is also the more honest test:
    /// what a person does is click somewhere, and a control that only works
    /// when pressed at its exact centre is not one that works.
    pub fn sweep(&mut self, area: iced::Rectangle, step: f32) -> Vec<(Point, Vec<Message>)> {
        let mut clicked = Vec::new();
        let mut y = area.y;
        while y < area.y + area.height {
            let mut x = area.x;
            while x < area.x + area.width {
                let at = Point::new(x, y);
                clicked.push((at, self.feed(click(at))));
                x += step;
            }
            y += step;
        }
        clicked
    }
}
