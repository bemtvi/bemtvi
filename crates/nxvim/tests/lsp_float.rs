//! Behavior tests for LSP **hover** and **signature help** rendering through the
//! content float (`nx.ui.float`'s sibling surface). Phase nx.ui.float reroutes
//! these from their old panel / echo placeholders into the cursor-anchored float.
//!
//! Wired exactly like `lsp_complete.rs`: the scripted mock language server
//! (`nxvim --__lsp-mock`, `nxvim_lsp::mock`) answers `textDocument/hover` and
//! `textDocument/signatureHelp`, the `$NXVIM_LSP_CMD` env hook overrides the
//! server's spawn argv, and the buffer is bound via the raw `nx._lsp_start`
//! bridge. The process-global env means these tests serialize on `serial_lock`.

use std::path::Path;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, map_get, serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// Write a mock LSP script (the whole JSON object) and point `$NXVIM_LSP_CMD` at
/// the binary's `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer with `foo` under the cursor, attach, and bind the mock
/// server. Returns the rpc + redraw stream; the caller drives hover / signature.
async fn start(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    // Put the cursor on `foo` (column 4) so a hover request has a symbol; it stays
    // there (no motion) so the reply's cursor-staleness gate passes.
    feed(&rpc, "0fw");
    exec_lua(
        &rpc,
        "nx._lsp_start('mock', { 'placeholder' }, vim.fn.getcwd(), 'rust', \
         vim.api.nvim_get_current_buf(), nil, nil, nil)",
    )
    .await;
    (rpc, incoming)
}

/// The content float's lines on the latest redraw carrying a `float` map, or `None`.
async fn poll_float_lines(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| {
        matches!(map_get(m, "float"), Some(Value::Map(_)))
    })?;
    let Some(Value::Map(float)) = map_get(&map, "float") else {
        return None;
    };
    match map_get(float, "lines") {
        Some(Value::Array(lines)) => Some(
            lines
                .iter()
                .map(|l| l.as_str().unwrap_or("").to_string())
                .collect(),
        ),
        _ => Some(Vec::new()),
    }
}

/// Retry the `trigger` Lua until the content float appears and some line contains
/// `want` (the async server reply takes a moment to land). Panics after the window.
async fn await_float(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    trigger: &str,
    want: &str,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, trigger).await;
        if let Some(lines) = poll_float_lines(rpc, incoming).await {
            if lines.iter().any(|l| l.contains(want)) {
                return lines;
            }
            if !lines.is_empty() {
                last = lines;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the content float never contained {want:?}; last float lines: {last:?}");
}

/// The shipped `examples/ui-float` config wired against a **real**
/// lua-language-server: open its `sample.lua`, press the example's `K` map, and
/// assert a hover content float appears. `#[ignore]`d (needs `lua-language-server`
/// on PATH and ~20s of indexing — not hermetic, per the repo's e2e convention).
/// Run with: `cargo test -p nxvim --test lsp_float -- --ignored example_ui_float`.
#[tokio::test]
#[ignore = "needs lua-language-server on PATH (real e2e, ~30s warmup)"]
async fn example_ui_float_hover_works_against_real_lua_ls() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ui-float")
        .canonicalize()
        .expect("examples/ui-float dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example.clone()],
        file: Some(example.join("sample.lua").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Land the cursor on `string` in `string.format`, then drive the example's `K`
    // hover map, retrying while lua-language-server indexes (it is slow to warm).
    feed(&rpc, "/string.format<CR>");
    let mut last = Vec::new();
    let mut got = None;
    for _ in 0..240 {
        feed(&rpc, "K");
        if let Some(lines) = poll_float_lines(&rpc, &mut incoming).await {
            if lines.iter().any(|l| l.contains("string")) {
                got = Some(lines);
                break;
            }
            if !lines.is_empty() {
                last = lines;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let lines = got.unwrap_or_else(|| {
        panic!("real lua_ls hover never opened a float mentioning `string`; last: {last:?}")
    });
    assert!(
        lines.iter().any(|l| l.contains("string")),
        "expected a real hover float, got {lines:?}"
    );
}

#[tokio::test]
async fn hover_reply_opens_the_content_float() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_hover");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let lines = await_float(&rpc, &mut incoming, "nx.lsp.hover()", "scripted hover").await;
    assert!(
        lines.iter().any(|l| l.contains("foo")),
        "hover float should carry the markup, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn signature_help_reply_opens_the_content_float() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_sig");
    arm_mock(
        &dir,
        r#"{ "signature_help": { "signatures": [
             { "label": "fn foo(a: i32, b: i32)",
               "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ] } ],
             "activeSignature": 0, "activeParameter": 0 } }"#,
    );
    let (rpc, mut incoming) = start(&dir).await;

    let lines = await_float(
        &rpc,
        &mut incoming,
        "nx.lsp.signature_help()",
        "fn foo(a: i32, b: i32)",
    )
    .await;
    // The active parameter is appended in brackets (the float renders plain lines).
    assert!(
        lines.iter().any(|l| l.contains("[a: i32]")),
        "signature float should mark the active parameter, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

#[tokio::test]
async fn empty_hover_echoes_instead_of_an_empty_float() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_float_empty");
    // No `hover` field ⇒ the mock returns null ⇒ a brief message, no float.
    arm_mock(&dir, r#"{ }"#);
    let (rpc, mut incoming) = start(&dir).await;

    // Drive hover until the server has attached and answered (its empty reply
    // echoes "No hover information"); the float must never open along the way. The
    // transient "No language server attached" startup message is skipped past.
    let mut saw_empty_hover = false;
    let mut last_message = String::new();
    for _ in 0..200 {
        exec_lua(&rpc, "nx.lsp.hover()").await;
        nxvim_test_harness::barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |_| true) {
            assert!(
                !matches!(map_get(&map, "float"), Some(Value::Map(_))),
                "an empty hover must not open a float"
            );
            if let Some(m) = map_get(&map, "message").and_then(Value::as_str) {
                if !m.is_empty() {
                    last_message = m.to_string();
                }
                if m.contains("hover information") {
                    saw_empty_hover = true;
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        saw_empty_hover,
        "expected the empty-hover message, last saw {last_message:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}
