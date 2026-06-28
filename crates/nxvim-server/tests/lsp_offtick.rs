//! A project-wide LSP `WorkspaceEdit` (rename / code action) must reach files that
//! aren't open in a buffer **even in a daemon / web session**, where an unopened
//! file's bytes live across the wire and can only be fetched off the editor tick.
//!
//! The reachable seam without a real stdio server is `nx._lsp_apply_workspace_edit`
//! (the Lua entry behind `vim.lsp.util.apply_workspace_edit`): it hands an LSP-shape
//! `WorkspaceEdit` into the same `apply_workspace_edit` path a native rename reply
//! uses. Driving it under an **async daemon fs** (so `host_fs_offtick` is on)
//! exercises the deferred-apply path: the unopened file's replica buffer is created,
//! its fetch enqueued, the edits stashed, and applied once the bytes land.
//!
//! Faithful, not a no-op: the unopened file's path is `/virtual/...`, which the
//! edit-host's *local* disk cannot read — so the renamed content appearing in its
//! buffer can only have come across the wire from the daemon's fs.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, buf_lines, command, exec_lua, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory [`HostFs`] for the **daemon** side: path -> bytes. `read_dir` errors
/// on every path (it models no directories), so a stored path classifies as a file
/// and an absent one as a new-file — never a directory. (Mirrors the fake in
/// `daemon_fs.rs`.)
#[derive(Clone, Default)]
struct DaemonFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl DaemonFs {
    fn with(entries: &[(&str, &str)]) -> Self {
        let me = DaemonFs::default();
        let mut map = me.files.lock().unwrap();
        for (path, contents) in entries {
            map.insert(PathBuf::from(path), contents.as_bytes().to_vec());
        }
        drop(map);
        me
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
/// (backed by `fake`) over an in-process duplex, opening `file`. UI-attached. The
/// `incoming` receiver is returned (not dropped): dropping it tears the connection
/// down and stops the server.
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

/// Poll the current buffer's lines until they match `want` or the budget runs out.
async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..150 {
        let got = buf_lines(rpc, 0).await;
        if got == want {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// A rename's `WorkspaceEdit` touches the open file *and* a file that was never
/// opened, in a daemon session. The unopened file's bytes are fetched over the wire,
/// then the stashed edits apply to its replica buffer — so a project-wide rename
/// reaches unopened files off-tick, not just locally.
#[tokio::test]
async fn workspace_edit_reaches_an_unopened_file_off_tick() {
    let fake = DaemonFs::with(&[
        ("/virtual/a.rs", "let foo = 1\n"),
        ("/virtual/b.rs", "use a::foo;\nfn g() { foo() }\n"),
    ]);
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/a.rs").await;

    // The startup file fetches over the wire first.
    assert_eq!(
        await_lines(&rpc, &["let foo = 1"]).await,
        vec!["let foo = 1"],
        "the open file should load over the daemon wire"
    );

    // Rename `foo` -> `bar`: one occurrence in the open `a.rs`, two in the unopened
    // `b.rs` (which has never been opened in a buffer).
    let edit = "nx._lsp_apply_workspace_edit({ changes = { \
        ['file:///virtual/a.rs'] = { \
          { range = { start = { line = 0, character = 4 }, ['end'] = { line = 0, character = 7 } }, newText = 'bar' } }, \
        ['file:///virtual/b.rs'] = { \
          { range = { start = { line = 0, character = 7 }, ['end'] = { line = 0, character = 10 } }, newText = 'bar' }, \
          { range = { start = { line = 1, character = 9 }, ['end'] = { line = 1, character = 12 } }, newText = 'bar' } } } })";
    exec_lua(&rpc, edit).await;

    // The open buffer is rewritten synchronously.
    assert_eq!(
        await_lines(&rpc, &["let bar = 1"]).await,
        vec!["let bar = 1"],
        "the open file should be renamed in place"
    );

    // The unopened `b.rs` was brought into a replica buffer, its bytes fetched over
    // the wire, and the stashed edits applied once they landed. Switch to it (the
    // edit created the buffer, so `:edit` reuses it) and check both occurrences.
    command(&rpc, "edit /virtual/b.rs").await;
    assert_eq!(
        await_lines(&rpc, &["use a::bar;", "fn g() { bar() }"]).await,
        vec!["use a::bar;", "fn g() { bar() }"],
        "the rename should reach both occurrences in the unopened, off-tick file"
    );
}
