//! `btv.decor.expr` — the frame-time paint block.
//!
//! The pure sibling of `btv.decor.provider`: a sandbox block handed one line and
//! its number, returning the spans to highlight, evaluated *during* the frame
//! rather than off it. Black-box throughout — the assertions are on the
//! `highlights` array a client paints from, which is where a decoration either
//! arrives or doesn't.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{
    drain_to_latest_redraw, exec_lua, feed, feed_sync, message, start_with_file_and_config,
    temp_dir, wait_redraw, window0_field, write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// A block that paints every `TODO` on the line with the `Todo` group — the
/// archetypal pure paint, and the shape the docs show.
const TODO_PAINT: &str = r#"btv.decor.expr([[
  local out, i = {}, 1
  while true do
    local s, e = line:find("TODO", i, true)
    if not s then break end
    out[#out + 1] = { s, e, "Todo" }
    i = e + 1
  end
  return out
]])"#;

/// Every highlight span the frame carries for `row`, as `(start, end, group)`.
fn spans_on(map: &[(Value, Value)], row: usize) -> Vec<(u64, u64, String)> {
    window0_field(map, "highlights")
        .and_then(Value::as_array)
        .and_then(|rows| rows.get(row))
        .and_then(Value::as_array)
        .map(|spans| {
            spans
                .iter()
                .filter_map(|s| {
                    let s = s.as_array()?;
                    Some((
                        s.first()?.as_u64()?,
                        s.get(1)?.as_u64()?,
                        s.get(2)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The spans carrying `group` on `row`, once the pending work has settled.
async fn painted(
    rpc: &Rpc,
    inc: &mut UnboundedReceiver<Incoming>,
    row: usize,
    group: &str,
) -> Vec<(u64, u64)> {
    let _ = exec_lua(rpc, "return 1").await;
    let map = wait_redraw(inc, |m| window0_field(m, "highlights").is_some()).await;
    spans_on(&map, row)
        .into_iter()
        .filter(|(_, _, g)| g == group)
        .map(|(s, e, _)| (s, e))
        .collect()
}

async fn start(name: &str, content: &str, init: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir(name);
    let file = write_temp(name, "txt", content);
    start_with_file_and_config(&dir, &file, init).await
}

// ===== painting ==============================================================

#[tokio::test]
async fn a_paint_block_highlights_its_match() {
    let (rpc, mut inc) = start("paint_basic", "ab TODO cd\n", TODO_PAINT).await;
    assert_eq!(
        painted(&rpc, &mut inc, 0, "Todo").await,
        vec![(3, 7)],
        "the 1-based inclusive columns `string.find` returns become the span"
    );
}

#[tokio::test]
async fn several_matches_on_one_line_each_paint() {
    let (rpc, mut inc) = start("paint_many", "TODO and TODO\n", TODO_PAINT).await;
    assert_eq!(
        painted(&rpc, &mut inc, 0, "Todo").await,
        vec![(0, 4), (9, 13)]
    );
}

#[tokio::test]
async fn a_line_with_no_match_paints_nothing() {
    let (rpc, mut inc) = start("paint_none", "TODO here\nnothing here\n", TODO_PAINT).await;
    assert_eq!(painted(&rpc, &mut inc, 0, "Todo").await, vec![(0, 4)]);
    assert!(painted(&rpc, &mut inc, 1, "Todo").await.is_empty());
}

#[tokio::test]
async fn lnum_is_in_scope() {
    // Paint the whole line, but only the third one.
    let init = r#"btv.decor.expr([[ if lnum ~= 3 then return {} end return { { 1, #line, "Search" } } ]])"#;
    let (rpc, mut inc) = start("paint_lnum", "one\ntwo\nthree\nfour\n", init).await;
    assert!(painted(&rpc, &mut inc, 1, "Search").await.is_empty());
    assert_eq!(painted(&rpc, &mut inc, 2, "Search").await, vec![(0, 5)]);
}

#[tokio::test]
async fn an_edit_repaints_the_line_in_the_same_frame() {
    let (rpc, mut inc) = start("paint_edit", "TODO x\n", TODO_PAINT).await;
    assert_eq!(painted(&rpc, &mut inc, 0, "Todo").await, vec![(0, 4)]);
    // Typing two characters in front of it moves the match; the paint follows on
    // the frame the edit landed in.
    feed_sync(&rpc, "iab<Esc>").await;
    assert_eq!(
        painted(&rpc, &mut inc, 0, "Todo").await,
        vec![(2, 6)],
        "the repaint tracks the edited line"
    );
}

#[tokio::test]
async fn a_new_match_typed_into_the_line_paints() {
    let (rpc, mut inc) = start("paint_new", "x\n", TODO_PAINT).await;
    assert!(painted(&rpc, &mut inc, 0, "Todo").await.is_empty());
    feed_sync(&rpc, "ITODO <Esc>").await;
    assert_eq!(painted(&rpc, &mut inc, 0, "Todo").await, vec![(0, 4)]);
}

#[tokio::test]
async fn scrolling_paints_the_rows_that_came_into_view() {
    // 60 lines, only every 30th carrying a match, so the second one is off screen
    // in an 80x24 window until the scroll.
    let mut content = String::new();
    for i in 0..60 {
        content.push_str(if i % 30 == 29 { "TODO far\n" } else { "x\n" });
    }
    let (rpc, mut inc) = start("paint_scroll", &content, TODO_PAINT).await;
    assert_eq!(painted(&rpc, &mut inc, 0, "Todo").await, Vec::new());
    // Park line 30 (the match) at the top of the window.
    feed_sync(&rpc, "30Gzt").await;
    assert_eq!(
        painted(&rpc, &mut inc, 0, "Todo").await,
        vec![(0, 4)],
        "a row scrolled into view is painted"
    );
}

#[tokio::test]
async fn a_multibyte_line_is_painted_on_character_boundaries() {
    // A span whose columns land inside the multi-byte `é` must clamp rather than
    // splitting it (which would be an invalid extmark).
    let init = r#"btv.decor.expr([[ return { { 1, 3, "Search" } } ]])"#;
    let (rpc, mut inc) = start("paint_utf8", "éx\n", init).await;
    // `é` is two bytes, so the requested columns cover the whole line: two display
    // cells, and no span splitting the character.
    assert_eq!(painted(&rpc, &mut inc, 0, "Search").await, vec![(0, 2)]);
}

// ===== decoration, not only colour ===========================================

/// The `virt_text` placements the frame carries, as `(pos, col, text)` — the wire
/// shape is `[pos, col, hl_mode, [[text, style], …]]`, and the style is a palette
/// id, so a test asserts on the text and where it draws.
fn virt_on(map: &[(Value, Value)], row: usize) -> Vec<(u64, u64, String)> {
    window0_field(map, "virt_text")
        .and_then(Value::as_array)
        .and_then(|rows| rows.get(row))
        .and_then(Value::as_array)
        .map(|places| {
            places
                .iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    let text = p
                        .get(3)?
                        .as_array()?
                        .iter()
                        .filter_map(|c| c.as_array()?.first()?.as_str())
                        .collect::<String>();
                    Some((p.first()?.as_u64()?, p.get(1)?.as_u64()?, text))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The gutter sign glyph on `row`, if any.
fn sign_on(map: &[(Value, Value)], row: usize) -> Option<String> {
    window0_field(map, "diagnostics_signs")
        .and_then(Value::as_array)
        .and_then(|rows| rows.get(row))
        .and_then(Value::as_array)
        .and_then(|s| s.first())
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The screen rows carrying a `line_bg` layer.
fn line_bg_rows(map: &[(Value, Value)]) -> Vec<u64> {
    window0_field(map, "line_bg")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.as_array()?.first()?.as_u64())
                .collect()
        })
        .unwrap_or_default()
}

/// Every virtual *line* row the frame carries, as `(screen row, text)`. A virtual
/// line is a whole row of its own, so it reads blank in `lines` and its text rides
/// this layer instead.
fn virt_line_rows(map: &[(Value, Value)]) -> Vec<(usize, String)> {
    window0_field(map, "virt_lines")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let text = r
                        .as_array()?
                        .iter()
                        .filter_map(|c| c.as_array()?.first()?.as_str())
                        .collect::<String>();
                    (!text.is_empty()).then_some((i, text))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `hl_mode` code of the first `virt_text` placement on `row` (`0` replace,
/// `1` combine, `2` blend).
fn virt_hl_mode(map: &[(Value, Value)], row: usize) -> Option<u64> {
    window0_field(map, "virt_text")
        .and_then(Value::as_array)
        .and_then(|rows| rows.get(row))
        .and_then(Value::as_array)
        .and_then(|places| places.first())
        .and_then(Value::as_array)
        .and_then(|p| p.get(2))
        .and_then(Value::as_u64)
}

/// The frame, once the pending work has settled.
async fn frame(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<(Value, Value)> {
    let _ = exec_lua(rpc, "return 1").await;
    wait_redraw(inc, |m| window0_field(m, "highlights").is_some()).await
}

#[tokio::test]
async fn a_span_can_carry_virtual_text() {
    let init = r#"btv.decor.expr([[
      local s, e = line:find("TODO", 1, true)
      if not s then return {} end
      return { { s, e, "Todo", virt_text = " <- fix me", virt_hl = "Comment" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_virt", "ab TODO cd\n", init).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(
        virt_on(&map, 0),
        vec![(0, 0, " <- fix me".to_string())],
        "an eol badge draws after the line, and the highlight still lands"
    );
    assert_eq!(spans_on(&map, 0), vec![(3, 7, "Todo".to_string())]);
}

#[tokio::test]
async fn virt_pos_places_the_text() {
    // `inline` draws at the span's own start column, not at the end of the line.
    let init = r#"btv.decor.expr([[
      return { { 4, 7, "Todo", virt_text = "!", virt_pos = "inline" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_virt_pos", "ab TODO cd\n", init).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(virt_on(&map, 0), vec![(1, 3, "!".to_string())]);
}

#[tokio::test]
async fn a_span_can_carry_a_gutter_sign() {
    let init = r#"btv.decor.expr([[
      if not line:find("TODO", 1, true) then return {} end
      return { { sign_text = ">>", sign_hl = "DiagnosticError" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_sign", "nothing\nTODO here\n", init).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(sign_on(&map, 0), None, "the first line has no match");
    assert_eq!(sign_on(&map, 1).as_deref(), Some(">>"));
}

#[tokio::test]
async fn a_span_can_back_the_whole_line() {
    // The group has to exist for the layer to resolve a colour for it — a
    // `line_hl_group` naming nothing is dropped at projection, as it is for any
    // extmark.
    let init = r##"vim.api.nvim_set_hl(0, "PaintBg", { bg = "#303030" })
    btv.decor.expr([[
      if lnum ~= 2 then return {} end
      return { { line_hl = "PaintBg" } }
    ]])"##;
    let (rpc, mut inc) = start("paint_line_hl", "one\ntwo\nthree\n", init).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(
        line_bg_rows(&map),
        vec![1],
        "only the second row gets a line background"
    );
}

#[tokio::test]
async fn a_decoration_needs_no_range_and_no_group() {
    // A sign anchors on the line, not on a stretch of it, so the columns and the
    // highlight group are both optional — and the span still paints.
    let init = r#"btv.decor.expr([[ return { { sign_text = "*" } } ]])"#;
    let (rpc, mut inc) = start("paint_pointmark", "x\n", init).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(sign_on(&map, 0).as_deref(), Some("*"));
    assert!(
        spans_on(&map, 0).is_empty(),
        "and it paints no highlight of its own"
    );
}

#[tokio::test]
async fn a_span_can_do_both_at_once() {
    let init = r##"vim.api.nvim_set_hl(0, "PaintBg", { bg = "#303030" })
    btv.decor.expr([[
      return { { 1, 2, "Todo", virt_text = "!", sign_text = "*", line_hl = "PaintBg" } }
    ]])"##;
    let (rpc, mut inc) = start("paint_all", "abc\n", init).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(spans_on(&map, 0), vec![(0, 2, "Todo".to_string())]);
    assert_eq!(virt_on(&map, 0), vec![(0, 0, "!".to_string())]);
    assert_eq!(sign_on(&map, 0).as_deref(), Some("*"));
    assert_eq!(line_bg_rows(&map), vec![0]);
}

#[tokio::test]
async fn a_decoration_is_repainted_as_the_line_changes() {
    let init = r#"btv.decor.expr([[
      if not line:find("TODO", 1, true) then return {} end
      return { { sign_text = "*" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_decor_edit", "x\n", init).await;
    assert_eq!(sign_on(&frame(&rpc, &mut inc).await, 0), None);
    feed_sync(&rpc, "ITODO <Esc>").await;
    assert_eq!(
        sign_on(&frame(&rpc, &mut inc).await, 0).as_deref(),
        Some("*")
    );
}

#[tokio::test]
async fn a_decoration_stays_out_of_the_extmark_mirror() {
    let init = r#"btv.decor.expr([[ return { { sign_text = "*", virt_text = "!" } } ]])"#;
    let (rpc, mut inc) = start("paint_decor_mirror", "x\n", init).await;
    assert_eq!(
        sign_on(&frame(&rpc, &mut inc).await, 0).as_deref(),
        Some("*")
    );
    let marks = exec_lua(
        &rpc,
        "return #btv.buf.extmarks(0, -1, 0, -1, { details = true })",
    )
    .await;
    assert_eq!(marks.as_u64(), Some(0));
}

#[tokio::test]
async fn a_span_that_draws_nothing_is_refused() {
    let init = r#"btv.decor.expr([[ return { { 1, 2 } } ]])"#;
    let (rpc, mut inc) = start("paint_draws_nothing", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("draws nothing") && msg.contains("paint disabled"),
        "a span with neither a group nor a decoration is a bug, got {msg:?}"
    );
}

#[tokio::test]
async fn an_unknown_span_key_is_refused() {
    // `virt_texts` for `virt_text` would otherwise be a decoration that silently
    // never appears.
    let init = r#"btv.decor.expr([[ return { { 1, 2, "Todo", virt_texts = "!" } } ]])"#;
    let (rpc, mut inc) = start("paint_unknown_key", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("unknown key `virt_texts`"),
        "a misspelled key should name itself, got {msg:?}"
    );
}

#[tokio::test]
async fn an_unknown_virt_pos_is_refused() {
    let init = r#"btv.decor.expr([[ return { { 1, 2, virt_text = "!", virt_pos = "middle" } } ]])"#;
    let (rpc, mut inc) = start("paint_bad_pos", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(msg.contains("unknown `virt_pos` `middle`"), "got {msg:?}");
}

#[tokio::test]
async fn a_qualifier_without_its_decoration_is_refused() {
    let init = r#"btv.decor.expr([[ return { { 1, 2, "Todo", virt_hl = "Comment" } } ]])"#;
    let (rpc, mut inc) = start("paint_dangling_hl", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("no `virt_text`"),
        "a group with nothing to colour is a half-written span, got {msg:?}"
    );
}

#[tokio::test]
async fn a_span_can_carry_virtual_lines() {
    let init = r#"btv.decor.expr([[
      if lnum ~= 1 then return {} end
      return { { virt_lines = "-- section --", virt_lines_hl = "Title" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_virt_lines", "one\ntwo\n", init).await;
    assert_eq!(
        virt_line_rows(&frame(&rpc, &mut inc).await),
        vec![(1, "-- section --".to_string())],
        "a virtual line takes the screen row under the line it is anchored on"
    );
}

#[tokio::test]
async fn virtual_lines_can_draw_above_their_line() {
    let init = r#"btv.decor.expr([[
      if lnum ~= 1 then return {} end
      return { { virt_lines = "header", virt_lines_above = true } }
    ]])"#;
    let (rpc, mut inc) = start("paint_virt_lines_above", "one\ntwo\n", init).await;
    assert_eq!(
        virt_line_rows(&frame(&rpc, &mut inc).await),
        vec![(0, "header".to_string())],
        "`virt_lines_above` puts the row before the line, not after it"
    );
}

#[tokio::test]
async fn a_list_of_virtual_lines_stacks_top_to_bottom() {
    let init = r#"btv.decor.expr([[
      if lnum ~= 1 then return {} end
      return { { virt_lines = { "first", "second" } } }
    ]])"#;
    let (rpc, mut inc) = start("paint_virt_lines_many", "one\ntwo\n", init).await;
    assert_eq!(
        virt_line_rows(&frame(&rpc, &mut inc).await),
        vec![(1, "first".to_string()), (2, "second".to_string())]
    );
}

#[tokio::test]
async fn a_span_can_fill_the_rest_of_the_row() {
    let init = r#"btv.decor.expr([[
      if lnum ~= 1 then return {} end
      return { { line_fill = "-", line_fill_hl = "NonText" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_line_fill", "ab\ncd\n", init).await;
    let places = virt_on(&frame(&rpc, &mut inc).await, 0);
    let (pos, col, text) = places.first().expect("the fill draws a placement");
    assert_eq!(
        (*pos, *col),
        (2, 2),
        "an overlay starting past the line's text"
    );
    assert!(
        text.len() > 1 && text.chars().all(|c| c == '-'),
        "the glyph repeats out to the right edge, got {text:?}"
    );
    assert!(
        virt_on(&frame(&rpc, &mut inc).await, 1).is_empty(),
        "the second line declined"
    );
}

#[tokio::test]
async fn a_span_can_say_how_its_virtual_text_merges() {
    let init = r#"btv.decor.expr([[
      return { { 1, 2, "Todo", virt_text = "!", virt_pos = "overlay", hl_mode = "combine" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_hl_mode", "abc\n", init).await;
    assert_eq!(
        virt_hl_mode(&frame(&rpc, &mut inc).await, 0),
        Some(1),
        "`combine` reaches the placement rather than the `replace` default"
    );
}

#[tokio::test]
async fn a_span_paints_at_the_priority_it_asks_for() {
    // Two spans over the same cells: the *later* one wins a priority tie, so the
    // earlier one showing through at all means its `priority` was honored.
    let init = r#"btv.decor.expr([[
      return { { 1, 3, "Search", priority = 5000 }, { 1, 3, "Todo" } }
    ]])"#;
    let (rpc, mut inc) = start("paint_priority", "abc\n", init).await;
    assert_eq!(
        spans_on(&frame(&rpc, &mut inc).await, 0),
        vec![(0, 3, "Search".to_string())],
        "the higher-priority span paints over the one written after it"
    );
}

// ===== the marks are internal ================================================

#[tokio::test]
async fn the_paint_stays_out_of_the_extmark_mirror() {
    let (rpc, mut inc) = start("paint_mirror", "TODO\n", TODO_PAINT).await;
    assert_eq!(painted(&rpc, &mut inc, 0, "Todo").await, vec![(0, 4)]);
    let marks = exec_lua(
        &rpc,
        "return #btv.buf.extmarks(0, -1, 0, -1, { details = true })",
    )
    .await;
    assert_eq!(
        marks.as_u64(),
        Some(0),
        "the paint namespace is editor state, not a user-visible extmark"
    );
}

// ===== failure, loudly =======================================================

/// The latest message any frame carries after `keys`.
async fn message_after(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>, keys: &str) -> String {
    feed(rpc, keys);
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(inc, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

#[tokio::test]
async fn a_compile_error_is_reported_when_the_block_is_configured() {
    let init = r#"btv.decor.expr([[ return { { 1, ]])"#;
    let (rpc, mut inc) = start("paint_compile", "x\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("btv.decor.expr") && msg.contains("invalid expression"),
        "a compile error should name itself, got {msg:?}"
    );
}

#[tokio::test]
async fn a_failing_block_reports_and_uninstalls_itself() {
    let init = r#"btv.decor.expr([[ error("boom") ]])"#;
    let (rpc, mut inc) = start("paint_runtime", "x\ny\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("paint disabled") && msg.contains("boom"),
        "a failing block should report once and say it is off, got {msg:?}"
    );
    // Uninstalled — which is what makes "once" true: nothing is left to fail on
    // the next frame, and nothing is painted.
    assert!(painted(&rpc, &mut inc, 0, "Todo").await.is_empty());
}

#[tokio::test]
async fn a_malformed_span_is_refused() {
    let init = r#"btv.decor.expr([[ return { { 1 } } ]])"#;
    let (rpc, mut inc) = start("paint_malformed", "x\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("paint disabled"),
        "a span that is not {{ first, last, group }} must be refused, got {msg:?}"
    );
}

#[tokio::test]
async fn a_non_list_return_is_refused() {
    let init = r#"btv.decor.expr([[ return "everything" ]])"#;
    let (rpc, mut inc) = start("paint_badreturn", "x\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("paint disabled"),
        "a string is not a list of spans, got {msg:?}"
    );
}

#[tokio::test]
async fn a_runaway_block_is_abandoned_at_its_deadline() {
    let init = r#"btv.decor.expr([[ while true do end ]])"#;
    let started = std::time::Instant::now();
    let (rpc, mut inc) = start("paint_deadline", "x\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("budget"),
        "the deadline should be reported, got {msg:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "a spinning paint must be abandoned promptly, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_block_cannot_reach_the_editor() {
    let init = r#"btv.decor.expr([[ return btv.o.number and {} or {} ]])"#;
    let (rpc, mut inc) = start("paint_pure", "x\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("paint disabled"),
        "reaching for btv should fail the call, got {msg:?}"
    );
}

#[tokio::test]
async fn nil_clears_the_paint() {
    let (rpc, mut inc) = start("paint_clear", "TODO\n", TODO_PAINT).await;
    assert_eq!(painted(&rpc, &mut inc, 0, "Todo").await, vec![(0, 4)]);
    exec_lua(&rpc, "btv.decor.expr(nil)").await;
    assert!(
        painted(&rpc, &mut inc, 0, "Todo").await.is_empty(),
        "clearing the block takes its paint with it"
    );
}

#[tokio::test]
async fn a_function_is_refused_because_a_closure_cannot_cross_vms() {
    let (rpc, _inc) = start("paint_function", "x\n", "").await;
    let err = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.decor.expr, function() end) return tostring(err)",
    )
    .await;
    let err = err.as_str().unwrap_or_default();
    assert!(
        err.contains("expected a string of Lua source"),
        "a function must be refused loudly, got {err:?}"
    );
}

#[tokio::test]
async fn a_qualifier_is_checked_against_its_own_decoration() {
    // The span *does* carry a decoration, so a check that only asked "is there
    // one?" would let this `sign_hl` through, colouring nothing.
    let init = r#"btv.decor.expr([[ return { { virt_text = "!", sign_hl = "Error" } } ]])"#;
    let (rpc, mut inc) = start("paint_wrong_target", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("`sign_hl`") && msg.contains("no `sign_text`"),
        "a qualifier must name the decoration it wanted, got {msg:?}"
    );
}

#[tokio::test]
async fn an_empty_virt_lines_list_draws_nothing() {
    let init = r#"btv.decor.expr([[ return { { virt_lines = {} } } ]])"#;
    let (rpc, mut inc) = start("paint_empty_lines", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("draws nothing"),
        "no rows is not a decoration, got {msg:?}"
    );
}

#[tokio::test]
async fn a_virtual_line_that_is_not_text_is_refused() {
    let init = r#"btv.decor.expr([[ return { { virt_lines = { {} } } } ]])"#;
    let (rpc, mut inc) = start("paint_bad_line_row", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("`virt_lines` row 1"),
        "a malformed row should name its position, got {msg:?}"
    );
}

#[tokio::test]
async fn an_unknown_hl_mode_is_refused() {
    let init = r#"btv.decor.expr([[ return { { 1, 2, virt_text = "!", hl_mode = "merge" } } ]])"#;
    let (rpc, mut inc) = start("paint_bad_mode", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(msg.contains("unknown `hl_mode` `merge`"), "got {msg:?}");
}

#[tokio::test]
async fn a_priority_that_is_not_a_number_is_refused() {
    let init = r#"btv.decor.expr([[ return { { 1, 2, "Todo", priority = "high" } } ]])"#;
    let (rpc, mut inc) = start("paint_bad_priority", "abc\n", init).await;
    let msg = message_after(&rpc, &mut inc, "j").await;
    assert!(
        msg.contains("`priority` that is not an integer"),
        "got {msg:?}"
    );
}

// ===== the memo is load-bearing ==============================================

/// A steady screen must make **no** calls: the paint is memoized per window on
/// `(buffer, top, bot, changedtick)`, so moving the cursor inside the viewport
/// re-evaluates nothing.
///
/// Measured against the same keystrokes with no block installed, rather than
/// against an absolute — what is being isolated is the paint's contribution, and
/// a deliberately slow block makes a lost memo obvious: 24 visible rows
/// re-evaluated on every keystroke instead of none. The per-call cost is kept to a
/// few milliseconds — well inside the 50ms budget — so a loaded machine cannot
/// trip the deadline and pass this vacuously by *uninstalling* the paint.
#[tokio::test]
async fn a_steady_screen_does_not_re_evaluate_the_paint() {
    let slow = r#"btv.decor.expr([[
      local acc = 0
      for i = 1, 100000 do acc = acc + i % 7 end
      if acc < 0 then return { { 1, 1, "Search" } } end
      return {}
    ]])"#;
    let content: String = (0..40).map(|i| format!("line {i}\n")).collect();

    // Baseline: the same keystrokes with nothing installed.
    let (rpc, _inc) = start("paint_perf_base", &content, "").await;
    let started = std::time::Instant::now();
    for _ in 0..30 {
        feed_sync(&rpc, "0$").await;
    }
    let baseline = started.elapsed();

    let (rpc, _inc) = start("paint_perf", &content, slow).await;
    // One pass is paid up front (and proves the block really is slow).
    let first = std::time::Instant::now();
    feed_sync(&rpc, "j").await;
    let one_pass = first.elapsed();
    let started = std::time::Instant::now();
    for _ in 0..30 {
        feed_sync(&rpc, "0$").await;
    }
    let steady = started.elapsed();

    assert!(
        steady < baseline + one_pass * 5,
        "in-viewport motion must not re-run the paint: {steady:?} against a \
         {baseline:?} baseline, one pass costing {one_pass:?}"
    );
}
