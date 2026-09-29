//! A captured framebuffer and the record of what changed in it.

use std::time::Duration;

use bytes::Bytes;
use pravera_core::{PixelFormat, Rect, Resolution};

use crate::DisplayId;

/// What changed since the previous frame.
///
/// Desktop content is overwhelmingly static: a blinking cursor in an editor
/// touches a few hundred pixels out of eight million. Carrying that fact from
/// capture to the encoder is the single largest compression win available, so
/// it is part of the frame rather than something rediscovered later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// Everything changed, or the backend does not track damage. Always safe.
    Full,
    /// Only these rectangles changed. Never empty — see [`Damage::regions`].
    Regions(Vec<Rect>),
}

impl Damage {
    /// Build from a backend's rectangle list.
    ///
    /// An empty list becomes [`Damage::Full`] rather than "nothing changed".
    /// Backends report no rectangles in two very different situations — the
    /// first frame of a session, and a frame where genuinely nothing moved —
    /// and guessing wrong in the optimistic direction leaves stale pixels on
    /// the viewer's screen with nothing scheduled to correct them.
    pub fn regions(rects: Vec<Rect>) -> Damage {
        let rects: Vec<Rect> = rects.into_iter().filter(|r| !r.is_empty()).collect();
        if rects.is_empty() {
            Damage::Full
        } else {
            Damage::Regions(rects)
        }
    }

    pub fn is_full(&self) -> bool {
        matches!(self, Damage::Full)
    }

    /// Total area covered, counting overlaps twice. A cheap estimate used to
    /// decide when scattered damage is no longer worth tracking separately.
    pub fn area(&self, resolution: Resolution) -> u64 {
        match self {
            Damage::Full => resolution.pixels(),
            Damage::Regions(rects) => rects.iter().map(|r| r.area()).sum(),
        }
    }

    /// One rectangle containing everything that changed.
    pub fn bounds(&self, resolution: Resolution) -> Rect {
        match self {
            Damage::Full => Rect::covering(resolution),
            Damage::Regions(rects) => rects
                .iter()
                .copied()
                .reduce(Rect::union)
                .unwrap_or_else(|| Rect::covering(resolution)),
        }
    }

    /// Absorb an earlier frame's damage into this one.
    ///
    /// Used when a frame is dropped before anyone reads it: the pixels of the
    /// dropped frame are gone, but the *fact* that those regions changed still
    /// has to reach the encoder, or it will leave them untouched and the
    /// viewer keeps looking at pixels from two frames ago.
    pub fn absorb(&mut self, earlier: &Damage) {
        match (&mut *self, earlier) {
            (Damage::Full, _) => {}
            (_, Damage::Full) => *self = Damage::Full,
            (Damage::Regions(mine), Damage::Regions(theirs)) => mine.extend_from_slice(theirs),
        }
    }

    /// Collapse to at most `limit` rectangles by unioning the rest together.
    ///
    /// Every rectangle costs the encoder a separate pass, so past some count
    /// it is cheaper to encode one larger area than many small ones.
    pub fn coalesce(&mut self, limit: usize, resolution: Resolution) {
        let Damage::Regions(rects) = self else {
            return;
        };
        if limit == 0 {
            *self = Damage::Full;
            return;
        }
        if rects.len() <= limit {
            return;
        }

        // Keep the largest `limit - 1` and fold everything else into one.
        rects.sort_by_key(|r| std::cmp::Reverse(r.area()));
        let tail: Vec<Rect> = rects.split_off(limit - 1);
        if let Some(merged) = tail.into_iter().reduce(Rect::union) {
            rects.push(merged);
        }

        // If the collapse ended up covering nearly everything, say so plainly.
        if self.area(resolution) * 10 >= resolution.pixels() * 9 {
            *self = Damage::Full;
        }
    }
}

/// One frame, owned and detached from whatever produced it.
///
/// Owned rather than borrowed on purpose: the Windows backend delivers frames
/// on a compositor callback thread and must return promptly, and a Wayland
/// buffer has to go back to the pool. Both mean the pixels are copied out
/// before the encoder ever sees them. That copy is the price of P1's CPU path
/// and the thing zero-copy import removes in P7.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub display: DisplayId,
    pub format: PixelFormat,
    pub resolution: Resolution,

    /// Bytes per row.
    ///
    /// Backends pack rows tightly, so this is `width * bytes_per_pixel`. It is
    /// carried explicitly anyway because GPU staging textures are padded to
    /// whatever alignment the driver likes, and a consumer that assumes tight
    /// packing produces a picture that shears diagonally — a bug that is
    /// obvious on screen and invisible in a unit test.
    pub stride: usize,

    pub pixels: Bytes,

    /// Time since the capture started, taken as close to the compositor's own
    /// timestamp as the platform allows. Relative, not wall-clock: it exists
    /// to measure intervals and pace playback, and a clock that can step
    /// backwards would do neither.
    pub elapsed: Duration,

    pub damage: Damage,
}

impl CapturedFrame {
    /// Whether the buffer is the size the other fields claim.
    ///
    /// Cheap, and worth asserting at the boundary: every downstream stage
    /// indexes this buffer using `stride` and `height`, so a mismatch here
    /// becomes an out-of-bounds read three crates away.
    pub fn is_consistent(&self) -> bool {
        let expected = self.stride.saturating_mul(self.resolution.height as usize);
        expected > 0
            && self.pixels.len() == expected
            && self.stride >= self.resolution.width as usize
    }

    /// Microseconds since capture began, in the width the wire protocol uses.
    ///
    /// Wraps roughly every 71 minutes. Harmless: the receiver only ever
    /// subtracts adjacent values to recover an interval.
    pub fn capture_micros(&self) -> u32 {
        self.elapsed.as_micros() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HD: Resolution = Resolution::new(1920, 1080);

    #[test]
    fn no_reported_damage_means_repaint_everything() {
        // The optimistic reading of an empty list leaves stale pixels on
        // screen with nothing scheduled to correct them.
        assert_eq!(Damage::regions(vec![]), Damage::Full);
        assert_eq!(Damage::regions(vec![Rect::new(0, 0, 0, 40)]), Damage::Full);
    }

    #[test]
    fn damage_from_a_dropped_frame_is_carried_forward() {
        let mut newest = Damage::regions(vec![Rect::new(0, 0, 10, 10)]);
        newest.absorb(&Damage::regions(vec![Rect::new(500, 500, 10, 10)]));

        let Damage::Regions(rects) = &newest else {
            panic!("expected regions, got {newest:?}");
        };
        assert!(rects.contains(&Rect::new(500, 500, 10, 10)));
    }

    #[test]
    fn absorbing_a_full_repaint_makes_the_result_full() {
        let mut newest = Damage::regions(vec![Rect::new(0, 0, 10, 10)]);
        newest.absorb(&Damage::Full);
        assert_eq!(newest, Damage::Full);

        let mut full = Damage::Full;
        full.absorb(&Damage::regions(vec![Rect::new(0, 0, 10, 10)]));
        assert_eq!(full, Damage::Full);
    }

    #[test]
    fn coalescing_keeps_the_big_regions_and_merges_the_crumbs() {
        let mut damage = Damage::regions(vec![
            Rect::new(0, 0, 400, 400),
            Rect::new(1000, 0, 4, 4),
            Rect::new(1004, 0, 4, 4),
            Rect::new(1008, 0, 4, 4),
        ]);
        damage.coalesce(2, HD);

        let Damage::Regions(rects) = &damage else {
            panic!("expected regions, got {damage:?}");
        };
        assert_eq!(rects.len(), 2);
        assert!(rects.contains(&Rect::new(0, 0, 400, 400)));
        assert!(rects.contains(&Rect::new(1000, 0, 12, 4)));
    }

    #[test]
    fn coalescing_that_covers_the_screen_admits_it_is_a_full_repaint() {
        // Two opposite corners union into the whole desktop. Reporting that as
        // a "region" would make every downstream size check meaningless.
        let mut damage = Damage::regions(vec![
            Rect::new(0, 0, 4, 4),
            Rect::new(960, 540, 4, 4),
            Rect::new(1916, 1076, 4, 4),
        ]);
        damage.coalesce(1, HD);
        assert_eq!(damage, Damage::Full);
    }

    #[test]
    fn coalescing_leaves_a_short_list_alone() {
        let original = Damage::regions(vec![Rect::new(0, 0, 10, 10)]);
        let mut damage = original.clone();
        damage.coalesce(4, HD);
        assert_eq!(damage, original);
    }

    #[test]
    fn bounds_of_scattered_damage_contain_all_of_it() {
        let damage = Damage::regions(vec![Rect::new(10, 10, 5, 5), Rect::new(100, 200, 5, 5)]);
        assert_eq!(damage.bounds(HD), Rect::new(10, 10, 95, 195));
        assert_eq!(Damage::Full.bounds(HD), Rect::covering(HD));
    }

    fn frame(stride: usize, len: usize) -> CapturedFrame {
        CapturedFrame {
            display: DisplayId::PRIMARY,
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(64, 4),
            stride,
            pixels: Bytes::from(vec![0u8; len]),
            elapsed: Duration::ZERO,
            damage: Damage::Full,
        }
    }

    #[test]
    fn a_buffer_that_does_not_match_its_stride_is_rejected() {
        assert!(frame(64 * 4, 64 * 4 * 4).is_consistent());
        assert!(!frame(64 * 4, 64 * 4 * 3).is_consistent());
        assert!(!frame(0, 0).is_consistent());
        // A stride narrower than a row cannot describe any real framebuffer.
        assert!(!frame(4, 16).is_consistent());
    }

    #[test]
    fn capture_time_survives_the_narrowing_to_the_wire_width() {
        let mut f = frame(64 * 4, 64 * 4 * 4);
        f.elapsed = Duration::from_micros(1_234_567);
        assert_eq!(f.capture_micros(), 1_234_567);
    }
}
