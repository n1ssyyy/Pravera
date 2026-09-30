//! The host's cursor, drawn over the picture.
//!
//! The picture carries no cursor from protocol version 3: the host sends the
//! pointer's image and position separately (see `pravera_client::cursor`) and
//! this draws it. That is what makes the pointer feel local. An image drawn at
//! the position of the viewer's own mouse, this frame, has no network in its
//! path at all; a cursor baked into the video arrives a round trip plus a
//! frame late.
//!
//! ## Where it is drawn
//!
//! - **Controlling, pointer over the picture**: at the local pointer. The host
//!   will place its own cursor there a moment later, so this is a prediction
//!   that is right except while the host is still catching up.
//! - **Otherwise** (view only, the pointer elsewhere, gaming mode): at the
//!   position the host reported.
//! - **Host says it is hidden, or nothing has arrived**: nothing is drawn, and
//!   the viewer's own cursor is left alone. That is what version 2 did, and it
//!   is the right fallback for a host that has no cursor to show.
//!
//! All the geometry is in [`placement`], which is a pure function of the same
//! [`fit`](crate::widget::video::fit) rectangle the picture and pointer input
//! use. Cursor positions arrive in the picture's own pixels, the same space a
//! click leaves in, so the mapping is the inverse of `point_in` and nothing
//! else.

use std::sync::Arc;

use iced::widget::canvas::{self, Canvas};
use iced::widget::image;
use iced::{mouse, Length, Point, Rectangle, Renderer, Theme};
use pravera_core::Resolution;

use crate::widget::video::fit;

/// One cursor image, ready to draw. Built once when the shape first arrives:
/// an image handle uploads its pixels the first time it is drawn and is looked
/// up by identity afterwards, so rebuilding one per frame would re-upload it
/// per frame.
#[derive(Debug, Clone)]
pub struct Shape {
    pub handle: image::Handle,
    pub width: u16,
    pub height: u16,
    /// The pixel of the image that sits on the pointer.
    pub hot_x: u16,
    pub hot_y: u16,
}

/// Where the host says its pointer is.
#[derive(Debug, Clone)]
pub struct Remote {
    /// In pixels of the streamed picture.
    pub x: i32,
    pub y: i32,
    /// False when the pointer is on another display or an application hid it.
    pub visible: bool,
    pub shape: Option<Arc<Shape>>,
}

impl Remote {
    /// Whether there is something to draw. When this is false the viewer's own
    /// cursor must stay visible, so nothing that hides it may be keyed on
    /// anything but this.
    pub fn is_drawable(&self) -> bool {
        self.visible && self.shape.is_some()
    }
}

/// Where a position in the picture's pixels lands in a widget of `bounds`.
///
/// The inverse of [`point_in`](crate::widget::video::point_in), except that it
/// takes pixels rather than a fraction and clamps rather than refusing: the
/// host's pointer is on the picture by construction, and one that rounds a
/// pixel over the edge should sit on the edge.
pub fn point_of(bounds: Rectangle, picture: Resolution, x: i32, y: i32) -> Point {
    let content = fit(bounds, picture);
    let across = (x as f32 / picture.width.max(1) as f32).clamp(0.0, 1.0);
    let down = (y as f32 / picture.height.max(1) as f32).clamp(0.0, 1.0);
    Point::new(
        content.x + across * content.width,
        content.y + down * content.height,
    )
}

/// The rectangle to draw the cursor image in, or `None` for nothing.
///
/// `local` is the viewer's own pointer in the same coordinates as `bounds`, and
/// `follow_local` says whether to trust it: the caller sets it only while the
/// person is controlling the host. The image is scaled by the same factor as
/// the picture, so a cursor keeps its size relative to what it is pointing at.
pub fn placement(
    bounds: Rectangle,
    picture: Resolution,
    remote: &Remote,
    local: Option<Point>,
    follow_local: bool,
) -> Option<Rectangle> {
    if !remote.visible {
        return None;
    }
    let shape = remote.shape.as_ref()?;
    let content = fit(bounds, picture);
    if content.width <= 0.0 || content.height <= 0.0 || picture.width == 0 {
        return None;
    }

    let scale = content.width / picture.width as f32;
    let at = match local {
        Some(point) if follow_local && content.contains(point) => point,
        _ => point_of(bounds, picture, remote.x, remote.y),
    };

    Some(Rectangle {
        x: at.x - f32::from(shape.hot_x) * scale,
        y: at.y - f32::from(shape.hot_y) * scale,
        width: f32::from(shape.width) * scale,
        height: f32::from(shape.height) * scale,
    })
}

/// The overlay: a canvas the size of the picture's widget, drawing the cursor
/// and nothing else. It handles no events and reports no interaction, so the
/// picture underneath receives everything as if it were not there.
pub struct Layer {
    pub picture: Resolution,
    pub remote: Option<Remote>,
    pub follow_local: bool,
}

impl<Message> canvas::Program<Message> for Layer {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let Some(remote) = &self.remote else {
            return Vec::new();
        };
        // The canvas draws in its own coordinates, so everything is measured
        // from its top-left rather than from the window's.
        let own = Rectangle::new(Point::ORIGIN, bounds.size());
        let local = cursor.position_in(bounds);
        let Some(rect) = placement(own, self.picture, remote, local, self.follow_local) else {
            return Vec::new();
        };
        let Some(shape) = &remote.shape else {
            return Vec::new();
        };

        let mut frame = canvas::Frame::new(renderer, bounds.size());
        frame.draw_image(
            rect,
            canvas::Image::new(shape.handle.clone()).filter_method(image::FilterMethod::Linear),
        );
        vec![frame.into_geometry()]
    }
}

/// The overlay as a widget filling its parent.
pub fn layer<'a, Message: 'a>(program: Layer) -> Canvas<Layer, Message> {
    Canvas::new(program).width(Length::Fill).height(Length::Fill)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::video::point_in;

    const PICTURE: Resolution = Resolution::new(1920, 1080);

    fn widget(width: f32, height: f32) -> Rectangle {
        Rectangle::new(Point::ORIGIN, iced::Size::new(width, height))
    }

    fn remote(x: i32, y: i32, hot: (u16, u16)) -> Remote {
        Remote {
            x,
            y,
            visible: true,
            shape: Some(Arc::new(Shape {
                handle: image::Handle::from_rgba(2, 2, vec![0u8; 16]),
                width: 32,
                height: 32,
                hot_x: hot.0,
                hot_y: hot.1,
            })),
        }
    }

    #[test]
    fn a_pixel_maps_to_where_a_click_there_would_have_come_from() {
        // The reverse of point_in: draw at the position a pointer at `p` would
        // have reported, and asking which fraction that is gives `p` back.
        for bounds in [widget(1920.0, 1080.0), widget(1200.0, 600.0), widget(800.0, 900.0)] {
            for (x, y) in [(0, 0), (960, 540), (1919, 1079), (100, 900)] {
                let at = point_of(bounds, PICTURE, x, y);
                let (fx, fy) = point_in(bounds, PICTURE, at).expect("on the picture");
                assert!((fx * 1920.0 - x as f32).abs() < 0.01, "{bounds:?} {x}");
                assert!((fy * 1080.0 - y as f32).abs() < 0.01, "{bounds:?} {y}");
            }
        }
    }

    #[test]
    fn a_letterboxed_picture_offsets_the_cursor_by_the_bars() {
        // 16:9 in a 2:1 widget: bars of 66.67 either side.
        let at = point_of(widget(1200.0, 600.0), PICTURE, 0, 0);
        assert!((at.x - 66.667).abs() < 0.01 && at.y == 0.0, "{at:?}");
        let far = point_of(widget(1200.0, 600.0), PICTURE, 1920, 1080);
        assert!((far.x - 1133.333).abs() < 0.01 && far.y == 600.0, "{far:?}");
    }

    #[test]
    fn a_position_outside_the_picture_sits_on_its_edge() {
        let bounds = widget(960.0, 540.0);
        assert_eq!(point_of(bounds, PICTURE, -50, 2000), Point::new(0.0, 540.0));
    }

    #[test]
    fn the_image_is_scaled_with_the_picture_and_the_hotspot_is_on_the_point() {
        // Half size: a 32px cursor is 16px, and its hotspot (8,4) is 4,2 in.
        let bounds = widget(960.0, 540.0);
        let rect = placement(bounds, PICTURE, &remote(960, 540, (8, 4)), None, false).unwrap();
        assert_eq!((rect.width, rect.height), (16.0, 16.0));
        assert_eq!((rect.x + 4.0, rect.y + 2.0), (480.0, 270.0));
    }

    #[test]
    fn while_controlling_it_is_drawn_at_the_local_pointer_not_the_hosts_last_report() {
        let bounds = widget(960.0, 540.0);
        let host = remote(0, 0, (0, 0));
        let local = Point::new(300.0, 200.0);

        let following = placement(bounds, PICTURE, &host, Some(local), true).unwrap();
        assert_eq!((following.x, following.y), (300.0, 200.0));

        // Not controlling: the host's own report is the truth.
        let watching = placement(bounds, PICTURE, &host, Some(local), false).unwrap();
        assert_eq!((watching.x, watching.y), (0.0, 0.0));
    }

    #[test]
    fn a_local_pointer_in_the_letterbox_does_not_drag_the_cursor_out_there() {
        let bounds = widget(1200.0, 600.0);
        let host = remote(960, 540, (0, 0));
        let in_the_bar = Point::new(10.0, 300.0);
        let rect = placement(bounds, PICTURE, &host, Some(in_the_bar), true).unwrap();
        assert!((rect.x - 600.0).abs() < 0.01, "{rect:?}");
    }

    #[test]
    fn a_hidden_or_shapeless_cursor_draws_nothing() {
        let bounds = widget(960.0, 540.0);
        let mut host = remote(1, 1, (0, 0));
        host.visible = false;
        assert!(placement(bounds, PICTURE, &host, None, false).is_none());
        assert!(!host.is_drawable());

        let mut host = remote(1, 1, (0, 0));
        host.shape = None;
        assert!(placement(bounds, PICTURE, &host, None, false).is_none());
        assert!(!host.is_drawable());
    }

    #[test]
    fn a_zero_sized_widget_draws_nothing_rather_than_dividing_by_zero() {
        assert!(placement(widget(0.0, 0.0), PICTURE, &remote(1, 1, (0, 0)), None, false).is_none());
    }
}
