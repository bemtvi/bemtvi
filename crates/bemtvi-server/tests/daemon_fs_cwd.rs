//! The daemon wire protocol, `btv.fs` working-directory half — **a relative `btv.fs` path
//! resolves against the session cwd (`DirState`), not the daemon's launch dir**
//! (`docs/plans/2026-06-23-remote-cwd.md`).
//!
//! In a daemon session the edit-host runs locally while files live on the remote daemon,
//! and one daemon process serves many sessions, so it keeps NO per-session process cwd —
//! it resolves a relative path against its own launch dir. The session's true cwd lives in
//! the edit-host's `DirState` (seeded from the daemon, moved by a remote `:cd`). So the
//! edit-host must absolutize a relative `btv.fs` path against `DirState` *before* it crosses
//! the wire — exactly as `drain_pending_opens` does for a relative `:edit`. Without that, a
//! `btv.fs.readdir(".")` lists the daemon's launch dir and silently ignores `:cd`.
//!
//! Faithful, not a no-op: a real `bemtvi --daemon` (`run_daemon_io`) serves the real disk
//! over an in-process duplex, seeded to a temp tree whose contents differ per directory. A
//! `.`-relative readdir that returns the temp tree's entries — never the daemon process's
//! launch dir (the crate dir) — can only have resolved against the session's `DirState`.

use std::path::PathBuf;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::{connect_daemon, ServerInit};
use bemtvi_test_harness::{attach, exec_lua, feed, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a full edit-host session against a real `bemtvi --daemon` over an in-process
/// duplex (every host seam over one link, as the binary does over stdio), with the
/// session cwd seeded to `cwd`. The daemon serves the real disk, so a relative `btv.fs`
/// path is answered against whatever dir the edit-host absolutized it to.
async fn spawn_remote(cwd: PathBuf) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (d_reader, d_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::run_daemon_io(d_reader, d_writer).await;
    });
    let (h_reader, h_writer) = tokio::io::split(host_end);
    let client = connect_daemon(h_reader, h_writer);
    let init = ServerInit {
        host_fs_async: Some(Box::new(client.host_fs)),
        host_proc: Some(Box::new(client.host_proc)),
        fs_jobs: Some(client.fs_jobs),
        remote_cwd: Some(cwd),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Run `btv.fs.readdir(path)` and poll until it settles, returning its entry names sorted
/// and comma-joined (an off-tick op resolves a moment after the call). Empty string on a
/// rejection (so a stale-dir readdir that errors is distinguishable from a hit).
async fn readdir_names(rpc: &Rpc, path: &str) -> String {
    exec_lua(
        rpc,
        &format!(
            "_G.__names = nil
             btv.async(function()
               local ok, es = pcall(btv.await, btv.fs.readdir(\"{path}\"))
               if not ok then _G.__names = \"\" return end
               local ns = {{}}
               for _, e in ipairs(es) do ns[#ns + 1] = e.name end
               table.sort(ns)
               _G.__names = table.concat(ns, \",\")
             end)()
             return 1"
        ),
    )
    .await;
    for _ in 0..200 {
        let v = exec_lua(rpc, "return _G.__names").await;
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    exec_lua(rpc, "return _G.__names")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Run `btv.run { cmd }` (no `cwd`) and poll until it exits, returning its stdout — the
/// child inherits the session cwd, so `ls` lists whatever dir the spawn was rooted at.
async fn run_stdout(rpc: &Rpc, cmd: &str) -> String {
    exec_lua(
        rpc,
        &format!(
            "_G.__out = nil
             btv.run({{ cmd = {{ {cmd} }} }}):next(function(r) _G.__out = r.stdout end)
             return 1"
        ),
    )
    .await;
    for _ in 0..200 {
        let v = exec_lua(rpc, "return _G.__out").await;
        if let Some(s) = v.as_str() {
            if !s.is_empty() {
                return s.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    exec_lua(rpc, "return _G.__out")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Poll `vim.fn.getcwd()` until it equals `want` (a remote `:cd` lands off the tick).
async fn await_getcwd(rpc: &Rpc, want: &str) {
    for _ in 0..200 {
        let cwd = exec_lua(rpc, "return vim.fn.getcwd()")
            .await
            .as_str()
            .unwrap_or_default()
            .to_string();
        if cwd == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A `btv.fs.readdir(".")` in a daemon session lists the *session* cwd (the seeded remote
/// dir), not the daemon process's launch dir. The seeded dir holds a uniquely-named file
/// the crate dir cannot, so a hit can only be a `DirState`-resolved `.`.
#[tokio::test]
async fn readdir_dot_resolves_against_the_session_cwd() {
    let root = temp_dir("daemon_fs_cwd_root");
    std::fs::write(root.join("alpha_marker.txt"), b"").unwrap();

    let (rpc, _incoming) = spawn_remote(root.clone()).await;

    let names = readdir_names(&rpc, ".").await;
    assert!(
        names.split(',').any(|n| n == "alpha_marker.txt"),
        "btv.fs.readdir(\".\") must resolve `.` against the session cwd (got: {names:?})"
    );
}

/// After a remote `:cd` into a subdirectory, `btv.fs.readdir(".")` follows it — the same
/// stale-cwd bug the user hit (readdir listing the *old* dir after `:cd`).
#[tokio::test]
async fn readdir_dot_follows_a_remote_cd() {
    let root = temp_dir("daemon_fs_cwd_cd");
    std::fs::write(root.join("alpha_marker.txt"), b"").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("beta_marker.txt"), b"").unwrap();

    let (rpc, _incoming) = spawn_remote(root.clone()).await;

    // `:cd sub` (relative) moves the session cwd into <root>/sub.
    feed(&rpc, ":cd sub<CR>");
    await_getcwd(&rpc, &root.join("sub").to_string_lossy()).await;

    let names = readdir_names(&rpc, ".").await;
    assert!(
        names.split(',').any(|n| n == "beta_marker.txt"),
        "after `:cd sub`, readdir(\".\") must list the new cwd's entries (got: {names:?})"
    );
    assert!(
        !names.split(',').any(|n| n == "alpha_marker.txt"),
        "readdir(\".\") must NOT still list the old cwd's entries (got: {names:?})"
    );
}

/// A `btv.run` with no `cwd` runs the child in the *session* cwd (like neovim's
/// `vim.system`), not the daemon's launch dir — so an `ls` sees the seeded dir's files.
/// This is the `:messages`-shows-the-old-git-branch half of the same stale-cwd bug: a
/// git spawn with no cwd ran in the daemon's dir.
#[tokio::test]
async fn spawn_without_cwd_runs_in_the_session_cwd() {
    let root = temp_dir("daemon_fs_cwd_spawn");
    std::fs::write(root.join("alpha_marker.txt"), b"").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("beta_marker.txt"), b"").unwrap();

    let (rpc, _incoming) = spawn_remote(root.clone()).await;

    let out = run_stdout(&rpc, "\"ls\"").await;
    assert!(
        out.lines().any(|l| l == "alpha_marker.txt"),
        "a no-cwd `btv.run` must inherit the session cwd (got: {out:?})"
    );

    feed(&rpc, ":cd sub<CR>");
    await_getcwd(&rpc, &root.join("sub").to_string_lossy()).await;

    let out = run_stdout(&rpc, "\"ls\"").await;
    assert!(
        out.lines().any(|l| l == "beta_marker.txt"),
        "after `:cd sub`, a no-cwd `btv.run` must follow the new cwd (got: {out:?})"
    );
}
