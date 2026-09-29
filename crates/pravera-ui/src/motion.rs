//! Motion vocabulary.
//!
//! Every duration, distance and curve the interface moves with. The constants
//! below are the source of truth; `DESIGN.md` restates them for people.
//!
//! | What | Constant | Value | Curve |
//! |---|---|---|---|
//! | hover, press, focus | [`MICRO`] | 120 ms | ease-out-quart |
//! | a value moving between two resting states | [`STANDARD`] | 200 ms | ease-out-cubic |
//! | a page's contents leaving | [`PAGE_OUT`] | 120 ms, no travel | exit |
//! | a page's contents arriving | [`ENTRANCE`] | 320 ms, from [`PAGE_RISE`] 6 px | enter |
//! | the stagger between a page's blocks | [`STAGGER_STEP`] | 22 ms, capped at [`STAGGER_MAX`] 220 ms | |
//! | the rail's highlight sliding | [`STANDARD`] | to the active item | ease-out-cubic |
//! | a segmented control's thumb sliding | [`STANDARD`] | to the chosen segment | ease-out-cubic |
//! | a dialog arriving | [`DIALOG_IN`] | 320 ms, from 97 % and transparent | enter |
//! | a dialog leaving | [`DIALOG_OUT`] | 120 ms | exit |
//! | the scrim | with its dialog | in and out together | enter / exit |
//! | a menu arriving | [`MENU_IN`] | 220 ms, from 97 % | enter |
//! | a menu leaving | [`MENU_OUT`] | 120 ms | exit |
//!
//! "Enter" is `cubic-bezier(0.22, 1, 0.36, 1)` — which is exactly the curve
//! known as ease-out-quint — so arrivals cover most of their distance before
//! the eye has settled and then land softly. "Exit" accelerates away: a thing
//! that is leaving should not linger, and easing out of view looks like it is
//! reluctant to go.
//!
//! The page's sheet is chrome and never moves: only what is on it changes,
//! the old contents fading out and the new ones rising in behind them.
//!
//! ## Reactive rendering
//!
//! iced 0.14 only redraws when something changes, so an animation in flight
//! will simply stop unless a redraw is requested every frame. Each screen owns
//! its animations and answers `is_animating(now)`; the application holds a
//! `window::frames()` subscription only while any answer is yes.
//!
//! ## Group opacity
//!
//! iced 0.14 cannot fade a subtree. [`rise`] fades by covering the content
//! with the surface it sits on; see [`crate::widget::transform`] for why that
//! is exact over a flat surface, and why dialogs thread alpha instead.

// This is the design system's motion vocabulary. The full set is defined up
// front so later screens reach for an existing token instead of inventing a
// duration; entries not yet consumed are intentional, not oversights.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use iced::animation::Easing;
use iced::{Animation, Color, Element, Vector};

use crate::theme::tokens as t;
use crate::widget::transform::Transform;

/// Hover, press, focus. Fast enough to feel like a direct response to the
/// pointer rather than an animation.
pub const MICRO: Duration = Duration::from_millis(120);
/// A value moving between two states it could legitimately hold: a switch, a
/// highlight crossing from one row to another.
pub const STANDARD: Duration = Duration::from_millis(200);
/// Something arriving on screen: a panel rising into place.
pub const ENTRANCE: Duration = Duration::from_millis(320);
/// A page's contents leaving: a fade and nothing else, as quick as pointer
/// feedback, because nobody is waiting to watch the old page go. Arriving is
/// [`ENTRANCE`].
pub const PAGE_OUT: Duration = MICRO;
/// A dialog arriving, and the scrim behind it: the same as anything else
/// that comes on screen.
pub const DIALOG_IN: Duration = ENTRANCE;
/// A dialog leaving, and the scrim with it: as quick as pointer feedback,
/// because a dialog that has been answered should not linger.
pub const DIALOG_OUT: Duration = MICRO;
/// A menu arriving. Shorter than a dialog: it was only ever a glance.
pub const MENU_IN: Duration = Duration::from_millis(220);
/// A menu leaving.
pub const MENU_OUT: Duration = MICRO;
/// A toast arriving or leaving.
pub const TOAST: Duration = Duration::from_millis(260);

/// How far a page's header and blocks rise as they arrive. Leaving does not
/// travel at all.
pub const PAGE_RISE: f32 = 6.0;
/// How far the rail rises when it arrives with the shell.
pub const PANEL_RISE: f32 = 10.0;
/// A dialog's arrival: from this scale and this far below.
pub const DIALOG_SCALE: f32 = 0.97;
pub const DIALOG_RISE: f32 = 3.0;

/// `cubic-bezier(0.22, 1, 0.36, 1)`.
pub const EASE_ENTER: Easing = Easing::EaseOutQuint;
/// Leaving: accelerate away.
pub const EASE_EXIT: Easing = Easing::EaseInCubic;
/// A value moving between two resting states.
pub const EASE_CHANGE: Easing = Easing::EaseOutCubic;
/// Pointer feedback. Quartic rather than quadratic so the tail is short enough
/// that a fast sweep across several items does not leave a comet trail.
pub const EASE_MICRO: Easing = Easing::EaseOutQuart;

/// The beat between one block of a page arriving and the next: the header
/// first, then each block of the body 22 ms after the one above it, and the
/// rows of a list the same again.
pub const STAGGER_STEP: Duration = Duration::from_millis(22);
/// Ceiling on accumulated stagger, so a long list is still one arrival rather
/// than a queue.
pub const STAGGER_MAX: Duration = Duration::from_millis(220);

/// The delay for the item at `index` in a staggered sequence.
pub fn stagger(index: usize) -> Duration {
    STAGGER_STEP
        .saturating_mul(index.min(u32::MAX as usize) as u32)
        .min(STAGGER_MAX)
}

/// How far into its entrance the panel at `index` is, for a page that began
/// arriving at `since`: 0 until its stagger has elapsed, then up the enter
/// curve to 1.
///
/// A pure function of two instants, so a page needs no animation state of its
/// own to cascade — only the moment it arrived.
pub fn cascade(since: Instant, now: Instant, index: usize) -> f32 {
    let start = since + stagger(index);
    if now <= start {
        return 0.0;
    }
    let progress = (now - start).as_secs_f32() / ENTRANCE.as_secs_f32();
    if progress >= 1.0 {
        1.0
    } else {
        EASE_ENTER.value(progress)
    }
}

/// Whether a cascade that began at `since` is still moving at `now`. Counts the
/// rows of a list inside a panel, which start after the panel does.
pub fn cascading(since: Instant, now: Instant) -> bool {
    now < since + STAGGER_MAX + STAGGER_MAX + ENTRANCE
}

/// How far into its entrance the row at `index` of a list is. The list itself
/// arrives at `panel` in the page's cascade, and its rows follow it, each
/// [`STAGGER_STEP`] after the last.
pub fn row_cascade(since: Instant, now: Instant, panel: usize, index: usize) -> f32 {
    cascade(since + stagger(index), now, panel)
}

/// A boolean animation on the micro tier: hover and press states.
pub fn micro(initial: bool) -> Animation<bool> {
    Animation::new(initial).duration(MICRO).easing(EASE_MICRO)
}

/// A boolean animation on the standard tier.
pub fn standard(initial: bool) -> Animation<bool> {
    Animation::new(initial).duration(STANDARD).easing(EASE_CHANGE)
}

/// A panel's entrance, staggered by its position.
pub fn entrance(initial: bool, index: usize) -> Animation<bool> {
    Animation::new(initial)
        .duration(ENTRANCE)
        .easing(EASE_ENTER)
        .delay(stagger(index))
}

// ---------------------------------------------------------------------------
// Tween
// ---------------------------------------------------------------------------

/// A number travelling from where it is to where it was told to go.
///
/// `iced::Animation` runs its curve backwards when a transition reverses, so
/// the same animation cannot ease out on the way in and ease in on the way
/// out. Enter and exit are not mirror images in this system, so everything
/// that has both — dialogs, menus, pages, toasts, the sidebar — uses this.
#[derive(Debug, Clone, Copy)]
pub struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    easing: Easing,
}

impl Tween {
    /// Resting at `value`.
    pub fn at(value: f32) -> Self {
        Tween {
            from: value,
            to: value,
            start: Instant::now(),
            duration: Duration::ZERO,
            easing: EASE_ENTER,
        }
    }

    /// Sets off towards `target` from wherever it is at `now`, so a reversal
    /// mid-flight continues from the drawn position rather than jumping.
    pub fn go(&mut self, target: f32, now: Instant, duration: Duration, easing: Easing) {
        self.go_after(target, now, Duration::ZERO, duration, easing);
    }

    /// The same, starting `delay` from now.
    pub fn go_after(
        &mut self,
        target: f32,
        now: Instant,
        delay: Duration,
        duration: Duration,
        easing: Easing,
    ) {
        self.from = self.value(now);
        self.to = target;
        self.start = now + delay;
        self.duration = duration;
        self.easing = easing;
    }

    /// Enters: towards 1 on the enter curve.
    pub fn enter(&mut self, now: Instant, duration: Duration) {
        self.go(1.0, now, duration, EASE_ENTER);
    }

    /// Exits: towards 0 on the exit curve.
    pub fn exit(&mut self, now: Instant, duration: Duration) {
        self.go(0.0, now, duration, EASE_EXIT);
    }

    /// Jumps to `value` with no transition.
    pub fn snap(&mut self, value: f32) {
        *self = Tween::at(value);
    }

    pub fn value(&self, now: Instant) -> f32 {
        if now <= self.start {
            return self.from;
        }
        if self.duration.is_zero() {
            return self.to;
        }
        let progress = (now - self.start).as_secs_f32() / self.duration.as_secs_f32();
        if progress >= 1.0 {
            return self.to;
        }
        let eased = self.easing.value(progress.clamp(0.0, 1.0));
        self.from + (self.to - self.from) * eased
    }

    /// Where it is going.
    pub fn target(&self) -> f32 {
        self.to
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.from != self.to && now < self.start + self.duration
    }

    /// Has arrived at 0 and is going nowhere: safe to unmount.
    pub fn is_gone(&self, now: Instant) -> bool {
        self.to == 0.0 && !self.is_animating(now)
    }
}

impl Default for Tween {
    fn default() -> Self {
        Tween::at(0.0)
    }
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// A panel or page rising into place over the window floor: `amount` 0 is
/// `distance` below and invisible, 1 is home and fully drawn.
pub fn rise<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, amount: f32) -> Element<'a, Message> {
    rise_on(content, amount, PANEL_RISE, t::BACKGROUND)
}

/// The same for something inside a page's sheet, which fades over the sheet's
/// colour rather than the window floor's, and rises [`PAGE_RISE`].
pub fn settle<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, amount: f32) -> Element<'a, Message> {
    rise_on(content, amount, PAGE_RISE, t::CARD)
}

/// The same over a specific surface, and by a specific distance. Negative
/// distances arrive from above.
pub fn rise_on<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    amount: f32,
    distance: f32,
    surface: Color,
) -> Element<'a, Message> {
    let amount = amount.clamp(0.0, 1.0);
    if amount >= 0.999 {
        return content.into();
    }
    Transform::new(content)
        .offset(Vector::new(0.0, distance * (1.0 - amount)))
        .fade(surface, amount)
        .into()
}

/// A dialog or menu arriving: scaled up from [`DIALOG_SCALE`] and risen
/// [`DIALOG_RISE`]. No fade — the caller threads `amount` through its styles,
/// because there is no flat surface behind a dialog to fade from.
pub fn pop<'a, Message: 'a>(content: impl Into<Element<'a, Message>>, amount: f32) -> Element<'a, Message> {
    let amount = amount.clamp(0.0, 1.0);
    if amount >= 0.999 {
        return content.into();
    }
    Transform::new(content)
        .offset(Vector::new(0.0, DIALOG_RISE * (1.0 - amount)))
        .scale(DIALOG_SCALE + (1.0 - DIALOG_SCALE) * amount)
        .into()
}

/// The chosen segment of a segmented control, and the thumb sliding to it.
///
/// Each screen keeps one for each control it draws and tells it when the
/// choice moves. `position` is a fractional segment index, so a slide that is
/// interrupted carries on from where the thumb is drawn, and each segment's
/// label can be tinted by how much of the thumb is under it.
#[derive(Debug, Clone, Copy)]
pub struct Thumb {
    at: Tween,
    chosen: usize,
}

impl Thumb {
    /// Resting under segment `chosen`.
    pub fn at(chosen: usize) -> Self {
        Thumb {
            at: Tween::at(chosen as f32),
            chosen,
        }
    }

    /// Slides to segment `index` over [`STANDARD`], from where it is drawn.
    /// Choosing what is already chosen does nothing.
    pub fn select(&mut self, index: usize, now: Instant) {
        if index != self.chosen {
            self.chosen = index;
            self.at.go(index as f32, now, STANDARD, EASE_CHANGE);
        }
    }

    /// Jumps to segment `index` with no slide: for a control whose contents
    /// were replaced rather than changed.
    pub fn snap(&mut self, index: usize) {
        self.chosen = index;
        self.at.snap(index as f32);
    }

    /// The segment chosen, which the thumb is on or heading for.
    pub fn chosen(&self) -> usize {
        self.chosen
    }

    /// Where the thumb is, in segments from the first: 1.5 is half way between
    /// the second and the third.
    pub fn position(&self, now: Instant) -> f32 {
        self.at.value(now)
    }

    /// How much of the thumb is under segment `index`, from 0 to 1: the share
    /// its label is lit by.
    pub fn amount(&self, index: usize, now: Instant) -> f32 {
        (1.0 - (self.position(now) - index as f32).abs()).clamp(0.0, 1.0)
    }

    pub fn is_animating(&self, now: Instant) -> bool {
        self.at.is_animating(now)
    }
}

/// Tracks which one of a row of sibling widgets the pointer is over, and
/// animates each one's hover state.
///
/// ## The bug this exists to prevent
///
/// The obvious way to do this is a bare `Option<usize>` with
/// `.on_enter(Hover(Some(i)))` and `.on_exit(Hover(None))`. It is wrong, and it
/// is wrong in a way that only shows up half the time.
///
/// `mouse_area` publishes enter and exit from inside its own `update`, and iced
/// walks a widget tree in order. Sweep the pointer rightwards from sibling 0 to
/// sibling 1 and the runtime visits 0 first: 0 exits, then 1 enters, and the
/// final state is `Some(1)`. Correct. Sweep leftwards from 1 to 0 and the
/// runtime still visits 0 first: 0 enters, *then* 1 exits and blanks it. Final
/// state `None`. The hover silently stops working in one direction.
///
/// The fix is that an exit has to say which widget it came from, so a stale one
/// cannot clear a fresh enter. [`HoverTracker::exit`] ignores any exit that is
/// not from the widget currently believed to be hovered, which makes the whole
/// thing independent of event order.
#[derive(Debug)]
pub struct HoverTracker {
    current: Option<usize>,
    animations: Vec<Animation<bool>>,
    /// How long one hover takes to land, and on what curve.
    timing: (Duration, Easing),
}

impl Default for HoverTracker {
    fn default() -> Self {
        HoverTracker::new(0)
    }
}

impl HoverTracker {
    /// A tracker for `len` siblings, none hovered.
    pub fn new(len: usize) -> Self {
        HoverTracker::with_timing(len, MICRO, EASE_MICRO)
    }

    /// The same, for siblings whose hover is a movement rather than a tint:
    /// the sidebar's entries grow when pointed at, and a growth that lands in
    /// the micro tier reads as a flicker.
    pub fn with_timing(len: usize, duration: Duration, easing: Easing) -> Self {
        HoverTracker {
            current: None,
            animations: (0..len)
                .map(|_| Animation::new(false).duration(duration).easing(easing))
                .collect(),
            timing: (duration, easing),
        }
    }

    fn fresh(&self) -> Animation<bool> {
        Animation::new(false).duration(self.timing.0).easing(self.timing.1)
    }

    /// Grows or shrinks to match a list whose length changed.
    ///
    /// Existing animations are kept so a rescan that returns the same peers
    /// does not visibly reset a hover the pointer is still inside.
    pub fn resize(&mut self, len: usize, now: Instant) {
        if self.animations.len() == len {
            return;
        }
        let fresh = self.fresh();
        self.animations.resize_with(len, || fresh.clone());
        // A shrink can strand the index that was hovered past the new end.
        if self.current.is_some_and(|i| i >= len) {
            self.assign(None, now);
        }
    }

    /// Applies one hover event: `entering` distinguishes an arrival from a
    /// departure. The pair a `mouse_area` produces maps straight onto this.
    pub fn set(&mut self, index: usize, entering: bool, now: Instant) {
        if entering {
            self.enter(index, now);
        } else {
            self.exit(index, now);
        }
    }

    /// The pointer entered sibling `index`.
    pub fn enter(&mut self, index: usize, now: Instant) {
        self.assign(Some(index), now);
    }

    /// The pointer left sibling `index`.
    ///
    /// A no-op unless `index` is the one currently held, which is what makes
    /// the tracker immune to out-of-order enter and exit.
    pub fn exit(&mut self, index: usize, now: Instant) {
        if self.current == Some(index) {
            self.assign(None, now);
        }
    }

    /// Clears any hover. For when the pointer leaves the whole group at once.
    pub fn clear(&mut self, now: Instant) {
        self.assign(None, now);
    }

    fn assign(&mut self, next: Option<usize>, now: Instant) {
        if self.current == next {
            return;
        }
        if let Some(previous) = self.current {
            if let Some(anim) = self.animations.get_mut(previous) {
                anim.go_mut(false, now);
            }
        }
        if let Some(index) = next {
            if let Some(anim) = self.animations.get_mut(index) {
                anim.go_mut(true, now);
            }
        }
        self.current = next;
    }

    /// How hovered sibling `index` is, from 0 to 1.
    pub fn amount(&self, index: usize, now: Instant) -> f32 {
        self.animations
            .get(index)
            .map_or(0.0, |a| a.interpolate(0.0, 1.0, now))
    }

    /// Which sibling the pointer is over, if any.
    pub fn current(&self) -> Option<usize> {
        self.current
    }

    /// Whether any hover transition is still in flight.
    pub fn is_animating(&self, now: Instant) -> bool {
        self.animations.iter().any(|a| a.is_animating(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the directional hover bug at the level it actually occurs:
    /// the order the runtime publishes enter and exit in.
    #[test]
    fn hover_survives_an_exit_that_arrives_after_the_next_enter() {
        let now = Instant::now();
        let mut hover = HoverTracker::new(3);

        // Sweeping right. Tree order puts the departed sibling first.
        hover.enter(0, now);
        hover.exit(0, now);
        hover.enter(1, now);
        assert_eq!(hover.current(), Some(1));

        // Sweeping left. Tree order now puts the *arriving* sibling first, so
        // the exit lands last. This is the case that used to blank the hover.
        hover.enter(0, now);
        hover.exit(1, now);
        assert_eq!(
            hover.current(),
            Some(0),
            "a stale exit must not clear a fresh enter"
        );
    }

    #[test]
    fn leaving_the_last_hovered_sibling_does_clear_it() {
        let now = Instant::now();
        let mut hover = HoverTracker::new(3);
        hover.enter(2, now);
        hover.exit(2, now);
        assert_eq!(hover.current(), None);
    }

    #[test]
    fn an_exit_from_a_sibling_that_was_never_entered_changes_nothing() {
        let now = Instant::now();
        let mut hover = HoverTracker::new(3);
        hover.enter(1, now);
        hover.exit(0, now);
        hover.exit(2, now);
        assert_eq!(hover.current(), Some(1));
    }

    #[test]
    fn hovering_starts_an_animation_and_resting_ends_it() {
        let start = Instant::now();
        let mut hover = HoverTracker::new(2);
        assert!(
            !hover.is_animating(start),
            "nothing should move before the pointer arrives"
        );

        hover.enter(0, start);
        assert!(hover.is_animating(start));
        assert_eq!(
            hover.amount(0, start),
            0.0,
            "the transition starts where it was"
        );

        let settled = start + MICRO + Duration::from_millis(1);
        assert!(!hover.is_animating(settled));
        assert_eq!(hover.amount(0, settled), 1.0);
        assert_eq!(hover.amount(1, settled), 0.0);
    }

    #[test]
    fn a_list_that_shrinks_past_the_hovered_row_forgets_it() {
        let now = Instant::now();
        let mut hover = HoverTracker::new(6);
        hover.enter(5, now);
        hover.resize(2, now);
        assert_eq!(
            hover.current(),
            None,
            "a stranded index would tint a row that no longer exists"
        );
        assert_eq!(
            hover.amount(5, now),
            0.0,
            "and must not panic when asked about it"
        );
    }

    /// Documents the trap that made these animations invisible even once they
    /// were written, and the reason `Pravera::update` refreshes its clock from
    /// `Instant::now()` before handling any interaction.
    #[test]
    fn an_animation_started_from_a_stale_clock_is_over_before_it_is_drawn() {
        let stale = Instant::now() - Duration::from_secs(30);
        let mut hover = HoverTracker::new(1);
        hover.enter(0, stale);

        let drawn_at = Instant::now();
        assert_eq!(hover.amount(0, drawn_at), 1.0);
        assert!(!hover.is_animating(drawn_at));

        let fresh = Instant::now();
        let mut hover = HoverTracker::new(1);
        hover.enter(0, fresh);
        assert!(hover.amount(0, fresh) < 1.0);
        assert!(hover.is_animating(fresh));
    }

    #[test]
    fn a_rescan_returning_the_same_length_leaves_the_hover_alone() {
        let now = Instant::now();
        let mut hover = HoverTracker::new(4);
        hover.enter(2, now);
        hover.resize(4, now);
        assert_eq!(hover.current(), Some(2));
    }

    #[test]
    fn leaving_is_quicker_than_arriving() {
        assert!(PAGE_OUT < ENTRANCE);
        assert!(DIALOG_OUT < DIALOG_IN);
        assert!(MENU_OUT < MENU_IN);
        assert!(MENU_IN <= DIALOG_IN);
    }

    #[test]
    fn micro_motion_stays_below_the_threshold_of_feeling_animated() {
        // Past ~150ms, pointer feedback starts reading as a delay.
        assert!(MICRO <= Duration::from_millis(150));
    }

    #[test]
    fn the_stagger_is_a_short_beat_and_stops() {
        assert_eq!(stagger(0), Duration::ZERO);
        assert_eq!(stagger(1), Duration::from_millis(22));
        assert_eq!(stagger(2), Duration::from_millis(44));
        assert_eq!(stagger(10), STAGGER_MAX);
        assert_eq!(stagger(1_000), STAGGER_MAX);
        assert_eq!(stagger(usize::MAX), STAGGER_MAX);
    }

    #[test]
    fn a_page_is_fully_in_before_a_second_has_gone() {
        // The header, a body block and a row far down a long list: the last
        // waits the most of everything and is still done inside a second.
        assert!(STAGGER_MAX + STAGGER_MAX + ENTRANCE <= Duration::from_millis(1000));
    }

    #[test]
    fn a_page_leaves_by_fading_alone() {
        // The leave is a pure fade: there is no distance for it to travel, and
        // it is over before a person could be waiting on it.
        assert!(PAGE_OUT <= Duration::from_millis(150));
        assert!(PAGE_RISE <= 8.0, "a rise of a few pixels, not a slide");
    }

    #[test]
    fn a_tween_arrives_and_then_rests() {
        let now = Instant::now();
        let mut tween = Tween::at(0.0);
        assert!(!tween.is_animating(now));
        tween.enter(now, DIALOG_IN);
        assert!(tween.is_animating(now));
        assert_eq!(tween.value(now), 0.0);
        let mid = tween.value(now + DIALOG_IN / 2);
        assert!(mid > 0.5, "the enter curve front-loads its travel, got {mid}");
        assert_eq!(tween.value(now + DIALOG_IN), 1.0);
        assert!(!tween.is_animating(now + DIALOG_IN));
    }

    #[test]
    fn a_tween_reversed_mid_flight_leaves_from_where_it_was_drawn() {
        let now = Instant::now();
        let mut tween = Tween::at(0.0);
        tween.enter(now, DIALOG_IN);
        let halfway = now + DIALOG_IN / 2;
        let drawn = tween.value(halfway);
        tween.exit(halfway, DIALOG_OUT);
        assert!((tween.value(halfway) - drawn).abs() < 1e-6, "no jump on reversal");
        let early = tween.value(halfway + DIALOG_OUT / 4);
        assert!(early > drawn * 0.8, "the exit curve starts gently, got {early}");
        assert!(tween.is_gone(halfway + DIALOG_OUT));
    }

    #[test]
    fn a_cascade_waits_its_turn_then_lands() {
        let since = Instant::now();
        assert_eq!(cascade(since, since, 0), 0.0);
        assert_eq!(cascade(since, since + stagger(0), 0), 0.0);
        assert!(cascade(since, since + stagger(0) + ENTRANCE / 2, 0) > 0.5);
        assert_eq!(cascade(since, since + stagger(3) + ENTRANCE, 3), 1.0);
        // Later panels are never ahead of earlier ones.
        let mid = since + Duration::from_millis(200);
        assert!(cascade(since, mid, 0) >= cascade(since, mid, 1));
        assert!(cascading(since, mid));
        // The rows of a list inside a panel start after the panel does, so the
        // cascade is over only once the last of them has landed.
        assert!(cascading(since, since + STAGGER_MAX + ENTRANCE));
        assert!(!cascading(since, since + STAGGER_MAX + STAGGER_MAX + ENTRANCE));
    }

    #[test]
    fn the_rows_of_a_list_follow_each_other_by_a_beat_and_stop_at_the_cap() {
        let since = Instant::now();
        let at = since + stagger(1) + STAGGER_STEP + ENTRANCE / 3;
        assert!(row_cascade(since, at, 1, 0) > row_cascade(since, at, 1, 1));
        assert!(row_cascade(since, at, 1, 1) > row_cascade(since, at, 1, 2));
        // Past the cap every row starts together, so a long list is still one
        // arrival and not a queue.
        let last = STAGGER_MAX.as_millis() / STAGGER_STEP.as_millis();
        let capped = last as usize + 5;
        assert_eq!(row_cascade(since, at, 1, last as usize), row_cascade(since, at, 1, capped));
        // And every row has landed by the time the cascade reports being done.
        let done = since + STAGGER_MAX + STAGGER_MAX + ENTRANCE;
        assert_eq!(row_cascade(since, done, 5, 99), 1.0);
    }

    #[test]
    fn a_thumb_slides_to_the_chosen_segment_over_the_standard_tier() {
        let now = Instant::now();
        let mut thumb = Thumb::at(0);
        assert_eq!(thumb.position(now), 0.0);
        assert!(!thumb.is_animating(now));

        thumb.select(2, now);
        assert_eq!(thumb.chosen(), 2);
        assert!(thumb.is_animating(now));
        assert_eq!(thumb.position(now), 0.0, "it sets off from where it was");
        let part = thumb.position(now + STANDARD / 4);
        assert!(part > 0.0 && part < 2.0, "{part}");
        assert_eq!(thumb.position(now + STANDARD), 2.0);
        assert!(!thumb.is_animating(now + STANDARD));
    }

    #[test]
    fn a_thumb_chosen_again_or_snapped_does_not_slide() {
        let now = Instant::now();
        let mut thumb = Thumb::at(1);
        thumb.select(1, now);
        assert!(!thumb.is_animating(now));
        thumb.snap(2);
        assert_eq!(thumb.position(now), 2.0);
        assert!(!thumb.is_animating(now));
    }

    #[test]
    fn a_thumb_reversed_mid_slide_carries_on_from_where_it_is_drawn() {
        let now = Instant::now();
        let mut thumb = Thumb::at(0);
        thumb.select(2, now);
        let halfway = now + STANDARD / 2;
        let drawn = thumb.position(halfway);
        thumb.select(0, halfway);
        assert!((thumb.position(halfway) - drawn).abs() < 1e-6);
    }

    #[test]
    fn the_shares_of_the_thumb_under_neighbouring_segments_add_up_to_one() {
        let now = Instant::now();
        let mut thumb = Thumb::at(0);
        thumb.select(1, now);
        for step in 0..=10 {
            let at = now + STANDARD * step / 10;
            let (a, b) = (thumb.amount(0, at), thumb.amount(1, at));
            assert!((a + b - 1.0).abs() < 1e-5, "{a} + {b}");
            assert_eq!(thumb.amount(2, at), 0.0);
        }
        assert_eq!(thumb.amount(1, now + STANDARD), 1.0);
    }

    #[test]
    fn a_delayed_tween_holds_still_until_its_moment() {
        let now = Instant::now();
        let mut tween = Tween::at(0.0);
        tween.go_after(1.0, now, Duration::from_millis(100), ENTRANCE, EASE_ENTER);
        assert_eq!(tween.value(now + Duration::from_millis(50)), 0.0);
        assert!(tween.is_animating(now));
    }
}
