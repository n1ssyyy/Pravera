//! The icon set.
//!
//! Every icon is hand-authored SVG on a 16x16 grid with a 1.5px round-capped
//! stroke and no fills. Window controls are the one exception: 10x10 with a 1px
//! stroke, because platform chrome glyphs are hairline everywhere and a heavier
//! weight there reads as a toolbar rather than a title bar.
//!
//! ## Why not unicode glyphs
//!
//! The obvious cheap answer is `"\u{2715}"` and friends. They were tried and
//! they are bad: the glyph comes from whichever fallback font happens to have
//! that codepoint, so weight, optical size and baseline all vary between icons
//! in the same row, and none of it is controllable from here. Drawing them
//! means one stroke weight across the whole set and a shared baseline by
//! construction.
//!
//! ## Colour
//!
//! iced's SVG renderer replaces RGB while preserving alpha, so a symbolic icon
//! can be drawn once in white and tinted at the call site. That is also what
//! lets an icon animate its colour: [`stroked`] takes the tint as a plain
//! [`Color`], so an interpolated value works exactly like a constant one.

// The set is defined in full so screens reach for an existing icon instead of
// inventing one. Icons for screens that are still placeholders are not dead
// weight, they are the reason those screens will look consistent when built.
#![allow(dead_code)]

use iced::widget::svg;
use iced::{Color, ContentFit, Element, Length, Radians, Rotation};

/// Wraps a path in the shared 16x16 stroke envelope.
///
/// A macro rather than a function because the result has to be a `&'static str`
/// known at compile time: [`svg::Handle::from_memory`] keys its cache on a hash
/// of the bytes, so a constant produces one cache entry for the life of the
/// process while a runtime-formatted string would rasterise afresh every frame.
///
/// `literal` rather than `expr` because `concat!` needs real literal tokens; an
/// `expr` fragment arrives opaque and will not concatenate.
macro_rules! icon16 {
    ($body:literal) => {
        concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="#ffffff" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">"##,
            $body,
            "</svg>"
        )
    };
}

/// The same, at the lighter weight window chrome uses.
macro_rules! icon10 {
    ($body:literal) => {
        concat!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10" fill="none" stroke="#ffffff" stroke-width="1" stroke-linecap="round" stroke-linejoin="round">"##,
            $body,
            "</svg>"
        )
    };
}

// ---------------------------------------------------------------- navigation

/// A display with a stand. The Devices section.
pub const DEVICES: &str = icon16!(
    r#"<rect x="1.75" y="2.5" width="12.5" height="8.5" rx="1.5"/><path d="M5.5 14h5M8 11v3"/>"#
);

/// An arrow entering an enclosure: joining a machine that is not yet listed.
pub const CONNECT: &str = icon16!(
    r#"<path d="M9.5 2.75h2.25A1.75 1.75 0 0 1 13.5 4.5v7a1.75 1.75 0 0 1-1.75 1.75H9.5"/><path d="M6.75 5.5 9.25 8l-2.5 2.5"/><path d="M9.25 8H2.5"/>"#
);

/// Head and shoulders. Accounts and roles.
pub const USERS: &str =
    icon16!(r#"<circle cx="8" cy="5.25" r="2.75"/><path d="M2.75 13.75a5.25 5.25 0 0 1 10.5 0"/>"#);

/// Two arrows in opposite directions: bytes moving both ways.
pub const TRANSFERS: &str = icon16!(
    r#"<path d="M4.75 13.25V3.25"/><path d="M2.25 5.75 4.75 3.25l2.5 2.5"/><path d="M11.25 2.75v10"/><path d="M13.75 10.25l-2.5 2.5-2.5-2.5"/>"#
);

/// Sliders, not a gear. What lives behind this section is a set of continuous
/// choices (codec, bitrate, quality profile), and a gear would promise a
/// preferences dialog instead.
pub const SETTINGS: &str = icon16!(
    r#"<path d="M2.5 4.25h1.4M7.1 4.25h6.4"/><circle cx="5.5" cy="4.25" r="1.6"/><path d="M2.5 11.75h3.9M9.6 11.75h3.9"/><circle cx="8" cy="11.75" r="1.6"/>"#
);

/// A code window. The agent bus: a loopback MCP server that lets an AI drive
/// this machine. Angle brackets rather than a robot head, because the agent is
/// a program talking to programs, not a person with a face.
pub const AGENT: &str = icon16!(
    r#"<rect x="1.75" y="3" width="12.5" height="10" rx="1.5"/><path d="M6.5 8 4.5 10 6.5 12"/><path d="M9.5 8 11.5 10 9.5 12"/>"#
);
/// Alias for the SYSTEM nav's MCP entry.
pub const MCP: &str = AGENT;

/// Three tiles and a plus: OpenCode's home toggle. Everything that is not
/// an open tab.
pub const GRID: &str = icon16!(
    r#"<rect x="2" y="2" width="4.75" height="4.75" rx="1"/><rect x="9.25" y="2" width="4.75" height="4.75" rx="1"/><rect x="2" y="9.25" width="4.75" height="4.75" rx="1"/><path d="M11.625 9.5v4.25M9.5 11.625h4.25"/>"#
);

/// A window with its left column ruled off. Folds the sidebar to its rail.
pub const SIDEBAR: &str = icon16!(
    r#"<rect x="1.75" y="2.5" width="12.5" height="11" rx="1.5"/><path d="M6.25 2.5v11"/>"#
);

// -------------------------------------------------------------------- inline

/// Two links of a chain. A physical cable between two machines.
pub const CABLE: &str = icon16!(
    r#"<path d="M6.6 9.4a2.9 2.9 0 0 0 4.3.3l1.9-1.9a2.9 2.9 0 1 0-4.1-4.1l-1.1 1.1"/><path d="M9.4 6.6a2.9 2.9 0 0 0-4.3-.3L3.2 8.2a2.9 2.9 0 1 0 4.1 4.1l1.1-1.1"/>"#
);

/// A signal arc. Reachable over a network rather than a cable.
pub const WIRELESS: &str = icon16!(
    r#"<path d="M1.5 6.25a9.5 9.5 0 0 1 13 0"/><path d="M4.25 9a5.75 5.75 0 0 1 7.5 0"/><path d="M6.9 11.75a2 2 0 0 1 2.2 0"/><path d="M8 13.5h.01"/>"#
);

/// A path detouring through a third node. A relayed route.
pub const RELAY: &str = icon16!(
    r#"<circle cx="2.75" cy="11.5" r="1.5"/><circle cx="13.25" cy="11.5" r="1.5"/><circle cx="8" cy="3.5" r="1.5"/><path d="M3.85 10.4 6.9 4.65M9.1 4.65l3.05 5.75"/>"#
);

/// A circular arrow. Scan the network again.
pub const REFRESH: &str =
    icon16!(r#"<path d="M13.25 8a5.25 5.25 0 1 1-1.6-3.78"/><path d="M13.25 2.5v3h-3"/>"#);

/// A closed padlock. End to end encryption is in force.
pub const LOCK: &str = icon16!(
    r#"<rect x="3" y="7" width="10" height="6.5" rx="1.5"/><path d="M5.5 7V5.25a2.5 2.5 0 0 1 5 0V7"/>"#
);

/// A globe. Reachable from anywhere, over the open internet.
///
/// Three strokes and no more. A fuller globe with a second meridian and two
/// parallels was tried and turned to mush: at fifteen pixels the strokes
/// converging on the poles merge into a solid blob, which is a decision every
/// detailed icon has to make at this size.
pub const GLOBE: &str = icon16!(
    r#"<circle cx="8" cy="8" r="6.25"/><path d="M1.75 8h12.5"/><ellipse cx="8" cy="8" rx="3.1" ry="6.25"/>"#
);

/// A right-pointing chevron. Drill into a row.
pub const CHEVRON_RIGHT: &str = icon16!(r#"<path d="M6.25 3.75 10.5 8l-4.25 4.25"/>"#);

/// A warning triangle. Something is wrong and needs reading.
pub const ALERT: &str = icon16!(
    r#"<path d="M6.9 2.6 1.6 11.6a1.3 1.3 0 0 0 1.1 1.9h10.6a1.3 1.3 0 0 0 1.1-1.9L9.1 2.6a1.3 1.3 0 0 0-2.2 0Z"/><path d="M8 6.25v2.9"/><path d="M8 11.2h.01"/>"#
);

// ----------------------------------------------------------------- session

/// A keyboard. Whether key presses are going to the other machine.
///
/// Five keys and a space bar rather than a full row of them: at fifteen pixels
/// a realistic key count turns into a grey band, and the shape that reads as a
/// keyboard is the outline plus a rhythm, not the exact number of keys.
pub const KEYBOARD: &str = icon16!(
    r#"<rect x="1" y="3.75" width="14" height="8.5" rx="1.5"/><path d="M4.25 6.5h.01M7 6.5h.01M9.75 6.5h.01M12.5 6.5h.01M4.25 9h.01M12.5 9h.01"/><path d="M6.5 9.5h3"/>"#
);

/// The mark — your smooth blob, one filled shape.
///
/// 16×16, the same silhouette as `assets/logo.svg` at title-bar size. The path
/// is the blob you attached, scaled to the grid; no stroke, just the shape.
pub const LOGO: &str = icon16!(
    r#"<g transform="translate(8 8) scale(0.007) translate(-1020.375 -1023.409)"><path fill='#ffffff' stroke='none' transform="scale(2 2)" d='M493.468 226.706C497.887 226.418 502.318 226.338 506.745 226.469C521.527 226.923 536.178 229.376 550.302 233.76C593.986 246.899 613.19 269.402 642.929 300.964L686.298 346.554C666.1 357.107 642.213 367.346 621.334 377.386L457.727 455.545L408.808 479.18C398.945 483.851 384.637 490.032 375.628 495.421C379.911 500.491 386.007 506.526 390.751 511.26C426.545 546.977 461.367 583.877 496.516 620.185C497.185 606.525 496.937 592.311 496.943 578.583L496.969 515.017C512.279 506.27 530.705 497.238 546.417 488.956C578.811 471.686 611.34 454.673 644.004 437.919L697.217 410.235C707.04 405.101 720.675 397.582 730.678 393.341C754.099 421.04 779.601 437.735 793.324 473.024C798.104 485.316 800.877 498.297 801.535 511.469C803.331 543.038 794.197 572.57 773.044 596.321C771.745 597.761 770.416 599.172 769.055 600.554C750.543 619.061 725.397 630.33 698.954 630.133C657.744 629.826 639.636 603.24 613.764 576.624C601.065 563.56 588.33 550.182 575.632 537.194L575.752 652.127C575.711 671.3 576.848 701.451 573.15 719.173C569.971 734.629 563.114 749.092 553.159 761.335C518.704 803.942 458.277 810.394 415.713 775.963C412.707 773.421 410.028 770.472 407.28 767.668C399.391 759.621 391.467 751.608 383.442 743.697C373.448 733.846 363.748 723.697 353.649 713.952C333.443 694.55 313.449 674.929 293.671 655.092C285.019 646.393 275.305 637.29 267.076 628.338C241.165 600.147 225.927 565.814 222.871 527.477C219.675 483.578 233.904 440.187 262.476 406.705C270.746 397.056 280.617 386.986 289.403 377.664L330.899 333.38L364.729 296.998C383.842 276.44 398.961 259.791 424.156 246.502C447.17 234.365 467.642 228.685 493.468 226.706Z'/></g>"#
);

/// A game pad. Gaming mode: fullscreen, with the pointer held inside the
/// window.
///
/// A body, a d-pad drawn as a cross, and two action buttons. At this size the
/// cross and the pair of dots are what carry the reading; a more detailed pad
/// would be grey noise.
pub const GAMEPAD: &str = icon16!(
    r#"<rect x="1.75" y="4.5" width="12.5" height="7" rx="3.5"/><path d="M5.5 6.5v3M4 8h3"/><circle cx="10.25" cy="7.25" r="0.75"/><circle cx="12" cy="9" r="0.75"/>"#
);

/// A cone with two waves leaving it. The host's sound is playing.
pub const SPEAKER: &str = icon16!(
    r#"<path d="M7.5 2.75 4 5.75H1.75v4.5H4l3.5 3Z"/><path d="M10.25 6a2.75 2.75 0 0 1 0 4"/><path d="M12.5 3.75a6 6 0 0 1 0 8.5"/>"#
);

/// A dial with a needle. The measurement panel.
pub const GAUGE: &str =
    icon16!(r#"<path d="M2 12.25a7 7 0 1 1 12 0"/><path d="M8 12.25 11 7.5"/>"#);

/// A plug pulled out of its socket. End the session.
pub const DISCONNECT: &str = icon16!(
    r#"<path d="M6.25 1.75v3M9.75 1.75v3"/><path d="M4.5 4.75h7v2.5a3.5 3.5 0 0 1-3.5 3.5 3.5 3.5 0 0 1-3.5-3.5Z"/><path d="M8 10.75v3.5"/><path d="M2 2 14 14"/>"#
);

/// A chevron pointing down. Pull the toolbar back into view.
pub const CHEVRON_DOWN: &str = icon16!(r#"<path d="M3.75 6.25 8 10.5l4.25-4.25"/>"#);

/// A plus. Add a device nothing has discovered.
pub const ADD: &str = icon16!(r#"<path d="M8 2.75v10.5M2.75 8h10.5"/>"#);

/// A command prompt. A shell on the far machine.
pub const TERMINAL: &str = icon16!(
    r#"<rect x="1.75" y="2.75" width="12.5" height="10.5" rx="1.5"/><path d="M4.25 6 6.5 8l-2.25 2M8.25 10.25h3.5"/>"#
);

/// Three dots. More to do with the thing beside it.
pub const MORE: &str = icon16!(r#"<path d="M3.5 8h.01M8 8h.01M12.5 8h.01"/>"#);

/// A broken link. Forget a machine: the record goes, discovery can find it again.
pub const FORGET: &str = icon16!(
    r#"<path d="M6.5 9.5 9.5 6.5"/><path d="M4.75 7.25 3.4 8.6a2.75 2.75 0 0 0 3.9 3.9l1.35-1.35"/><path d="M11.25 8.75l1.35-1.35a2.75 2.75 0 0 0-3.9-3.9L7.35 4.85"/>"#
);

/// A lightning bolt. Something is being done by dedicated silicon rather than
/// by the processor: the hardware encoder, and a direct route.
pub const BOLT: &str = icon16!(r#"<path d="M9 1.5 3.5 9h4L7 14.5 12.5 7h-4Z"/>"#);

/// A shield. Capabilities and what grants them.
pub const SHIELD: &str = icon16!(
    r#"<path d="M8 1.5 13.5 3.75v4C13.5 11 11.1 13.7 8 14.5 4.9 13.7 2.5 11 2.5 7.75v-4Z"/>"#
);

/// A shield with a tick. A capability that is held.
pub const SHIELD_CHECK: &str = icon16!(
    r#"<path d="M8 1.5 13.5 3.75v4C13.5 11 11.1 13.7 8 14.5 4.9 13.7 2.5 11 2.5 7.75v-4Z"/><path d="M5.75 7.75 7.25 9.25l3-3"/>"#
);

/// A plus. Add something.
pub const PLUS: &str = icon16!(r#"<path d="M8 3.25v9.5M3.25 8h9.5"/>"#);

/// A waste basket. Remove something permanently.
pub const TRASH: &str = icon16!(
    r#"<path d="M2.75 4.25h10.5"/><path d="M6.25 4.25V2.75h3.5v1.5"/><path d="M4.25 4.25l.6 8.25a1 1 0 0 0 1 .75h4.3a1 1 0 0 0 1-.75l.6-8.25"/>"#
);

/// A circle with a line through it. Switched off but still there.
pub const DISABLED: &str = icon16!(r#"<circle cx="8" cy="8" r="5.75"/><path d="M4 12 12 4"/>"#);

/// A single person. One account.
pub const USER: &str = icon16!(
    r#"<circle cx="8" cy="5.25" r="2.75"/><path d="M2.75 14c0-2.9 2.35-4.5 5.25-4.5s5.25 1.6 5.25 4.5"/>"#
);

// ----------------------------------------------------------------- files

/// A folder. Something to go into rather than to move.
pub const FOLDER: &str = icon16!(
    r#"<path d="M1.75 4.25A1.25 1.25 0 0 1 3 3h3l1.5 1.75h4.5A1.25 1.25 0 0 1 13.25 6v6a1.25 1.25 0 0 1-1.25 1.25H3A1.25 1.25 0 0 1 1.75 12Z"/>"#
);

/// A page with a folded corner. One file.
pub const FILE: &str = icon16!(
    r#"<path d="M3.25 2.75A1.25 1.25 0 0 1 4.5 1.5h4L12.75 5.75v7.5A1.25 1.25 0 0 1 11.5 14.5h-7a1.25 1.25 0 0 1-1.25-1.25Z"/><path d="M8.5 1.5v3.25a1 1 0 0 0 1 1h3.25"/>"#
);

/// A page whose corner is gone: something that is neither file nor folder.
pub const FILE_OTHER: &str = icon16!(
    r#"<path d="M3.25 2.75A1.25 1.25 0 0 1 4.5 1.5h4L12.75 5.75v7.5A1.25 1.25 0 0 1 11.5 14.5h-7a1.25 1.25 0 0 1-1.25-1.25Z"/><path d="M6 9.5h4"/>"#
);

/// A corner turning back and up. The directory above this one.
pub const LEVEL_UP: &str = icon16!(
    r#"<path d="M3.25 12.75V7a2.25 2.25 0 0 1 2.25-2.25h7.25"/><path d="M10 1.75 13.25 4.75 10 7.75"/>"#
);

/// An arrow into a tray. Bring a copy here.
pub const DOWNLOAD: &str = icon16!(
    r#"<path d="M8 1.75v8.5"/><path d="M4.75 7.25 8 10.5l3.25-3.25"/><path d="M2.5 13.25h11"/>"#
);

/// An arrow out of a tray. Put a copy over there.
pub const UPLOAD: &str = icon16!(
    r#"<path d="M8 10.25v-8.5"/><path d="M4.75 5 8 1.75 11.25 5"/><path d="M2.5 13.25h11"/>"#
);

/// A tick. Finished, and it worked.
pub const CHECK: &str = icon16!(r#"<path d="M3.25 8.5 6.5 11.75l6.25-7"/>"#);

/// Two overlapping sheets. Put this on the clipboard.
pub const COPY: &str = icon16!(
    r#"<rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 3.25A1.25 1.25 0 0 0 9.25 2.5h-5.5A1.25 1.25 0 0 0 2.5 3.75v5.5a1.25 1.25 0 0 0 .75 1.15"/>"#
);

/// A cross at icon weight. Stop this, or dismiss it.
pub const CLOSE: &str = icon16!(r#"<path d="M4 4l8 8M12 4l-8 8"/>"#);

// ------------------------------------------------------------ window chrome

/// A single rule across the middle.
pub const WIN_MINIMIZE: &str = icon10!(r#"<path d="M0.5 5h9"/>"#);

/// One square: the window will fill the screen.
pub const WIN_MAXIMIZE: &str = icon10!(r#"<rect x="0.5" y="0.5" width="9" height="9" rx="1.25"/>"#);

/// Two offset squares: the window will shrink back off the screen edge.
pub const WIN_RESTORE: &str = icon10!(
    r#"<path d="M2.75 2.75V1.75A1.25 1.25 0 0 1 4 0.5h4.25A1.25 1.25 0 0 1 9.5 1.75V6a1.25 1.25 0 0 1-1.25 1.25h-1"/><rect x="0.5" y="2.75" width="6.75" height="6.75" rx="1.25"/>"#
);

/// A cross.
pub const WIN_CLOSE: &str = icon10!(r#"<path d="M0.9 0.9 9.1 9.1M9.1 0.9 0.9 9.1"/>"#);

// ------------------------------------------------------------------ rendering

/// Renders one icon at `size`, tinted.
///
/// `size` is both width and height: every icon in the set is square, and
/// letting a caller stretch one would break the shared stroke weight that makes
/// the set look like a set.
pub fn stroked<'a, Message: 'a>(
    source: &'static str,
    size: f32,
    tint: Color,
) -> Element<'a, Message> {
    turned(source, size, tint, 0.0)
}

/// The same, rotated by `turns` (1.0 is a full revolution).
///
/// Rotation is applied by the renderer as a transform rather than baked into
/// the raster, so an icon can spin continuously without re-rasterising. The
/// rotation floats, meaning layout is computed from the unrotated bounds and a
/// spinning icon never nudges its neighbours.
///
/// ## Alpha does not travel through the colour filter
///
/// The renderer's tint replaces the red, green and blue channels of each pixel
/// and leaves alpha exactly as the rasteriser produced it. That is what keeps
/// the stroke's antialiasing intact, but it also means the alpha of `tint` is
/// discarded: passing a fully transparent colour paints a fully opaque icon.
///
/// So the alpha is lifted off `tint` and applied through the widget's own
/// opacity instead, and only opaque colour reaches the filter. Callers can then
/// fade an icon by fading its colour, exactly as they do with text.
pub fn turned<'a, Message: 'a>(
    source: &'static str,
    size: f32,
    tint: Color,
    turns: f32,
) -> Element<'a, Message> {
    let opacity = tint.a.clamp(0.0, 1.0);
    let solid = Color { a: 1.0, ..tint };

    svg(svg::Handle::from_memory(source.as_bytes()))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .content_fit(ContentFit::Contain)
        .rotation(Rotation::Floating(Radians(turns * std::f32::consts::TAU)))
        .opacity(opacity)
        .style(move |_, _| svg::Style { color: Some(solid) })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 16-grid set, so structural tests can sweep all of it.
    const ALL_16: &[(&str, &str)] = &[
        ("DEVICES", DEVICES),
        ("CONNECT", CONNECT),
        ("USERS", USERS),
        ("TRANSFERS", TRANSFERS),
        ("SETTINGS", SETTINGS),
        ("AGENT", AGENT),
        ("CABLE", CABLE),
        ("WIRELESS", WIRELESS),
        ("RELAY", RELAY),
        ("REFRESH", REFRESH),
        ("LOCK", LOCK),
        ("GLOBE", GLOBE),
        ("CHEVRON_RIGHT", CHEVRON_RIGHT),
        ("ALERT", ALERT),
        ("KEYBOARD", KEYBOARD),
        ("GAUGE", GAUGE),
        ("DISCONNECT", DISCONNECT),
        ("CHEVRON_DOWN", CHEVRON_DOWN),
        ("FOLDER", FOLDER),
        ("FILE", FILE),
        ("FILE_OTHER", FILE_OTHER),
        ("LEVEL_UP", LEVEL_UP),
        ("DOWNLOAD", DOWNLOAD),
        ("UPLOAD", UPLOAD),
        ("CHECK", CHECK),
        ("COPY", COPY),
        ("CLOSE", CLOSE),
        ("GRID", GRID),
        ("SIDEBAR", SIDEBAR),
        ("TERMINAL", TERMINAL),
        ("ADD", ADD),
        ("FORGET", FORGET),
        ("GAMEPAD", GAMEPAD),
        ("SPEAKER", SPEAKER),
        ("BOLT", BOLT),
    ];

    const ALL_10: [(&str, &str); 4] = [
        ("WIN_MINIMIZE", WIN_MINIMIZE),
        ("WIN_MAXIMIZE", WIN_MAXIMIZE),
        ("WIN_RESTORE", WIN_RESTORE),
        ("WIN_CLOSE", WIN_CLOSE),
    ];

    #[test]
    fn every_icon_is_a_complete_svg_document() {
        for (name, source) in ALL_16.iter().chain(ALL_10.iter()) {
            assert!(source.starts_with("<svg"), "{name} does not open with <svg");
            assert!(source.ends_with("</svg>"), "{name} is not closed");
            assert!(
                source.contains("xmlns="),
                "{name} has no namespace, resvg will reject it"
            );
        }
    }

    #[test]
    fn the_sixteen_grid_shares_one_stroke_weight() {
        for (name, source) in ALL_16.iter().copied() {
            assert!(
                source.contains(r#"viewBox="0 0 16 16""#),
                "{name} is off-grid"
            );
            assert!(
                source.contains(r#"stroke-width="1.5""#),
                "{name} breaks the stroke weight"
            );
        }
    }

    #[test]
    fn window_chrome_is_lighter_than_the_body_set() {
        for (name, source) in ALL_10 {
            assert!(
                source.contains(r#"viewBox="0 0 10 10""#),
                "{name} is off-grid"
            );
            assert!(
                source.contains(r#"stroke-width="1""#),
                "{name} is too heavy for chrome"
            );
        }
    }

    /// The tint filter overwrites RGB and keeps alpha, so a stroke icon tints
    /// cleanly. One that leaned on a partly transparent fill would silently
    /// lose its shading instead. Keeping the set fill-free means every icon
    /// tints identically.
    #[test]
    fn stroke_icons_declare_no_fill() {
        for (name, source) in ALL_16.iter().chain(ALL_10.iter()) {
            assert!(source.contains(r#"fill="none""#), "{name} must not fill");
        }
    }

    #[test]
    fn a_handle_built_from_the_same_icon_twice_is_the_same_handle() {
        // iced caches rasterised SVGs by a hash of the bytes. If this ever
        // stopped holding, every icon would re-rasterise on every frame.
        let a = svg::Handle::from_memory(DEVICES.as_bytes());
        let b = svg::Handle::from_memory(DEVICES.as_bytes());
        assert_eq!(a.id(), b.id());
        assert_ne!(a.id(), svg::Handle::from_memory(USERS.as_bytes()).id());
    }
}
