//! LSP Phase 7b Slice 2: `vim.diagnostic.*`.
//!
//! `get` reads the Rust→Lua mirror through `nvim_exec_lua`; the actions
//! (`goto_next`/`goto_prev`/`setloclist`/`config`) enqueue an LspOp the server
//! applies, reusing the native cursor-move / panel / underline paths.

use crate::support::*;

#[tokio::test]
async fn vim_diagnostic_get_returns_the_mirror_with_a_severity_filter() {
    let _guard = test_lock().lock().await;
    // Two diagnostics of different severities are published; `get(0)` reads them
    // back from the mirror with neovim's field shape (0-based lnum/col, severity
    // 1=ERROR…4=HINT), and `opts.severity` filters to one.
    let record = configure_mock(
        "diag-get",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(0, 4, 7, 1, "error one"),
                diag(2, 0, 3, 2, "warn two"),
            ],
        }),
    );
    let file = temp_file("diag-get", "rs", "let bad = 1\nfn ok() {}\nzzz = 2\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // Poll the mirror until both publishes have landed.
    let all = loop {
        let v = exec_lua(&rpc, "return vim.diagnostic.get(0)").await;
        if v.as_array().map(|a| a.len()).unwrap_or(0) == 2 {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let arr = all.as_array().unwrap();

    // The first entry carries the error, indexed the neovim way.
    let e = &arr[0];
    assert_eq!(map_get(e, "lnum").and_then(Value::as_i64), Some(0));
    assert_eq!(map_get(e, "col").and_then(Value::as_i64), Some(4));
    assert_eq!(map_get(e, "end_col").and_then(Value::as_i64), Some(7));
    assert_eq!(map_get(e, "severity").and_then(Value::as_i64), Some(1));
    assert_eq!(
        map_get(e, "message").and_then(Value::as_str),
        Some("error one")
    );

    // The severity filter keeps only the matching diagnostic.
    let errors = exec_lua(&rpc, "return vim.diagnostic.get(0, { severity = 1 })").await;
    assert_eq!(
        errors.as_array().map(|a| a.len()),
        Some(1),
        "severity=1 keeps only the error: {errors:?}"
    );
    let warns = exec_lua(&rpc, "return vim.diagnostic.get(0, { severity = 2 })").await;
    assert_eq!(
        warns
            .as_array()
            .and_then(|a| a.first())
            .and_then(|d| map_get(d, "message"))
            .and_then(Value::as_str),
        Some("warn two"),
        "severity=2 keeps only the warning: {warns:?}"
    );
}

#[tokio::test]
async fn vim_diagnostic_goto_moves_across_diagnostics_and_wraps() {
    let _guard = test_lock().lock().await;
    // Diagnostics at (line 0, col 4) and (line 2, col 0). goto_next walks forward
    // and wraps past the last back to the first; goto_prev wraps before the first
    // to the last.
    let record = configure_mock(
        "diag-goto",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(0, 4, 7, 1, "first"),
                diag(2, 0, 3, 1, "second"),
            ],
        }),
    );
    let file = temp_file("diag-goto", "rs", "let bad = 1\nfn ok() {}\nzzz = 2\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // From (0,0): forward to the first diagnostic, then the second.
    exec_lua(&rpc, "vim.diagnostic.goto_next()").await;
    wait_for_cursor(&rpc, (1, 4)).await;
    exec_lua(&rpc, "vim.diagnostic.goto_next()").await;
    wait_for_cursor(&rpc, (3, 0)).await;
    // Past the last: wrap to the first.
    exec_lua(&rpc, "vim.diagnostic.goto_next()").await;
    wait_for_cursor(&rpc, (1, 4)).await;
    // Before the first: wrap to the last.
    exec_lua(&rpc, "vim.diagnostic.goto_prev()").await;
    wait_for_cursor(&rpc, (3, 0)).await;
}

#[tokio::test]
async fn vim_diagnostic_setloclist_opens_the_navigable_panel() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-loclist",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(1, 4, 5, 1, "x is bad")],
        }),
    );
    let file = temp_file("diag-loclist", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    exec_lua(&rpc, "vim.diagnostic.setloclist()").await;
    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP diagnostics");
    assert_eq!(lines.len(), 1, "one diagnostic, one row: {lines:?}");
    assert!(
        lines[0].contains("x is bad"),
        "row carries the message: {lines:?}"
    );

    // The panel is navigable: `<CR>` jumps to line 2 (1-based), byte col 4.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 4)).await;
}

#[tokio::test]
async fn vim_diagnostic_config_underline_false_hides_the_squiggles() {
    let _guard = test_lock().lock().await;
    // The one config key with a backing surface: `underline = false` removes the
    // diagnostic underline spans from the redraw (the message line/panel stay).
    let record = configure_mock(
        "diag-config",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "bad")],
        }),
    );
    let file = temp_file("diag-config", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // Disable the underline; the spans drain out of the redraw.
    exec_lua(&rpc, "vim.diagnostic.config({ underline = false })").await;
    let mut hidden = false;
    for _ in 0..80 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if diagnostics_of(&params).iter().all(|row| row.is_empty()) {
                hidden = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(hidden, "underline=false should clear the diagnostic spans");

    // Re-enabling brings them back.
    exec_lua(&rpc, "vim.diagnostic.config({ underline = true })").await;
    wait_for_diagnostics(&rpc, &mut incoming).await;
}
