//! Helix editing model — Phase 2: `Mode::HelixNormal` + range-returning motions.
//!
//! Helix is selection-first: every cursor is a persistent `anchor..head` range,
//! and a motion *re-selects* on each keystroke (no operator-pending wait). These
//! tests drive the opt-in mode (entered with `:helix`) and assert on the rendered
//! selection span the same way the multicursor suite does — plus the cursor head
//! and the reported `mode()` code. This file covers navigation + the
//! move-and-select vs. extend distinction; the verbs live in `helix_verbs.rs`
//! and its siblings.

use crate::support::*;

/// `:helix` enters the selection-first normal mode; `mode()` reports its own code.
#[tokio::test]
async fn helix_command_enters_and_leaves_the_mode() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>0");
    assert_eq!(mode(&rpc).await, "n", "starts in vim normal mode");

    feed(&rpc, ":helix<CR>");
    assert_eq!(mode(&rpc).await, "hn", "`:helix` entered Helix normal mode");

    // The toggle leaves back to vim normal mode.
    feed(&rpc, ":helix<CR>");
    assert_eq!(mode(&rpc).await, "n", "`:helix` toggled back out");
}

/// A word motion *selects*: the anchor stays at the old head and the head moves to
/// the motion target, so `w` selects from the cursor across the word.
#[tokio::test]
async fn word_motion_selects_the_range() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "w").await;
    // Helix `w` selects the word plus its trailing whitespace, stopping *before* the
    // next word: the head lands on the space (col 5), not on "world"'s `w` — the
    // selection is "hello " (screen cols [0, 6)), unlike vim's `w` which lands on col 6.
    assert_eq!(
        cursor(&rpc).await,
        (1, 5),
        "head stopped on the space before the next word"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "the selection covers the word and its trailing whitespace",
    );
}

/// Helix `w` on the *final* word of the buffer has no next word to stop before, so it
/// selects through to the last char (no off-by-one trim) — the clamp case.
#[tokio::test]
async fn word_motion_on_last_word_selects_to_end() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (1, 4), "head on the last char `o`");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 5)),
        "the whole final word stays selected",
    );
}

/// Repeated `w` walks word-by-word, re-anchoring each time — it must NOT get stuck
/// on the trailing space. (Regression: reusing vim's `w` collapsed the second `w`
/// onto the space it started from.) "jumps over the lazy": w → "jumps ", w → "over ".
#[tokio::test]
async fn repeated_word_motion_walks_forward() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ijumps over the lazy<Esc>0:helix<CR>");

    // First `w` selects "jumps " (cols 0..5, head on the space at col 5).
    let m1 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (1, 5), "1st w: head on the space");
    assert_eq!(
        view_selection(&m1).first().copied().flatten(),
        Some((0, 6)),
        "1st w selected \"jumps \"",
    );

    // Second `w` re-anchors on "over" and selects "over " — not stuck on the space.
    let m2 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (1, 10), "2nd w: head on the next space");
    assert_eq!(
        view_selection(&m2).first().copied().flatten(),
        Some((6, 11)),
        "2nd w re-anchored and selected \"over \"",
    );
}

/// `w` never gets stuck on adjacent runs of a different class: on "practice on.",
/// once `w` has selected "on" a further `w` advances onto the "." (a punctuation
/// word), rather than collapsing onto the "n". (Regression: with no whitespace before
/// the next word, the "head just before the next word" landed on the current char.)
#[tokio::test]
async fn word_motion_advances_across_adjacent_punctuation() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ipractice on.<Esc>0:helix<CR>");

    // `w` `w` → "practice " then "on" (cols 9..10, head on the `n`).
    feed(&rpc, "ww");
    assert_eq!(cursor(&rpc).await, (1, 10), "second w selected \"on\"");

    // A third `w` advances onto the "." — not stuck on the "n".
    let map = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 11),
        "w advanced onto the punctuation"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((11, 12)),
        "w selected the \".\"",
    );
}

/// A word motion never selects the line break, but when the line has no word left it
/// jumps to the next NON-EMPTY line (skipping blank lines) and selects a fresh word
/// there. On "foo" / "" / "bar": `w` selects "foo", a further `w` skips the empty line
/// and selects "bar" on line 3 — with the selection wholly on that line (no newline).
#[tokio::test]
async fn word_motion_jumps_to_next_nonempty_line() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR><CR>bar<Esc>gg0:helix<CR>");

    // `w` selects "foo" (cols 0..2 on line 1) — the newline is not part of it.
    let m1 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "w selected foo, head on its last char"
    );
    assert_eq!(
        view_selection(&m1).first().copied().flatten(),
        Some((0, 3)),
        "w selected \"foo\" only",
    );

    // A further `w` skips the empty line 2 and lands on "bar" (line 3), fresh.
    let m2 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (3, 2), "w jumped to bar on line 3");
    let sel = view_selection(&m2);
    assert_eq!(
        sel.first().copied().flatten(),
        None,
        "the selection is not on line 1 anymore",
    );
    assert_eq!(
        sel.get(2).copied().flatten(),
        Some((0, 3)),
        "the fresh selection is \"bar\" on line 3 — no newline spanned",
    );
}

/// A line's leading whitespace is its own word: `w` onto an indented line selects the
/// indentation first (Helix's rule), and a further `w` takes the actual word. On "foo"
/// / "    bar": w → "foo", w → "    " (the indent), w → "bar".
#[tokio::test]
async fn word_motion_treats_leading_indent_as_a_word() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>    bar<Esc>gg0:helix<CR>");

    // `w` → "foo" on line 1.
    feed(&rpc, "w");
    // `w` crosses onto line 2 and selects the 4-space indentation (cols 0..3).
    let m1 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(
        cursor(&rpc).await,
        (2, 3),
        "w selected the indentation, head on its last space"
    );
    assert_eq!(
        view_selection(&m1).get(1).copied().flatten(),
        Some((0, 4)),
        "the leading whitespace is selected as its own word",
    );

    // A further `w` takes the actual word "bar" (cols 4..6).
    let m2 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (2, 6), "w then selected bar");
    assert_eq!(
        view_selection(&m2).get(1).copied().flatten(),
        Some((4, 7)),
        "the actual word follows the indentation",
    );
}

/// Leading whitespace behaves like a real word even when the cursor is already *inside*
/// it (e.g. after a `b` landed there): `w` from the start of the indentation selects the
/// indentation (head on its last space), then a further `w` takes the first word — it
/// does NOT skip straight to the word's end. (Regression: `w` from col 0 jumped past
/// the indentation to the word's last char.)
#[tokio::test]
async fn word_motion_from_inside_indent_selects_the_indent() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i    bar<Esc>0:helix<CR>"); // "    bar", cursor at col 0

    // `w` from the start of the indentation selects the 4 spaces (cols 0..3).
    let m1 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 3),
        "w selected the indentation, head on its last space — not the word's end"
    );
    assert_eq!(
        view_selection(&m1).first().copied().flatten(),
        Some((0, 4)),
        "the indentation is selected as its own word",
    );

    // A further `w` takes "bar".
    let m2 = redraw_after(&rpc, &mut incoming, "w").await;
    assert_eq!(cursor(&rpc).await, (1, 6), "then w selected bar");
    assert_eq!(
        view_selection(&m2).first().copied().flatten(),
        Some((4, 7)),
        "the word follows the indentation",
    );
}

/// `b` mirrors the jump: from the first word of a line it crosses back to the last
/// word of the previous non-empty line (skipping blanks).
#[tokio::test]
async fn back_word_jumps_to_previous_nonempty_line() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR><CR>bar<Esc>0:helix<CR>");
    // Cursor starts on "bar"'s `b` (line 3, col 0). `b` crosses back over the empty
    // line to "foo" on line 1.
    let map = redraw_after(&rpc, &mut incoming, "b").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "b jumped back to foo's start on line 1"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 3)),
        "b selected \"foo\" on the previous non-empty line",
    );
}

/// `b` selects backward to the previous word start, *excluding* the char the cursor
/// started on. (Regression: it kept the old head as an inclusive anchor, so `b` from
/// "over"'s `o` wrongly selected "jumps o".)
#[tokio::test]
async fn back_word_excludes_the_starting_char() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ijumps over the lazy<Esc>0:helix<CR>");

    // Put the head on "over"'s `o` (col 6), then `b`.
    let map = redraw_after(&rpc, &mut incoming, "6lb").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "head moved back to the word start"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "b selected \"jumps \" — not \"jumps o\"",
    );
}

/// `e` off a word end captures the *leading* whitespace + next word (Helix's `e`);
/// `e` from the start of a word selects just that word (no stray earlier char).
#[tokio::test]
async fn end_word_captures_leading_whitespace() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ijumps over the lazy<Esc>0:helix<CR>");

    // First `e` from the start selects "jumps" (cols 0..4) — no leading space.
    let m1 = redraw_after(&rpc, &mut incoming, "e").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 4),
        "1st e: head on jumps' last char"
    );
    assert_eq!(
        view_selection(&m1).first().copied().flatten(),
        Some((0, 5)),
        "1st e selected \"jumps\"",
    );

    // Second `e` starts off the word end (`s`), so it selects the SPACE + "over"
    // (cols 5..9) — the leading whitespace is part of the selection, not a stray `s`.
    let m2 = redraw_after(&rpc, &mut incoming, "e").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 9),
        "2nd e: head on over's last char"
    );
    assert_eq!(
        view_selection(&m2).first().copied().flatten(),
        Some((5, 10)),
        "2nd e selected \" over\" (leading space included) — not \"over\" or \"s over\"",
    );
}

/// Alternating a forward word motion with `b` keeps the last character — `b` must
/// not drop the char the forward motion landed on. After `w` → "jumps " (head on the
/// space), `b` returns "jumps " (the space is kept); after `e` → "jumps" (head on
/// `s`), `b` returns "jumps" (the `s` is kept).
#[tokio::test]
async fn back_word_after_forward_keeps_last_char() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ijumps over the lazy<Esc>0:helix<CR>");

    // `w` → "jumps " (head on the space at col 5), then `b` back to the start.
    feed(&rpc, "w");
    let m1 = redraw_after(&rpc, &mut incoming, "b").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "b landed on the word start");
    assert_eq!(
        view_selection(&m1).first().copied().flatten(),
        Some((0, 6)),
        "b kept the trailing space → \"jumps \", not \"jumps\"",
    );

    // From the start again: `e` → "jumps" (head on `s`), then `b` keeps the `s`.
    feed(&rpc, "0e");
    let m2 = redraw_after(&rpc, &mut incoming, "b").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "b landed on the word start");
    assert_eq!(
        view_selection(&m2).first().copied().flatten(),
        Some((0, 5)),
        "b kept the `s` → \"jumps\"",
    );
}

/// A plain character motion *collapses*: the range becomes a 1-wide block at the
/// target — no growing selection in Helix normal mode.
#[tokio::test]
async fn char_motion_collapses_to_a_block() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "ll").await;
    assert_eq!(cursor(&rpc).await, (1, 2), "moved two cells right");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((2, 3)),
        "the selection is a lone 1-wide block at the head, not a grown range",
    );
}

/// `v` enters select (extend) mode, where *every* motion — including a plain
/// character motion — moves only the head and keeps the anchor, growing the range.
#[tokio::test]
async fn select_mode_extends_on_every_motion() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    feed(&rpc, "v");
    assert_eq!(mode(&rpc).await, "hs", "`v` entered Helix select mode");

    // Extend three cells right: anchor pinned at col 0, head at col 3.
    let map = redraw_after(&rpc, &mut incoming, "lll").await;
    assert_eq!(cursor(&rpc).await, (1, 3), "head advanced under extend");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 4)),
        "the anchor stayed put and the selection grew to the head",
    );
}

/// A count applies to a Helix motion (`3l` moves three cells), and `<Esc>` collapses
/// the selection back to a point at the head without leaving Helix mode.
#[tokio::test]
async fn count_applies_and_escape_collapses() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    // Grow a selection with a counted extend, then collapse it.
    feed(&rpc, "v3l");
    assert_eq!(
        cursor(&rpc).await,
        (1, 3),
        "counted extend moved three cells"
    );

    feed(&rpc, "<Esc>");
    assert_eq!(
        mode(&rpc).await,
        "hn",
        "<Esc> from select returned to Helix normal"
    );
    // A second <Esc> in Helix normal collapses the range to a block at the head.
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((3, 4)),
        "<Esc> collapsed the selection to a 1-wide block at the head",
    );
}

/// A find motion (`f{char}`) selects up to and including the found character —
/// mutation guard: a broken find target would move the head elsewhere.
#[tokio::test]
async fn find_motion_selects_to_target() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    // `fw` finds 'w' (col 6); the selection covers [0, 7) inclusive of the target.
    let map = redraw_after(&rpc, &mut incoming, "fw").await;
    assert_eq!(cursor(&rpc).await, (1, 6), "find landed the head on 'w'");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 7)),
        "the selection covers from the origin through the found char",
    );
}

/// `W` is a real WORD motion: a run of any non-blank chars (word + punctuation
/// classes merged) is one word, so on "foo.bar baz" a single `W` selects
/// "foo.bar " — where `w` selects only "foo".
#[tokio::test]
async fn big_word_motion_selects_across_punctuation() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo.bar baz<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "W").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 7),
        "W's head stopped on the space before baz"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 8)),
        "W selected the whole punctuated WORD plus its trailing whitespace",
    );

    // `w` from the same origin is the small-word contrast: just "foo".
    let m2 = redraw_after(&rpc, &mut incoming, "0<Esc>w").await;
    assert_eq!(
        view_selection(&m2).first().copied().flatten(),
        Some((0, 3)),
        "small-word w selected only \"foo\"",
    );
}

/// `E` lands the head on the WORD's last char (crossing the punctuation); `B`
/// from the following word crosses the whole WORD back to its start.
#[tokio::test]
async fn big_word_end_and_back_cross_punctuation() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo.bar baz<Esc>0:helix<CR>");

    let m1 = redraw_after(&rpc, &mut incoming, "E").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 6),
        "E landed on the WORD's last char"
    );
    assert_eq!(
        view_selection(&m1).first().copied().flatten(),
        Some((0, 7)),
        "E selected through the punctuated WORD",
    );

    // Put the head on "baz" (the 2nd `b`, col 8), then `B` crosses back to col 0.
    let m2 = redraw_after(&rpc, &mut incoming, "<Esc>02fbB").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "B crossed the WORD to its start"
    );
    assert_eq!(
        view_selection(&m2).first().copied().flatten(),
        Some((0, 8)),
        "B selected back across the whole WORD",
    );
}

// ----- viewport scrolling (`<C-d>`/`<C-u>`/`<C-f>`/`<C-b>`, PageUp/PageDown) -----
// Helix scrolls exactly like vim; these were inert in Helix mode until wired.

/// `<C-d>` scrolls a half page down (viewport height 24 → 12), like vim.
#[tokio::test]
async fn ctrl_d_scrolls_half_page_in_helix() {
    let path = write_n_lines("hxcd", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>");

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;
    assert_eq!(scroll_u64(&map, "to_top"), 12, "view scrolled half a page");
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        12,
        "cursor followed the scroll"
    );
}

/// `<C-f>` scrolls a full page down (height 24 → page = 22); `<C-b>` scrolls back.
#[tokio::test]
async fn ctrl_f_and_ctrl_b_scroll_full_page_in_helix() {
    let path = write_n_lines("hxcf", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>");

    let map = scroll_after(&rpc, &mut incoming, "<C-f>").await;
    assert_eq!(scroll_u64(&map, "to_top"), 22, "full page forward");
    assert_eq!(scroll_u64(&map, "to_cursor"), 22);

    let back = scroll_after(&rpc, &mut incoming, "<C-b>").await;
    assert_eq!(scroll_u64(&back, "from_top"), 22);
    assert_eq!(scroll_u64(&back, "to_top"), 0, "full page back to the top");
}

/// PageDown scrolls a half page in Helix, like `<C-d>` (matching vim).
#[tokio::test]
async fn page_down_scrolls_in_helix() {
    let path = write_n_lines("hxpg", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>");

    let map = scroll_after(&rpc, &mut incoming, "<PageDown>").await;
    assert_eq!(
        scroll_u64(&map, "to_top"),
        12,
        "PageDown scrolled a half page"
    );
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
}

// ----- the view menu (`z` — `zt`/`zz`/`zb`) ----------------------------------
// `z` repositions the viewport around the cursor line without moving the cursor
// (so the selection is untouched), reusing the same `view_reposition` as vim.

/// `zz` centers the cursor's line; the cursor (and selection) stays put.
#[tokio::test]
async fn zz_centers_the_cursor_line_in_helix() {
    let path = write_n_lines("hxzz", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>");

    // `50G` lands the cursor on line 50 (index 49); `zz` centers it (49 - 24/2).
    let _ = scroll_after(&rpc, &mut incoming, "50G").await;
    let map = scroll_after(&rpc, &mut incoming, "zz").await;
    assert_eq!(scroll_u64(&map, "to_top"), 37, "line 49 centered: 49 - 12");
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        49,
        "zz leaves the cursor put"
    );
    assert_eq!(cursor(&rpc).await, (50, 0), "the cursor stays on line 50");
}

/// `zt` puts the cursor's line at the top row; `zb` at the bottom.
#[tokio::test]
async fn zt_and_zb_reposition_the_viewport_in_helix() {
    let path = write_n_lines("hxzt", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>");

    let _ = scroll_after(&rpc, &mut incoming, "50G").await;
    let top = scroll_after(&rpc, &mut incoming, "zt").await;
    assert_eq!(scroll_u64(&top, "to_top"), 49, "zt: line 49 to the top row");
    assert_eq!(cursor(&rpc).await, (50, 0), "zt leaves the cursor put");

    // From centered, `zb` drops the line to the bottom row (49 + 1 - 24).
    let _ = scroll_after(&rpc, &mut incoming, "zz").await;
    let bot = scroll_after(&rpc, &mut incoming, "zb").await;
    assert_eq!(
        scroll_u64(&bot, "to_top"),
        26,
        "zb: line 49 to the bottom row"
    );
    assert_eq!(cursor(&rpc).await, (50, 0), "zb leaves the cursor put");
}

/// In select mode (`v`), a scroll *extends* the selection — the anchor holds at
/// the origin while the head follows the scroll down half a page.
#[tokio::test]
async fn scroll_extends_selection_in_select_mode() {
    let path = write_n_lines("hxsel", 100);
    let (rpc, _i) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>v<C-d>");

    // Anchor stayed on line 1 (1-based); head rode the half-page scroll to line 13.
    assert_eq!(cursor(&rpc).await, (13, 0), "head followed the scroll");
    // Deleting the (extended) selection removes line1..line12 plus the 'l' of line13
    // — proving the anchor held (a collapsed selection would delete just one char).
    feed(&rpc, "d");
    let ls = lines(&rpc).await;
    assert_eq!(
        ls.first().map(String::as_str),
        Some("ine13"),
        "lines 1-12 were part of the extended selection",
    );
    assert_eq!(ls.len(), 88, "twelve whole lines were deleted");
}

// A scroll's selection must ride the slide, not snap to its final extent. The
// redraw carries `sel_extends_down` so the client clips the selection to the
// interpolated cursor — it was `None` for Helix (the snap the user saw).

/// Select mode: an extending selection scrolling down reports `extends_down`.
#[tokio::test]
async fn scroll_gesture_clips_the_extending_selection_in_select_mode() {
    let path = write_n_lines("hxgd", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>v");

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;
    assert_eq!(
        scroll_sel_extends_down(&map),
        Some(true),
        "the growing selection is clipped downward, not snapped to its full extent",
    );
}

/// Normal mode: the collapsed selection is a moving point — the redraw still
/// carries a `sel_extends_down` (not `None`) so the 1-wide block is hidden during
/// the slide (it reappears on settle) instead of jumping ahead of the cursor.
#[tokio::test]
async fn scroll_gesture_hides_the_collapsed_selection_in_normal_mode() {
    let path = write_n_lines("hxgn", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":helix<CR>");

    // Down: the block sits at the destination (below the sliding cursor) → clipped
    // as "past" the cursor while scrolling down.
    let down = scroll_after(&rpc, &mut incoming, "<C-d>").await;
    assert_eq!(
        scroll_sel_extends_down(&down),
        Some(true),
        "collapsed selection is clipped during a downward scroll (not None)",
    );

    // Up: the block sits above the sliding cursor → clipped the other way.
    let up = scroll_after(&rpc, &mut incoming, "<C-u>").await;
    assert_eq!(
        scroll_sel_extends_down(&up),
        Some(false),
        "collapsed selection is clipped during an upward scroll",
    );
}

// ----- cross-line find (`f`/`F`/`t`/`T`) -----------------------------------------
// Unlike vim, Helix's find motions are NOT confined to the current line — they
// match the next occurrence anywhere in the document.

/// `f{char}` finds the target on a *later* line (vim's `f` would miss and stay put).
#[tokio::test]
async fn find_forward_crosses_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>baz<Esc>gg0:helix<CR>");

    // From line 1, `fz` reaches the 'z' on line 3 (col 2).
    feed(&rpc, "fz");
    assert_eq!(cursor(&rpc).await, (3, 2), "f crossed two lines to the 'z'");
}

/// `F{char}` finds the target on an *earlier* line, scanning backward across lines.
#[tokio::test]
async fn find_backward_crosses_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>baz<Esc>G$:helix<CR>");

    // From the end (line 3), `Ff` reaches the 'f' on line 1 (col 0).
    feed(&rpc, "Ff");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "F crossed back two lines to the 'f'"
    );
}

/// `t{char}` stops one grapheme short of a target found on a later line.
#[tokio::test]
async fn till_forward_crosses_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>xbar<Esc>gg0:helix<CR>");

    // 'b' is on line 2 at col 1; `tb` stops one short → the 'x' at line 2 col 0.
    feed(&rpc, "tb");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "t landed just before the 'b' on line 2"
    );
}

/// A counted find crosses lines to reach the count-th occurrence.
#[tokio::test]
async fn counted_find_crosses_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifo<CR>fo<Esc>gg0:helix<CR>");

    // 'o' at line1 col1 and line2 col1; `2fo` reaches the second one on line 2.
    feed(&rpc, "2fo");
    assert_eq!(
        cursor(&rpc).await,
        (2, 1),
        "2f reached the second 'o', on line 2"
    );
}
