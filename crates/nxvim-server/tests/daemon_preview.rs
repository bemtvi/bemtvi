//! Picker **preview over the daemon wire** (the off-tick-fs half of
//! `docs/plans/2026-06-24-remote-web-pickers.md`, Phase 2b).
//!
//! A picker preview of an UN-loaded file used to be stuck `"<path>: loading…"` on a
//! daemon / the web client: the preview read went through the synchronous host FS, which
//! is off-tick there. Now `ensure_preview` fetches the file over the same `fs_fetch`
//! seam `:edit` uses (tagged with a reserved buffer id so its landing routes to the
//! preview cache, not a buffer) and repaints when it lands.
//!
//! Faithful, not a no-op: a real editor whose async fs is a `RemoteHostFs` talking to a
//! `serve_fs_daemon` over an in-process duplex previews a file that the edit-host's local
//! disk can't hold (a `/virtual/...` path) — so its content can only have crossed the
//! wire. Black-box: assert on the `preview` pane in the `redraw` frame.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, barrier, drain_to_latest_redraw, exec_lua, map_get, spawn};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory multi-file [`HostFs`] for the daemon side: path → bytes. `read_dir`
/// errors everywhere (no directories), so a stored path is a file and an absent one a
/// new-file.
#[derive(Clone, Default)]
struct DaemonFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl DaemonFs {
    fn set(&self, path: &str, contents: &str) -> &Self {
        self.files
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), contents.as_bytes().to_vec());
        self
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
        Err(io::Error::new(io::ErrorKind::NotFound, "no dirs"))
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// A server whose async fs is a `RemoteHostFs` over an in-process duplex to a
/// `serve_fs_daemon(fake)` — off-tick fs on, like a daemon / the browser. `config_dir`
/// loads `init_lua` so a test can register a custom picker source.
async fn spawn_with_daemon_fs(
    fake: DaemonFs,
    file: &str,
    config_dir: &Path,
    init_lua: &str,
    remote_cwd: Option<&str>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(config_dir.join("init.lua"), init_lua).expect("write init.lua");
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
        config_dir: Some(config_dir.to_path_buf()),
        runtimepath: vec![config_dir.to_path_buf()],
        remote_cwd: remote_cwd.map(PathBuf::from),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll for the latest redraw whose `menu.preview.lines` are present, returning those
/// lines. The off-tick preview fetch lands a moment after the picker opens, so this
/// retries until the placeholder is replaced (or the budget runs out).
async fn poll_preview_lines(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    until: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..100 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            if let Some(Value::Map(menu)) = map_get(&map, "menu") {
                if let Some(Value::Map(preview)) = map_get(menu, "preview") {
                    if let Some(Value::Array(a)) = map_get(preview, "lines") {
                        last = a
                            .iter()
                            .map(|v| v.as_str().unwrap_or("").to_string())
                            .collect();
                        if until(&last) {
                            return last;
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    last
}

/// A custom file-preview source pointing at an un-loaded `/virtual` file: the picker
/// preview must fetch it over the wire and show its content, not a "loading…" stub.
#[tokio::test]
async fn picker_preview_fetches_an_unloaded_file_over_the_wire() {
    let dir = nxvim_test_harness::temp_dir("daemon_preview");
    let fake = DaemonFs::default();
    fake.set("/virtual/start.txt", "start\n")
        .set("/virtual/target.txt", "PREVIEW CONTENT\nsecond line\n");
    let init_lua = r#"
nx.picker.source {
  name = "preview_test",
  preview = "file",
  items = function(ctx)
    ctx.push { text = "target", path = "/virtual/target.txt" }
  end,
  confirm = function() end,
}
"#;
    let (rpc, mut incoming) =
        spawn_with_daemon_fs(fake, "/virtual/start.txt", &dir, init_lua, None).await;

    exec_lua(&rpc, "nx.picker.open('preview_test')").await;

    // The preview pane fills with the file's content fetched over the wire — never the
    // un-loaded target's path can be read off the edit-host's (empty) local disk.
    let lines = poll_preview_lines(&rpc, &mut incoming, |l| {
        l.first().is_some_and(|s| s.contains("PREVIEW CONTENT"))
    })
    .await;
    assert!(
        lines.first().is_some_and(|s| s.contains("PREVIEW CONTENT")),
        "the preview must show the over-the-wire fetched content, got {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A RELATIVE preview target (what `rg`/`grep`/nx.fs emit) is resolved against the
/// effective cwd before the off-tick fetch — without that, the daemon read (which has no
/// session cwd) would miss the file and the preview would never land. The session's cwd
/// is the remote `/virtual`, so `target.txt` must fetch `/virtual/target.txt`.
#[tokio::test]
async fn relative_preview_target_resolves_against_the_remote_cwd() {
    let dir = nxvim_test_harness::temp_dir("daemon_preview_rel");
    let fake = DaemonFs::default();
    fake.set("/virtual/start.txt", "start\n")
        .set("/virtual/target.txt", "RELATIVE RESOLVED\n");
    let init_lua = r#"
nx.picker.source {
  name = "rel_test",
  preview = "file",
  items = function(ctx) ctx.push { text = "target", path = "target.txt" } end,
  confirm = function() end,
}
"#;
    let (rpc, mut incoming) =
        spawn_with_daemon_fs(fake, "/virtual/start.txt", &dir, init_lua, Some("/virtual")).await;

    exec_lua(&rpc, "nx.picker.open('rel_test')").await;

    let lines = poll_preview_lines(&rpc, &mut incoming, |l| {
        l.first().is_some_and(|s| s.contains("RELATIVE RESOLVED"))
    })
    .await;
    assert!(
        lines.first().is_some_and(|s| s.contains("RELATIVE RESOLVED")),
        "a relative target must resolve against the remote cwd and fetch over the wire, got {lines:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
