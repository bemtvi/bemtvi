//! The `nx.plugins` manager is REMOTE-AWARE: in a daemon (edit-host) session it
//! manages plugins on the LOCAL disk, not the remote's — plugins load into the local
//! Lua VM via the local runtimepath, so cloning / discovering / sourcing them on the
//! remote would clone a plugin that never loads. See
//! `docs/plans/2026-07-03-remote-aware-plugin-manager.md`.
//!
//! Faithful, not a no-op: the session's `nx.fs` is a real [`RemoteFsJobs`] seam
//! (`FsBackend::Remote`) answered over an in-process wire by a daemon-side `LuaFs`
//! that COUNTS every op it serves. A control `nx.fs.exists` proves the counter is
//! live (a session `nx.fs` op DOES cross to the daemon). Then a local-`dir` plugin is
//! loaded: its `plugin/*.lua` is discovered + sourced through the manager's fs seam.
//! With the fix, that stays on the local disk, so the daemon counter does NOT move;
//! before the fix it routed remote and the counter would climb. The plugin's own
//! `plugin/` flag confirms it actually loaded.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use nxvim_lua::{LuaDirEntry, LuaFs, LuaStat, StdLuaFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteFsJobs, ServerInit};
use nxvim_test_harness::{attach, exec_lua, lua_bool, q, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// A [`LuaFs`] that delegates to a real [`StdLuaFs`] but counts every op it serves.
/// Wired as the *daemon's* fs, so any op the counter records is one that crossed the
/// wire from the editor — the exact thing the manager must NOT do for its own
/// management ops.
struct CountingFs {
    inner: StdLuaFs,
    ops: Arc<AtomicUsize>,
}

impl CountingFs {
    fn new(ops: Arc<AtomicUsize>) -> CountingFs {
        CountingFs {
            inner: StdLuaFs::new(),
            ops,
        }
    }
    fn tick(&self) {
        self.ops.fetch_add(1, Ordering::SeqCst);
    }
}

impl LuaFs for CountingFs {
    fn open(&self, path: &str, flags: &str, mode: u32) -> io::Result<i64> {
        self.tick();
        self.inner.open(path, flags, mode)
    }
    fn close(&self, fd: i64) -> io::Result<()> {
        self.tick();
        self.inner.close(fd)
    }
    fn read(&self, fd: i64, size: usize, offset: Option<i64>) -> io::Result<Vec<u8>> {
        self.tick();
        self.inner.read(fd, size, offset)
    }
    fn write(&self, fd: i64, data: &[u8], offset: Option<i64>) -> io::Result<usize> {
        self.tick();
        self.inner.write(fd, data, offset)
    }
    fn fstat(&self, fd: i64) -> io::Result<LuaStat> {
        self.tick();
        self.inner.fstat(fd)
    }
    fn stat(&self, path: &str) -> io::Result<LuaStat> {
        self.tick();
        self.inner.stat(path)
    }
    fn lstat(&self, path: &str) -> io::Result<LuaStat> {
        self.tick();
        self.inner.lstat(path)
    }
    fn scandir(&self, path: &str) -> io::Result<Vec<LuaDirEntry>> {
        self.tick();
        self.inner.scandir(path)
    }
    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> io::Result<()> {
        self.tick();
        self.inner.mkdir(path, mode, recursive)
    }
    fn rmdir(&self, path: &str) -> io::Result<()> {
        self.tick();
        self.inner.rmdir(path)
    }
    fn unlink(&self, path: &str) -> io::Result<()> {
        self.tick();
        self.inner.unlink(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.tick();
        self.inner.rename(from, to)
    }
    fn copyfile(&self, src: &str, dest: &str, excl: bool) -> io::Result<()> {
        self.tick();
        self.inner.copyfile(src, dest, excl)
    }
    fn utime(&self, path: &str, atime: f64, mtime: f64) -> io::Result<()> {
        self.tick();
        self.inner.utime(path, atime, mtime)
    }
    fn access(&self, path: &str, modes: &str) -> bool {
        self.tick();
        self.inner.access(path, modes)
    }
    fn realpath(&self, path: &str) -> io::Result<String> {
        self.tick();
        self.inner.realpath(path)
    }
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        self.tick();
        self.inner.read_file(path)
    }
    fn which(&self, name: &str) -> Option<String> {
        self.tick();
        self.inner.which(name)
    }
}

/// Start a daemon-backed session whose `nx.fs` seam is a [`RemoteFsJobs`] answered by a
/// [`CountingFs`] over an in-process duplex. Returns the RPC, the kept-alive notification
/// receiver, and the daemon-side op counter.
async fn spawn_with_counting_daemon() -> (Rpc, UnboundedReceiver<Incoming>, Arc<AtomicUsize>) {
    let ops = Arc::new(AtomicUsize::new(0));
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    let fs_ops = ops.clone();
    tokio::spawn(async move {
        let _ = nxvim_server::serve_luafs_daemon(
            daemon_reader,
            daemon_writer,
            Box::new(CountingFs::new(fs_ops)),
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
    (rpc, incoming, ops)
}

/// A local-`dir` plugin's `plugin/` script is discovered + sourced on the LOCAL disk in a
/// daemon session — the manager never routes its management fs to the remote.
#[tokio::test]
async fn plugin_management_stays_local_in_a_daemon_session() {
    let (rpc, _incoming, ops) = spawn_with_counting_daemon().await;

    // A `dir` plugin on the local disk: a require-able module + an auto-sourced
    // `plugin/delta.lua` that sets a flag (the flag rides the manager's fs seam).
    let src = temp_dir("daemon_plug_dir");
    let repo = src.join("delta");
    std::fs::create_dir_all(repo.join("lua").join("delta")).unwrap();
    std::fs::create_dir_all(repo.join("plugin")).unwrap();
    std::fs::write(
        repo.join("lua").join("delta").join("init.lua"),
        "local M = {}\nfunction M.setup() _G.delta_setup = true end\nreturn M\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("plugin").join("delta.lua"),
        "_G.delta_plugin = true\n",
    )
    .unwrap();

    // Control: a *session* `nx.fs` op DOES cross to the daemon — proves the counter is
    // live, so a zero delta below is meaningful (not a dead wire).
    exec_lua(
        &rpc,
        &format!(
            "_G.__ctl = nil
             nx.fs.exists(\"{p}\"):next(function(v) _G.__ctl = v end)
             return 1",
            p = q(&repo)
        ),
    )
    .await;
    let mut saw_control = false;
    for _ in 0..200 {
        if lua_bool(&rpc, "return _G.__ctl == true").await == Some(true) {
            saw_control = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(saw_control, "control nx.fs.exists should reach the daemon");
    let baseline = ops.load(Ordering::SeqCst);
    assert!(baseline > 0, "the daemon fs counter must be live");

    // Now load the dir plugin. Its discovery + `plugin/` sourcing runs through the
    // manager's fs seam — which must stay LOCAL.
    exec_lua(
        &rpc,
        &format!(
            "nx.plugins {{ {{ name = \"delta\", dir = \"{dir}\",\n\
               config = function() require(\"delta\").setup() end }} }}",
            dir = q(&repo)
        ),
    )
    .await;

    let mut loaded = false;
    for _ in 0..200 {
        if lua_bool(&rpc, "return _G.delta_plugin == true").await == Some(true) {
            loaded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        loaded,
        "the dir plugin's plugin/ script should have sourced (locally)"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.delta_setup == true").await,
        Some(true),
        "config() should have run"
    );

    // The proof: loading the plugin added ZERO daemon fs ops — its management stayed on
    // the local disk. Before the fix, `source_runtime`'s discovery + read routed remote
    // and this delta would be > 0.
    let after = ops.load(Ordering::SeqCst);
    assert_eq!(
        after,
        baseline,
        "plugin management routed to the daemon ({} ops) — it must stay local",
        after - baseline
    );
}
