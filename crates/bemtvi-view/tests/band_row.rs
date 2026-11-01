//! A slide's interpolated `leftcol` on one band row: the client half of the model the
//! server applies to a settled window. These match the server's results for the same
//! line in `tests/editing/rendering.rs` (`nowrap_clips_…`, `nowrap_keeps_…`, `nowrap_blanks_…`).

use bemtvi_view::{clip_band_row, InlayHint, VirtPlacement};

fn inline(col: u16, text: &str) -> VirtPlacement {
    VirtPlacement {
        pos: 1,
        col,
        hl_mode: 0,
        chunks: vec![(text.to_string(), None)],
    }
}

fn hint(col: u16, text: &str) -> InlayHint {
    (col, text.to_string(), None)
}

fn text(p: &VirtPlacement) -> String {
    p.chunks.iter().map(|c| c.0.as_str()).collect()
}

#[test]
fn a_column_short_of_every_splice_costs_nothing() {
    assert!(clip_band_row(0, &[], &[inline(0, "12345678")]).is_none());
    assert!(clip_band_row(5, &[hint(9, "ab")], &[inline(5, "x")]).is_none());
    assert!(clip_band_row(5, &[], &[]).is_none());
}

#[test]
fn inside_a_placement_the_rest_of_it_shows() {
    let row = clip_band_row(3, &[], &[inline(0, "12345678")]).expect("clipped");
    assert_eq!(row.left, 0);
    assert_eq!(text(&row.vtext[0]), "45678");
}

#[test]
fn past_a_placement_the_text_stays_where_its_cells_put_it() {
    let row = clip_band_row(10, &[], &[inline(0, "12345678")]).expect("clipped");
    assert_eq!(row.left, 2);
    assert!(row.vtext.is_empty());
}

#[test]
fn a_wide_char_cut_in_half_leaves_a_blank() {
    let row = clip_band_row(1, &[], &[inline(0, "語x")]).expect("clipped");
    assert_eq!(text(&row.vtext[0]), " x");
}

#[test]
fn a_hint_comes_before_inline_text_at_a_shared_anchor() {
    // `HI` paints at 0..2, then `xyz` at 2..5.
    let row = clip_band_row(3, &[hint(0, "HI")], &[inline(0, "xyz")]).expect("clipped");
    assert!(row.inlay.is_empty(), "the hint is wholly hidden");
    assert_eq!(text(&row.vtext[0]), "yz");
    assert_eq!(row.left, 0);
}

#[test]
fn other_positions_pass_through() {
    let eol = VirtPlacement {
        pos: 0,
        col: 0,
        hl_mode: 0,
        chunks: vec![("eol".to_string(), None)],
    };
    let row = clip_band_row(10, &[], &[inline(0, "12345678"), eol]).expect("clipped");
    assert_eq!(row.vtext.len(), 1);
    assert_eq!(text(&row.vtext[0]), "eol");
}
