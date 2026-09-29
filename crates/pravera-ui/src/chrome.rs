//! The part of the window Pravera does not draw.
//!
//! The title bar, the buttons and the whole interior belong to
//! [`crate::components::titlebar`] and the screens. What is left is the outline
//! itself — the corners, the hairline around the edge, the shadow the
//! compositor casts — and on Windows that outline is drawn by the desktop
//! window manager, not by us.
//!
//! Left alone it comes out wrong for this application. A window with its
//! decorations turned off still gets the system's default frame colour, which
//! is a light grey chosen for light-mode applications, and on a machine where
//! the "show accent colour on title bars" setting is on it gets the accent
//! colour instead. Either one puts a stripe around a dark neutral interface in
//! a colour the design system never chose.
//!
//! Three attributes fix it, all of them one call each:
//!
//! * **Rounded corners.** Windows 11 rounds ordinary windows at eight pixels,
//!   which is exactly [`tokens::RADIUS_LG`] — the corner of the content
//!   frame, one step up from the cards inside it. Asking for it rather than
//!   drawing it ourselves is not laziness: the compositor's rounding is
//!   anti-aliased in hardware, clips the shadow to match, and keeps the corner
//!   correct through snapping, maximising and DPI changes, none of which a
//!   shape drawn inside the client area would survive.
//! * **Dark mode.** Everything the system still draws for a frameless window —
//!   the resize border, the snap-layout flyout — follows this flag.
//! * **Border colour.** Set to the same [`tokens::BORDER`] the panels inside
//!   use, so the hairline reads as the outermost edge of the interface rather
//!   than as a frame around it.
//!
//! Everywhere else this is a no-op. Nothing in the application depends on it
//! having worked, so a failure is logged once and forgotten: a square-cornered
//! window is a cosmetic disappointment, not a broken one.

use iced::window::Id;
use iced::Task;

/// Ask the window manager to draw Pravera's outline the way the rest of the
/// interface is drawn.
///
/// Runs on the windowing thread, which is where these calls have to happen, and
/// resolves to nothing — there is no outcome the application needs to react to.
pub fn dress<T: Send + 'static>(window: Id) -> Task<T> {
    iced::window::run(window, |handle| {
        platform::dress(handle);
    })
    .discard()
}

#[cfg(windows)]
mod platform {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{COLORREF, HWND};
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMWINDOWATTRIBUTE,
    };

    use crate::theme::tokens;

    pub fn dress(handle: &dyn HasWindowHandle) {
        let Some(window) = hwnd(handle) else {
            return;
        };

        // Corner first. It is the one that changes the silhouette, and doing it
        // before the colours means a slow frame cannot show a square window
        // wearing the right border.
        set(window, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND.0);
        // A Windows BOOL, which this attribute reads as four bytes rather than
        // one.
        set(window, DWMWA_USE_IMMERSIVE_DARK_MODE, 1i32);
        set(window, DWMWA_BORDER_COLOR, colorref(tokens::BORDER));
    }

    /// The window handle, if this is the platform it was compiled for.
    ///
    /// A mismatch is impossible in a normal build and not worth panicking over
    /// in an abnormal one, so it simply means no decoration happens.
    fn hwnd(handle: &dyn HasWindowHandle) -> Option<HWND> {
        match handle.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut _)),
            _ => None,
        }
    }

    /// Set one attribute, or say why not.
    ///
    /// Every one of these is a Windows 11 addition. On Windows 10 they return
    /// `E_INVALIDARG` and the window keeps the square corners it has always
    /// had, which is the correct appearance for that version of the system —
    /// so the failure is logged at debug, not as a warning.
    fn set<T: Copy>(window: HWND, attribute: DWMWINDOWATTRIBUTE, value: T) {
        let result = unsafe {
            DwmSetWindowAttribute(
                window,
                attribute,
                std::ptr::from_ref(&value).cast(),
                std::mem::size_of::<T>() as u32,
            )
        };
        if let Err(error) = result {
            tracing::debug!(
                attribute = attribute.0,
                %error,
                "the window manager would not take this frame attribute"
            );
        }
    }

    /// A design-system colour as the `0x00bbggrr` the desktop window manager
    /// wants.
    ///
    /// Note the byte order: this is not the `0xrrggbb` of every other colour
    /// literal in the world, and getting it backwards produces a border that is
    /// the right brightness and the wrong hue — a mistake that survives review
    /// because neutral greys look identical either way. The tokens are neutral,
    /// so a test pins the conversion on a colour that is not.
    fn colorref(color: iced::Color) -> COLORREF {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        COLORREF(channel(color.r) | channel(color.g) << 8 | channel(color.b) << 16)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_colour_is_handed_over_blue_first() {
            // Pure red, which the desktop window manager wants as 0x0000ff.
            let red = colorref(iced::Color::from_rgb(1.0, 0.0, 0.0));
            assert_eq!(red.0, 0x0000_00ff);

            let blue = colorref(iced::Color::from_rgb(0.0, 0.0, 1.0));
            assert_eq!(blue.0, 0x00ff_0000);
        }

        #[test]
        fn the_border_the_window_wears_is_the_border_the_panels_wear() {
            // Not a tautology: it fails the moment somebody hard-codes a colour
            // here instead of reaching for the token, which is exactly how a
            // window frame drifts away from the interface it contains.
            let expected = {
                let c = tokens::BORDER;
                let channel = |v: f32| (v * 255.0).round() as u32;
                channel(c.r) | channel(c.g) << 8 | channel(c.b) << 16
            };
            assert_eq!(colorref(tokens::BORDER).0, expected);
        }

        #[test]
        fn the_window_corner_is_the_corner_everything_else_uses() {
            // Windows 11 rounds at eight pixels and the design system's large
            // radius — the content frame's — is eight pixels. That agreement
            // is why the native corner is worth using rather than drawing one;
            // if the token ever moves, the two stop matching and this says so.
            assert_eq!(tokens::RADIUS_LG, 8.0);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use raw_window_handle::HasWindowHandle;

    /// Nothing to do.
    ///
    /// A Wayland compositor rounds and shadows windows to its own taste and
    /// offers no way to ask for something else, which is the correct
    /// arrangement — a desktop looks like itself, not like each application in
    /// turn. X11 has no compositor guarantee at all.
    pub fn dress(_: &dyn HasWindowHandle) {}
}
