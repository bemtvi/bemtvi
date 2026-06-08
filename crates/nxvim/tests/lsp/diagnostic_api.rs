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

#[tokio::test]
async fn vim_diagnostic_config_virtual_text_paints_the_message_inline() {
    let _guard = test_lock().lock().await;
    // `virtual_text` is off by default (neovim 0.10): the `diagnostics_virt`
    // redraw key carries no decoration. Enabling it surfaces the diagnostic's
    // message on its own row, prefixed, at the diagnostic's severity.
    let record = configure_mock(
        "diag-virt",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "use of bad")],
        }),
    );
    let file = temp_file("diag-virt", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    let params = wait_for_diagnostics(&rpc, &mut incoming).await;
    assert!(
        diagnostics_virt_of(&params).iter().all(Option::is_none),
        "virtual_text is off by default, so no inline decoration"
    );

    exec_lua(&rpc, "vim.diagnostic.config({ virtual_text = true })").await;
    let (text, severity) = wait_for_virt_text(&rpc, &mut incoming).await;
    assert!(
        text.contains("use of bad"),
        "the inline text carries the message: {text:?}"
    );
    assert!(
        text.starts_with("■"),
        "and the default prefix glyph: {text:?}"
    );
    assert_eq!(severity, 1, "at the diagnostic's severity (error)");
}

#[tokio::test]
async fn virtual_text_picks_the_highest_severity_on_a_row_and_honors_a_prefix() {
    let _guard = test_lock().lock().await;
    // Two diagnostics share line 0 — a warning and an error. The inline text shows
    // the *error* (the most severe), and a custom `prefix` from the table form of
    // `virtual_text` leads it.
    let record = configure_mock(
        "diag-virt-sev",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(0, 0, 3, 2, "just a warning"),
                diag(0, 4, 7, 1, "real error"),
            ],
        }),
    );
    let file = temp_file("diag-virt-sev", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    exec_lua(
        &rpc,
        "vim.diagnostic.config({ virtual_text = { prefix = '>> ' } })",
    )
    .await;
    let (text, severity) = wait_for_virt_text(&rpc, &mut incoming).await;
    assert_eq!(severity, 1, "the error wins over the warning on the row");
    assert!(
        text.contains("real error") && !text.contains("just a warning"),
        "showing the most severe message: {text:?}"
    );
    assert!(text.starts_with(">> "), "with the custom prefix: {text:?}");
}

#[tokio::test]
async fn virtual_text_strips_terminal_control_characters_from_the_message() {
    let _guard = test_lock().lock().await;
    // The message text is server-controlled (untrusted). A hostile server that
    // embeds an ANSI escape sequence must not get it painted to the terminal: the
    // control bytes are stripped at the projection, leaving only the visible text.
    let record = configure_mock(
        "diag-virt-esc",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "safe\u{1b}[31mINJECT\u{7}ed")],
        }),
    );
    let file = temp_file("diag-virt-esc", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    exec_lua(&rpc, "vim.diagnostic.config({ virtual_text = true })").await;
    let (text, _severity) = wait_for_virt_text(&rpc, &mut incoming).await;
    assert!(
        !text.chars().any(|c| c.is_control()),
        "no control characters survive into the virt-text: {text:?}"
    );
    assert!(
        text.contains("safe[31mINJECTed"),
        "the visible characters are kept, only the control bytes dropped: {text:?}"
    );
}

#[tokio::test]
async fn signs_are_on_by_default_and_reserve_a_column() {
    let _guard = test_lock().lock().await;
    // `signs` defaults on (neovim 0.10): a published diagnostic puts a severity
    // glyph on its own row and reserves the sign column; clean rows stay blank.
    let record = configure_mock(
        "diag-signs",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(1, 4, 5, 1, "x is bad")],
        }),
    );
    let file = temp_file("diag-signs", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    let (glyph, severity) = wait_for_signs(&rpc, &mut incoming).await;
    assert_eq!(glyph, "E", "the built-in error glyph");
    assert_eq!(severity, 1, "at the diagnostic's severity (error)");

    // The full-frame view: the sign sits on line 2 (row index 1), line 1 is blank,
    // and the column is reserved.
    let params = loop {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(p) = drain_latest_redraw(&mut incoming) {
            if diagnostics_signs_of(&p).iter().any(Option::is_some) {
                break p;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(sign_column_of(&params), "the sign column is reserved");
    let signs = diagnostics_signs_of(&params);
    assert_eq!(signs[0], None, "line 1 (no diagnostic) has no sign");
    assert_eq!(
        signs[1],
        Some(("E".to_string(), 1)),
        "line 2 carries the error sign"
    );
}

#[tokio::test]
async fn signs_pick_the_highest_severity_on_a_line() {
    let _guard = test_lock().lock().await;
    // A warning and an error share line 0; the gutter shows the error glyph.
    let record = configure_mock(
        "diag-signs-sev",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(0, 0, 3, 2, "just a warning"),
                diag(0, 4, 7, 1, "real error"),
            ],
        }),
    );
    let file = temp_file("diag-signs-sev", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    let (glyph, severity) = wait_for_signs(&rpc, &mut incoming).await;
    assert_eq!(severity, 1, "the error wins over the warning on the line");
    assert_eq!(glyph, "E", "showing the error glyph");
}

#[tokio::test]
async fn signs_false_reserves_no_column() {
    let _guard = test_lock().lock().await;
    // `signs = false` drains the signs and un-reserves the column, restoring the
    // pre-signs gutter layout.
    let record = configure_mock(
        "diag-signs-off",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "bad")],
        }),
    );
    let file = temp_file("diag-signs-off", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_signs(&rpc, &mut incoming).await;

    exec_lua(&rpc, "vim.diagnostic.config({ signs = false })").await;
    let mut cleared = false;
    for _ in 0..80 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if !sign_column_of(&params) && diagnostics_signs_of(&params).iter().all(Option::is_none)
            {
                cleared = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        cleared,
        "signs=false should un-reserve the column and clear the glyphs"
    );

    // Re-enabling brings them back.
    exec_lua(&rpc, "vim.diagnostic.config({ signs = true })").await;
    wait_for_signs(&rpc, &mut incoming).await;
}

#[tokio::test]
async fn signs_honor_a_custom_text_glyph() {
    let _guard = test_lock().lock().await;
    // The `signs.text` map overrides a severity's glyph (keyed by the severity
    // number, as in neovim).
    let record = configure_mock(
        "diag-signs-text",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "bad")],
        }),
    );
    let file = temp_file("diag-signs-text", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_signs(&rpc, &mut incoming).await;

    exec_lua(
        &rpc,
        "vim.diagnostic.config({ signs = { text = { [vim.diagnostic.severity.ERROR] = '✘' } } })",
    )
    .await;
    let mut seen = None;
    for _ in 0..80 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if let Some(hit) = diagnostics_signs_of(&params).into_iter().flatten().next() {
                if hit.0 == "✘" {
                    seen = Some(hit);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (glyph, severity) = seen.expect("the custom error glyph should appear");
    assert_eq!(glyph, "✘", "the configured glyph replaces the default E");
    assert_eq!(severity, 1);
}

#[tokio::test]
async fn vim_diagnostic_open_float_shows_the_cursor_lines_diagnostics() {
    let _guard = test_lock().lock().await;
    // A diagnostic with a multi-line message plus `source`/`code` on line 2; with
    // the cursor on that line, `open_float()` pops a float (the panel) carrying the
    // full message — both lines — the inline virtual text would truncate.
    let record = configure_mock(
        "diag-float",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [{
                "range": {
                    "start": { "line": 1, "character": 4 },
                    "end": { "line": 1, "character": 5 },
                },
                "severity": 1,
                "source": "rustc",
                "code": "E0308",
                "message": "mismatched types\nexpected `u8`, found `i32`",
            }],
        }),
    );
    let file = temp_file("diag-float", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // Move onto the diagnostic line, then pop the float.
    feed(&rpc, "j");
    wait_for_cursor(&rpc, (2, 0)).await;
    exec_lua(&rpc, "vim.diagnostic.open_float()").await;

    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "Diagnostics");
    // The header carries the severity letter, source, code, and first message
    // line; the second message line rides its own row (the full, untruncated text).
    assert!(
        lines.iter().any(|l| l.contains("rustc:")
            && l.contains("mismatched types")
            && l.contains("[E0308]")),
        "header row carries source/message/code: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("expected `u8`, found `i32`")),
        "the second message line is shown in full: {lines:?}"
    );
}

#[tokio::test]
async fn vim_diagnostic_open_float_on_a_clean_line_opens_nothing() {
    let _guard = test_lock().lock().await;
    // The diagnostic is on line 2; with the cursor resting on the clean line 1,
    // `open_float()` opens no panel — it echoes a loud nothing instead.
    let record = configure_mock(
        "diag-float-clean",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(1, 4, 5, 1, "x is bad")],
        }),
    );
    let file = temp_file("diag-float-clean", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // Cursor stays on line 1 (clean); open_float echoes and opens no panel.
    exec_lua(&rpc, "vim.diagnostic.open_float()").await;
    let mut echoed = false;
    for _ in 0..40 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            assert!(
                panel_of(&params).is_none(),
                "a clean line opens no float: {:?}",
                panel_of(&params)
            );
            if message_of(&params).contains("No diagnostics under cursor") {
                echoed = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        echoed,
        "the clean-line no-op is a loud echo, not a silent skip"
    );
}

#[tokio::test]
async fn vim_diagnostic_open_float_lists_all_diagnostics_severity_sorted() {
    let _guard = test_lock().lock().await;
    // Three diagnostics on the cursor's line, published out of severity order; the
    // float lists them all, ordered error → warn → info (severity then column).
    let record = configure_mock(
        "diag-float-multi",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(1, 8, 9, 3, "info note"),
                diag(1, 0, 1, 1, "the error"),
                diag(1, 4, 5, 2, "a warning"),
            ],
        }),
    );
    let file = temp_file("diag-float-multi", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    feed(&rpc, "j");
    wait_for_cursor(&rpc, (2, 0)).await;
    exec_lua(&rpc, "vim.diagnostic.open_float()").await;

    let (_title, lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(lines.len(), 3, "one row per diagnostic: {lines:?}");
    // Each is single-line, so the order of the rows is the severity sort.
    assert!(lines[0].contains("the error"), "error first: {lines:?}");
    assert!(lines[1].contains("a warning"), "warn second: {lines:?}");
    assert!(lines[2].contains("info note"), "info third: {lines:?}");
}
