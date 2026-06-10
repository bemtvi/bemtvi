//! The daemon wire protocol, process half (edit-host split, Phase 3 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Proves the centerpiece of the native split end-to-end over a real wire: an
//! editor whose [`HostProc`](nxvim_server::HostProc) is a
//! [`RemoteHostProc`](nxvim_server::RemoteHostProc) forwards its `vim.system`
//! spawns over an in-process `tokio::io::duplex` to a
//! [`serve_daemon`](nxvim_server::serve_daemon) running the children, and the
//! results stream back. The duplex stands in for the eventual ssh stdio to
//! `nxvim --daemon`; the protocol is transport-agnostic.
//!
//! Faithful, not a no-op: the daemon spawns a **real** `sh` and the editor's
//! `on_exit` sees that process's *actual* stdout / exit code — output a stub could
//! not invent. Two concurrent spawns each see their own result (proving the wire's
//! per-`id` demux routes correctly, not a shared constant), and a killed child's
//! `on_exit` fires with `code = -1` (proving `proc_kill` crosses the wire and the
//! daemon terminates the child).
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, driven by
//! `nvim_exec_lua`, asserting on observable Lua state.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostProc, ServerInit};
use nxvim_test_harness::{exec_lua, lua_u64, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server whose process host is a [`RemoteHostProc`] talking to a
/// [`serve_daemon`] over an in-process duplex, UI-attached. Both the daemon task and
/// the remote host's RPC tasks live on this (the test's) runtime — the same split
/// the harness already makes between the test-runtime client and the server's own
/// thread — while the server runs on its own thread and reaches the daemon only
/// through the injected host. The notification receiver is returned (not dropped):
/// an `on_exit` firing off-tick triggers a redraw, and a dropped receiver would
/// close the client connection.
async fn start_with_daemon() -> (Rpc, UnboundedReceiver<Incoming>) {
    // The wire: edit-host end ↔ daemon end.
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_daemon(daemon_reader, daemon_writer).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteHostProc::connect(host_reader, host_writer);
    let init = ServerInit {
        host_proc: Some(Box::new(remote)),
        ..Default::default()
    };
    start_attached(init, 80, 24).await
}

/// An async `vim.system` runs on the daemon and its real stdout / exit code come
/// back over the wire — a stub couldn't produce the live `sh` output.
#[tokio::test]
async fn vim_system_runs_on_the_daemon_over_the_wire() {
    let (rpc, _incoming) = start_with_daemon().await;

    exec_lua(
        &rpc,
        "_G.code = nil\n\
         _G.out = nil\n\
         vim.system({ 'sh', '-c', 'printf hello-from-daemon' }, {}, function(r)\n\
           _G.code = r.code\n\
           _G.out = r.stdout\n\
         end)",
    )
    .await;
    // Give the wire round-trip + the real child time to run and `on_exit` to fire.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        lua_u64(&rpc, "return _G.code").await,
        Some(0),
        "a real `sh` ran on the daemon and exited 0"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.out").await.as_str(),
        Some("hello-from-daemon"),
        "on_exit must see the child's actual stdout, carried back over the wire"
    );
}

/// Two concurrent spawns each see their *own* result: the wire correlates replies to
/// the right spawn by `id`, not a shared last-write-wins constant.
#[tokio::test]
async fn concurrent_spawns_are_demuxed_by_id() {
    let (rpc, _incoming) = start_with_daemon().await;

    exec_lua(
        &rpc,
        "_G.a = nil\n\
         _G.b = nil\n\
         vim.system({ 'sh', '-c', 'printf AAA' }, {}, function(r) _G.a = r.stdout end)\n\
         vim.system({ 'sh', '-c', 'printf BBB' }, {}, function(r) _G.b = r.stdout end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(exec_lua(&rpc, "return _G.a").await.as_str(), Some("AAA"));
    assert_eq!(exec_lua(&rpc, "return _G.b").await.as_str(), Some("BBB"));
}

/// A non-zero exit status round-trips faithfully — the daemon reports the child's
/// real code, not a flattened success.
#[tokio::test]
async fn exit_code_round_trips() {
    let (rpc, _incoming) = start_with_daemon().await;

    exec_lua(
        &rpc,
        "_G.code = nil\n\
         vim.system({ 'sh', '-c', 'exit 7' }, {}, function(r) _G.code = r.code end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        lua_u64(&rpc, "return _G.code").await,
        Some(7),
        "the child's real exit status 7 must survive the wire"
    );
}

/// Killing a long-running child crosses the wire (`proc_kill`): the daemon
/// terminates the process and reports the kill exit (`code = -1`), so `on_exit`
/// fires promptly instead of waiting out the child's natural lifetime.
#[tokio::test]
async fn kill_crosses_the_wire() {
    let (rpc, _incoming) = start_with_daemon().await;

    // A child that would sleep far longer than the test's wait window, so a fired
    // `on_exit` can only mean the kill reached and terminated it.
    exec_lua(
        &rpc,
        "_G.code = nil\n\
         _G.handle = vim.system({ 'sh', '-c', 'sleep 30' }, {}, function(r) _G.code = r.code end)",
    )
    .await;
    // Let the spawn land on the daemon (the handle needs a real pid before a kill
    // can target it), then signal the kill over the wire.
    tokio::time::sleep(Duration::from_millis(150)).await;
    exec_lua(&rpc, "_G.handle:kill()").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        exec_lua(&rpc, "return _G.code").await.as_i64(),
        Some(-1),
        "a killed child's on_exit must fire with code -1, well before its 30s sleep"
    );
}
