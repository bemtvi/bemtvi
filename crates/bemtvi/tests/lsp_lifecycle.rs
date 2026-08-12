//! Stopping a server runs its whole **exit path**, not just its process.
//!
//! `btv.lsp.stop` / `btv.lsp.restart` kill the server synchronously, but the exit is
//! reported back asynchronously as an `Exited` event — and everything the editor owes
//! a departing server hangs off that event: the config's `on_exit(code, signal,
//! client)`, the `LspDetach` autocmd for each buffer it served, and dropping its handle
//! from `btv.lsp.clients()`.
//!
//! The leaked handle is the one that rots quietly. `:LspStop` with no argument derives
//! its list from `btv.lsp.clients()`, so a phantom makes the next `:LspStop` report that
//! it stopped something already gone; `:LspInfo`, which reads the engine directly, says
//! the opposite; and a plugin gating on "is a server attached here" sees one that
//! cannot answer a single request.
//!
//! Wired like `lsp_client_api.rs`: the scripted mock stands in for a real server (a
//! server that never completes `initialize` is never mirrored at all, so a dumb script
//! could not test this), and the observable is what the Lua side reports afterwards.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, serial_lock, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

/// Point `$BEMTVI_LSP_CMD` at the binary's mock mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path) {
    std::fs::write(dir.join("mock.json"), "{}").expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a rust buffer, record what the lifecycle hooks see, and bring up one `demo`
/// server on it. Returns once its handle is mirrored, so "gone afterwards" is a
/// statement about a handle that really existed.
async fn start_demo(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let x = 1\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    exec_lua(&rpc, "_G.exits = {} _G.detached = {}").await;
    exec_lua(
        &rpc,
        "btv.autocmd.create('LspDetach', { callback = function(a) \
           _G.detached[#_G.detached + 1] = tostring(a.data and a.data.client_id or '?') end })",
    )
    .await;
    exec_lua(
        &rpc,
        "btv.lsp.config('demo', { cmd = { 'unused' }, filetypes = { 'rust' }, \
           on_exit = function(_code, _signal, client) \
             _G.exits[#_G.exits + 1] = tostring(client and client.name or '?') end }) \
         btv.lsp.enable('demo')",
    )
    .await;
    eventually(&rpc, "#btv.lsp.clients()", 1, "the demo server came up").await;
    (rpc, incoming)
}

/// Poll `expr` until it reports `want`, failing with what it last said.
async fn eventually(rpc: &Rpc, expr: &str, want: i64, what: &str) {
    let code = format!("return {expr}");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut got = -1;
    loop {
        got = exec_lua(rpc, &code).await.as_i64().unwrap_or(got);
        if got == want {
            return;
        }
        if std::time::Instant::now() >= deadline {
            std::env::remove_var("BEMTVI_LSP_CMD");
            panic!("{what}: expected {expr} == {want}, still {got}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn stop_runs_the_exit_path_and_forgets_the_client() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-stop-exitpath");
    arm_mock(dir.as_path());
    let (rpc, _incoming) = start_demo(dir.as_path()).await;

    exec_lua(&rpc, "btv.lsp.stop('demo')").await;

    eventually(&rpc, "#btv.lsp.clients()", 0, "the handle is dropped").await;
    eventually(&rpc, "#_G.exits", 1, "on_exit ran").await;
    eventually(&rpc, "#_G.detached", 1, "LspDetach fired").await;
    let named = exec_lua(&rpc, "return _G.exits[1]").await;
    std::env::remove_var("BEMTVI_LSP_CMD");
    assert_eq!(
        named.as_str(),
        Some("demo"),
        "on_exit received the handle of the server it was configured for"
    );
}

#[tokio::test]
async fn restart_forgets_the_client_it_replaced() {
    // A restart is a stop plus a start, and the stop half owes the same exit path.
    // Without it the old id accumulates beside the new one, so a buffer served by one
    // server reports two — and one of them is dead.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-restart-exitpath");
    arm_mock(dir.as_path());
    let (rpc, _incoming) = start_demo(dir.as_path()).await;

    exec_lua(&rpc, "btv.lsp.restart('demo')").await;

    eventually(&rpc, "#_G.exits", 1, "on_exit ran for the replaced process").await;
    eventually(&rpc, "#btv.lsp.clients()", 1, "exactly one live handle").await;
    std::env::remove_var("BEMTVI_LSP_CMD");
}
