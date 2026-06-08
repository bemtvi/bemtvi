//! LSP Phase 2: diagnostics — screen-column projection, the under-cursor
//! message line, the diagnostics panel, and the painted underline.

use crate::support::*;
use ratatui::style::{Color, Modifier};

#[tokio::test]
async fn diagnostics_are_projected_with_screen_columns() {
    // The headline conversion guard: a leading tab (expands to the default
    // tabstop, 4 cells) then a 2-byte `é` (1 cell), with a diagnostic over "diag"
    // — utf-8 bytes 3..7. It must surface on *screen* columns 5..9, proving both
    // byte->screen (`virtcol` over the tab- and wide-aware line) and the LSP
    // char->byte step.
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-cols",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 3, 7, 1, "bad diag")],
        }),
    );
    let file = temp_file("diag-cols", "rs", "\tédiag\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    let params = wait_for_diagnostics(&rpc, &mut incoming).await;
    let rows = diagnostics_of(&params);
    assert_eq!(
        rows[0],
        vec![(5, 9, 1)],
        "the diagnostic spans screen columns 5..9 at severity 1 (error)"
    );
}

#[tokio::test]
async fn the_diagnostic_under_the_cursor_shows_on_the_message_line() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-msg",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "use of bad")],
        }),
    );
    let file = temp_file("diag-msg", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // The cursor opens at column 0 ('l'), off the diagnostic (chars 4..7) — `w`
    // moves it onto "bad", and the message line picks up its text.
    feed(&rpc, "w");
    let params = wait_for_message(&rpc, &mut incoming, "use of bad").await;
    assert_eq!(message_of(&params), "use of bad");

    // Moving off the diagnostic clears the message again (it never went to
    // `:messages`, so nothing lingers).
    feed(&rpc, "$");
    let params = wait_for_message(&rpc, &mut incoming, "").await;
    assert_eq!(message_of(&params), "");
}

#[tokio::test]
async fn lsp_diagnostics_panel_lists_and_jumps() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-panel",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(1, 4, 5, 1, "x is bad")],
        }),
    );
    let file = temp_file("diag-panel", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    rpc.request("nvim_command", vec![Value::from("LspDiagnostics")])
        .await
        .expect("LspDiagnostics");
    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP diagnostics");
    assert_eq!(lines.len(), 1, "one diagnostic, one row: {lines:?}");
    assert!(
        lines[0].contains("2:5"),
        "the row names the 1-based line:col, got {:?}",
        lines[0]
    );
    assert!(
        lines[0].contains("x is bad"),
        "and the message: {:?}",
        lines[0]
    );

    // `<CR>` on the entry closes the panel and jumps to the diagnostic — line 2
    // (1-based), byte column 4.
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    let cursor = rpc
        .request("nvim_win_get_cursor", vec![])
        .await
        .expect("cursor");
    assert_eq!(
        cursor,
        Value::Array(vec![Value::from(2u64), Value::from(4u64)]),
        "the cursor jumped to the diagnostic's line and column"
    );
}

#[tokio::test]
async fn a_diagnostic_cell_is_painted_with_an_underline() {
    // Tier 2: the real client paint. A diagnostic cell carries the UNDERLINED
    // modifier and the error severity's `sp` underline color, while an adjacent
    // non-diagnostic cell carries neither — proving the span boundaries survive
    // all the way to the rendered grid.
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-paint",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "bad")],
        }),
    );
    let file = temp_file("diag-paint", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    let params = wait_for_diagnostics(&rpc, &mut incoming).await;
    let buf = paint(&View::from_redraw(&params), COLS, ROWS);

    // "bad" sits at byte/screen columns 4..7 on line 0; the painted cells are
    // offset by the number-column gutter.
    let on = GUTTER + 4; // first cell of "bad"
    let off = GUTTER + 7; // the space just after "bad"
    assert_eq!(buf.cell((on, 0)).unwrap().symbol(), "b");
    assert!(
        buf.cell((on, 0))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED),
        "the diagnostic cell is underlined"
    );
    assert_eq!(
        buf.cell((on, 0)).unwrap().style().underline_color,
        Some(Color::Red),
        "with the error severity's built-in underline color"
    );
    assert!(
        !buf.cell((off, 0))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED),
        "the cell just past the diagnostic is not underlined"
    );
}
