//! The `HostFs` injection seam (edit-host split, Phase 3a of
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Proves the server routes *all* buffer I/O — including the **initial** file —
//! through the [`HostFs`] handed to it in [`ServerInit::host_fs`], not `std::fs`.
//! A fake in-memory fs both serves the startup buffer and captures `:w`, so a
//! later remote/daemon backend can swap in at exactly this seam: editing stays
//! local, bytes cross the injected fs.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, driven by
//! `nx_input` / `nx_command`, asserting on `nvim_buf_get_lines` and on what
//! the fake fs observed.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, buf_lines, spawn};
use rmpv::Value;

/// An in-memory [`HostFs`]: path -> bytes, behind a shared `Arc<Mutex<…>>` so the
/// test thread keeps a handle to inspect what the server read and wrote. `Send`
/// (so it can ride [`ServerInit`] onto the server thread) without being the real
/// disk.
#[derive(Clone, Default)]
struct FakeFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl FakeFs {
    fn with(path: &str, contents: &str) -> Self {
        let me = FakeFs::default();
        me.files
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
        me
    }

    /// The current bytes stored under `path`, as a string (for assertions).
    fn read(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap()
            .get(Path::new(path))
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }
}

impl HostFs for FakeFs {
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
        Ok(Vec::new())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// The startup file is fetched through the injected fs — the bytes the fake
/// serves (never touching disk; `/virtual/...` does not exist) land in the first
/// buffer.
#[tokio::test]
async fn initial_buffer_is_read_through_the_injected_host_fs() {
    let fake = FakeFs::with("/virtual/note.txt", "alpha\nbeta\n");
    let init = ServerInit {
        file: Some("/virtual/note.txt".to_string()),
        host_fs: Some(Box::new(fake.clone())),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    assert_eq!(buf_lines(&rpc, 0).await, vec!["alpha", "beta"]);
}

/// `:w` writes back through the injected fs — the fake captures the edited bytes,
/// proving the save path also crosses the seam (not `std::fs`).
#[tokio::test]
async fn write_goes_back_through_the_injected_host_fs() {
    let fake = FakeFs::with("/virtual/note.txt", "alpha\nbeta\n");
    let init = ServerInit {
        file: Some("/virtual/note.txt".to_string()),
        host_fs: Some(Box::new(fake.clone())),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Edit line 1 ("alpha" -> "alphaX"), then save. The awaited command is the
    // barrier: the write has run through the fs by the time it returns.
    rpc.request("nx_input", vec![Value::from("AX<Esc>")])
        .await
        .expect("input");
    rpc.request("nx_command", vec![Value::from("w")])
        .await
        .expect("write");

    assert_eq!(
        fake.read("/virtual/note.txt").as_deref(),
        Some("alphaX\nbeta\n"),
        "the save must land in the injected fs, with the edit applied"
    );
}

/// A bare session (no startup file) still installs the injected fs, so a later
/// `:write <path>` routes through it rather than the disk.
#[tokio::test]
async fn bare_session_routes_a_later_write_through_the_injected_host_fs() {
    let fake = FakeFs::default();
    let init = ServerInit {
        file: None,
        host_fs: Some(Box::new(fake.clone())),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    rpc.request("nx_input", vec![Value::from("ihello<Esc>")])
        .await
        .expect("input");
    rpc.request("nx_command", vec![Value::from("write /virtual/out.txt")])
        .await
        .expect("write");

    assert_eq!(
        fake.read("/virtual/out.txt").as_deref(),
        Some("hello\n"),
        "a `:write path` on a bare session must use the injected fs"
    );
}
