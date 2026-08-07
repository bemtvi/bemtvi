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
    attach, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, menu_items, menu_of,
    serial_lock, spawn, temp_dir,
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
    // Single-shot (one barrier + drain), NOT the harness's retrying `poll_menu`:
    // the callers run their own retry loops around this.
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| {
        matches!(map_get(m, "menu"), Some(Value::Map(_)))
    })?;
    Some(menu_items(&menu_of(&map)))
}

/// The completion **docs float window** (`[CompletionDocs]`) lines of the latest redraw
/// carrying it, or `None` if none arrives within the poll window. The completion docs
/// are a real doc-float window now (not a `menu.docs` overlay): its stripped-markdown
/// lines are the selected item's `detail` (fenced) + `documentation` (Phase 4-D).
async fn poll_menu_docs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    fn docs_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
        let Some(Value::Array(wins)) = map_get(map, "windows") else {
            return None;
        };
        wins.iter().find_map(|w| match w {
            Value::Map(wm)
                if map_get(wm, "file_name").and_then(Value::as_str) == Some("[CompletionDocs]") =>
            {
                Some(wm.clone())
            }
            _ => None,
        })
    }
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| docs_window(m).is_some())?;
    let win = docs_window(&map)?;
    match map_get(&win, "lines") {
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
async fn accepting_a_snippet_item_expands_tabstops() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_snippet");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    // A snippet item (`insertTextFormat = 2`): its textEdit body has a `$1` tabstop
    // inside the parens and a final `$0` after them.
    let completion = r#"[ {
        "label": "print_value",
        "insertTextFormat": 2,
        "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                 "end": { "line": 0, "character": 2 } },
                      "newText": "print_value($1)$0" }
    } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "pr").await;

    await_items(&rpc, &mut incoming, "print_value").await;
    feed(&rpc, "<C-y>");
    // The markers are gone and the cursor sits at `$1`, inside the parens.
    assert_eq!(lines(&rpc).await, vec!["print_value()"]);
    assert_eq!(cursor(&rpc).await, (1, 12));

    // Typing fills the tabstop; <Tab> jumps to the final `$0` after the parens.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["print_value(x)"]);
    feed(&rpc, "<Tab>");
    assert_eq!(cursor(&rpc).await, (1, 14));

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

/// The latest menu's `(label, kind)` pairs — the projected `items` array zipped with
/// the parallel `kinds` array (`None` per row where the key/entry is absent).
async fn poll_menu_rows(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(String, Option<String>)>> {
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| {
        matches!(map_get(m, "menu"), Some(Value::Map(_)))
    })?;
    let menu = menu_of(&map);
    let labels = menu_items(&menu);
    let kinds = match map_get(&menu, "kinds") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string)).collect(),
        _ => vec![None; labels.len()],
    };
    Some(labels.into_iter().zip(kinds).collect())
}

/// An `lsp` completion row projects its `CompletionItemKind` as a readable kind
/// label (`3`→`"Function"`, `6`→`"Variable"`), so the popup shows what each item is.
/// A kind-less item (no `kind` field) projects `None`.
#[tokio::test]
async fn lsp_completion_projects_item_kind_labels() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_kind");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    let completion = r#"[ { "label": "println", "insertText": "println", "kind": 3 },
                          { "label": "print_flag", "insertText": "print_flag", "kind": 6 },
                          { "label": "print_bare", "insertText": "print_bare" } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "print").await;

    // Wait until the async reply lands and the popup shows the items.
    await_items(&rpc, &mut incoming, "println").await;
    let rows = poll_menu_rows(&rpc, &mut incoming)
        .await
        .expect("menu rows with kinds");
    let kind_of = |label: &str| {
        rows.iter()
            .find(|(l, _)| l == label)
            .map(|(_, k)| k.clone())
            .unwrap_or_else(|| panic!("row {label:?} not in menu: {rows:?}"))
    };
    assert_eq!(kind_of("println").as_deref(), Some("Function"));
    assert_eq!(kind_of("print_flag").as_deref(), Some("Variable"));
    // An item without a `kind` field projects no label (not a bogus one).
    assert_eq!(kind_of("print_bare"), None);

    std::env::remove_var("NXVIM_LSP_CMD");
}

// ----- Phase 3c: completion across every capable server ----------------------
// docs/plans/2026-07-25-multi-server-lsp-attach.md. Completion was the last
// single-target kind: only the first server advertising `completionProvider` was
// asked, so a `pyright` + `ruff` buffer showed half the candidates and accepted
// them all at the first server's encoding.

/// Point `$NXVIM_LSP_CMD_<NAME>` at the mock with its own script, so two servers
/// answer differently (the blanket `$NXVIM_LSP_CMD` would aim both at one script).
fn arm_completion_mock(dir: &Path, name: &str, script: &str) {
    let file = dir.join(format!("mock-{name}.json"));
    std::fs::write(&file, script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        format!("NXVIM_LSP_CMD_{}", name.to_uppercase()),
        format!("{NXVIM_BIN} --__lsp-mock {}", file.display()),
    );
}

fn disarm_completion_mocks() {
    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");
}

/// Poll `expr` until it equals `want`; returns whether it matched.
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..200 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

/// Open `body` in a `.rs` buffer with BOTH mock servers enabled for `rust`, wait for
/// them to attach, enable the `lsp` completion source, then run `keys` (which enters
/// insert mode and types the prefix).
async fn start_two_servers(
    dir: &Path,
    body: &str,
    keys: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both completion servers attached"
    );
    exec_lua(
        &rpc,
        "nx.complete.setup { sources = { { 'lsp' } }, min_chars = 1 }",
    )
    .await;
    feed(&rpc, keys);
    (rpc, incoming)
}

#[tokio::test]
async fn completion_merges_candidates_from_every_capable_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_two_servers");
    arm_completion_mock(
        dir.as_path(),
        "alpha",
        r#"{ "completion": [ { "label": "pr_alpha", "insertText": "pr_alpha" } ] }"#,
    );
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "completion": [ { "label": "pr_beta", "insertText": "pr_beta" } ] }"#,
    );
    let (rpc, mut incoming) = start_two_servers(dir.as_path(), "", "ipr").await;

    // Both servers' candidates in one popup. Only alpha's showed before this phase —
    // it is the first `completionProvider` in key order, and it alone was asked.
    let mut merged = Vec::new();
    for _ in 0..200 {
        exec_lua(&rpc, "nx.complete.trigger()").await;
        if let Some(items) = poll_menu_items(&rpc, &mut incoming).await {
            if items.iter().any(|i| i == "pr_alpha") && items.iter().any(|i| i == "pr_beta") {
                merged = items;
                break;
            }
            if !items.is_empty() {
                merged = items;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    disarm_completion_mocks();
    assert!(
        merged.contains(&"pr_alpha".to_string()) && merged.contains(&"pr_beta".to_string()),
        "both servers' candidates merge into one popup, got {merged:?}"
    );
}

#[tokio::test]
async fn accepting_a_candidate_applies_its_own_server_encoding() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_two_enc");
    // `föö.` is bytes 0..6 but utf-16 units 0..4 — the two servers disagree about
    // every column after the first multi-byte glyph.
    arm_completion_mock(dir.as_path(), "alpha", r#"{ "completion": [] }"#);
    // beta replaces utf-16 units 3..6 (`.pr`) with `.print()`. Read at alpha's utf-8
    // that range is bytes 3..6 — the second `ö` and the dot — so a mis-encoded accept
    // eats a glyph and yields `fö.print()`.
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "position_encoding": "utf-16",
             "completion": [ {
               "label": "print",
               "textEdit": { "range": { "start": { "line": 0, "character": 3 },
                                        "end":   { "line": 0, "character": 6 } },
                             "newText": ".print()" } } ] }"#,
    );
    let (rpc, mut incoming) = start_two_servers(dir.as_path(), "föö.\n", "Apr").await;

    await_items(&rpc, &mut incoming, "print").await;
    feed(&rpc, "<C-y>");
    let line = lines(&rpc).await.first().cloned().unwrap_or_default();

    disarm_completion_mocks();
    assert_eq!(
        line, "föö.print()",
        "the accept converts the textEdit at BETA's utf-16, not the first server's \
         utf-8 (`fö.print()` is the mis-encoded read)"
    );
}

#[tokio::test]
async fn a_lazy_docs_resolve_goes_back_to_the_items_own_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_two_resolve");
    // alpha offers nothing but still sorts first, so it is the buffer's "primary"
    // server — and it answers `completionItem/resolve` with its OWN docs. The only
    // row in the popup is beta's, so its docs must come from beta: a resolve routed
    // to the first server is not a degraded result, it is a wrong request (the `data`
    // blob is only meaningful to the server that issued the item).
    arm_completion_mock(
        dir.as_path(),
        "alpha",
        r#"{ "completion": [],
             "completion_resolve": { "label": "compute",
               "detail": "FROM-ALPHA", "documentation": "FROM-ALPHA-RESOLVE" } }"#,
    );
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "completion": [ { "label": "compute", "insertText": "compute" } ],
             "completion_resolve": { "label": "compute",
               "detail": "fn compute() -> i32", "documentation": "FROM-BETA-RESOLVE" } }"#,
    );
    let (rpc, mut incoming) = start_two_servers(dir.as_path(), "", "icom").await;

    await_items(&rpc, &mut incoming, "compute").await;
    let docs = await_docs(&rpc, &mut incoming, "FROM-BETA-RESOLVE").await;

    disarm_completion_mocks();
    assert!(
        docs.iter().any(|l| l.contains("FROM-BETA-RESOLVE")),
        "the resolve reached the item's own server: {docs:?}"
    );
    assert!(
        !docs.iter().any(|l| l.contains("FROM-ALPHA")),
        "and not the buffer's first server: {docs:?}"
    );
}

#[tokio::test]
async fn a_completion_burst_does_not_accumulate_candidates() {
    // The amplification guard (plan §Risks): fanning out re-requests per keystroke,
    // so a burst must stay bounded — each round REPLACES the merged candidates rather
    // than piling both servers' items on top of the last round's. A cache that grew
    // per keystroke would still "work" until the popup showed hundreds of duplicates.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_two_burst");
    arm_completion_mock(
        dir.as_path(),
        "alpha",
        r#"{ "completion": [ { "label": "prfix_alpha", "insertText": "prfix_alpha" } ] }"#,
    );
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "completion": [ { "label": "prfix_beta", "insertText": "prfix_beta" } ] }"#,
    );
    let (rpc, mut incoming) = start_two_servers(dir.as_path(), "", "ip").await;

    // Type a burst inside one word, each key re-triggering the fan-out.
    let started = std::time::Instant::now();
    for _ in 0..12 {
        feed(&rpc, "r<BS>");
    }
    feed(&rpc, "r");
    nxvim_test_harness::barrier(&rpc).await;
    let elapsed = started.elapsed();

    let mut rows = Vec::new();
    for _ in 0..200 {
        exec_lua(&rpc, "nx.complete.trigger()").await;
        if let Some(items) = poll_menu_items(&rpc, &mut incoming).await {
            if items.iter().any(|i| i == "prfix_alpha") && items.iter().any(|i| i == "prfix_beta") {
                rows = items;
                break;
            }
            if !items.is_empty() {
                rows = items;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let alphas = rows.iter().filter(|i| *i == "prfix_alpha").count();
    let betas = rows.iter().filter(|i| *i == "prfix_beta").count();

    disarm_completion_mocks();
    assert_eq!(
        (alphas, betas),
        (1, 1),
        "each round replaces the merged candidates — one row per server after a \
         12-keystroke burst, got {rows:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "the burst must not stall the editor (took {elapsed:?})"
    );
}

/// A candidate **both** servers offer is one row in the popup — the two would be
/// indistinguishable and accept to the same text — but what each server has to *say*
/// about it is its own, and the docs float shows both, each under its labelled rule,
/// in routing order.
///
/// Regression: the duplicate offer was dropped outright, so the docs shown came from
/// whichever server happened to answer first — one server's explanation vanished, and
/// which one it was varied run to run. This is the completion twin of the merged hover
/// (`docs/plans/2026-07-25-multi-server-lsp-attach.md`).
#[tokio::test]
async fn a_candidate_two_servers_offer_shows_both_servers_docs_in_sections() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_shared_docs");
    // Identical offers — same label, same inserted text — so they share a row. Only
    // the documentation differs, which is exactly what must survive the merge.
    arm_completion_mock(
        dir.as_path(),
        "alpha",
        r#"{ "completion": [ { "label": "compute", "insertText": "compute",
             "documentation": "ALPHA-EXPLAINS" } ] }"#,
    );
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "completion": [ { "label": "compute", "insertText": "compute",
             "documentation": "BETA-EXPLAINS" } ] }"#,
    );
    let (rpc, mut incoming) = start_two_servers(dir.as_path(), "", "icom").await;

    await_items(&rpc, &mut incoming, "compute").await;
    let docs = await_docs(&rpc, &mut incoming, "BETA-EXPLAINS").await;
    let rows = poll_menu_items(&rpc, &mut incoming)
        .await
        .unwrap_or_default();

    disarm_completion_mocks();
    assert!(
        docs.iter().any(|l| l.contains("ALPHA-EXPLAINS"))
            && docs.iter().any(|l| l.contains("BETA-EXPLAINS")),
        "both servers' docs reach the float, not just the quickest one's: {docs:?}"
    );
    // Each under its own labelled rule — `─ alpha ────`, the merged hover's section
    // header — so the reader knows which server made which claim...
    let at = |label: &str| {
        docs.iter()
            .position(|l| l.trim() == format!("─ {label}"))
            .unwrap_or_else(|| panic!("no `{label}` section header in {docs:?}"))
    };
    // ...and in routing order (`alpha` sorts first), NOT reply order.
    assert!(
        at("alpha") < at("beta"),
        "sections follow the servers' routing order: {docs:?}"
    );
    // The popup still shows the shared candidate ONCE — two identical rows would be
    // noise, which is why the offers merge rather than doubling up.
    assert_eq!(
        rows.iter().filter(|r| *r == "compute").count(),
        1,
        "the shared candidate is one row, got {rows:?}"
    );
}

/// A shared row's docs are **lazy** per contributor: each server holds its own behind
/// its own `completionItem/resolve`, and each resolve must go back to the server that
/// issued that section's item. Both sections fill in, one round-trip at a time.
///
/// The one-at-a-time walk is forced by the request layer — `lsp_requests` keeps a
/// single slot per request kind, so two concurrent resolves would supersede each other
/// and one section would settle empty forever.
#[tokio::test]
async fn each_contributors_docs_resolve_against_its_own_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_shared_resolve");
    // Neither server sends inline documentation — both withhold it until resolved.
    arm_completion_mock(
        dir.as_path(),
        "alpha",
        r#"{ "completion": [ { "label": "compute", "insertText": "compute" } ],
             "completion_resolve": { "label": "compute",
               "documentation": "ALPHA-RESOLVED" } }"#,
    );
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "completion": [ { "label": "compute", "insertText": "compute" } ],
             "completion_resolve": { "label": "compute",
               "documentation": "BETA-RESOLVED" } }"#,
    );
    let (rpc, mut incoming) = start_two_servers(dir.as_path(), "", "icom").await;

    await_items(&rpc, &mut incoming, "compute").await;
    // The second section only exists once the *first* resolve has landed and the walk
    // moved on, so waiting for beta's implies alpha's arrived too.
    let docs = await_docs(&rpc, &mut incoming, "BETA-RESOLVED").await;

    disarm_completion_mocks();
    assert!(
        docs.iter().any(|l| l.contains("ALPHA-RESOLVED")),
        "the first contributor's resolve landed in its own section: {docs:?}"
    );
}
