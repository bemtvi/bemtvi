//! The `>` / `<` shift operators — `>>`/`<<`, `>{motion}`, `>{textobject}`,
//! and their visual-mode forms. Shift moves whole lines by one `shiftwidth`
//! (right for `>`, left for `<`), clamped at column 0, leaving blank lines
//! untouched. Driven black-box: feed keys, assert on buffer lines / cursor.
//!
//! Every test sets `expandtab` so the inserted indent is spaces and the
//! assertions read literally; `shiftwidth` is set explicitly per test.

use crate::support::*;

/// Open a fresh server on a plain temp file seeded with `content`. No extension,
/// so no grammar / filetype indent gets in the way of the shift under test.
async fn start_seeded(tag: &str, content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = temp_path(tag).to_string_lossy().into_owned();
    std::fs::write(&path, content).expect("write temp file");
    start(Some(path)).await
}

// ===== `>>` / `<<` (doubled) =================================================

#[tokio::test]
async fn shift_right_indents_current_line_by_one_shiftwidth() {
    let (rpc, _i) = start_seeded("sr_basic", "foo\n").await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, ">>");
    assert_eq!(lines(&rpc).await, vec!["    foo"]);
}

#[tokio::test]
async fn shift_left_dedents_current_line_by_one_shiftwidth() {
    let (rpc, _i) = start_seeded("sl_basic", "        foo\n").await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "<lt><lt>");
    assert_eq!(lines(&rpc).await, vec!["    foo"]);
}

#[tokio::test]
async fn shift_left_clamps_at_column_zero() {
    // `<<` on an indent shallower than one shiftwidth bottoms out at 0, never
    // negative — vim's behavior.
    let (rpc, _i) = start_seeded("sl_clamp", "  foo\n").await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "<lt><lt>");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
    // A second `<<` on the already-flush line is a no-op.
    feed(&rpc, "<lt><lt>");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn shift_right_stacks() {
    let (rpc, _i) = start_seeded("sr_stack", "foo\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, ">>>>");
    assert_eq!(lines(&rpc).await, vec!["    foo"]);
}

#[tokio::test]
async fn count_before_doubled_shift_covers_several_lines() {
    let (rpc, _i) = start_seeded("sr_count", "a\nb\nc\nd\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, "3>>");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b", "  c", "d"]);
}

#[tokio::test]
async fn shift_settles_cursor_on_first_non_blank() {
    let (rpc, _i) = start_seeded("sr_cursor", "a\nb\nc\n").await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "3>>");
    // Cursor on line 1 (1-based), at the first non-blank — column 4 after a
    // 4-space indent.
    assert_eq!(cursor(&rpc).await, (1, 4));
}

// ===== `>{motion}` ===========================================================

#[tokio::test]
async fn shift_with_a_linewise_motion() {
    let (rpc, _i) = start_seeded("sr_motion", "a\nb\nc\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    // `>j` = shift this line and the next (linewise over `j`).
    feed(&rpc, ">j");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b", "c"]);
}

#[tokio::test]
async fn shift_to_end_of_file() {
    let (rpc, _i) = start_seeded("sr_G", "a\nb\nc\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, ">G");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b", "  c"]);
}

// ===== `>{textobject}` =======================================================

#[tokio::test]
async fn shift_a_paragraph_text_object() {
    let (rpc, _i) = start_seeded("sr_ip", "a\nb\n\nc\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    // `>ip` shifts the paragraph under the cursor (the first block), leaving the
    // blank separator and the second block alone.
    feed(&rpc, ">ip");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b", "", "c"]);
}

// ===== blank lines ===========================================================

#[tokio::test]
async fn shift_right_leaves_blank_lines_untouched() {
    let (rpc, _i) = start_seeded("sr_blank", "a\n\nb\n").await;
    feed(&rpc, ":set expandtab shiftwidth=4<CR>");
    feed(&rpc, "VG>");
    // The empty middle line stays empty — `>` never indents a blank line.
    assert_eq!(lines(&rpc).await, vec!["    a", "", "    b"]);
}

// ===== visual mode ===========================================================

#[tokio::test]
async fn visual_line_shift_right() {
    let (rpc, _i) = start_seeded("vis_sr", "a\nb\nc\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, "Vj>");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b", "c"]);
}

#[tokio::test]
async fn visual_shift_exits_to_normal_mode() {
    let (rpc, _i) = start_seeded("vis_exit", "a\nb\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, "Vj>");
    // Back in normal mode: a plain `x` deletes one char rather than extending a
    // selection.
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec![" a", "  b"]);
}

#[tokio::test]
async fn visual_count_multiplies_the_shift() {
    let (rpc, _i) = start_seeded("vis_count", "a\nb\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    // `V2>` shifts the line by two shiftwidths at once.
    feed(&rpc, "V2>");
    assert_eq!(lines(&rpc).await, vec!["    a", "b"]);
}

#[tokio::test]
async fn visual_shift_left() {
    let (rpc, _i) = start_seeded("vis_sl", "    a\n    b\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, "Vj<lt>");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b"]);
}

// ===== dot-repeat ============================================================

#[tokio::test]
async fn dot_repeats_doubled_shift() {
    let (rpc, _i) = start_seeded("dot_sr", "foo\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    feed(&rpc, ">>");
    feed(&rpc, ".");
    assert_eq!(lines(&rpc).await, vec!["    foo"]);
}

#[tokio::test]
async fn dot_repeats_visual_shift_on_same_extent() {
    let (rpc, _i) = start_seeded("dot_vis", "a\nb\nc\nd\n").await;
    feed(&rpc, ":set expandtab shiftwidth=2<CR>");
    // Visual-shift the first two lines, then `.` on line 3 re-shifts the same
    // two-line extent from there.
    feed(&rpc, "Vj>");
    feed(&rpc, "jj.");
    assert_eq!(lines(&rpc).await, vec!["  a", "  b", "  c", "  d"]);
}

// ===== tabs / shiftwidth resolution ==========================================

#[tokio::test]
async fn shift_right_without_expandtab_uses_a_tab() {
    let (rpc, _i) = start_seeded("sr_tab", "foo\n").await;
    // noexpandtab (the default), shiftwidth follows tabstop=8.
    feed(&rpc, ":set noexpandtab tabstop=8 shiftwidth=8<CR>");
    feed(&rpc, ">>");
    assert_eq!(lines(&rpc).await, vec!["\tfoo"]);
}

#[tokio::test]
async fn shiftwidth_zero_follows_tabstop() {
    let (rpc, _i) = start_seeded("sr_sw0", "foo\n").await;
    // `shiftwidth=0` resolves to `tabstop`, so `>>` indents by 3 spaces here.
    feed(&rpc, ":set expandtab tabstop=3 shiftwidth=0<CR>");
    feed(&rpc, ">>");
    assert_eq!(lines(&rpc).await, vec!["   foo"]);
}
