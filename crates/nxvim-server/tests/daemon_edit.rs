//! The daemon wire protocol, filesystem half — **`:edit` over the wire** (edit-host
//! split, Phase 3f of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_fs.rs` (initial open) and `daemon_save.rs` (save). Here a real
//! editor whose async fs is a [`RemoteHostFs`](nxvim_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](nxvim_server::serve_fs_daemon) over an in-process duplex opens a
//! *second* file at runtime via `:edit` — fetched **over the wire, off the editor tick**
//! through the same `HostFsAsync` + replica path the initial open uses:
//!
//! - `:e /virtual/other.txt` fills a new buffer with bytes the edit-host's local disk
//!   can't hold (the `/virtual/...` path), so they can only have crossed the wire.
//! - `:e /virtual/fresh.txt` on a not-yet-existing path opens an empty new-file buffer.
//! - `:e!` reload-in-place **refetches** over the wire (a content change on the daemon
//!   shows up after the reload — proving a real re-read, not just a local-edit discard).
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on
//! buffer lines and the buffer name.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, buf_lines, exec_lua, feed, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory multi-file [`HostFs`] for the **daemon** side: path → bytes.
/// `read_dir` errors on every path (it models no directories), so a stored path
/// classifies as a file and an absent one as a new-file — never a directory.
#[derive(Clone, Default)]
struct DaemonFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
}

impl DaemonFs {
    /// Store (or overwrite) `path`'s contents — both the test's initial seeding and a
    /// mid-test mutation a `:e!` reload should then see across the wire.
    fn set(&self, path: &str, contents: &str) -> &Self {
        self.set_bytes(path, contents.as_bytes())
    }

    /// Store raw `bytes` (not necessarily valid UTF-8) — for the encoding-seam test
    /// that a non-UTF-8 file opens identically over the wire as it does locally.
    fn set_bytes(&self, path: &str, bytes: &[u8]) -> &Self {
        self.files
            .lock()
            .unwrap()
            .insert(PathBuf::from(path), bytes.to_vec());
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
        Err(io::Error::new(io::ErrorKind::NotFound, "not a directory"))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }
}

/// Start a server whose async fs is a [`RemoteHostFs`] talking to a `serve_fs_daemon`
/// (backed by `fake`) over an in-process duplex, opening `file`. UI-attached. The
/// notification receiver is returned (not dropped: dropping it would stop the server).
async fn spawn_with_daemon_fs(fake: DaemonFs, file: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    spawn_with_daemon_fs_home(fake, file, None).await
}

/// [`spawn_with_daemon_fs`] with the daemon's home seeded (`ServerInit::remote_home`) —
/// what the `config_bundle` handshake carries in a real session — so a leading `~` in a
/// file argument expands against the daemon's home rather than the edit-host's local
/// `$HOME`.
async fn spawn_with_daemon_fs_home(
    fake: DaemonFs,
    file: &str,
    remote_home: Option<&str>,
) -> (Rpc, UnboundedReceiver<Incoming>) {
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
        remote_home: remote_home.map(PathBuf::from),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out — the
/// off-tick fetch (initial open or `:edit`) lands a moment after the command.
async fn await_lines(rpc: &Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        if buf_lines(rpc, 0).await == want {
            return buf_lines(rpc, 0).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// The current buffer's name (`nvim_buf_get_name(0)`).
async fn buf_name(rpc: &Rpc) -> String {
    exec_lua(rpc, "return vim.api.nvim_buf_get_name(0)")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A leading `~` in a file argument expands against the **daemon's** home, not the
/// edit-host's local `$HOME` — the file read lands on the daemon even though the core
/// runs on the client. The session is seeded with the daemon's home (`/daemon/home`,
/// what the `config_bundle` handshake carries in a real remote session); `:e ~/other.txt`
/// must fetch `/daemon/home/other.txt` over the wire. Expanding against the local `$HOME`
/// would name a path the daemon fs doesn't have, so the buffer would come back empty —
/// exactly the mutation this guards against.
#[tokio::test]
async fn edit_expands_tilde_against_the_daemon_home() {
    let fake = DaemonFs::default();
    fake.set("/daemon/home/note.txt", "start\n")
        .set("/daemon/home/other.txt", "daemon\nhome\nfile\n");
    let (rpc, _incoming) =
        spawn_with_daemon_fs_home(fake, "/daemon/home/note.txt", Some("/daemon/home")).await;
    await_lines(&rpc, &["start"]).await;

    feed(&rpc, ":edit ~/other.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["daemon", "home", "file"]).await,
        vec!["daemon", "home", "file"],
        "`:e ~/…` must expand against the daemon's home and fetch that file over the wire"
    );
    assert_eq!(
        buf_name(&rpc).await,
        "/daemon/home/other.txt",
        "the buffer is named for the daemon-home-expanded path, not a local $HOME expansion"
    );
}

/// `:e /virtual/other.txt` fetches a *second* file's bytes over the wire into a new
/// buffer — content the edit-host's local disk can't hold, so it crossed the wire.
#[tokio::test]
async fn edit_fetches_a_second_file_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/other.txt", "second\nfile\nhere\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":edit /virtual/other.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["second", "file", "here"]).await,
        vec!["second", "file", "here"],
        "`:edit` must fill the buffer with the second file's bytes from over the wire"
    );
    assert_eq!(
        buf_name(&rpc).await,
        "/virtual/other.txt",
        "the buffer is named for the edited remote path"
    );
}

/// A non-UTF-8 file fetched **over the wire** decodes through the same encoding seam
/// as a local open: the daemon path used to `from_utf8_lossy` (silently mangling the
/// bytes), now it routes the raw bytes through `decode_to_rope` exactly like
/// `Buffer::from_file` — so latin1's 0xe9 shows as `é` and the fileencoding is `latin1`,
/// matching the local `encoding` suite. This is the local↔daemon-agree guarantee.
#[tokio::test]
async fn nonutf8_file_decodes_over_the_wire_like_local() {
    let fake = DaemonFs::default();
    fake.set_bytes("/virtual/latin1.txt", b"caf\xe9\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/latin1.txt").await;

    assert_eq!(
        await_lines(&rpc, &["café"]).await,
        vec!["café"],
        "0xe9 must decode to é over the wire (no from_utf8_lossy mangling)"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.fileencoding")
            .await
            .as_str()
            .unwrap_or_default(),
        "latin1",
        "the remote replica carries the detected fileencoding, like a local open"
    );
}

/// `:e /virtual/fresh.txt` on a path the daemon doesn't have opens an empty new-file
/// buffer (not an error), with its name bound for a later `:w`.
#[tokio::test]
async fn edit_missing_path_opens_a_new_file_buffer() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":edit /virtual/fresh.txt<CR>");
    // Wait for the name to bind (the off-tick open), then assert the buffer is empty.
    for _ in 0..100 {
        if buf_name(&rpc).await == "/virtual/fresh.txt" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        buf_name(&rpc).await,
        "/virtual/fresh.txt",
        "a missing remote file still binds its name (a new-file buffer)"
    );
    assert_eq!(
        buf_lines(&rpc, 0).await,
        vec![""],
        "a new-file buffer is empty, not an error or stale content"
    );
}

/// `:e!` reload-in-place **refetches** over the wire: a content change made on the
/// daemon after the file was opened shows up after the reload — proving a real
/// re-read, not merely a discard of local edits.
#[tokio::test]
async fn edit_reload_refetches_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "original\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["original"]).await;

    // Make a local edit (so the buffer is modified)...
    feed(&rpc, "ggIlocal <Esc>");
    assert_eq!(
        await_lines(&rpc, &["local original"]).await,
        vec!["local original"]
    );

    // ...and meanwhile the file changed on the daemon. A `:e!` of the current file
    // must refetch *that* (nxvim's `:edit` needs the path; a bare `:e!` is `E32`).
    fake.set("/virtual/note.txt", "changed on the daemon\n");
    feed(&rpc, ":edit! /virtual/note.txt<CR>");
    assert_eq!(
        await_lines(&rpc, &["changed on the daemon"]).await,
        vec!["changed on the daemon"],
        "`:e!` must refetch the file over the wire (the daemon's new content), \
         not just discard the local edit"
    );
}

/// `:tabnew /virtual/other.txt` opens the remote file in a **new tab**, fetched over the
/// wire — `:tabnew` was the last user-command `from_file` site that bypassed the off-tick
/// path (Phase 3h unifies it onto the shared open kernel). The `/virtual/...` path can't
/// be read from the edit-host's local disk, so the new tab's content crossed the wire.
#[tokio::test]
async fn tabnew_fetches_a_file_over_the_wire() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/other.txt", "tab\ncontent\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    feed(&rpc, ":tabnew /virtual/other.txt<CR>");
    // The new (now-current) tab's buffer fills with the remote file's bytes...
    assert_eq!(
        await_lines(&rpc, &["tab", "content"]).await,
        vec!["tab", "content"],
        "`:tabnew` fills the new tab's buffer with the remote file's bytes over the wire"
    );
    // ...there really are two tab pages now (not an in-place `:edit`)...
    let tab_count = exec_lua(&rpc, "return #vim.api.nvim_list_tabpages()")
        .await
        .as_u64()
        .unwrap_or(0);
    assert_eq!(tab_count, 2, "`:tabnew` opened a second tab page");
    // ...and the new tab's buffer is named for the remote path.
    assert_eq!(buf_name(&rpc).await, "/virtual/other.txt");
}

/// An **existing** remote file opened off-tick fires `BufReadPost` when its content lands —
/// the daemon stats the file at read and ships the stat, so the replica gets a `disk`
/// baseline (without it `buffer_is_new_file` is true and the content-load fires `BufNewFile`
/// too, so a config that seeds diagnostics / attaches an LSP on `BufReadPost` never runs).
/// The diagnostic the handler seeds therefore lands on the buffer. (The off-tick `:edit`
/// creates the empty buffer before the fetch returns, so a placeholder `BufNewFile` precedes
/// the content-load `BufReadPost` — a pre-existing artifact of the off-tick split, not this
/// fix's concern; the regression guard is that the load now fires `BufReadPost`.) A genuinely
/// new remote path still fires only `BufNewFile`, proving the distinction is real.
#[tokio::test]
async fn edit_existing_remote_file_fires_bufreadpost_not_bufnewfile() {
    let fake = DaemonFs::default();
    fake.set("/virtual/note.txt", "alpha\n")
        .set("/virtual/lint.txt", "needs linting\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake, "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    // Tap both events (pattern matches either remote file), then seed a diagnostic from the
    // BufReadPost handler — exactly the shape of a real `nx.diagnostic.set`-on-read config.
    exec_lua(
        &rpc,
        r#"
        _G.events = {}
        local ns = nx.ns.create("daemon-read-test")
        nx.autocmd.create({ "BufReadPost" }, { pattern = "*lint.txt", callback = function(a)
          _G.events[#_G.events + 1] = "BufReadPost"
          nx.diagnostic.set(ns, a.buf, { { lnum = 0, col = 0, severity = 1, message = "lint!" } })
        end })
        nx.autocmd.create({ "BufNewFile" }, { pattern = "*lint.txt", callback = function()
          _G.events[#_G.events + 1] = "BufNewFile"
        end })
        return 1
        "#,
    )
    .await;

    // Open the existing remote file (fetched + stat'd over the wire, off-tick).
    feed(&rpc, ":edit /virtual/lint.txt<CR>");
    await_lines(&rpc, &["needs linting"]).await;

    let events = exec_lua(&rpc, "return table.concat(_G.events, ',')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        events.split(',').next_back() == Some("BufReadPost"),
        "the content-load of an existing remote file must fire BufReadPost (got {events:?})"
    );

    // End-to-end: the BufReadPost handler's diagnostic landed on the opened buffer.
    assert_eq!(
        exec_lua(&rpc, "return #nx.diagnostic.get(0)")
            .await
            .as_u64(),
        Some(1),
        "the BufReadPost handler seeded its diagnostic on the remote file"
    );

    // The control: a genuinely-new remote path still fires BufNewFile.
    exec_lua(&rpc, "_G.events = {}").await;
    feed(&rpc, ":edit /virtual/fresh-lint.txt<CR>");
    for _ in 0..100 {
        if buf_name(&rpc).await == "/virtual/fresh-lint.txt" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let events = exec_lua(&rpc, "return table.concat(_G.events, ',')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        events.split(',').next_back() == Some("BufNewFile") && !events.contains("BufReadPost"),
        "a missing remote path is still a new file (fires BufNewFile, never BufReadPost; got {events:?})"
    );
}
