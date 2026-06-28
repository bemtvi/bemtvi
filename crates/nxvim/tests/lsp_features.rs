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
    attach, barrier, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, serial_lock,
    spawn, temp_dir, window0_field,
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
    for _ in 0..80 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            let Some(Value::Map(menu)) = map_get(&map, "menu") else {
                continue;
            };
            if let Some(Value::Array(items)) = map_get(menu, "items") {
                return Some(
                    items
                        .iter()
                        .map(|row| match row {
                            Value::Array(a) => {
                                a.first().and_then(Value::as_str).unwrap_or("").to_string()
                            }
                            Value::String(s) => s.as_str().unwrap_or("").to_string(),
                            _ => String::new(),
                        })
                        .collect(),
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
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
