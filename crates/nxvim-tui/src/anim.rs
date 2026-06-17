//! The scroll-animation state machine: the in-flight [`Animation`] driven by the
//! client clock, and the [`arm_animation`] lifecycle owner shared by the live
//! loop and the test driver. The [`ScrollData`](nxvim_view::ScrollData) gesture
//! it animates is decoded from the redraw in [`nxvim_view`].

use std::time::{Duration, Instant};

use nxvim_view::{
    HlSpan, IncSearchSpans, InlayHint, ScrollData, SearchSpans, Style, View, VirtPlacement,
};

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
    /// Orientation of the sliding visual selection (see
    /// [`ScrollData::sel_extends_down`]): `Some(true)` extends down, `Some(false)`
    /// up, `None` when none is sliding. Drives the selection edge clip in `render`.
    pub(crate) sel_extends_down: Option<bool>,
    pub(crate) numbers: Vec<Option<usize>>,
    pub(crate) highlights: Vec<Vec<HlSpan>>,
    /// `hlsearch` / `incsearch` match spans for the band (aligned with `lines`), so
    /// the search highlight slides with the text instead of vanishing until the
    /// slide settles.
    pub(crate) search: SearchSpans,
    pub(crate) incsearch: IncSearchSpans,
    /// Inline inlay hints for the band (aligned with `lines`), so they slide with
    /// the text instead of vanishing until the slide settles.
    pub(crate) inlay_hints: Vec<Vec<InlayHint>>,
    /// Extmark `virt_text` placements for the band (aligned with `lines`), so they
    /// slide with the line instead of flashing out and back when the slide settles.
    pub(crate) virt_text: Vec<Vec<VirtPlacement>>,
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
            sel_extends_down: s.sel_extends_down,
            numbers: s.numbers.clone(),
            highlights: s.highlights.clone(),
            search: s.search.clone(),
            incsearch: s.incsearch.clone(),
            inlay_hints: s.inlay_hints.clone(),
            virt_text: s.virt_text.clone(),
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
    // The scroll gesture, like the cursor, belongs to the focused window.
    let scroll = view.focused().and_then(|w| w.scroll.as_ref());
    if let Some(s) = scroll {
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
                                                   // The slide belongs to the focused window; read its destination viewport.
    let Some(win) = view.focused() else {
        return false;
    };
    let top_line = win.numbers.first().copied().flatten();
    top_line == Some(dest_top) && win.cursor_line == dest_cursor
}
