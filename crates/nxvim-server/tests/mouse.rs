//! Behavior tests for mouse support, driven the way a real client drives the
//! editor: a global screen cell goes in via `nx_input_mouse`, and we assert on
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
//! Phase 6 — clicking a tabline cell switches to that tab. Phase 7 — the
//! right-click `'mousemodel'` branch, middle-click paste, and insert-mode click.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, exec_lua, feed, feed_mouse, feed_mouse_at, field, lines, message,
    mode, spawn, temp_dir, wait_redraw, write_temp, FakeClipboard, TestClock,
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
        "nx_input_mouse",
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

/// Send a button press carrying an arbitrary `modifier` string (e.g. `"C"`,
/// `"C-S"`, `"A"`) — the general counterpart of [`shift_press`], for driving the
/// `<C-LeftMouse>` / `<RightMouse>` / `<MiddleMouse>` mappable-button path.
/// Fire-and-forget; pair with a barrier read.
fn press_mod(rpc: &Rpc, button: &str, modifier: &str, row: usize, col: usize) {
    rpc.notify(
        "nx_input_mouse",
        vec![
            Value::from(button),
            Value::from("press"),
            Value::from(modifier),
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

/// A malformed `nx_input_mouse` call (unknown action for the button) is
/// rejected loudly at the boundary, never silently coerced.
#[tokio::test]
async fn unknown_mouse_action_errors() {
    let (rpc, _incoming) = start("hello").await;
    let bad = rpc
        .request(
            "nx_input_mouse",
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
    // Wait for the frame carrying the echoed value (the redraw can lag under load).
    let map = wait_redraw(&mut incoming, |m| message(m) == "mousetime=250").await;
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

/// A dynamic sign column (a gutter that appears only because a sign is placed)
/// is part of the gutter the hit-test must skip. With `nonumber` the only gutter
/// is the 2-cell sign column; a click must subtract it, or every column lands two
/// cells to the right of where the user clicked. Regression test for the
/// diagnostics-gutter mouse shift.
#[tokio::test]
async fn left_click_accounts_for_dynamic_sign_column() {
    let (rpc, mut incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    // Place a gutter sign so `signcolumn=auto` reserves its 2-cell column. An
    // extmark sign is core-shared, exercising the same gutter widening a
    // diagnostic sign drives (the merge is sign-source-agnostic).
    exec_lua(
        &rpc,
        r#"
        local ns = vim.api.nvim_create_namespace('mouse_sign')
        vim.api.nvim_buf_set_extmark(0, ns, 1, 0, { sign_text = '>>' })
        "#,
    )
    .await;
    // Wait until the frame actually reserves the 2-cell sign column, so the
    // hit-test sees the widened gutter the user clicked against.
    wait_redraw(&mut incoming, |m| {
        field(m, "sign_width").and_then(Value::as_u64) == Some(2)
    })
    .await;
    // Sign column = 2 cells, no number gutter. Screen col 5 → text col 5 − 2 = 3,
    // byte 3 of "second line" ('o'). Without skipping the sign column it would
    // wrongly land on byte 5 ('d').
    feed_mouse(&rpc, "left", "press", 1, 5);
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

/// `'padding'` insets the text body, so a click maps through the per-side margin:
/// the text starts `pad.top` rows down and `pad.left` cols in. A click in the
/// margin itself hits no window and leaves the cursor put.
#[tokio::test]
async fn left_click_maps_through_padding() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set padding=2").await;
    // Text body now starts at global (row 2, col 2). The window's first line sits
    // at row 2; clicking there lands on line 1, byte 0.
    feed_mouse(&rpc, "left", "press", 2, 2);
    assert_eq!(cursor(&rpc).await, (1, 0));
    // Row 3 → line 2; col 5 → byte 3 (5 − 2 left-margin), "second line"'s 'o'.
    feed_mouse(&rpc, "left", "press", 3, 5);
    assert_eq!(cursor(&rpc).await, (2, 3));
}

#[tokio::test]
async fn click_in_the_padding_margin_is_a_no_op() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set padding=3").await;
    // Park the cursor on a known cell inside the text body first.
    feed_mouse(&rpc, "left", "press", 4, 4);
    assert_eq!(cursor(&rpc).await, (2, 1));
    // A click in the top-left margin (inside the window rect, outside the padded
    // content box) hits no window → the cursor stays where it was.
    feed_mouse(&rpc, "left", "press", 1, 1);
    assert_eq!(cursor(&rpc).await, (2, 1), "margin click moves nothing");
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

/// `<LeftMouse>` is a mappable key (the general mouse-mapping primitive): a single
/// left press fires the map, after the cursor is placed at the click.
#[tokio::test]
async fn left_mouse_can_be_mapped_like_any_key() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.clicks = 0
        _G.line_at_click = nil
        nx.keymap.set('n', '<LeftMouse>', function()
          _G.clicks = _G.clicks + 1
          _G.line_at_click = vim.fn.line('.')
        end)
        return true
    "#,
    )
    .await;
    feed_mouse(&rpc, "left", "press", 1, 3); // row 1 → line 2
    assert_eq!(exec_lua(&rpc, "return _G.clicks").await.as_u64(), Some(1));
    assert_eq!(
        exec_lua(&rpc, "return _G.line_at_click").await.as_u64(),
        Some(2),
        "the cursor was placed on the clicked line before the map fired"
    );
}

/// A `<2-LeftMouse>` map fires on the double-click *instead of* the default word
/// selection — the mapping suppresses the default gesture rather than both running.
#[tokio::test]
async fn double_click_fires_a_mapped_2leftmouse_and_suppresses_selection() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    exec_lua(
        &rpc,
        r#"
        _G.opened = 0
        nx.keymap.set('n', '<2-LeftMouse>', function() _G.opened = _G.opened + 1 end)
        return true
    "#,
    )
    .await;
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n", "the first press is a single click");
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 0, 7);
    assert_eq!(exec_lua(&rpc, "return _G.opened").await.as_u64(), Some(1));
    assert_eq!(
        mode(&rpc).await,
        "n",
        "the mapped double-click did NOT also start a Visual word selection"
    );
}

/// A buffer-local `<2-LeftMouse>` map fires only in the buffer it was set for; an
/// unmapped buffer keeps the default word-select double-click.
#[tokio::test]
async fn a_buffer_local_mouse_map_is_scoped_to_its_buffer() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line").await;
    command(&rpc, "set nonumber norelativenumber mousetime=500").await;
    exec_lua(
        &rpc,
        r#"
        _G.local_hits = 0
        nx.keymap.set('n', '<2-LeftMouse>', function() _G.local_hits = _G.local_hits + 1 end,
          { buffer = 0 })
        return true
    "#,
    )
    .await;
    // In the mapped buffer the double-click fires the map (no Visual).
    feed_mouse_at(&rpc, &clock, 0, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n");
    feed_mouse_at(&rpc, &clock, 100, "left", "press", 0, 7);
    assert_eq!(
        exec_lua(&rpc, "return _G.local_hits").await.as_u64(),
        Some(1)
    );
    assert_eq!(mode(&rpc).await, "n", "mapped buffer: no default selection");

    // A different buffer (where the map doesn't apply): the double-click falls back
    // to the default word selection.
    let other = write_temp("mouse_other", "txt", "alpha beta gamma");
    command(&rpc, &format!("edit {other}")).await;
    feed_mouse_at(&rpc, &clock, 1000, "left", "press", 0, 7);
    assert_eq!(mode(&rpc).await, "n");
    feed_mouse_at(&rpc, &clock, 1100, "left", "press", 0, 7);
    assert_eq!(
        exec_lua(&rpc, "return _G.local_hits").await.as_u64(),
        Some(1),
        "the buffer-local map must not fire in another buffer"
    );
    assert_eq!(
        mode(&rpc).await,
        "v",
        "the unmapped buffer keeps the default word-select double-click"
    );
}

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
        "nx_input_mouse",
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

// ===== Phase 4b: drag past the window edge auto-scrolls the buffer ==========

/// Window `win`'s top-left position row in the windows area (`nvim_win_get_position`
/// → `[row, col]`), the global screen row of its first text line when no tabline is
/// shown. Doubles as a barrier.
async fn win_position_row(rpc: &Rpc, win: u64) -> usize {
    match rpc
        .request("nvim_win_get_position", vec![Value::from(win)])
        .await
        .expect("win_get_position")
    {
        Value::Array(a) => a.first().and_then(Value::as_u64).unwrap_or(0) as usize,
        _ => 0,
    }
}

/// Dragging the selection below the window's last text row scrolls the buffer
/// down a line per drag event and keeps the live end on the newly-exposed bottom
/// line — vim's mouse drag-scroll, so a selection can grow past the viewport.
#[tokio::test]
async fn drag_below_window_scrolls_and_extends() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    let h = win_height(&rpc, win).await as usize; // text rows in the window
    assert_eq!(win_topline(&rpc, win).await, 1, "starts at the top");
    feed_mouse(&rpc, "left", "press", 0, 0); // anchor on line 1
                                             // Drag well below the text body (past the status line).
    feed_mouse(&rpc, "left", "drag", h + 5, 0);
    assert_eq!(mode(&rpc).await, "v", "the drag entered Visual");
    assert_eq!(
        win_topline(&rpc, win).await,
        2,
        "one line scrolled into view"
    );
    assert_eq!(
        cursor(&rpc).await.0 as usize,
        2 + h - 1,
        "the live end rode the new bottom line"
    );
    // A second drag at the same below-edge cell keeps scrolling.
    feed_mouse(&rpc, "left", "drag", h + 5, 0);
    assert_eq!(
        win_topline(&rpc, win).await,
        3,
        "held at the edge, it keeps scrolling"
    );
}

/// Dragging to the **top visible line** scrolls the buffer up — the single-window
/// case where that line is global row 0, so there is no row "above" the window to
/// reach (the pointer clamps at 0). The edge line itself must trigger the scroll,
/// or upward auto-scroll is impossible for the topmost window.
#[tokio::test]
async fn drag_to_top_visible_line_scrolls_up() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    feed(&rpc, "40G"); // scroll down so there is room to scroll back up
    let top0 = win_topline(&rpc, win).await;
    assert!(top0 > 1, "the buffer scrolled down to show line 40");
    feed_mouse(&rpc, "left", "press", 5, 0); // anchor a few lines down
    feed_mouse(&rpc, "left", "drag", 0, 0); // drag up to the first visible line
    assert_eq!(mode(&rpc).await, "v");
    assert_eq!(
        win_topline(&rpc, win).await,
        top0 - 1,
        "the top visible line scrolled the buffer up (single window, no tabline)"
    );
    // Held there (the client repeats the drag), it keeps scrolling up.
    feed_mouse(&rpc, "left", "drag", 0, 0);
    assert_eq!(win_topline(&rpc, win).await, top0 - 2, "keeps scrolling up");
}

/// Dragging above a window's first text row scrolls the buffer up. Exercised on
/// the lower split (whose first text row sits mid-screen) so there are rows above
/// it to drag into.
#[tokio::test]
async fn drag_above_window_scrolls_up() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "split").await; // stack two windows; the new top one is focused
    feed(&rpc, "<C-w>j"); // focus the bottom window
    let win = current_win(&rpc).await;
    feed(&rpc, "50G"); // scroll it so there is room to scroll back up
    let top0 = win_topline(&rpc, win).await;
    assert!(top0 > 1, "the bottom split scrolled down to show line 50");
    let row = win_position_row(&rpc, win).await; // its first text row (global)
    feed_mouse(&rpc, "left", "press", row, 0); // anchor on its top visible line
    feed_mouse(&rpc, "left", "drag", 0, 0); // drag up, above the split
    assert_eq!(mode(&rpc).await, "v");
    assert_eq!(
        win_topline(&rpc, win).await,
        top0 - 1,
        "dragging above the split scrolled it up a line"
    );
}

/// A drag that stays inside the text body does not scroll — only crossing the
/// edge does (regression guard for the auto-scroll branch).
#[tokio::test]
async fn drag_within_window_does_not_scroll() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 3, 2); // a few rows down, still on screen
    assert_eq!(mode(&rpc).await, "v");
    assert_eq!(cursor(&rpc).await.0, 4, "extended to the dragged-over line");
    assert_eq!(win_topline(&rpc, win).await, 1, "no scroll while on screen");
}

/// Triple-click then drag below the window extends the *linewise* selection while
/// auto-scrolling — the unit chosen by the click count survives the scroll.
#[tokio::test]
async fn linewise_drag_below_window_autoscrolls() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    let win = current_win(&rpc).await;
    let h = win_height(&rpc, win).await as usize;
    // Triple-click line 1 (a wall-clock gesture is fine — three same-cell presses
    // inside the default 500ms `mousetime` escalate to a linewise select).
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "press", 0, 0);
    assert_eq!(mode(&rpc).await, "V", "triple-click is linewise Visual");
    feed_mouse(&rpc, "left", "drag", h + 5, 0);
    assert_eq!(mode(&rpc).await, "V", "still linewise after the drag");
    assert_eq!(win_topline(&rpc, win).await, 2, "auto-scrolled down a line");
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

// ── Phase 6: tabline click switches tabs ───────────────────────────────────

/// Start a server with three tab pages over files `aaa.txt` / `bbb.txt` /
/// `ccc.txt` (contents `alpha` / `beta` / `gamma`) in a shared temp dir, each a
/// single unmodified window. Three 7-char names with the default ` {name} ` cell
/// (no window-count, no `+`) make the tabline read
/// `[ aaa.txt ][ bbb.txt ][ ccc.txt ]` — nine columns per cell, so tab 0 covers
/// cols 0..9, tab 1 cols 9..18, tab 2 cols 18..27. The last-opened tab
/// (`ccc.txt`) is current.
async fn start_tabs() -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir("mouse_tabs");
    for (name, body) in [
        ("aaa.txt", "alpha"),
        ("bbb.txt", "beta"),
        ("ccc.txt", "gamma"),
    ] {
        std::fs::write(dir.join(name), body).expect("write tab file");
    }
    let init = ServerInit {
        file: Some(dir.join("aaa.txt").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    command(&rpc, &format!("tabnew {}", dir.join("bbb.txt").display())).await;
    command(&rpc, &format!("tabnew {}", dir.join("ccc.txt").display())).await;
    (rpc, incoming)
}

#[tokio::test]
async fn click_tab_switches_to_it() {
    let (rpc, _incoming) = start_tabs().await;
    // The last :tabnew left ccc.txt current.
    assert_eq!(lines(&rpc).await, vec!["gamma"]);
    // Click inside tab 1's cell (cols 9..18) on the tabline row.
    feed_mouse(&rpc, "left", "press", 0, 11);
    assert_eq!(
        lines(&rpc).await,
        vec!["beta"],
        "switched to the bbb.txt tab"
    );
    assert_eq!(mode(&rpc).await, "n", "the click didn't start a selection");
}

#[tokio::test]
async fn click_first_tab_switches_back() {
    let (rpc, _incoming) = start_tabs().await;
    // Click inside tab 0's cell (cols 0..9).
    feed_mouse(&rpc, "left", "press", 0, 3);
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha"],
        "switched to the aaa.txt tab"
    );
}

#[tokio::test]
async fn click_active_tab_is_noop() {
    let (rpc, _incoming) = start_tabs().await;
    // ccc.txt is current; clicking inside its own cell (cols 18..27) stays put
    // and must not fall through to placing a cursor on the tabline row.
    feed_mouse(&rpc, "left", "press", 0, 20);
    assert_eq!(lines(&rpc).await, vec!["gamma"], "stayed on the active tab");
    assert_eq!(mode(&rpc).await, "n");
}

#[tokio::test]
async fn click_past_last_tab_is_noop() {
    let (rpc, _incoming) = start_tabs().await;
    // The blank fill past the last cell (col 27+) is vim's `TabLineFill` — inert.
    feed_mouse(&rpc, "left", "press", 0, 40);
    assert_eq!(lines(&rpc).await, vec!["gamma"], "the fill strip is inert");
    assert_eq!(mode(&rpc).await, "n");
}

#[tokio::test]
async fn click_custom_tabline_without_regions_is_noop() {
    let (rpc, _incoming) = start_tabs().await;
    // A custom `'tabline'` has no *built-in* click cells: switching a tab needs
    // explicit `%nT` regions (see `custom_tabline_nt_click_switches_tab`). This one
    // has none, so the column that would switch tabs with the built-in cells is inert.
    command(&rpc, "set tabline=MYTABLINE").await;
    feed_mouse(&rpc, "left", "press", 0, 3);
    assert_eq!(
        lines(&rpc).await,
        vec!["gamma"],
        "a region-less custom tabline isn't clickable"
    );
}

#[tokio::test]
async fn row0_click_is_text_when_tabline_hidden() {
    let path = write_temp("mouse", "txt", "alphabet here\nsecond line");
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    command(&rpc, "set nonumber norelativenumber").await;
    // With a single tab the tabline is hidden, so row 0 is the first *text* line:
    // the click must place the cursor there, not be swallowed as a tab switch.
    feed_mouse(&rpc, "left", "press", 0, 5);
    assert_eq!(
        cursor(&rpc).await,
        (1, 5),
        "row 0 placed the text cursor when no tabline is shown"
    );
}

// ── Phase 7: right-click model, middle-click paste, insert-mode click ───────

/// Like [`start`], but inject an in-memory clipboard so middle-click paste has a
/// `"*` register to read. Returns the [`FakeClipboard`] handle to seed/peek.
async fn start_with_clipboard(content: &str) -> (Rpc, FakeClipboard, UnboundedReceiver<Incoming>) {
    let path = write_temp("mouse", "txt", content);
    let fake = FakeClipboard::default();
    let init = ServerInit {
        file: Some(path),
        clipboard: nxvim_server::ClipboardProvider::Custom(Box::new(fake.clone())),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, fake, incoming)
}

/// `mousemodel=popup_setpos` (the default): a right-click moves the cursor to the
/// click without starting a selection.
#[tokio::test]
async fn right_click_popup_setpos_moves_cursor() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "right", "press", 1, 3); // row 1 → line 2, col 3
    assert_eq!(cursor(&rpc).await, (2, 3));
    assert_eq!(
        mode(&rpc).await,
        "n",
        "right-click didn't start a selection"
    );
}

/// `popup_setpos`: a right-click *inside* the active Visual selection keeps it
/// (so a context menu could act on it) and leaves the cursor put.
#[tokio::test]
async fn right_click_popup_setpos_keeps_selection_inside() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "vlll"); // Visual, anchor (1,0), cursor (1,3): covers cols 0..=3
    assert_eq!(mode(&rpc).await, "v");
    feed_mouse(&rpc, "right", "press", 0, 2); // col 2 is inside the selection
    assert_eq!(mode(&rpc).await, "v", "click inside the selection keeps it");
    assert_eq!(cursor(&rpc).await, (1, 3), "and doesn't move the cursor");
}

/// `popup_setpos`: a right-click *outside* the selection ends Visual and moves
/// the cursor to the click.
#[tokio::test]
async fn right_click_popup_setpos_outside_ends_selection() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "vlll"); // cursor (1,3)
    feed_mouse(&rpc, "right", "press", 0, 8); // col 8 is past the selection
    assert_eq!(mode(&rpc).await, "n", "click outside ends Visual");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

/// `mousemodel=extend`: a right-click extends the selection toward the click,
/// like `<S-LeftMouse>`.
#[tokio::test]
async fn right_click_extend_model_extends_selection() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set mousemodel=extend").await;
    feed(&rpc, "vl"); // Visual, anchor (1,0), cursor (1,1)
    feed_mouse(&rpc, "right", "press", 0, 8); // extend out to col 8
    assert_eq!(cursor(&rpc).await, (1, 8));
    assert_eq!(mode(&rpc).await, "v");
    feed(&rpc, "d"); // delete cols 0..=8 of "hello world" → "ld"
    assert_eq!(lines(&rpc).await[0], "ld");
}

/// `mousemodel=popup`: a right-click only pops a (not-yet-built) menu, so with no
/// menu it's an observable no-op — the cursor and mode are untouched.
#[tokio::test]
async fn right_click_popup_model_is_noop() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set mousemodel=popup").await;
    feed_mouse(&rpc, "right", "press", 1, 5);
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "popup model doesn't move the cursor"
    );
    assert_eq!(mode(&rpc).await, "n");
}

/// Middle-click pastes the `"*` clipboard register at the click position with
/// `gP` semantics (spliced after the clicked grapheme).
#[tokio::test]
async fn middle_click_pastes_clipboard() {
    let (rpc, clip, _incoming) = start_with_clipboard("hello").await;
    command(&rpc, "set nonumber norelativenumber").await;
    clip.seed("XX", false); // a charwise primary selection
    feed_mouse(&rpc, "middle", "press", 0, 0); // click on 'h'
    assert_eq!(
        lines(&rpc).await,
        vec!["hXXello"],
        "pasted after the clicked char"
    );
}

/// Middle-click with nothing on the clipboard (no provider) is a silent no-op —
/// nothing to paste, exactly like middle-clicking with an empty primary selection.
#[tokio::test]
async fn middle_click_empty_clipboard_is_noop() {
    let (rpc, _incoming) = start("hello").await; // default: no clipboard provider
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "middle", "press", 0, 2);
    assert_eq!(lines(&rpc).await, vec!["hello"], "nothing pasted");
    assert_eq!(mode(&rpc).await, "n");
}

// ===== getmousepos() ========================================================

/// `vim.fn.getmousepos()` reports the last click's screen cell, window, and buffer
/// position (all 1-based) — the position signal a mouse handler reads.
#[tokio::test]
async fn getmousepos_reports_the_clicked_position() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    let win = current_win(&rpc).await;
    feed_mouse(&rpc, "left", "press", 1, 3); // global row 1, col 3 → line 2, byte col 3
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().winid")
            .await
            .as_u64(),
        Some(win)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().line")
            .await
            .as_u64(),
        Some(2),
        "1-based buffer line"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().column")
            .await
            .as_u64(),
        Some(4),
        "1-based byte column"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().screenrow")
            .await
            .as_u64(),
        Some(2)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().screencol")
            .await
            .as_u64(),
        Some(4)
    );
    // The window sits at the screen origin here, so window-relative == screen.
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().winrow")
            .await
            .as_u64(),
        Some(2)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().wincol")
            .await
            .as_u64(),
        Some(4)
    );
}

/// Before any mouse event, `getmousepos()` is all-zero for the window/buffer fields
/// (no stale position) — `winid`/`line`/`column` are 0.
#[tokio::test]
async fn getmousepos_is_zero_before_any_click() {
    let (rpc, _incoming) = start("hello world").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().winid")
            .await
            .as_u64(),
        Some(0)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().line")
            .await
            .as_u64(),
        Some(0)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().column")
            .await
            .as_u64(),
        Some(0)
    );
}

/// `winrow`/`wincol` are window-relative, so chrome above the window (a tabline)
/// offsets them from the global `screenrow`/`screencol`.
#[tokio::test]
async fn getmousepos_window_relative_accounts_for_chrome() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber showtabline=2").await;
    // The tabline eats global row 0, so the window's first line is global row 1.
    feed_mouse(&rpc, "left", "press", 2, 5); // global row 2 → window row 2, line 2
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().screenrow")
            .await
            .as_u64(),
        Some(3),
        "global screen row is 1-based from the very top"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().winrow")
            .await
            .as_u64(),
        Some(2),
        "window-relative row excludes the tabline above the window"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getmousepos().line")
            .await
            .as_u64(),
        Some(2)
    );
}

/// The key use case: a `<RightMouse>` map reads `getmousepos()` to act on the
/// *clicked* cell even though a right-click doesn't move the cursor — the position is
/// current inside the mapping (the mirror is pushed before the RHS fires).
#[tokio::test]
async fn a_mouse_map_reads_getmousepos_for_the_clicked_cell() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.clicked_line = nil
        _G.clicked_col = nil
        nx.keymap.set('n', '<RightMouse>', function()
          local p = vim.fn.getmousepos()
          _G.clicked_line = p.line
          _G.clicked_col = p.column
        end)
        return true
    "#,
    )
    .await;
    assert_eq!(cursor(&rpc).await, (1, 0));
    feed_mouse(&rpc, "right", "press", 2, 2); // line 3, byte col 2 → column 3
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "the right-click map ran without moving the cursor"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.clicked_line").await.as_u64(),
        Some(3),
        "the map saw the clicked line, not the cursor line"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.clicked_col").await.as_u64(),
        Some(3)
    );
}

// ===== Modifiers + right/middle as mappable buttons =========================

/// `<C-LeftMouse>` is mappable: a Ctrl+left press fires the map, and — like a plain
/// `<LeftMouse>` — the cursor is placed at the click first, so the map can act on the
/// clicked position (the `<C-LeftMouse>` → go-to-definition idiom).
#[tokio::test]
async fn ctrl_left_mouse_can_be_mapped_and_places_cursor() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.ctrl_hits = 0
        _G.line_at_click = nil
        nx.keymap.set('n', '<C-LeftMouse>', function()
          _G.ctrl_hits = _G.ctrl_hits + 1
          _G.line_at_click = vim.fn.line('.')
        end)
        return true
    "#,
    )
    .await;
    press_mod(&rpc, "left", "C", 2, 4); // row 2 → line 3
    assert_eq!(
        exec_lua(&rpc, "return _G.ctrl_hits").await.as_u64(),
        Some(1)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.line_at_click").await.as_u64(),
        Some(3),
        "the cursor was placed on the clicked line before the Ctrl+click map fired"
    );
}

/// A modifier distinguishes the mapping: with both `<LeftMouse>` and `<C-LeftMouse>`
/// bound, a plain left fires only the plain map and a Ctrl+left only the Ctrl map —
/// neither click triggers the other.
#[tokio::test]
async fn a_modifier_distinguishes_the_left_mouse_map() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.plain = 0
        _G.ctrl = 0
        nx.keymap.set('n', '<LeftMouse>', function() _G.plain = _G.plain + 1 end)
        nx.keymap.set('n', '<C-LeftMouse>', function() _G.ctrl = _G.ctrl + 1 end)
        return true
    "#,
    )
    .await;
    // Distinct cells so the two presses aren't counted as a double-click (a same-cell
    // repeat within `'mousetime'` would escalate to `<C-2-LeftMouse>`).
    feed_mouse(&rpc, "left", "press", 0, 0); // plain
    assert_eq!(exec_lua(&rpc, "return _G.plain").await.as_u64(), Some(1));
    assert_eq!(
        exec_lua(&rpc, "return _G.ctrl").await.as_u64(),
        Some(0),
        "a plain click must not fire the <C-LeftMouse> map"
    );
    press_mod(&rpc, "left", "C", 1, 3); // ctrl, a different cell
    assert_eq!(
        exec_lua(&rpc, "return _G.plain").await.as_u64(),
        Some(1),
        "a Ctrl+click must not fire the plain <LeftMouse> map"
    );
    assert_eq!(exec_lua(&rpc, "return _G.ctrl").await.as_u64(), Some(1));
}

/// `<RightMouse>` is mappable: the press fires the map *instead of* the `'mousemodel'`
/// default, so the default `popup_setpos` cursor move is suppressed.
#[tokio::test]
async fn right_mouse_can_be_mapped_and_suppresses_the_default() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.right_hits = 0
        nx.keymap.set('n', '<RightMouse>', function() _G.right_hits = _G.right_hits + 1 end)
        return true
    "#,
    )
    .await;
    assert_eq!(cursor(&rpc).await, (1, 0));
    feed_mouse(&rpc, "right", "press", 1, 3); // would move to (2,3) under popup_setpos
    assert_eq!(
        exec_lua(&rpc, "return _G.right_hits").await.as_u64(),
        Some(1)
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "the mapped right-click suppressed the popup_setpos cursor move"
    );
}

/// `<MiddleMouse>` is mappable: the press fires the map *instead of* the `"*` paste,
/// so the buffer is untouched.
#[tokio::test]
async fn middle_mouse_can_be_mapped_and_suppresses_the_paste() {
    let (rpc, clip, _incoming) = start_with_clipboard("hello").await;
    command(&rpc, "set nonumber norelativenumber").await;
    clip.seed("XX", false);
    exec_lua(
        &rpc,
        r#"
        _G.mid_hits = 0
        nx.keymap.set('n', '<MiddleMouse>', function() _G.mid_hits = _G.mid_hits + 1 end)
        return true
    "#,
    )
    .await;
    feed_mouse(&rpc, "middle", "press", 0, 0);
    assert_eq!(exec_lua(&rpc, "return _G.mid_hits").await.as_u64(), Some(1));
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "the mapped middle-click suppressed the clipboard paste"
    );
}

/// `<S-LeftMouse>` (the selection-extend gesture) is mappable too: the shift-press
/// fires the map instead of entering Visual to extend.
#[tokio::test]
async fn shift_left_mouse_can_be_mapped_and_suppresses_extend() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.shift_hits = 0
        nx.keymap.set('n', '<S-LeftMouse>', function() _G.shift_hits = _G.shift_hits + 1 end)
        return true
    "#,
    )
    .await;
    shift_press(&rpc, 0, 6); // unmapped this would enter Visual and extend to col 6
    assert_eq!(
        exec_lua(&rpc, "return _G.shift_hits").await.as_u64(),
        Some(1)
    );
    assert_eq!(
        mode(&rpc).await,
        "n",
        "the mapped <S-LeftMouse> did not enter Visual to extend"
    );
}

// ===== Drag / release as mappable gestures ==================================

/// `<LeftDrag>` is a mappable gesture: the drag fires the map *instead of* the
/// default text-selection drag, so no Visual selection starts.
#[tokio::test]
async fn left_drag_can_be_mapped_and_suppresses_select() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.drags = 0
        nx.keymap.set('n', '<LeftDrag>', function() _G.drags = _G.drags + 1 end)
        return true
    "#,
    )
    .await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 0, 4);
    assert_eq!(exec_lua(&rpc, "return _G.drags").await.as_u64(), Some(1));
    assert_eq!(
        mode(&rpc).await,
        "n",
        "the mapped <LeftDrag> did not start a Visual selection"
    );
}

/// With no `<LeftDrag>` map, a drag still enters Visual select — the default gesture
/// survives the deferral behind the keymap lookup.
#[tokio::test]
async fn an_unmapped_left_drag_still_selects() {
    let (rpc, _incoming) = start("hello world\nsecond line").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed_mouse(&rpc, "left", "press", 0, 0);
    feed_mouse(&rpc, "left", "drag", 0, 4);
    assert_eq!(
        mode(&rpc).await,
        "v",
        "an unmapped drag still enters Visual select"
    );
}

/// `<LeftRelease>` is mappable: the button release fires the map.
#[tokio::test]
async fn left_release_can_be_mapped() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.releases = 0
        nx.keymap.set('n', '<LeftRelease>', function() _G.releases = _G.releases + 1 end)
        return true
    "#,
    )
    .await;
    feed_mouse(&rpc, "left", "press", 1, 2);
    feed_mouse(&rpc, "left", "release", 1, 2);
    assert_eq!(exec_lua(&rpc, "return _G.releases").await.as_u64(), Some(1));
}

/// A right-button drag is mappable too (`<RightDrag>`) — drag/release are wired for
/// every button, not just the left.
#[tokio::test]
async fn right_drag_can_be_mapped() {
    let (rpc, _incoming) = start("hello world\nsecond line").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.rdrag = 0
        nx.keymap.set('n', '<RightDrag>', function() _G.rdrag = _G.rdrag + 1 end)
        return true
    "#,
    )
    .await;
    feed_mouse(&rpc, "right", "press", 0, 0);
    feed_mouse(&rpc, "right", "drag", 0, 3);
    assert_eq!(exec_lua(&rpc, "return _G.rdrag").await.as_u64(), Some(1));
}

/// An insert-mode left-click moves the caret to the click and stays in Insert
/// (the default `'mouse'` includes `i`); the caret may sit one past the last char.
#[tokio::test]
async fn insert_click_moves_caret_and_stays_insert() {
    let (rpc, _incoming) = start("hello world").await;
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "i"); // enter Insert at col 0
    assert_eq!(mode(&rpc).await, "i");
    feed_mouse(&rpc, "left", "press", 0, 6); // click on 'w'
    assert_eq!(mode(&rpc).await, "i", "the click didn't leave Insert");
    assert_eq!(cursor(&rpc).await, (1, 6));
}

// ── Docks: the wheel and separator-drag reach a region you aren't focused in ──

/// The wheel scrolls the window **under the pointer** even when it is in a dock
/// the cursor is not focused in — without moving focus or scrolling the main area.
#[tokio::test]
async fn wheel_over_a_dock_scrolls_it_without_focus() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    // Open a left dock showing the same 100-line buffer, then cross back to main so
    // the dock is not focused. The dock window occupies cols 0..20.
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, buf = vim.api.nvim_get_current_buf() }",
    )
    .await;
    feed(&rpc, "<C-w><C-w>l");
    let main_win = current_win(&rpc).await;
    let dock_win = all_wins(&rpc)
        .await
        .into_iter()
        .find(|w| *w != main_win)
        .expect("a dock window");
    let dock_top0 = win_topline(&rpc, dock_win).await;
    let main_top0 = win_topline(&rpc, main_win).await;
    feed_mouse(&rpc, "wheel", "down", 5, 5); // col 5 lands in the left dock
    assert_eq!(
        win_topline(&rpc, dock_win).await,
        dock_top0 + 3,
        "the non-focused dock scrolled three lines"
    );
    assert_eq!(
        win_topline(&rpc, main_win).await,
        main_top0,
        "the main area did not scroll"
    );
    assert_eq!(current_win(&rpc).await, main_win, "focus did not move");
}

/// Dragging the **edge** of a left dock (the separator between it and the main
/// area) resizes the dock band itself — growing the dock and shrinking main —
/// without moving focus or starting a selection.
#[tokio::test]
async fn drag_left_dock_edge_resizes_the_band() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    // A left dock of width 20: content cols 0..20, its edge separator at col 20.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "<C-w><C-w>l"); // cross to main so the dock is not focused
    let main_win = current_win(&rpc).await;
    let dock_win = all_wins(&rpc)
        .await
        .into_iter()
        .find(|w| *w != main_win)
        .expect("a dock window");
    assert_eq!(win_width(&rpc, dock_win).await, 20);

    // Grab the edge at col 20 and drag it 6 cells right.
    feed_mouse(&rpc, "left", "press", 2, 20);
    feed_mouse(&rpc, "left", "drag", 2, 26);
    feed_mouse(&rpc, "left", "release", 2, 26);

    assert_eq!(
        win_width(&rpc, dock_win).await,
        26,
        "the dock band grew to follow the pointer"
    );
    assert_eq!(
        current_win(&rpc).await,
        main_win,
        "the drag didn't move focus"
    );
    assert_eq!(mode(&rpc).await, "n", "the drag didn't start a selection");
}

/// The dock edge drag is absolute against the pointer: dragging the edge back
/// past the press point shrinks the dock again (it tracks both ways).
#[tokio::test]
async fn drag_left_dock_edge_back_shrinks() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "<C-w><C-w>l");
    let main_win = current_win(&rpc).await;
    let dock_win = all_wins(&rpc)
        .await
        .into_iter()
        .find(|w| *w != main_win)
        .expect("a dock window");
    feed_mouse(&rpc, "left", "press", 2, 20);
    feed_mouse(&rpc, "left", "drag", 2, 30); // grow to 30
    feed_mouse(&rpc, "left", "drag", 2, 12); // back past the origin → 12
    feed_mouse(&rpc, "left", "release", 2, 12);
    assert_eq!(
        win_width(&rpc, dock_win).await,
        12,
        "the dock tracked the pointer back"
    );
}

/// Dragging the edge of a **right** dock resizes it the mirrored way: dragging
/// the edge left (toward main) grows the right dock.
#[tokio::test]
async fn drag_right_dock_edge_resizes_the_band() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    // A right dock of width 20: it occupies the right-most columns, its edge
    // separator sits at col 80 - 20 - 1 = 59.
    exec_lua(&rpc, "nx.dock.open{ side = 'right', size = 20 }").await;
    feed(&rpc, "<C-w><C-w>h"); // cross to main so the dock is not focused
    let main_win = current_win(&rpc).await;
    let dock_win = all_wins(&rpc)
        .await
        .into_iter()
        .find(|w| *w != main_win)
        .expect("a dock window");
    assert_eq!(win_width(&rpc, dock_win).await, 20);

    // Grab the edge at col 59 and drag it 5 cells left → right dock grows to 25.
    feed_mouse(&rpc, "left", "press", 2, 59);
    feed_mouse(&rpc, "left", "drag", 2, 54);
    feed_mouse(&rpc, "left", "release", 2, 54);
    assert_eq!(
        win_width(&rpc, dock_win).await,
        25,
        "the right dock grew as its edge moved toward main"
    );
    assert_eq!(
        current_win(&rpc).await,
        main_win,
        "the drag didn't move focus"
    );
}

/// Dragging the edge of a **bottom** dock up grows it (height), mirroring the
/// horizontal-dock geometry.
#[tokio::test]
async fn drag_bottom_dock_edge_resizes_the_band() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 6 }").await;
    feed(&rpc, "<C-w><C-w>k"); // cross to main so the dock is not focused
    let main_win = current_win(&rpc).await;
    let dock_win = all_wins(&rpc)
        .await
        .into_iter()
        .find(|w| *w != main_win)
        .expect("a dock window");
    // The dock window's text height is its band size minus its own status row.
    let h0 = win_height(&rpc, dock_win).await;
    // The main window sits at the top (row 0); its rect spans its text plus its
    // status row, so the bottom dock's edge separator is the row just past it.
    let sep_row = win_height(&rpc, main_win).await as usize + 1;
    feed_mouse(&rpc, "left", "press", sep_row, 5);
    feed_mouse(&rpc, "left", "drag", sep_row - 4, 5); // drag the edge up 4 rows
    feed_mouse(&rpc, "left", "release", sep_row - 4, 5);
    assert_eq!(
        win_height(&rpc, dock_win).await,
        h0 + 4,
        "the bottom dock grew as its edge moved up"
    );
    assert_eq!(
        current_win(&rpc).await,
        main_win,
        "the drag didn't move focus"
    );
}

/// Dragging a split divider *inside* a dock resizes that dock's windows — from the
/// main area, without crossing focus (the region-aware resize hit-test).
#[tokio::test]
async fn drag_resizes_a_split_inside_a_dock() {
    let (rpc, _incoming) = start(&numbered(100)).await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    // Split the dock horizontally: the new (top) window takes focus.
    feed(&rpc, "<C-w>s");
    let top = current_win(&rpc).await;
    // Cross to the main area so the dock is not focused.
    feed(&rpc, "<C-w><C-w>l");
    let main_win = current_win(&rpc).await;
    let top_h0 = win_height(&rpc, top).await;
    // The top dock window's own status row (its last content row, at `top_h0`) is a
    // resize handle because another dock window sits below it. Grab it and drag down.
    feed_mouse(&rpc, "left", "press", top_h0 as usize, 5);
    feed_mouse(&rpc, "left", "drag", top_h0 as usize + 3, 5);
    feed_mouse(&rpc, "left", "release", top_h0 as usize + 3, 5);
    assert_eq!(
        win_height(&rpc, top).await,
        top_h0 + 3,
        "the drag grew the dock's top window"
    );
    assert_eq!(
        current_win(&rpc).await,
        main_win,
        "focus stayed in the main area"
    );
}

// ── Phase 9: statusline click regions (%@handler@…%X) ───────────────────────

/// Set the global `'statusline'` via `vim.opt` (no `:set` escaping), through a
/// barrier so the option lands before the next gesture.
async fn set_statusline(rpc: &Rpc, fmt: &str) {
    exec_lua(rpc, &format!("vim.opt.statusline = {fmt:?}")).await;
}

/// The single window's status row (its last content row): a lone window sits at
/// y = 0, so the status line is at row `win_height` (text rows are `0..height`).
async fn lone_status_row(rpc: &Rpc) -> usize {
    win_height(rpc, current_win(rpc).await).await as usize
}

/// A left-click inside a `%@v:lua.Fn@…%X` region fires its handler with neovim's
/// click arguments: the `%N@` `minwid`, the click count, the button, and the
/// (here empty) modifier string.
#[tokio::test]
async fn statusline_click_region_fires_handler() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.sl = nil
        function _G.OnClick(minwid, clicks, button, mods)
          _G.sl = string.format("%d/%d/%s/%s", minwid, clicks, button, mods)
        end
        "#,
    )
    .await;
    // "AB" then the clickable "[X]" (minwid 7) then a right-aligned tail.
    set_statusline(&rpc, "AB%7@v:lua.OnClick@[X]%X%=end").await;
    let row = lone_status_row(&rpc).await;
    // "[X]" sits at columns 2,3,4 (after "AB"); col 3 is inside it.
    feed_mouse(&rpc, "left", "press", row, 3);
    let got = exec_lua(&rpc, "return _G.sl").await;
    assert_eq!(got.as_str(), Some("7/1/l/"), "handler ran with click args");
}

/// A click on the status line *outside* every region does not fire a handler (it
/// still focuses the window — already focused here, so the observable result is
/// simply that no handler ran).
#[tokio::test]
async fn statusline_click_outside_region_is_noop() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"_G.sl = nil; function _G.OnClick() _G.sl = "fired" end"#,
    )
    .await;
    set_statusline(&rpc, "AB%@v:lua.OnClick@[X]%X").await;
    let row = lone_status_row(&rpc).await;
    // Column 0 is on the literal "AB", before the region at columns 2..5.
    feed_mouse(&rpc, "left", "press", row, 0);
    let got = exec_lua(&rpc, "return _G.sl").await;
    assert_eq!(got, Value::Nil, "no handler fires outside a region");
}

/// Two regions on one status line resolve by the clicked column: each click fires
/// the handler whose span covers it, distinguished by `minwid`.
#[tokio::test]
async fn statusline_click_resolves_region_by_column() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"_G.last = nil; function _G.Pick(minwid) _G.last = minwid end"#,
    )
    .await;
    // "[L]" (minwid 1) at cols 0,1,2; a gap; "[R]" (minwid 2) at cols 5,6,7.
    set_statusline(&rpc, "%1@v:lua.Pick@[L]%X  %2@v:lua.Pick@[R]%X").await;
    let row = lone_status_row(&rpc).await;
    feed_mouse(&rpc, "left", "press", row, 1);
    assert_eq!(
        as_num(&exec_lua(&rpc, "return _G.last").await),
        1,
        "click in the left region"
    );
    feed_mouse(&rpc, "left", "press", row, 6);
    assert_eq!(
        as_num(&exec_lua(&rpc, "return _G.last").await),
        2,
        "click in the right region"
    );
}

/// A click handler's queued effects settle on the same gesture: a handler that
/// runs `vim.cmd` has its effect applied + driven to convergence by the click
/// dispatch (the mouse arm doesn't otherwise `run_pending`), so a following barrier
/// read already sees it rather than waiting for a keystroke.
#[tokio::test]
async fn statusline_click_handler_effects_settle() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"function _G.SetSw() vim.cmd("set shiftwidth=13") end"#,
    )
    .await;
    set_statusline(&rpc, "%@v:lua.SetSw@[go]%X").await;
    let row = lone_status_row(&rpc).await;
    feed_mouse(&rpc, "left", "press", row, 1);
    // The `exec_lua` request is a barrier ordered after the mouse notification, so
    // by the time it returns the handler's `set shiftwidth=13` has been applied.
    assert_eq!(
        as_num(&exec_lua(&rpc, "return vim.o.shiftwidth").await),
        13,
        "the handler's vim.cmd effect was applied + settled by the click"
    );
}

/// A double-click on a region reports `clicks = 2` (the same `'mousetime'`
/// multi-click machinery the text path uses), driven by a fake clock.
#[tokio::test]
async fn statusline_click_counts_double_click() {
    let (rpc, clock, _incoming) = start_clocked("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"_G.n = nil; function _G.Click(_, clicks) _G.n = clicks end"#,
    )
    .await;
    set_statusline(&rpc, "%@v:lua.Click@[X]%X").await;
    let row = lone_status_row(&rpc).await;
    // Two presses on the same cell within 'mousetime' (100ms apart) count as a
    // double-click.
    feed_mouse_at(&rpc, &clock, 0, "left", "press", row, 1);
    feed_mouse_at(&rpc, &clock, 100, "left", "press", row, 1);
    assert_eq!(
        as_num(&exec_lua(&rpc, "return _G.n").await),
        2,
        "second same-cell press inside 'mousetime' is a double-click"
    );
}

/// A non-`v:lua` click handler errors loud (CLAUDE.md no-silent-stub) rather than
/// being silently ignored — the failure surfaces on the message line.
#[tokio::test]
async fn statusline_click_bad_handler_errors_loud() {
    let (rpc, mut incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    // A bare name (not a `v:lua.` reference) is rejected by the bridge.
    set_statusline(&rpc, "%@NotVLua@[X]%X").await;
    let row = lone_status_row(&rpc).await;
    feed_mouse(&rpc, "left", "press", row, 1);
    let map = wait_redraw(&mut incoming, |m| message(m).contains("v:lua")).await;
    assert!(
        message(&map).contains("v:lua"),
        "the bad handler errored loud: {:?}",
        message(&map)
    );
}

// ── Phase 9b: clickable nx.statusline segments (on_click) ───────────────────

/// A left-click on a `nx.statusline` segment whose spec carries `on_click` fires
/// that handler — the segment analogue of the `%@…%X` format region, resolved
/// through the same click dispatch. The handler gets `(minwid=0, clicks, button,
/// mods)` (a segment has no `minwid`).
#[tokio::test]
async fn statusline_segment_on_click_fires() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.seg = nil
        function _G.OnSeg(minwid, clicks, button, mods)
          _G.seg = string.format("%d/%d/%s/%s", minwid, clicks, button, mods)
        end
        nx.statusline.segment{
          name = "clicky",
          on_click = "v:lua.OnSeg",
          render = function() return { { text = "[GIT]" } } end,
        }
        nx.statusline.setup{ left = { "clicky" } }
        "#,
    )
    .await;
    let row = lone_status_row(&rpc).await;
    // push_side writes a leading space, so "[GIT]" sits at columns 1..6; col 3 is
    // inside it.
    feed_mouse(&rpc, "left", "press", row, 3);
    assert_eq!(
        exec_lua(&rpc, "return _G.seg").await.as_str(),
        Some("0/1/l/"),
        "segment on_click fired with click args"
    );
}

/// A per-cell `on_click` resolves by column, and a cell with no handler (and no
/// segment-wide default) is not clickable.
#[tokio::test]
async fn statusline_segment_per_cell_on_click() {
    let (rpc, _incoming) = start("hello world\nsecond line\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    exec_lua(
        &rpc,
        r#"
        _G.hit = nil
        function _G.Pick(_, _, _, _) _G.hit = "L" end
        nx.statusline.segment{
          name = "two",
          render = function()
            return {
              { text = "[A]", on_click = "v:lua.Pick" },  -- clickable
              { text = "[B]" },                            -- not clickable
            }
          end,
        }
        nx.statusline.setup{ left = { "two" } }
        "#,
    )
    .await;
    let row = lone_status_row(&rpc).await;
    // " [A][B] " — "[A]" at cols 1..4 (clickable), "[B]" at cols 4..7 (not).
    feed_mouse(&rpc, "left", "press", row, 5); // inside [B]
    assert_eq!(
        exec_lua(&rpc, "return _G.hit").await,
        Value::Nil,
        "the non-clickable cell fires nothing"
    );
    feed_mouse(&rpc, "left", "press", row, 2); // inside [A]
    assert_eq!(
        exec_lua(&rpc, "return _G.hit").await.as_str(),
        Some("L"),
        "the per-cell on_click fired"
    );
}

// ── Phase 9c: laststatus=3 global-bar click regions ─────────────────────────

/// At `laststatus=3` the single global status bar (one row, full editor width,
/// the focused window's facts) carries click regions too: a `%@…%X` region on it
/// fires, resolved against the focused window at the full width.
#[tokio::test]
async fn statusline_global_bar_click_region_fires() {
    let (rpc, _incoming) = start("hello\nworld\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set laststatus=3").await;
    exec_lua(
        &rpc,
        r#"
        _G.g = nil
        function _G.OnGlobal(minwid, clicks, button, mods)
          _G.g = string.format("%d/%d/%s/%s", minwid, clicks, button, mods)
        end
        "#,
    )
    .await;
    set_statusline(&rpc, "AB%9@v:lua.OnGlobal@[X]%X%=tail").await;
    // At laststatus=3 the per-window status row is reclaimed as text; the single
    // global bar sits one row below the window text — at row `win_height`.
    let row = win_height(&rpc, current_win(&rpc).await).await as usize;
    // "[X]" sits at columns 2..5 (after "AB"); col 3 is inside it.
    feed_mouse(&rpc, "left", "press", row, 3);
    assert_eq!(
        exec_lua(&rpc, "return _G.g").await.as_str(),
        Some("9/1/l/"),
        "global-bar region fired with its minwid + click args"
    );
}

/// A click on the global bar *outside* every region is a no-op (no handler runs).
#[tokio::test]
async fn statusline_global_bar_click_outside_region_is_noop() {
    let (rpc, _incoming) = start("hello\nworld\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set laststatus=3").await;
    exec_lua(&rpc, r#"_G.g = nil; function _G.OnG() _G.g = "x" end"#).await;
    set_statusline(&rpc, "AB%@v:lua.OnG@[X]%X").await;
    let row = win_height(&rpc, current_win(&rpc).await).await as usize;
    feed_mouse(&rpc, "left", "press", row, 0); // on "AB", before the region
    assert_eq!(
        exec_lua(&rpc, "return _G.g").await,
        Value::Nil,
        "no handler fires outside a region on the global bar"
    );
}

/// The global bar also honours `nx.statusline` **segment** `on_click` (resolved at
/// the full editor width, against the focused window's layout).
#[tokio::test]
async fn statusline_global_bar_segment_click_fires() {
    let (rpc, _incoming) = start("hello\nworld\nthird").await;
    command(&rpc, "set nonumber norelativenumber").await;
    command(&rpc, "set laststatus=3").await;
    exec_lua(
        &rpc,
        r#"
        _G.s = nil
        function _G.OnSeg() _G.s = "seg" end
        nx.statusline.segment{
          name = "clicky",
          on_click = "v:lua.OnSeg",
          render = function() return { { text = "[GIT]" } } end,
        }
        nx.statusline.setup{ left = { "clicky" } }
        "#,
    )
    .await;
    let row = win_height(&rpc, current_win(&rpc).await).await as usize;
    // Leading space at col 0, "[GIT]" at cols 1..6; col 3 is inside it.
    feed_mouse(&rpc, "left", "press", row, 3);
    assert_eq!(
        exec_lua(&rpc, "return _G.s").await.as_str(),
        Some("seg"),
        "global-bar segment on_click fired"
    );
}

// ── Phase 9d: clickable custom tabline (%nT tab-select) ─────────────────────

/// A click on a `%nT` region of a custom `'tabline'` switches to that tab page —
/// the tabline analogue of the statusline click, resolved against the `'tabline'`
/// format at the full editor width.
#[tokio::test]
async fn custom_tabline_nt_click_switches_tab() {
    let (rpc, _incoming) = start_tabs().await; // 3 tabs; ccc.txt current
                                               // A custom tabline of three tab-select regions: "[1][2][3]". `%nT` opens tab
                                               // page n's region, `%T` ends the labels.
    exec_lua(&rpc, r#"vim.opt.tabline = "%1T[1]%2T[2]%3T[3]%T""#).await;
    assert_eq!(lines(&rpc).await, vec!["gamma"], "starts on tab 3");
    // "[1]" cols 0..3, "[2]" cols 3..6, "[3]" cols 6..9 — click inside "[1]".
    feed_mouse(&rpc, "left", "press", 0, 1);
    assert_eq!(lines(&rpc).await, vec!["alpha"], "clicked %1T → tab page 1");
    // Click inside "[2]".
    feed_mouse(&rpc, "left", "press", 0, 4);
    assert_eq!(lines(&rpc).await, vec!["beta"], "clicked %2T → tab page 2");
    assert_eq!(mode(&rpc).await, "n", "the click started no selection");
}

/// The original motivation: a `tabline = '%!v:lua.…'` builder that emits `%nT`
/// regions works verbatim — the Lua-produced format is re-parsed and its tab
/// regions are clickable.
#[tokio::test]
async fn custom_tabline_vlua_nt_click_switches_tab() {
    let (rpc, _incoming) = start_tabs().await;
    exec_lua(
        &rpc,
        r#"
        function _G.tabline()
          return "%1T aaa %2T bbb %3T ccc %T"
        end
        vim.opt.tabline = "%!v:lua.tabline()"
        "#,
    )
    .await;
    // " aaa " cols 0..5, " bbb " cols 5..10, " ccc " cols 10..15 — click " bbb ".
    feed_mouse(&rpc, "left", "press", 0, 7);
    assert_eq!(lines(&rpc).await, vec!["beta"], "clicked %2T → tab page 2");
}

/// A click on the custom tabline *outside* every `%nT` region (the `%T` fill) does
/// not switch tabs.
#[tokio::test]
async fn custom_tabline_click_outside_region_is_noop() {
    let (rpc, _incoming) = start_tabs().await;
    exec_lua(&rpc, r#"vim.opt.tabline = "%1T[1]%T   fill""#).await;
    assert_eq!(lines(&rpc).await, vec!["gamma"]);
    // "[1]" is cols 0..3; col 6 is in the non-clickable "   fill" past `%T`.
    feed_mouse(&rpc, "left", "press", 0, 6);
    assert_eq!(
        lines(&rpc).await,
        vec!["gamma"],
        "fill area switched nothing"
    );
}

/// Defense-in-depth at the wire-input boundary: a hostile near-`usize::MAX`
/// screen dimension (`nx_ui_try_resize`) and mouse coordinate (`nx_input_mouse`)
/// must NOT OOM/abort the server or wedge it in an effectively infinite
/// row-fill loop. The dispatch boundary clamps screen-cell dimensions/coords to
/// a sane ceiling (`MAX_SCREEN_DIM`), far above any real display, so an absurd
/// value is capped rather than driving a `usize::MAX`-element grid allocation in
/// the view. We assert survival: the server still answers a barrier with the
/// correct buffer after the hostile requests.
#[tokio::test]
async fn hostile_resize_and_mouse_coords_do_not_oom_the_server() {
    let (rpc, _incoming) = start("alpha\nbeta\ngamma").await;

    // A pathological resize: width/height near the integer ceiling. Without the
    // boundary clamp this sizes `Vec::with_capacity(height)` and a
    // `while rows.len() < height` filler loop in the view at ~`usize::MAX`.
    rpc.request(
        "nx_ui_try_resize",
        vec![Value::from(u64::MAX), Value::from(u64::MAX)],
    )
    .await
    .expect("the resize request is answered, not OOM/aborted");

    // A pathological mouse cell at the same magnitude (row/col params 4/5).
    rpc.notify(
        "nx_input_mouse",
        vec![
            Value::from("left"),
            Value::from("press"),
            Value::from(""),
            Value::from(0u64),
            Value::from(u64::MAX),
            Value::from(u64::MAX),
        ],
    );

    // The server is still alive and consistent: the buffer is intact and a
    // barrier round-trips. (If the hostile values had propagated, the redraw
    // following either request would have OOM'd / hung before this resolves.)
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta", "gamma"],
        "server survived the hostile geometry and serves the buffer"
    );
    assert_eq!(mode(&rpc).await, "n");
}

// ===== Soft-wrap: hit-test the right wrapped row =============================

/// Start a server with a **narrow** UI so a long line soft-wraps onto several
/// screen rows. `'wrap'` is on by default; the caller usually turns the number
/// gutter off so a global cell maps straight to (line, byte col).
async fn start_narrow(content: &str, cols: u16) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("mouse", "txt", content);
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, cols, 24).await;
    (rpc, incoming)
}

/// Clicking the **continuation row** of a soft-wrapped line lands on that line's
/// wrapped text — not on the next buffer line. This is the reported bug: the
/// hit-test counted one screen row per buffer line, so a click on a wrapped
/// line's second display row resolved to the line below it.
#[tokio::test]
async fn click_on_wrapped_continuation_row() {
    // "0123456789ABCDEFGHIJxyz" (23 chars) in a 20-col text area wraps to two
    // rows: cols 0..19 (bytes 0..20), then "xyz" at start_col 20 (bytes 20..23).
    let (rpc, _incoming) = start_narrow("0123456789ABCDEFGHIJxyz\nsecondline", 20).await;
    command(&rpc, "set nonumber norelativenumber wrap").await;
    // Screen row 1 is the wrapped continuation of line 1; col 1 is its 'y'.
    feed_mouse(&rpc, "left", "press", 1, 1);
    assert_eq!(
        cursor(&rpc).await,
        (1, 21),
        "continuation-row click lands on the wrapped line, not the next one"
    );
    // The first display row still maps straight through.
    feed_mouse(&rpc, "left", "press", 0, 5);
    assert_eq!(cursor(&rpc).await, (1, 5));
    // And the buffer's *second* line is on screen row 2 once the wrap is counted.
    feed_mouse(&rpc, "left", "press", 2, 3);
    assert_eq!(cursor(&rpc).await, (2, 3));
}

/// Dragging a Visual selection into the middle of a wrapped line keeps the cursor
/// aligned with the pointer — it doesn't jump to the next line. Mirrors the bug
/// report (visual-select with the mouse mid-wrapped-line).
#[tokio::test]
async fn drag_into_wrapped_continuation_stays_aligned() {
    let (rpc, _incoming) = start_narrow("0123456789ABCDEFGHIJxyz\nsecondline", 20).await;
    command(&rpc, "set nonumber norelativenumber wrap").await;
    feed_mouse(&rpc, "left", "press", 0, 2); // '2' on the first display row
    feed_mouse(&rpc, "left", "drag", 1, 1); // 'y' on the continuation row
    assert_eq!(mode(&rpc).await, "v");
    assert_eq!(
        cursor(&rpc).await,
        (1, 21),
        "the drag end tracks the pointer on the wrapped row"
    );
    // Inclusive [(1,2)..=(1,21)] deletes '2'..'y', leaving "01" + "z".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["01z", "secondline"]);
}
