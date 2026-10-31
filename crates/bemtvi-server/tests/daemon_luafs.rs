//! The daemon wire protocol, `btv.fs` half (the `luafs_op` leg) — Phase 3 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`, after the blocking-IO
//! consolidation that retired the per-op `luafs`/`RemoteLuaFs` bridge.
//!
//! Proves async `btv.fs` runs **on the daemon over a real wire** in a native-daemon
//! session: the editor is given a [`RemoteFsJobs`](bemtvi_server::RemoteFsJobs) as its
//! fs seam (so the event-loop actor is `FsBackend::Remote` — it has NO local fs and can
//! ONLY send `luafs_op` requests), and a `serve_luafs_daemon` backed by a real
//! [`StdLuaFs`](bemtvi_lua::StdLuaFs) answers them over an in-process `tokio::io::duplex`.
//!
//! Faithful, not a no-op: because the actor holds no local filesystem, an `btv.fs.write`
//! that lands bytes on disk — and an `btv.fs.read_text` that returns a file's content —
//! can only have crossed the wire to the daemon's `StdLuaFs`. The whole [`FsJob`] is
//! encoded ([`fs_job_to_value`](bemtvi_lua)) into one `luafs_op` request, run through
//! [`run_fs_job`](bemtvi_lua) daemon-side, and the typed reply decoded back — the same
//! leg (and codec) the wasm edit-host uses, exercised natively here.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, driving `btv.fs`
//! through `exec_lua` and asserting on real on-disk bytes.

use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::{RemoteFsJobs, ServerInit};
use bemtvi_test_harness::{attach, exec_lua, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server whose `btv.fs` seam is a [`RemoteFsJobs`] talking to a
/// `serve_luafs_daemon` (a real local [`StdLuaFs`](bemtvi_lua::StdLuaFs)) over an
/// in-process duplex. UI-attached. The actor is `FsBackend::Remote` — no local fs — so
/// every `btv.fs` op must cross the wire. The notification receiver is returned (not
/// dropped): dropping it would tear the client connection down and stop the server.
async fn spawn_with_daemon_luafs() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::serve_luafs_daemon(
            daemon_reader,
            daemon_writer,
            Box::new(bemtvi_lua::StdLuaFs::new()),
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
/// `btv.fs` promise settles on a later tick, so a bounded retry beats a fixed sleep.
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

/// `btv.fs.write` lands bytes on the daemon's disk over the wire. The actor has no local
/// fs, so the file appearing can only have come from the daemon's `StdLuaFs`.
#[tokio::test]
async fn btv_fs_write_lands_on_the_daemon_over_the_wire() {
    let dir = temp_dir("daemon_luafs_write");
    let path = dir.join("out.txt");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__w = nil
               btv.fs.write("{path_str}", "wrote-over-the-wire"):next(
                 function() _G.__w = "ok" end,
                 function(e) _G.__w = "err:" .. tostring(e) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__w", "ok").await,
        "btv.fs.write should resolve (it ran on the daemon over the wire); got {:?}",
        exec_lua(&rpc, "return tostring(_G.__w)").await.as_str(),
    );
    // The bytes are on real disk — read them back in the test process.
    let on_disk = std::fs::read_to_string(&path).expect("the daemon wrote the file to disk");
    assert_eq!(
        on_disk, "wrote-over-the-wire",
        "the daemon's StdLuaFs wrote the bytes btv.fs.write sent over the wire"
    );
}

/// `btv.fs.read_text` returns a file's content fetched from the daemon over the wire.
#[tokio::test]
async fn btv_fs_read_text_fetches_from_the_daemon_over_the_wire() {
    let dir = temp_dir("daemon_luafs_read");
    let path = dir.join("in.txt");
    std::fs::write(&path, "fetched-over-the-wire").expect("seed the daemon-side file");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__r = nil
               btv.fs.read_text("{path_str}"):next(
                 function(text) _G.__r = text end,
                 function(e) _G.__r = "err:" .. tostring(e) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__r", "fetched-over-the-wire").await,
        "btv.fs.read_text should resolve with the daemon's file content; got {:?}",
        exec_lua(&rpc, "return tostring(_G.__r)").await.as_str(),
    );
}

/// A missing path rejects loud (ENOENT) over the wire — never a silent empty result.
#[tokio::test]
async fn btv_fs_read_missing_rejects_over_the_wire() {
    let dir = temp_dir("daemon_luafs_missing");
    let path = dir.join("nope.txt");
    let path_str = path.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__e = nil
               btv.fs.read("{path_str}"):next(
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

/// The `.gitignore`-aware walk runs over the wire too: every `readdir` and every ignore
/// file it reads is a `luafs_op` to the daemon, so the pruned listing can only have come
/// from the daemon's tree — what the pure web client's `files_git` relies on.
#[tokio::test]
async fn btv_fs_walk_honours_gitignore_over_the_wire() {
    let dir = temp_dir("daemon_luafs_walk_gitignore");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
    std::fs::write(dir.join(".gitignore"), "node_modules/\n*.log\n").unwrap();
    for f in ["node_modules/pkg/index.js", "debug.log", "main.rs", ".env"] {
        std::fs::write(dir.join(f), "x").unwrap();
    }
    let dir_str = dir.to_string_lossy().replace('\\', "\\\\");
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;
    exec_lua(
        &rpc,
        &format!(
            "_G.walked = nil\n\
             btv.async(function()\n\
               local files = btv.await(btv.fs.walk(\"{dir_str}\", {{ hidden = true, gitignore = true }}))\n\
               table.sort(files)\n\
               _G.walked = table.concat(files, \",\")\n\
             end)()"
        ),
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "_G.walked", ".env,.gitignore,main.rs").await,
        "the daemon-side walk pruned node_modules/ and *.log, got {:?}",
        exec_lua(&rpc, "return _G.walked").await
    );
}

/// The parts of the `.gitignore` walk that read *outside* the walked tree also cross the
/// wire: the ancestor scan above `dir` (an anchored rule in the worktree root's
/// `.gitignore` applies to the subdirectory walked), and a linked worktree's `.git` FILE
/// followed through `gitdir:` and `commondir` to the main repository's `info/exclude`.
/// A second worktree whose `gitdir:` runs through a regular file makes the daemon answer
/// `ENOTDIR` for its `commondir` / `info/exclude`; that must be read as "no rules", not
/// reject the walk — the error code has to survive the `luafs_op` codec intact.
#[tokio::test]
async fn btv_fs_walk_gitignore_reads_ancestors_and_worktree_exclude_over_the_wire() {
    let dir = temp_dir("daemon_luafs_walk_worktree");
    // The main repository: its `info/exclude` ignores `*.tmp`; the worktree's git dir
    // names it through `commondir`.
    std::fs::create_dir_all(dir.join("main/.git/info")).unwrap();
    std::fs::create_dir_all(dir.join("main/.git/worktrees/wt")).unwrap();
    std::fs::write(dir.join("main/.git/info/exclude"), "*.tmp\n").unwrap();
    std::fs::write(dir.join("main/.git/worktrees/wt/commondir"), "../..\n").unwrap();
    // The linked worktree: a `.git` file, and an anchored rule for `sub/gen/`.
    std::fs::create_dir_all(dir.join("wt/sub/gen")).unwrap();
    let gitdir = dir.join("main/.git/worktrees/wt");
    std::fs::write(
        dir.join("wt/.git"),
        format!("gitdir: {}\n", gitdir.to_string_lossy()),
    )
    .unwrap();
    std::fs::write(dir.join("wt/.gitignore"), "/sub/gen/\n").unwrap();
    for f in ["wt/sub/gen/a.js", "wt/sub/keep.rs", "wt/sub/x.tmp"] {
        std::fs::write(dir.join(f), "x").unwrap();
    }
    // The broken worktree: `gitdir:` runs through a regular file.
    std::fs::create_dir_all(dir.join("bad/sub")).unwrap();
    std::fs::write(dir.join("notadir"), "x").unwrap();
    let bad_gitdir = dir.join("notadir/wt");
    std::fs::write(
        dir.join("bad/.git"),
        format!("gitdir: {}\n", bad_gitdir.to_string_lossy()),
    )
    .unwrap();
    std::fs::write(dir.join("bad/sub/kept.tmp"), "x").unwrap();

    let esc = |p: std::path::PathBuf| p.to_string_lossy().replace('\\', "\\\\");
    let wt_sub = esc(dir.join("wt/sub"));
    let bad_sub = esc(dir.join("bad/sub"));
    let (rpc, _incoming) = spawn_with_daemon_luafs().await;
    exec_lua(
        &rpc,
        &format!(
            "_G.wt, _G.bad = nil, nil\n\
             local function walk(d, g)\n\
               btv.async(function()\n\
                 local ok, files = pcall(btv.await, btv.fs.walk(d, {{ hidden = true, gitignore = true }}))\n\
                 if not ok then _G[g] = 'ERR:' .. tostring(type(files) == 'table' and (files.message or files.code) or files) return end\n\
                 table.sort(files)\n\
                 _G[g] = table.concat(files, ',')\n\
               end)()\n\
             end\n\
             walk(\"{wt_sub}\", 'wt')\n\
             walk(\"{bad_sub}\", 'bad')"
        ),
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "_G.wt", "keep.rs").await,
        "the ancestor's anchored /sub/gen/ and the main repo's info/exclude (*.tmp) applied \
         over the wire, got {:?}",
        exec_lua(&rpc, "return _G.wt").await
    );
    assert!(
        await_lua_eq(&rpc, "_G.bad", "kept.tmp").await,
        "an ENOTDIR from the daemon reads as no rules, not a rejected walk, got {:?}",
        exec_lua(&rpc, "return _G.bad").await
    );
}
