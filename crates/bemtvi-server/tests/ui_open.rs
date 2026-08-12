//! Behavior tests for `btv.ui.open` (alias `vim.ui.open`) — hand a file path or
//! URL to the OS opener. The opener argv prefix is chosen by platform in Rust
//! (`btv._ui_opener`: `open` / `explorer` / `xdg-open`); the Lua wrapper appends
//! the uri and runs it off-tick through `btv.run` (the promise-only process API).
//!
//! Black-box like the rest: a real server over RPC, driven with `nvim_exec_lua`.
//! To stay hermetic we DON'T launch a real browser/opener — the test overrides
//! `btv._ui_opener` with a `sh` command that records the uri it was handed into a
//! temp file, then asserts the recorded uri (proof the wrapper appends the uri to
//! the opener argv and spawns it). The spawn is off-tick, so we use the two-barrier
//! shape from async_runtime.rs: assert pending, sleep, then assert the effect.

use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, lua_bool, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Override `btv._ui_opener` so the "opener" is `sh -c 'printf %s "$1" > out' sh`.
/// The wrapper appends the uri as the next argv element, which becomes `$1`, so
/// the recorded file ends up holding exactly the uri the opener received.
fn record_opener_into(out: &std::path::Path) -> String {
    format!(
        "btv._ui_opener = function() return {{ 'sh', '-c', 'printf %s \"$1\" > {}', 'sh' }} end",
        out.display()
    )
}

#[tokio::test]
async fn btv_ui_open_runs_the_platform_opener_with_the_uri() {
    let (rpc, _incoming) = start().await;
    let out = temp_dir("ui_open").join("opened");
    let uri = "https://example.com/page?q=1";

    exec_lua(&rpc, &record_opener_into(&out)).await;
    exec_lua(&rpc, &format!("btv.ui.open('{uri}')")).await;
    // Barrier #1: the child runs off-tick, so nothing has been written inline.
    assert!(!out.exists(), "opener ran inline (should be off-tick)");

    // Past the run: the opener received the uri as its trailing argument.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let recorded = std::fs::read_to_string(&out).expect("opener wrote the uri");
    assert_eq!(recorded, uri);
}

#[tokio::test]
async fn btv_ui_open_returns_a_promise_of_the_exit_result() {
    let (rpc, _incoming) = start().await;
    // btv.ui.open is promise-only (the btv.run shape): it resolves to { code, ... }.
    // Use `true` as the opener so the exit code is a clean 0.
    exec_lua(&rpc, "btv._ui_opener = function() return { 'true' } end").await;
    exec_lua(
        &rpc,
        "_G.res = nil\n\
         btv.ui.open('whatever'):next(function(r) _G.res = r end)",
    )
    .await;
    // Pending inline (off-tick).
    assert_eq!(lua_bool(&rpc, "return _G.res == nil").await, Some(true));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(lua_bool(&rpc, "return _G.res.code == 0").await, Some(true));
}

#[tokio::test]
async fn btv_ui_open_rejects_a_non_string_uri() {
    let (rpc, _incoming) = start().await;
    // Fail loud (ADR / project convention): a non-string uri raises, not no-ops.
    let msg = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.ui.open, 123); return tostring(ok) .. '|' .. tostring(err)",
    )
    .await;
    let msg = msg.as_str().unwrap_or("");
    assert!(
        msg.starts_with("false|"),
        "expected a raised error, got {msg}"
    );
    assert!(
        msg.contains("uri must be a string"),
        "expected the fail-loud message, got {msg}"
    );
}

#[tokio::test]
async fn vim_ui_open_alias_runs_the_opener_too() {
    let (rpc, _incoming) = start().await;
    let out = temp_dir("ui_open").join("opened_alias");
    let uri = "file:///tmp/doc.txt";

    exec_lua(&rpc, &record_opener_into(&out)).await;
    // The neovim muscle-memory alias must drive the same path.
    assert_eq!(
        lua_bool(&rpc, "return type(vim.ui.open) == 'function'").await,
        Some(true)
    );
    exec_lua(&rpc, &format!("vim.ui.open('{uri}')")).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let recorded = std::fs::read_to_string(&out).expect("alias opener wrote the uri");
    assert_eq!(recorded, uri);
}
