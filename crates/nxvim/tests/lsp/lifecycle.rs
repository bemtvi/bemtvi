//! LSP Phase 1: lifecycle + document sync (didOpen / didChange / didSave /
//! didClose), encoding negotiation, resilience, and `LspInfo`.

use crate::support::*;

#[tokio::test]
async fn opening_a_rust_buffer_initializes_and_did_opens() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("init", serde_json::json!({}));
    let content = "fn main() {}\n";
    let file = temp_file("init", "rs", content);
    let (rpc, _incoming) = start(Some(file)).await;

    // The handshake then the first didOpen flow asynchronously: poll until both
    // are recorded.
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // initialize advertised utf-8 (preferred) among the position encodings.
    let init = find(&recs, "initialize").expect("an initialize request");
    let encodings = &init["params"]["capabilities"]["general"]["positionEncodings"];
    assert_eq!(
        encodings[0].as_str(),
        Some("utf-8"),
        "utf-8 should be advertised first, got {encodings:?}"
    );

    // didOpen carries the buffer text and the rust languageId, at version 1.
    let open = find(&recs, "textDocument/didOpen").unwrap();
    let doc = &open["params"]["textDocument"];
    assert_eq!(doc["text"].as_str(), Some(content));
    assert_eq!(doc["languageId"].as_str(), Some("rust"));
    assert_eq!(doc["version"].as_i64(), Some(1));

    // The LSP log got its START banner and (at DEBUG) the outgoing didOpen.
    let log = std::fs::read_to_string(lsp_log_path("init")).unwrap_or_default();
    assert!(
        log.contains("[START]") && log.contains("LSP logging initiated"),
        "the lsp log should carry a START banner, got:\n{log}"
    );
    assert!(
        log.contains("didOpen"),
        "the lsp log should record the outgoing didOpen at DEBUG, got:\n{log}"
    );
}

#[tokio::test]
async fn typing_sends_an_incremental_did_change_with_a_version_bump() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("change", serde_json::json!({ "sync_kind": "incremental" }));
    let file = temp_file("change", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Wait for the document to open first.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Insert "hello" at the top in one input batch.
    feed(&rpc, "ggihello<Esc>");
    let recs = wait_for_record(&rpc, &record, |r| did_change_text(r).contains("hello")).await;

    // The change(s) carry the inserted text, and the version bumped past the
    // didOpen's version 1.
    assert!(did_change_text(&recs).contains("hello"));
    let change = find(&recs, "textDocument/didChange").unwrap();
    assert!(
        change["params"]["textDocument"]["version"]
            .as_i64()
            .unwrap()
            >= 2,
        "the document version should bump past 1, got {:?}",
        change["params"]["textDocument"]["version"]
    );
    // Incremental sync sends ranges, not just full text.
    assert!(
        change["params"]["contentChanges"][0]["range"].is_object(),
        "incremental changes carry a range"
    );
}

#[tokio::test]
async fn did_change_reaches_the_server_after_the_syntax_worker_drains() {
    // Regression: the syntax worker and the LSP client each drain the buffer's
    // edit journal, and the syntax sync runs first. Once the worker has caught up
    // (not mid-parse), it consumed the edits before the LSP sync could — leaving
    // the language server's document frozen at `didOpen` (every `didChange`
    // carried 0 changes), so completion and friends ran against stale text. With
    // independent journals, an edit must still reach the server after syntax has
    // drained. This deterministically reproduces it by settling syntax first.
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "sync-share",
        serde_json::json!({ "sync_kind": "incremental" }),
    );
    let file = temp_file("sync-share", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Let the syntax worker reply (highlights appear), so it is now non-pending
    // and drains the journal before the LSP sync on the next edit.
    wait_for_highlights(&rpc, &mut incoming).await;

    // Type a distinctive change; the server must still receive it as a real
    // content change (before the fix this `didChange` was empty and `ZZZ` never
    // arrived).
    feed(&rpc, "oZZZ<Esc>");
    // `did_change_text` concatenates every `didChange`'s content-change texts;
    // the inserted `ZZZ` shows up there only if the server actually received the
    // change (before the fix, the journal was drained by syntax, so this batch's
    // `didChange` was empty and `ZZZ` never arrived — this would time out).
    let recs = wait_for_record(&rpc, &record, |r| did_change_text(r).contains("ZZZ")).await;
    assert!(
        did_change_text(&recs).contains("ZZZ"),
        "the language server received the edit after syntax drained the journal"
    );
}

#[tokio::test]
async fn a_non_ascii_prefix_yields_the_right_utf8_position() {
    // Regression guard for Decision 4: positions are byte/encoding units, not
    // char counts. The line starts with a 2-byte `é`, so an edit appended after
    // it must land at character 2 under the negotiated utf-8 encoding (a char
    // count would wrongly say 1).
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "utf8",
        serde_json::json!({ "position_encoding": "utf-8", "sync_kind": "incremental" }),
    );
    let file = temp_file("utf8", "rs", "é\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Append `x` at end of the line (byte column 2, right after `é`).
    feed(&rpc, "Ax<Esc>");
    let recs = wait_for_record(&rpc, &record, |r| did_change_text(r).contains('x')).await;

    let change = recs
        .iter()
        .find(|r| {
            r["method"] == "textDocument/didChange"
                && r["params"]["contentChanges"][0]["text"] == "x"
        })
        .expect("a didChange inserting x");
    let start = &change["params"]["contentChanges"][0]["range"]["start"];
    assert_eq!(start["line"].as_i64(), Some(0));
    assert_eq!(
        start["character"].as_i64(),
        Some(2),
        "the insert is at byte/utf-8 column 2 (after the 2-byte é), not char count 1"
    );
}

#[tokio::test]
async fn writing_then_deleting_sends_did_save_and_did_close() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("save", serde_json::json!({}));
    let file = temp_file("save", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Edit then write: the buffer's write counter advances on a successful :w.
    feed(&rpc, "ohello<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
        .await
        .expect("write");
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didSave")).await;

    // Delete the buffer: didClose.
    rpc.request("nvim_command", vec![Value::from("bd")])
        .await
        .expect("bdelete");
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didClose")).await;
    assert!(has_method(&recs, "textDocument/didSave"));
    assert!(has_method(&recs, "textDocument/didClose"));
}

#[tokio::test]
async fn undo_back_to_the_saved_state_does_not_fire_did_save() {
    // didSave is a real save hook (the buffer's write counter), not a
    // `modified`-flag heuristic: undoing back to the on-disk content clears
    // `modified` without any `:w`, and must NOT be mistaken for a save. Only a
    // real write does.
    let _guard = test_lock().lock().await;
    let record = configure_mock("nosave", serde_json::json!({}));
    let file = temp_file("nosave", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Edit, then undo straight back to the saved content (modified clears, no :w).
    feed(&rpc, "ohello<Esc>u");
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !has_method(&record_lines(&record), "textDocument/didSave"),
        "undo-to-clean must not fire didSave; saw {:?}",
        record_lines(&record)
    );

    // A genuine write now does fire it — proving the hook works, not just stays quiet.
    feed(&rpc, "ohello<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
        .await
        .expect("write");
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didSave")).await;
}

#[tokio::test]
async fn a_plain_text_buffer_starts_no_server() {
    let _guard = test_lock().lock().await;
    // The mock is configured, but a `.txt` filetype maps to no server.
    let record = configure_mock("plain", serde_json::json!({}));
    let file = temp_file("plain", "txt", "just text\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Give any (erroneous) server time to start and receive a message.
    feed(&rpc, "ihello<Esc>");
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        record_lines(&record).is_empty(),
        "a .txt buffer must never start a language server, got {:?}",
        record_lines(&record)
    );
}

#[tokio::test]
async fn the_editor_survives_a_server_that_exits_after_initialize() {
    // Resilience: the mock replies to initialize then exits, every time. The
    // manager respawns it (then the breaker gives up), but the editor must stay
    // fully responsive throughout — the LSP analogue of the syntax crash test.
    let _guard = test_lock().lock().await;
    configure_mock(
        "resil",
        serde_json::json!({ "exit_after_initialize": true }),
    );
    let file = temp_file("resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Hammer the editor with edits while the server crash-loops.
    feed(&rpc, "ggdGiline one<CR>line two<CR>line three<Esc>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string()
        ],
        "the editor must apply every keystroke regardless of the dying server"
    );
}

#[tokio::test]
async fn lsp_info_reports_the_running_server() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "info",
        serde_json::json!({ "position_encoding": "utf-8", "sync_kind": "incremental" }),
    );
    let file = temp_file("info", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    // Attach first (didOpen) so the server is initialized and the buffer attached.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    rpc.request("nvim_command", vec![Value::from("LspInfo")])
        .await
        .expect("LspInfo");
    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;

    assert_eq!(title, "LSP info");
    let body = lines.join("\n");
    assert!(body.contains("mock"), "names the mock server:\n{body}");
    assert!(
        body.contains("utf-8"),
        "shows the negotiated encoding:\n{body}"
    );
    assert!(body.contains("incremental"), "shows the sync kind:\n{body}");
    assert!(body.contains("attached"), "the buffer is attached:\n{body}");
}
