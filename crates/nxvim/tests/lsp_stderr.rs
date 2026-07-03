//! A language server's stderr is *not* guaranteed UTF-8 (binary logging, raw
//! panic dumps). The manager drains stderr for the server's whole life — but a
//! drain that reads *lines as `String`s* errors on the first invalid-UTF-8 line
//! and stops, dropping the pipe's read end. The server's next stderr write then
//! kills it (SIGPIPE for a C server, an `eprintln!` panic for a Rust one) — a
//! crashed language server from junk on a channel that is purely diagnostic.
//!
//! Wired like `lsp_config.rs`: the scripted mock (`nxvim --__lsp-mock`) stands in
//! for the server via `$NXVIM_LSP_CMD` (process-global env ⇒ `serial_lock`). The
//! mock's `stderr_noise` field floods stderr with 0xFF-filled lines — several
//! times the 64 KiB pipe capacity — before it starts serving, and (faithful to a
//! real server) dies if a stderr write fails; so the handshake completes only if
//! the client keeps draining stderr through the invalid bytes.

use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, map_get, serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

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

/// Open a `.rs` buffer (filetype `rust`, `foo` under the cursor) and attach.
async fn open_rust(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    // Cursor on `foo` so the hover request has a symbol under it.
    feed(&rpc, "0fw");
    (rpc, incoming)
}

/// The first floating *window* (`windows[]` with `floating == true`) in a redraw —
/// the hover doc float — or `None`. (Mirrors the helper in `lsp_config.rs`.)
fn floating_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let windows = map_get(map, "windows")?.as_array()?;
    windows
        .iter()
        .filter_map(Value::as_map)
        .find(|w| map_get(w, "floating").and_then(Value::as_bool) == Some(true))
        .cloned()
}

/// A float window's rendered text rows (the redraw `lines` array — plain strings).
fn window_lines(win: &[(Value, Value)]) -> Vec<String> {
    match map_get(win, "lines") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|r| r.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Retry `nx.lsp.hover()` until the hover float carries `want`, or panic after the
/// window — a server wedged on its stderr pipe never gets there.
async fn await_hover_float(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, "nx.lsp.hover()").await;
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()) {
            if let Some(win) = floating_window(&map) {
                let lines = window_lines(&win);
                if lines.iter().any(|l| l.contains(want)) {
                    return lines;
                }
                if !lines.is_empty() {
                    last = lines;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "the hover float never contained {want:?} — the server never came up \
         (killed by its own stderr once the drain stopped?); last float lines: {last:?}"
    );
}

/// A server that floods stderr with invalid-UTF-8 junk (4× the pipe capacity)
/// before serving must still initialize and answer hover: the manager's stderr
/// drain has to keep consuming *through* undecodable bytes. A drain that dies on
/// the first invalid line abandons the pipe, the server is killed by its own next
/// stderr write — `initialize` is never answered and hover never appears.
#[tokio::test]
async fn server_with_non_utf8_stderr_flood_still_initializes_and_answers() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_stderr_noise");
    // Keep the drained-noise WARN lines out of the user's real state dir.
    std::env::set_var("NXVIM_LSP_LOG_FILE", dir.join("lsp.log"));
    // 256 KiB of 0xFF lines — four times the default 64 KiB pipe, so the mock
    // reliably blocks unless the client drains.
    arm_mock(
        &dir,
        r#"{ "stderr_noise": 262144,
             "hover": { "contents": { "kind": "markdown",
               "value": "`foo`: hover despite stderr noise" } } }"#,
    );
    let (rpc, mut incoming) = open_rust(&dir).await;

    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_hover_float(&rpc, &mut incoming, "hover despite stderr noise").await;
    assert!(
        lines.iter().any(|l| l.contains("foo")),
        "hover float should carry the markup, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
    std::env::remove_var("NXVIM_LSP_LOG_FILE");
}
