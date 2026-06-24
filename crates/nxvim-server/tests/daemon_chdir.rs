//! The daemon wire protocol, working-directory half — **remote-aware `:cd` / `:pwd` /
//! `getcwd`** (`docs/plans/2026-06-23-remote-cwd.md`).
//!
//! In a daemon session the edit-host runs locally while files live on the remote daemon,
//! so the *daemon's* cwd is the one true working directory. These tests drive a real
//! editor whose async fs is a [`RemoteHostFs`](nxvim_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](nxvim_server::serve_fs_daemon) over an in-process duplex, with the
//! daemon's cwd seeded through [`ServerInit::remote_cwd`] (the edit-host fetches it over
//! the `config_bundle` handshake in production). The `/virtual/...` paths can't exist on
//! the edit-host's local disk, so a `:pwd` / `getcwd` that reports them can only be the
//! remote cwd, not the local process's — the faithfulness argument the sibling daemon
//! suites make.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, barrier, buf_lines, exec_lua, feed, message, spawn, wait_redraw};
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory daemon filesystem modeling files and directories (the same shape
/// `daemon_explorer.rs` uses): a path is a file (with bytes), a directory (with entries),
/// or absent. `read_dir` succeeds only for a directory, so a directory classifies as a
/// listing and a file as a file — exactly as the real `StdHostFs` does.
#[derive(Clone, Default)]
struct DaemonFs {
    inner: Arc<Mutex<Tree>>,
}

#[derive(Default)]
struct Tree {
    files: HashMap<PathBuf, Vec<u8>>,
    dirs: HashMap<PathBuf, Vec<(bool, String)>>,
}

impl DaemonFs {
    fn file(&self, path: &str, contents: &str) -> &Self {
        self.inner
            .lock()
            .unwrap()
            .files
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
        self
    }

    fn dir(&self, path: &str, entries: &[(bool, &str)]) -> &Self {
        let entries = entries
            .iter()
            .map(|(is_dir, name)| (*is_dir, name.to_string()))
            .collect();
        self.inner
            .lock()
            .unwrap()
            .dirs
            .insert(PathBuf::from(path), entries);
        self
    }
}

impl HostFs for DaemonFs {
    fn exists(&self, path: &Path) -> bool {
        let t = self.inner.lock().unwrap();
        t.files.contains_key(path) || t.dirs.contains_key(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read>> {
        match self.inner.lock().unwrap().files.get(path) {
            Some(bytes) => Ok(Box::new(Cursor::new(bytes.clone()))),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        }
    }

    fn stat(&self, path: &Path) -> Option<FileStat> {
        self.inner
            .lock()
            .unwrap()
            .files
            .get(path)
            .map(|b| FileStat {
                mtime: None,
                size: b.len() as u64,
            })
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.inner
            .lock()
            .unwrap()
            .files
            .insert(path.to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        match self.inner.lock().unwrap().dirs.get(dir) {
            Some(entries) => Ok(entries
                .iter()
                .map(|(is_dir, name)| DirEntry {
                    is_dir: *is_dir,
                    name: name.clone(),
                })
                .collect()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "not a directory")),
        }
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// A fake daemon fs with `/virtual/proj` (holding `src/` and a README) and
/// `/virtual/proj/src` (holding `main.rs`).
fn fixture() -> DaemonFs {
    let fs = DaemonFs::default();
    fs.dir("/virtual/proj", &[(true, "src"), (false, "README.md")])
        .dir("/virtual/proj/src", &[(false, "main.rs")])
        .file("/virtual/proj/README.md", "# Readme\n")
        .file("/virtual/proj/src/main.rs", "fn main() {}\n");
    fs
}

/// Start a server whose async fs is a [`RemoteHostFs`] talking to a `serve_fs_daemon`
/// (backed by `fake`) over an in-process duplex, opening `file` with the daemon's cwd
/// seeded to `cwd`. UI-attached.
async fn spawn_remote(fake: DaemonFs, cwd: &str, file: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_fs_daemon(daemon_reader, daemon_writer, Box::new(fake)).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteHostFs::connect(host_reader, host_writer);
    let init = ServerInit {
        file: Some(file.to_string()),
        host_fs_async: Some(Box::new(remote)),
        remote_cwd: Some(PathBuf::from(cwd)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// The cwd as the server reports it through `vim.fn.getcwd`.
async fn getcwd(rpc: &Rpc) -> String {
    exec_lua(rpc, "return vim.fn.getcwd()")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Poll `getcwd()` until it matches `want` or the budget runs out — a remote `:cd` lands a
/// moment after the command (the daemon `fs_chdir` round trip is off the editor tick).
async fn await_getcwd(rpc: &Rpc, want: &str) -> String {
    for _ in 0..100 {
        if getcwd(rpc).await == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    getcwd(rpc).await
}

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out (an off-tick
/// open lands a moment after the command).
async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        if buf_lines(rpc, 0).await == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// A remote session seeds its working directory from the daemon: `vim.fn.getcwd()`
/// reports the daemon's cwd, not the edit-host's local process cwd. The `/virtual/proj`
/// path can't be the local cwd, so this can only be the seeded remote dir.
#[tokio::test]
async fn getcwd_reports_the_daemon_cwd() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    assert_eq!(
        getcwd(&rpc).await,
        "/virtual/proj",
        "getcwd() reports the daemon's seeded cwd, not the local process cwd"
    );
}

/// `:pwd` prints the daemon's cwd in a remote session (it reads `DirState`, the seeded
/// remote dir, rather than `std::env::current_dir()`).
#[tokio::test]
async fn pwd_prints_the_daemon_cwd() {
    let (rpc, mut incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    feed(&rpc, ":pwd<CR>");
    barrier(&rpc).await;
    let map = wait_redraw(&mut incoming, |m| !message(m).is_empty()).await;
    assert_eq!(
        message(&map),
        "/virtual/proj",
        "`:pwd` reports the daemon's seeded cwd"
    );
}

/// `:cd <abs remote dir>` moves the working directory over the wire (validated +
/// canonicalized by the daemon), and a subsequent *relative* `:e` resolves against the new
/// dir — proving `:cd` actually took effect on the remote, not just in a local mirror.
#[tokio::test]
async fn cd_absolute_moves_cwd_and_relative_edit_follows() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd /virtual/proj/src<CR>");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj/src").await,
        "/virtual/proj/src",
        "`:cd <remote dir>` moves the cwd over the wire"
    );

    // A relative `:e` now resolves against the new remote cwd (`/virtual/proj/src`), so it
    // opens that directory's `main.rs` over the wire — not the launch dir's.
    feed(&rpc, ":edit main.rs<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "a relative `:edit` after `:cd` reads the file under the new remote cwd"
    );
}

/// `:cd <relative>` resolves against the current effective dir (edit-host side) before the
/// daemon validates it — `:cd src` from `/virtual/proj` lands in `/virtual/proj/src`.
#[tokio::test]
async fn cd_relative_resolves_against_the_effective_dir() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd src<CR>");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj/src").await,
        "/virtual/proj/src",
        "`:cd src` resolves against the effective dir and moves into the subdirectory"
    );
}

/// `:cd <missing remote dir>` fails loud with `E344` and leaves the cwd unchanged — the
/// daemon validated the target (its `read_dir` failed), so the error is real, not local.
#[tokio::test]
async fn cd_nonexistent_remote_dir_errors_e344_and_keeps_cwd() {
    let (rpc, mut incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd /virtual/nope<CR>");
    let map = wait_redraw(&mut incoming, |m| message(m).contains("E344")).await;
    assert!(
        message(&map).contains("E344"),
        "a missing remote directory reports E344 (got {:?})",
        message(&map)
    );
    assert_eq!(
        getcwd(&rpc).await,
        "/virtual/proj",
        "a failed `:cd` leaves the cwd where it was"
    );
}

/// `:cd X` immediately followed by a *relative* `:e Y` (no wait between) resolves Y against
/// X — the optimistic cwd update moves `DirState` the instant `:cd` runs, so the open in
/// the same breath sees the new dir without waiting for the daemon's validation round trip.
#[tokio::test]
async fn cd_then_immediate_relative_edit_uses_the_new_cwd() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    // Back-to-back in one feed — no await between the `:cd` and the `:edit`.
    feed(&rpc, ":cd /virtual/proj/src<CR>:edit main.rs<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "a relative `:edit` issued immediately after `:cd` reads the file under the new cwd"
    );
}

/// `:cd <missing>` that moved the cwd *optimistically* rolls back when the daemon rejects
/// it: getcwd returns to the original dir once the `E344` lands.
#[tokio::test]
async fn cd_rolls_back_when_the_daemon_rejects_the_dir() {
    let (rpc, mut incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    feed(&rpc, ":cd /virtual/nope<CR>");
    // The daemon's `read_dir` fails → `E344` lands and the optimistic move is reverted.
    let map = wait_redraw(&mut incoming, |m| message(m).contains("E344")).await;
    assert!(message(&map).contains("E344"), "a missing dir reports E344");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj").await,
        "/virtual/proj",
        "the optimistic cwd is rolled back to the original after the daemon rejects it"
    );
}

/// `:lcd` is window-local over the wire: it moves only the focused window's cwd, and
/// switching back to the other window restores the global dir (the focus hook re-points
/// the `getcwd` mirror per window).
#[tokio::test]
async fn lcd_is_window_local_over_the_wire() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    // Split, then set a window-local cwd in the new (focused) window.
    feed(&rpc, ":split<CR>");
    barrier(&rpc).await;
    feed(&rpc, ":lcd /virtual/proj/src<CR>");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj/src").await,
        "/virtual/proj/src",
        "`:lcd` moves the focused window's cwd"
    );

    // The other window still sees the global dir.
    feed(&rpc, "<C-w>w");
    assert_eq!(
        await_getcwd(&rpc, "/virtual/proj").await,
        "/virtual/proj",
        "the unaffected window keeps the global cwd (lcd is window-local)"
    );
}

/// Switching focus across a `:lcd` boundary fires `DirChanged` in a daemon session — the
/// remote analogue of the local `fix_current_dir` announce (Phase 1 only re-pointed the
/// `getcwd` mirror there; now it announces a real move too).
#[tokio::test]
async fn focus_switch_fires_dirchanged_over_the_wire() {
    let (rpc, _incoming) =
        spawn_remote(fixture(), "/virtual/proj", "/virtual/proj/README.md").await;
    await_getcwd(&rpc, "/virtual/proj").await;

    // Record every DirChanged's resulting cwd.
    exec_lua(
        &rpc,
        r#"_G.dir_events = {}
           vim.api.nvim_create_autocmd("DirChanged", {
             callback = function() table.insert(_G.dir_events, vim.v.event.cwd) end,
           })"#,
    )
    .await;

    // Split (the new window inherits the global dir — no boundary), then give the focused
    // window a window-local dir.
    feed(&rpc, ":split<CR>");
    barrier(&rpc).await;
    feed(&rpc, ":lcd /virtual/proj/src<CR>");
    await_getcwd(&rpc, "/virtual/proj/src").await;

    // Switch to the other window (global `/virtual/proj`): a real cwd boundary.
    feed(&rpc, "<C-w>w");
    await_getcwd(&rpc, "/virtual/proj").await;

    // The focus switch announced DirChanged for the destination cwd (the last event).
    let events = exec_lua(&rpc, "return _G.dir_events").await;
    let last = events
        .as_array()
        .and_then(|a| a.last())
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        last, "/virtual/proj",
        "switching back to the global-dir window fires DirChanged with that cwd"
    );
}
