//! Black-box tests for the built-in **`lsp` completion source** on the unified
//! `btv.complete` engine (Phase 4-C — the bespoke pmenu was retired). A real server
//! is driven through the scripted mock language server (`bemtvi --__lsp-mock`,
//! `bemtvi_lsp::mock`): it speaks real LSP over stdio and returns deterministic
//! `textDocument/completion` results, so the whole path — request, the streamed
//! reply landing in the unified menu, fuzzy ranking by prefix, and the delegated
//! accept applying `textEdit` + `additionalTextEdits` — is exercised end-to-end,
//! network-free.
//!
//! The mock is wired exactly like the syntax tests wire `BEMTVI_TS_WORKER`: the
//! `$BEMTVI_LSP_CMD` env hook overrides the server's spawn argv (so a test points it
//! at `bemtvi --__lsp-mock <script>`), and the server is bound to the buffer via the
//! raw `btv._lsp_start` bridge. Because that env is process-global, these tests
//! serialize on `serial_lock`.

use std::path::Path;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, menu_items, menu_of,
    serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The real `bemtvi` binary — re-invoked in its hidden `--__lsp-mock` mode as the
/// scripted language server (the LSP analogue of `BEMTVI_TS_WORKER`).
const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

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
    bemtvi_test_harness::barrier(rpc).await;
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
    bemtvi_test_harness::barrier(rpc).await;
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
        exec_lua(rpc, "btv.complete.trigger()").await;
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
/// to the buffer, enable `btv.complete` with the `lsp` source, and enter insert mode
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

    // Bind the mock as this buffer's server. `$BEMTVI_LSP_CMD` overrides the spawn
    // argv (set by the caller under the serial lock), so the `cmd` here is a
    // placeholder; the filetype is `rust` to match the `.rs` document.
    exec_lua(
        &rpc,
        "btv._lsp_start('mock', { 'placeholder' }, vim.fn.getcwd(), 'rust', \
         vim.api.nvim_get_current_buf(), nil, nil, nil)",
    )
    .await;
    exec_lua(
        &rpc,
        "btv.complete.setup { sources = { { 'lsp' } }, min_chars = 1 }",
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
        exec_lua(rpc, "btv.complete.trigger()").await;
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
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
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

    std::env::remove_var("BEMTVI_LSP_CMD");
}

#[tokio::test]
async fn accepting_an_lsp_item_applies_its_text_edit() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_textedit");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
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

    std::env::remove_var("BEMTVI_LSP_CMD");
}

#[tokio::test]
async fn accepting_a_snippet_item_expands_tabstops() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_snippet");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
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

    std::env::remove_var("BEMTVI_LSP_CMD");
}

#[tokio::test]
async fn lsp_completion_docs_sidebar_shows_inline_documentation() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_docs_inline");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
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

    std::env::remove_var("BEMTVI_LSP_CMD");
}

#[tokio::test]
async fn lsp_completion_docs_resolve_fills_the_sidebar() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_docs_resolve");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
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

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// The latest menu's `(label, kind)` pairs — the projected `items` array zipped with
/// the parallel `kinds` array (`None` per row where the key/entry is absent).
async fn poll_menu_rows(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(String, Option<String>)>> {
    bemtvi_test_harness::barrier(rpc).await;
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
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
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

    std::env::remove_var("BEMTVI_LSP_CMD");
}

// ----- Phase 3c: completion across every capable server ----------------------
// docs/plans/2026-07-25-multi-server-lsp-attach.md. Completion was the last
// single-target kind: only the first server advertising `completionProvider` was
// asked, so a `pyright` + `ruff` buffer showed half the candidates and accepted
// them all at the first server's encoding.

/// Point `$BEMTVI_LSP_CMD_<NAME>` at the mock with its own script, so two servers
/// answer differently (the blanket `$BEMTVI_LSP_CMD` would aim both at one script).
fn arm_completion_mock(dir: &Path, name: &str, script: &str) {
    let file = dir.join(format!("mock-{name}.json"));
    std::fs::write(&file, script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        format!("BEMTVI_LSP_CMD_{}", name.to_uppercase()),
        format!("{BEMTVI_BIN} --__lsp-mock {}", file.display()),
    );
}

fn disarm_completion_mocks() {
    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");
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
    start_two_servers_ranked(dir, body, keys, "", "").await
}

/// [`start_two_servers`] with each server's extra `btv.lsp.config` fields spelled out
/// (`alpha_extra` / `beta_extra`, e.g. `priority = 10,`), so a test can state a routing
/// order that disagrees with the alphabetical key order.
async fn start_two_servers_ranked(
    dir: &Path,
    body: &str,
    keys: &str,
    alpha_extra: &str,
    beta_extra: &str,
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
        &format!(
            "btv.lsp.config('alpha', {{ {alpha_extra} cmd = {{ 'unused' }}, \
             filetypes = {{ 'rust' }} }})\n\
             btv.lsp.config('beta',  {{ {beta_extra} cmd = {{ 'unused' }}, \
             filetypes = {{ 'rust' }} }})\n\
             btv.lsp.enable({{ 'alpha', 'beta' }})"
        ),
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both completion servers attached"
    );
    exec_lua(
        &rpc,
        "btv.complete.setup { sources = { { 'lsp' } }, min_chars = 1 }",
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
        exec_lua(&rpc, "btv.complete.trigger()").await;
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
    bemtvi_test_harness::barrier(&rpc).await;
    let elapsed = started.elapsed();

    let mut rows = Vec::new();
    for _ in 0..200 {
        exec_lua(&rpc, "btv.complete.trigger()").await;
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

/// The position of `label` among the popup rows, or a panic naming what was there.
fn row_at(rows: &[String], label: &str) -> usize {
    rows.iter()
        .position(|r| r == label)
        .unwrap_or_else(|| panic!("no `{label}` row in {rows:?}"))
}

/// Poll the completion popup until every label in `want` is present, then return the
/// rows in display order. Panics after the window with the last rows seen.
async fn await_all_rows(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &[&str],
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, "btv.complete.trigger()").await;
        if let Some(items) = poll_menu_items(rpc, incoming).await {
            if want.iter().all(|w| items.iter().any(|i| i == w)) {
                return items;
            }
            if !items.is_empty() {
                last = items;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("lsp completion never produced all of {want:?}; last menu rows: {last:?}");
}

/// Equally-good candidates from two servers order by the buffer's **routing order** —
/// the configured `btv.lsp.config{ priority }` — not by which server answered first and
/// not by how the two configs happen to be spelled.
///
/// Regression: the completion merge resolved each row's rank from the buffer's server
/// map in plain **key** order (config name, then root) while claiming to use routing
/// order, so a stated `priority` had no effect on the popup at all. `beta` here is both
/// alphabetically second *and* the slower responder, and must still lead.
#[tokio::test]
async fn completion_rows_order_by_server_priority_not_key_order() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_priority_order");
    // Same shape, same match positions against `pr` — so the two tie on fuzzy score and
    // the merge's source rank is what decides which leads.
    arm_completion_mock(
        dir.as_path(),
        "alpha",
        r#"{ "completion": [ { "label": "pr_one", "insertText": "pr_one" } ] }"#,
    );
    arm_completion_mock(
        dir.as_path(),
        "beta",
        r#"{ "reply_delay_ms": 120,
             "completion": [ { "label": "pr_two", "insertText": "pr_two" } ] }"#,
    );
    let (rpc, mut incoming) =
        start_two_servers_ranked(dir.as_path(), "", "ipr", "", "priority = 10,").await;

    let rows = await_all_rows(&rpc, &mut incoming, &["pr_one", "pr_two"]).await;

    disarm_completion_mocks();
    assert!(
        row_at(&rows, "pr_two") < row_at(&rows, "pr_one"),
        "the higher-`priority` server's candidate leads even though it sorts second by \
         key and answers later, got {rows:?}"
    );
}

/// Among equally-good matches from one server, the popup follows the server's own
/// `sortText` — the field the protocol reserves for exactly this ("relevance here", not
/// "quality of the match"), which is how a server puts a call's parameters above the
/// globals that match the same prefix.
///
/// Regression: `sortText` was parsed and then dropped, so equal-scoring rows fell back
/// to the order the items arrived in — the server's array order, or across servers the
/// reply order, i.e. random from the reader's side.
#[tokio::test]
async fn completion_rows_break_ties_on_the_servers_sort_text() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_sort_text");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
    // Sent worst-first: the array order and the `sortText` order disagree, so only a
    // client that reads `sortText` shows `arg_one` on top.
    let (rpc, mut incoming) = start_typed(
        dir.as_path(),
        r#"[ { "label": "arg_two", "insertText": "arg_two", "sortText": "20" },
             { "label": "arg_six", "insertText": "arg_six", "sortText": "30" },
             { "label": "arg_one", "insertText": "arg_one", "sortText": "10" } ]"#,
        "ar",
    )
    .await;

    let rows = await_all_rows(&rpc, &mut incoming, &["arg_one", "arg_two", "arg_six"]).await;

    assert!(
        row_at(&rows, "arg_one") < row_at(&rows, "arg_two")
            && row_at(&rows, "arg_two") < row_at(&rows, "arg_six"),
        "equal-scoring rows follow the server's `sortText`, not the order its items \
         arrived in, got {rows:?}"
    );
}

// ================================ a cached item's textEdit is only valid for its text
//
// A completion item's `textEdit` range is authored against the buffer AS IT WAS when
// the round was requested. The cache re-serves a complete list while the word grows,
// which is what makes typing feel instant — but the ranges do not grow with it. Accept
// one after the text moved and the replacement is spliced into the MIDDLE of the word:
// `pri` + a `[0,2)` edit leaves `print_value()i`.
//
// The dispatch normally re-requests on the keystroke that changed the text, so the
// cache is refreshed before an accept can see it. An edit that lands WITHOUT a
// dispatch — a settle-order edit from an autocmd, a paste that did not re-arm the
// source — leaves the stale range behind, and the accept path is the last line of
// defence: when the tick has moved it ignores the item's range and falls back to the
// word replacement, which is recomputed against the live text.

#[tokio::test]
async fn accepting_after_an_undispatched_edit_does_not_splice_into_the_word() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_stale_tick");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    // The item's textEdit covers exactly the two typed chars — correct for `pr`,
    // stale for anything longer.
    let completion = r#"[ {
        "label": "print_value",
        "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                 "end": { "line": 0, "character": 2 } },
                      "newText": "print_value()" }
    } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "pr").await;
    await_items(&rpc, &mut incoming, "print_value").await;

    // Grow the word from Lua — a queued buffer write, NOT a keystroke, so the
    // completion dispatch never sees it and the cache keeps its `pr`-era ranges.
    exec_lua(&rpc, r#"btv.buf.set_text(0, 0, 2, 0, 2, { "i" })"#).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["pri"],
        "the undispatched edit landed"
    );

    feed(&rpc, "<C-y>");
    // The item's own text replaces the whole live word. Note it is `print_value`,
    // not the textEdit's `print_value()`: the range is what has gone stale, so the
    // fallback distrusts the edit entirely rather than re-siting its `newText`.
    assert_eq!(
        lines(&rpc).await,
        vec!["print_value"],
        "the accept must replace the WHOLE live word with the item's own text — \
         applying the cached [0,2) range instead splices the textEdit into a word \
         it was never measured against"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// A completion item's `documentation` is honored at the `MarkupKind` the server
/// declared for it, exactly as a hover's contents is: `plaintext` renders verbatim in
/// the docs float instead of being reflowed into one paragraph with its `*`/`_` eaten.
/// The `detail` above it carries no kind — LSP declares one for `documentation` and
/// hover contents, and nothing at all for `detail` — so it is unaffected.
#[tokio::test]
async fn plaintext_completion_docs_render_verbatim() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_complete_docs_plaintext");
    // SAFETY: serialized on `serial_lock`.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );

    let completion = r#"[ { "label": "println", "insertText": "println",
                          "documentation": { "kind": "plaintext",
                            "value": "Options:\n  *args* is positional\n  _kw_ is keyword" } } ]"#;
    let (rpc, mut incoming) = start_typed(&dir, completion, "pr").await;

    await_items(&rpc, &mut incoming, "println").await;
    let docs = await_docs(&rpc, &mut incoming, "Options:").await;
    let body: Vec<&str> = docs
        .iter()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        body,
        vec!["Options:", "  *args* is positional", "  _kw_ is keyword"],
        "plaintext docs keep their line breaks, indentation and literal markers: {docs:?}"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}
