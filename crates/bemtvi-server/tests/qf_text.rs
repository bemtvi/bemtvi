//! `btv.qf.text` — a custom rendering for the rows of the quickfix window.
//!
//! vim spells it `'quickfixtextfunc'`. It is the first sandbox surface handed a
//! *record* rather than a list of scalars: one entry in, one row out. Black-box
//! throughout — the assertions are on the text the `:copen` buffer actually
//! holds, on the jumps that text has to keep working, and on the colouring that
//! rides it.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    buf_name, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, message,
    message_after, start_attached, write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Three entries, two of them carrying a severity.
const LIST: &str = r#"vim.fn.setqflist({
     { filename = "a.c", lnum = 10, col = 5, text = "boom", type = "E" },
     { filename = "b.c", lnum = 3, text = "later", type = "W" },
     { filename = "c.c", lnum = 7, text = "note" },
   }, " ")"#;

/// The archetypal render: the message first, the location trailing.
const TEXT_EXPR: &str =
    r#"btv.qf.text([[ item.text .. " @ " .. item.filename .. ":" .. item.lnum ]])"#;

/// The latest message any frame carries after `keys` — the failure paths report
/// while a list is being re-rendered, which is not always the frame the input
/// itself produced.
async fn msg_after(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>, keys: &str) -> String {
    feed(rpc, keys);
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(inc, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

/// The highlight groups painted on `row` of **any** window in the frame — the
/// quickfix display is a dock tab, so it is not `windows[0]`.
fn groups_on(map: &[(Value, Value)], row: usize) -> Vec<String> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return Vec::new();
    };
    wins.iter()
        .filter_map(|w| match w {
            Value::Map(m) => map_get(m, "highlights"),
            _ => None,
        })
        .filter_map(Value::as_array)
        .filter_map(|rows| rows.get(row))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|s| s.as_array()?.get(2)?.as_str().map(str::to_string))
        .collect()
}

// ===== rendering =============================================================

#[tokio::test]
async fn a_render_expression_replaces_the_default_rows() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, TEXT_EXPR).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    let rendered = lines(&rpc).await;
    assert_eq!(rendered[0], "boom @ a.c:10");
    assert_eq!(rendered[1], "later @ b.c:3");
    assert_eq!(rendered[2], "note @ c.c:7");
}

#[tokio::test]
async fn installing_it_rerenders_a_list_that_is_already_open() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    assert_eq!(
        lines(&rpc).await[0],
        "a.c|10 col 5| boom",
        "the built-in rendering, before anything is installed"
    );
    exec_lua(&rpc, TEXT_EXPR).await;
    assert_eq!(
        lines(&rpc).await[0],
        "boom @ a.c:10",
        "installing re-renders the open list rather than waiting for it to change"
    );
}

#[tokio::test]
async fn clearing_it_restores_the_default_rendering() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, TEXT_EXPR).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    assert_eq!(lines(&rpc).await[0], "boom @ a.c:10");
    exec_lua(&rpc, "btv.qf.text(nil)").await;
    assert_eq!(lines(&rpc).await[0], "a.c|10 col 5| boom");
}

#[tokio::test]
async fn every_documented_key_is_readable_off_the_item() {
    let (rpc, mut inc) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({{
             filename = "a.c", bufnr = 0, module = "mod", lnum = 10, end_lnum = 12,
             col = 5, end_col = 9, vcol = true, nr = 42, pattern = "pat",
             text = "boom", type = "E", valid = true,
           }}, " ")"#,
    )
    .await;
    exec_lua(
        &rpc,
        r#"btv.qf.text([[ table.concat({
             item.filename, item.bufnr, item.module, item.lnum, item.end_lnum,
             item.col, item.end_col, tostring(item.vcol), item.nr, item.pattern,
             item.text, item.type, tostring(item.valid),
           }, "/") ]])"#,
    )
    .await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    assert_eq!(
        lines(&rpc).await[0],
        "a.c/0/mod/10/12/5/9/true/42/pat/boom/E/true"
    );
}

#[tokio::test]
async fn idx_is_the_entrys_one_based_position() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, r#"btv.qf.text([[ idx .. ". " .. item.text ]])"#).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    let rendered = lines(&rpc).await;
    assert_eq!(rendered[0], "1. boom");
    assert_eq!(rendered[2], "3. note");
}

#[tokio::test]
async fn a_newline_inside_a_rendered_row_is_flattened() {
    // One row per entry is load-bearing: the row index *is* the entry index for
    // `<CR>`, `:cc` and the severity paint, so a two-line render would desync them.
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(
        &rpc,
        r#"btv.qf.text([[ item.text .. "\n" .. item.filename ]])"#,
    )
    .await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    let rendered = lines(&rpc).await;
    assert_eq!(rendered[0], "boom a.c");
    assert_eq!(rendered[1], "later b.c");
}

#[tokio::test]
async fn a_number_is_accepted_as_a_row() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, "btv.qf.text([[ item.lnum ]])").await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    assert_eq!(lines(&rpc).await[0], "10");
}

// ===== what the custom rows must keep working ================================

#[tokio::test]
async fn a_custom_row_is_still_jumpable() {
    let (rpc, mut inc) = start().await;
    let path = write_temp("qf_text_jump", "txt", "one\ntwo\nthree\nfour\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{
                 {{ filename = "{p}", lnum = 2, text = "first" }},
                 {{ filename = "{p}", lnum = 4, text = "second" }},
               }}, " ")"#,
            p = path.replace('\\', "\\\\")
        ),
    )
    .await;
    exec_lua(&rpc, r#"btv.qf.text([[ "-> " .. item.text ]])"#).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    assert_eq!(lines(&rpc).await[1], "-> second");
    // <CR> on the *second* row lands on the second entry, so the row index still
    // means the entry index.
    message_after(&rpc, &mut inc, "j<CR>").await;
    assert_eq!(buf_name(&rpc).await, path);
    assert_eq!(cursor(&rpc).await.0, 4);
}

#[tokio::test]
async fn the_severity_colour_still_lands_on_a_custom_row() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, TEXT_EXPR).await;
    let map = message_after_map(&rpc, &mut inc, ":copen<CR>").await;
    assert!(
        groups_on(&map, 0).iter().any(|g| g == "DiagnosticError"),
        "the `type = \"E\"` row keeps its severity paint under a custom render"
    );
    assert!(groups_on(&map, 1).iter().any(|g| g == "DiagnosticWarn"));
    assert!(
        groups_on(&map, 2).is_empty(),
        "a typeless entry stays uncoloured"
    );
}

/// `:copen` and the frame it produced (the severity paint rides that frame).
async fn message_after_map(
    rpc: &Rpc,
    inc: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    bemtvi_test_harness::redraw_after_matching(rpc, inc, keys, |m| {
        map_get(m, "windows")
            .and_then(Value::as_array)
            .is_some_and(|w| w.len() > 1)
    })
    .await
}

#[tokio::test]
async fn a_location_list_and_a_named_list_use_it_too() {
    let (rpc, mut inc) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setloclist(0, { { filename = "l.c", lnum = 2, text = "loc" } }, " ")"#,
    )
    .await;
    exec_lua(&rpc, r#"btv.qf.text([[ "* " .. item.text ]])"#).await;
    message_after(&rpc, &mut inc, ":lopen<CR>").await;
    assert_eq!(lines(&rpc).await[0], "* loc");
    message_after(&rpc, &mut inc, ":lclose<CR>").await;

    exec_lua(
        &rpc,
        r#"btv.qf.list("refs", { { filename = "n.c", lnum = 1, text = "named" } }, {})"#,
    )
    .await;
    exec_lua(&rpc, r#"btv.qf.show("refs")"#).await;
    assert_eq!(lines(&rpc).await[0], "* named");
}

// ===== failure, loudly =======================================================

#[tokio::test]
async fn a_compile_error_is_reported_where_it_is_configured() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, r#"btv.qf.text([[ item.text .. ]])"#).await;
    let msg = msg_after(&rpc, &mut inc, ":copen<CR>").await;
    assert!(
        msg.contains("btv.qf.text") && msg.contains("invalid expression"),
        "a bad expression should be reported when it is installed, got {msg:?}"
    );
    assert_eq!(
        lines(&rpc).await[0],
        "a.c|10 col 5| boom",
        "and nothing is installed, so the default rendering stands"
    );
}

/// Open the quickfix window on `LIST`, rendered by the default, and return the
/// message the *next* `exec_lua` produced.
///
/// The failures below are triggered with the list already open, which is both the
/// common case (a render is installed while you are looking at the list) and the
/// one whose report reaches the message line: opening the quickfix dock clears
/// the message line as every dock open does, so a failure *during* `:copen` is
/// only recoverable from `:messages` — which the last test here pins.
async fn open_then(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>, code: &str) -> String {
    exec_lua(rpc, LIST).await;
    message_after(rpc, inc, ":copen<CR>").await;
    exec_lua(rpc, code).await;
    for _ in 0..50 {
        if let Some(m) = drain_to_latest_redraw(inc, |m| !message(m).is_empty()) {
            return message(&m);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    String::new()
}

#[tokio::test]
async fn an_erroring_expression_reports_and_restores_the_default() {
    let (rpc, mut inc) = start().await;
    let msg = open_then(&rpc, &mut inc, r#"btv.qf.text([[ error("boom") ]])"#).await;
    assert!(
        msg.contains("default rendering restored") && msg.contains("boom"),
        "a failing render should report and say it is off, got {msg:?}"
    );
    assert_eq!(lines(&rpc).await[0], "a.c|10 col 5| boom");
    // Uninstalled — which is what makes "once" true: a *new* list renders with the
    // default too, rather than failing again.
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "z.c", lnum = 1, text = "fresh" } }, " ")"#,
    )
    .await;
    assert_eq!(lines(&rpc).await[0], "z.c|1| fresh");
}

#[tokio::test]
async fn a_non_string_return_reports_and_restores_the_default() {
    let (rpc, mut inc) = start().await;
    let msg = open_then(&rpc, &mut inc, "btv.qf.text([[ { item.text } ]])").await;
    assert!(
        msg.contains("expected a string or number"),
        "a table row is a bug in the expression, got {msg:?}"
    );
    assert_eq!(lines(&rpc).await[0], "a.c|10 col 5| boom");
}

#[tokio::test]
async fn a_deadline_overrun_reports_and_restores_the_default() {
    let (rpc, mut inc) = start().await;
    let msg = open_then(
        &rpc,
        &mut inc,
        "btv.qf.text([[ (function() while true do end end)() ]])",
    )
    .await;
    assert!(
        msg.contains("budget") && msg.contains("default rendering restored"),
        "a runaway expression should be abandoned and reported, got {msg:?}"
    );
    assert_eq!(lines(&rpc).await[0], "a.c|10 col 5| boom");
}

#[tokio::test]
async fn a_failure_while_the_window_opens_is_still_in_the_message_history() {
    // Opening the quickfix dock clears the message line, so the report of a render
    // that failed *during* `:copen` would be gone — except that every echo is
    // recorded, and `:messages` is where it stays.
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(&rpc, r#"btv.qf.text([[ error("boom") ]])"#).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    assert_eq!(lines(&rpc).await[0], "a.c|10 col 5| boom");
    feed(&rpc, ":messages<CR>");
    let history = lines(&rpc).await;
    assert!(
        history
            .iter()
            .any(|l| l.contains("btv.qf.text") && l.contains("boom")),
        "the failure must survive the dock open in the history: {history:?}"
    );
}

#[tokio::test]
async fn a_failure_restores_every_open_list_not_just_the_one_that_failed() {
    let (rpc, mut inc) = start().await;
    exec_lua(&rpc, LIST).await;
    exec_lua(
        &rpc,
        r#"btv.qf.list("refs", { { filename = "n.c", lnum = 1, text = "named" } }, {})"#,
    )
    .await;
    exec_lua(&rpc, r#"btv.qf.text([[ "* " .. item.text ]])"#).await;
    message_after(&rpc, &mut inc, ":copen<CR>").await;
    exec_lua(&rpc, r#"btv.qf.show("refs")"#).await;
    assert_eq!(lines(&rpc).await[0], "* named");

    // A render that fails only on the *second* entry: the quickfix list has three,
    // the named list one, so the named list renders cleanly and the quickfix list
    // is the one that blows up.
    exec_lua(
        &rpc,
        r#"btv.qf.text([[ idx == 2 and error("boom") or ("* " .. item.text) ]])"#,
    )
    .await;
    assert_eq!(
        lines(&rpc).await[0],
        "n.c|1| named",
        "the list that rendered cleanly goes back to the default too"
    );
}

#[tokio::test]
async fn a_non_string_argument_raises_at_the_call_site() {
    let (rpc, _inc) = start().await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.qf.text, 42) return tostring(e)",
    )
    .await;
    assert!(
        err.as_str().unwrap_or("").contains("expected a string"),
        "got {err:?}"
    );
}
