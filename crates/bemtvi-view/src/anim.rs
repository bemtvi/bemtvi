//! The scroll-slide state machine shared by every animating client: the
//! in-flight [`ScrollAnim`] snapshot driven by the client's local clock, and the
//! [`arm_scroll`] lifecycle owner. The [`ScrollData`] gesture it animates is
//! decoded from the redraw in [`crate::View`]; *painting* the interpolated frame
//! stays per client (the TUI slices an owned band window, the GUI projects a
//! borrowed `ScrollFrame`).

use std::time::Instant;

use crate::{ScrollData, View};

/// An in-flight scroll slide: the gesture's band snapshot (cloned once when the
/// slide arms) plus the arm instant progress is measured from. The band is
/// **screen-row based**: the slide advances a screen-row offset (`from_row` →
/// `to_row`) into `lines`/the parallel overlay arrays, so interleaved
/// `virt_lines` slide with the text instead of snapping.
pub struct ScrollAnim {
    /// The gesture snapshot the slide plays — band rows, every overlay layer,
    /// and the style palette captured with them (see [`ScrollData`]).
    pub data: ScrollData,
    pub start: Instant,
}

impl ScrollAnim {
    pub fn new(s: &ScrollData) -> Self {
        ScrollAnim {
            data: s.clone(),
            start: Instant::now(),
        }
    }

    /// Whether the slide has played out (time to settle on the destination view).
    pub fn done(&self) -> bool {
        self.start.elapsed() >= self.data.duration
    }

    /// Eased progress in `[0, 1]` at the current instant (ease-out cubic — the
    /// shared feel across clients). [`arm_scroll`] never arms a zero-duration
    /// slide; the guard keeps progress from going NaN/inf if that ever changes.
    pub fn progress(&self) -> f32 {
        let raw = if self.data.duration.is_zero() {
            1.0
        } else {
            (self.start.elapsed().as_secs_f32() / self.data.duration.as_secs_f32()).clamp(0.0, 1.0)
        };
        1.0 - (1.0 - raw).powi(3)
    }
}

/// Linear interpolation from `a` to `b` at `t`.
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Decide the scroll slide to run after a `redraw`, given any slide already in
/// flight (`current`). The single owner of the slide's lifecycle, called by each
/// client's event loop (and the TUI's test driver) so they can't diverge.
///
/// - A redraw carrying a scroll gesture (re)arms a fresh slide.
/// - A redraw with no scroll gesture *that just repaints the slide's
///   destination* — e.g. an async syntax-highlight reply for the lines we
///   scrolled to — is left to play out: clearing it there would snap the view.
/// - Any other scroll-less redraw is a real change (a keypress, edit, or cursor
///   move) and interrupts the slide.
pub fn arm_scroll(view: &View, current: Option<ScrollAnim>) -> Option<ScrollAnim> {
    // The scroll gesture, like the cursor, belongs to the focused window.
    if let Some(s) = view.focused().and_then(|w| w.scroll.as_ref()) {
        // A zero-duration gesture has no slide to play, and arming one would
        // later divide elapsed time by a zero duration when computing progress
        // (a NaN/inf that paints one glitched frame). Drop any in-flight slide
        // and show the static destination the redraw already carries.
        if s.duration.is_zero() {
            return None;
        }
        return Some(ScrollAnim::new(s));
    }
    current.filter(|a| repaints_destination(view, a))
}

/// Whether `view` merely repaints the destination viewport `anim` is sliding
/// toward — same first visible line and cursor line — rather than reflecting a
/// new navigation state. Such a redraw (a delayed highlight reply) must not
/// abort the slide.
fn repaints_destination(view: &View, anim: &ScrollAnim) -> bool {
    // The destination viewport top / cursor buffer lines are read off the band at
    // its settle offsets (`to_row` / `to_cursor_row`): `numbers` carries each band
    // row's 1-based buffer line, so the row at `to_row` is the destination's top
    // line and the row at `to_cursor_row` is its cursor line.
    let dest_top = anim
        .data
        .numbers
        .get(anim.data.to_row.round() as usize)
        .copied()
        .flatten();
    let dest_cursor = anim
        .data
        .numbers
        .get(anim.data.to_cursor_row.round() as usize)
        .copied()
        .flatten();
    // The slide belongs to the focused window; read its destination viewport.
    let Some(win) = view.focused() else {
        return false;
    };
    win.numbers.first().copied().flatten() == dest_top && Some(win.cursor_line) == dest_cursor
}
