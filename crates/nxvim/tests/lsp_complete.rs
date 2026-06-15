//! Black-box tests for the built-in **`lsp` completion source** on the unified
//! `nx.complete` engine (Phase 4-C — the bespoke pmenu was retired). A real server
//! is driven through the scripted mock language server (`nxvim --__lsp-mock`,
//! `nxvim_lsp::mock`): it speaks real LSP over stdio and returns deterministic
//! `textDocument/completion` results, so the whole path — request, the streamed
//! reply landing in the unified menu, fuzzy ranking by prefix, and the delegated
//! accept applying `textEdit` + `additionalTextEdits` — is exercised end-to-end,
//! network-free.
//!
//! The mock is wired exactly like the syntax tests wire `NXVIM_TS_WORKER`: the
//! `$NXVIM_LSP_CMD` env hook overrides the server's spawn argv (so a test points it
//! at `nxvim --__lsp-mock <script>`), and the server is bound to the buffer via the
//! raw `nx._lsp_start` bridge. Because that env is process-global, these tests
//! serialize on `serial_lock`.

use std::path::Path;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, lines, map_get, serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The real `nxvim` binary — re-invoked in its hidden `--__lsp-mock` mode as the
/// scripted language server (the LSP analogue of `NXVIM_TS_WORKER`).
const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// Write the mock LSP script (a JSON object the mock reads) and return its path.
/// `completion_json` is the `completion` field — a `CompletionItem[]`.
fn write_script(dir: &Path, completion_json: &str) -> String {
    let script = format!(r#"{{ "completion": {completion_json} }}"#);
    let path = dir.join("mock.json");
    std::fs::write(&path, script).expect("write mock script");
    path.to_string_lossy().into_owned()
}

/// The visible completion-menu row labels of the latest redraw carrying a menu, or
/// `None` if none arrives within the poll window.
async fn poll_menu_items(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| {
        matches!(map_get(m, "menu"), Some(Value::Map(_)))
    })?;
    let Some(Value::Map(menu)) = map_get(&map, "menu") else {
        return None;
    };
    match map_get(menu, "items") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|row| match row {
                    Value::Array(a) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
                    Value::String(s) => s.as_str().unwrap_or("").to_string(),
                    _ => String::new(),
                })
                .collect(),
        ),
        _ => Some(Vec::new()),
    }
}

/// The docs-sidebar lines of the latest redraw whose menu carries a `docs` sub-map,
/// or `None` if none arrives within the poll window. The sidebar's `lines` are the
/// selected item's `detail` + `documentation` (Phase 4-D).
async fn poll_menu_docs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| match map_get(m, "menu") {
        Some(Value::Map(menu)) => matches!(map_get(menu, "docs"), Some(Value::Map(_))),
        _ => false,
    })?;
    let Some(Value::Map(menu)) = map_get(&map, "menu") else {
        return None;
    };
    let Some(Value::Map(docs)) = map_get(menu, "docs") else {
        return None;
    };
    match map_get(docs, "lines") {
        Some(Value::Array(lines)) => Some(
            lines
                .iter()
                .map(|l| l.as_str().unwrap_or("").to_string())
                .collect(),
        ),
        _ => Some(Vec::new()),
    }
}

/// Retry the manual trigger until the docs sidebar appears and contains `want` (a
/// substring of some line) — the inline-docs path shows it at once, the lazy
/// `completionItem/resolve` path after the round-trip lands. Panics after the window.
async fn await_docs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, "nx.complete.trigger()").await;
        if let Some(docs) = poll_menu_docs(rpc, incoming).await {
            if docs.iter().any(|l| l.contains(want)) {
                return docs;
            }
            last = docs;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("docs sidebar never contained {want:?}; last sidebar lines: {last:?}");
}

/// Start a server editing a fresh `.rs` file, attach a UI, bind the mock LSP server
/// to the buffer, enable `nx.complete` with the `lsp` source, and enter insert mode
/// with `prefix` typed. Returns the rpc + redraw stream. The caller drives the
/// completion (retrying the trigger until the async server reply lands).
async fn start_typed(
    dir: &Path,
    completion_json: &str,
    prefix: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    write_script(dir, completion_json);
    start_after_script(dir, prefix).await
}

/// Like [`start_typed`], but the caller supplies the **whole** mock script object
/// (so a test can add a `completion_resolve` field for the lazy-docs path), not just
/// the `completion` array.
async fn start_typed_raw(
    dir: &Path,
    raw_script: &str,
    prefix: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("mock.json"), raw_script).expect("write mock script");
    start_after_script(dir, prefix).await
}

/// The shared post-script setup for [`start_typed`] / [`start_typed_raw`]: open the
/// `.rs` buffer, attach, bind the mock server, enable the `lsp` source, type `prefix`.
async fn start_after_script(dir: &Path, prefix: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "").expect("write test file");

    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Bind the mock as this buffer's server. `$NXVIM_LSP_CMD` overrides the spawn
    // argv (set by the caller under the serial lock), so the `cmd` here is a
    // placeholder; the filetype is `rust` to match the `.rs` document.
    exec_lua(
        &rpc,
        "nx._lsp_start('mock', { 'placeholder' }, vim.fn.getcwd(), 'rust', \
         vim.api.nvim_get_current_buf(), nil, nil, nil)",
    )
    .await;
    exec_lua(
        &rpc,
        "nx.complete.setup { sources = { { 'lsp' } }, min_chars = 1 }",
    )
    .await;

    feed(&rpc, &format!("i{prefix}"));
    (rpc, incoming)
}

/// Retry the manual completion trigger until the async LSP reply lands and the menu
/// shows the expected items (the server takes a moment to initialize). Panics after
/// the window with the last seen items.
async fn await_items(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, "nx.complete.trigger()").await;
        if let Some(items) = poll_menu_items(rpc, incoming).await {
            if items.iter().any(|i| i == want) {
                return items;
            }
            last = items;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("lsp completion never produced {want:?}; last menu items: {last:?}");
}

#[tokio::test]
async fn lsp_completion_candidates_appear_in_the_unified_menu() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_show");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    let completion = r#"[ { "label": "println", "insertText": "println" },
                          { "label": "print_value", "insertText": "print_value" } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "pr").await;

    let items = await_items(&rpc, &mut incoming, "println").await;
    assert!(
        items.contains(&"println".to_string()) && items.contains(&"print_value".to_string()),
        "the server's items reach the unified menu: {items:?}"
    );
    // The document holds only the typed prefix — completion did not eat the keys.
    assert_eq!(lines(&rpc).await, vec!["pr"]);

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn accepting_an_lsp_item_applies_its_text_edit() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_textedit");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    // An item with an explicit textEdit replacing the typed `pr` (cols 0..2) with a
    // call, plus an additionalTextEdit prepending an import on line 0.
    let completion = r#"[ {
        "label": "print_value",
        "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                 "end": { "line": 0, "character": 2 } },
                      "newText": "print_value()" },
        "additionalTextEdits": [ { "range": { "start": { "line": 0, "character": 0 },
                                              "end": { "line": 0, "character": 0 } },
                                   "newText": "use foo;\n" } ]
    } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "pr").await;

    // Drive the trigger until the item shows (it preselects row 0 on a manual
    // trigger), then accept: the server applies the textEdit + additionalTextEdits.
    await_items(&rpc, &mut incoming, "print_value").await;
    feed(&rpc, "<C-y>");
    assert_eq!(
        lines(&rpc).await,
        vec!["use foo;", "print_value()"],
        "textEdit + additionalTextEdits applied as one delegated edit"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn lsp_completion_docs_sidebar_shows_inline_documentation() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_docs_inline");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    // The item carries its docs *inline* (`detail` + `documentation`), so the docs
    // sidebar renders them with no `completionItem/resolve` round-trip.
    let completion = r#"[ { "label": "println", "insertText": "println",
                          "detail": "macro println!",
                          "documentation": "Prints to stdout with a trailing newline." } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "pr").await;

    // The manual trigger preselects row 0, so the selected item's docs float beside
    // the popup.
    await_items(&rpc, &mut incoming, "println").await;
    let docs = await_docs(&rpc, &mut incoming, "Prints to stdout").await;
    assert!(
        docs.iter().any(|l| l.contains("macro println!")),
        "the item's `detail` heads the docs sidebar: {docs:?}"
    );
    assert!(
        docs.iter().any(|l| l.contains("Prints to stdout")),
        "the item's `documentation` body shows in the docs sidebar: {docs:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn lsp_completion_docs_resolve_fills_the_sidebar() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_docs_resolve");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    // The completion item carries **no** inline docs (rust_analyzer's shape): the
    // server must issue `completionItem/resolve` for the selected row, and the mock's
    // scripted `completion_resolve` supplies the lazy docs that fill the sidebar.
    let script = r#"{ "completion": [ { "label": "compute", "insertText": "compute" } ],
                      "completion_resolve": { "label": "compute",
                        "detail": "fn compute() -> i32",
                        "documentation": "Resolved: computes the answer." } }"#;
    let (rpc, mut incoming) = start_typed_raw(&dir, script, "com").await;

    await_items(&rpc, &mut incoming, "compute").await;
    // No inline docs → the sidebar fills only after the resolve round-trip lands.
    let docs = await_docs(&rpc, &mut incoming, "Resolved: computes").await;
    assert!(
        docs.iter().any(|l| l.contains("fn compute() -> i32")),
        "the resolved `detail` heads the docs sidebar: {docs:?}"
    );
    assert!(
        docs.iter()
            .any(|l| l.contains("Resolved: computes the answer")),
        "the resolved `documentation` fills the docs sidebar: {docs:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}
