//! Multi-server LSP over the **daemon wire** — Phase 6 of
//! `docs/plans/2026-07-25-multi-server-lsp-attach.md`.
//!
//! The remote session is a tier-1 target: a buffer served by two language servers
//! has to behave identically whether their stdio is a local pipe or a tunnel to a
//! daemon. That is *plausible* by construction — the whole multi-server layer
//! (`LspDocState.servers`, the per-server pending map, the merged mirrors) lives in
//! `EditHost`, and both transports are already keyed by `ServerKey` — but "the design
//! says it should" is not a verification, so these drive it.
//!
//! Wiring: [`spawn_with_daemon_lsp`] injects a `RemoteLspTransport` talking to a
//! `serve_lsp_daemon` over an in-process duplex, so each mock server is a real child
//! held by the daemon side with its stdio streamed over the wire. `$NXVIM_LSP_CMD_
//! <NAME>` points the two servers at different scripts (the blanket `$NXVIM_LSP_CMD`
//! would aim both at one, and no assertion could tell which answered). The env is
//! process-global, so these serialize on `serial_lock`.

use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, serial_lock, spawn_with_daemon_lsp, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// Point `$NXVIM_LSP_CMD_<NAME>` at the mock with its own script.
fn arm_mock_named(dir: &Path, name: &str, script: &str) {
    let file = dir.join(format!("mock-{name}.json"));
    std::fs::write(&file, script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        format!("NXVIM_LSP_CMD_{}", name.to_uppercase()),
        format!("{NXVIM_BIN} --__lsp-mock {}", file.display()),
    );
}

fn disarm_mocks() {
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// Poll `expr` until it contains `want`; returns the last value seen.
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

/// Open a `.rs` buffer in a daemon-LSP session and enable both mock servers.
async fn start_two_over_daemon(dir: &Path, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let (rpc, incoming) = spawn_with_daemon_lsp(ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    })
    .await;
    // Cursor on `foo` so a hover has a symbol under it.
    feed(&rpc, "0fw");
    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    (rpc, incoming)
}

#[tokio::test]
async fn two_servers_attach_and_publish_over_the_daemon_wire() {
    // Both servers spawn on the daemon, both receive `didOpen` over the tunnel, and
    // both pushes land merged in the editor's diagnostic state. `publishDiagnostics`
    // is the sharpest probe available: it is a SERVER→client push that only a real
    // `didOpen` reaching that server can trigger, so seeing both messages proves each
    // server holds the document — not merely that two children were spawned.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("daemon-lsp-two");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "diagnostics": [ { "range": { "start": { "line": 0, "character": 4 },
                                           "end": { "line": 0, "character": 7 } },
                               "severity": 1, "message": "diag-from-alpha" } ] }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "diagnostics": [ { "range": { "start": { "line": 0, "character": 10 },
                                           "end": { "line": 0, "character": 13 } },
                               "severity": 2, "message": "diag-from-beta" } ] }"#,
    );
    let (rpc, _incoming) = start_two_over_daemon(dir.as_path(), "let foo = bar()\n").await;

    let attached = await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await;
    let msgs = await_lua_contains(
        &rpc,
        "(function()\n\
         \x20 local out = {}\n\
         \x20 for _, d in ipairs(nx.diagnostic.get(0) or {}) do out[#out+1] = d.message end\n\
         \x20 table.sort(out)\n\
         \x20 return table.concat(out, ',')\n\
         end)()",
        "diag-from-beta",
    )
    .await;

    disarm_mocks();
    assert!(attached, "both servers attached over the daemon wire");
    assert_eq!(
        msgs, "diag-from-alpha,diag-from-beta",
        "both servers' pushed diagnostics merge in the editor's state over the wire"
    );
}

#[tokio::test]
async fn a_request_routes_by_capability_over_the_daemon_wire() {
    // The capability routing (Phase 3a) has to survive the tunnel: `alpha` sorts first
    // but withholds `hoverProvider`, so the hover must reach `beta`. A reply decoded
    // from the wrong server's tunnel would answer nothing (alpha's hover is scripted
    // but never asked for), so the assertion distinguishes the two.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("daemon-lsp-route");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "capabilities": { "hoverProvider": false },
             "hover": { "contents": "FROM-ALPHA" } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "hover": { "contents": "FROM-BETA" } }"#,
    );
    let (rpc, _incoming) = start_two_over_daemon(dir.as_path(), "let foo = bar()\n").await;

    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached over the daemon wire"
    );

    // `nx.lsp.hover()` resolves with the reply's markup; read it off the promise
    // rather than the float so this stays a pure wire assertion.
    exec_lua(
        &rpc,
        "nx._daemon_hover = nil\n\
         nx.lsp.hover():next(function(r) nx._daemon_hover = tostring(r) end)",
    )
    .await;
    let hover = await_lua_contains(&rpc, "nx._daemon_hover", "FROM-BETA").await;

    disarm_mocks();
    assert!(
        hover.contains("FROM-BETA"),
        "the hover reached the server that advertises it, over the wire: {hover:?}"
    );
    assert!(
        !hover.contains("FROM-ALPHA"),
        "and not the first server, which withholds hoverProvider: {hover:?}"
    );
}
