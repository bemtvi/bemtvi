//! Tier 1: the pure inline LSP inlay-hint layer the GUI renderer feeds each text
//! row through — the per-column *shift* math (how far a hint pushes the glyphs and
//! the column-keyed overlays to its right) and the *segment splice* (inserting the
//! hint text into the row's colored runs). Black-box, no window, no GPU — the inlay
//! analogue of the `keys` / `mouse` tests. The painted frame itself needs a GPU and
//! is validated by running the client; these cover the math and the text assembly
//! it depends on, and mirror the TUI's inline splice (whose paint *is* black-box
//! tested over RPC).

use nxvim_gui::{inlay_shift, splice_inlay, Seg, DEFAULT_INLAY};
use nxvim_view::{InlayHint, Style};

/// A hint at `col` with `text` and no resolved style (the dim-fallback case).
fn hint(col: u16, text: &str) -> InlayHint {
    (col, text.to_string(), None)
}

/// The visible string the spliced segments lay out left to right.
fn text(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

// --- inlay_shift: the per-column shift the overlays key off ------------------

#[test]
fn shift_is_zero_with_no_hints() {
    assert_eq!(inlay_shift(&[], 0, 10, true), 0);
    assert_eq!(inlay_shift(&[], 0, 10, false), 0);
}

#[test]
fn shift_counts_hint_text_width_in_cells() {
    // A 5-cell hint ("=> i8") anchored at column 3.
    let hints = [hint(3, "=> i8")];
    // A glyph well past it shifts by the whole hint, inclusive or not.
    assert_eq!(inlay_shift(&hints, 0, 9, true), 5);
    assert_eq!(inlay_shift(&hints, 0, 9, false), 5);
}

#[test]
fn shift_boundary_is_inclusive_for_left_edges_exclusive_for_right() {
    // A hint at column 4 sits *before* the glyph at column 4 (the splice emits it
    // ahead of that glyph). So a left edge / cursor at column 4 clears it
    // (inclusive), but a span's right edge *at* column 4 does not (the hint is past
    // the span).
    let hints = [hint(4, "XX")]; // width 2
    assert_eq!(inlay_shift(&hints, 0, 4, true), 2); // left edge at 4 → shifted
    assert_eq!(inlay_shift(&hints, 0, 4, false), 0); // right edge at 4 → not
                                                     // One column further the exclusive form also clears it.
    assert_eq!(inlay_shift(&hints, 0, 5, false), 2);
    // One column short of it, neither form counts it.
    assert_eq!(inlay_shift(&hints, 0, 3, true), 0);
}

#[test]
fn shift_drops_hints_scrolled_off_the_left() {
    // Under a horizontal scroll, a hint left of `leftcol` paints no cells, so it
    // adds no shift (the best-effort horizontal-scroll behavior).
    let hints = [hint(1, "A"), hint(6, "BB")];
    assert_eq!(inlay_shift(&hints, 3, 10, true), 2); // only the col-6 hint counts
}

#[test]
fn shift_sums_multiple_hints_up_to_the_column() {
    let hints = [hint(1, "A"), hint(3, "BB"), hint(7, "CCC")];
    // Up to column 3 (inclusive): A + BB = 3 cells. The col-7 hint is past it.
    assert_eq!(inlay_shift(&hints, 0, 3, true), 3);
    // The whole row's inserted width (every hint at/after leftcol).
    assert_eq!(inlay_shift(&hints, 0, u16::MAX, true), 6);
}

// --- splice_inlay: inserting hint text into the row's colored runs ----------

#[test]
fn splice_with_no_hints_returns_the_base_untouched() {
    let base = vec![Seg::plain("let x".into(), 0xff_ff_ff)];
    let out = splice_inlay(base, &[], 0, &[]);
    assert_eq!(text(&out), "let x");
}

#[test]
fn splice_inserts_a_hint_before_the_glyph_at_its_column() {
    // "abcd" with a hint at column 2 → the hint sits before 'c'.
    let base = vec![Seg::plain("abcd".into(), 0xff_ff_ff)];
    let out = splice_inlay(base, &[hint(2, "T")], 0, &[]);
    assert_eq!(text(&out), "abTcd");
}

#[test]
fn splice_at_column_zero_prefixes_the_line() {
    let base = vec![Seg::plain("abcd".into(), 0xff_ff_ff)];
    let out = splice_inlay(base, &[hint(0, ">>")], 0, &[]);
    assert_eq!(text(&out), ">>abcd");
}

#[test]
fn splice_appends_a_hint_at_or_past_end_of_text() {
    // A type annotation anchored at end-of-line (column == len) and one past it
    // both append after the text.
    let base = vec![Seg::plain("abcd".into(), 0xff_ff_ff)];
    assert_eq!(
        text(&splice_inlay(base.clone(), &[hint(4, ": i32")], 0, &[])),
        "abcd: i32"
    );
    assert_eq!(
        text(&splice_inlay(base, &[hint(99, ": i32")], 0, &[])),
        "abcd: i32"
    );
}

#[test]
fn splice_drops_hints_scrolled_off_but_keeps_visible_ones() {
    // Under leftcol=2 the base covers columns [2,4) = "cd". A hint at column 1 is
    // scrolled off (dropped); one at column 2 (== leftcol) prefixes the visible run.
    let base = vec![Seg::plain("cd".into(), 0xff_ff_ff)];
    let out = splice_inlay(base, &[hint(1, "OFF"), hint(2, "ON")], 2, &[]);
    assert_eq!(text(&out), "ONcd");
}

#[test]
fn splice_colors_the_hint_from_its_resolved_style_else_the_dim_fallback() {
    let styles = vec![Style {
        fg: Some(0x12_34_56),
        ..Default::default()
    }];
    let base = vec![Seg::plain("ab".into(), 0xff_ff_ff)];
    // Resolved style id → that style's fg.
    let resolved = splice_inlay(base.clone(), &[(1, "H".into(), Some(0))], 0, &styles);
    let h = resolved.iter().find(|s| s.text == "H").unwrap();
    assert_eq!(h.fg, 0x12_34_56);
    // No id (or an out-of-range one) → the dim built-in.
    let fallback = splice_inlay(base, &[(1, "H".into(), Some(9))], 0, &styles);
    let h = fallback.iter().find(|s| s.text == "H").unwrap();
    assert_eq!(h.fg, DEFAULT_INLAY);
}

#[test]
fn splice_preserves_the_weight_and_slant_of_the_run_it_splits() {
    // A hint splitting a bold/italic syntax run leaves both halves bold/italic; the
    // hint itself is plain (its own color, no weight).
    let base = vec![Seg {
        text: "keyword".into(),
        fg: 0xaa_bb_cc,
        bold: true,
        italic: true,
    }];
    let out = splice_inlay(base, &[hint(3, "·")], 0, &[]);
    assert_eq!(text(&out), "key·word");
    for s in &out {
        if s.text == "·" {
            assert!(!s.bold && !s.italic, "hint run is plain");
        } else {
            assert!(s.bold && s.italic, "split halves keep weight/slant");
        }
    }
}
