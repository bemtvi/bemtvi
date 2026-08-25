//! Tier 1: the cell-grid contract — every glyph of a shaped ASCII row must land on
//! `col * cell_w`. Black-box, no window, no GPU (like `caret.rs` / `keys.rs`), but
//! it drives the *real* shaper with the *real* metrics the renderer would use.
//!
//! The regression: the GUI paints a row as one shaped cosmic-text buffer at
//! `x = 0`, and paints the cursor (and every quad — selection, colorcolumn, the
//! statusline blocks) at `col * cell_w`. Those two agree only while the shaper's
//! per-glyph advance *is* `cell_w`. Under `set_monospace_width` cosmic-text snaps
//! each advance with
//!
//! ```text
//! let match_em_width = monospace_width / font_size;      // em
//! x_advance = (x_advance / match_em_width).round() * match_em_width;  // px / em (!)
//! ```
//!
//! — a unit mismatch (px divided by em), so the quantum is `cell_w / font_size`
//! instead of `cell_w`, and the snapped advance comes out
//! `cell_w * round(font_size) / font_size`. With an integral device-pixel font size
//! that is exactly `cell_w` and nothing drifts, which is why Linux/macOS (scale 1
//! or 2, so `15.0 * scale` is whole) never showed it. On Windows at 125% / 150%
//! display scaling the device-pixel size is `18.75` / `22.5`, the advance is off by
//! 1.3% / 2.2% of a cell, and the error *accumulates*: by column 80 the text has
//! slid a cell or two to the right of the grid and the cursor reads as drifting
//! ever further left of the character it is on.

use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};

/// The renderer's line-height factor (`render.rs`'s `LINE_SPACING`). Only the
/// vertical metric — it plays no part in the horizontal drift, but the buffer needs
/// metrics that match what the renderer builds.
const LINE_SPACING: f32 = 1.30;

/// Shape `COLS` monospace cells the way `Renderer::shape_segments` does — system
/// monospace family, `Shaping::Advanced`, then `set_monospace_width(cell_w)` — and
/// return the worst `|glyph.x − col * cell_w|`, in cells. `None` when the host has
/// no usable monospace font (a bare CI container), which the caller skips on.
fn worst_drift_cells(size_pt: f32, scale: f32) -> Option<f32> {
    let font_size = bemtvi_gui::physical_font_size(size_pt, scale);
    let line_height = (font_size * LINE_SPACING).round();
    let metrics = Metrics::new(font_size, line_height);
    let mut fs = FontSystem::new();
    let attrs = Attrs::new().family(Family::Monospace);

    // `measure_cell`: the cell is the shaped advance of `M` at these metrics.
    let mut probe = Buffer::new(&mut fs, metrics);
    probe.set_text(&mut fs, "M", &attrs, Shaping::Advanced, None);
    probe.shape_until_scroll(&mut fs, false);
    let cell_w = probe
        .layout_runs()
        .next()
        .and_then(|r| r.glyphs.first().map(|g| g.w))?;
    if cell_w < 1.0 {
        return None; // no real monospace face on this host
    }

    // A row wide enough that a per-glyph error accumulates into something a reader
    // sees — the reported symptom is at the right-hand end of a wide window.
    let text: String = std::iter::repeat_n("abcdefghij", 12).collect();
    let mut buf = Buffer::new(&mut fs, metrics);
    buf.set_text(&mut fs, &text, &attrs, Shaping::Advanced, None);
    buf.set_monospace_width(&mut fs, Some(cell_w));
    buf.shape_until_scroll(&mut fs, false);

    let run = buf.layout_runs().next()?;
    let worst = run
        .glyphs
        .iter()
        .enumerate()
        .map(|(col, g)| ((g.x - col as f32 * cell_w) / cell_w).abs())
        .fold(0.0f32, f32::max);
    Some(worst)
}

/// A whole cell of accumulated slide is the point at which the cursor is visibly on
/// the wrong character; a fraction of a cell is sub-glyph and invisible. Assert far
/// tighter than that — a correct grid drifts by float noise only.
const MAX_DRIFT_CELLS: f32 = 0.05;

#[test]
fn glyphs_stay_on_the_cell_grid_at_fractional_display_scales() {
    // 1.25 / 1.5 / 1.75 are Windows' standard display-scaling steps — the ones that
    // turn a 15pt font into a fractional device-pixel size.
    for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
        let Some(drift) = worst_drift_cells(15.0, scale) else {
            eprintln!("skip: no monospace font available");
            return;
        };
        assert!(
            drift < MAX_DRIFT_CELLS,
            "at {scale}x display scale the shaped row drifts {drift:.3} cells off the \
             grid — the cursor (painted at col * cell_w) reads further left of its \
             character the further right it goes",
        );
    }
}

#[test]
fn glyphs_stay_on_the_cell_grid_at_fractional_point_sizes() {
    // The same failure without any display scaling: `:set guifont=…:h13.5`, or a
    // font size stepped with `<C-ScrollWheel>` onto a half point.
    for pt in [12.0, 13.5, 14.2, 15.0, 16.7] {
        let Some(drift) = worst_drift_cells(pt, 1.0) else {
            eprintln!("skip: no monospace font available");
            return;
        };
        assert!(
            drift < MAX_DRIFT_CELLS,
            "at {pt}pt the shaped row drifts {drift:.3} cells off the grid",
        );
    }
}
