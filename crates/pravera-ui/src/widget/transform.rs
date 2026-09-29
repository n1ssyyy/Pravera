//! Moves, scales and fades a subtree without touching layout.
//!
//! Every entrance used to be faked with padding: a panel "slid" by being laid
//! out 800px lower and brought back up. That reflowed everything around it,
//! grew the scrollable it sat in, got clamped by iced whenever the free space
//! ran out (the dialog that sat stuck at the bottom and then jumped), and it
//! never faded, because iced 0.14 has no group opacity.
//!
//! This widget does it the way a compositor would. Layout is the child's own,
//! untouched; drawing happens under a transformation, and the pointer is
//! mapped back through the inverse so hover and clicks land where the content
//! is drawn, not where it will end up.
//!
//! ## The fade
//!
//! There is no way to multiply the alpha of everything a subtree paints. What
//! there is, is a layer: a quad drawn in a *new* layer after the child is
//! composited over all of it, text included (inside one layer iced draws text
//! after quads, so a quad in the same layer would sit underneath the words).
//! Painting that quad in the colour of the surface the content sits on, at
//! `1 - opacity`, is indistinguishable from fading the content in from that
//! surface. It only works over a flat surface, which is where it is used:
//! pages and panels on the window floor, rows inside a card. A dialog over the
//! blurred backdrop has nothing flat behind it and threads its alpha through
//! its styles instead.

use iced::advanced::layout::{self, Layout};
use iced::advanced::overlay;
use iced::advanced::renderer::{self, Quad};
use iced::advanced::widget::{self, Tree, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::{
    mouse, Border, Color, Element, Event, Length, Rectangle, Size, Transformation, Vector,
};

pub struct Transform<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    offset: Vector,
    scale: f32,
    /// The surface colour to fade from, and how opaque the content is.
    cover: Option<(Color, f32)>,
    /// Corner radius of the cover, so a fading rounded card does not show
    /// square corners of floor colour against a darker surround.
    radius: f32,
}

impl<'a, Message, Theme, Renderer> Transform<'a, Message, Theme, Renderer> {
    pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
        Transform {
            content: content.into(),
            offset: Vector::ZERO,
            scale: 1.0,
            cover: None,
            radius: 0.0,
        }
    }

    /// Draws the content displaced by `offset`, in logical pixels.
    pub fn offset(mut self, offset: Vector) -> Self {
        self.offset = offset;
        self
    }

    /// Draws the content scaled about its centre.
    pub fn scale(mut self, scale: f32) -> Self {
        self.scale = scale;
        self
    }

    /// Fades the content in from `surface`: at `opacity` 0 it is invisible
    /// against that colour, at 1 it is untouched.
    pub fn fade(mut self, surface: Color, opacity: f32) -> Self {
        let opacity = opacity.clamp(0.0, 1.0);
        self.cover = (opacity < 0.999).then_some((surface, opacity));
        self
    }

    /// Rounds the fade cover to match rounded content.
    pub fn radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    fn transformation(&self, bounds: Rectangle) -> Transformation {
        let centre = bounds.center();
        Transformation::translate(centre.x + self.offset.x, centre.y + self.offset.y)
            * Transformation::scale(self.scale)
            * Transformation::translate(-centre.x, -centre.y)
    }

    fn is_identity(&self) -> bool {
        self.offset == Vector::ZERO && (self.scale - 1.0).abs() < f32::EPSILON
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Transform<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::Renderer,
{
    fn tag(&self) -> widget::tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> widget::tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
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
            .operate(tree, layout, renderer, operation);
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
        let (cursor, viewport) = if self.is_identity() {
            (cursor, *viewport)
        } else {
            let inverse = self.transformation(layout.bounds()).inverse();
            (cursor * inverse, *viewport * inverse)
        };
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, &viewport,
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
        let bounds = layout.bounds();

        if self.is_identity() {
            self.content
                .as_widget()
                .draw(tree, renderer, theme, style, layout, cursor, viewport);
        } else {
            let transformation = self.transformation(bounds);
            let inverse = transformation.inverse();
            renderer.with_transformation(transformation, |renderer| {
                self.content.as_widget().draw(
                    tree,
                    renderer,
                    theme,
                    style,
                    layout,
                    cursor * inverse,
                    &(*viewport * inverse),
                );
            });
        }

        if let Some((surface, opacity)) = self.cover {
            // Over the content as drawn, so the cover moves with it.
            let drawn = bounds * self.transformation(bounds);
            // A layer of its own: see the module note on why the same layer
            // would leave the text showing through.
            renderer.with_layer(*viewport, |renderer| {
                renderer.fill_quad(
                    Quad {
                        bounds: drawn,
                        border: Border {
                            radius: self.radius.into(),
                            ..Border::default()
                        },
                        ..Quad::default()
                    },
                    Color {
                        a: surface.a * (1.0 - opacity),
                        ..surface
                    },
                );
            });
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let (cursor, viewport) = if self.is_identity() {
            (cursor, *viewport)
        } else {
            let inverse = self.transformation(layout.bounds()).inverse();
            (cursor * inverse, *viewport * inverse)
        };
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, &viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        // Overlays (tooltips, pick lists) follow the slide but not the scale;
        // they are only ever open once the content has settled.
        self.content.as_widget_mut().overlay(
            tree,
            layout,
            renderer,
            viewport,
            translation + self.offset,
        )
    }
}

impl<'a, Message, Theme, Renderer> From<Transform<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::Renderer + 'a,
{
    fn from(transform: Transform<'a, Message, Theme, Renderer>) -> Self {
        Element::new(transform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> Transform<'static, ()> {
        Transform::new(iced::widget::Space::new())
    }

    #[test]
    fn an_offset_moves_every_point_by_exactly_that_much() {
        let bounds = Rectangle::new(iced::Point::new(10.0, 20.0), Size::new(100.0, 50.0));
        let moved = bounds * probe().offset(Vector::new(0.0, 12.0)).transformation(bounds);
        assert_eq!(moved.x, 10.0);
        assert_eq!(moved.y, 32.0);
        assert_eq!(moved.width, 100.0);
    }

    #[test]
    fn scaling_keeps_the_centre_where_it_was() {
        let bounds = Rectangle::new(iced::Point::new(0.0, 0.0), Size::new(200.0, 100.0));
        let scaled = bounds * probe().scale(0.5).transformation(bounds);
        assert_eq!(scaled.center(), bounds.center());
        assert_eq!(scaled.width, 100.0);
    }

    #[test]
    fn a_settled_transform_does_no_work() {
        assert!(probe().is_identity());
        assert!(probe().fade(Color::BLACK, 1.0).cover.is_none());
        assert!(probe().fade(Color::BLACK, 0.3).cover.is_some());
    }
}
