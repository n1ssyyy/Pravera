//! Which screens this machine has.

use std::fmt;

use pravera_core::Resolution;

/// How many displays Pravera will enumerate.
///
/// Windows allows far more than anyone owns. The cap exists because display
/// ids are a single byte on the wire, and truncating loudly beats overflowing
/// quietly.
pub const MAX_DISPLAYS: usize = 64;

/// A display's position in this host's list.
///
/// **Zero is always the primary display.** That is not a platform guarantee —
/// neither `EnumDisplayMonitors` nor a Wayland compositor promises any
/// particular order — so backends sort their list to make it true. The
/// protocol relies on it: a client with only `VIEW` may ask for display 0
/// without holding `MULTI_MONITOR`, and that shortcut is only safe if 0 means
/// "the obvious one".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayId(pub u8);

impl DisplayId {
    pub const PRIMARY: DisplayId = DisplayId(0);

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for DisplayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "display {}", self.0)
    }
}

/// One screen, as the host sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub id: DisplayId,

    /// What to show a person. The monitor's own name when the platform knows
    /// it ("DELL U2723QE"), otherwise the device path.
    pub name: String,

    /// Physical pixels, before any scaling.
    pub resolution: Resolution,

    /// Top-left corner in the virtual desktop. Negative on displays arranged
    /// above or to the left of the primary, which is why this is signed.
    pub position: (i32, i32),

    /// 1.0 at 96 dpi. Sent to the client so it can size the pointer and text
    /// hints correctly; capture itself always works in physical pixels.
    pub scale: f32,

    pub primary: bool,

    /// Reported refresh rate, or 0 when the platform will not say. Used as the
    /// default capture cadence, never as a promise.
    pub refresh_hz: u32,
}

impl Display {
    /// Sort a freshly enumerated list into Pravera order and assign ids.
    ///
    /// Primary first, then left-to-right and top-to-bottom by desktop
    /// position, so the ids match how the screens are physically arranged
    /// rather than the order the OS happened to walk them in. Anything past
    /// [`MAX_DISPLAYS`] is dropped.
    pub fn normalise(displays: Vec<Display>) -> Vec<Display> {
        let paired = displays.into_iter().map(|display| (display, ())).collect();
        Display::normalise_with(paired)
            .into_iter()
            .map(|(display, ())| display)
            .collect()
    }

    /// [`Display::normalise`], carrying a platform handle alongside each entry.
    ///
    /// Backends need to get from an id back to whatever the OS uses to
    /// identify a screen. Reordering the displays and the handles separately
    /// would eventually let them disagree, and a session would silently
    /// capture the wrong monitor.
    pub(crate) fn normalise_with<T>(mut displays: Vec<(Display, T)>) -> Vec<(Display, T)> {
        displays.sort_by(|(a, _), (b, _)| {
            (!a.primary, a.position.0, a.position.1, &a.name).cmp(&(
                !b.primary,
                b.position.0,
                b.position.1,
                &b.name,
            ))
        });
        displays.truncate(MAX_DISPLAYS);

        for (index, (display, _)) in displays.iter_mut().enumerate() {
            display.id = DisplayId(index as u8);
            display.primary = index == 0;
        }
        displays
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(name: &str, x: i32, primary: bool) -> Display {
        Display {
            id: DisplayId(200),
            name: name.to_string(),
            resolution: Resolution::new(1920, 1080),
            position: (x, 0),
            scale: 1.0,
            primary,
            refresh_hz: 60,
        }
    }

    #[test]
    fn the_primary_display_is_always_id_zero() {
        // The OS listed the primary second. The protocol says id 0 is primary,
        // so the list is reordered rather than the promise weakened.
        let displays = Display::normalise(vec![
            display("left", -1920, false),
            display("centre", 0, true),
        ]);

        assert_eq!(displays[0].name, "centre");
        assert_eq!(displays[0].id, DisplayId::PRIMARY);
        assert!(displays[0].primary);
        assert_eq!(displays[1].name, "left");
        assert_eq!(displays[1].id, DisplayId(1));
    }

    #[test]
    fn the_rest_are_ordered_the_way_they_sit_on_the_desk() {
        let displays = Display::normalise(vec![
            display("right", 1920, false),
            display("far left", -3840, false),
            display("centre", 0, true),
            display("left", -1920, false),
        ]);

        let names: Vec<&str> = displays.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["centre", "far left", "left", "right"]);
    }

    #[test]
    fn a_list_with_no_primary_still_produces_one() {
        // A backend that cannot tell which display is primary must not leave
        // the host with no display 0 at all.
        let displays = Display::normalise(vec![display("b", 1920, false), display("a", 0, false)]);

        assert_eq!(displays[0].name, "a");
        assert!(displays[0].primary);
        assert!(!displays[1].primary);
    }

    #[test]
    fn a_platform_handle_stays_with_its_display_through_the_reorder() {
        // The whole point of the paired form: if the handles were sorted
        // separately, a session would capture a different screen than the one
        // the operator picked.
        let paired = Display::normalise_with(vec![
            (display("left", -1920, false), "HMONITOR-left"),
            (display("centre", 0, true), "HMONITOR-centre"),
        ]);

        assert_eq!(paired[0].0.name, "centre");
        assert_eq!(paired[0].1, "HMONITOR-centre");
        assert_eq!(paired[1].1, "HMONITOR-left");
    }

    #[test]
    fn ids_stay_within_one_byte() {
        let many: Vec<Display> = (0..300)
            .map(|n| display(&format!("m{n}"), n, false))
            .collect();
        let displays = Display::normalise(many);

        assert_eq!(displays.len(), MAX_DISPLAYS);
        assert_eq!(
            displays.last().unwrap().id,
            DisplayId((MAX_DISPLAYS - 1) as u8)
        );
    }
}
