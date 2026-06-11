//! The daemon wire protocol — the **blocking `vim.system` shell-out** leg (edit-host
//! split, Phase 3 / Open Decision #5's *residual* blocking-bridge note in
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_proc.rs` (the *async* `vim.system` / `jobstart` over `HostProc`).
//! Here a real editor whose blocking-system backend is a
//! [`RemoteBlockingSystem`](nxvim_server::RemoteBlockingSystem) talking to a
//! [`serve_sys_daemon`](nxvim_server::serve_sys_daemon) over an in-process duplex runs a
//! **synchronous** `vim.system(...):wait()` — the shape an `lsp/<server>.lua` `root_dir`
//! uses (`cargo metadata`) — and the contract holds:
//!
//! - The shell-out runs **on the daemon**, not the edit-host's local machine: the call
//!   reaches a tool that is not on the local `PATH` yet returns `code = 0` with the daemon
//!   fake's output — a result a real local spawn could not produce (it would be `-1`,
//!   "failed to spawn"). So the spawn was intercepted across the wire.
//! - The exact `argv`, `cwd`, and `env` cross the wire faithfully (the fake echoes them
//!   back; two distinct calls echo distinctly — it reacts to input, not a canned constant).
//! - The call stays **synchronous**: `:wait()` returns the already-complete result inline,
//!   even though the work happened on a separate daemon thread (the blocking bridge parks
//!   the editor thread on the reply, with the wire's RPC tasks on their own thread so that
//!   park can't deadlock).
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on the
//! `vim.system(...):wait()` result table the daemon produced.

use nxvim_lua::{BlockingSystem, SystemOutput, SystemSpec};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteBlockingSystem, ServerInit};
use nxvim_test_harness::{attach, exec_lua, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// The **daemon-side** blocking-system backend: it does *not* spawn anything — it echoes
/// the request back so a test can prove the spec crossed the wire intact. `stdout` is the
/// argv joined by spaces (output a real local spawn of a not-on-`PATH` tool could never
/// produce — so observing it proves the daemon intercepted the call by reacting to the
/// *actual* argv), `stderr` carries the cwd + env, and `pid` is a sentinel a local spawn
/// would never mint. Faithful, not a no-op: every field is derived from the input.
struct EchoSystem;

impl BlockingSystem for EchoSystem {
    fn run(&self, spec: SystemSpec) -> SystemOutput {
        let env = spec
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        SystemOutput {
            code: 0,
            stdout: spec.cmd.join(" ").into_bytes(),
            stderr: format!("cwd={};env={}", spec.cwd.unwrap_or_default(), env).into_bytes(),
            pid: Some(4242),
        }
    }
}

/// Start a server whose blocking-system backend is a [`RemoteBlockingSystem`] talking to a
/// `serve_sys_daemon` (backed by [`EchoSystem`]) over an in-process duplex. UI-attached.
/// Returns the client RPC handle *and* its notification receiver — kept, not dropped:
/// dropping the receiver would tear the client connection down and stop the server.
async fn spawn_with_daemon_sys() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_sys_daemon(daemon_reader, daemon_writer, Box::new(EchoSystem))
            .await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteBlockingSystem::connect(host_reader, host_writer);
    let init = ServerInit {
        blocking_system: Some(Box::new(remote)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// `vim.system({...}):wait()` runs the shell-out **on the daemon** and returns its result
/// inline. The tool name (`daemon-only-tool`) is not on the edit-host's `PATH`, so a real
/// *local* spawn would fail with `code = -1`; observing `code = 0` plus the echoed argv and
/// the sentinel pid proves the spawn was intercepted across the wire (the faithfulness
/// argument the rest of the daemon suite makes for `/virtual/...` paths).
#[tokio::test]
async fn vim_system_wait_runs_the_shellout_on_the_daemon() {
    let (rpc, _incoming) = spawn_with_daemon_sys().await;

    let result = exec_lua(
        &rpc,
        r#"
        local r = vim.system({ "daemon-only-tool", "--flag", "value" }):wait()
        return { r.code, r.stdout, r.pid }
        "#,
    )
    .await;

    let a = result.as_array().expect("result is a table");
    assert_eq!(a[0].as_i64(), Some(0), "exit code came back over the wire");
    assert_eq!(
        a[1].as_str(),
        Some("daemon-only-tool --flag value"),
        "the daemon echoed the exact argv it received — it crossed the wire faithfully"
    );
    assert_eq!(
        a[2].as_u64(),
        Some(4242),
        "the daemon's sentinel pid — a local spawn would never mint it"
    );
}

/// The `cwd` and `env` opts cross the wire intact (the daemon fake echoes them on stderr).
#[tokio::test]
async fn vim_system_forwards_cwd_and_env_over_the_wire() {
    let (rpc, _incoming) = spawn_with_daemon_sys().await;

    let stderr = exec_lua(
        &rpc,
        r#"
        local r = vim.system(
          { "tool" },
          { cwd = "/remote/project", env = { RUSTUP_TOOLCHAIN = "stable" } }
        ):wait()
        return r.stderr
        "#,
    )
    .await;

    assert_eq!(
        stderr.as_str(),
        Some("cwd=/remote/project;env=RUSTUP_TOOLCHAIN=stable"),
        "cwd and env reached the daemon over the wire"
    );
}

/// Two distinct calls echo distinctly — the bridge relays each call's own argv, not a
/// shared/canned constant (the "reacts to input" guard against a faithful-looking no-op).
#[tokio::test]
async fn each_call_relays_its_own_argv() {
    let (rpc, _incoming) = spawn_with_daemon_sys().await;

    let first = exec_lua(&rpc, r#"return vim.system({ "alpha", "1" }):wait().stdout"#).await;
    let second = exec_lua(&rpc, r#"return vim.system({ "beta", "2" }):wait().stdout"#).await;

    assert_eq!(first.as_str(), Some("alpha 1"));
    assert_eq!(second.as_str(), Some("beta 2"));
}
