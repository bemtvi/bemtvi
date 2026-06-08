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
//! against a fake clock. Phase 4 — the wheel scrolls the window under the pointer
//! (`'mousescroll'` step, `Shift` = page) without moving focus or the cursor.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, drain_to_latest_redraw, exec_lua, feed, feed_mouse, feed_mouse_at,
    lines, message, mode, spawn, temp_dir, write_temp, TestClock,
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

// ===== Phase 4: wheel scrolls the window under the pointer ===================

/// A buffer of `n` distinct lines (`line001`…), tall enough to scroll.
fn numbered(n: usize) -> String {
    (1..=n)
        .map(|i| format!("line{i:03}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Coerce a Lua-returned number `Value` to `u64` (it may arrive as int or float).
fn as_num(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| v.as_i64().map(|i| i as u64))
        .or_else(|| v.as_f64().map(|f| f as u64))
        .expect("number")
}

/// Window `win`'s 1-based topline (`winsaveview().topline`), read through
/// `nvim_win_call` so it works for an unfocused window too. Doubles as a barrier.
async fn win_topline(rpc: &Rpc, win: u64) -> u64 {
    let v = exec_lua(
        rpc,
        &format!(
            "return vim.api.nvim_win_call({win}, function() return vim.fn.winsaveview().topline end)"
        ),
    )
    .await;
    as_num(&v)
}

/// Window `win`'s `leftcol` (`winsaveview().leftcol`), via `nvim_win_call`.
async fn win_leftcol(rpc: &Rpc, win: u64) -> u64 {
    let v = exec_lua(
        rpc,
        &format!(
            "return vim.api.nvim_win_call({win}, function() return vim.fn.winsaveview().leftcol end)"
        ),
    )
    .await;
    as_num(&v)
}

/// Window `win`'s content height in rows (`nvim_win_get_height`) — the page size a
/// `Shift`+wheel notch scrolls.
async fn win_height(rpc: &Rpc, win: u64) -> u64 {
    as_num(&exec_lua(rpc, &format!("return vim.api.nvim_win_get_height({win})")).await)
}

/// All window ids in layout order (`nvim_list_wins`).
async fn all_wins(rpc: &Rpc) -> Vec<u64> {
    match rpc
        .request("nvim_list_wins", vec![])
        .await
        .expect("list_wins")
    {
        Value::Array(a) => a.iter().filter_map(Value::as_u64).collect(),
        _ => Vec::new(),
    }
}

/// A wheel gesture carrying a `modifier` string (e.g. `"S"` for a `Shift`-page),
/// which the modifier-less `feed_mouse` can't send.
fn wheel_mod(rpc: &Rpc, action: &str, modifier: &str, row: usize, col: usize) {
    rpc.notify(
        "nvim_input_mouse",
        vec![
            Value::from("wheel"),
            Value::from(action),
            Value::from(modifier),
            Value::from(0u64),
            Value::from(row as u64),
            Value::from(col as u64),
        ],
    );
}

/// A vertical wheel-down notch scrolls the focused window by `'mousescroll'` (3 by
/// default) lines, leaving the cursor on its line while that line stays visible.
#[tokio::test]
async fn wheel_down_scrolls_three_lines() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    feed(&rpc, "8G"); // cursor mid-screen so a 3-line scroll won't push it off
    assert_eq!(win_topline(&rpc, win).await, 1);
    feed_mouse(&rpc, "wheel", "down", 5, 10);
    assert_eq!(win_topline(&rpc, win).await, 4, "scrolled 3 lines down");
    assert_eq!(cursor(&rpc).await.0, 8, "cursor stays on its line");
    assert_eq!(current_win(&rpc).await, win, "focus unchanged");
}

/// Wheel-up scrolls back toward the top of the buffer, the inverse of wheel-down,
/// and stops at the first line.
#[tokio::test]
async fn wheel_up_scrolls_back() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    feed(&rpc, "8G");
    feed_mouse(&rpc, "wheel", "down", 5, 10); // top 0 -> 3
    feed_mouse(&rpc, "wheel", "down", 5, 10); // top 3 -> 6
    assert_eq!(win_topline(&rpc, win).await, 7);
    feed_mouse(&rpc, "wheel", "up", 5, 10); // top 6 -> 3
    assert_eq!(win_topline(&rpc, win).await, 4, "scrolled 3 lines up");
    assert_eq!(cursor(&rpc).await.0, 8, "cursor stayed visible throughout");
}

/// Wheeling over an *unfocused* split scrolls only that split — focus and the
/// active window's view stay put (the wheel scrolls a window you are not in).
#[tokio::test]
async fn wheel_over_other_split_scrolls_only_it() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-w>v"); // vertical split: new (left) window focused, original right
    let focused = current_win(&rpc).await;
    let other = all_wins(&rpc)
        .await
        .into_iter()
        .find(|w| *w != focused)
        .expect("a second window");
    let focused_top0 = win_topline(&rpc, focused).await;
    let other_top0 = win_topline(&rpc, other).await;
    feed_mouse(&rpc, "wheel", "down", 5, 45); // col 45 lands in the right split
    assert_eq!(
        win_topline(&rpc, other).await,
        other_top0 + 3,
        "the pointed-at split scrolled"
    );
    assert_eq!(
        win_topline(&rpc, focused).await,
        focused_top0,
        "the focused split did not scroll"
    );
    assert_eq!(current_win(&rpc).await, focused, "focus did not move");
    assert_eq!(cursor(&rpc).await.0, 1, "cursor did not move");
}

/// `Shift`+wheel scrolls a whole page (window height minus a two-line overlap),
/// not a `'mousescroll'` notch.
#[tokio::test]
async fn shift_wheel_scrolls_page() {
    let (rpc, _incoming) = start(&numbered(200)).await;
    let win = current_win(&rpc).await;
    feed(&rpc, "gg");
    let page = win_height(&rpc, win).await.saturating_sub(2).max(1);
    wheel_mod(&rpc, "down", "S", 5, 10);
    assert_eq!(
        win_topline(&rpc, win).await,
        1 + page,
        "a shift-notch scrolled one page"
    );
}

/// `'mousescroll'` sets the vertical step: `ver:5` scrolls five lines a notch.
#[tokio::test]
async fn mousescroll_sets_vertical_step() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    command(&rpc, "set mousescroll=ver:5,hor:6").await;
    feed(&rpc, "10G"); // mid-screen so the 5-line scroll keeps it visible
    feed_mouse(&rpc, "wheel", "down", 5, 10);
    assert_eq!(win_topline(&rpc, win).await, 6, "scrolled five lines");
    assert_eq!(cursor(&rpc).await.0, 10, "cursor stays put");
}

/// `'mousescroll'` `ver:0` disables the vertical wheel entirely — a notch is a
/// no-op, not a one-line fallback.
#[tokio::test]
async fn mousescroll_ver_zero_disables_vertical() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    command(&rpc, "set mousescroll=ver:0,hor:6").await;
    feed(&rpc, "8G");
    let top0 = win_topline(&rpc, win).await;
    feed_mouse(&rpc, "wheel", "down", 5, 10);
    assert_eq!(
        win_topline(&rpc, win).await,
        top0,
        "vertical wheel disabled"
    );
}

/// A horizontal wheel notch scrolls the window by `'mousescroll'` hor columns
/// under `nowrap`, pulling the cursor into the newly-visible band.
#[tokio::test]
async fn wheel_right_scrolls_columns() {
    let long = "x".repeat(200);
    let (rpc, _incoming) = start(&long).await;
    command(&rpc, "set nonumber norelativenumber nowrap").await;
    let win = current_win(&rpc).await;
    feed(&rpc, "gg0");
    assert_eq!(win_leftcol(&rpc, win).await, 0);
    wheel_mod(&rpc, "right", "", 5, 10);
    assert_eq!(
        win_leftcol(&rpc, win).await,
        6,
        "scrolled six columns right"
    );
    // The cursor was pulled to the first visible column so the scroll sticks.
    assert_eq!(cursor(&rpc).await.1, 6);
}

/// The horizontal wheel won't scroll past the content: when every line already
/// fits in the window there is nothing off-screen to the right, so a right notch
/// is a no-op (vim doesn't scroll into empty space).
#[tokio::test]
async fn wheel_right_does_not_scroll_past_content() {
    let (rpc, _incoming) = start("hello\nworld\nshort lines").await;
    command(&rpc, "set nonumber norelativenumber nowrap").await;
    let win = current_win(&rpc).await;
    feed(&rpc, "gg0");
    assert_eq!(win_leftcol(&rpc, win).await, 0);
    wheel_mod(&rpc, "right", "", 5, 3);
    assert_eq!(
        win_leftcol(&rpc, win).await,
        0,
        "no horizontal scroll when every line fits on screen"
    );
}

/// The horizontal wheel stops at the content's right edge — it can scroll a long
/// line into view but no further, so the last column never leaves the screen.
#[tokio::test]
async fn wheel_right_stops_at_content_edge() {
    // One line a touch wider than the 80-col window (no gutter): 90 columns.
    let (rpc, _incoming) = start(&"x".repeat(90)).await;
    command(&rpc, "set nonumber norelativenumber nowrap").await;
    let win = current_win(&rpc).await;
    feed(&rpc, "gg0");
    // Many right notches; leftcol saturates at widest(90) - width(80) = 10.
    for _ in 0..20 {
        wheel_mod(&rpc, "right", "", 5, 3);
        let _ = cursor(&rpc).await; // barrier between gestures
    }
    assert_eq!(
        win_leftcol(&rpc, win).await,
        10,
        "leftcol clamps so the line's last column stays on screen"
    );
}

/// A wheel notch that lands on no window (here, far below the single window in the
/// bottom chrome / past the buffer) does not scroll it.
#[tokio::test]
async fn wheel_outside_any_window_is_ignored() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    feed(&rpc, "8G");
    let top0 = win_topline(&rpc, win).await;
    feed_mouse(&rpc, "wheel", "down", 200, 200); // row/col past every window
    assert_eq!(win_topline(&rpc, win).await, top0, "no window scrolled");
}

// ===== Phase 5: drag the separator / status line to resize splits ===========

/// A window's rect width (`nvim_win_get_width`).
async fn win_width(rpc: &Rpc, win: u64) -> u64 {
    rpc.request("nvim_win_get_width", vec![Value::from(win)])
        .await
        .expect("get_width")
        .as_u64()
        .expect("width")
}

/// The two window ids in layout order (left→right for a vsplit, top→bottom for a
/// horizontal split).
async fn two_wins(rpc: &Rpc) -> (u64, u64) {
    let wins = all_wins(rpc).await;
    (wins[0], wins[1])
}

/// Dragging a vertical separator right resizes the splits by the drag delta,
/// without moving focus or starting a selection.
#[tokio::test]
async fn drag_separator_resizes_vertical_split() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    // Two side-by-side windows: left width 40 (cols 0..40), separator at x=40,
    // right at cols 41..80. The new (left) window holds focus.
    feed(&rpc, "<C-w>v");
    let (left, right) = two_wins(&rpc).await;
    assert_eq!(win_width(&rpc, left).await, 40);
    let focused = current_win(&rpc).await;

    // Grab the separator at col 40 and drag it 5 cells right.
    feed_mouse(&rpc, "left", "press", 2, 40);
    feed_mouse(&rpc, "left", "drag", 2, 45);
    feed_mouse(&rpc, "left", "release", 2, 45);

    assert_eq!(
        win_width(&rpc, left).await,
        45,
        "left grew by the drag delta"
    );
    assert_eq!(win_width(&rpc, right).await, 34, "right shrank to match");
    assert_eq!(
        current_win(&rpc).await,
        focused,
        "the drag didn't move focus"
    );
    assert_eq!(mode(&rpc).await, "n", "the drag didn't start a selection");
}

/// Dragging a separator back left shrinks the window it had grown — the drag is
/// absolute against the press point, so it tracks the pointer both ways.
#[tokio::test]
async fn drag_separator_left_shrinks() {
    let (rpc, _incoming) = start("hello\nworld").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-w>v");
    let (left, right) = two_wins(&rpc).await;
    feed_mouse(&rpc, "left", "press", 2, 40);
    feed_mouse(&rpc, "left", "drag", 2, 46); // +6 → left 46
    feed_mouse(&rpc, "left", "drag", 2, 34); // back past origin → left 34
    feed_mouse(&rpc, "left", "release", 2, 34);
    assert_eq!(
        win_width(&rpc, left).await,
        34,
        "left tracked the pointer back"
    );
    assert_eq!(win_width(&rpc, right).await, 45);
}

/// Dragging a window's status line down resizes a horizontal split's heights —
/// the status line acts as the divider (vim's status-line drag).
#[tokio::test]
async fn drag_status_line_resizes_horizontal_split() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-w>s"); // stack: new top focused, original bottom
    let (top, bottom) = two_wins(&rpc).await;
    let (top0, bot0) = (win_height(&rpc, top).await, win_height(&rpc, bottom).await);

    // The top window sits at y=0, so its status line is the row at its text
    // height (0-based). Grab it and drag two rows down.
    let status_row = top0 as usize;
    feed_mouse(&rpc, "left", "press", status_row, 5);
    feed_mouse(&rpc, "left", "drag", status_row + 2, 5);
    feed_mouse(&rpc, "left", "release", status_row + 2, 5);

    assert_eq!(
        win_height(&rpc, top).await,
        top0 + 2,
        "top grew by the drag"
    );
    assert_eq!(
        win_height(&rpc, bottom).await,
        bot0 - 2,
        "bottom shrank to match"
    );
}

/// The horizontal separator row between two stacked windows is a drag handle too
/// (it sits one row below the upper window's status line).
#[tokio::test]
async fn drag_horizontal_separator_resizes_height() {
    let (rpc, _incoming) = start("hello\nworld").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-w>s");
    let (top, bottom) = two_wins(&rpc).await;
    let top0 = win_height(&rpc, top).await;
    let sep_row = top0 as usize + 1; // separator one row below the status line
    feed_mouse(&rpc, "left", "press", sep_row, 5);
    feed_mouse(&rpc, "left", "drag", sep_row + 3, 5);
    feed_mouse(&rpc, "left", "release", sep_row + 3, 5);
    assert_eq!(win_height(&rpc, top).await, top0 + 3);
    let _ = bottom;
}

/// The bottom-most window's status line has nothing below it, so pressing it is a
/// no-op — no resize, no crash.
#[tokio::test]
async fn drag_bottom_status_line_does_not_resize() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<C-w>s");
    let (top, bottom) = two_wins(&rpc).await;
    let (top0, bot0) = (win_height(&rpc, top).await, win_height(&rpc, bottom).await);
    // The bottom window's status line is its last row, just above the command
    // line; there is no window beneath it to resize against.
    let bottom_y = rpc
        .request("nvim_win_get_position", vec![Value::from(bottom)])
        .await
        .expect("get_position")
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_u64)
        .expect("y") as usize;
    let status_row = bottom_y + bot0 as usize; // bottom_y + text_height
    feed_mouse(&rpc, "left", "press", status_row, 5);
    feed_mouse(&rpc, "left", "drag", status_row + 2, 5);
    feed_mouse(&rpc, "left", "release", status_row + 2, 5);
    assert_eq!(win_height(&rpc, top).await, top0, "heights unchanged");
    assert_eq!(win_height(&rpc, bottom).await, bot0);
}

/// The shipped `examples/mouse/` config sources cleanly (no Lua error) and its
/// `:set` calls actually reach the core — observable through `:set mousescroll?`.
#[tokio::test]
async fn mouse_example_config_runs() {
    let dir = temp_dir("mouse-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/mouse/init.lua"
    ))
    .expect("read example init.lua");
    std::fs::write(dir.join("init.lua"), &init).expect("write init.lua");
    let server_init = ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(server_init);
    attach(&rpc, 80, 24).await;

    // The example's `vim.cmd("set mousescroll=ver:5,hor:6")` reached the core.
    command(&rpc, "set mousescroll?").await;
    let map = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw");
    assert_eq!(message(&map), "mousescroll=ver:5,hor:6");
    // And nothing errored on the way (the notify, the user command, the sets).
    let banner = message(&map);
    assert!(!banner.contains("Error"), "example errored: {banner:?}");
}
