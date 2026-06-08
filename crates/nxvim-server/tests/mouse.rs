//! Behavior tests for mouse support, driven the way a real client drives the
//! editor: a global screen cell goes in via `nvim_input_mouse`, and we assert on
//! the observable result (cursor position, focused window). The editor owns the
//! whole hit-test from cell to buffer position, so these tests exercise that
//! reverse mapping end-to-end.
//!
//! Phase 0 — the RPC + option plumbing: the `'mouse'` gate, and a malformed call
//! failing loud. Phase 1 — left-click places the cursor and focuses the window.
//! Phase 2 — left-drag makes a charwise Visual selection. Phase 3 — multi-click
//! escalates the unit (double = word, triple = line), timed by `'mousetime'`
//! against a fake clock.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, drain_to_latest_redraw, feed, feed_mouse, feed_mouse_at, lines,
    message, mode, spawn, write_temp, TestClock,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server with a temp file of `content` open in the first window, a
/// 80×24 UI attached. Returns the client and the incoming-notification channel.
async fn start(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("mouse", "txt", content);
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Like [`start`], but inject a fake mouse clock so multi-click timing is
/// deterministic. Returns the [`TestClock`] alongside the client and the incoming
/// channel (which the test must keep alive); advance the clock with
/// [`feed_mouse_at`] to place clicks inside or outside `'mousetime'`.
async fn start_clocked(content: &str) -> (Rpc, TestClock, UnboundedReceiver<Incoming>) {
    let path = write_temp("mouse", "txt", content);
    let clock = TestClock::new();
    let init = ServerInit {
        file: Some(path),
        mouse_clock: Some(clock.handle()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, clock, incoming)
}

/// Send a left-press with the `modifier` string (e.g. `"S"` for shift) — the
/// modifier-carrying gesture `feed_mouse` doesn't cover. Fire-and-forget; pair
/// with a barrier read to observe the effect.
fn shift_press(rpc: &Rpc, row: usize, col: usize) {
    rpc.notify(
        "nvim_input_mouse",
        vec![
            Value::from("left"),
            Value::from("press"),
            Value::from("S"),
            Value::from(0u64),
            Value::from(row as u64),
            Value::from(col as u64),
        ],
    );
}

/// The focused window's id (`nvim_get_current_win`).
async fn current_win(rpc: &Rpc) -> u64 {
    rpc.request("nvim_get_current_win", vec![])
        .await
        .expect("get_current_win")
        .as_u64()
        .expect("win id")
}

// ===== Phase 0: RPC + option plumbing =======================================

/// With `'mouse'` not enabling the current mode, a click is a silent no-op (the
/// cursor doesn't move) — vim-faithful gating, not an error.
#[tokio::test]
async fn mouse_gate_disabled_ignores_click() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set mouse=").await; // disable mouse in every mode
    command(&rpc, "set nonumber norelativenumber").await;
    assert_eq!(cursor(&rpc).await, (1, 0));
    feed_mouse(&rpc, "left", "press", 2, 3);
    assert_eq!(cursor(&rpc).await, (1, 0), "click ignored while mouse off");
}

/// A malformed `nvim_input_mouse` call (unknown action for the button) is
/// rejected loudly at the boundary, never silently coerced.
#[tokio::test]
async fn unknown_mouse_action_errors() {
    let (rpc, _incoming) = start("hello").await;
    let bad = rpc
        .request(
            "nvim_input_mouse",
            vec![
                Value::from("left"),
                Value::from("up"), // "up" is a wheel direction, not a button action
                Value::from(""),
                Value::from(0u64),
                Value::from(0u64),
                Value::from(0u64),
            ],
        )
        .await;
    assert!(bad.is_err(), "left+up must be rejected, got {bad:?}");
}

/// The `'mousetime'` option round-trips through `:set` — proving the new option
/// is wired into the parse/apply/query path even before any phase reads it.
#[tokio::test]
async fn mousetime_option_roundtrips() {
    let (rpc, mut incoming) = start("hello").await;
    command(&rpc, "set mousetime=250").await;
    command(&rpc, "set mousetime?").await;
    let map = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw");
    assert_eq!(message(&map), "mousetime=250");
}

// ===== Phase 1: left click places the cursor & focuses the window ============

/// Left-click in the text body moves the cursor to the clicked cell. With the
/// number gutter off, a global cell maps straight to (line, byte col).
#[tokio::test]
async fn left_click_moves_cursor() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 1, 3);
    // Row 1 → line 2 (1-based); col 3 → byte 3.
    assert_eq!(cursor(&rpc).await, (2, 3));
}

/// Clicking past the end of a line lands on its last character (Normal mode
/// can't sit on the trailing newline) — the cursor clamp does this for free.
#[tokio::test]
async fn left_click_past_eol_lands_on_last_char() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 2, 40); // far past "third" (len 5)
    assert_eq!(cursor(&rpc).await, (3, 4)); // 'd', the last char
}

/// A click in the number gutter places the cursor at the start of that line, not
/// in the text — the gutter is click-through to column 0.
#[tokio::test]
async fn left_click_in_gutter_lands_col0() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    // Default gutter (number + relativenumber) is 4 cells for this small buffer.
    feed_mouse(&rpc, "left", "press", 1, 2); // col 2 is inside the gutter
    assert_eq!(cursor(&rpc).await, (2, 0));
    // And col 4+3 = 7 lands 3 cells into the text of line 2 ("second line").
    feed_mouse(&rpc, "left", "press", 1, 7);
    assert_eq!(cursor(&rpc).await, (2, 3));
}

/// Tabs are accounted for: a screen column over a tab-indented line maps back to
/// the correct byte, using the buffer's `tabstop` (default 4).
#[tokio::test]
async fn left_click_respects_tab_expansion() {
    let (rpc, _incoming) = start("\tend\nsecond\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    // Line 0 is "\tend": tab spans screen cols 0..4, then e=4, n=5, d=6.
    // Clicking screen col 5 lands on 'n', byte offset 2 ("\t","e","n").
    feed_mouse(&rpc, "left", "press", 0, 5);
    assert_eq!(cursor(&rpc).await, (1, 2));
}

/// A click in another split focuses that window (focus follows the click) and
/// places the cursor there — the hit-test resolves the right window first.
#[tokio::test]
async fn left_click_focuses_other_split() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    // Vertical split: the new (left) window takes focus; the original is on the
    // right. Left width 40 (cols 0..40), separator at 40, right at cols 41..80.
    feed(&rpc, "<C-w>v");
    let left = current_win(&rpc).await;
    feed_mouse(&rpc, "left", "press", 2, 45); // col 45 is in the right window
    let now = current_win(&rpc).await;
    assert_ne!(now, left, "focus moved to the clicked window");
    // rel col = 45 - 41 = 4 → byte 4 of "third" = 'd'; row 2 → line 3.
    assert_eq!(cursor(&rpc).await, (3, 4));
}

// ===== Phase 2: left drag → charwise Visual =================================

/// Press then drag enters charwise Visual anchored at the press point and
/// extends to the drag cell — the selection is inclusive of the char under the
/// cursor, so deleting it removes the whole dragged-over run.
#[tokio::test]
async fn drag_enters_visual_and_selects() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    assert_eq!(mode(&rpc).await, "n", "a bare press doesn't start Visual");
    feed_mouse(&rpc, "left", "drag", 0, 4);
    assert_eq!(mode(&rpc).await, "v", "the first drag enters Visual");
    assert_eq!(cursor(&rpc).await, (1, 4));
    // Selection is cols 0..=4 ("hello"); deleting it leaves " world".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], " world");
}

/// A charwise drag selection spans lines; deleting the inclusive range joins the
/// surviving heads and tails.
#[tokio::test]
async fn drag_selects_across_lines() {
    let (rpc, _incoming) = start("abc\ndef\nghi").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 1); // 'b' on line 1
    feed_mouse(&rpc, "left", "drag", 2, 1); // 'h' on line 3
    assert_eq!(mode(&rpc).await, "v");
    // Inclusive [(0,1)..(2,1)] deletes "bc\ndef\ngh", leaving "a" + "i".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["ai"]);
}

/// Releasing after a drag keeps the Visual selection (vim doesn't drop it on
/// button-up); a later motion still extends from the same anchor.
#[tokio::test]
async fn drag_release_keeps_visual() {
    let (rpc, _incoming) = start("hello world").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 0, 2);
    feed_mouse(&rpc, "left", "release", 0, 2);
    assert_eq!(mode(&rpc).await, "v", "selection survives the release");
}

/// A bare click (press then release, no drag) leaves you in Normal mode — only a
/// drag starts a selection.
#[tokio::test]
async fn click_without_drag_stays_normal() {
    let (rpc, _incoming) = start("hello world").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 6);
    feed_mouse(&rpc, "left", "release", 0, 6);
    assert_eq!(mode(&rpc).await, "n");
    assert_eq!(cursor(&rpc).await, (1, 6));
}

/// A left-press while a (keyboard-started) Visual selection is active stops it
/// and places the cursor at the click — vim's `<LeftMouse>` ends Visual.
#[tokio::test]
async fn press_cancels_active_visual() {
    let (rpc, _incoming) = start("hello world").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "vll"); // keyboard Visual, cursor at col 2
    assert_eq!(mode(&rpc).await, "v");
    feed_mouse(&rpc, "left", "press", 0, 8);
    assert_eq!(mode(&rpc).await, "n", "the click ended Visual");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

// ===== Phase 3: multi-click — double=word, triple=line ======================

/// Two presses at the same cell within `'mousetime'` count as a double-click and
/// select the word under the pointer (charwise Visual), like vim. `mousetime` is
/// driven off the injected fake clock, so the two clicks are exactly 100 ms apart.
#[tokio::test]
async fn double_click_selects_word() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    // Click twice on 'o' (col 7) of "world" within mousetime → select "world".
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n", "the first press is a single click");
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "v", "the second press is a double-click");
    // The selection covers "world" (cols 6..=10); deleting it leaves "hello ".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], "hello ");
}

/// Two presses at the same cell *more* than `'mousetime'` apart are two separate
/// single clicks — no word selection, the editor stays in Normal mode.
#[tokio::test]
async fn slow_second_click_is_not_a_double() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n");
    // 600 ms > mousetime=500 → the second press is a fresh single click.
    feed_mouse_at(&rpc, &clock, 600, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n", "too slow to be a double-click");
    assert_eq!(cursor(&rpc).await, (1, 7));
}

/// A second click within mousetime but at a *different* cell is not a double —
/// multi-click requires the same screen cell, so it stays a single click.
#[tokio::test]
async fn second_click_elsewhere_is_not_a_double() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n");
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 1, 2); // different cell
    assert_eq!(mode(&rpc).await, "n", "a different cell resets the count");
    assert_eq!(cursor(&rpc).await, (2, 2));
}

/// Three presses at the same cell within mousetime escalate to a triple-click,
/// which selects the whole line (linewise Visual).
#[tokio::test]
async fn triple_click_selects_line() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 1, 3);
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 1, 3);
    feed_mouse_at(&rpc, &clock, 200, "left", "press", 1, 3);
    assert_eq!(mode(&rpc).await, "V", "the third press is a triple-click");
    // Deleting the linewise selection removes the whole 2nd line.
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["hello world", "third"]);
}

/// Dragging after a double-click extends the selection by whole words, not by
/// single characters — the unit chosen by the click count carries into the drag.
#[tokio::test]
async fn word_wise_drag_extends_by_words() {
    let (rpc, clock, _incoming) = start_clocked("alpha beta gamma delta").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    // Double-click "beta" (cols 6..=9), then drag onto "gamma" (col 13).
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 7);
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "v");
    feed_mouse(&rpc, "left", "drag", 0, 13);
    // The selection now spans whole words "beta gamma" (cols 6..=15); deleting it
    // leaves "alpha " + " delta".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], "alpha  delta");
}

/// A triple-click then drag onto another line extends the linewise selection a
/// whole line at a time.
#[tokio::test]
async fn line_wise_drag_extends_by_lines() {
    let (rpc, clock, _incoming) = start_clocked("one\ntwo\nthree\nfour").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 1);
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 0, 1);
    feed_mouse_at(&rpc, &clock, 200, "left", "press", 0, 1);
    assert_eq!(mode(&rpc).await, "V");
    feed_mouse(&rpc, "left", "drag", 2, 0); // drag down to "three"
                                            // Lines 1..=3 ("one","two","three") are selected; deleting leaves "four".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["four"]);
}

// ===== Shift-click extends the selection (`<S-LeftMouse>`) ===================

/// A shift+left-click with no selection active starts a charwise Visual from the
/// cursor's current position to the click — vim's `<S-LeftMouse>` extend gesture.
#[tokio::test]
async fn shift_click_starts_selection_to_click() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0); // cursor at (1,0), no selection
    assert_eq!(mode(&rpc).await, "n");
    shift_press(&rpc, 0, 4); // shift-click on col 4
    assert_eq!(mode(&rpc).await, "v", "shift-click enters Visual");
    assert_eq!(cursor(&rpc).await, (1, 4));
    // Selection covers cols 0..=4 ("hello"); deleting it leaves " world".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], " world");
}

/// A shift+left-click while a Visual selection is active moves the live end to
/// the click, keeping the anchor — extending (or shrinking) the selection.
#[tokio::test]
async fn shift_click_extends_active_visual() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "vll"); // keyboard Visual, anchor (1,0), cursor (1,2)
    assert_eq!(mode(&rpc).await, "v");
    shift_press(&rpc, 0, 10); // extend out to col 10
    assert_eq!(cursor(&rpc).await, (1, 10));
    // Anchor stayed at col 0; selection now 0..=10 ("hello world").
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], "");
}

/// Shift-click extends backward too: clicking before the anchor flips which end
/// the cursor leads, the anchor still pinned at its original spot.
#[tokio::test]
async fn shift_click_extends_backward() {
    let (rpc, _incoming) = start("hello world").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "$"); // cursor to 'd' (col 10)
    feed_mouse(&rpc, "left", "press", 0, 6); // place cursor at 'w' (col 6)
    shift_press(&rpc, 0, 0); // shift-click back to col 0
    assert_eq!(mode(&rpc).await, "v");
    assert_eq!(cursor(&rpc).await, (1, 0));
    // Anchor at col 6, cursor at col 0 → selection 0..=6 ("hello w").
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], "orld");
}

/// A shift+left-click while a *linewise* Visual is active extends it line-wise,
/// staying in VisualLine — the click's column is ignored.
#[tokio::test]
async fn shift_click_extends_linewise_visual() {
    let (rpc, _incoming) = start("one\ntwo\nthree\nfour").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "V"); // linewise Visual on line 1
    assert_eq!(mode(&rpc).await, "V");
    shift_press(&rpc, 2, 1); // shift-click on line 3
    assert_eq!(mode(&rpc).await, "V", "stays linewise");
    // Lines 1..=3 selected; deleting leaves "four".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["four"]);
}

/// After a shift-click extends a selection, a plain drag keeps extending it
/// charwise from the same anchor.
#[tokio::test]
async fn drag_after_shift_click_keeps_extending() {
    let (rpc, _incoming) = start("hello world").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0); // cursor at col 0
    shift_press(&rpc, 0, 4); // shift-click extends to col 4
    assert_eq!(cursor(&rpc).await, (1, 4));
    feed_mouse(&rpc, "left", "drag", 0, 8); // drag continues to col 8
    assert_eq!(cursor(&rpc).await, (1, 8));
    // Anchor still col 0 → selection 0..=8 ("hello wor").
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await[0], "ld");
}
