//! Tier 1: the pure "selection / search wins over a span's own background" layer
//! the GUI renderer feeds each text row through. Black-box, no window, no GPU.
//! Guards the regression where a syntax group carrying a `bg` (markdown inline
//! `code`, a fenced code block's `@markup.raw.block`) painted its background quad
//! *over* the `Visual` selection, so selecting those runs showed no highlight. The
//! painted frame needs a GPU; this covers the background drop the paint depends on.

use bemtvi_gui::{clear_bg_under, seg_bg_spans, Seg};

const FG: u32 = 0xc0_c0_c0;
const CODE_BG: u32 = 0x30_30_30;

fn seg(text: &str, bg: Option<u32>) -> Seg {
    Seg {
        text: text.into(),
        fg: FG,
        bg,
        bold: false,
        italic: false,
    }
}

/// "see `code` here" with the backticked run on a code background.
fn row() -> Vec<Seg> {
    vec![
        seg("see ", None),
        seg("`code`", Some(CODE_BG)),
        seg(" here", None),
    ]
}

fn text(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn a_selection_over_a_background_span_drops_that_background() {
    // Select the whole line: no cell may keep the code background, or its quad
    // paints over the selection.
    let out = clear_bg_under(row(), &[(0, 15)], 0);
    assert_eq!(text(&out), "see `code` here", "no text lost");
    assert!(
        seg_bg_spans(&out, 0).is_empty(),
        "{:?}",
        seg_bg_spans(&out, 0)
    );
}

#[test]
fn a_partial_selection_keeps_the_background_outside_it() {
    // Select "see `co" [0,7): the code run's first 3 cells lose their bg, the
    // remaining "de`" [7,10) keep it.
    let out = clear_bg_under(row(), &[(0, 7)], 0);
    assert_eq!(text(&out), "see `code` here");
    assert_eq!(seg_bg_spans(&out, 0), vec![(7, 3, CODE_BG)]);
}

#[test]
fn spans_are_in_buffer_columns_under_a_horizontal_scroll() {
    // leftcol 4: the row starts at column 4 ("`code` here"); a span [4,6) covers
    // "`c", so the bg survives on "ode`" — at screen-relative offset 2 here.
    let segs = vec![seg("`code`", Some(CODE_BG)), seg(" here", None)];
    let out = clear_bg_under(segs, &[(4, 6)], 4);
    assert_eq!(seg_bg_spans(&out, 0), vec![(2, 4, CODE_BG)]);
}

#[test]
fn no_spans_leaves_the_row_untouched() {
    let out = clear_bg_under(row(), &[], 0);
    assert_eq!(seg_bg_spans(&out, 0), vec![(4, 6, CODE_BG)]);
}
