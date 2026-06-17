//! The scroll-animation state machine: the in-flight [`Animation`] driven by the
//! client clock, and the [`arm_animation`] lifecycle owner shared by the live
//! loop and the test driver. The [`ScrollData`](nxvim_view::ScrollData) gesture
//! it animates is decoded from the redraw in [`nxvim_view`].

use std::time::{Duration, Instant};

use nxvim_view::{
    DiagSign, DiagSpan, DiagVirt, HlSpan, IncSearchSpans, InlayHint, ScrollData, SearchSpans,
    Style, View, VirtChunk, VirtPlacement,
};

/// An in-flight scroll animation, driven by the client's local clock. The band is
/// **screen-row based**: the slide advances a screen-row offset (`from_row` →
/// `to_row`) into `lines`/the parallel overlay arrays, so interleaved `virt_lines`
/// slide with the text instead of snapping.
pub(crate) struct Animation {
    pub(crate) from_row: f32,
    pub(crate) to_row: f32,
    pub(crate) from_cursor_row: f32,
    pub(crate) to_cursor_row: f32,
    pub(crate) start: Instant,
    pub(crate) duration: Duration,
    pub(crate) lines: Vec<String>,
    pub(crate) selection: Vec<Option<(u16, u16)>>,
    /// Per band row, the secondary multi-cursors' selection spans, so they slide too.
    pub(crate) secondary_selection: SearchSpans,
    /// Orientation of the sliding visual selection (see
    /// [`ScrollData::sel_extends_down`]): `Some(true)` extends down, `Some(false)`
    /// up, `None` when none is sliding. Drives the selection edge clip in `render`.
    pub(crate) sel_extends_down: Option<bool>,
    pub(crate) numbers: Vec<Option<usize>>,
    /// Per band row, `true` on a soft-wrap continuation row, so the gutter blanks
    /// the wrapped rows while the slide animates (the band sibling of the per-window
    /// `continuation`).
    pub(crate) continuation: Vec<bool>,
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
    /// Extmark `virt_lines` content per band row, so the interleaved virtual rows
    /// slide with the text instead of only appearing once the slide settles.
    pub(crate) virt_lines: Vec<Option<Vec<VirtChunk>>>,
    /// Inline diagnostic virtual text per band row, sliding with the line.
    pub(crate) diagnostics_virt: Vec<Option<DiagVirt>>,
    /// Diagnostic underline spans / sign-column glyphs per band row, so the
    /// squiggles and signs slide with the text instead of blanking for the slide.
    pub(crate) diagnostics: Vec<Vec<DiagSpan>>,
    pub(crate) diagnostics_signs: Vec<Option<DiagSign>>,
    /// Palette snapshot the band's style ids index into (see [`ScrollData`]).
    pub(crate) styles: Vec<Style>,
}

impl Animation {
    fn new(s: &ScrollData) -> Self {
        Animation {
            from_row: s.from_row,
            to_row: s.to_row,
            from_cursor_row: s.from_cursor_row,
            to_cursor_row: s.to_cursor_row,
            start: Instant::now(),
            duration: s.duration,
            lines: s.lines.clone(),
            selection: s.selection.clone(),
            secondary_selection: s.secondary_selection.clone(),
            sel_extends_down: s.sel_extends_down,
            numbers: s.numbers.clone(),
            continuation: s.continuation.clone(),
            highlights: s.highlights.clone(),
            search: s.search.clone(),
            incsearch: s.incsearch.clone(),
            inlay_hints: s.inlay_hints.clone(),
            virt_text: s.virt_text.clone(),
            virt_lines: s.virt_lines.clone(),
            diagnostics_virt: s.diagnostics_virt.clone(),
            diagnostics: s.diagnostics.clone(),
            diagnostics_signs: s.diagnostics_signs.clone(),
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
    // The destination viewport top / cursor buffer lines are read off the band at
    // its settle offsets (`to_row` / `to_cursor_row`): `numbers` carries each band
    // row's 1-based buffer line, so the row at `to_row` is the destination's top
    // line and the row at `to_cursor_row` is its cursor line.
    let dest_top = anim
        .numbers
        .get(anim.to_row.round() as usize)
        .copied()
        .flatten();
    let dest_cursor = anim
        .numbers
        .get(anim.to_cursor_row.round() as usize)
        .copied()
        .flatten();
    // The slide belongs to the focused window; read its destination viewport.
    let Some(win) = view.focused() else {
        return false;
    };
    let top_line = win.numbers.first().copied().flatten();
    top_line == dest_top && Some(win.cursor_line) == dest_cursor
}
