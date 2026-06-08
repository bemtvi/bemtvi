//! LSP Phase 3: go-to definition & references, hover, signature help.

use crate::support::*;

#[tokio::test]
async fn gd_jumps_to_a_definition_in_the_same_file() {
    let _guard = test_lock().lock().await;
    // `target` is defined on line 0; the call site is on line 1. `gd` from the
    // call site jumps to the definition's (line, col).
    let file = temp_file("gd-same", "rs", "fn target() {}\nfn main() { target() }\n");
    let record = configure_mock(
        "gd-same",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Move to the call site (line 1) and request go-to-definition via the `gd`
    // built-in keymap.
    feed(&rpc, "jgd");
    // The reply lands the cursor at the definition: 1-based line 1, byte col 3.
    wait_for_cursor(&rpc, (1, 3)).await;

    // The keymap actually issued the LSP request (not swallowed as editor motion).
    assert!(
        has_method(&record_lines(&record), "textDocument/definition"),
        "gd should send a textDocument/definition request"
    );
}

#[tokio::test]
async fn gd_switches_buffers_for_a_cross_file_definition() {
    let _guard = test_lock().lock().await;
    // The definition lives in a *different* file, on a line with a 2-byte `é`
    // before the target column — and the server negotiated utf-16. So the jump
    // must (a) open/switch to the file, then (b) read its just-loaded line to
    // convert the utf-16 character into a byte column. The `=` is utf-16 unit 9
    // ("let café " is 9 units, é counting as one) but byte 10 (é is two bytes):
    // landing on byte col 10 proves the cross-file char→byte conversion.
    let other = temp_file("gd-other", "rs", "let café = 1\n");
    let main = temp_file("gd-main", "rs", "fn main() {}\n");
    let record = configure_mock(
        "gd-cross",
        serde_json::json!({
            "position_encoding": "utf-16",
            "definition": location(&other, 0, 9),
        }),
    );
    let (rpc, _incoming) = start(Some(main)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gd");
    // Switched to the other file, cursor on the `=`: 1-based line 1, byte col 10.
    wait_for_cursor(&rpc, (1, 10)).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["let café = 1".to_string()],
        "the jump switched to the definition's buffer"
    );
}

#[tokio::test]
async fn gr_lists_references_in_the_panel_and_jumps() {
    let _guard = test_lock().lock().await;
    // Two references to `x`, on lines 1 and 2 (col 8 in each). `gr` lists them in
    // a select panel; `<CR>` on the first jumps to it.
    let file = temp_file("gr", "rs", "let x = 1\nlet y = x\nlet z = x\n");
    let record = configure_mock(
        "gr",
        serde_json::json!({ "references": [location(&file, 1, 8), location(&file, 2, 8)] }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gr");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references");
    assert_eq!(
        panel_lines.len(),
        2,
        "one row per reference: {panel_lines:?}"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/references"),
        "gr should send a textDocument/references request"
    );

    // `<CR>` on the first row jumps to it: 1-based line 2, byte col 8.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 8)).await;
}

#[tokio::test]
async fn panelopen_reopens_the_references_panel_and_still_jumps() {
    let _guard = test_lock().lock().await;
    // Regression: navigating from the references list with `<CR>` closes the
    // panel; reopening it with `:panelopen` must keep its jump targets, so a
    // second `<CR>` still navigates (previously the targets were lost on the
    // first jump and the reopened list was inert).
    let file = temp_file("gr-reopen", "rs", "let x = 1\nlet y = x\nlet z = x\n");
    let record = configure_mock(
        "gr-reopen",
        serde_json::json!({ "references": [location(&file, 1, 8), location(&file, 2, 8)] }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gr");
    let (title, original_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references");

    // First navigation: `<CR>` on row 0 jumps to reference 1 (line 2, col 8) and
    // closes the panel.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 8)).await;

    // Reopen the dismissed list and navigate again — to a *different* row, so the
    // jump is observable: row 1 is reference 2 (line 3, col 8).
    rpc.request("nvim_command", vec![Value::from("panelopen")])
        .await
        .expect("panelopen");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references", "the references panel came back");
    // The reopened list carries the exact content it had before — comparing to the
    // original render rather than a fixed row count keeps this robust to how long
    // temp paths word-wrap to the panel width.
    assert_eq!(
        panel_lines, original_lines,
        "reopened with its content intact"
    );

    feed(&rpc, "j<CR>");
    wait_for_cursor(&rpc, (3, 8)).await;
}

#[tokio::test]
async fn an_empty_definition_reply_reports_no_definition() {
    let _guard = test_lock().lock().await;
    // The server returns nothing for `gd`: a brief message, no jump, no panel.
    let file = temp_file("gd-empty", "rs", "fn main() {}\n");
    let record = configure_mock("gd-empty", serde_json::json!({}));
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gd");
    let params = wait_for_message(&rpc, &mut incoming, "No definition found").await;
    assert_eq!(message_of(&params), "No definition found");
    // The cursor never left the origin.
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn a_definition_reply_is_dropped_if_the_cursor_moved() {
    // Stale-reply drop (Decision 3): fire `gd`, move the cursor before the async
    // reply lands, and the jump must be discarded — the move wins, not the
    // now-irrelevant definition. `gdj` does exactly this in one input batch: the
    // request is issued at (0,0), then `j` moves to (1,0) before the reply (which
    // the select! loop only processes after the batch) is handled.
    let _guard = test_lock().lock().await;
    let file = temp_file(
        "gd-stale",
        "rs",
        "fn target() {}\nfn main() {}\nlet z = 1\n",
    );
    let record = configure_mock(
        "gd-stale",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gdj");
    // Give the (now-stale) reply ample time to arrive and be dropped.
    for _ in 0..8 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "the cursor stayed where `j` moved it; the stale definition reply did not jump"
    );
    // The request was genuinely sent (so we really exercised the drop, not a
    // never-issued request).
    assert!(has_method(
        &record_lines(&record),
        "textDocument/definition"
    ));
}

#[tokio::test]
async fn k_shows_hover_docs_in_the_panel() {
    let _guard = test_lock().lock().await;
    // The mock returns markdown hover contents; `K` opens the panel with the
    // markup rendered as plain lines (the trailing blank line is trimmed).
    let file = temp_file("hover", "rs", "fn target() {}\n");
    let record = configure_mock(
        "hover",
        serde_json::json!({
            "hover": {
                "contents": {
                    "kind": "markdown",
                    "value": "fn target()\n\nThe target function\n",
                }
            }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "K");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP hover");
    assert_eq!(
        panel_lines,
        vec![
            "fn target()".to_string(),
            String::new(),
            "The target function".to_string(),
        ],
        "the hover markup is rendered as plain lines, trailing blank trimmed"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/hover"),
        "K should send a textDocument/hover request"
    );
}

#[tokio::test]
async fn a_long_hover_line_wraps_in_the_panel() {
    let _guard = test_lock().lock().await;
    // A hover line longer than the panel width must wrap across rows, not clip.
    // The panel spans the full terminal width (COLS), so a 100-char unbroken run
    // hard-breaks into an 80-cell row and a 20-cell row.
    let file = temp_file("hover-wrap", "rs", "fn main() {}\n");
    let long = "a".repeat(100);
    let record = configure_mock(
        "hover-wrap",
        serde_json::json!({
            "hover": { "contents": { "kind": "markdown", "value": long } }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "K");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP hover");
    assert_eq!(
        panel_lines,
        vec!["a".repeat(COLS as usize), "a".repeat(100 - COLS as usize)],
        "the long line wrapped to the panel width instead of being clipped"
    );
}

#[tokio::test]
async fn an_empty_hover_reply_reports_no_information() {
    let _guard = test_lock().lock().await;
    // The server has nothing to say at the cursor: a brief message, no panel.
    let file = temp_file("hover-empty", "rs", "fn main() {}\n");
    let record = configure_mock("hover-empty", serde_json::json!({}));
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "K");
    let params = wait_for_message(&rpc, &mut incoming, "No hover information").await;
    assert_eq!(message_of(&params), "No hover information");
    assert!(panel_of(&params).is_none(), "an empty hover opens no panel");
}

#[tokio::test]
async fn ctrl_k_shows_signature_help_with_the_active_parameter() {
    let _guard = test_lock().lock().await;
    // The mock returns a two-parameter signature with the second parameter active;
    // `<C-k>` in insert mode renders the active signature on the message line with
    // its active parameter highlighted in brackets.
    let file = temp_file("sighelp", "rs", "fn add(a: i32, b: i32) -> i32 { a }\n");
    let record = configure_mock(
        "sighelp",
        serde_json::json!({
            "signature_help": {
                "signatures": [
                    {
                        "label": "fn add(a: i32, b: i32) -> i32",
                        "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ],
                    }
                ],
                "activeSignature": 0,
                "activeParameter": 1,
            }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Enter insert mode, then trigger signature help with `<C-k>` (which must not
    // insert a literal `k`).
    feed(&rpc, "i<C-k>");
    let params = wait_for_message(
        &rpc,
        &mut incoming,
        "fn add(a: i32, b: i32) -> i32    [b: i32]",
    )
    .await;
    assert_eq!(
        message_of(&params),
        "fn add(a: i32, b: i32) -> i32    [b: i32]"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/signatureHelp"),
        "<C-k> should send a textDocument/signatureHelp request"
    );

    // The buffer is unchanged: `<C-k>` was consumed as a mapping, not typed.
    assert_eq!(
        lines(&rpc).await,
        vec!["fn add(a: i32, b: i32) -> i32 { a }".to_string()],
        "<C-k> did not insert a literal k"
    );
}
