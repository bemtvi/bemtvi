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

    // Confirming the action applies its edit.
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
