//! Tier 1: caret cell placement for the command line and the picker prompt.
//!
//! `cmdline_cursor` / `query_cursor` are **char offsets** on the wire
//! (bemtvi-core's `cmdline_cursor()` / `cursor_chars()`), and the server measures
//! the `vim.ui.input` prompt by **display width**. The GUI paints those rows as
//! one shaped run in which a wide CJK/emoji char occupies two cells
//! (`set_monospace_width`), so the caret's screen cell must be the display width
//! of everything before it — counting chars lands the caret one cell short per
//! wide char. Black-box, no window, no GPU, like `keys.rs`.

use bemtvi_gui::{cmdline_caret_col, query_caret_col};

#[test]
fn ascii_cmdline_caret_is_prefix_plus_chars() {
    // With ASCII text char count == display width, so the old behavior holds:
    // the single-cell `:`/`/`/`?` prefix plus the chars before the caret.
    assert_eq!(cmdline_caret_col("", "", 0), 1);
    assert_eq!(cmdline_caret_col("", "abc", 0), 1);
    assert_eq!(cmdline_caret_col("", "abc", 2), 3);
    assert_eq!(cmdline_caret_col("", "abc", 3), 4);
    // A char offset past the end (defensive) clamps to the line's full width.
    assert_eq!(cmdline_caret_col("", "ab", 10), 3);
}

#[test]
fn wide_chars_before_the_cmdline_caret_take_two_cells() {
    // `:e 日本語` with the caret at end: cursor_chars = 5 chars into "e 日本語",
    // but the three CJK chars each occupy two cells in the shaped run — the
    // caret sits at 1 (prefix) + 2 (ASCII) + 3×2 = 9, not 1 + 5 = 6.
    assert_eq!(cmdline_caret_col("", "e 日本語", 5), 9);
    // Mid-line, only the chars before the caret count: "e 日" = 1 + 2 + 2 = 5.
    assert_eq!(cmdline_caret_col("", "e 日本語", 3), 5);
    // A zero-width combining mark adds no cell.
    assert_eq!(cmdline_caret_col("", "e\u{301}x", 2), 2);
}

#[test]
fn prompt_width_is_display_width_not_char_count() {
    // A CJK `vim.ui.input` prompt: "名前: " is 4 chars but 6 cells (the server
    // measures it with display_width; the GUI must agree or the caret starts
    // inside the prompt's glyphs).
    assert_eq!(cmdline_caret_col("名前: ", "", 0), 6);
    assert_eq!(cmdline_caret_col("名前: ", "ab", 2), 8);
    // An ASCII prompt is unchanged (width == chars).
    assert_eq!(cmdline_caret_col("Name: ", "x", 1), 7);
}

#[test]
fn picker_query_caret_counts_display_width() {
    // The `"> "` prefix is 2 cells; the query chars before the caret count at
    // display width, so a CJK query moves the caret two cells per char.
    assert_eq!(query_caret_col("", 0), 2);
    assert_eq!(query_caret_col("abc", 3), 5);
    // 2 (prefix) + 2×2 (two wide chars) — not the old 2 + 2 char count.
    assert_eq!(query_caret_col("日本", 2), 6);
    // 2 (prefix) + 1 (a) + 2 (日) — the caret after the wide char, mid-query.
    assert_eq!(query_caret_col("a日b", 2), 5);
    // Past-the-end clamps to the query's full width.
    assert_eq!(query_caret_col("日", 9), 4);
}
