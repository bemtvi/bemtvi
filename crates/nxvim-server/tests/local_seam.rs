//! The public **local-always** seam — `nx.run_local` / `nx.fs_local` (prelude/localseam.lua),
//! the twins of `nx.run` / `nx.fs` that always act on the client machine even in a daemon
//! session (`nx.http.fetch_local` is the HTTP twin). Backs the plugin manager and remote
//! connectors (§E). Black-box per project conventions: a real server over RPC, driven with
//! `nvim_exec_lua`, asserting on observable Lua state + the real filesystem. In this local
//! harness the local seam behaves exactly like `nx.run` / `nx.fs`; these prove the surface
//! exists, runs a child, and round-trips the disk (the same ops the daemon-routing forces
//! local in a remote session).

use std::fs;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Poll a `return`-style chunk until it yields a non-nil value (~3s). The local seam settles
/// OFF the editor tick (like `nx.run` / `nx.fs`), so the global its chain sets is nil until
/// the loop processes the actor's result.
async fn poll_settled(rpc: &Rpc, code: &str) -> rmpv::Value {
    for _ in 0..150 {
        let v = exec_lua(rpc, code).await;
        if !matches!(v, rmpv::Value::Nil) {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    exec_lua(rpc, code).await
}

#[tokio::test]
async fn run_local_runs_a_child_and_resolves_the_exit_result() {
    let (rpc, _incoming) = start().await;

    exec_lua(
        &rpc,
        "_G.r = nil\n\
         nx.async(function()\n\
         \x20 _G.r = nx.await(nx.run_local({ cmd = 'printf', args = { 'hi-local' } }))\n\
         end)()",
    )
    .await;

    let code = poll_settled(&rpc, "return _G.r and _G.r.code").await;
    assert_eq!(code.as_i64(), Some(0), "the child exits 0");
    let out = exec_lua(&rpc, "return _G.r.stdout").await;
    assert_eq!(
        out.as_str(),
        Some("hi-local"),
        "run_local buffers the child's stdout on the client",
    );
}

#[tokio::test]
async fn run_local_reports_a_spawn_failure_as_code_minus_one() {
    let (rpc, _incoming) = start().await;

    exec_lua(
        &rpc,
        "_G.r = nil\n\
         nx.async(function()\n\
         \x20 _G.r = nx.await(nx.run_local({ cmd = 'definitely-not-a-real-binary-xyz' }))\n\
         end)()",
    )
    .await;

    let code = poll_settled(&rpc, "return _G.r and _G.r.code").await;
    assert_eq!(
        code.as_i64(),
        Some(-1),
        "a spawn failure resolves code = -1 (never rejects), like nx.run",
    );
}

#[tokio::test]
async fn fs_local_round_trips_the_client_disk() {
    let (rpc, _incoming) = start().await;
    let dir = temp_dir("fs_local");
    let file = dir.join("note.txt");
    let file_lua = file.to_string_lossy().replace('\\', "\\\\");

    exec_lua(
        &rpc,
        &format!(
            "_G.done = nil\n\
             nx.async(function()\n\
             \x20 nx.await(nx.fs_local.write('{file_lua}', 'from-fs-local'))\n\
             \x20 _G.seen = nx.await(nx.fs_local.exists('{file_lua}'))\n\
             \x20 _G.text = nx.await(nx.fs_local.read_text('{file_lua}'))\n\
             \x20 _G.done = true\n\
             end)()",
        ),
    )
    .await;

    for _ in 0..150 {
        if lua_bool(&rpc, "return _G.done").await == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The write actually hit the real client disk.
    assert_eq!(
        fs::read_to_string(&file).ok().as_deref(),
        Some("from-fs-local"),
        "fs_local.write persisted to the real filesystem",
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.seen").await.as_bool(),
        Some(true),
        "fs_local.exists sees the file it wrote",
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.text").await.as_str(),
        Some("from-fs-local"),
        "fs_local.read_text reads it back",
    );
}
