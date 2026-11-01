//! Black-box tests for the gutter text column: the `gutter_text` extmark key, which
//! fills a column between the line numbers and the text (a diff's old and new line
//! numbers, a blame hash).
//!
//! The column's cells ride the window's `gutter_text` redraw key, one entry per
//! screen row, and its width rides `gutter_text_width`. The width is the widest
//! `gutter_text` in the whole buffer, not the viewport, so it is pinned here with a
//! mark that is off screen. Every core width sum (cursor, wrap, mouse) counts the
//! column, which is what a fake made of inline virtual text gets wrong (see
//! `docs/plans/2026-10-31-gutter-text.md`, Phase 0).

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{
    command, cursor, exec_lua, feed, feed_mouse, lines, start_with_file, wait_redraw, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Settle pending work, then the latest frame that carries the gutter text keys.
async fn frame(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<(Value, Value)> {
    let _ = exec_lua(rpc, "return 1").await;
    wait_redraw(inc, |m| window0_field(m, "gutter_text_width").is_some()).await
}

/// The column's width in the latest frame.
async fn width(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> u64 {
    let map = frame(rpc, inc).await;
    window0_field(&map, "gutter_text_width")
        .and_then(Value::as_u64)
        .expect("gutter_text_width")
}

/// Each screen row's gutter text, its chunks joined (`None` on a blank cell).
fn cells_of(map: &[(Value, Value)]) -> Vec<Option<String>> {
    window0_field(map, "gutter_text")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array().map(|chunks| {
                        chunks
                            .iter()
                            .filter_map(|c| c.as_array()?.first()?.as_str())
                            .collect::<String>()
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn cells(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<Option<String>> {
    cells_of(&frame(rpc, inc).await)
}

/// Each screen row's buffer line number (`None` for a virtual or filler row).
fn numbers_of(map: &[(Value, Value)]) -> Vec<Option<u64>> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// Put `text` in the gutter on 0-based `row`, under namespace `gt`, returning the id.
async fn put(rpc: &Rpc, row: usize, text: &str) -> u64 {
    exec_lua(
        rpc,
        &format!(
            "_G.gt = _G.gt or btv.ns.create('gt')
             return btv.buf.set_extmark(0, _G.gt, {row}, 0, {{ gutter_text = {{ {{ {text:?} }} }} }})"
        ),
    )
    .await
    .as_u64()
    .expect("mark id")
}

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[tokio::test]
async fn a_mark_fills_its_lines_cell_and_the_column_is_as_wide_as_it() {
    let (rpc, mut inc) = start_with_file(&numbered(5)).await;
    assert_eq!(width(&rpc, &mut inc).await, 0, "no mark, no column");
    put(&rpc, 1, " 12  13").await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(
        window0_field(&map, "gutter_text_width").and_then(Value::as_u64),
        Some(7)
    );
    let rows = cells_of(&map);
    assert_eq!(rows[0], None);
    assert_eq!(rows[1].as_deref(), Some(" 12  13"));
    assert_eq!(rows[2], None);
}

#[tokio::test]
async fn chunks_keep_their_groups_and_a_chunk_without_one_rides_as_nil() {
    let (rpc, mut inc) = start_with_file(&numbered(3)).await;
    exec_lua(
        &rpc,
        "btv.hl.define(0, 'GtOld', { fg = '#ff0000' })
         local ns = btv.ns.create('gt')
         btv.buf.set_extmark(0, ns, 0, 0, { gutter_text = { { '12', 'GtOld' }, { ' 13' } } })",
    )
    .await;
    let map = frame(&rpc, &mut inc).await;
    let row = window0_field(&map, "gutter_text")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(Value::as_array)
        .cloned()
        .expect("row 0 carries chunks");
    assert_eq!(row.len(), 2);
    let style = |i: usize| row[i].as_array().and_then(|c| c.get(1)).cloned();
    assert!(
        style(0).is_some_and(|s| s.as_u64().is_some()),
        "a grouped chunk resolves to a style id: {row:?}"
    );
    assert_eq!(
        style(1),
        Some(Value::Nil),
        "a group-less chunk paints as the numbers do"
    );
}

#[tokio::test]
async fn a_wrapped_line_shows_it_on_its_first_row_only() {
    let long = "x".repeat(200);
    let (rpc, mut inc) = start_with_file(&format!("{long}\nshort\n")).await;
    command(&rpc, "set wrap").await;
    put(&rpc, 0, "abc").await;
    put(&rpc, 1, "def").await;
    let map = frame(&rpc, &mut inc).await;
    let numbers = numbers_of(&map);
    let rows = cells_of(&map);
    // Line 1 wraps over several rows; only its first one carries the text.
    let first_of_two = numbers
        .iter()
        .position(|n| *n == Some(2))
        .expect("line 2 shown");
    assert!(first_of_two >= 3, "line 1 wraps: {numbers:?}");
    assert_eq!(rows[0].as_deref(), Some("abc"));
    for (i, row) in rows.iter().enumerate().take(first_of_two).skip(1) {
        assert_eq!(*row, None, "continuation row {i} stays blank");
    }
    assert_eq!(rows[first_of_two].as_deref(), Some("def"));
}

#[tokio::test]
async fn a_closed_fold_shows_its_first_lines_text() {
    let (rpc, mut inc) = start_with_file(&numbered(6)).await;
    put(&rpc, 1, "two").await;
    put(&rpc, 2, "three").await;
    put(&rpc, 4, "five").await;
    // Fold lines 2..4 (closed by `zf`).
    feed(&rpc, "jzfj");
    feed(&rpc, "j");
    let _ = lines(&rpc).await;
    let map = frame(&rpc, &mut inc).await;
    let numbers = numbers_of(&map);
    assert_eq!(
        &numbers[..4],
        &[Some(1), Some(2), Some(4), Some(5)],
        "the fold closed"
    );
    let rows = cells_of(&map);
    assert_eq!(
        rows[1].as_deref(),
        Some("two"),
        "the fold row shows its first line's"
    );
    assert_eq!(rows[2], None, "line 4 has none");
    assert_eq!(rows[3].as_deref(), Some("five"));
}

#[tokio::test]
async fn a_virtual_line_leaves_the_column_blank() {
    let (rpc, mut inc) = start_with_file(&numbered(3)).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('gt')
         btv.buf.set_extmark(0, ns, 0, 0, { gutter_text = { { 'one' } },
           virt_lines = { { { 'virtual' } } } })",
    )
    .await;
    let map = frame(&rpc, &mut inc).await;
    let numbers = numbers_of(&map);
    assert_eq!(
        &numbers[..3],
        &[Some(1), None, Some(2)],
        "a virtual row under line 1"
    );
    let rows = cells_of(&map);
    assert_eq!(rows[0].as_deref(), Some("one"));
    assert_eq!(rows[1], None, "the virtual row is blank");
}

#[tokio::test]
async fn the_highest_priority_wins_then_the_latest() {
    let (rpc, mut inc) = start_with_file(&numbered(3)).await;
    exec_lua(
        &rpc,
        "local a, b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 0, 0, { gutter_text = { { 'high' } }, priority = 5000 })
         btv.buf.set_extmark(0, b, 0, 0, { gutter_text = { { 'low' } }, priority = 10 })
         btv.buf.set_extmark(0, a, 1, 0, { gutter_text = { { 'first' } } })
         btv.buf.set_extmark(0, a, 1, 0, { gutter_text = { { 'second' } } })",
    )
    .await;
    let rows = cells(&rpc, &mut inc).await;
    assert_eq!(rows[0].as_deref(), Some("high"), "they don't concatenate");
    assert_eq!(rows[1].as_deref(), Some("second"));
}

#[tokio::test]
async fn the_width_follows_the_widest_mark_in_the_buffer_even_off_screen() {
    let (rpc, mut inc) = start_with_file(&numbered(300)).await;
    let near = put(&rpc, 0, "abc").await;
    assert_eq!(width(&rpc, &mut inc).await, 3);
    // Line 250 is far below the 80x24 viewport.
    let far = put(&rpc, 249, "abcdefghij").await;
    assert_eq!(
        width(&rpc, &mut inc).await,
        10,
        "an off-screen mark widens it"
    );
    let rows = cells(&rpc, &mut inc).await;
    assert_eq!(rows[0].as_deref(), Some("abc"));
    exec_lua(&rpc, &format!("btv.buf.del_extmark(0, _G.gt, {far})")).await;
    assert_eq!(
        width(&rpc, &mut inc).await,
        3,
        "deleting the widest narrows it"
    );
    // Replacing a mark under its id swaps the old width for the new.
    exec_lua(
        &rpc,
        &format!("btv.buf.set_extmark(0, _G.gt, 0, 0, {{ id = {near}, gutter_text = {{ {{ 'ab' }} }} }})"),
    )
    .await;
    assert_eq!(width(&rpc, &mut inc).await, 2);
    put(&rpc, 10, "wide wide").await;
    put(&rpc, 20, "wide wide").await;
    assert_eq!(width(&rpc, &mut inc).await, 9);
    exec_lua(&rpc, "btv.buf.clear_namespace(0, _G.gt, 15, -1)").await;
    assert_eq!(
        width(&rpc, &mut inc).await,
        9,
        "one of the two widest is left"
    );
    exec_lua(&rpc, "btv.buf.clear_namespace(0, _G.gt, 0, -1)").await;
    assert_eq!(
        width(&rpc, &mut inc).await,
        0,
        "clearing all removes the column"
    );
}

#[tokio::test]
async fn the_width_is_measured_in_display_cells() {
    let (rpc, mut inc) = start_with_file(&numbered(3)).await;
    // Two wide CJK characters: 6 bytes, 2 chars, 4 cells.
    put(&rpc, 0, "日本").await;
    assert_eq!(width(&rpc, &mut inc).await, 4);
}

#[tokio::test]
async fn marks_follow_edits_and_undo() {
    let (rpc, mut inc) = start_with_file(&numbered(5)).await;
    put(&rpc, 2, "three").await;
    assert_eq!(cells(&rpc, &mut inc).await[2].as_deref(), Some("three"));
    // Open a line above line 1: the mark moves down with its line.
    feed(&rpc, "ggOnew<Esc>");
    let _ = lines(&rpc).await;
    let rows = cells(&rpc, &mut inc).await;
    assert_eq!(rows[2], None);
    assert_eq!(
        rows[3].as_deref(),
        Some("three"),
        "the mark moved with line 3"
    );
    feed(&rpc, "u");
    let _ = lines(&rpc).await;
    let rows = cells(&rpc, &mut inc).await;
    assert_eq!(rows[2].as_deref(), Some("three"), "undo moves it back");
    assert_eq!(
        width(&rpc, &mut inc).await,
        5,
        "undo keeps the mark, and its width"
    );
}

#[tokio::test]
async fn the_cursor_screen_column_counts_the_column() {
    let (rpc, mut inc) = start_with_file("hello world\nsecond\n").await;
    command(&rpc, "set nonumber norelativenumber signcolumn=no").await;
    feed(&rpc, "w");
    let _ = width(&rpc, &mut inc).await;
    let bare = exec_lua(&rpc, "return btv.screen.col()").await.as_u64();
    assert_eq!(bare, Some(7), "the `w` of world, 1-based, with no gutter");
    put(&rpc, 0, "1234").await;
    assert_eq!(width(&rpc, &mut inc).await, 4);
    let with = exec_lua(&rpc, "return btv.screen.col()").await.as_u64();
    assert_eq!(with, Some(11), "four cells further right");
}

#[tokio::test]
async fn a_click_skips_the_column() {
    let (rpc, mut inc) = start_with_file("hello world\nsecond line\n").await;
    command(&rpc, "set nonumber norelativenumber signcolumn=no").await;
    put(&rpc, 0, " 12  13 ").await;
    assert_eq!(width(&rpc, &mut inc).await, 8);
    // The `w` of world is painted at screen column 8 + 6.
    feed_mouse(&rpc, "left", "press", 0, 14);
    feed_mouse(&rpc, "left", "release", 0, 14);
    assert_eq!(cursor(&rpc).await, (1, 6));
    // A click on the column itself lands on the line's first character, as a click
    // on a line number does.
    feed_mouse(&rpc, "left", "press", 1, 3);
    feed_mouse(&rpc, "left", "release", 1, 3);
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn wrapping_wraps_into_the_text_width_left_by_the_column() {
    // 80 columns, no other gutter: a 76-cell line fits on one row until an 8-cell
    // column takes its share, and then it wraps onto a second.
    let (rpc, mut inc) = start_with_file(&format!("{}\nnext\n", "x".repeat(76))).await;
    command(&rpc, "set wrap nonumber norelativenumber signcolumn=no").await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(&numbers_of(&map)[..2], &[Some(1), Some(2)]);
    put(&rpc, 1, "12345678").await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(
        &numbers_of(&map)[..3],
        &[Some(1), Some(1), Some(2)],
        "line 1 now takes two rows"
    );
}

#[tokio::test]
async fn nowrap_scrolls_by_the_text_width_left_by_the_column() {
    // `$` on a long line scrolls so the last character sits in the last text column;
    // with a 10-cell column the text area is 10 cells narrower, so it scrolls 10
    // further.
    let (rpc, mut inc) = start_with_file(&format!("{}\n", "y".repeat(300))).await;
    command(&rpc, "set nowrap nonumber norelativenumber signcolumn=no").await;
    feed(&rpc, "$");
    let map = frame(&rpc, &mut inc).await;
    let bare = window0_field(&map, "leftcol")
        .and_then(Value::as_u64)
        .unwrap();
    feed(&rpc, "0");
    put(&rpc, 0, "0123456789").await;
    feed(&rpc, "$");
    let map = frame(&rpc, &mut inc).await;
    let with = window0_field(&map, "leftcol")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(with, bare + 10);
    // The column itself never scrolls: its cell is still sent for the row.
    assert_eq!(cells_of(&map)[0].as_deref(), Some("0123456789"));
}

/// The error a `set_extmark` with `opts` raises, as text.
async fn error_for(rpc: &Rpc, opts: &str) -> String {
    let v = exec_lua(
        rpc,
        &format!(
            "local ok, err = pcall(btv.buf.set_extmark, 0, btv.ns.create('bad'), 0, 0, {opts})
             return ok and 'no error' or tostring(err)"
        ),
    )
    .await;
    v.as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn a_malformed_mark_raises_naming_the_key() {
    let (rpc, _inc) = start_with_file("one\n").await;
    for (opts, want) in [
        ("{ gutter_text = 'oops' }", "gutter_text must be a list"),
        (
            "{ gutter_text = { 'oops' } }",
            "gutter_text[1] must be { text, group? }",
        ),
        (
            "{ gutter_text = { { 12 } } }",
            "gutter_text[1] must be { text, group? }",
        ),
        (
            "{ gutter_text = { { 'a', 3 } } }",
            "gutter_text[1]: the group must be a string",
        ),
        (
            "{ gutter_text = { { 'a' }, { 'b\\nc' } } }",
            "gutter_text[2] may not contain a newline",
        ),
        (
            "{ gutter_text = { { 'a\\tb' } } }",
            "gutter_text[1] may not contain a newline or a tab",
        ),
        (
            "{ gutter_text = { text = 'a' } }",
            "gutter_text must be a list",
        ),
    ] {
        let err = error_for(&rpc, opts).await;
        assert!(err.contains(want), "{opts}: got {err:?}, want {want:?}");
    }
    // A well-formed one raises nothing.
    assert_eq!(
        error_for(&rpc, "{ gutter_text = { { 'a', 'LineNr' } } }").await,
        "no error"
    );
}

#[tokio::test]
async fn a_details_read_after_the_tick_returns_it() {
    let (rpc, mut inc) = start_with_file("one\n").await;
    put(&rpc, 0, "x").await;
    let _ = frame(&rpc, &mut inc).await;
    let v = exec_lua(
        &rpc,
        "local m = btv.buf.extmarks(0, _G.gt, 0, -1, { details = true })[1]
         return m[4].gutter_text[1][1]",
    )
    .await;
    assert_eq!(v.as_str(), Some("x"));
}

#[tokio::test]
async fn a_decor_provider_can_publish_it() {
    let (rpc, mut inc) = start_with_file(&numbered(3)).await;
    exec_lua(
        &rpc,
        "btv.decor.provider{ name = 'gutter', on_range = function(ctx, publish)
           publish { { ctx.top, 0, gutter_text = { { 'top' } } } }
         end }
         btv.decor.invalidate()",
    )
    .await;
    let mut rows = Vec::new();
    for _ in 0..50 {
        rows = cells(&rpc, &mut inc).await;
        if rows.first().is_some_and(Option::is_some) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(rows.first().cloned().flatten().as_deref(), Some("top"));
}

/// How long 300 keystrokes take in a 20 000-line buffer with `marks` marks set
/// from `opts` (a Lua table body that may use the line index `i`), one per line from
/// the top: only ~23 of them are on screen.
async fn typing_cost(marks: usize, opts: &str) -> std::time::Duration {
    let (rpc, _inc) = start_with_file(&numbered(20_000)).await;
    exec_lua(
        &rpc,
        &format!(
            "local ns = btv.ns.create('bench')
             for i = 0, {} do btv.buf.set_extmark(0, ns, i, 0, {{ {opts} }}) end",
            marks - 1
        ),
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for _ in 0..300 {
        feed(&rpc, "j");
    }
    let _ = exec_lua(&rpc, "return 1").await;
    started.elapsed()
}

#[tokio::test]
async fn the_column_costs_a_frame_no_more_than_signs_do() {
    // A ratio, not a wall clock, so a loaded `cargo test --workspace` doesn't flake
    // it. The column's width costs O(1) per frame (the store's per-width counts, not
    // a scan) and a buffer with no column skips its projection outright; its marks are
    // found through the store's position index like every other layer's. This pins
    // the column's own cost against the sign layer's at the same mark count, while
    // typing (each keystroke an edit); the off-screen cost of every layer is pinned in
    // `extmark_render.rs`.
    let signs = typing_cost(20_000, "sign_text = '>>'").await;
    let gutter = typing_cost(20_000, "gutter_text = { { tostring(i) } }").await;
    let ratio = gutter.as_secs_f64() / signs.as_secs_f64();
    assert!(
        ratio < 1.5,
        "moving with 20000 gutter marks cost {ratio:.1}x 20000 sign marks ({gutter:?} vs {signs:?})"
    );
}

#[tokio::test]
async fn a_control_character_raises_like_a_newline() {
    // An ESC, a DEL or an 8-bit CSI (U+009B) has no cell of its own: the frame ships
    // it scrubbed to its `^[` / `<9b>` spelling, wider than the width the column was
    // measured at, so the text would be cut short. It fails loud instead, as a
    // newline does — on every surface that sets extmarks.
    let (rpc, _inc) = start_with_file("one\n").await;
    for text in ["a\\27[31mb", "\\127", "x\\194\\155y"] {
        let opts = format!("{{ gutter_text = {{ {{ 'ok' }}, {{ '{text}' }} }} }}");
        let err = error_for(&rpc, &opts).await;
        assert!(
            err.contains("gutter_text[2] may not contain a control character"),
            "{opts}: got {err:?}"
        );
    }
    // The Rust lowering refuses it too, for a caller that reaches it without the
    // prelude's splitter (`btv.view:set_decor`'s batch, a direct bridge call).
    let raw = exec_lua(
        &rpc,
        "local ok, err = pcall(btv._extmark_set, btv._resolve_bufnr(0), btv.ns.create('raw'), 1, 0, 0, nil, nil,
           nil, 4096, { gutter_text = { { 'a\\27b' } } })
         return ok and 'no error' or tostring(err)",
    )
    .await;
    let raw = raw.as_str().unwrap_or_default().to_owned();
    assert!(raw.contains("control character"), "bridge: got {raw:?}");
    // Printable text, wide or combining, still goes through.
    assert_eq!(
        error_for(&rpc, "{ gutter_text = { { '日本 é' } } }").await,
        "no error"
    );
}

#[tokio::test]
async fn a_huge_gutter_text_ships_no_more_than_the_window_can_show() {
    // A plugin's gutter text is unbounded; the frame is not. The column can never be
    // wider than the window, so each row ships at most the window's width of it, not
    // the whole string on every row of every frame.
    let (rpc, mut inc) = start_with_file(&numbered(30)).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('gt')
         for i = 0, 29 do
           btv.buf.set_extmark(0, ns, i, 0, { gutter_text = { { string.rep('w', 50000) }, { 'tail', 'LineNr' } } })
         end",
    )
    .await;
    let map = frame(&rpc, &mut inc).await;
    let rows = cells_of(&map);
    assert!(
        rows.iter().filter(|r| r.is_some()).count() >= 20,
        "{}",
        rows.len()
    );
    for (i, row) in rows.iter().enumerate() {
        if let Some(text) = row {
            assert!(text.len() <= 80, "row {i} shipped {} bytes", text.len());
        }
    }
    // Nor does the width: a client pads every row to it.
    let w = window0_field(&map, "gutter_text_width")
        .and_then(Value::as_u64)
        .expect("gutter_text_width");
    assert!(w <= 80, "shipped a {w}-cell column");
    // A short one is shipped whole.
    exec_lua(
        &rpc,
        "btv.buf.clear_namespace(0, btv.ns.create('gt'), 0, -1)",
    )
    .await;
    put(&rpc, 0, "abc").await;
    assert_eq!(cells(&rpc, &mut inc).await[0].as_deref(), Some("abc"));
}

#[tokio::test]
async fn a_column_wider_than_the_window_leaves_the_editor_responsive() {
    // The column can take the whole content box, leaving the text no cells at all.
    // Moving, typing, wrapping and scrolling sideways against a zero text width must
    // neither panic nor spin.
    let (rpc, mut inc) = start_with_file(&numbered(40)).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('gt')
         btv.buf.set_extmark(0, ns, 39, 0, { gutter_text = { { string.rep('w', 500) } } })",
    )
    .await;
    for wrap in ["wrap", "nowrap"] {
        command(&rpc, &format!("set {wrap}")).await;
        feed(&rpc, "gg$jj10lAtyped text here<Esc>0zl$G<C-e>gg");
        let _ = frame(&rpc, &mut inc).await;
        assert!(lines(&rpc).await[2].ends_with("typed text here"), "{wrap}");
    }
}

#[tokio::test]
async fn a_list_that_is_empty_or_more_than_its_chunks_raises() {
    // Each of these would otherwise be accepted and quietly paint less than it says:
    // an empty list paints nothing (and passes a provider's "draws nothing" check), a
    // chunk past a hole or a named key beside the list is never visited, and a third
    // slot in a chunk is ignored.
    let (rpc, _inc) = start_with_file("one\n").await;
    for (opts, want) in [
        ("{ gutter_text = {} }", "gutter_text must be a list of"),
        (
            "{ gutter_text = { [1] = { 'a' }, [3] = { 'b' } } }",
            "gutter_text must be a list of",
        ),
        (
            "{ gutter_text = { { 'a' }, foo = 1 } }",
            "gutter_text must be a list of",
        ),
        (
            "{ gutter_text = { { 'a', 'LineNr', 'x' } } }",
            "gutter_text[1] must be { text, group? }",
        ),
        (
            "{ gutter_text = { { 'a', group = 'LineNr' } } }",
            "gutter_text[1] must be { text, group? }",
        ),
    ] {
        let err = error_for(&rpc, opts).await;
        assert!(err.contains(want), "{opts}: got {err:?}, want {want:?}");
    }
}

#[tokio::test]
async fn a_provider_publishing_an_empty_one_reports_it() {
    // A provider's mark must draw something; an empty `gutter_text` draws nothing, so
    // it fails loud through the provider-error path rather than vanishing.
    let (rpc, mut inc) = start_with_file(&numbered(3)).await;
    exec_lua(
        &rpc,
        "_G.notes = {}
         local notify = btv.notify
         btv.notify = function(msg, ...) table.insert(_G.notes, msg); return notify(msg, ...) end
         btv.decor.provider{ name = 'empty', on_range = function(ctx, publish)
           publish { { ctx.top, 0, gutter_text = {} } }
         end }
         btv.decor.invalidate()",
    )
    .await;
    let mut notes = String::new();
    for _ in 0..50 {
        let _ = frame(&rpc, &mut inc).await;
        notes = exec_lua(&rpc, "return table.concat(_G.notes, '|')")
            .await
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !notes.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        notes.contains("provider 'empty'") && notes.contains("gutter_text"),
        "{notes:?}"
    );
}

#[tokio::test]
async fn the_bridge_refuses_a_malformed_one_naming_the_key() {
    // `btv._extmark_set` (and `btv._decor_publish`, which lowers through the same
    // converter) is reached past the prelude's splitter by `view:set_decor`'s batch
    // and the decor path, so it holds the same line: a number is not text, a hole
    // drops nothing, and every error names `gutter_text`.
    let (rpc, _inc) = start_with_file("one\n").await;
    for (decoration, want) in [
        ("{ gutter_text = 'oops' }", "gutter_text must be a list"),
        ("{ gutter_text = { 'oops' } }", "gutter_text[1] must be"),
        ("{ gutter_text = { { 12 } } }", "gutter_text[1] must be"),
        ("{ gutter_text = { { 'a', 3 } } }", "gutter_text[1]"),
        (
            "{ gutter_text = { [1] = { 'a' }, [3] = { 'b' } } }",
            "gutter_text must be a list",
        ),
        ("{ gutter_text = {} }", "gutter_text must be a list"),
    ] {
        let raw = exec_lua(
            &rpc,
            &format!(
                "local ok, err = pcall(btv._extmark_set, btv._resolve_bufnr(0), btv.ns.create('raw'),
                   1, 0, 0, nil, nil, nil, 4096, {decoration})
                 return ok and 'no error' or tostring(err)"
            ),
        )
        .await;
        let raw = raw.as_str().unwrap_or_default().to_owned();
        assert!(
            raw.contains(want),
            "{decoration}: got {raw:?}, want {want:?}"
        );
    }
}

#[tokio::test]
async fn a_column_wider_than_the_window_still_fits_beside_the_other_gutters() {
    // The clients lay the gutters out side by side — fold, sign, number, then this
    // column — and start the text after them, so the shipped widths must add up to no
    // more than the window. A column capped at the whole content box, beside a number
    // gutter, put the text origin past the window's right edge and painted the
    // column's backgrounds into the window beside it.
    let (rpc, mut inc) = start_with_file(&numbered(40)).await;
    command(&rpc, "set number foldcolumn=2 signcolumn=yes").await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('gt')
         btv.buf.set_extmark(0, ns, 0, 0, { gutter_text = { { string.rep('w', 500), 'DiffDelete' } } })",
    )
    .await;
    let map = frame(&rpc, &mut inc).await;
    let field = |k: &str| {
        window0_field(&map, k)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("{k}"))
    };
    let window = window0_field(&map, "rect")
        .and_then(|r| match r {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some("width"))
                .and_then(|(_, v)| v.as_u64()),
            _ => None,
        })
        .expect("rect width");
    let (fold, sign, number) = (
        field("foldcolumn_width"),
        field("sign_width"),
        field("number_width"),
    );
    assert!(fold > 0 && sign > 0 && number > 0, "{fold} {sign} {number}");
    let column = field("gutter_text_width");
    assert_eq!(
        fold + sign + number + column,
        window,
        "the column takes what the other gutters leave, no more \
         (fold {fold}, sign {sign}, number {number}, column {column})"
    );
    // …and the row ships no more than that either.
    let row = cells_of(&map)[0].clone().expect("row 0's text");
    assert_eq!(row.chars().count() as u64, column);
}

#[tokio::test]
async fn the_scroll_band_carries_each_rows_text_beside_its_line() {
    // A scroll slides the band, not the settled window, so the column must ride the
    // band row for row, or a diff's numbers blank out (or stick) for the slide.
    let (rpc, mut inc) = start_with_file(&numbered(100)).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('gt')
         for i = 0, 99 do btv.buf.set_extmark(0, ns, i, 0, { gutter_text = { { 'g' .. (i + 1) } } }) end",
    )
    .await;
    let _ = frame(&rpc, &mut inc).await;
    let map = bemtvi_test_harness::redraw_after_matching(&rpc, &mut inc, "<C-d>", |m| {
        matches!(window0_field(m, "scroll"), Some(Value::Map(_)))
    })
    .await;
    let Some(Value::Map(band)) = window0_field(&map, "scroll") else {
        panic!("no scroll band");
    };
    let get = |k: &str| {
        band.iter()
            .find(|(key, _)| key.as_str() == Some(k))
            .and_then(|(_, v)| v.as_array())
            .unwrap_or_else(|| panic!("band {k}"))
    };
    let (numbers, texts) = (get("numbers"), get("gutter_text"));
    assert_eq!(numbers.len(), texts.len(), "one entry per band row");
    let mut seen = 0;
    for (n, t) in numbers.iter().zip(texts) {
        let Some(n) = n.as_u64() else { continue };
        let text: String = t
            .as_array()
            .expect("a line's row carries its text")
            .iter()
            .filter_map(|c| c.as_array()?.first()?.as_str())
            .collect();
        assert_eq!(text, format!("g{n}"), "band row of line {n}");
        seen += 1;
    }
    assert!(seen > 24, "the band spans more than the viewport ({seen})");
}

#[tokio::test]
async fn a_wide_character_cut_by_the_cap_leaves_its_cells_in_the_chunks_style() {
    // A row cut at the window's edge mid wide character keeps its chunk's style over
    // the cell the character can't fill, as a client's own fit of a too-long row
    // does (`fit_gutter_text`): a diff's background band reaches the edge rather
    // than stopping one cell short in the number column's colours.
    let (rpc, mut inc) = start_with_file("one\n").await;
    command(
        &rpc,
        "set nonumber norelativenumber signcolumn=no foldcolumn=0",
    )
    .await;
    let window = {
        let map = frame(&rpc, &mut inc).await;
        window0_field(&map, "rect")
            .and_then(|r| match r {
                Value::Map(m) => m
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("width"))
                    .and_then(|(_, v)| v.as_u64()),
                _ => None,
            })
            .expect("rect width") as usize
    };
    exec_lua(
        &rpc,
        &format!(
            "btv.hl.define(0, 'GtBand', {{ bg = '#402020' }})
             local ns = btv.ns.create('gt')
             btv.buf.set_extmark(0, ns, 0, 0, {{ gutter_text = {{ {{ string.rep('w', {}) .. '日', 'GtBand' }} }} }})",
            window - 1
        ),
    )
    .await;
    let map = frame(&rpc, &mut inc).await;
    let column = window0_field(&map, "gutter_text_width")
        .and_then(Value::as_u64)
        .expect("gutter_text_width") as usize;
    assert_eq!(column, window, "capped at the window");
    let row = window0_field(&map, "gutter_text")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(Value::as_array)
        .cloned()
        .expect("row 0 carries chunks");
    let styled: usize = row
        .iter()
        .filter_map(|c| {
            let c = c.as_array()?;
            c.get(1)?.as_u64()?;
            Some(c.first()?.as_str()?.chars().count())
        })
        .sum();
    assert_eq!(
        styled, column,
        "every cell of the cut row is in the chunk's style: {row:?}"
    );
}
