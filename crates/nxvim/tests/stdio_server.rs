//! Black-box test of `nxvim --server` — the binary's **headless** role, speaking
//! nxvim's msgpack-RPC over stdin/stdout. This is the transport a remote SSH
//! client drives: `ssh host nxvim --server` execs this, and the local client
//! talks to it through the ssh pipe.
//!
//! It spawns the *real* compiled binary (`CARGO_BIN_EXE_nxvim`) with `--server`
//! and piped stdio, then connects [`nxvim_rpc`] to the child's stdout(read) +
//! stdin(write) and drives it with the shared harness helpers — the whole remote
//! mechanism minus the network hop (see
//! `docs/plans/2026-06-09-remote-ssh-client.md`).

use std::process::Stdio;

use nxvim_rpc::{connect, Incoming};
use nxvim_test_harness::{attach, feed, lines, temp_dir, write_temp};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedReceiver;

/// Spawn `nxvim --server [file]` with a hermetic config (no user `init.lua` /
/// plugins) and connect to its stdio. Returns the live child *and* the `incoming`
/// receiver — the caller must keep both alive: dropping the child closes the pipe
/// (`kill_on_drop` reaps it), and dropping `incoming` would make the reader task
/// tear the connection down on the server's first `redraw` notification.
fn spawn_headless(file: Option<&str>) -> (nxvim_rpc::Rpc, UnboundedReceiver<Incoming>, Child) {
    // Point the config dir at a fresh empty dir so `default_runtime` finds no
    // `init.lua` to source — the server starts on a bare `[No Name]` buffer.
    let cfg = temp_dir("headless-cfg");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nxvim"));
    cmd.arg("--server")
        .env("NXVIM_CONFIG", &cfg)
        .env_remove("NXVIM_RUNTIMEPATH")
        .env_remove("XDG_CONFIG_HOME");
    if let Some(file) = file {
        cmd.arg(file);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nxvim --server");
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let (rpc, incoming) = connect(stdout, stdin);
    (rpc, incoming, child)
}

#[tokio::test]
async fn headless_server_edits_a_buffer_over_its_pipes() {
    let (rpc, _incoming, _child) = spawn_headless(None);
    attach(&rpc, 80, 24).await;

    feed(&rpc, "ihello world<Esc>obye<Esc>");

    assert_eq!(
        lines(&rpc).await,
        vec!["hello world".to_string(), "bye".to_string()]
    );
}

#[tokio::test]
async fn headless_server_opens_the_file_argument() {
    // The remote file argument `ssh … nxvim --server <file>` opens on startup.
    let path = write_temp("headless-open", "txt", "alpha\nbeta\n");
    let (rpc, _incoming, _child) = spawn_headless(Some(&path));
    attach(&rpc, 80, 24).await;

    assert_eq!(
        lines(&rpc).await,
        vec!["alpha".to_string(), "beta".to_string()]
    );
}
