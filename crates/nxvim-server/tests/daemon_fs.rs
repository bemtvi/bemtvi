//! The daemon wire protocol, filesystem half (edit-host split, Phase 3 of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Proves the **initial buffer** is fetched over a real wire, off the editor tick:
//! an editor given a [`RemoteHostFs`](nxvim_server::RemoteHostFs) as its async fs
//! starts empty, the server requests the startup file from a
//! [`serve_fs_daemon`](nxvim_server::serve_fs_daemon) over an in-process
//! `tokio::io::duplex`, and the bytes load into a replica buffer. The duplex stands
//! in for the eventual ssh stdio to `nxvim --daemon`.
//!
//! Faithful, not a no-op: the path is `/virtual/...`, which the edit-host's *local*
//! disk cannot read — so the content appearing in the buffer can only have come
//! across the wire from the daemon's fs (the same argument `host_fs.rs` makes for the
//! sync seam). The `attach` handshake completes before the file loads, evidence the
//! fetch did not block startup. A second test proves a not-yet-existing path opens as
//! an empty new-file buffer (not an error), and its name is bound for a later `:w`.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on
//! `nvim_buf_get_lines` / the buffer name.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{
    attach, buf_lines, command, exec_lua, map_get, spawn, wait_redraw, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory [`HostFs`] for the **daemon** side: path → bytes. Unlike a generic
/// fake, [`read_dir`](HostFs::read_dir) errors on every path (it models no
/// directories), so the daemon's file/dir/new classification resolves a stored path
/// to a file and an absent one to a new-file — never mistaking a file for a directory.
#[derive(Clone, Default)]
struct DaemonFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl DaemonFs {
    fn with(path: &str, contents: &str) -> Self {
        let me = DaemonFs::default();
        me.files
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
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
        // No directories in this fake: a file path must not classify as one.
        Err(io::Error::new(io::ErrorKind::NotFound, "not a directory"))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// Start a server whose async fs is a [`RemoteHostFs`] talking to a
/// [`serve_fs_daemon`] (backed by `fake`) over an in-process duplex, opening `file`.
/// UI-attached. The daemon task and the remote fs's RPC tasks live on the test
/// runtime; the server runs on its own thread and reaches the daemon only through the
/// injected async fs. The client's notification receiver is returned (not dropped):
/// dropping it would tear the client connection down and stop the server.
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
    // `attach` returning proves startup did not block on the (deferred) file fetch.
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out — the
/// initial open lands off-tick, a moment after attach, so a bounded retry beats a
/// fixed sleep.
async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        let lines = buf_lines(rpc, 0).await;
        if lines == want {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// The startup file's bytes are fetched over the wire and loaded into the first
/// buffer — content from a `/virtual/...` path the edit-host's local disk can't read,
/// so it can only have crossed the daemon wire.
#[tokio::test]
async fn initial_buffer_is_fetched_over_the_daemon_wire() {
    let fake = DaemonFs::with("/virtual/note.txt", "fetched\nover\nthe\nwire\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;

    assert_eq!(
        await_lines(&rpc, &["fetched", "over", "the", "wire"]).await,
        vec!["fetched", "over", "the", "wire"],
        "the buffer must hold the bytes the daemon served over the wire"
    );
    // The buffer is named for the remote path, the way an opened file is.
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)")
            .await
            .as_str(),
        Some("/virtual/note.txt"),
        "the replica buffer must carry the remote path as its name"
    );
}

/// A not-yet-existing remote path opens as an empty new-file buffer (not an error),
/// with its name bound — the `:e newfile` case, so a first `:w` would create it.
#[tokio::test]
async fn missing_path_opens_a_new_file_buffer() {
    let fake = DaemonFs::default(); // serves nothing → the path is "new"
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/fresh.txt").await;

    // Wait for the name to bind (the off-tick open), then assert the buffer is empty.
    for _ in 0..100 {
        let name = exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)")
            .await
            .as_str()
            .map(str::to_string);
        if name.as_deref() == Some("/virtual/fresh.txt") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_buf_get_name(0)")
            .await
            .as_str(),
        Some("/virtual/fresh.txt"),
        "a missing remote file still binds its name (a new-file buffer)"
    );
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec![""],
        "a new-file buffer is empty, not an error or stale content"
    );
}

/// In a daemon session an image preview's bytes live on the remote host, so the
/// redraw marks the marker `remote = true` and the native client fetches the bytes
/// over `nxvim_image_read` — content from a `/virtual/...` path the edit-host's local
/// disk can't read, so it can only have crossed the daemon wire. (The editor — and so
/// the marker's `path` — runs local; only the bytes are remote.)
#[tokio::test]
async fn image_preview_is_remote_and_bytes_fetch_over_the_wire() {
    let fake = DaemonFs::with("/virtual/note.txt", "plain\n");
    fake.files
        .lock()
        .unwrap()
        .insert(PathBuf::from("/virtual/pic.png"), b"PNGBYTES\n".to_vec());
    // Open a plain buffer at startup, enable previews, then edit the remote image.
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    exec_lua(&rpc, "nx.o.imagepreview = true").await;
    command(&rpc, "edit /virtual/pic.png").await;

    let frame = wait_redraw(&mut incoming, |m| {
        matches!(window0_field(m, "image"), Some(Value::Map(_)))
    })
    .await;
    let Some(Value::Map(img)) = window0_field(&frame, "image") else {
        panic!("the redraw window carries an image marker");
    };
    assert_eq!(
        map_get(img, "path").and_then(Value::as_str),
        Some("/virtual/pic.png"),
        "the marker carries the remote image path"
    );
    assert_eq!(
        map_get(img, "remote").and_then(Value::as_bool),
        Some(true),
        "a daemon session marks the image preview remote"
    );

    let reply = rpc
        .request("nxvim_image_read", vec![Value::from("/virtual/pic.png")])
        .await
        .expect("nxvim_image_read responds");
    assert_eq!(
        reply,
        Value::Binary(b"PNGBYTES\n".to_vec()),
        "nxvim_image_read returns the bytes the daemon served over the wire"
    );
}
