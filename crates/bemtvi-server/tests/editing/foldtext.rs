//! `btv.fold.text` — the text a **closed** fold shows on its collapsed row
//! (vim's `'foldtext'`), computed by a sandbox expression.
//!
//! Black-box: source an `init.lua`, fold a range with `zf`, and read the
//! rendered row off the redraw `lines` array.

use crate::support::*;

/// Six numbered lines under `init_lua`, cursor at the top.
async fn six_lines_with(tag: &str, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir(tag);
    let (rpc, incoming) = start_with_config(&dir, init_lua).await;
    feed(&rpc, "iL1<CR>L2<CR>L3<CR>L4<CR>L5<CR>L6<Esc>gg");
    assert_eq!(lines(&rpc).await, vec!["L1", "L2", "L3", "L4", "L5", "L6"]);
    (rpc, incoming)
}

/// The first rendered row after folding lines 2..4 closed.
async fn folded_row(tag: &str, init_lua: &str) -> String {
    let (rpc, mut incoming) = six_lines_with(tag, init_lua).await;
    let map = redraw_after(&rpc, &mut incoming, "2Gzf2j").await;
    view_lines(&map)
        .into_iter()
        .find(|l| !l.is_empty() && l != "L1")
        .unwrap_or_default()
}

#[tokio::test]
async fn without_an_expression_the_builtin_default_renders() {
    let row = folded_row("ft_default", "").await;
    assert!(
        row.contains("3 lines: L2"),
        "expected vim's default shape, got {row:?}"
    );
}

#[tokio::test]
async fn a_custom_expression_renders_on_the_collapsed_row() {
    let row = folded_row(
        "ft_custom",
        r#"btv.fold.text([[ "<< " .. first .. " >>" ]])"#,
    )
    .await;
    assert_eq!(row.trim_end(), "<< L2 >>");
}

#[tokio::test]
async fn lines_and_lnum_describe_the_fold() {
    // Lines 2..4 folded: three lines, starting on 1-based line 2.
    let row = folded_row("ft_args", r#"btv.fold.text([[ lnum .. ":" .. lines ]])"#).await;
    assert_eq!(row.trim_end(), "2:3");
}

#[tokio::test]
async fn a_number_result_is_accepted() {
    let row = folded_row("ft_number", "btv.fold.text([[ lines * 10 ]])").await;
    assert_eq!(row.trim_end(), "30");
}

#[tokio::test]
async fn clearing_restores_the_builtin_default() {
    let row = folded_row(
        "ft_clear",
        "btv.fold.text([[ \"custom\" ]])\nbtv.fold.text(nil)\n",
    )
    .await;
    assert!(
        row.contains("3 lines: L2"),
        "expected the default back, got {row:?}"
    );
}

#[tokio::test]
async fn a_failing_expression_reports_and_falls_back_to_the_default() {
    let (rpc, mut incoming) =
        six_lines_with("ft_raise", r#"btv.fold.text([[ error("boom") ]])"#).await;
    let map = redraw_after(&rpc, &mut incoming, "2Gzf2j").await;
    let msg = message(&map);
    assert!(
        msg.contains("btv.fold.text"),
        "expected a report, got {msg:?}"
    );
    // Degraded to the built-in rather than rendering an empty or wedged row.
    let row = view_lines(&map)
        .into_iter()
        .find(|l| l.contains("lines:"))
        .unwrap_or_default();
    assert!(
        row.contains("3 lines: L2"),
        "expected the default, got {row:?}"
    );
}

#[tokio::test]
async fn a_table_result_is_refused() {
    let (rpc, mut incoming) = six_lines_with("ft_badret", "btv.fold.text([[ {} ]])").await;
    let map = redraw_after(&rpc, &mut incoming, "2Gzf2j").await;
    let msg = message(&map);
    assert!(
        msg.contains("btv.fold.text"),
        "a table result must be refused, got {msg:?}"
    );
}

#[tokio::test]
async fn a_compile_error_is_reported_where_it_is_configured() {
    let dir = temp_dir("ft_badsyntax");
    let (rpc, mut incoming) = start_with_config(&dir, "btv.fold.text([[ first .. ]])").await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "").await);
    assert!(
        msg.contains("btv.fold.text") && msg.contains("invalid expression"),
        "expected a configure-time complaint, got {msg:?}"
    );
}

#[tokio::test]
async fn a_closure_is_rejected_at_the_lua_boundary() {
    let dir = temp_dir("ft_badarg");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(btv.fold.text, function() end) return tostring(e)",
    )
    .await;
    let s = err.as_str().unwrap_or_default();
    assert!(
        s.contains("expected a string of Lua source"),
        "passing a closure must fail loud, got {s:?}"
    );
}
