//! The single source of truth for every colour, dimension and type size in
//! Pravera.
//!
//! No literal colour or magic pixel value may appear anywhere else in the UI.
//! Everything is expressed as a token here, so the whole app can be retuned
//! from one file and stays visually coherent by construction.
//!
//! ## Lineage
//!
//! The language is DigiClip's: a near-monochrome "dark room" of five greys,
//! JetBrains Mono at 13px for every word, 1px bevelled edges instead of drop
//! shadows, a 5px structural rhythm, and colour rationed to status. The shell
//! follows OpenCode's desktop app: 28px list rows with 6px corners, a quiet
//! section label over each group, hover as a one-step tone lift that lands in
//! about 120ms, and open work shown as tabs in the title bar.

// The design system defines its full token set and style vocabulary up front so
// that later screens select an existing token rather than inventing a value.
// Tokens not yet consumed are intentional, not oversights.
#![allow(dead_code)]

use iced::font::{Family, Weight};
use iced::{Color, Font};

/// Builds a `Color` from 8-bit channels at compile time.
const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    }
}

/// Same colour with an explicit alpha, for overlays and scrims.
pub const fn with_alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

// ---------------------------------------------------------------------------
// Raw scale
// ---------------------------------------------------------------------------
//
// Not Tailwind's `neutral` any more, although it shares the upper half. The
// dark end sits a step below DigiClip's: the floor and the panels are nearly
// black, so the bevelled edges — which keep DigiClip's exact greys — carry
// the structure, and text lands on a surface dark enough to read crisply.
// The steps between #0a0a0a and #292929 are where a dark interface actually
// lives, so that is where the scale is densest.

/// Window, title bar, sidebar. The floor everything else sits on.
pub const NEUTRAL_950: Color = rgb(0x0a, 0x0a, 0x0a);
/// Panels: cards, inputs, dialogs, menus.
pub const NEUTRAL_900: Color = rgb(0x0f, 0x0f, 0x0f);
/// Rows and tiles inside a panel; toasts and tooltips.
pub const NEUTRAL_850: Color = rgb(0x16, 0x16, 0x16);
/// The selected entry. Between a row and a hover, so the page you are on
/// reads as held rather than pointed at.
pub const NEUTRAL_825: Color = rgb(0x1b, 0x1b, 0x1b);
/// Hover, and every filled secondary surface.
pub const NEUTRAL_800: Color = rgb(0x22, 0x22, 0x22);
/// Hairlines.
pub const NEUTRAL_750: Color = rgb(0x29, 0x29, 0x29);
/// Pressed.
pub const NEUTRAL_700: Color = rgb(0x33, 0x33, 0x33);
pub const NEUTRAL_600: Color = rgb(0x52, 0x52, 0x52);
pub const NEUTRAL_500: Color = rgb(0x7c, 0x7c, 0x7c);
pub const NEUTRAL_400: Color = rgb(0xa8, 0xa8, 0xa8);
pub const NEUTRAL_300: Color = rgb(0xd4, 0xd4, 0xd4);
pub const NEUTRAL_200: Color = rgb(0xe5, 0xe5, 0xe5);
pub const NEUTRAL_100: Color = rgb(0xf2, 0xf2, 0xf2);
pub const NEUTRAL_50: Color = rgb(0xfa, 0xfa, 0xfa);

// ---------------------------------------------------------------------------
// Semantic roles
// ---------------------------------------------------------------------------

/// The page itself: window, title bar and sidebar are one continuous floor.
pub const BACKGROUND: Color = NEUTRAL_950;
/// Raised surfaces: cards, panels, settings groups.
pub const CARD: Color = NEUTRAL_900;
/// A row or tile inside a card.
pub const ROW: Color = NEUTRAL_850;
/// Floating surfaces: menus and dialogs. The same tone as a card; what lifts
/// them is the bevel and the scrim, not a lighter fill.
pub const POPOVER: Color = NEUTRAL_900;
/// Toasts and tooltips, one step up from a popover because they float over
/// panels rather than over the scrim.
pub const TOAST: Color = NEUTRAL_850;
/// Primary body text.
pub const FOREGROUND: Color = NEUTRAL_100;
/// Secondary text: labels, captions, metadata.
pub const MUTED_FOREGROUND: Color = NEUTRAL_400;
/// Text that must recede further still: placeholders, disabled labels.
pub const SUBTLE_FOREGROUND: Color = NEUTRAL_500;
/// Hairlines between surfaces.
pub const BORDER: Color = NEUTRAL_750;
/// Border of an input at rest.
pub const INPUT: Color = NEUTRAL_750;
/// Filled secondary surfaces and every hover.
pub const SECONDARY: Color = NEUTRAL_800;
/// Pressed tint for a secondary surface.
pub const ACCENT: Color = NEUTRAL_700;
/// The sidebar entry for the page on screen.
pub const SELECTED: Color = NEUTRAL_825;
/// Focus ring.
pub const RING: Color = NEUTRAL_300;

/// Primary action: a near-white fill with near-black text, so the single most
/// important control on a screen is unmistakable without a brand hue.
pub const PRIMARY: Color = NEUTRAL_50;
pub const PRIMARY_HOVER: Color = NEUTRAL_200;
pub const PRIMARY_FOREGROUND: Color = rgb(0x17, 0x17, 0x17);

/// DigiClip's lime. The one brand colour, and rationed: "ready", "online",
/// "live". Anywhere else it would stop meaning anything.
pub const LIME: Color = rgb(0xa3, 0xe6, 0x35);
pub const LIME_FOREGROUND: Color = rgb(0x0a, 0x0a, 0x0a);

/// Status colours. The only other chromatic values in the system, reserved so
/// that colour always carries meaning rather than decoration.
pub const SUCCESS: Color = rgb(0x00, 0xbc, 0x7d);
pub const SUCCESS_SOFT: Color = rgb(0x0f, 0x2b, 0x22);
pub const WARNING: Color = rgb(0xff, 0x8a, 0x1f);
pub const WARNING_SOFT: Color = rgb(0x2e, 0x1f, 0x0f);
pub const DESTRUCTIVE: Color = rgb(0xcf, 0x30, 0x30);
pub const DESTRUCTIVE_HOVER: Color = rgb(0xbc, 0x2d, 0x2d);
pub const DESTRUCTIVE_SOFT: Color = rgb(0x27, 0x17, 0x17);
/// Destructive as text on a dark surface: the fill red is too dark to read.
pub const DESTRUCTIVE_TEXT: Color = rgb(0xf1, 0x74, 0x71);
pub const DESTRUCTIVE_FOREGROUND: Color = NEUTRAL_50;
/// Informational, for the rare notice that is neither good nor bad news.
pub const INFO: Color = rgb(0x76, 0x98, 0xfd);

/// Route-quality accents, used by the connection badges.
pub const ROUTE_DIRECT: Color = SUCCESS;
pub const ROUTE_RELAY: Color = WARNING;
pub const ROUTE_OFFLINE: Color = NEUTRAL_600;

/// The black round a remote picture: the bars of a picture that is not the
/// window's shape read as the edge of that screen, not as part of this
/// interface, so they are not the app's background.
pub const LETTERBOX: Color = rgb(0, 0, 0);

/// The colour every shadow is cast in.
pub const SHADOW_INK: Color = rgb(0, 0, 0);

/// The scrim behind a dialog.
pub const SCRIM: Color = with_alpha(rgb(0, 0, 0), 0.6);

// ---------------------------------------------------------------------------
// Bevel — the signature edge
// ---------------------------------------------------------------------------
//
// A 1px border whose top is lighter than its sides and whose bottom is nearly
// black. It reads as a panel catching light from above, which is how a dark
// interface gets hierarchy without drop shadows that vanish on a black floor.

/// Per-edge colours of a bevel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bevel {
    pub top: Color,
    pub sides: Color,
    pub bottom: Color,
}

/// Cards and panels.
pub const BEVEL_CARD: Bevel = Bevel {
    top: rgb(0x37, 0x37, 0x37),
    sides: rgb(0x26, 0x26, 0x26),
    bottom: rgb(0x0a, 0x0a, 0x0a),
};
/// Inputs, dialogs, rows, tiles and segmented controls: one notch crisper.
pub const BEVEL_RAISED: Bevel = Bevel {
    top: rgb(0x43, 0x43, 0x43),
    sides: rgb(0x2b, 0x2b, 0x2b),
    bottom: rgb(0x08, 0x08, 0x08),
};
/// A panel under the pointer.
pub const BEVEL_HOVER: Bevel = Bevel {
    top: rgb(0x4f, 0x4f, 0x4f),
    sides: rgb(0x32, 0x32, 0x32),
    bottom: rgb(0x0a, 0x0a, 0x0a),
};

// ---------------------------------------------------------------------------
// Spacing — a 4px grid, plus DigiClip's 5px structural gap
// ---------------------------------------------------------------------------

pub const SPACE_HALF: f32 = 2.0;
pub const SPACE_1: f32 = 4.0;
/// Between panels, and between the content area and the window edge. Five,
/// not four: the odd step is what makes the panels read as separate objects
/// rather than as one surface with lines drawn on it.
pub const GAP: f32 = 5.0;
pub const SPACE_1_5: f32 = 6.0;
pub const SPACE_2: f32 = 8.0;
pub const SPACE_3: f32 = 12.0;
pub const SPACE_4: f32 = 16.0;
pub const SPACE_5: f32 = 20.0;
pub const SPACE_6: f32 = 24.0;
pub const SPACE_8: f32 = 32.0;
pub const SPACE_10: f32 = 40.0;
pub const SPACE_12: f32 = 48.0;
pub const SPACE_16: f32 = 64.0;

// ---------------------------------------------------------------------------
// Radius
// ---------------------------------------------------------------------------

/// Badges, menu rows, keycaps.
pub const RADIUS_SM: f32 = 4.0;
/// Buttons, inputs, cards, dialogs, menus, list rows.
pub const RADIUS: f32 = 6.0;
/// Settings groups and the content frame.
pub const RADIUS_LG: f32 = 8.0;
pub const RADIUS_XL: f32 = 10.0;
/// Pills, switches and dots.
pub const RADIUS_FULL: f32 = 9999.0;

// ---------------------------------------------------------------------------
// Shell dimensions
// ---------------------------------------------------------------------------

/// Title bar height: DigiClip's, and the same as the sidebar rail is wide, so
/// the top-left corner is a square.
pub const TITLEBAR_HEIGHT: f32 = 42.0;
/// The sidebar: an icon rail exactly as wide as the title bar is tall.
pub const SIDEBAR_RAIL: f32 = TITLEBAR_HEIGHT;
/// One rail entry at rest: a rounded square, [`GAP`] in from the window edge.
pub const RAIL_ITEM: f32 = 32.0;
/// A rail entry under the pointer, grown rightwards over the page so its
/// label can be read. DigiClip's `w-40`.
pub const RAIL_PILL: f32 = 160.0;
/// One tab, one menu entry, one dense list row: OpenCode's 28.
pub const ROW_HEIGHT: f32 = 28.0;
/// A list row that carries two facts side by side.
pub const LIST_ROW: f32 = 36.0;
/// A control: button, input, select.
pub const CONTROL_HEIGHT: f32 = 32.0;
/// A compact control inside a dense row.
pub const CONTROL_HEIGHT_SM: f32 = 28.0;
/// A page's header, the top region of its sheet: the same on every page, and
/// tall enough for a title at [`TEXT_LG`] with a control on either side of it.
pub const HEADER_HEIGHT: f32 = 56.0;
/// The widest a column of reading text is allowed to run.
pub const READING_WIDTH: f32 = 760.0;
/// Standard icon size.
pub const ICON: f32 = 16.0;
pub const ICON_SM: f32 = 14.0;

// ---------------------------------------------------------------------------
// Type scale
// ---------------------------------------------------------------------------

/// Uppercase section labels only.
pub const TEXT_2XS: f32 = 10.0;
/// Hints, badges, small buttons, metadata.
pub const TEXT_XS: f32 = 11.0;
/// Body. The size of almost every word in the app.
pub const TEXT_SM: f32 = 13.0;
/// A step up, for a row's primary line when it has to lead a dense block.
pub const TEXT_BASE: f32 = 14.0;
/// Stat figures and dialog titles.
pub const TEXT_LG: f32 = 16.0;
/// Page titles.
pub const TEXT_XL: f32 = 20.0;
/// Hero figures.
pub const TEXT_2XL: f32 = 26.0;

/// The one face. JetBrains Mono, embedded so the interface looks the same on
/// every machine; see [`FONT_BYTES`].
pub const FAMILY: &str = "JetBrains Mono";

const fn face(weight: Weight) -> Font {
    Font {
        family: Family::Name(FAMILY),
        weight,
        ..Font::DEFAULT
    }
}

/// Body text.
pub const FONT_UI: Font = face(Weight::Normal);
/// Labels and buttons.
pub const FONT_UI_MEDIUM: Font = face(Weight::Medium);
/// Titles, the wordmark, figures that head a group.
///
/// The embedded face is variable, so every weight on its axis exists and none
/// of these can silently resolve to another family — the failure that made a
/// medium weight render as a serif when the interface leaned on Segoe UI.
pub const FONT_UI_STRONG: Font = face(Weight::Semibold);

/// Machine strings: addresses, device IDs, millisecond figures. The same face
/// as everything else now that everything is mono; kept as its own token so
/// the call sites still say what the text *is*.
pub const FONT_MONO: Font = FONT_UI;
pub const FONT_MONO_STRONG: Font = FONT_UI_MEDIUM;

/// The font file itself. Registered with iced at startup and made the default,
/// so a widget that forgets to set a font still lands on the right one.
///
/// JetBrains Mono is © The JetBrains Mono Project Authors, under the SIL Open
/// Font License 1.1; the licence text ships beside it in `assets/fonts`.
pub const FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Variable.ttf");

/// The weights the interface asks for above regular, each of which needs a
/// face of its own; see [`font_at`].
pub const EXTRA_WEIGHTS: [u16; 2] = [500, 600];

/// The same font file again, claiming a different weight.
///
/// The file is variable, so it registers once, at its default weight of 400.
/// cosmic-text only takes the requested family's own face when its registered
/// weight is exactly the one asked for; otherwise it tries the fallback list
/// first, and on Windows that is Segoe UI — which has a real Semibold. So every
/// `FONT_UI_STRONG` title came out in Segoe UI beside mono body text. The copy
/// registered here has its OS/2 weight class rewritten, which gives each weight
/// an exact match in its own family; the glyphs still come from the `wght`
/// axis at render time.
pub fn font_at(weight: u16) -> Vec<u8> {
    let mut bytes = FONT_BYTES.to_vec();
    if let Some(at) = table(&bytes, b"OS/2").map(|os2| os2 + 4) {
        if let Some(field) = bytes.get_mut(at..at + 2) {
            field.copy_from_slice(&weight.to_be_bytes());
        }
    }
    bytes
}

/// Where a table starts in a TrueType file, read off its table directory.
fn table(font: &[u8], tag: &[u8; 4]) -> Option<usize> {
    let count = u16::from_be_bytes(font.get(4..6)?.try_into().ok()?) as usize;
    (0..count).map(|index| 12 + 16 * index).find_map(|record| {
        (font.get(record..record + 4)? == tag)
            .then(|| font.get(record + 8..record + 12))
            .flatten()
            .map(|offset| u32::from_be_bytes(offset.try_into().unwrap()) as usize)
    })
}

/// Formerly letterspaced a label with thin spaces. A monospaced face already
/// sets uppercase evenly, and JetBrains Mono has no thin space to borrow: the
/// fallback font that would supply it is exactly what once turned "PRAVERA"
/// into "PĜRĜAĜVĜEĜRĜA". So this now only upper-cases.
pub fn tracked(label: &str) -> String {
    label.to_uppercase()
}

// ---------------------------------------------------------------------------
// Borders
// ---------------------------------------------------------------------------

/// Hairline width.
pub const BORDER_WIDTH: f32 = 1.0;
/// Focus ring width.
pub const RING_WIDTH: f32 = 2.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded face is variable on `wght`, so any weight is real. What
    /// must never happen again is a token pointing at a family the app does
    /// not ship, which is how a medium weight once became a serif.
    #[test]
    fn every_font_token_names_the_embedded_family() {
        for (name, font) in [
            ("FONT_UI", FONT_UI),
            ("FONT_UI_MEDIUM", FONT_UI_MEDIUM),
            ("FONT_UI_STRONG", FONT_UI_STRONG),
            ("FONT_MONO", FONT_MONO),
            ("FONT_MONO_STRONG", FONT_MONO_STRONG),
        ] {
            assert_eq!(font.family, Family::Name(FAMILY), "{name} left the embedded face");
        }
    }

    #[test]
    fn the_embedded_font_is_a_real_truetype_file() {
        // `true` (0x00010000) is the TrueType signature; anything else means
        // the asset was swapped for a format iced cannot load, such as woff2.
        assert_eq!(&FONT_BYTES[..4], &[0x00, 0x01, 0x00, 0x00]);
        assert!(FONT_BYTES.len() > 100_000, "the font asset looks truncated");
    }

    /// What the OS/2 table says a copy of the font weighs.
    fn weight_class(font: &[u8]) -> u16 {
        let at = table(font, b"OS/2").expect("no OS/2 table") + 4;
        u16::from_be_bytes([font[at], font[at + 1]])
    }

    #[test]
    fn every_weight_the_interface_uses_has_a_face_that_claims_it() {
        // A weight with no exact face in the family goes to the fallback
        // list, which is how Semibold titles ended up in Segoe UI.
        assert_eq!(weight_class(FONT_BYTES), 400);
        for weight in EXTRA_WEIGHTS {
            let copy = font_at(weight);
            assert_eq!(weight_class(&copy), weight);
            assert_eq!(copy.len(), FONT_BYTES.len(), "only the weight class may change");
        }
        let asked = |font: Font| match font.weight {
            Weight::Normal => 400,
            Weight::Medium => 500,
            Weight::Semibold => 600,
            other => panic!("{other:?} has no face registered"),
        };
        for font in [FONT_UI_MEDIUM, FONT_UI_STRONG] {
            assert!(EXTRA_WEIGHTS.contains(&asked(font)));
        }
    }

    #[test]
    fn emphasis_is_actually_heavier_than_body() {
        assert!(FONT_UI_MEDIUM.weight != FONT_UI.weight);
        assert!(FONT_UI_STRONG.weight != FONT_UI_MEDIUM.weight);
    }

    #[test]
    fn tracking_only_upper_cases() {
        assert_eq!(tracked("devices"), "DEVICES");
        assert_eq!(tracked(""), "");
    }

    #[test]
    fn the_neutral_scale_runs_monotonically_from_dark_to_light() {
        let scale = [
            NEUTRAL_950,
            NEUTRAL_900,
            NEUTRAL_850,
            NEUTRAL_825,
            NEUTRAL_800,
            NEUTRAL_750,
            NEUTRAL_700,
            NEUTRAL_600,
            NEUTRAL_500,
            NEUTRAL_400,
            NEUTRAL_300,
            NEUTRAL_200,
            NEUTRAL_100,
            NEUTRAL_50,
        ];
        for pair in scale.windows(2) {
            assert!(
                pair[1].r > pair[0].r,
                "the scale must brighten monotonically, so a step never inverts contrast"
            );
        }
    }

    #[test]
    fn rgb_maps_the_endpoints_exactly() {
        assert_eq!(
            rgb(0, 0, 0),
            Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0
            }
        );
        assert_eq!(
            rgb(255, 255, 255),
            Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0
            }
        );
    }

    /// Relative luminance per WCAG 2.1.
    fn luminance(c: Color) -> f32 {
        fn channel(v: f32) -> f32 {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (l1, l2) = (luminance(a), luminance(b));
        let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn body_text_clears_wcag_aa_on_every_surface() {
        for (surface, name) in [
            (BACKGROUND, "background"),
            (CARD, "card"),
            (ROW, "row"),
            (POPOVER, "popover"),
            (SECONDARY, "hover"),
        ] {
            let ratio = contrast(FOREGROUND, surface);
            assert!(
                ratio >= 4.5,
                "{name}: contrast {ratio:.2} is below the 4.5:1 AA threshold"
            );
        }
    }

    #[test]
    fn muted_text_clears_wcag_aa_for_normal_text() {
        for surface in [BACKGROUND, CARD, ROW] {
            let ratio = contrast(MUTED_FOREGROUND, surface);
            assert!(ratio >= 4.5, "muted foreground contrast {ratio:.2} is below 4.5:1");
        }
    }

    #[test]
    fn primary_buttons_are_legible() {
        let ratio = contrast(PRIMARY_FOREGROUND, PRIMARY);
        assert!(ratio >= 4.5, "primary button contrast {ratio:.2} is below 4.5:1");
    }

    #[test]
    fn lime_pills_are_legible() {
        let ratio = contrast(LIME_FOREGROUND, LIME);
        assert!(ratio >= 4.5, "lime pill contrast {ratio:.2} is below 4.5:1");
    }

    #[test]
    fn destructive_text_is_legible_on_its_own_fill() {
        let ratio = contrast(DESTRUCTIVE_FOREGROUND, DESTRUCTIVE);
        assert!(ratio >= 3.0, "destructive contrast {ratio:.2} is too low for a button label");
    }

    #[test]
    fn destructive_text_reads_on_a_card() {
        let ratio = contrast(DESTRUCTIVE_TEXT, CARD);
        assert!(ratio >= 4.5, "destructive text contrast {ratio:.2} is below 4.5:1");
    }

    #[test]
    fn borders_are_visible_against_the_surfaces_they_separate() {
        assert_ne!(BORDER, CARD);
        assert_ne!(BORDER, BACKGROUND);
        assert_ne!(BORDER, ROW);
    }

    #[test]
    fn a_bevel_is_lit_from_above() {
        for bevel in [BEVEL_CARD, BEVEL_RAISED, BEVEL_HOVER] {
            assert!(bevel.top.r > bevel.sides.r, "the top edge must catch the light");
            assert!(bevel.sides.r > bevel.bottom.r, "the bottom edge must fall into shadow");
        }
        assert!(BEVEL_HOVER.top.r > BEVEL_CARD.top.r, "hover must brighten the edge");
    }

    #[test]
    fn spacing_stays_on_the_grid_except_the_structural_gap() {
        for s in [
            SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_5, SPACE_6, SPACE_8, SPACE_10, SPACE_12,
            SPACE_16,
        ] {
            assert_eq!(s % 4.0, 0.0, "{s} is off the 4px grid");
        }
        assert_eq!(GAP, 5.0);
    }

    #[test]
    fn route_colours_are_distinguishable_from_each_other() {
        assert_ne!(ROUTE_DIRECT, ROUTE_RELAY);
        assert_ne!(ROUTE_RELAY, ROUTE_OFFLINE);
        assert_ne!(ROUTE_DIRECT, ROUTE_OFFLINE);
    }

    #[test]
    fn alpha_helper_preserves_hue() {
        let ghost = with_alpha(PRIMARY, 0.1);
        assert_eq!((ghost.r, ghost.g, ghost.b), (PRIMARY.r, PRIMARY.g, PRIMARY.b));
        assert_eq!(ghost.a, 0.1);
    }
}
