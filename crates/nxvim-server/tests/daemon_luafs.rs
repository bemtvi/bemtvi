//! The daemon wire protocol, `nx.fs` half (the `luafs_op` leg) — Phase 3 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`, after the blocking-IO
//! consolidation that retired the per-op `luafs`/`RemoteLuaFs` bridge.
//!
//! Proves async `nx.fs` runs **on the daemon over a real wire** in a native-daemon
//! session: the editor is given a [`RemoteFsJobs`](nxvim_server::RemoteFsJobs) as its
//! fs seam (so the event-loop actor is `FsBackend::Remote` — it has NO local fs and can
//! ONLY send `luafs_op` requests), and a `serve_luafs_daemon` backed by a real
//! [`StdLuaFs`](nxvim_lua::StdLuaFs) answers them over an in-process `tokio::io::duplex`.
//!
//! Faithful, not a no-op: because the actor holds no local filesystem, an `nx.fs.write`
//! that lands bytes on disk — and an `nx.fs.read_text` that returns a file's content —
//! can only have crossed the wire to the daemon's `StdLuaFs`. The whole [`FsJob`] is
//! encoded ([`fs_job_to_value`](nxvim_lua)) into one `luafs_op` request, run through
//! [`run_fs_job`](nxvim_lua) daemon-side, and the typed reply decoded back — the same
//! leg (and codec) the wasm edit-host uses, exercised natively here.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, driving `nx.fs`
//! through `exec_lua` and asserting on real on-disk bytes.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteFsJobs, ServerInit};
use nxvim_test_harness::{attach, exec_lua, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server whose `nx.fs` seam is a [`RemoteFsJobs`] talking to a
/// `serve_luafs_daemon` (a real local [`StdLuaFs`](nxvim_lua::StdLuaFs)) over an
/// in-process duplex. UI-attached. The actor is `FsBackend::Remote` — no local fs — so
/// every `nx.fs` op must cross the wire. The notification receiver is returned (not
/// dropped): dropping it would tear the client connection down and stop the server.
async fn spawn_with_daemon_luafs() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_luafs_daemon(
            daemon_reader,
            daemon_writer,
            Box::new(nxvim_lua::StdLuaFs::new()),
        )
        .await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteFsJobs::connect(host_reader, host_writer);
    let init = ServerInit {
        fs_jobs: Some(remote),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `return tostring(<expr>)` until it equals `want` (or the budget runs out) — an
/// `nx.fs` promise settles on a later tick, so a bounded retry beats a fixed sleep.
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..150 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// `nx.fs.write` lands bytes on the daemon's disk over the wire. The actor has no local
/// fs, so the file appearing can only have come from the daemon's `StdLuaFs`.
#[tokio::test]
async fn nx_fs_write_lands_on_the_daemon_over_the_wire() {
    let dir = temp_dir("daemon_luafs_write");
    let path = dir.join("out.txt");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__w = nil
               nx.fs.write("{path_str}", "wrote-over-the-wire"):next(
                 function() _G.__w = "ok" end,
                 function(e) _G.__w = "err:" .. tostring(e) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__w", "ok").await,
        "nx.fs.write should resolve (it ran on the daemon over the wire); got {:?}",
        exec_lua(&rpc, "return tostring(_G.__w)").await.as_str(),
    );
    // The bytes are on real disk — read them back in the test process.
    let on_disk = std::fs::read_to_string(&path).expect("the daemon wrote the file to disk");
    assert_eq!(
        on_disk, "wrote-over-the-wire",
        "the daemon's StdLuaFs wrote the bytes nx.fs.write sent over the wire"
    );
}

/// `nx.fs.read_text` returns a file's content fetched from the daemon over the wire.
#[tokio::test]
async fn nx_fs_read_text_fetches_from_the_daemon_over_the_wire() {
    let dir = temp_dir("daemon_luafs_read");
    let path = dir.join("in.txt");
    std::fs::write(&path, "fetched-over-the-wire").expect("seed the daemon-side file");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__r = nil
               nx.fs.read_text("{path_str}"):next(
                 function(text) _G.__r = text end,
                 function(e) _G.__r = "err:" .. tostring(e) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__r", "fetched-over-the-wire").await,
        "nx.fs.read_text should resolve with the daemon's file content; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__r)").await.as_str(),
    );
}

/// A missing path rejects loud (ENOENT) over the wire — never a silent empty result.
#[tokio::test]
async fn nx_fs_read_missing_rejects_over_the_wire() {
    let dir = temp_dir("daemon_luafs_missing");
    let path = dir.join("nope.txt");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__e = nil
               nx.fs.read("{path_str}"):next(
                 function() _G.__e = "unexpected-ok" end,
                 function(err) _G.__e = err.code end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__e", "ENOENT").await,
        "reading a missing file should reject with ENOENT over the wire; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__e)").await.as_str(),
    );
}
