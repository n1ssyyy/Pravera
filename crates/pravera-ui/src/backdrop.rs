//! Backdrop for dialogs: a live `backdrop-filter: blur(2px)` on the parent.
//!
//! The scrim is `crate::widget::backdrop::Backdrop`, a shader primitive that
//! samples the live framebuffer behind it and blurs it on the GPU every frame
//! — no snapshot, no CPU copy, nothing to go stale or misaligned. The two
//! earlier snapshot attempts (GDI `BitBlt`, then `window::screenshot` → CPU
//! Gaussian → `image`) both duplicated the UI because a copy always carries
//! its capture moment; that machinery is gone.

use iced::Element;

/// The dialog scrim at `strength`: 0 = frame passes through untouched, 1 =
/// fully frosted. The dialog's open/close animation drives it. Wrap it in
/// `mouse_area` at the call site to make clicking it dismiss the dialog.
pub fn view<'a, Message: 'a>(strength: f32) -> Element<'a, Message> {
    iced::widget::shader(crate::widget::backdrop::Backdrop::new(strength))
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
        .into()
}
