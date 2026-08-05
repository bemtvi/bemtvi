//! Behavior tests for the LSP **feature surfaces** that ride the intact engine
//! but were missing their Lua control surface: semantic tokens and inlay hints
//! (the `nx.lsp.semantic_tokens.*` / `nx.lsp.inlay_hint.*` buffer-state surface,
//! design §"buffer nouns", with `vim.lsp.*` aliases).
//!
//! Wired like `lsp_config.rs`: the scripted mock server (`nxvim --__lsp-mock`)
//! stands in for a real language server, `$NXVIM_LSP_CMD` overrides the spawn
//! argv, and a `rust`-filetype buffer drives the dispatch. The process-global env
//! means these tests serialize on `serial_lock`.

use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, barrier, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, menu_items,
    menu_of, message, mode, poll_menu, serial_lock, spawn, temp_dir, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// The `file://` URI of a path under `dir`, as the mock script's locations need.
fn file_uri(dir: &Path, name: &str) -> String {
    format!("file://{}", dir.join(name).display())
}

/// Poll for the latest redraw whose `menu` key is a map (the picker surface), then
/// return its visible row labels. `None` if no menu appears in the window.
async fn poll_menu_items(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    Some(menu_items(&menu_of(&poll_menu(rpc, incoming).await?)))
}

/// Byte offset of an (ASCII) LSP `(line, character)` position in `text`, clamped to
/// the line — the test-side inverse used to replay an incremental `didChange`.
fn pos_to_byte(text: &str, line: u64, ch: u64) -> usize {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let l = line as usize;
    let Some(&base) = starts.get(l) else {
        return text.len();
    };
    let line_end = starts.get(l + 1).copied().unwrap_or(text.len());
    (base + ch as usize).min(line_end)
}

/// Replay every `textDocument/didChange` the client recorded onto `original`,
/// reconstructing the document as the *server* sees it, and report whether any
/// change was a whole-document (range-less) replacement. Diverging from the real
/// buffer means we sent a bad incremental sync (the server would diagnose phantom
/// text); a range-less change means we fell back to full-text sync. ASCII-only (so
/// an LSP character index is a byte index).
fn replay_server_changes(original: &str, record_path: &Path) -> (String, bool) {
    let content = std::fs::read_to_string(record_path).unwrap_or_default();
    let mut doc = original.to_string();
    let mut any_full = false;
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("method").and_then(serde_json::Value::as_str) != Some("textDocument/didChange") {
            continue;
        }
        let Some(changes) = v
            .pointer("/params/contentChanges")
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for ch in changes {
            let text = ch
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match ch.get("range") {
                Some(range) => {
                    let g = |p: &str| {
                        range
                            .pointer(p)
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0)
                    };
                    let s = pos_to_byte(&doc, g("/start/line"), g("/start/character"));
                    let e = pos_to_byte(&doc, g("/end/line"), g("/end/character"));
                    doc.replace_range(s..e, text);
                }
                // A `range`-less change is a whole-document replacement (FULL sync).
                None => {
                    any_full = true;
                    doc = text.to_string();
                }
            }
        }
    }
    (doc, any_full)
}

/// Write a mock LSP script and point `$NXVIM_LSP_CMD` at the binary's
/// `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer (filetype `rust`), attach, and `nx.lsp.enable` a mock
/// server bound to the `cmd` placeholder. Returns once the buffer is open; the
/// caller polls for the async attach + reply to settle.
async fn open_with_server(dir: &Path, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    feed(&rpc, "gg0");
    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", { cmd = { "mock" }, filetypes = { "rust" } })
        nx.lsp.enable({ "mock" })
        "#,
    )
    .await;
    (rpc, incoming)
}

/// Poll `expr` (a `return`-ed Lua expression) until it equals `want`, or fail.
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..200 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Poll `expr` (a `return`-ed Lua expression) until its string form contains `want`;
/// returns the last value seen.
async fn await_lua_contains(rpc: &Rpc, expr: &str, want: &str) -> String {
    let code = format!("return tostring({expr})");
    let mut last = String::new();
    for _ in 0..200 {
        last = exec_lua(rpc, &code)
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        if last.contains(want) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    last
}

/// Enabling inlay hints requests them, the mock replies, and the decoded hint
/// reaches the `nx.lsp.inlay_hint.get` read mirror — exercising the full
/// enable→request→reply→`nx._set_inlay_hints`→getter chain (the mirror receiver
/// was dangling before this surface, so the push silently errored).
#[tokio::test]
async fn inlay_hint_enable_requests_and_get_reads_the_mirror() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features");
    arm_mock(
        &dir,
        r#"{
            "inlay_hints": [
                { "position": { "line": 0, "character": 7 }, "label": ": i32", "kind": 1 }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let x = 1\n").await;

    // Off by default.
    assert!(
        await_lua_eq(&rpc, "nx.lsp.inlay_hint.is_enabled()", "false").await,
        "inlay hints should be off by default"
    );

    // Enable → the engine requests hints, the mock replies, the mirror fills.
    exec_lua(&rpc, "vim.lsp.inlay_hint.enable(true)").await;
    assert!(
        await_lua_eq(&rpc, "nx.lsp.inlay_hint.is_enabled()", "true").await,
        "is_enabled should report on after enable"
    );
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.inlay_hint.get({ bufnr = 0 })", "1").await,
        "the decoded hint should reach the get() mirror"
    );
    assert!(
        await_lua_eq(
            &rpc,
            "(nx.lsp.inlay_hint.get({ bufnr = 0 })[1] or {}).inlay_hint.label",
            ": i32"
        )
        .await,
        "the hint's label should round-trip through the mirror"
    );

    // Disabling clears the mirror (an empty list is pushed).
    exec_lua(&rpc, "vim.lsp.inlay_hint.enable(false)").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.inlay_hint.get({ bufnr = 0 })", "0").await,
        "disabling should clear the hint mirror"
    );
}

/// Semantic tokens are decoded and pushed to the `get_at_pos` read mirror once a
/// server with the capability attaches (the projection is on by default) —
/// exercising the `nx._set_semantic_tokens` receiver and the position getter.
#[tokio::test]
async fn semantic_tokens_decode_into_the_get_at_pos_mirror() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_sem");
    // One token: line 0, delta-start col 4, length 3, type index 0 ("keyword"),
    // no modifiers. LSP packs as [deltaLine, deltaStart, length, type, mods].
    arm_mock(
        &dir,
        r#"{
            "semantic_tokens": {
                "legend": { "tokenTypes": ["keyword", "variable"], "tokenModifiers": [] },
                "data": [0, 4, 3, 0, 0]
            }
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = 1\n").await;

    // The token at (row 0, col 4) decodes and reaches the mirror.
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.semantic_tokens.get_at_pos(0, 0, 4)", "1").await,
        "the decoded token should be readable at its column"
    );
    assert!(
        await_lua_eq(
            &rpc,
            "(nx.lsp.semantic_tokens.get_at_pos(0, 0, 4)[1] or {}).type",
            "keyword"
        )
        .await,
        "the token type should round-trip through the mirror"
    );
    // A column outside [start_col, end_col) carries no token.
    assert_eq!(
        exec_lua(&rpc, "return #nx.lsp.semantic_tokens.get_at_pos(0, 0, 0)")
            .await
            .as_i64(),
        Some(0),
        "a column before the token should be empty"
    );
}

/// `references` (always a list) resolves into `nx.picker` — the multi-hit reply
/// opens the picker with one `path:line:col` row per location (design principle 4:
/// locations dogfood the shared picker, not a bespoke loclist).
#[tokio::test]
async fn references_open_the_location_picker() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_refs");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "references": [
                    {{ "uri": "{uri}", "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }} }},
                    {{ "uri": "{uri}", "range": {{ "start": {{ "line": 1, "character": 0 }}, "end": {{ "line": 1, "character": 3 }} }} }}
                ]
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = bar()\nfoo()\n").await;
    // Wait for the server to attach before issuing the request.
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let mut items = None;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.references()").await;
        if let Some(rows) = poll_menu_items(&rpc, &mut incoming).await {
            if rows.len() == 2 {
                items = Some(rows);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let items = items.expect("references should open a 2-row location picker");
    assert!(
        items.iter().all(|r| r.contains("a.rs:")),
        "each picker row should be a path:line:col location, got {items:?}"
    );
    // Confirm jumps the cursor to a reference location (the picker's selected row;
    // cursor() is 1-based, so the two reference lines are rows 1 and 2).
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;
    let landed = cursor(&rpc).await.0;
    assert!(
        landed == 1 || landed == 2,
        "confirming a reference jumps to one of its lines (1-based), got {landed}"
    );
}

/// A `definition` reply with a single hit jumps straight to it — it never reaches
/// the picker (only multi-hit / list replies do).
#[tokio::test]
async fn single_definition_jumps_without_a_picker() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_def");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "definition": {{ "uri": "{uri}", "range": {{ "start": {{ "line": 2, "character": 0 }}, "end": {{ "line": 2, "character": 3 }} }} }}
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = bar()\nfoo()\nbar()\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Cursor starts on line 1 (1-based); the definition jumps it to the target's
    // 0-based line 2, which `cursor()` reports 1-based as row 3.
    let mut jumped = false;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.definition()").await;
        nxvim_test_harness::barrier(&rpc).await;
        if cursor(&rpc).await.0 == 3 {
            jumped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        jumped,
        "a single definition hit should jump the cursor to the target line"
    );
}

/// `document_symbol` flattens the server's symbol list into the picker — one row
/// per symbol, each tagged with its kind, jumping to its location on confirm.
#[tokio::test]
async fn document_symbol_opens_the_symbol_picker() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_docsym");
    // Nested DocumentSymbols carry no URI (implicitly the document's), so none here.
    // A nested DocumentSymbol (a struct with a field) — the flatten walks children.
    arm_mock(
        &dir,
        r#"{
                "document_symbols": [
                    {
                        "name": "Foo", "kind": 23,
                        "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                        "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 10 } },
                        "children": [
                            {
                                "name": "bar", "kind": 8,
                                "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 12 } },
                                "selectionRange": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 7 } }
                            }
                        ]
                    }
                ]
            }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "struct Foo {\n    bar: i32,\n}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let mut items = None;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.document_symbol()").await;
        if let Some(rows) = poll_menu_items(&rpc, &mut incoming).await {
            if rows.len() == 2 {
                items = Some(rows);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let items = items.expect("document symbols should open a 2-row picker (struct + nested field)");
    assert!(
        items
            .iter()
            .any(|r| r.contains("Foo") && r.contains("[Struct]")),
        "the struct symbol row should carry its name + kind, got {items:?}"
    );
    assert!(
        items
            .iter()
            .any(|r| r.contains("bar") && r.contains("[Field]")),
        "the nested field symbol should be flattened in, got {items:?}"
    );
}

/// `workspace_symbol(query)` requests `workspace/symbol` and opens the matches in
/// the picker — the flat `SymbolInformation` form carries its own location.
#[tokio::test]
async fn workspace_symbol_opens_the_symbol_picker() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_wssym");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "workspace_symbols": [
                    {{
                        "name": "Widget", "kind": 5,
                        "location": {{ "uri": "{uri}", "range": {{ "start": {{ "line": 0, "character": 6 }}, "end": {{ "line": 0, "character": 12 }} }} }}
                    }}
                ]
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "class Widget {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let mut items = None;
    for _ in 0..80 {
        // Pass the query directly (no prompt) so the test stays non-interactive.
        exec_lua(&rpc, "nx.lsp.workspace_symbol('Wid')").await;
        if let Some(rows) = poll_menu_items(&rpc, &mut incoming).await {
            if !rows.is_empty() {
                items = Some(rows);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let items = items.expect("workspace symbols should open the picker");
    assert!(
        items
            .iter()
            .any(|r| r.contains("Widget") && r.contains("[Class]")),
        "the workspace symbol row should carry its name + kind, got {items:?}"
    );
}

/// `format()` applies the server's `textDocument/formatting` edits to the buffer.
#[tokio::test]
async fn format_applies_the_servers_text_edits() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_fmt");
    // Replace the whole (badly-spaced) first line with a tidy one.
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                {
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 9 } },
                    "newText": "let x = 1"
                }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let  x=1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let mut formatted = false;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.format()").await;
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.first().map(String::as_str) == Some("let x = 1") {
            formatted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(formatted, "formatting should rewrite the first line");
}

/// Formatting edits that SHARE a start position (a line-diff formatter like
/// efm-langserver deletes the changed lines and emits several zero-width inserts at
/// one point) must land in ARRAY order. Regression: they were applied so each insert
/// prepended before the previous, silently swapping adjacent reformatted lines.
#[tokio::test]
async fn format_edits_sharing_a_position_keep_array_order() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_fmt_order");
    // Delete lines 1 and 2, then insert NEW_A and NEW_B (in that order) at line 3 —
    // the shape efm emits for a two-line reformat (inserts at the end of the deleted
    // block). The two inserts share the start position, so their order is the test.
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                { "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 2, "character": 0 } }, "newText": "" },
                { "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 3, "character": 0 } }, "newText": "" },
                { "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 0 } }, "newText": "NEW_A\n" },
                { "range": { "start": { "line": 3, "character": 0 }, "end": { "line": 3, "character": 0 } }, "newText": "NEW_B\n" }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "keep0\nDELME1\nDELME2\nkeep3\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let want = vec!["keep0", "NEW_A", "NEW_B", "keep3"];
    let mut ok = false;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.format()").await;
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await == want {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        ok,
        "same-position inserts must keep array order (NEW_A before NEW_B); got {:?}",
        lines(&rpc).await
    );
}

/// `rename(name)` applies the server's `WorkspaceEdit` across the buffer.
#[tokio::test]
async fn rename_applies_the_workspace_edit() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_rename");
    let uri = file_uri(&dir, "a.rs");
    // Rename `foo` → `bar` on line 0.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "rename": {{
                    "changes": {{
                        "{uri}": [
                            {{
                                "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }},
                                "newText": "bar"
                            }}
                        ]
                    }}
                }}
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let mut renamed = false;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.rename('bar')").await;
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.first().map(String::as_str) == Some("let bar = 1") {
            renamed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        renamed,
        "rename should rewrite the symbol across the buffer"
    );
}

/// A project-wide rename whose `WorkspaceEdit` also touches a file that was never
/// opened must load that file into a buffer on the spot and apply the edit there —
/// the cross-file rename neovim's `apply_text_edits` does. The unopened file's
/// buffer is left modified (saved with `:wa`), exactly as the open buffer is.
#[tokio::test]
async fn rename_reaches_an_unopened_file() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_rename_unopened");
    let uri_a = file_uri(&dir, "a.rs");
    let uri_b = file_uri(&dir, "b.rs");
    // `b.rs` exists on disk but is never opened by the test; the rename must reach it.
    std::fs::write(dir.join("b.rs"), "use a::foo;\nfn g() { foo() }\n").expect("write b.rs");
    // Rename `foo` → `bar`: one occurrence in the open `a.rs`, two in the unopened `b.rs`.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "rename": {{
                    "changes": {{
                        "{uri_a}": [
                            {{
                                "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }},
                                "newText": "bar"
                            }}
                        ],
                        "{uri_b}": [
                            {{
                                "range": {{ "start": {{ "line": 0, "character": 7 }}, "end": {{ "line": 0, "character": 10 }} }},
                                "newText": "bar"
                            }},
                            {{
                                "range": {{ "start": {{ "line": 1, "character": 9 }}, "end": {{ "line": 1, "character": 12 }} }},
                                "newText": "bar"
                            }}
                        ]
                    }}
                }}
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Request the rename once and wait for the open buffer to settle.
    let mut renamed = false;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.rename('bar')").await;
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.first().map(String::as_str) == Some("let bar = 1") {
            renamed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(renamed, "rename should rewrite the open buffer");

    // The unopened `b.rs` was loaded into a buffer and edited in place. Switch to it
    // by absolute path (the load reused — not re-read — its on-disk path) and check
    // both occurrences.
    let b_path = dir.join("b.rs");
    feed(&rpc, &format!(":edit {}<CR>", b_path.display()));
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["use a::bar;", "fn g() { bar() }"],
        "the rename should reach both occurrences in the unopened file"
    );
}

/// A rename whose `WorkspaceEdit` inserts a line *above* the cursor (e.g. an added
/// `use` import) must carry the cursor down with the text it sits on — like
/// neovim's `apply_text_edits` — not leave it pinned to a now-stale absolute line.
/// Regression: the cursor was left on the previous line's content (its "previous
/// edit point") instead of following the symbol it was on.
#[tokio::test]
async fn rename_carries_the_cursor_through_edits_above_it() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_rename_cursor");
    let uri = file_uri(&dir, "a.rs");
    // The rename of `foo` → `bar` also inserts a `use bar;` line at the top (an
    // import the server adds), plus rewrites both occurrences.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "rename": {{
                    "changes": {{
                        "{uri}": [
                            {{
                                "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 0 }} }},
                                "newText": "use bar;\n"
                            }},
                            {{
                                "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }},
                                "newText": "bar"
                            }},
                            {{
                                "range": {{ "start": {{ "line": 3, "character": 7 }}, "end": {{ "line": 3, "character": 10 }} }},
                                "newText": "bar"
                            }}
                        ]
                    }}
                }}
            }}"#
        ),
    );
    let (rpc, _incoming) =
        open_with_server(&dir, "let foo = 1\nlet a = 2\nlet b = 3\nreturn foo\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Park the cursor on the second occurrence (line 4, the `foo` in `return foo`).
    feed(&rpc, "4G0fo");
    barrier(&rpc).await;
    let before = cursor(&rpc).await;
    assert_eq!(before.0, 4, "precondition: cursor parked on line 4");

    // Request the rename exactly once and wait for its single reply — re-requesting
    // in the poll loop can stack a second `use bar;` insert (cursor would land a line
    // too far), so the apply must be one-shot for the line assertion to be exact.
    exec_lua(&rpc, "nx.lsp.rename('bar')").await;
    let mut renamed = false;
    for _ in 0..80 {
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.last().map(String::as_str) == Some("return bar") {
            renamed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        renamed,
        "rename should rewrite the symbol and add the import"
    );

    // The inserted `use bar;` pushed everything down one line, so the symbol the
    // cursor sat on is now on line 5. The cursor must have followed it there.
    let after = cursor(&rpc).await;
    assert_eq!(
        after.0, 5,
        "the cursor should follow its line down past the inserted import, \
         not stay pinned to the now-previous line"
    );
}

/// Undoing a rename must land the cursor where it was *before* the rename (on the
/// renamed symbol), not at the top of the file. Regression: a workspace edit didn't
/// bake the live cursor into the node it would undo back to, so the root node's
/// stale top-of-file cursor was restored when the symbol was reached by navigation
/// (no intervening edit) rather than by editing.
#[tokio::test]
async fn undo_of_a_rename_restores_the_pre_rename_cursor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_rename_undo");
    let uri = file_uri(&dir, "a.rs");
    // Rename `foo` → `bar` on the last line only.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "rename": {{
                    "changes": {{
                        "{uri}": [
                            {{
                                "range": {{ "start": {{ "line": 3, "character": 7 }}, "end": {{ "line": 3, "character": 10 }} }},
                                "newText": "bar"
                            }}
                        ]
                    }}
                }}
            }}"#
        ),
    );
    let (rpc, _incoming) =
        open_with_server(&dir, "let a = 1\nlet b = 2\nlet c = 3\nreturn foo\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Reach the symbol purely by *navigation* (no edit), so the only committed undo
    // node is the root — whose snapshot cursor is the load-time top of file.
    feed(&rpc, "4G0fo");
    barrier(&rpc).await;
    assert_eq!(cursor(&rpc).await.0, 4, "precondition: cursor on line 4");

    // One-shot request (re-requesting could double-apply and add extra undo nodes).
    exec_lua(&rpc, "nx.lsp.rename('bar')").await;
    let mut renamed = false;
    for _ in 0..80 {
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.last().map(String::as_str) == Some("return bar") {
            renamed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(renamed, "rename should rewrite the symbol");

    // Undo the rename. The text reverts *and* the cursor returns to line 4, not 1.
    feed(&rpc, "u");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await.last().map(String::as_str),
        Some("return foo"),
        "undo should revert the rename"
    );
    assert_eq!(
        cursor(&rpc).await.0,
        4,
        "undo should restore the cursor to the symbol, not the top of the file"
    );
}

/// After a multi-occurrence rename, the `didChange` stream we send must reconstruct
/// *exactly* the renamed buffer on the server side — otherwise the server diagnoses
/// phantom text. The classic failure is a UTF-16 server (rust-analyzer's default):
/// the journaled byte deltas were converted to code-unit columns against the
/// post-edit buffer, clamping a shortened line's later deltas, so `balance` → `aa`
/// left the server seeing `aae`. Verified under both negotiated encodings.
async fn check_rename_sync_round_trips(encoding: &str) {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir(&format!(
        "lsp_features_rename_sync_{}",
        encoding.replace('-', "")
    ));
    let uri = file_uri(&dir, "a.rs");
    let record = dir.join("rec.jsonl");
    // Rename both `balance` occurrences → `aa`: line 0 (char 4..11) and line 1
    // (char 9..16, inside `fn f() { balance }`).
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "position_encoding": "{encoding}",
                "rename": {{
                    "changes": {{
                        "{uri}": [
                            {{
                                "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 11 }} }},
                                "newText": "aa"
                            }},
                            {{
                                "range": {{ "start": {{ "line": 1, "character": 9 }}, "end": {{ "line": 1, "character": 16 }} }},
                                "newText": "aa"
                            }}
                        ]
                    }}
                }}
            }}"#,
            rec = record.display(),
        ),
    );
    let original = "let balance = 1\nfn f() { balance }\n";
    let (rpc, _incoming) = open_with_server(&dir, original).await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.rename('aa')").await;
    let mut renamed = false;
    for _ in 0..80 {
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.first().map(String::as_str) == Some("let aa = 1") {
            renamed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(renamed, "rename should rewrite both occurrences");
    // Let the post-rename `didChange` flush to the mock's record file.
    tokio::time::sleep(Duration::from_millis(50)).await;
    nxvim_test_harness::barrier(&rpc).await;

    let buffer_text = format!("{}\n", lines(&rpc).await.join("\n"));
    let (server_view, any_full) = replay_server_changes(original, &record);
    assert_eq!(
        server_view, buffer_text,
        "[{encoding}] the server's reconstructed document must match the buffer after the rename"
    );
    // The fix must stay *incremental* (neovim-style shadow), not fall back to
    // shipping the whole document on every change — including under UTF-16.
    assert!(
        !any_full,
        "[{encoding}] the rename should sync as incremental ranged edits, not a full-document replacement"
    );
}

#[tokio::test]
async fn rename_sync_round_trips_utf16() {
    check_rename_sync_round_trips("utf-16").await;
}

#[tokio::test]
async fn rename_sync_round_trips_utf8() {
    check_rename_sync_round_trips("utf-8").await;
}

/// A whole-document format (one edit spanning the cursor) must leave the cursor on
/// its line, not fling it to the end of the file. Regression: an edit the cursor
/// sat *inside* moved it to the end of the replacement — the whole buffer.
#[tokio::test]
async fn format_does_not_fling_the_cursor_to_end_of_file() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_fmt_cursor");
    // One edit replacing the entire (badly-spaced) buffer with a tidy one.
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                {
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 8 } },
                    "newText": "let a = 1\nlet b = 2\nlet c = 3"
                }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let  a=1\nlet  b=2\nlet  c=3\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Park the cursor on the middle line.
    feed(&rpc, "2G0");
    barrier(&rpc).await;
    assert_eq!(cursor(&rpc).await.0, 2, "precondition: cursor on line 2");

    // One-shot request, then wait for the reply (re-requesting risks a stale double).
    exec_lua(&rpc, "nx.lsp.format()").await;
    let mut formatted = false;
    for _ in 0..80 {
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.get(1).map(String::as_str) == Some("let b = 2") {
            formatted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(formatted, "formatting should tidy the buffer");

    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "the cursor should stay on its line through a whole-document format, \
         not fly to the end of the file"
    );
}

/// Undo then redo of a format must round-trip the cursor: undo returns it to the
/// pre-format spot, redo returns it to where the format left it. Regression: the
/// redo node was committed *before* the cursor was repositioned, so redo restored
/// the pre-format cursor instead of the post-format one.
#[tokio::test]
async fn redo_of_a_format_restores_the_post_format_cursor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_fmt_redo");
    // The format inserts a header line at the top, pushing the cursor's line down.
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                {
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } },
                    "newText": "// header\n"
                }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1\nlet b = 2\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Park the cursor on line 2; the inserted header pushes it down to line 3.
    feed(&rpc, "2G0");
    barrier(&rpc).await;

    // Request the format exactly once, then wait for its single reply to apply —
    // re-requesting in the poll loop would stack extra header inserts (extra undo
    // nodes) and break the undo/redo sequence this test exercises.
    exec_lua(&rpc, "nx.lsp.format()").await;
    let mut formatted = false;
    for _ in 0..80 {
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.first().map(String::as_str) == Some("// header") {
            formatted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(formatted, "formatting should insert the header");
    let after_format = cursor(&rpc).await;
    assert_eq!(
        after_format.0, 3,
        "the cursor should follow its line down to 3"
    );

    // Undo returns the cursor to the pre-format line 2.
    feed(&rpc, "u");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await.first().map(String::as_str),
        Some("let a = 1"),
        "undo should drop the header"
    );
    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "undo should restore the cursor to line 2"
    );

    // Redo re-applies the format and returns the cursor to where it left it (line 3).
    feed(&rpc, "<C-r>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await.first().map(String::as_str),
        Some("// header"),
        "redo should re-insert the header"
    );
    assert_eq!(
        cursor(&rpc).await.0,
        after_format.0,
        "redo should restore the post-format cursor, not the pre-format one"
    );
}

/// `code_action()` lists the server's actions in the select menu; confirming one
/// applies its eager `WorkspaceEdit`.
#[tokio::test]
async fn code_action_lists_then_applies_the_chosen_action() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_ca");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "code_action": [
                    {{
                        "title": "Replace with bar",
                        "edit": {{
                            "changes": {{
                                "{uri}": [
                                    {{
                                        "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }},
                                        "newText": "bar"
                                    }}
                                ]
                            }}
                        }}
                    }}
                ]
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // The action title shows in the select menu.
    let mut listed = false;
    for _ in 0..80 {
        exec_lua(&rpc, "nx.lsp.code_action()").await;
        if let Some(rows) = poll_menu_items(&rpc, &mut incoming).await {
            if rows.iter().any(|r| r.contains("Replace with bar")) {
                listed = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(listed, "the code action should appear in the select menu");

    // The chooser opens noselect (an explicit pick is required, like the completion
    // popup), so highlight the row first, then confirm applies its edit.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");
    let mut applied = false;
    for _ in 0..40 {
        nxvim_test_harness::barrier(&rpc).await;
        if lines(&rpc).await.first().map(String::as_str) == Some("let bar = 1") {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(applied, "confirming the code action should apply its edit");
}

/// Per-row underline-span count from a redraw's focused-window `diagnostics`
/// array. Empty when the key is absent.
fn diag_span_counts(map: &[(Value, Value)]) -> Vec<usize> {
    window0_field(map, "diagnostics")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|r| r.as_array().map_or(0, Vec::len))
                .collect()
        })
        .unwrap_or_default()
}

/// The server-pushed set and `vim.diagnostic.set` paint TOGETHER on one buffer:
/// `diagnostics_merged` is additive, not either/or. The mock reports an error on
/// line 0; the client adds one on line 1; both rows must carry an underline span.
#[tokio::test]
async fn lsp_and_client_set_diagnostics_coexist_on_one_buffer() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features");
    arm_mock(
        &dir,
        r#"{
            "diagnostics": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end":   { "line": 0, "character": 4 } },
                  "severity": 1, "message": "server says no" }
            ]
        }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "aaaa bbbb\ncccc dddd\n").await;

    // Wait for the server's publishDiagnostics to land (row 0 gets a span).
    let mut got_lsp = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |_| true) {
            if diag_span_counts(&map).first().copied().unwrap_or(0) > 0 {
                got_lsp = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(got_lsp, "the server's diagnostic should paint on row 0");

    // Add a client-set diagnostic on row 1, no server involved.
    exec_lua(
        &rpc,
        r#"vim.diagnostic.set(7, 0, {
          { lnum = 1, col = 0, end_lnum = 1, end_col = 4, severity = 2, message = "client says no" },
        })"#,
    )
    .await;

    // Both rows now carry a span — the two sources coexist.
    let mut both = false;
    for _ in 0..80 {
        barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |_| true) {
            let counts = diag_span_counts(&map);
            if counts.first().copied().unwrap_or(0) > 0 && counts.get(1).copied().unwrap_or(0) > 0 {
                both = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        both,
        "LSP (row 0) and client-set (row 1) diagnostics must paint together"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The 0-based rows carrying an underline span, from the latest frame.
async fn painted_diag_rows(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<usize> {
    barrier(rpc).await;
    let Some(map) = drain_to_latest_redraw(incoming, |_| true) else {
        return Vec::new();
    };
    diag_span_counts(&map)
        .into_iter()
        .enumerate()
        .filter(|&(_, n)| n > 0)
        .map(|(row, _)| row)
        .collect()
}

/// Poll until the painted rows equal `want`, or give up. Returns what was last seen.
async fn await_painted_rows(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &[usize],
) -> Vec<usize> {
    let mut last = Vec::new();
    for _ in 0..200 {
        last = painted_diag_rows(rpc, incoming).await;
        if last == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    last
}

/// A server republishing per keystroke — which is what every real language server
/// does, since nxvim syncs a `didChange` per key — must not repaint while you type.
/// The publish is held (here under an interval long enough that only `InsertLeave`
/// can end the wait) and applied whole on the way back to normal mode.
#[tokio::test]
async fn server_publishes_during_insert_are_held_until_insert_leave() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features");
    arm_mock(
        &dir,
        r#"{
            "diagnostics": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end":   { "line": 0, "character": 4 } },
                  "severity": 1, "message": "server says no" }
            ],
            "diagnostics_on_change": true
        }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "aaaa bbbb\ncccc dddd\n").await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;

    // The `didOpen` publish lands on row 0 while we're in normal mode.
    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[0]).await,
        vec![0],
        "the server's opening diagnostic should paint on row 0"
    );

    // Type on line 1. Each key is a `didChange`, and the mock answers each with a
    // publish naming row 1 — none of which may reach the screen while inserting.
    feed(&rpc, "jAxyz");
    for _ in 0..20 {
        assert_eq!(
            painted_diag_rows(&rpc, &mut incoming).await,
            vec![0],
            "a publish landing mid-insert must not repaint"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // `vim.diagnostic.get` reports the same held set the screen shows, so the two
    // can't disagree for the length of an insert.
    let held = exec_lua(&rpc, "return vim.diagnostic.get(0)[1].message").await;
    assert_eq!(
        held.as_str(),
        Some("server says no"),
        "the mirror stays on the held set too"
    );

    // Leaving insert applies the newest held publish — the one for the last key.
    feed(&rpc, "<Esc>");
    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[1]).await,
        vec![1],
        "`InsertLeave` applies what the server published while typing"
    );
    let resumed = await_lua_contains(&rpc, "vim.diagnostic.get(0)[1].message", "typed: z").await;
    assert!(
        resumed.contains("typed: z"),
        "the applied set is the publish for the LAST keystroke, got {resumed:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The default timing, against a real per-keystroke publisher: held while the keys
/// are coming, then applied by the debounce once typing stops — still in insert
/// mode, no `<Esc>` needed. (A short interval here; the shipped default is 3s.)
#[tokio::test]
async fn a_held_server_publish_applies_once_typing_goes_quiet() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features");
    arm_mock(
        &dir,
        r#"{
            "diagnostics": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end":   { "line": 0, "character": 4 } },
                  "severity": 1, "message": "server says no" }
            ],
            "diagnostics_on_change": true
        }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "aaaa bbbb\ncccc dddd\n").await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 150 })").await;
    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[0]).await,
        vec![0],
        "the server's opening diagnostic should paint on row 0"
    );

    feed(&rpc, "jAxyz");
    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[1]).await,
        vec![1],
        "the debounce applies the publish once the keys stop"
    );
    assert_eq!(
        mode(&rpc).await,
        "i",
        "and it did so without leaving insert mode"
    );
    let applied = await_lua_contains(&rpc, "vim.diagnostic.get(0)[1].message", "typed: z").await;
    assert!(
        applied.contains("typed: z"),
        "the applied set is the publish for the LAST keystroke, got {applied:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The opt-out: `update_in_insert = true` puts every publish on screen the moment
/// it lands, mid-insert included.
#[tokio::test]
async fn update_in_insert_lets_server_publishes_through_while_typing() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features");
    arm_mock(
        &dir,
        r#"{
            "diagnostics": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end":   { "line": 0, "character": 4 } },
                  "severity": 1, "message": "server says no" }
            ],
            "diagnostics_on_change": true
        }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "aaaa bbbb\ncccc dddd\n").await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = true })").await;
    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[0]).await,
        vec![0],
        "the server's opening diagnostic should paint on row 0"
    );

    // Still in insert mode, the row-1 publish reaches the screen.
    feed(&rpc, "jAxyz");
    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[1]).await,
        vec![1],
        "with `update_in_insert` on, a publish repaints while typing"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The visible buffer-line numbers of window 0 in `map` (filler rows dropped) — what
/// the screen shows once folds collapse hidden lines.
fn visible_numbers(map: &[(Value, Value)]) -> Vec<u64> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// Pump redraws (each `barrier` forces one) until window 0's visible line numbers
/// satisfy `pred` — used to wait out the async `foldingRange` round-trip. Returns
/// the matching numbers, or `None` if they never settle.
async fn poll_visible_numbers(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    pred: impl Fn(&[u64]) -> bool,
) -> Option<Vec<u64>> {
    for _ in 0..200 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            let nums = visible_numbers(&map);
            if pred(&nums) {
                return Some(nums);
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

/// `foldmethod=expr` with the LSP foldexpr marker requests `textDocument/foldingRange`
/// and folds the buffer from the server's ranges: a `[1,3]` range collapses lines
/// 2-4 behind a placeholder, so only lines 1, 2, 5, 6 stay visible.
#[tokio::test]
async fn lsp_folding_range_folds_the_buffer() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_folding");
    arm_mock(
        &dir,
        r#"{ "folding_ranges": [ { "startLine": 1, "endLine": 3 } ] }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "L1\nL2\nL3\nL4\nL5\nL6\n").await;
    // Switch the buffer to the LSP fold source. The next redraw issues the
    // foldingRange request, the mock replies, and the fold engine collapses [1,3].
    feed(&rpc, ":set foldmethod=expr<CR>");
    feed(&rpc, ":set foldexpr=v:lua.nx.lsp.foldexpr()<CR>");
    let numbers = poll_visible_numbers(&rpc, &mut incoming, |n| n == [1, 2, 5, 6]).await;
    assert_eq!(
        numbers,
        Some(vec![1, 2, 5, 6]),
        "the LSP folding range [1,3] should hide lines 3-4"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

// ===== Phase 1: async `nx.lsp.*` verbs return promises ===================
// docs/plans/2026-07-23-async-lsp-verbs.md — the verbs resolve when the round-trip
// completes and its effect is applied/presented, so actions can be sequenced.

/// `format()` returns a promise that resolves only AFTER the server's edits are
/// applied: a `:next` continuation reads the buffer and sees the *formatted* text,
/// proving the resolution happens post-apply (not when the request is merely sent).
#[tokio::test]
async fn format_promise_resolves_after_edits_apply() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_fmt");
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 8 } }, "newText": "let x = 1" }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let  x=1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Issue once; the reply lands async and settles the promise, whose continuation
    // records the line text it sees at resolution time.
    exec_lua(
        &rpc,
        r#"
        _G.fmt_seen = nil
        _G.fmt_done = false
        nx.lsp.format():next(function()
            _G.fmt_seen = (vim.api.nvim_buf_get_lines(0, 0, 1, false))[1]
            _G.fmt_done = true
        end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.fmt_done)", "true").await,
        "the format promise should resolve"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.fmt_seen").await.as_str(),
        Some("let x = 1"),
        "the continuation runs after the edits applied (it sees formatted text)"
    );
}

/// `references()` resolves its promise with the `{ text, path, row, col }` item
/// list — the same rows the picker shows — so a handler can consume the locations.
#[tokio::test]
async fn references_promise_resolves_with_the_item_list() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_refs");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "references": [
                    {{ "uri": "{uri}", "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }} }},
                    {{ "uri": "{uri}", "range": {{ "start": {{ "line": 1, "character": 0 }}, "end": {{ "line": 1, "character": 3 }} }} }}
                ]
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = bar()\nfoo()\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.ref_items = nil
        nx.lsp.references():next(function(items) _G.ref_items = items end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.ref_items and #_G.ref_items or 0)", "2").await,
        "the references promise should resolve with 2 location items"
    );
    // Each item carries the fields a picker row / a `nx.picker.edit` jump needs.
    assert!(
        await_lua_eq(
            &rpc,
            "tostring(_G.ref_items[1].path ~= nil and _G.ref_items[1].row ~= nil and _G.ref_items[1].col ~= nil)",
            "true"
        )
        .await,
        "each resolved item should carry path/row/col"
    );
    assert!(
        await_lua_eq(
            &rpc,
            "tostring(string.find(_G.ref_items[1].path, 'a.rs', 1, true) ~= nil)",
            "true"
        )
        .await,
        "the item path should be the referenced file"
    );
}

/// The headline: two edit verbs run in SEQUENCE — `format():next(-> rename())` runs
/// the rename only after the format edits land, and the final `:next` only after the
/// rename lands. The continuations witness the buffer at each step, proving ordering.
#[tokio::test]
async fn edit_verbs_chain_in_sequence() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_chain");
    let uri = file_uri(&dir, "a.rs");
    // format rewrites line 1 ("x" -> "y"); rename rewrites "foo" -> "bar" on line 0.
    // The two touch different lines so each edit's range stays valid at apply time.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "formatting": [
                    {{ "range": {{ "start": {{ "line": 1, "character": 0 }}, "end": {{ "line": 1, "character": 1 }} }}, "newText": "y" }}
                ],
                "rename": {{
                    "changes": {{
                        "{uri}": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }}, "newText": "bar" }}
                        ]
                    }}
                }}
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "foo\nx\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.chain = {}
        local function snap()
            return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "|")
        end
        nx.lsp.format():next(function()
            table.insert(_G.chain, snap())      -- after format, before rename
            return nx.lsp.rename("bar")
        end):next(function()
            table.insert(_G.chain, snap())      -- after rename
        end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(#_G.chain)", "2").await,
        "both continuations should run (rename ran after format's promise resolved)"
    );
    // Step 1 sees format applied (x->y) but NOT yet the rename; step 2 sees both.
    assert_eq!(
        exec_lua(&rpc, "return _G.chain[1]").await.as_str(),
        Some("foo|y"),
        "the first continuation sees the format applied and the rename not yet"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.chain[2]").await.as_str(),
        Some("bar|y"),
        "the second continuation sees the rename applied on top of the format"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["bar", "y"],
        "the buffer reflects both edits, applied in order"
    );
}

/// A superseded request settles its promise (resolve `nil`) rather than hanging:
/// two `references()` calls in one tick — the second bumps the generation, so the
/// first's promise resolves `nil` at supersede time while the second resolves with
/// the items.
#[tokio::test]
async fn superseded_request_settles_its_promise() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_supersede");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "references": [
                    {{ "uri": "{uri}", "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }} }}
                ]
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "foo\nbar\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.sup_a = false          -- first (superseded) request's promise
        _G.sup_b = "unset"        -- second (live) request's promise
        nx.lsp.references():next(function() _G.sup_a = true end)
        nx.lsp.references():next(function(items) _G.sup_b = items end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.sup_a)", "true").await,
        "the superseded request's promise should still settle (resolve nil)"
    );
    assert!(
        await_lua_eq(
            &rpc,
            "tostring((type(_G.sup_b) == 'table') and #_G.sup_b or -1)",
            "1"
        )
        .await,
        "the live request's promise should resolve with the item list"
    );
}

/// A verb issued with no language server attached settles its promise (resolve
/// `nil`) instead of hanging forever — so a chain built on it still proceeds.
#[tokio::test]
async fn verb_with_no_server_resolves_nil() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_noserver");
    // No mock, no `nx.lsp.enable` — the buffer has no server attached.
    std::env::remove_var("NXVIM_LSP_CMD");
    let file_path = dir.join("a.txt");
    std::fs::write(&file_path, "hello\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    exec_lua(
        &rpc,
        r#"
        _G.ns_done = false
        _G.ns_res = "unset"
        nx.lsp.format():next(function(res)
            _G.ns_res = res
            _G.ns_done = true
        end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.ns_done)", "true").await,
        "the promise should settle even with no server attached"
    );
    assert!(
        exec_lua(&rpc, "return _G.ns_res == nil").await.as_bool() == Some(true),
        "with no server the verb resolves nil"
    );
}

// ===== Phase 2: async `code_action` — resolves after the picked edit applies =====
// docs/plans/2026-07-23-async-lsp-verbs.md. Unlike the other verbs the reply only
// opens the chooser; the promise settles on the user's pick + apply (or nil on cancel).

/// The headline: `code_action():next(-> format())` chains — the format runs only
/// after the chosen code action's edit applies, and the final `:next` only after the
/// format lands. Proves the code-action promise settles post-apply (chainable), the
/// "organize imports then format" pattern.
#[tokio::test]
async fn code_action_chains_into_format() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_ca_chain");
    let uri = file_uri(&dir, "a.rs");
    // code action rewrites line 0 (foo -> bar, eager edit); format rewrites line 1
    // (x -> y). Different lines so each edit's range stays valid at apply time.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "code_action": [
                    {{
                        "title": "Rename foo to bar",
                        "edit": {{ "changes": {{ "{uri}": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }}, "newText": "bar" }}
                        ] }} }}
                    }}
                ],
                "formatting": [
                    {{ "range": {{ "start": {{ "line": 1, "character": 0 }}, "end": {{ "line": 1, "character": 1 }} }}, "newText": "y" }}
                ]
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "foo\nx\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // Issue once, chaining format after the code action. The chooser opens; picking
    // the row applies the edit and settles the promise, running the chain.
    exec_lua(
        &rpc,
        r#"
        _G.chain2 = {}
        local function snap()
            return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "|")
        end
        nx.lsp.code_action():next(function()
            table.insert(_G.chain2, snap())     -- after the code action, before format
            return nx.lsp.format()
        end):next(function()
            table.insert(_G.chain2, snap())     -- after format
        end)
        "#,
    )
    .await;
    // Wait for the chooser to list the action, then pick it (noselect ⇒ highlight first).
    let listed = poll_menu_items(&rpc, &mut incoming)
        .await
        .is_some_and(|rows| rows.iter().any(|r| r.contains("Rename foo to bar")));
    assert!(listed, "the code action should appear in the chooser");
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");

    assert!(
        await_lua_eq(&rpc, "tostring(#_G.chain2)", "2").await,
        "both continuations should run (format ran after the code action's promise resolved)"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.chain2[1]").await.as_str(),
        Some("bar|x"),
        "the first continuation sees the code action applied, format not yet"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.chain2[2]").await.as_str(),
        Some("bar|y"),
        "the second continuation sees the format applied on top of the code action"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["bar", "y"],
        "the buffer reflects both edits, applied in order"
    );
}

/// A LAZY code action (no eager edit, resolved via `codeAction/resolve`) settles its
/// promise only after the resolve round-trip lands and applies the edit.
#[tokio::test]
async fn code_action_lazy_resolve_settles_after_the_roundtrip() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_ca_lazy");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "code_action": [
                    {{ "title": "Lazy fix", "data": {{ "id": 1 }} }}
                ],
                "code_action_resolve": {{
                    "title": "Lazy fix",
                    "edit": {{ "changes": {{ "{uri}": [
                        {{ "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }}, "newText": "bar" }}
                    ] }} }}
                }}
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.lazy_seen = nil
        _G.lazy_done = false
        nx.lsp.code_action():next(function()
            _G.lazy_seen = (vim.api.nvim_buf_get_lines(0, 0, 1, false))[1]
            _G.lazy_done = true
        end)
        "#,
    )
    .await;
    let listed = poll_menu_items(&rpc, &mut incoming)
        .await
        .is_some_and(|rows| rows.iter().any(|r| r.contains("Lazy fix")));
    assert!(listed, "the lazy code action should appear in the chooser");
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");

    assert!(
        await_lua_eq(&rpc, "tostring(_G.lazy_done)", "true").await,
        "the lazy code action's promise should settle after the resolve round-trip"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.lazy_seen").await.as_str(),
        Some("let bar = 1"),
        "the continuation runs after the resolved edit applied"
    );
}

/// Cancelling the chooser (Esc) settles the promise with `nil` — a chain built on it
/// still proceeds rather than hanging.
#[tokio::test]
async fn code_action_cancel_resolves_nil() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_async_ca_cancel");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "code_action": [
                    {{
                        "title": "Replace with bar",
                        "edit": {{ "changes": {{ "{uri}": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 4 }}, "end": {{ "line": 0, "character": 7 }} }}, "newText": "bar" }}
                        ] }} }}
                    }}
                ]
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.cancel_done = false
        _G.cancel_res = "unset"
        nx.lsp.code_action():next(function(res)
            _G.cancel_res = res
            _G.cancel_done = true
        end)
        "#,
    )
    .await;
    let listed = poll_menu_items(&rpc, &mut incoming)
        .await
        .is_some_and(|rows| rows.iter().any(|r| r.contains("Replace with bar")));
    assert!(listed, "the code action should appear in the chooser");
    // Cancel instead of confirming.
    feed(&rpc, "<Esc>");

    assert!(
        await_lua_eq(&rpc, "tostring(_G.cancel_done)", "true").await,
        "cancelling the chooser should still settle the promise"
    );
    assert!(
        exec_lua(&rpc, "return _G.cancel_res == nil")
            .await
            .as_bool()
            == Some(true),
        "a cancelled code action resolves nil"
    );
    // And no edit was applied.
    assert_eq!(
        lines(&rpc).await.first().map(String::as_str),
        Some("let foo = 1"),
        "cancelling applies no edit"
    );
}

// ===== `nx.lsp.code_action(opts)` — kind filter + one-shot apply =====
// docs/plans/2026-07-23-code-action-filter-apply.md. `context.only` narrows which
// actions are offered (sent to the server AND re-applied to the reply); `apply` skips
// the chooser when exactly ONE action survives — the difference between a one-shot
// action and one with options.

/// A two-action mock script: a `source.fixAll.mock` action rewriting line 0 and an
/// unrelated `quickfix` action rewriting line 1, so which one ran is visible in the
/// buffer. `extra` splices in further script fields.
fn two_kind_mock(uri: &str, extra: &str) -> String {
    format!(
        r#"{{
            {extra}
            "code_action": [
                {{
                    "title": "Fix all mock issues",
                    "kind": "source.fixAll.mock",
                    "edit": {{ "changes": {{ "{uri}": [
                        {{ "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }}, "newText": "FIXED" }}
                    ] }} }}
                }},
                {{
                    "title": "Quick fix this line",
                    "kind": "quickfix",
                    "edit": {{ "changes": {{ "{uri}": [
                        {{ "range": {{ "start": {{ "line": 1, "character": 0 }}, "end": {{ "line": 1, "character": 3 }} }}, "newText": "QUICK" }}
                    ] }} }}
                }}
            ]
        }}"#
    )
}

/// `context.only` goes out **on the wire** as the request's `context.only`, not just as
/// a client-side filter: the mock echoes back what it received as the action's title.
#[tokio::test]
async fn code_action_only_is_sent_to_the_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_wire");
    arm_mock(&dir, r#"{ "code_action_echo_only": true }"#);
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    // No `apply`, so the chooser opens and shows the echoed title.
    exec_lua(
        &rpc,
        r#"nx.lsp.code_action({ context = { only = { "source.fixAll", "quickfix" } } })"#,
    )
    .await;
    let rows = poll_menu_items(&rpc, &mut incoming)
        .await
        .expect("the echoed action should open the chooser");
    assert!(
        rows.iter()
            .any(|r| r.contains("only=[source.fixAll,quickfix]")),
        "the server should have received context.only verbatim, got {rows:?}"
    );
    feed(&rpc, "<Esc>");
}

/// The headline: `{ context = { only = … }, apply = true }` with a single surviving
/// action applies it **with no chooser** — nothing is typed, no menu is answered, and
/// only the filtered action's edit lands. This is what makes `code_action` usable as a
/// save action. Here the mock is compliant (it honors `context.only` and drops the
/// `quickfix` action itself), and `source.fixAll` matches the hierarchy below it
/// (`source.fixAll.mock`).
#[tokio::test]
async fn code_action_only_and_apply_is_a_one_shot_with_no_chooser() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_oneshot");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(&dir, &two_kind_mock(&uri, ""));
    let (rpc, _incoming) = open_with_server(&dir, "aaa\nbbb\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.shot_done = false
        nx.lsp.code_action({ context = { only = { "source.fixAll" } }, apply = true })
            :next(function() _G.shot_done = true end)
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "tostring(_G.shot_done)", "true").await,
        "the one-shot promise settles without any pick"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["FIXED", "bbb"],
        "only the filtered action applied, and it applied without the chooser"
    );
}

/// The counterpart: when the filter leaves **more than one** action there is a real
/// choice to make, so `apply` still opens the chooser rather than guessing. Both
/// matching actions are listed; picking one applies it.
#[tokio::test]
async fn code_action_apply_with_several_matches_still_opens_the_chooser() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_multi");
    let uri = file_uri(&dir, "a.rs");
    // Both actions are under `source.fixAll`, so the filter can't narrow to one.
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "code_action": [
                    {{
                        "title": "Fix all with mock",
                        "kind": "source.fixAll.mock",
                        "edit": {{ "changes": {{ "{uri}": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }}, "newText": "FIRST" }}
                        ] }} }}
                    }},
                    {{
                        "title": "Fix all with other",
                        "kind": "source.fixAll.other",
                        "edit": {{ "changes": {{ "{uri}": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }}, "newText": "SECOND" }}
                        ] }} }}
                    }}
                ]
            }}"#
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.multi_done = false
        nx.lsp.code_action({ context = { only = { "source.fixAll" } }, apply = true })
            :next(function() _G.multi_done = true end)
        "#,
    )
    .await;

    let rows = poll_menu_items(&rpc, &mut incoming).await;
    let rows = rows.expect("two matching actions must still open the chooser");
    assert!(
        rows.iter().any(|r| r.contains("Fix all with mock"))
            && rows.iter().any(|r| r.contains("Fix all with other")),
        "both surviving actions are offered, got {rows:?}"
    );
    // The chooser opens noselect, so highlight then confirm.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");
    assert!(
        await_lua_eq(&rpc, "tostring(_G.multi_done)", "true").await,
        "picking an action settles the promise"
    );
    assert_eq!(
        lines(&rpc).await.first().map(String::as_str),
        Some("FIRST"),
        "the picked action applied"
    );
}

/// Honoring `context.only` is a protocol *should*, so the editor re-applies the filter
/// to the reply. With a server that returns everything regardless
/// (`code_action_ignore_only`), the one-shot still applies the right action — the
/// client-side filter carries it, including the `source.fixAll` → `source.fixAll.mock`
/// hierarchy match.
#[tokio::test]
async fn code_action_only_is_enforced_client_side_when_the_server_ignores_it() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_noncompliant");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &two_kind_mock(&uri, r#""code_action_ignore_only": true,"#),
    );
    let (rpc, _incoming) = open_with_server(&dir, "aaa\nbbb\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.ncf_done = false
        nx.lsp.code_action({ context = { only = { "source.fixAll" } }, apply = true })
            :next(function() _G.ncf_done = true end)
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "tostring(_G.ncf_done)", "true").await,
        "the one-shot settles even though the server returned both actions"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["FIXED", "bbb"],
        "the editor's own filter dropped the unmatched `quickfix` action, leaving one"
    );
}

/// A filter nothing matches is the existing empty-reply path: a message, no edit, and
/// the promise resolves `nil` (so a save chain proceeds rather than hanging).
#[tokio::test]
async fn code_action_only_with_no_match_resolves_nil() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_nomatch");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &two_kind_mock(&uri, r#""code_action_ignore_only": true,"#),
    );
    let (rpc, _incoming) = open_with_server(&dir, "aaa\nbbb\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.nomatch_done = false
        _G.nomatch_res = "unset"
        nx.lsp.code_action({ context = { only = { "source.organizeImports" } }, apply = true })
            :next(function(res)
                _G.nomatch_res = res
                _G.nomatch_done = true
            end)
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "tostring(_G.nomatch_done)", "true").await,
        "a filter with no match still settles the promise"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.nomatch_res == nil")
            .await
            .as_bool(),
        Some(true),
        "no matching action resolves nil"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["aaa", "bbb"],
        "no action matched, so nothing was applied"
    );
}

/// The one-shot chains like any other verb: `code_action(...):next(format)` — the
/// canonical fixAll-then-format save chain, with no interaction anywhere in it.
#[tokio::test]
async fn code_action_one_shot_chains_into_format() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_shot_chain");
    let uri = file_uri(&dir, "a.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "code_action": [
                    {{
                        "title": "Fix all mock issues",
                        "kind": "source.fixAll.mock",
                        "edit": {{ "changes": {{ "{uri}": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 0 }}, "end": {{ "line": 0, "character": 3 }} }}, "newText": "FIXED" }}
                        ] }} }}
                    }}
                ],
                "formatting": [
                    {{ "range": {{ "start": {{ "line": 1, "character": 0 }}, "end": {{ "line": 1, "character": 3 }} }}, "newText": "FMT" }}
                ]
            }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "aaa\nbbb\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(
        &rpc,
        r#"
        _G.shot_chain = {}
        local function snap()
            return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "|")
        end
        nx.lsp.code_action({ context = { only = { "source.fixAll" } }, apply = true })
            :next(function()
                table.insert(_G.shot_chain, snap())   -- fixAll applied, format not yet
                return nx.lsp.format()
            end):next(function()
                table.insert(_G.shot_chain, snap())   -- format applied on top
            end)
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "tostring(#_G.shot_chain)", "2").await,
        "both continuations run with no interaction at all"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.shot_chain[1]").await.as_str(),
        Some("FIXED|bbb"),
        "the first continuation sees the one-shot action applied"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["FIXED", "FMT"],
        "fixAll then format, in order"
    );
}

/// An option nxvim doesn't model fails LOUD rather than being silently dropped — a
/// quietly-ignored `filter` would apply the wrong action.
#[tokio::test]
async fn code_action_rejects_unsupported_options() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_badopts");
    arm_mock(&dir, "{}");
    let (rpc, _incoming) = open_with_server(&dir, "aaa\n").await;

    for (expr, want) in [
        (
            "nx.lsp.code_action({ filter = function() end })",
            "unsupported option 'filter'",
        ),
        (
            "nx.lsp.code_action({ only = { 'source.fixAll' } })",
            "unsupported option 'only'",
        ),
        (
            "nx.lsp.code_action({ context = { diagnostics = {} } })",
            "unsupported option 'context.diagnostics'",
        ),
        (
            "nx.lsp.code_action({ context = { only = 'source.fixAll' } })",
            "must be a list of kind strings",
        ),
        (
            "nx.lsp.code_action({ apply = 'yes' })",
            "opts.apply must be a boolean",
        ),
    ] {
        let got = exec_lua(
            &rpc,
            &format!("local ok, err = pcall(function() {expr} end) return tostring(ok) .. '|' .. tostring(err)"),
        )
        .await;
        let got = got.as_str().unwrap_or("");
        assert!(
            got.starts_with("false|") && got.contains(want),
            "`{expr}` should fail loud with {want:?}, got {got:?}"
        );
    }
}

// ===== `code_action` over a RANGE — the visual selection / `:'<,'>` / `opts.range` =====
// A `textDocument/codeAction` request carries a range, and the range-scoped refactors
// (`refactor.extract`, `refactor.inline`) are the ones a server gates on a non-empty
// one. The request used to collapse to a point at the cursor; these read the range
// that actually went over the wire back off the chooser (`code_action_echo_range`).

/// Issue a code action with `setup` (keys fed first, then `lua` run) and return the
/// chooser's single echoed title — the `range=[…] diags=N` the mock saw. Dismisses
/// the chooser before returning so the next call starts clean.
async fn echoed_range(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
    lua: &str,
) -> String {
    if !keys.is_empty() {
        feed(rpc, keys);
    }
    exec_lua(rpc, lua).await;
    let rows = poll_menu_items(rpc, incoming)
        .await
        .expect("the echoed action should open the chooser");
    feed(rpc, "<Esc>");
    barrier(rpc).await;
    // Flush the frames this call queued (including its own menu) so a following
    // `poll_menu_items` can't read *this* chooser back as the next one's.
    drain_to_latest_redraw(incoming, |_| true);
    rows.first().cloned().unwrap_or_default()
}

/// The headline: a **charwise Visual selection** rides the request as its range.
/// `v` at `(0,0)` down-and-right to `(1,1)` sends `(0,0)..(1,2)` — end-exclusive, so
/// the character under the cursor is included, exactly like the selection paints.
#[tokio::test]
async fn code_action_sends_the_visual_selection_range() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_vrange");
    arm_mock(&dir, r#"{ "code_action_echo_range": true }"#);
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\nccc\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let title = echoed_range(&rpc, &mut incoming, "ggvjl", "nx.lsp.code_action()").await;
    assert!(
        title.contains("range=[0,0-1,2]"),
        "the visual selection should ride the request, got {title:?}"
    );
    // The selection is consumed: the editor is back in Normal (vim leaves Visual on a
    // command that acts on the selection), with the `'<` / `'>` marks stamped.
    assert_eq!(
        exec_lua(&rpc, "return nx.mode().mode").await.as_str(),
        Some("n"),
        "issuing the action should leave Visual"
    );
}

/// A **linewise** (`V`) selection spans whole lines: `V j` over lines 0-1 sends
/// `(0,0)..(2,0)` — the start of the line after the last selected one, the linewise
/// range a server expects for a "extract these lines" refactor.
#[tokio::test]
async fn code_action_visual_line_selection_spans_whole_lines() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_vlrange");
    arm_mock(&dir, r#"{ "code_action_echo_range": true }"#);
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\nccc\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let title = echoed_range(&rpc, &mut incoming, "ggVj", "nx.lsp.code_action()").await;
    assert!(
        title.contains("range=[0,0-2,0]"),
        "a linewise selection should span whole lines, got {title:?}"
    );
}

/// `:'<,'>LspCodeAction` — the ex surface takes the addressed **line** range (vim's
/// `:` model: an address is a line, not a column), so the whole of lines 0-1 goes out.
/// Typing `:` from Visual prefills `'<,'>`, which is how this is reached in practice.
#[tokio::test]
async fn code_action_ex_range_uses_the_addressed_lines() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_exrange");
    arm_mock(&dir, r#"{ "code_action_echo_range": true }"#);
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\nccc\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    feed(&rpc, "ggVj:LspCodeAction<CR>");
    let rows = poll_menu_items(&rpc, &mut incoming)
        .await
        .expect("the echoed action should open the chooser");
    let title = rows.first().cloned().unwrap_or_default();
    feed(&rpc, "<Esc>");
    assert!(
        title.contains("range=[0,0-1,3]"),
        "`:'<,'>LspCodeAction` should send the addressed lines, got {title:?}"
    );
}

/// A bare `:LspCodeAction` (no address) is still a point at the cursor — an ex range
/// only applies when one was actually given.
#[tokio::test]
async fn code_action_without_a_range_is_a_point_at_the_cursor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_point");
    arm_mock(&dir, r#"{ "code_action_echo_range": true }"#);
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\nccc\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let title = echoed_range(&rpc, &mut incoming, "ggjll", "nx.lsp.code_action()").await;
    assert!(
        title.contains("range=[1,2-1,2]"),
        "with no selection the request is a point at the cursor, got {title:?}"
    );
}

/// `opts.range` is the explicit, non-interactive form — 0-based rows / byte columns,
/// end-exclusive (the `nx.win.select_range` convention) — and it wins over both the
/// cursor and any live selection.
#[tokio::test]
async fn code_action_explicit_range_wins_over_the_selection() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_optrange");
    arm_mock(&dir, r#"{ "code_action_echo_range": true }"#);
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\nccc\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    let title = echoed_range(
        &rpc,
        &mut incoming,
        "ggvl",
        "nx.lsp.code_action({ range = { start_row = 1, start_col = 1, end_row = 2, end_col = 2 } })",
    )
    .await;
    assert!(
        title.contains("range=[1,1-2,2]"),
        "opts.range should win over the live selection, got {title:?}"
    );
}

/// `context.diagnostics` is range-aware too: a selection covering a diagnostic on
/// another line carries it, where a point at the cursor carries none. Without this a
/// quickfix action over a selection would be offered no diagnostic to fix.
#[tokio::test]
async fn code_action_range_carries_the_diagnostics_it_covers() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_rangediag");
    arm_mock(
        &dir,
        r#"{
            "code_action_echo_range": true,
            "diagnostics": [
                { "range": { "start": { "line": 1, "character": 0 },
                             "end":   { "line": 1, "character": 3 } },
                  "severity": 1, "message": "bad bbb" }
            ]
        }"#,
    );
    let (rpc, mut incoming) = open_with_server(&dir, "aaa\nbbb\nccc\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.diagnostic.get(0)", "1").await,
        "the mock's diagnostic should land"
    );

    let cursor_only = echoed_range(&rpc, &mut incoming, "gg0", "nx.lsp.code_action()").await;
    assert!(
        cursor_only.contains("diags=0"),
        "line 0 has no diagnostic under the cursor, got {cursor_only:?}"
    );
    // Selected *upward* (line 1 → line 0), so the cursor ends on the clean line and
    // only the selection's other end covers the diagnostic — the range is doing the
    // work here, not the cursor.
    let selected = echoed_range(&rpc, &mut incoming, "ggjVk", "nx.lsp.code_action()").await;
    assert!(
        selected.contains("diags=1"),
        "a selection covering line 1 should carry its diagnostic, got {selected:?}"
    );
}

/// A malformed `range` fails LOUD rather than being silently dropped or half-read —
/// a quietly-ignored range would send a point request and offer the wrong actions.
#[tokio::test]
async fn code_action_rejects_a_malformed_range() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_ca_badrange");
    arm_mock(&dir, "{}");
    let (rpc, _incoming) = open_with_server(&dir, "aaa\n").await;

    for (expr, want) in [
        (
            "nx.lsp.code_action({ range = { start_row = 0 } })",
            "opts.range must be a table",
        ),
        (
            "nx.lsp.code_action({ range = { 0, 0, 1, 1 } })",
            "opts.range must be a table",
        ),
        (
            "nx.lsp.code_action({ range = { start_row = 0, start_col = 0, end_row = 1, end_col = -1 } })",
            "opts.range must be a table",
        ),
        (
            "nx.lsp.code_action({ range = 'lines' })",
            "opts.range must be a table",
        ),
    ] {
        let got = exec_lua(
            &rpc,
            &format!("local ok, err = pcall(function() {expr} end) return tostring(ok) .. '|' .. tostring(err)"),
        )
        .await;
        let got = got.as_str().unwrap_or("");
        assert!(
            got.starts_with("false|") && got.contains(want),
            "`{expr}` should fail loud with {want:?}, got {got:?}"
        );
    }
}

// ----- didSave: exactly one per write, never on a reload ---------------------

/// Count the `textDocument/didSave` notifications the mock has recorded.
fn did_save_count(record: &Path) -> usize {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains("\"textDocument/didSave\""))
        .count()
}

/// Poll (with a sync barrier) until the record holds `want` didSave lines, so an
/// in-flight notification isn't miscounted; returns the settled count.
async fn await_did_saves(rpc: &Rpc, record: &Path, want: usize) -> usize {
    for _ in 0..80 {
        barrier(rpc).await;
        if did_save_count(record) >= want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    did_save_count(record)
}

/// `didSave` fires exactly when bytes reach disk: once per `:w` (a repeat `:w`
/// with no edit is still a write, as vim's `BufWritePost` fires per write) and
/// **never** for a reload. `:e!` replaces the `Buffer`, whose fresh `save_tick`
/// restarted at 0 — the sync's `!=` comparison then read the reload as a "save"
/// and fired a spurious `didSave` for a pure disk read (the load paths'
/// `save_tick = changedtick` assignment in `mark_clean` was the same
/// instability). `save_tick` is carried across reloads and bumped only by real
/// saves.
#[tokio::test]
async fn did_save_fires_per_write_and_never_on_reload() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_didsave");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, record.display()));
    let (rpc, _incoming) = open_with_server(&dir, "let balance = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    feed(&rpc, ":w<CR>");
    assert_eq!(await_did_saves(&rpc, &record, 1).await, 1, "first :w");

    // A repeat `:w` with no intervening edit is still a write.
    feed(&rpc, ":w<CR>");
    assert_eq!(await_did_saves(&rpc, &record, 2).await, 2, "repeat :w");

    // A reload is a read, not a save: the count must not move. Poll a few settled
    // barriers so a spurious notification would have landed before the assert.
    // (`:e!` needs the file name — the bare form is E32 — and the current-file
    // check is cwd-aware, so the absolute path reloads in place.)
    feed(&rpc, &format!(":e! {}<CR>", dir.join("a.rs").display()));
    for _ in 0..8 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        did_save_count(&record),
        2,
        ":e! must not fire a didSave — a reload is not a save"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

// ===== server→client `workspace/applyEdit` =====

/// Poll the mock's record file for the body it captured under `method`, up to a few
/// seconds. `None` if it never arrives (the caller asserts with its own message).
async fn await_recorded(record: &Path, method: &str) -> Option<serde_json::Value> {
    for _ in 0..200 {
        let content = std::fs::read_to_string(record).unwrap_or_default();
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("method").and_then(serde_json::Value::as_str) == Some(method) {
                return v.get("params").cloned();
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

/// The `workspace/applyEdit` script both tests below share: the *exact* shape gopls
/// sends for "Extract declarations to new file" — cut the declaration out of the
/// open file, `create` a sibling, then paste into it — plus the code action that
/// triggers it as a bare `command` (which is why the edit has to arrive as a
/// server→client request at all: the `executeCommand` reply carries nothing).
fn extract_to_new_file_mock(dir: &Path, record: &Path, tail: &str) -> String {
    format!(
        r#"{{
            "record": "{rec}",
            "code_action": [
                {{
                    "title": "Extract declarations to new file",
                    "kind": "refactor.extract",
                    "command": {{ "title": "Extract", "command": "mock.extract_to_new_file" }}
                }}
            ],
            "apply_edit": {{
                "documentChanges": [
                    {{
                        "textDocument": {{ "uri": "{a}", "version": 1 }},
                        "edits": [ {{
                            "range": {{ "start": {{ "line": 0, "character": 0 }},
                                        "end": {{ "line": 1, "character": 0 }} }},
                            "newText": ""
                        }} ]
                    }},
                    {{ "kind": "create", "uri": "{b}" }},
                    {{
                        "textDocument": {{ "uri": "{b}", "version": 0 }},
                        "edits": [ {{
                            "range": {{ "start": {{ "line": 0, "character": 0 }},
                                        "end": {{ "line": 0, "character": 0 }} }},
                            "newText": "fn helper() {{}}\n"
                        }} ]
                    }}{tail}
                ]
            }}
        }}"#,
        rec = record.display(),
        a = file_uri(dir, "a.rs"),
        b = file_uri(dir, "helper.rs"),
    )
}

/// The headline: a server→client `workspace/applyEdit` is applied — across an open
/// buffer *and* a file the `create` operation brings into existence — and answered
/// with the real `applied` flag.
///
/// This is the whole apply half of the protocol: a refactor delivered as a `command`
/// (gopls's `extract_to_new_file`, ts_ls's move-to-file) replies to
/// `workspace/executeCommand` with nothing and pushes its edit back as this request.
/// nxvim answered it with method-not-found, so every such refactor failed outright.
#[tokio::test]
async fn a_servers_apply_edit_creates_the_new_file_and_is_answered_applied() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &extract_to_new_file_mock(&dir, &record, ""));
    let (rpc, mut incoming) = open_with_server(&dir, "fn helper() {}\nfn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    // Work from the project directory, as a session editing it would — that is what
    // makes the created file's name relative (the assertion below). Restored at the end
    // so the process cwd this `:cd` moves doesn't leak into the next test.
    let original_cwd = std::env::current_dir().expect("cwd");
    feed(&rpc, &format!(":cd {}<CR>", dir.display()));
    barrier(&rpc).await;

    // One action, `apply` ⇒ a one-shot: the command dispatches straight away and the
    // mock answers it with the applyEdit push.
    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    // The open buffer loses the extracted declaration.
    let mut cut = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        if lines(&rpc).await == vec!["fn main() {}".to_string()] {
            cut = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(cut, "the edit for the open document should apply");

    // The `create`d file appears on disk — and **empty**. That is the whole of what the
    // resource operation asks for: the file exists, and the extracted text the edits put
    // in its buffer is unsaved, exactly like every other change in a workspace edit
    // (neovim's model).
    let created = dir.join("helper.rs");
    let mut exists = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        exists = created.exists();
        if exists {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(exists, "the `create` must put the file on disk");
    assert_eq!(
        std::fs::read_to_string(&created).unwrap_or_default(),
        "",
        "…empty: a `create` creates the file, it does not save the content for you"
    );

    // …and its buffer holds the extracted text, **modified** (that is the unsaved part),
    // named the way `:e` would have named it: relative to the cwd, not the absolute path
    // the server sent.
    feed(&rpc, ":e helper.rs<CR>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn helper() {}".to_string()],
        "the created file's buffer should hold the pasted declaration"
    );
    assert_eq!(
        exec_lua(&rpc, "return tostring(nx.bo[0].modified)")
            .await
            .as_str(),
        Some("true"),
        "the content is yours to save — the buffer must be left modified"
    );
    assert_eq!(
        exec_lua(&rpc, "return tostring(nx.buf.name(0))")
            .await
            .as_str(),
        Some("helper.rs"),
        "the created buffer's name should be cwd-relative, like every other buffer's"
    );

    // Saving it is an ordinary `:w` — no "file changed on disk" complaint about the empty
    // placeholder nxvim itself wrote (the disk baseline was re-snapshotted for exactly
    // that reason), and the content lands.
    feed(&rpc, ":w<CR>");
    barrier(&rpc).await;
    let mut saved = String::new();
    for _ in 0..80 {
        barrier(&rpc).await;
        saved = std::fs::read_to_string(&created).unwrap_or_default();
        if !saved.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        saved, "fn helper() {}\n",
        "a plain `:w` on the created buffer must write the extracted text"
    );
    // …and nxvim's own placeholder write never came back as somebody else's change: a
    // `:checktime` over the buffer must find nothing to report (its disk baseline was
    // re-snapshotted when the placeholder landed), so no W11/W12/E211.
    feed(&rpc, ":checktime<CR>");
    barrier(&rpc).await;
    let msg = drain_to_latest_redraw(&mut incoming, |_| true)
        .map(|m| message(&m))
        .unwrap_or_default();
    assert!(
        !msg.contains("W12") && !msg.contains("W11") && !msg.contains("E211"),
        "nxvim's own placeholder write must not be reported as an external change: {msg:?}"
    );

    // And the server was told it landed — the response the whole round trip hangs on.
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the server must be answered `applied: true`, got {answer:?}"
    );

    // The capability that makes a server willing to send it in the first place.
    let init = await_recorded(&record, "initialize")
        .await
        .unwrap_or_default();
    assert_eq!(
        init.pointer("/capabilities/workspace/applyEdit")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "we must advertise workspace.applyEdit"
    );
    let ops = init
        .pointer("/capabilities/workspace/workspaceEdit/resourceOperations")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        ops.iter().any(|v| v.as_str() == Some("create")),
        "we must advertise the `create` resource operation, got {ops:?}"
    );
    std::env::set_current_dir(original_cwd).expect("restore cwd");
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The `rename` resource operation moves the **real file** and the open buffer
/// follows it: same buffer, same content, new name. Both halves matter — a rename
/// that only renamed the buffer would leave the old file on disk (and the new one
/// never written), and one that only moved the file would leave the editor holding a
/// window onto a path that no longer exists.
#[tokio::test]
async fn a_rename_op_moves_the_file_and_the_buffer_follows() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_rename");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Move file", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.move" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "rename", "oldUri": "{a}", "newUri": "{b}" }}
                ] }}
            }}"#,
            rec = record.display(),
            a = file_uri(&dir, "a.rs"),
            b = file_uri(&dir, "moved.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(
        &dir,
        "fn main() {}
",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let moved = dir.join("moved.rs");
    let mut on_disk = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        if moved.exists() && !dir.join("a.rs").exists() {
            on_disk = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(on_disk, "the file itself must move on disk");
    assert_eq!(
        std::fs::read_to_string(&moved).unwrap_or_default(),
        "fn main() {}\n",
        "…carrying its contents, not a fresh empty file"
    );

    // The buffer is the same buffer — its name followed the file, and its text is
    // still there (a re-open would have been a different buffer, and a wipe would
    // have lost the unsaved state a real refactor may leave).
    let name = await_lua_contains(&rpc, "nx.buf.name(0)", "moved.rs").await;
    assert!(
        name.contains("moved.rs"),
        "the buffer name should follow the file, got {name:?}"
    );
    assert_eq!(lines(&rpc).await, vec!["fn main() {}".to_string()]);

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the response must wait for the move and report it, got {answer:?}"
    );

    // The server follows the move too: the moved buffer is a *different document*, so
    // the old URI closes and the new one opens — on the same, still-attached server.
    // (Dropping the buffer's LSP state instead would leave it silently server-less:
    // nothing re-attaches it, since `FileType` doesn't fire again when only the stem
    // changed. The browser leg caught exactly that.)
    let opened = std::fs::read_to_string(&record).unwrap_or_default();
    assert!(
        opened
            .lines()
            .any(|l| l.contains("didClose") && l.contains("/a.rs")),
        "the old document must close"
    );
    assert!(
        opened
            .lines()
            .any(|l| l.contains("didOpen") && l.contains("moved.rs")),
        "…and the new one open, on the same server"
    );
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the buffer must still be attached after the move"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The `delete` resource operation removes the **real file** and wipes its buffer —
/// a window onto a deleted file would let a later `:w` recreate what the server asked
/// to remove.
#[tokio::test]
async fn a_delete_op_removes_the_file_and_wipes_its_buffer() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_delete");
    let record = dir.join("rec.jsonl");
    let doomed = dir.join("doomed.rs");
    std::fs::write(&doomed, "fn doomed() {}\n").expect("write doomed file");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Remove file", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.remove" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "delete", "uri": "{d}" }}
                ] }}
            }}"#,
            rec = record.display(),
            d = file_uri(&dir, "doomed.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(
        &dir,
        "fn main() {}
",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    // Open the doomed file too, so the wipe has a buffer to act on.
    feed(&rpc, &format!(":e {}<CR>", doomed.display()));
    feed(&rpc, ":e #<CR>");
    barrier(&rpc).await;
    assert!(
        await_lua_contains(&rpc, "#nx.buf.list()", "2")
            .await
            .contains('2'),
        "both files should be open before the delete"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let mut gone = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        if !doomed.exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gone, "the file itself must be removed from disk");
    let buffers = await_lua_contains(&rpc, "#nx.buf.list()", "1").await;
    assert_eq!(
        buffers, "1",
        "the deleted file's buffer must be wiped, got {buffers:?} buffer(s)"
    );

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the response must wait for the delete and report it, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A file operation that *fails* is reported **loud** — the server is told
/// `applied: false` with the reason, and the user sees it. The alternative (answering
/// "applied" because the edit was dispatched) is what makes a half-done refactor look
/// like it worked.
#[tokio::test]
async fn a_failing_file_operation_is_reported_with_a_reason() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_failure");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Remove file", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.remove" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "delete", "uri": "{d}" }}
                ] }}
            }}"#,
            rec = record.display(),
            // Never created: the delete must fail rather than quietly "succeed".
            d = file_uri(&dir, "absent.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(
        &dir,
        "fn main() {}
",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "a failed file operation must not report success, got {answer:?}"
    );
    let reason = answer
        .as_ref()
        .and_then(|v| v.get("failureReason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        reason.contains("delete") && reason.contains("absent.rs"),
        "the reason must name the operation that failed, got {reason:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// `documentChanges` is a **sequence**: two file operations on the same file only make
/// sense applied in order (`a.rs → b.rs`, then `b.rs → c.rs`). Queued concurrently — one
/// `tokio::spawn` each — the second races the first and renames a file that isn't there
/// yet, so the refactor half-lands.
///
/// The first operation carries `ignoreIfExists`, which costs it an extra round trip (the
/// destination probe), so the race is decided rather than merely likely — the same
/// widening a daemon session's link latency does to *every* operation.
#[tokio::test]
async fn chained_file_operations_apply_in_order() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_order");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Move twice", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.move2" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "rename", "oldUri": "{a}", "newUri": "{b}",
                       "options": {{ "ignoreIfExists": true }} }},
                    {{ "kind": "rename", "oldUri": "{b}", "newUri": "{c}" }}
                ] }}
            }}"#,
            rec = record.display(),
            a = file_uri(&dir, "a.rs"),
            b = file_uri(&dir, "b.rs"),
            c = file_uri(&dir, "c.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        std::fs::read_to_string(dir.join("c.rs")).unwrap_or_default(),
        "fn main() {}\n",
        "the second rename must run on the first one's result"
    );
    assert!(
        !dir.join("a.rs").exists() && !dir.join("b.rs").exists(),
        "neither intermediate name should be left behind"
    );
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "…and both operations report as applied, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// `failureHandling: abort` is a promise, so it has to hold: when a change fails, the
/// ones *after* it don't run, and the server is told which one broke. Here the delete
/// fails (no such file) and the rename that follows must never happen — the alternative
/// is a refactor that half-lands while the server thinks it succeeded.
#[tokio::test]
async fn a_failed_change_aborts_the_ones_after_it() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_abort");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Clean up", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.cleanup" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "delete", "uri": "{absent}" }},
                    {{ "kind": "rename", "oldUri": "{a}", "newUri": "{b}" }}
                ] }}
            }}"#,
            rec = record.display(),
            absent = file_uri(&dir, "absent.rs"),
            a = file_uri(&dir, "a.rs"),
            b = file_uri(&dir, "moved.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert!(
        dir.join("a.rs").exists() && !dir.join("moved.rs").exists(),
        "the rename after the failing delete must not run"
    );
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "the edit did not apply, got {answer:?}"
    );
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("failedChange"))
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "…and the server is told which change broke, got {answer:?}"
    );
}

/// A document the edit can't resolve aborts it **before anything is applied** — the
/// text edits are staged against their buffers first, so an edit naming one file we can
/// open and one we can't leaves the openable one untouched rather than half-refactored.
#[tokio::test]
async fn an_unresolvable_document_aborts_before_any_edit_applies() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_preflight");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Rewrite", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.rewrite" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{
                        "textDocument": {{ "uri": "{a}", "version": 1 }},
                        "edits": [ {{
                            "range": {{ "start": {{ "line": 0, "character": 3 }},
                                        "end": {{ "line": 0, "character": 7 }} }},
                            "newText": "renamed"
                        }} ]
                    }},
                    {{
                        "textDocument": {{ "uri": "jdt://contents/Foo.class", "version": 1 }},
                        "edits": [ {{
                            "range": {{ "start": {{ "line": 0, "character": 0 }},
                                        "end": {{ "line": 0, "character": 0 }} }},
                            "newText": "x"
                        }} ]
                    }}
                ] }}
            }}"#,
            rec = record.display(),
            a = file_uri(&dir, "a.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string()],
        "the resolvable document must be left untouched when a later one can't resolve"
    );
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("failedChange"))
            .and_then(serde_json::Value::as_u64),
        Some(1),
        "the server is told which change could not be resolved, got {answer:?}"
    );
}

/// A change may address a document by the name an *earlier* change gives it —
/// `rename a → b`, then edits to `b`. The rename moves real bytes, so it can only run
/// off the editor tick, *after* the text edits are staged; the edits therefore have to
/// rewind through it to reach the buffer that still holds the file.
///
/// Without that they resolve to nothing, open a fresh buffer for a file that doesn't
/// exist yet, and the rename then binds a **second** buffer to the same name — two
/// buffers called `moved.rs` and the edit silently lost.
#[tokio::test]
async fn edits_addressed_to_a_renamed_file_reach_the_renamed_buffer() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_rename_then_edit");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Move and rewrite", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.move_edit" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "rename", "oldUri": "{a}", "newUri": "{b}" }},
                    {{
                        "textDocument": {{ "uri": "{b}", "version": 1 }},
                        "edits": [ {{
                            "range": {{ "start": {{ "line": 0, "character": 3 }},
                                        "end": {{ "line": 0, "character": 7 }} }},
                            "newText": "renamed"
                        }} ]
                    }}
                ] }}
            }}"#,
            rec = record.display(),
            a = file_uri(&dir, "a.rs"),
            b = file_uri(&dir, "moved.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "both changes should apply, got {answer:?}"
    );

    // One buffer, holding the edit, under the new name.
    assert!(
        await_lua_eq(&rpc, "#nx.buf.list()", "1").await,
        "the rename must not strand a second buffer for the same file: {:?}",
        exec_lua(
            &rpc,
            "local t = {} for _, b in ipairs(nx.buf.list()) do \
             t[#t + 1] = tostring(nx.buf.name(b)) end return table.concat(t, ', ')"
        )
        .await
    );
    let name = await_lua_contains(&rpc, "nx.buf.name(0)", "moved.rs").await;
    assert!(
        name.contains("moved.rs"),
        "the buffer name should follow the file, got {name:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["fn renamed() {}".to_string()],
        "the edit addressed by the new name must land in the buffer being renamed"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `create` whose target is **already open** with content. The buffer is emptied
/// first, as one undoable edit, so the create is a create rather than a silent no-op on
/// stale content — and an emptied buffer holds an *empty document*, which is exactly
/// what `'endofline'` has to say about it.
///
/// If the flag stayed on from the file that was read, the rope's phantom `\n` would
/// still count as the document's own final newline: the edits that fill the buffer would
/// no longer reach the document's end, so they would land *before* the phantom and leave
/// a spurious trailing blank line — the bug the pre-`'endofline'` `len_bytes() == 1`
/// special case used to paper over for exactly this case.
#[tokio::test]
async fn a_create_of_an_already_open_file_empties_its_document_first() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_create_open_buffer");
    let record = dir.join("rec.jsonl");
    // The `create` target exists, is terminated, and gets opened below — so the create
    // takes the empty-an-open-buffer branch rather than minting a fresh buffer.
    let target = dir.join("helper.rs");
    std::fs::write(&target, "fn stale() {}\n").expect("write the stale file");
    arm_mock(&dir, &extract_to_new_file_mock(&dir, &record, ""));
    let (rpc, _incoming) = open_with_server(&dir, "fn helper() {}\nfn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    feed(&rpc, &format!(":e {}<CR>", target.display()));
    barrier(&rpc).await;
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "true").await,
        "the stale file was read terminated"
    );
    feed(&rpc, &format!(":e {}<CR>", dir.join("a.rs").display()));
    barrier(&rpc).await;

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;
    let mut cut = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        if lines(&rpc).await == vec!["fn main() {}".to_string()] {
            cut = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(cut, "the edit for the open document should apply");

    feed(&rpc, &format!(":e {}<CR>", target.display()));
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn helper() {}".to_string()],
        "the created buffer holds the extracted text and nothing else — no blank line \
         left over from the emptied document's phantom newline"
    );
    // …and the document it now holds really is unterminated-then-terminated by that
    // text, so a `:w` reproduces it byte for byte.
    feed(&rpc, ":w<CR>");
    barrier(&rpc).await;
    let mut saved = String::new();
    for _ in 0..80 {
        barrier(&rpc).await;
        saved = std::fs::read_to_string(&target).unwrap_or_default();
        if !saved.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        saved, "fn helper() {}\n",
        "the saved file matches the edit, with no trailing blank line"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `create` may name a file in a directory that doesn't exist yet — a refactor that
/// extracts into a new module / package directory. `:w` refuses to create one (vim's
/// `E212`, rightly), so the created buffer has to be written *behind* a recursive
/// mkdir; otherwise the file silently never appears while the server is told the edit
/// applied.
#[tokio::test]
async fn a_create_into_a_missing_directory_makes_the_directory() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_create_subdir");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Extract to a new package", "kind": "refactor.extract",
                       "command": {{ "title": "Extract", "command": "mock.extract" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "create", "uri": "{b}" }},
                    {{
                        "textDocument": {{ "uri": "{b}", "version": 0 }},
                        "edits": [ {{
                            "range": {{ "start": {{ "line": 0, "character": 0 }},
                                        "end": {{ "line": 0, "character": 0 }} }},
                            "newText": "fn helper() {{}}\n"
                        }} ]
                    }}
                ] }}
            }}"#,
            rec = record.display(),
            b = file_uri(&dir, "sub/nested/helper.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    // The file has to *appear* — inside a directory that did not exist — which is what
    // fails when the `mkdir` isn't put in front of it. Empty, like every `create`: the
    // extracted text stays in the buffer for you to save.
    let created = dir.join("sub/nested/helper.rs");
    let mut exists = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        exists = created.exists();
        if exists {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        exists,
        "the created file's directory must be made for it, not assumed to exist"
    );
    assert_eq!(
        std::fs::read_to_string(&created).unwrap_or_default(),
        "",
        "…and the file itself is created empty, not written with the buffer's content"
    );
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "…and the server told it applied, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `workspace/applyEdit` is a **request**: the server is blocked until nxvim
/// answers, and the answer waits for the file operations the edit asked for. An fs
/// leg that stops answering (a daemon link that goes quiet rather than erroring)
/// would block that server forever — so a watchdog gives up on the stalled operation
/// and the server is told, truthfully, that the edit did not apply.
///
/// The stall is real, not simulated: the session's `nx.fs` job leg is a
/// [`RemoteFsJobs`] pointed at a duplex nobody serves, so the `delete` below is sent
/// and never answered.
#[tokio::test]
async fn a_stalled_file_operation_gives_up_instead_of_blocking_the_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_apply_edit_stall");
    let record = dir.join("rec.jsonl");
    let doomed = dir.join("doomed.rs");
    std::fs::write(&doomed, "fn doomed() {}\n").expect("write doomed file");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Remove file", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.remove" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "delete", "uri": "{d}" }}
                ] }}
            }}"#,
            rec = record.display(),
            d = file_uri(&dir, "doomed.rs"),
        ),
    );
    // Short enough to keep the test quick; the product default is 30s.
    // SAFETY: serialized on `serial_lock`, like every env write in this suite.
    std::env::set_var("NXVIM_WORKSPACE_FS_TIMEOUT_MS", "400");

    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "fn main() {}\n").expect("write test file");
    // The far end of the link is held open but never served: every fs job crosses and
    // waits forever, which is the failure mode the watchdog exists for.
    let (host_end, _never_served) = tokio::io::duplex(1 << 16);
    let (host_reader, host_writer) = tokio::io::split(host_end);
    let (rpc, _incoming) = spawn(ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        fs_jobs: Some(nxvim_server::RemoteFsJobs::connect(
            host_reader,
            host_writer,
        )),
        ..Default::default()
    });
    attach(&rpc, 80, 24).await;
    feed(&rpc, "gg0");
    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", { cmd = { "mock" }, filetypes = { "rust" } })
        nx.lsp.enable({ "mock" })
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "the server must be answered rather than left blocked, got {answer:?}"
    );
    let reason = answer
        .as_ref()
        .and_then(|v| v.get("failureReason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        reason.contains("ETIMEDOUT") && reason.contains("may still complete"),
        "the reason must say we gave up, and not claim the file is gone: {reason:?}"
    );
    assert!(
        doomed.exists(),
        "nothing answered the delete, so the file is still there — the point of the \
         hedged wording"
    );
    std::env::remove_var("NXVIM_WORKSPACE_FS_TIMEOUT_MS");
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A goto whose target lives in a file that **isn't open yet** lands on the target,
/// not at the top of the file. The read of a freshly-opened file is deferred (the
/// cursor set now is clamped to a still-empty buffer and re-landed when the bytes
/// arrive), so refining the column against that clamped line overwrote the pending
/// target with line 1 — the single most-used LSP feature, going to the wrong place
/// whenever the definition was in another file.
#[tokio::test]
async fn a_definition_in_an_unopened_file_lands_on_the_definition() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_def_crossfile");
    std::fs::write(dir.join("b.rs"), "one\ntwo\nthree\n").expect("write b.rs");
    let uri = file_uri(&dir, "b.rs");
    arm_mock(
        &dir,
        &format!(
            r#"{{ "definition": {{ "uri": "{uri}", "range": {{ "start": {{ "line": 2, "character": 1 }}, "end": {{ "line": 2, "character": 3 }} }} }} }}"#
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = bar()\nfoo()\nbar()\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.definition()").await;
    let name = await_lua_contains(&rpc, "nx.buf.name(0)", "b.rs").await;
    assert!(
        name.contains("b.rs"),
        "the jump should open b.rs, got {name:?}"
    );
    let mut landed = (0, 0);
    for _ in 0..80 {
        barrier(&rpc).await;
        landed = cursor(&rpc).await;
        if landed == (3, 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        landed,
        (3, 1),
        "the cursor must land on the definition's own line and column"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

// ===== `nx.lsp.apply_workspace_edit` / `nx.lsp.show_document` (the Lua entry) =====

/// Open `dir/a.rs` with no language server — the Lua entry points below apply an edit
/// a caller already holds, so nothing needs to be attached.
async fn open_plain(dir: &Path, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let (rpc, incoming) = spawn(ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    });
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// The whole `WorkspaceEdit` surface is reachable from Lua — `nx.lsp.apply_workspace_edit`
/// (and its `vim.lsp.util` alias), which a plugin or an `nx.lsp.commands` handler uses
/// for an edit a server handed it as command arguments. Resource operations included:
/// they were only ever reachable from a server reply before.
#[tokio::test]
async fn the_lua_entry_applies_resource_operations_too() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_lua_apply_resource_ops");
    std::fs::write(dir.join("old.rs"), "fn moved() {}\n").expect("write old.rs");
    std::fs::write(dir.join("doomed.rs"), "fn doomed() {}\n").expect("write doomed.rs");
    let (rpc, _incoming) = open_plain(&dir, "fn main() {}\n").await;

    let edit = format!(
        r#"nx.lsp.apply_workspace_edit({{ documentChanges = {{
            {{ kind = "create", uri = "{new}" }},
            {{ textDocument = {{ uri = "{new}", version = 0 }},
              edits = {{ {{ range = {{ start = {{ line = 0, character = 0 }},
                                       ["end"] = {{ line = 0, character = 0 }} }},
                           newText = "fn created() {{}}\n" }} }} }},
            {{ kind = "rename", oldUri = "{old}", newUri = "{moved}" }},
            {{ kind = "delete", uri = "{doomed}" }},
        }} }})"#,
        new = file_uri(&dir, "new.rs"),
        old = file_uri(&dir, "old.rs"),
        moved = file_uri(&dir, "moved.rs"),
        doomed = file_uri(&dir, "doomed.rs"),
    );
    exec_lua(&rpc, &edit).await;

    // The file operations settle off-tick, one at a time, so poll for the end state.
    let created = dir.join("new.rs");
    let moved = dir.join("moved.rs");
    let doomed = dir.join("doomed.rs");
    let mut settled = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        if created.exists() && moved.exists() && !doomed.exists() {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        settled,
        "create/rename/delete from Lua must all land (created={} moved={} doomed_gone={})",
        created.exists(),
        moved.exists(),
        !doomed.exists()
    );
    assert_eq!(
        std::fs::read_to_string(&created).unwrap_or_default(),
        "",
        "the created file is created empty — its content stays in the buffer, unsaved"
    );
    assert_eq!(
        std::fs::read_to_string(&moved).unwrap_or_default(),
        "fn moved() {}\n",
        "the renamed file carries its own bytes"
    );
    assert!(
        !dir.join("old.rs").exists(),
        "the rename must not leave the old name behind"
    );
}

/// A `create` carrying `ignoreIfExists` is two operations wearing one name, and both
/// halves have to be right **locally** as well as off-tick:
///
/// - over a file that **is** there, it is "open what is already there" — the edits land
///   on the real content and the file is not rewritten;
/// - over a file that turns out **not** to be there, it is an ordinary create — the
///   file lands on disk holding exactly what the edits put in it.
///
/// The second half reached its (empty) buffer through `ensure_buffer_loaded` rather
/// than `create_file_buffer`, so it missed the phantom-newline handling the plain
/// `create` gets: nxvim's rope always carries a trailing newline, every position in an
/// empty document maps to byte 0, and the fill therefore inserted *before* it — the
/// created file gained a spurious blank last line.
#[tokio::test]
async fn an_ignore_if_exists_create_spares_a_file_and_still_creates_an_absent_one() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_ignore_if_exists_local");
    std::fs::write(dir.join("keep.rs"), "fn keep() {}\n").expect("write keep.rs");
    let (rpc, _incoming) = open_plain(&dir, "fn main() {}\n").await;

    let edit = format!(
        r#"nx.lsp.apply_workspace_edit({{ documentChanges = {{
            {{ kind = "create", uri = "{keep}", options = {{ ignoreIfExists = true }} }},
            {{ textDocument = {{ uri = "{keep}", version = 0 }},
              edits = {{ {{ range = {{ start = {{ line = 1, character = 0 }},
                                       ["end"] = {{ line = 1, character = 0 }} }},
                           newText = "fn added() {{}}\n" }} }} }},
            {{ kind = "create", uri = "{fresh}", options = {{ ignoreIfExists = true }} }},
            {{ textDocument = {{ uri = "{fresh}", version = 0 }},
              edits = {{ {{ range = {{ start = {{ line = 0, character = 0 }},
                                       ["end"] = {{ line = 0, character = 0 }} }},
                           newText = "fn fresh() {{}}\n" }} }} }},
        }} }})"#,
        keep = file_uri(&dir, "keep.rs"),
        fresh = file_uri(&dir, "fresh.rs"),
    );
    exec_lua(&rpc, &edit).await;

    // The absent one is a create: the file appears on disk (empty — a `create` creates
    // the file, the content stays in the buffer). It goes out behind a recursive `mkdir`
    // on the off-tick fs seam, so poll for it.
    let fresh = dir.join("fresh.rs");
    let mut exists = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        exists = fresh.exists();
        if exists {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        exists,
        "an `ignoreIfExists` create over an absent file is a create: the file must appear"
    );
    assert_eq!(
        std::fs::read_to_string(&fresh).unwrap_or_default(),
        "",
        "…created empty, like every other `create`"
    );
    // And the buffer holds exactly the edit's text — no spurious blank last line from the
    // rope's phantom newline, which is the bug this half of the test was written for.
    feed(&rpc, &format!(":e {}<CR>", fresh.display()));
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn fresh() {}".to_string()],
        "the fill must consume the phantom newline, not insert before it"
    );

    // The one that was already there kept its own content, with the edit on top and
    // nothing written behind our back — a `create` we were told to spare is not a
    // create, so it stays an in-memory edit like every other.
    assert_eq!(
        std::fs::read_to_string(dir.join("keep.rs")).unwrap_or_default(),
        "fn keep() {}\n",
        "a spared file must not be rewritten by the edit that spared it"
    );
    feed(&rpc, &format!(":e {}<CR>", dir.join("keep.rs").display()));
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn keep() {}".to_string(), "fn added() {}".to_string()],
        "…and the edits after it land on its real content, not on an emptied buffer"
    );
}

/// The `vim.lsp.util` spelling exists and is the same verb — a neovim-shaped plugin
/// reaches for it, and it used to be `nil` (the example in `nx.lsp.commands`'s own
/// documentation called it), so every such call errored.
#[tokio::test]
async fn the_vim_lsp_util_aliases_are_the_same_verbs() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_lua_apply_alias");
    let (rpc, _incoming) = open_plain(&dir, "let foo = 1\n").await;

    let edit = format!(
        r#"vim.lsp.util.apply_workspace_edit({{ changes = {{
            ["{a}"] = {{ {{ range = {{ start = {{ line = 0, character = 4 }},
                                       ["end"] = {{ line = 0, character = 7 }} }},
                           newText = "bar" }} }},
        }} }}, "utf-16")"#,
        a = file_uri(&dir, "a.rs"),
    );
    exec_lua(&rpc, &edit).await;
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["let bar = 1".to_string()],
        "the alias must apply the edit, not fail on a nil table"
    );

    // …and the location jump, whose docs example is what named the missing table.
    std::fs::write(dir.join("b.rs"), "one\ntwo\nthree\n").expect("write b.rs");
    let jump = format!(
        r#"vim.lsp.util.show_document({{ uri = "{b}",
            range = {{ start = {{ line = 2, character = 1 }},
                       ["end"] = {{ line = 2, character = 1 }} }} }})"#,
        b = file_uri(&dir, "b.rs"),
    );
    exec_lua(&rpc, &jump).await;
    let name = await_lua_contains(&rpc, "nx.buf.name(0)", "b.rs").await;
    assert!(
        name.contains("b.rs"),
        "the jump should open b.rs, got {name:?}"
    );
    let mut landed = (0, 0);
    for _ in 0..80 {
        barrier(&rpc).await;
        landed = cursor(&rpc).await;
        if landed == (3, 1) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        landed,
        (3, 1),
        "…with the cursor on the location's own line and column"
    );
}

// ===== change annotations, and a `create` that names a directory =====

/// Poll the latest redraw until its `cmdline_prompt` contains `want` (the confirm
/// dialog nxvim opens for an annotation), returning what was seen either way.
async fn await_prompt(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, want: &str) -> String {
    let mut last = String::new();
    for _ in 0..200 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            last = map_get(&map, "cmdline_prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        if last.contains(want) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    last
}

/// The `changeAnnotations` script both tests below share: two groups over one
/// document — a "Rename" group that applies unconditionally and a "Comments" group the
/// server marks `needsConfirmation`, exactly the split annotations exist for.
fn annotated_edit_mock(dir: &Path, record: &Path) -> String {
    format!(
        r#"{{
            "record": "{rec}",
            "code_action": [
                {{ "title": "Rename symbol", "kind": "refactor",
                   "command": {{ "title": "run", "command": "mock.rename" }} }}
            ],
            "apply_edit": {{
                "changeAnnotations": {{
                    "rename": {{ "label": "Rename symbol" }},
                    "comments": {{ "label": "Update comments",
                                   "description": "occurrences in comments",
                                   "needsConfirmation": true }}
                }},
                "documentChanges": [
                    {{
                        "textDocument": {{ "uri": "{a}", "version": 1 }},
                        "edits": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 4 }},
                                           "end": {{ "line": 0, "character": 7 }} }},
                               "newText": "bar", "annotationId": "rename" }},
                            {{ "range": {{ "start": {{ "line": 1, "character": 3 }},
                                           "end": {{ "line": 1, "character": 6 }} }},
                               "newText": "bar", "annotationId": "comments" }}
                        ]
                    }}
                ]
            }}
        }}"#,
        rec = record.display(),
        a = file_uri(dir, "a.rs"),
    )
}

/// A change the server marked `needsConfirmation` does **not** apply until the user
/// says so — and the rest of the edit waits with it, because a server-initiated
/// `workspace/applyEdit` is answered with what actually happened. Saying yes applies
/// everything.
#[tokio::test]
async fn a_confirmed_annotation_applies_the_whole_edit() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_annotation_yes");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &annotated_edit_mock(&dir, &record));
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n// foo again\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    // The question is asked, and until it is answered NOTHING has applied — not even
    // the unannotated half.
    let asked = await_prompt(&rpc, &mut incoming, "Update comments").await;
    assert!(
        asked.contains("Update comments") && asked.contains("occurrences in comments"),
        "the confirm should name the annotation and its description, got {asked:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["let foo = 1".to_string(), "// foo again".to_string()],
        "nothing may apply while the user is still being asked"
    );

    feed(&rpc, "y");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["let bar = 1".to_string(), "// bar again".to_string()],
        "yes applies the confirmed group along with the rest"
    );
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "…and only then is the server told it applied, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// Declining takes only that annotation's changes with it: the unannotated (and
/// un-confirmable) half of the same edit still applies, which is the point of a server
/// splitting them.
#[tokio::test]
async fn a_declined_annotation_drops_only_its_own_changes() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_annotation_no");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &annotated_edit_mock(&dir, &record));
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n// foo again\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;
    await_prompt(&rpc, &mut incoming, "Update comments").await;
    feed(&rpc, "n");
    barrier(&rpc).await;

    let mut landed = Vec::new();
    for _ in 0..80 {
        barrier(&rpc).await;
        landed = lines(&rpc).await;
        if landed.first().map(String::as_str) == Some("let bar = 1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        landed,
        vec!["let bar = 1".to_string(), "// foo again".to_string()],
        "the declined group must be dropped and only it — the rename still applies"
    );
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the edit did apply, minus what was declined, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `create` whose URI ends in `/` names a **directory**: it is made (with its
/// parents) rather than opened as a file. Deliberately a directory *nothing else in
/// the edit touches — a file create brings its own `mkdir -p`, so a folder that only
/// exists because some file needed it would prove nothing about this.
#[tokio::test]
async fn a_create_of_a_directory_makes_the_directory() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_create_folder");
    let record = dir.join("rec.jsonl");
    // A URI with the trailing slash that says "directory".
    let folder_uri = file_uri(&dir, "assets/icons") + "/";
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Scaffold the package", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.scaffold" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "create", "uri": "{folder}" }}
                ] }}
            }}"#,
            rec = record.display(),
            folder = folder_uri,
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let made = dir.join("assets/icons");
    let mut is_dir = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        is_dir = made.is_dir();
        if is_dir {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        is_dir,
        "the `create` of a `/`-terminated URI must make a directory (and its parent), \
         got exists={} is_dir={}",
        made.exists(),
        made.is_dir()
    );
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `changeAnnotations` script where **every** change is `needsConfirmation`, so
/// declining leaves nothing to apply. The two tests below drive the two ways that can
/// happen — the user saying no, and nobody being able to ask.
fn all_annotated_edit_mock(dir: &Path, record: &Path) -> String {
    format!(
        r#"{{
            "record": "{rec}",
            "code_action": [
                {{ "title": "Rewrite", "kind": "refactor",
                   "command": {{ "title": "run", "command": "mock.rewrite" }} }}
            ],
            "apply_edit": {{
                "changeAnnotations": {{
                    "risky": {{ "label": "Update string literals",
                                "needsConfirmation": true }}
                }},
                "documentChanges": [
                    {{
                        "textDocument": {{ "uri": "{a}", "version": 1 }},
                        "edits": [
                            {{ "range": {{ "start": {{ "line": 0, "character": 4 }},
                                           "end": {{ "line": 0, "character": 7 }} }},
                               "newText": "bar", "annotationId": "risky" }}
                        ]
                    }}
                ]
            }}
        }}"#,
        rec = record.display(),
        a = file_uri(dir, "a.rs"),
    )
}

/// Declining *everything* is not a success: the server asked whether its edit was
/// applied, and it wasn't, so the answer is `applied: false` with a reason it can act
/// on — not the unconditional "applied" a held-back response would otherwise settle to.
#[tokio::test]
async fn declining_every_change_answers_the_server_not_applied() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_decline_all");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &all_annotated_edit_mock(&dir, &record));
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;
    await_prompt(&rpc, &mut incoming, "Update string literals").await;
    feed(&rpc, "n");
    barrier(&rpc).await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "declining every change must be reported as not applied, got {answer:?}"
    );
    assert!(
        answer
            .as_ref()
            .and_then(|v| v.get("failureReason"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|r| r.contains("declined")),
        "…with a reason naming the decline, got {answer:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["let foo = 1".to_string()],
        "and nothing may have applied"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The other way an annotated edit ends up unapplied: the confirm can't be *asked* at
/// all (a config that broke `nx.lsp._confirm_edit`). Declining is the right fallback —
/// but the server has to be told, and this settled the held-back response before it
/// existed, so it answered the unconditional `applied: true` for an edit that never
/// touched a buffer.
#[tokio::test]
async fn a_confirm_that_cannot_be_asked_declines_instead_of_pretending() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_confirm_unreachable");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &all_annotated_edit_mock(&dir, &record));
    let (rpc, _incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    // The ask itself fails, so nothing is ever presented to the user.
    exec_lua(
        &rpc,
        r#"nx.lsp._confirm_edit = function() error("no ui here") end"#,
    )
    .await;

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "an edit nobody could confirm is declined, and the server must hear that \
         rather than a success it can act on, got {answer:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["let foo = 1".to_string()],
        "and nothing may have applied"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `workspace/applyEdit` whose payload we cannot read is refused, loud. Normalizing
/// degraded an unparseable edit to an *empty* one, which is indistinguishable from "the
/// server sent no changes" and settles to `applied: true` — a success reported for
/// something that never reached a buffer. (The wasm client already refused it; this is
/// the native router catching up, so both legs answer a server the same way.)
#[tokio::test]
async fn a_malformed_apply_edit_is_refused_rather_than_acked() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_malformed_edit");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Rewrite", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.rewrite" }} }}
                ],
                "apply_edit": {{ "documentChanges": "not a list of changes" }}
            }}"#,
            rec = record.display(),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "an edit we could not parse must be refused, not acked, got {answer:?}"
    );
    assert!(
        answer
            .as_ref()
            .and_then(|v| v.get("failureReason"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|r| r.contains("malformed")),
        "…with a reason that says so, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// `failedChange` is an index into the `documentChanges` the **server** sent, which is
/// the only numbering it can act on — nxvim declares `failureHandling: abort`, so the
/// index is precisely the "everything before this applied" boundary. A confirmation
/// that drops a change in front of the failure must not shift it: change 0 is declined,
/// change 2 fails, and 2 is what goes back.
#[tokio::test]
async fn failed_change_is_indexed_against_the_edit_the_server_sent() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_failed_change_index");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Rewrite", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.rewrite" }} }}
                ],
                "apply_edit": {{
                    "changeAnnotations": {{
                        "risky": {{ "label": "Update string literals",
                                    "needsConfirmation": true }}
                    }},
                    "documentChanges": [
                        {{
                            "textDocument": {{ "uri": "{a}", "version": 1 }},
                            "edits": [
                                {{ "range": {{ "start": {{ "line": 0, "character": 4 }},
                                               "end": {{ "line": 0, "character": 7 }} }},
                                   "newText": "nope", "annotationId": "risky" }}
                            ]
                        }},
                        {{ "kind": "create", "uri": "{made}" }},
                        {{ "kind": "delete", "uri": "{gone}" }}
                    ]
                }}
            }}"#,
            rec = record.display(),
            a = file_uri(&dir, "a.rs"),
            made = file_uri(&dir, "made.rs"),
            // Never created: the delete fails, and it is change index 2.
            gone = file_uri(&dir, "absent.rs"),
        ),
    );
    let (rpc, mut incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;
    await_prompt(&rpc, &mut incoming, "Update string literals").await;
    feed(&rpc, "n");
    barrier(&rpc).await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("failedChange"))
            .and_then(serde_json::Value::as_u64),
        Some(2),
        "the failing delete is change 2 in the edit the server sent, whatever the \
         confirmation dropped in front of it, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A `delete` with `ignoreIfNotExists` over a file that is already gone is the outcome
/// the server asked for, not a failure to report back — and it must not abort the
/// changes after it.
#[tokio::test]
async fn an_ignore_if_not_exists_delete_of_an_absent_file_is_not_a_failure() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_delete_ignore_missing");
    let record = dir.join("rec.jsonl");
    arm_mock(
        &dir,
        &format!(
            r#"{{
                "record": "{rec}",
                "code_action": [
                    {{ "title": "Tidy up", "kind": "refactor",
                       "command": {{ "title": "run", "command": "mock.tidy" }} }}
                ],
                "apply_edit": {{ "documentChanges": [
                    {{ "kind": "delete", "uri": "{gone}",
                       "options": {{ "ignoreIfNotExists": true }} }},
                    {{ "kind": "create", "uri": "{after}" }},
                    {{ "textDocument": {{ "uri": "{after}", "version": 0 }},
                       "edits": [ {{ "range": {{ "start": {{ "line": 0, "character": 0 }},
                                                 "end": {{ "line": 0, "character": 0 }} }},
                                     "newText": "fn after() {{}}\n" }} ] }}
                ] }}
            }}"#,
            rec = record.display(),
            gone = file_uri(&dir, "never-existed.rs"),
            after = file_uri(&dir, "after.rs"),
        ),
    );
    let (rpc, _incoming) = open_with_server(&dir, "fn main() {}\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    // The change *after* the skipped delete still runs — an `abort` would have dropped it,
    // and then this file would never appear at all.
    let after = dir.join("after.rs");
    let mut exists = false;
    for _ in 0..200 {
        barrier(&rpc).await;
        exists = after.exists();
        if exists {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        exists,
        "an ignored-missing delete must not abort the changes after it"
    );
    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "…and it is not reported as a failure, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A Lua-built edit's columns are counted in the encoding its **caller** names, not in
/// whatever the current buffer's first server happens to have negotiated (utf-8 when it
/// has none). The documented default is the protocol's `utf-16`, so a plain
/// `nx.lsp.apply_workspace_edit` reads utf-16 columns — which is the whole point on a
/// line with multi-byte characters, where the two counts diverge.
#[tokio::test]
async fn a_lua_edits_columns_are_read_at_the_encoding_the_caller_names() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_lua_apply_encoding");
    // `ééé ` is 4 utf-16 units and 7 utf-8 bytes, so `foo` starts at utf-16 column 4
    // and byte column 7 — reading one as the other lands three cells off.
    let (rpc, _incoming) = open_plain(&dir, "ééé foo\n").await;

    let edit = format!(
        r#"nx.lsp.apply_workspace_edit({{ changes = {{
            ["{a}"] = {{ {{ range = {{ start = {{ line = 0, character = 4 }},
                                       ["end"] = {{ line = 0, character = 7 }} }},
                           newText = "bar" }} }},
        }} }})"#,
        a = file_uri(&dir, "a.rs"),
    );
    exec_lua(&rpc, &edit).await;
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["ééé bar".to_string()],
        "the default must be utf-16, the protocol's own — as documented"
    );

    // And an explicit `utf-8` is honored: the same word is at byte column 4..7 now.
    let edit = format!(
        r#"nx.lsp.apply_workspace_edit({{ changes = {{
            ["{a}"] = {{ {{ range = {{ start = {{ line = 0, character = 4 }},
                                       ["end"] = {{ line = 0, character = 7 }} }},
                           newText = "X" }} }},
        }} }}, {{ encoding = "utf-8" }})"#,
        a = file_uri(&dir, "a.rs"),
    );
    exec_lua(&rpc, &edit).await;
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["ééXbar".to_string()],
        "an explicit utf-8 must count bytes — `ééé bar`'s bytes 4..7 are the third \
         `é` and the space, so utf-16's `bar` is untouched"
    );
}

/// A confirm that *rejects* rather than answering must still settle the edit. A
/// server-initiated `workspace/applyEdit` is a request the server is blocked on until
/// the decision lands, so a broken chain would park it — and the server — forever,
/// the same hole the file-operation watchdog closes on the other side.
#[tokio::test]
async fn a_confirm_that_rejects_still_answers_the_waiting_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_confirm_rejects");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &all_annotated_edit_mock(&dir, &record));
    let (rpc, _incoming) = open_with_server(&dir, "let foo = 1\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    // The question is asked but never answered — the promise rejects instead.
    exec_lua(
        &rpc,
        r#"nx.ui.confirm = function() return nx.promise.reject("no ui here") end"#,
    )
    .await;

    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let answer = await_recorded(&record, "_apply_edit_response").await;
    assert_eq!(
        answer
            .as_ref()
            .and_then(|v| v.get("applied"))
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "a confirm nobody could answer declines, and the server is told so instead of \
         being left blocked, got {answer:?}"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

// ----- `'endofline'` and the LSP document seam -----

/// The `text` the mock recorded on the first `textDocument/didOpen`, or `None`.
fn recorded_did_open_text(record: &Path) -> Option<String> {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| {
            v.get("method").and_then(serde_json::Value::as_str) == Some("textDocument/didOpen")
        })
        .and_then(|v| {
            v.pointer("/params/textDocument/text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// Poll until the mock has recorded a `didOpen`, then return its `text`.
async fn await_did_open_text(rpc: &Rpc, record: &Path) -> String {
    for _ in 0..200 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(text) = recorded_did_open_text(record) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the mock never recorded a didOpen");
}

/// What the editor tells a server a file *contains* is the document, not the rope. A
/// file read without a trailing newline must not arrive at the server with one — the
/// rope's phantom `\n` is nxvim bookkeeping standing in for vim's implicit newline after
/// the last line, and neovim's own `buf_get_full_text` likewise appends the line ending
/// only `if vim.bo.eol`. Sending it anyway puts the server's idea of the document one
/// byte (and one line) out of step with the file on disk.
#[tokio::test]
async fn did_open_sends_the_document_not_the_rope() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_didopen");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, record.display()));
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1\nlet b = 2").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    assert_eq!(
        await_did_open_text(&rpc, &record).await,
        "let a = 1\nlet b = 2",
        "a no-eol file must reach the server unterminated"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// …and an ordinary terminated file still arrives terminated (the same code path must
/// not start stripping newlines off every document).
#[tokio::test]
async fn did_open_keeps_the_trailing_newline_of_an_ordinary_file() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_didopen_normal");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, record.display()));
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1\nlet b = 2\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );

    assert_eq!(
        await_did_open_text(&rpc, &record).await,
        "let a = 1\nlet b = 2\n",
        "a terminated file reaches the server terminated"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// Editing a no-eol buffer must keep the server's document in step — *and* stay
/// incremental. A rope-space delta is not a document-space delta when the document is
/// the rope minus its last byte (`dd` on the last line of `a\nb` deletes rope bytes
/// `2..4` but document bytes `1..3`), so the journal is replayed bracketed by the
/// phantom newline: put it on, replay in rope coordinates, take it off. Two extra
/// changes, no whole-document push.
#[tokio::test]
async fn edits_to_a_no_eol_buffer_keep_the_server_in_step() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_didchange");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, record.display()));
    let original = "let a = 1\nlet b = 2";
    let (rpc, _incoming) = open_with_server(&dir, original).await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    await_did_open_text(&rpc, &record).await;

    // `dd` on the last line — the exact shape rope-space deltas get wrong — then an
    // ordinary insert, so the replay covers more than the one bad case.
    feed(&rpc, "Gdd");
    barrier(&rpc).await;
    feed(&rpc, "ggIx <Esc>");
    barrier(&rpc).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    barrier(&rpc).await;

    let (server_view, any_full) = replay_server_changes(original, &record);
    assert_eq!(
        server_view, "x let a = 1",
        "the server's document must match the buffer's document after the edits"
    );
    assert!(
        !any_full,
        "a no-eol buffer must stay incremental — the bracketed replay exists precisely \
         so it never falls back to shipping the whole document"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The `'endofline'` a buffer syncs under can *change* under it, and the two brackets
/// are independent precisely so that costs no resync. A `'fixendofline'` write appends
/// the newline the file was missing — the document the server should hold grows by a
/// byte while the rope, and so `changedtick`, never moves. The flip has to reach the
/// server on its own, as a lone appended-newline change, and the document must still
/// reconstruct exactly afterwards.
#[tokio::test]
async fn a_write_that_terminates_the_file_syncs_the_new_newline() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_write_syncs");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, record.display()));
    let original = "let a = 1\nlet b = 2";
    let (rpc, _incoming) = open_with_server(&dir, original).await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    assert_eq!(
        await_did_open_text(&rpc, &record).await,
        original,
        "the document opens unterminated"
    );

    // `:w` under the default `'fixendofline'` writes the terminator and turns the flag
    // on — with no edit behind it, so nothing bumped `changedtick`.
    feed(&rpc, ":w<CR>");
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "true").await,
        "the write supplied the terminator"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    barrier(&rpc).await;

    let (server_view, any_full) = replay_server_changes(original, &record);
    assert_eq!(
        server_view, "let a = 1\nlet b = 2\n",
        "the server must be told about the newline the write added"
    );
    assert!(
        !any_full,
        "and told incrementally, not by a whole-document push"
    );

    // An ordinary edit afterwards still lands correctly — the shadow crossed the flip
    // rather than drifting a byte out of step with the server.
    feed(&rpc, "ggIx <Esc>");
    barrier(&rpc).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    barrier(&rpc).await;
    let (server_view, _) = replay_server_changes(original, &record);
    assert_eq!(
        server_view, "x let a = 1\nlet b = 2\n",
        "edits after the flip stay in step"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The inverse crossing: a buffer that *loses* its terminator mid-session (a formatter
/// stripping it, here forced with `:set noeol`) must hand the server a document that
/// shrinks by that byte, and keep syncing incrementally from there.
#[tokio::test]
async fn a_buffer_that_loses_its_terminator_syncs_the_removal() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_loses");
    let record = dir.join("rec.jsonl");
    arm_mock(&dir, &format!(r#"{{ "record": "{}" }}"#, record.display()));
    let original = "let a = 1\nlet b = 2\n";
    let (rpc, _incoming) = open_with_server(&dir, original).await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    assert_eq!(
        await_did_open_text(&rpc, &record).await,
        original,
        "the document opens terminated"
    );

    feed(&rpc, ":set noeol<CR>");
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the flag is off"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    barrier(&rpc).await;
    let (server_view, any_full) = replay_server_changes(original, &record);
    assert_eq!(
        server_view, "let a = 1\nlet b = 2",
        "the server's document loses the terminator too"
    );
    assert!(!any_full, "incrementally");

    // And a following edit is still in step, now under the bracketed path.
    feed(&rpc, "Gdd");
    barrier(&rpc).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    barrier(&rpc).await;
    let (server_view, _) = replay_server_changes(original, &record);
    assert_eq!(
        server_view, "let a = 1",
        "`dd` on the last line — the case rope-space deltas get wrong — still lands"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A formatter's reply is about the *document*, so its trailing newline (or absence of
/// one) is the authority on `'endofline'`. Formatting a no-eol file with text that ends
/// in `\n` means "this file now ends with a newline" — the buffer must record that, and
/// the next `:w` must honor it even under `'nofixendofline'`.
#[tokio::test]
async fn a_formatter_that_terminates_the_file_sets_endofline() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_format_adds");
    // A whole-document replacement whose text ends with a newline. The range spans the
    // document (`0:0` to `1:9`, the end of the unterminated last line).
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end": { "line": 1, "character": 9 } },
                  "newText": "let a = 1\nlet b = 3\n" }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1\nlet b = 2").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the file was read unterminated"
    );

    // Issue the request ONCE and wait on its promise. Re-firing it on a timer (the
    // pattern this test used to share with the older ones) can put two requests in
    // flight, and the second carries the *first* request's range — computed against the
    // document as it was before the format — so it applies a stale edit to the formatted
    // text and mangles it. That is correct behavior for a stale range, and a test that
    // provokes it fails at random.
    exec_lua(
        &rpc,
        r#"
        _G.fmt_done = false
        nx.lsp.format():next(function() _G.fmt_done = true end,
                             function(e) _G.fmt_done = "error: " .. tostring(e) end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.fmt_done)", "true").await,
        "the format promise should resolve (got {:?})",
        exec_lua(&rpc, "return tostring(_G.fmt_done)").await
    );
    assert_eq!(
        lines(&rpc).await.get(1).map(String::as_str),
        Some("let b = 3"),
        "formatting should rewrite the second line"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "true").await,
        "the formatter's trailing newline is the document's, so 'endofline' turns on"
    );
    // The formatted text landed once — no spurious blank line from the phantom.
    assert_eq!(
        lines(&rpc).await,
        vec!["let a = 1", "let b = 3"],
        "the replacement must not leave a trailing empty line"
    );
    // And it is what reaches disk, even with `'fixendofline'` off (so the newline can
    // only have come from the flag the formatter set).
    feed(&rpc, ":set nofixeol<CR>:w<CR>");
    barrier(&rpc).await;
    assert_eq!(
        std::fs::read(dir.join("a.rs")).expect("re-read"),
        b"let a = 1\nlet b = 3\n",
        "the formatted document is written terminated"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The converse: a formatter that strips the trailing newline off a terminated file is
/// telling us the document no longer ends with one. Under `'nofixendofline'` that has to
/// reach disk — the previous behavior silently kept the file terminated, because the
/// phantom newline was indistinguishable from a real one.
#[tokio::test]
async fn a_formatter_that_strips_the_terminator_clears_endofline() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_format_strips");
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end": { "line": 2, "character": 0 } },
                  "newText": "let a = 1\nlet b = 3" }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1\nlet b = 2\n").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "true").await,
        "the file was read terminated"
    );

    // Issue the request ONCE and wait on its promise. Re-firing it on a timer (the
    // pattern this test used to share with the older ones) can put two requests in
    // flight, and the second carries the *first* request's range — computed against the
    // document as it was before the format — so it applies a stale edit to the formatted
    // text and mangles it. That is correct behavior for a stale range, and a test that
    // provokes it fails at random.
    exec_lua(
        &rpc,
        r#"
        _G.fmt_done = false
        nx.lsp.format():next(function() _G.fmt_done = true end,
                             function(e) _G.fmt_done = "error: " .. tostring(e) end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.fmt_done)", "true").await,
        "the format promise should resolve (got {:?})",
        exec_lua(&rpc, "return tostring(_G.fmt_done)").await
    );
    assert_eq!(
        lines(&rpc).await.get(1).map(String::as_str),
        Some("let b = 3"),
        "formatting should rewrite the second line"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the formatter's unterminated text clears 'endofline'"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["let a = 1", "let b = 3"],
        "the lines are unchanged in shape"
    );
    feed(&rpc, ":set nofixeol<CR>:w<CR>");
    barrier(&rpc).await;
    assert_eq!(
        std::fs::read(dir.join("a.rs")).expect("re-read"),
        b"let a = 1\nlet b = 3",
        "'nofixendofline' honors the formatter's stripped terminator"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// Which edit owns the document's **tail** is decided by where it *starts*, not by its
/// index in the reply. Two edits can end at the same offset — the document's end — and
/// then the rightmost one in the result is the one that starts later, whatever order the
/// server listed them in (plenty of servers emit their edits bottom-up).
///
/// Getting that wrong doesn't just mis-set `'endofline'`: the tail edit is the one
/// widened to swallow the rope's phantom newline, so widening the *earlier* edit
/// stretches it over its sibling and one of the two is eaten.
///
/// Here the document is `let a = 1` (no terminator). The reply appends `;` at the end and
/// replaces the `1` before it — listed in that order, i.e. tail first.
#[tokio::test]
async fn the_tail_edit_is_the_one_that_starts_last_not_the_one_listed_last() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_eol_format_tail_order");
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                { "range": { "start": { "line": 0, "character": 9 },
                             "end": { "line": 0, "character": 9 } },
                  "newText": ";" },
                { "range": { "start": { "line": 0, "character": 8 },
                             "end": { "line": 0, "character": 9 } },
                  "newText": "2" }
            ]
        }"#,
    );
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the file was read unterminated"
    );

    // Once, on the promise — a re-firing retry can land a second, stale-ranged request
    // over the formatted text (see the note in the whole-document test above).
    exec_lua(
        &rpc,
        r#"
        _G.fmt_done = false
        nx.lsp.format():next(function() _G.fmt_done = true end,
                             function(e) _G.fmt_done = "error: " .. tostring(e) end)
        "#,
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "tostring(_G.fmt_done)", "true").await,
        "the format promise should resolve (got {:?})",
        exec_lua(&rpc, "return tostring(_G.fmt_done)").await
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["let a = 2;"],
        "both edits land: the appended `;` is not swallowed by the replacement before it"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the tail edit's text has no newline, so the document stays unterminated"
    );
    feed(&rpc, ":w<CR>");
    barrier(&rpc).await;
    assert_eq!(
        std::fs::read(dir.join("a.rs")).expect("re-read"),
        b"let a = 2;\n",
        "the whole formatted document reaches disk"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}

/// A formatter's **whole-document** range on a file with no trailing newline replaces the
/// whole document — not just its first line.
///
/// The range servers send for "format everything" ends at `{ line: <line count>,
/// character: 0 }`: one row past the last one, the position that addresses the end of a
/// terminated document. On an unterminated document that row does not exist, and clamping
/// the row alone resolved the position to the *start of the last row* — a whole line short
/// of the document's end — so the replacement landed over line 1 and the rest of the file
/// was left dangling after it. `let a = 1 / let b = 2` (unterminated) formatted to
/// `let a = 1 / let b = 3let b = 2`: silent corruption of the buffer, on the single most
/// ordinary LSP edit there is, for every file that happens not to end with a newline.
///
/// Formatting again is the second half of the guarantee: the same edit over the result is
/// a no-op, because it really did cover the whole document.
#[tokio::test]
async fn formatting_replaces_the_whole_document_when_it_has_no_trailing_newline() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_features_format_noeol_whole");
    arm_mock(
        &dir,
        r#"{
            "formatting": [
                { "range": { "start": { "line": 0, "character": 0 },
                             "end": { "line": 2, "character": 0 } },
                  "newText": "let a = 1\nlet b = 3" }
            ]
        }"#,
    );
    // The file on disk ends without a newline, so 'endofline' is off from the first read.
    let (rpc, _incoming) = open_with_server(&dir, "let a = 1\nlet b = 2").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should attach"
    );
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the file was read unterminated"
    );

    for round in 1..=2 {
        exec_lua(
            &rpc,
            r#"
            _G.fmt_done = false
            nx.lsp.format():next(function() _G.fmt_done = true end,
                                 function(e) _G.fmt_done = "error: " .. tostring(e) end)
            "#,
        )
        .await;
        assert!(
            await_lua_eq(&rpc, "tostring(_G.fmt_done)", "true").await,
            "round {round}: the format promise should resolve"
        );
        assert_eq!(
            lines(&rpc).await,
            vec!["let a = 1", "let b = 3"],
            "round {round}: the whole document should be replaced"
        );
    }
    // Still unterminated, and it reaches disk that way under 'nofixeol'.
    assert!(
        await_lua_eq(&rpc, "nx.bo[0].endofline", "false").await,
        "the formatter's text still ends without a newline"
    );
    std::env::remove_var("NXVIM_LSP_CMD");
}
