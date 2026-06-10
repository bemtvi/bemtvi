//! The `HostProc` injection seam (edit-host split, Phase 3b of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Proves the server spawns child processes — the async `vim.system` /
//! `jobstart` path — through the [`HostProc`](nxvim_server::HostProc) handed to it
//! in [`ServerInit::host_proc`], not `tokio::process` directly. A fake that never
//! touches the OS both **records** the argv it was asked to run and **serves** a
//! result the editor's `on_exit` then observes, so a later remote/daemon backend
//! can swap in at exactly this seam: editing stays local, processes run through the
//! injected host.
//!
//! Faithful, not a no-op: the fake echoes back the *actual* argv it received as
//! stdout, and the test runs a program name that exists on no PATH — a real spawn
//! would fail with `code = -1`, so observing `code = 0` and the echoed argv proves
//! the injected host intercepted the spawn and that bytes round-trip through it.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, driven by
//! `nvim_exec_lua`, asserting on observable Lua state and on what the fake observed.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{HostProc, ProcEvents, ProcSpec, ServerInit};
use nxvim_test_harness::{exec_lua, lua_u64, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;

/// An in-memory [`HostProc`]: it records every argv it is asked to run (behind a
/// shared `Arc<Mutex<…>>` the test thread inspects) and reports a synthetic result
/// — `code = 0` with the argv echoed back as stdout — without spawning anything.
/// `Send + Sync` (so it can ride [`ServerInit`] onto the server thread and be
/// shared across spawns) without being a real process host.
#[derive(Clone, Default)]
struct FakeProc {
    spawns: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeProc {
    /// The argv lists the server has asked this host to run, in order.
    fn recorded(&self) -> Vec<Vec<String>> {
        self.spawns.lock().unwrap().clone()
    }
}

impl HostProc for FakeProc {
    fn run(
        &self,
        spec: ProcSpec,
        _kill: oneshot::Receiver<()>,
        events: ProcEvents,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let spawns = self.spawns.clone();
        Box::pin(async move {
            spawns.lock().unwrap().push(spec.argv.clone());
            // A distinctive pid no real spawn on this host would mint, then a
            // result *derived from the input* (the echoed argv) — proof the fake
            // reacted to what it was given, not a canned constant.
            events.spawned(Some(424_242));
            let echoed = spec.argv.join(" ").into_bytes();
            events.exited(0, echoed, Vec::new());
        })
    }
}

/// Start a server (its own thread, timers enabled so the event-loop actor runs)
/// with `fake` as the process host, UI-attached. The notification receiver is
/// returned (not dropped): once `on_exit` fires off-tick it triggers a redraw, and
/// a dropped receiver would kill the client's reader task and close the connection.
async fn start_with(fake: FakeProc) -> (Rpc, UnboundedReceiver<Incoming>) {
    let init = ServerInit {
        host_proc: Some(Box::new(fake)),
        ..Default::default()
    };
    start_attached(init, 80, 24).await
}

/// Async `vim.system` runs through the injected host: its `on_exit` sees the
/// result the fake served, for a program that exists on no PATH (a real spawn
/// would give `code = -1`).
#[tokio::test]
async fn async_vim_system_spawns_through_the_injected_host_proc() {
    let fake = FakeProc::default();
    let (rpc, _incoming) = start_with(fake.clone()).await;

    exec_lua(
        &rpc,
        "_G.code = nil\n\
         _G.out = nil\n\
         vim.system({ 'nxvim-no-such-binary', 'hello', 'world' }, {}, function(r)\n\
           _G.code = r.code\n\
           _G.out = r.stdout\n\
         end)",
    )
    .await;
    // Let the actor drive the fake and on_exit fire with the served result. (That
    // it runs *off-tick* is covered by `async_runtime`; this test proves *which*
    // host ran it, so it just waits for the result rather than racing a barrier.)
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        lua_u64(&rpc, "return _G.code").await,
        Some(0),
        "the fake host served code 0 — a real spawn of a missing binary would be -1"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.out").await.as_str(),
        Some("nxvim-no-such-binary hello world"),
        "on_exit must see the argv the fake echoed back — bytes round-trip the seam"
    );

    assert_eq!(
        fake.recorded(),
        vec![vec![
            "nxvim-no-such-binary".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ]],
        "the injected host must have been asked to run the exact argv"
    );
}

/// Each spawn reaches the host distinctly — two `vim.system` calls record two
/// argvs and each `on_exit` sees its *own* echoed result (the fake reacts to input,
/// it doesn't return a shared constant).
#[tokio::test]
async fn each_spawn_reaches_the_host_with_its_own_argv() {
    let fake = FakeProc::default();
    let (rpc, _incoming) = start_with(fake.clone()).await;

    exec_lua(
        &rpc,
        "_G.a = nil\n\
         _G.b = nil\n\
         vim.system({ 'first', 'one' }, {}, function(r) _G.a = r.stdout end)\n\
         vim.system({ 'second' }, {}, function(r) _G.b = r.stdout end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        exec_lua(&rpc, "return _G.a").await.as_str(),
        Some("first one")
    );
    assert_eq!(exec_lua(&rpc, "return _G.b").await.as_str(), Some("second"));

    assert_eq!(
        fake.recorded(),
        vec![
            vec!["first".to_string(), "one".to_string()],
            vec!["second".to_string()],
        ],
        "both spawns must reach the host, each with its own argv"
    );
}
