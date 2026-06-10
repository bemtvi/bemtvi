//! The daemon wire protocol, filesystem half — the **watch leg** (`HostWatch`, the
//! edit-host split, `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_save.rs` (the off-tick *write*) and `daemon_fs.rs` (the
//! off-tick *read*). Here the daemon **owns change detection**: it watches the files
//! the edit-host opened and pushes a `fs_changed` notification when one drifts, which
//! the edit-host turns into a `FileChangedShell` reconcile off the editor tick — the
//! remote analogue of the local per-buffer file watch.
//!
//! Both tests use a `/virtual/...` path the edit-host's *local* disk can't hold, so the
//! reload bytes can only have crossed the wire (the same faithfulness argument
//! `daemon_save` makes for the write):
//!
//! - an external change to an unmodified buffer **autoreloads** over the wire (the
//!   daemon detected it, pushed it, and the edit-host re-fetched the new bytes), and
//! - a `FileChangedShell` handler fires on the edit-host with `v:fcs_reason` set and
//!   its `v:fcs_choice = "reload"` drives the off-tick re-fetch.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, buf_lines, exec_lua, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory [`HostFs`] for the **daemon** side, mutable from the test so it can
/// simulate an *external* change to a watched file (someone else rewriting it). `stat`
/// reports the byte length as the size (mtime is `None`), so a change must alter the
/// length to be observable — which every case here does.
#[derive(Clone, Default)]
struct DaemonFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl DaemonFs {
    fn with(path: &str, contents: &str) -> Self {
        let me = DaemonFs::default();
        me.set(path, contents);
        me
    }

    /// Rewrite `path` on the daemon — the test's stand-in for an external process
    /// changing the remote file under the editor's feet.
    fn set(&self, path: &str, contents: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
    }
}

impl HostFs for DaemonFs {
    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn Read>> {
        match self.files.lock().unwrap().get(path) {
            Some(bytes) => Ok(Box::new(Cursor::new(bytes.clone()))),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        }
    }

    fn stat(&self, path: &Path) -> Option<FileStat> {
        self.files.lock().unwrap().get(path).map(|b| FileStat {
            mtime: None,
            size: b.len() as u64,
        })
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), contents.to_vec());
        Ok(())
    }

    fn read_dir(&self, _dir: &Path) -> io::Result<Vec<DirEntry>> {
        Err(io::Error::new(io::ErrorKind::NotFound, "not a directory"))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// Start a server whose async fs is a [`RemoteHostFs`] talking to a `serve_fs_daemon`
/// (backed by `fake`) over an in-process duplex, opening `file`. UI-attached. Returns
/// the client RPC handle and its notification receiver (kept, not dropped: dropping it
/// would tear the client connection down and stop the server).
async fn spawn_with_daemon_fs(fake: DaemonFs, file: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
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
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `nvim_buf_get_lines` until it matches `want` (the off-tick fetch lands a moment
/// after the action that triggered it), failing with the last value if it never does.
async fn await_lines(rpc: &Rpc, want: &[&str]) {
    for _ in 0..150 {
        if buf_lines(rpc, 0).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        buf_lines(rpc, 0).await,
        want,
        "buffer never reached {want:?}"
    );
}

/// An external change to an unmodified buffer **autoreloads over the wire**: the daemon
/// notices the file drifted from its baseline, pushes `fs_changed`, and the edit-host
/// re-fetches the new bytes (`'autoread'` is on by default). The new content is a
/// `/virtual/...` path the edit-host's local disk can't hold, so it can only have
/// crossed the wire — and there is **no** `:checktime`, proving the daemon's watch
/// drove it on its own.
#[tokio::test]
async fn an_external_change_autoreloads_over_the_daemon_watch() {
    let fake = DaemonFs::with("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    // Someone else rewrites the remote file (a different length, so the daemon's
    // size-based stat sees the change). No `:checktime` — the daemon's watch detects it.
    fake.set("/virtual/note.txt", "alpha\nbeta\ngamma\n");

    await_lines(&rpc, &["alpha", "beta", "gamma"]).await;
}

/// The `FileChangedShell` round-trip works **over the wire**: with `'noautoread'`, the
/// edit-host fires `FileChangedShell` (with `v:fcs_reason` set) for a daemon-pushed
/// change, and the handler's `v:fcs_choice = "reload"` drives the off-tick re-fetch.
#[tokio::test]
async fn file_changed_shell_handler_reloads_over_the_daemon_watch() {
    let fake = DaemonFs::with("/virtual/doc.txt", "first\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/doc.txt").await;
    await_lines(&rpc, &["first"]).await;

    // 'noautoread' so the change won't silently reload — it must route through the
    // handler. Register a handler that records the reason and redirects to a reload.
    exec_lua(
        &rpc,
        r#"
        vim.o.autoread = false
        vim.g.fcs_reason = ""
        vim.api.nvim_create_autocmd("FileChangedShell", {
          callback = function()
            vim.g.fcs_reason = vim.v.fcs_reason
            vim.v.fcs_choice = "reload"
          end,
        })
        "#,
    )
    .await;

    fake.set("/virtual/doc.txt", "second\nthird\n");

    // The handler's "reload" choice re-fetches the new bytes over the wire...
    await_lines(&rpc, &["second", "third"]).await;
    // ...and it saw v:fcs_reason = "changed" (unmodified buffer, file present).
    assert_eq!(
        exec_lua(&rpc, "return vim.g.fcs_reason").await.as_str(),
        Some("changed"),
        "the FileChangedShell handler must see v:fcs_reason over the wire"
    );
}
