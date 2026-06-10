//! The daemon wire protocol, filesystem half — **the remote explorer over the wire**
//! (edit-host split, Phase 3g of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_fs.rs` (initial open), `daemon_save.rs` (save), and
//! `daemon_edit.rs` (`:edit`). Here a real editor whose async fs is a
//! [`RemoteHostFs`](nxvim_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](nxvim_server::serve_fs_daemon) over an in-process duplex opens a
//! **remote directory** — listed over the wire, off the editor tick, through the same
//! `HostFsAsync` + replica path a file open uses. Until this slice a remote directory
//! came back as a loud `fs_read` error ("remote directory open not yet supported"); now
//! it lists, navigates, and opens entries — all without touching the edit-host's local
//! disk:
//!
//! - `nxvim /virtual/proj` (startup) lists the remote directory's entries.
//! - `:edit /virtual/proj` lists it at runtime.
//! - `<CR>` on a sub-directory descends into it over the wire; `-` goes back up.
//! - `<CR>` on a file entry opens that remote file's bytes over the wire.
//!
//! The `/virtual/...` paths can't exist on the edit-host's local disk, so any listing or
//! content can *only* have crossed the wire (the faithfulness argument the other daemon
//! suites make).

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, buf_lines, feed, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory **daemon** filesystem that models both files and directories: a path is
/// a file (with bytes), a directory (with entries), or absent. `read_dir` succeeds only
/// for a directory path — so a directory classifies as a listing and a file as a file,
/// exactly as the real [`StdHostFs`](nxvim_core::StdHostFs) does.
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

    /// Register a directory at `path` whose entries are `(is_dir, name)` pairs.
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

/// A fake daemon fs pre-populated with `/virtual/proj` (a directory holding a `src/`
/// sub-directory and two files), `/virtual/proj/src` (holding `main.rs`), and the two
/// readable files. The standard fixture for these tests.
fn fixture() -> DaemonFs {
    let fs = DaemonFs::default();
    fs.dir(
        "/virtual/proj",
        &[(true, "src"), (false, "README.md"), (false, "notes.txt")],
    )
    .dir("/virtual/proj/src", &[(false, "main.rs")])
    .file("/virtual/proj/README.md", "# Readme\n")
    .file("/virtual/proj/src/main.rs", "fn main() {}\n");
    fs
}

/// Start a server whose async fs is a [`RemoteHostFs`] talking to a `serve_fs_daemon`
/// (backed by `fake`) over an in-process duplex, opening `path`. UI-attached.
async fn spawn_with_daemon_fs(fake: DaemonFs, path: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_fs_daemon(daemon_reader, daemon_writer, Box::new(fake)).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteHostFs::connect(host_reader, host_writer);
    let init = ServerInit {
        file: Some(path.to_string()),
        host_fs_async: Some(Box::new(remote)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out — the
/// off-tick listing (startup, `:edit`, or a descend) lands a moment after the command.
async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        if buf_lines(rpc, 0).await == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// `nxvim /virtual/proj` lists the remote directory over the wire: a `../` up-entry,
/// then directories first (suffixed `/`), then files, each group case-insensitively by
/// name. The `/virtual/...` path can't be a local directory, so the listing crossed the
/// wire.
#[tokio::test]
async fn startup_lists_a_remote_directory() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    assert_eq!(
        await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await,
        vec!["../", "src/", "notes.txt", "README.md"],
        "the startup directory lists its remote entries (dirs first, then files by name)"
    );
}

/// `:edit /virtual/proj` lists a remote directory at runtime (the same off-tick path as
/// the startup open).
#[tokio::test]
async fn edit_lists_a_remote_directory() {
    let fake = fixture();
    fake.file("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":edit /virtual/proj<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await,
        vec!["../", "src/", "notes.txt", "README.md"],
        "`:edit <remote-dir>` lists the remote directory over the wire"
    );
}

/// `<CR>` on a sub-directory descends into it over the wire (re-listing in place); `-`
/// goes back up to the parent — both remote `read_dir`s.
#[tokio::test]
async fn enter_descends_and_dash_goes_up_over_the_wire() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await;

    // Row 1 is `src/`; `<CR>` lists that sub-directory over the wire.
    feed(&rpc, "j<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "main.rs"]).await,
        vec!["../", "main.rs"],
        "`<CR>` on `src/` descends into the remote sub-directory"
    );

    // `-` lists the parent again (another remote read_dir).
    feed(&rpc, "-");
    assert_eq!(
        await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await,
        vec!["../", "src/", "notes.txt", "README.md"],
        "`-` lists the remote parent directory again"
    );
}

/// `<CR>` on a file entry opens that remote file's bytes over the wire (and destroys the
/// listing, as netrw does).
#[tokio::test]
async fn enter_on_a_file_opens_it_over_the_wire() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await;

    // Descend into `src/` (row 1), then open `main.rs` (row 1 of that listing).
    feed(&rpc, "j<CR>");
    await_lines(&rpc, &["../", "main.rs"]).await;
    feed(&rpc, "j<CR>");
    assert_eq!(
        await_lines(&rpc, &["fn main() {}"]).await,
        vec!["fn main() {}"],
        "`<CR>` on a file row opens the remote file's bytes over the wire"
    );
}

/// `:tabnew /virtual/proj/src` opens the remote *directory* as the explorer in a **new
/// tab** — the unified off-tick open kernel (Phase 3h) composes with the remote-explorer
/// listing (Phase 3g): a directory routed through `:tabnew` lists over the wire just as
/// `:edit` does, in its own tab.
#[tokio::test]
async fn tabnew_lists_a_remote_directory_in_a_new_tab() {
    let (rpc, _incoming) = spawn_with_daemon_fs(fixture(), "/virtual/proj").await;
    await_lines(&rpc, &["../", "src/", "notes.txt", "README.md"]).await;

    feed(&rpc, ":tabnew /virtual/proj/src<CR>");
    assert_eq!(
        await_lines(&rpc, &["../", "main.rs"]).await,
        vec!["../", "main.rs"],
        "`:tabnew <remote-dir>` lists the directory as the explorer in a new tab"
    );
}
