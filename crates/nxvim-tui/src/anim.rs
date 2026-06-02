//! The scroll-animation state machine: the [`ScrollData`] gesture mirrored from
//! a redraw, the in-flight [`Animation`] driven by the client clock, and the
//! [`arm_animation`] lifecycle owner shared by the live loop and the test driver.

use ratatui::style::Style;
use std::time::{Duration, Instant};

use crate::parse::HlSpan;
use crate::view::View;

/// The scroll gesture mirrored from the server's redraw, ready to animate.
/// Line/cursor positions are kept as `f32` for interpolation; `lines`/`selection`
/// are the band covering the slide, anchored at `base_line`.
#[derive(Clone)]
pub(crate) struct ScrollData {
    pub(crate) from_top: f32,
    pub(crate) to_top: f32,
    pub(crate) from_cursor: f32,
    pub(crate) to_cursor: f32,
    pub(crate) duration: Duration,
    pub(crate) base_line: usize,
    pub(crate) lines: Vec<String>,
    pub(crate) selection: Vec<Option<(u16, u16)>>,
    pub(crate) numbers: Vec<Option<usize>>,
    /// Syntax highlights for the band (aligned with `lines`), so the slide is
    /// colored frame by frame instead of flashing white until it settles. Style
    /// ids index `styles` below.
    pub(crate) highlights: Vec<Vec<HlSpan>>,
    /// The style palette captured with this gesture. Snapshotted (not read live
    /// from [`View::styles`]) because a delayed highlight redraw arriving
    /// mid-slide replaces the live palette, which would leave the band's frozen
    /// style ids pointing at the wrong entries.
    pub(crate) styles: Vec<Style>,
}

/// An in-flight scroll animation, driven by the client's local clock.
pub(crate) struct Animation {
    pub(crate) from_top: f32,
    pub(crate) to_top: f32,
    pub(crate) from_cursor: f32,
    pub(crate) to_cursor: f32,
    pub(crate) start: Instant,
    pub(crate) duration: Duration,
    pub(crate) base_line: usize,
    pub(crate) lines: Vec<String>,
    pub(crate) selection: Vec<Option<(u16, u16)>>,
    pub(crate) numbers: Vec<Option<usize>>,
    pub(crate) highlights: Vec<Vec<HlSpan>>,
    /// Palette snapshot the band's style ids index into (see [`ScrollData`]).
    pub(crate) styles: Vec<Style>,
}

impl Animation {
    fn new(s: &ScrollData) -> Self {
        Animation {
            from_top: s.from_top,
            to_top: s.to_top,
            from_cursor: s.from_cursor,
            to_cursor: s.to_cursor,
            start: Instant::now(),
            duration: s.duration,
            base_line: s.base_line,
            lines: s.lines.clone(),
            selection: s.selection.clone(),
            numbers: s.numbers.clone(),
            highlights: s.highlights.clone(),
            styles: s.styles.clone(),
        }
    }
}

pub(crate) fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Decide the scroll animation to run after a `redraw`, given any animation
/// already in flight (`current`). The single owner of the animation's lifecycle,
/// called by both the live event loop and the test driver so they can't diverge.
///
/// - A redraw carrying a scroll gesture (re)arms a fresh slide.
/// - A redraw with no scroll gesture *that just repaints the slide's
///   destination* — e.g. an async syntax-highlight reply for the lines we
///   scrolled to — is left to play out: clearing it there would snap the view.
/// - Any other scroll-less redraw is a real change (a keypress, edit, or cursor
///   move) and interrupts the slide, as before.
pub(crate) fn arm_animation(view: &View, current: Option<Animation>) -> Option<Animation> {
    if let Some(s) = view.scroll.as_ref() {
        // A zero-duration gesture has no slide to play, and arming one would
        // later divide elapsed time by a zero duration when computing progress
        // (a NaN/inf that paints one glitched frame). Drop any in-flight slide
        // and show the static destination the redraw already carries.
        if s.duration.is_zero() {
            return None;
        }
        return Some(Animation::new(s));
    }
    current.filter(|a| repaints_destination(view, a))
}

/// Whether `view` merely repaints the destination viewport `anim` is sliding
/// toward — same first visible line and cursor line — rather than reflecting a
/// new navigation state. Such a redraw (a delayed highlight reply) must not
/// abort the slide.
fn repaints_destination(view: &View, anim: &Animation) -> bool {
    // `to_top` / `to_cursor` are whole line indices kept as `f32` for
    // interpolation; the destination is reached at exactly those lines.
    let dest_top = anim.to_top as usize + 1; // first visible line, 1-based
    let dest_cursor = anim.to_cursor as usize + 1; // cursor line, 1-based
    let top_line = view.numbers.first().copied().flatten();
    top_line == Some(dest_top) && view.cursor_line == dest_cursor
}
