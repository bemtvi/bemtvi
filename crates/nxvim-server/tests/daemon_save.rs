//! The daemon wire protocol, filesystem half — the **save path** (edit-host split,
//! Phase 3e of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_fs.rs` (which proved the off-tick *read*). Here a real editor
//! whose async fs is a [`RemoteHostFs`](nxvim_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](nxvim_server::serve_fs_daemon) over an in-process duplex writes
//! a buffer back **over the wire, off the editor tick** — and the contract holds:
//!
//! - `:w` pushes the *edited* bytes to the daemon (a `/virtual/...` path the
//!   edit-host's local disk can't hold, so the bytes can only have crossed the wire),
//!   and the `modified` flag clears **only after the daemon acks** (ack-gated state).
//! - `:wq` defers its quit until the write acks, then exits — and a *failing* write
//!   **cancels** the quit and leaves the buffer modified (quit waits for the ack).
//! - A write failure surfaces **loudly** (no silent drop) and the buffer stays dirty.
//!
//! Black-box like the rest: a real server over the in-process RPC pipe, asserting on
//! buffer lines, `vim.bo.modified`, the daemon's stored bytes, and the redraw message.

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHostFs, ServerInit};
use nxvim_test_harness::{attach, buf_lines, exec_lua, feed, message_of, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// An in-memory [`HostFs`] for the **daemon** side: path → bytes, plus a switch that
/// makes every write fail (to exercise the loud-failure / quit-cancel contract).
/// `read_dir` errors on every path (it models no directories), so a stored path
/// classifies as a file and an absent one as a new-file — never a directory.
#[derive(Clone, Default)]
struct DaemonFs {
    files: Arc<Mutex<HashMap<PathBuf, Vec<u8>>>>,
    fail_writes: bool,
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

    /// Seed several `(path, contents)` files at once — for the multi-buffer `:wall` /
    /// `:wqa` tests, which open and edit more than one remote file.
    fn with_files(entries: &[(&str, &str)]) -> Self {
        let me = DaemonFs::default();
        {
            let mut files = me.files.lock().unwrap();
            for (path, contents) in entries {
                files.insert(PathBuf::from(*path), contents.as_bytes().to_vec());
            }
        }
        me
    }

    /// The bytes currently stored at `path`, as a string (the daemon's view of the
    /// file the editor wrote across the wire). `None` if nothing is stored there.
    fn content(&self, path: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap()
            .get(Path::new(path))
            .map(|b| String::from_utf8_lossy(b).into_owned())
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
        if self.fail_writes {
            // A loud failure the edit-host must surface — never a silent success.
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon refuses the write",
            ));
        }
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

/// Poll `nvim_buf_get_lines` until it matches `want` (the off-tick initial open lands
/// a moment after attach), so a test can edit a loaded buffer rather than racing it.
async fn await_lines(rpc: &Rpc, want: &[&str]) {
    for _ in 0..100 {
        if buf_lines(rpc, 0).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(buf_lines(rpc, 0).await, want, "initial buffer never loaded");
}

/// Whether the current buffer reports `modified` (the mirrored `vim.bo.modified`).
async fn modified(rpc: &Rpc) -> bool {
    exec_lua(rpc, "return vim.bo.modified").await.as_bool() == Some(true)
}

/// Whether **any** loaded buffer reports `modified` — for the multi-buffer `:wall` /
/// `:wqa` tests, where more than one buffer is in flight at once.
async fn any_modified(rpc: &Rpc) -> bool {
    exec_lua(
        rpc,
        "for _, b in ipairs(vim.api.nvim_list_bufs()) do \
           if vim.bo[b].modified then return true end \
         end \
         return false",
    )
    .await
    .as_bool()
        == Some(true)
}

/// Poll until `fake` holds `want` at `path` (an off-tick write lands a moment after the
/// command), then assert it — the multi-buffer analogue of the inline poll in
/// `write_pushes_edited_bytes_…`.
async fn await_daemon_content(fake: &DaemonFs, path: &str, want: &str) {
    for _ in 0..100 {
        if fake.content(path).as_deref() == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        fake.content(path).as_deref(),
        Some(want),
        "the daemon never received the bytes for {path}"
    );
}

/// Open `/virtual/a.txt`, edit it dirty, then `:edit /virtual/b.txt` and edit *it* dirty
/// — leaving two modified file-backed buffers, both loaded over the wire. Returns the
/// daemon fake and the live client. The edits prepend a marker so the written bytes are
/// content a stub couldn't invent and the local disk can't hold (the `/virtual/...`
/// faithfulness argument the rest of this suite makes).
async fn two_edited_buffers(fail_writes: bool) -> (DaemonFs, Rpc, UnboundedReceiver<Incoming>) {
    let fake = DaemonFs {
        fail_writes,
        ..DaemonFs::with_files(&[("/virtual/a.txt", "aaa\n"), ("/virtual/b.txt", "bbb\n")])
    };
    let (rpc, incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/a.txt").await;
    await_lines(&rpc, &["aaa"]).await;
    feed(&rpc, "ggIA <Esc>");
    feed(&rpc, ":edit /virtual/b.txt<CR>");
    await_lines(&rpc, &["bbb"]).await;
    feed(&rpc, "ggIB <Esc>");
    assert!(
        any_modified(&rpc).await,
        "both edited buffers should be modified before the write"
    );
    (fake, rpc, incoming)
}

/// `:wall` writes **every** modified file-backed buffer over the wire — each buffer's
/// edited bytes land on the daemon (off-tick, concurrently), and every buffer reads
/// clean once its ack arrives. The multi-buffer companion to the single-buffer `:w`.
#[tokio::test]
async fn wall_writes_every_modified_buffer_over_the_wire() {
    let (fake, rpc, _incoming) = two_edited_buffers(false).await;

    feed(&rpc, ":wall<CR>");

    // Both files cross the wire with their own edits — a stub serving a constant couldn't
    // produce two distinct bodies, and neither path lives on the edit-host's local disk.
    await_daemon_content(&fake, "/virtual/a.txt", "A aaa\n").await;
    await_daemon_content(&fake, "/virtual/b.txt", "B bbb\n").await;

    // And every buffer reads clean once its write acked (ack-gated state, per buffer).
    for _ in 0..100 {
        if !any_modified(&rpc).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !any_modified(&rpc).await,
        "no buffer stays modified after `:wall` acks every write"
    );
}

/// `:wqa` writes every modified buffer over the wire **and then quits** — but only once
/// *all* the writes have acked (the all-buffers-ack-then-quit contract). Both files hold
/// their edited bytes on the daemon before the editor exits, proving the quit waited.
#[tokio::test]
async fn wqa_writes_all_buffers_then_quits() {
    let (fake, rpc, mut incoming) = two_edited_buffers(false).await;

    feed(&rpc, ":wqa<CR>");

    // The editor quits — but only after *both* off-tick writes ack (the gate fires `:qa`
    // when its seq-set drains). Either an `nxvim_exit` notification or the closed channel
    // counts.
    let quit = {
        let timeout = Duration::from_secs(2);
        loop {
            match tokio::time::timeout(timeout, incoming.recv()).await {
                Ok(None) => break true,
                Ok(Some(Incoming::Notification { method, .. })) if method == "nxvim_exit" => {
                    break true
                }
                Ok(Some(_)) => continue,
                Err(_) => break false,
            }
        }
    };
    assert!(quit, "`:wqa` quits once every off-tick write acks");
    // Both writes landed on the daemon before the quit fired (the quit waited for the
    // whole batch, not just the first).
    assert_eq!(
        fake.content("/virtual/a.txt").as_deref(),
        Some("A aaa\n"),
        "the first buffer's `:wqa` write landed before the quit"
    );
    assert_eq!(
        fake.content("/virtual/b.txt").as_deref(),
        Some("B bbb\n"),
        "the second buffer's `:wqa` write landed before the quit"
    );
}

/// A **failing** write in a `:wqa` batch **cancels** the whole quit (the multi-buffer
/// form of a failing `:wq` keeping the editor up): the editor stays running, the buffers
/// stay modified, the daemon gets nothing, and the failure surfaces loudly.
#[tokio::test]
async fn wqa_with_a_failing_write_cancels_the_quit() {
    let (fake, rpc, mut incoming) = two_edited_buffers(true).await;

    feed(&rpc, ":wqa<CR>");

    // A failed batch write must not exit the editor, and the failure must surface loudly
    // on the message line. Classify both signals in one bounded drain.
    let mut exited = false;
    let mut saw_failure = false;
    let deadline = Duration::from_millis(1200);
    loop {
        match tokio::time::timeout(deadline, incoming.recv()).await {
            Ok(None) => {
                exited = true;
                break;
            }
            Ok(Some(Incoming::Notification { method, params })) => {
                if method == "nxvim_exit" {
                    exited = true;
                    break;
                }
                if method == "redraw" && message_of(&params).contains("failed") {
                    saw_failure = true;
                }
            }
            Ok(Some(_)) => {}
            // Quiet window elapsed: the editor is still running (the expected outcome).
            Err(_) => break,
        }
    }
    assert!(!exited, "a failing `:wqa` write must not quit the editor");
    assert!(
        saw_failure,
        "the write failure must be surfaced loudly on the message line"
    );

    // The buffers stay dirty (no write cleared them) and the daemon holds neither edit.
    assert!(
        any_modified(&rpc).await,
        "a failed `:wqa` leaves the buffers modified"
    );
    assert_eq!(
        fake.content("/virtual/a.txt").as_deref(),
        Some("aaa\n"),
        "the daemon must not hold any bytes from the failed `:wqa`"
    );
    assert_eq!(
        fake.content("/virtual/b.txt").as_deref(),
        Some("bbb\n"),
        "the daemon must not hold any bytes from the failed `:wqa`"
    );
}

/// `:w` pushes the **edited** bytes to the daemon across the wire, and `modified`
/// clears only once the daemon has acked the write (ack-gated state).
#[tokio::test]
async fn write_pushes_edited_bytes_over_the_wire_and_clears_modified_on_ack() {
    let fake = DaemonFs::with("/virtual/note.txt", "alpha\n");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["alpha"]).await;

    // Edit the buffer, then save. The edit makes it dirty; `:w` enqueues the off-tick
    // write that crosses the wire.
    feed(&rpc, "ggIhello <Esc>");
    assert!(modified(&rpc).await, "the edit marks the buffer modified");
    feed(&rpc, ":w<CR>");

    // The daemon eventually holds the *edited* bytes — content a stub couldn't invent
    // and the edit-host's local disk can't hold (the `/virtual/...` path), so it can
    // only have crossed the wire from this buffer.
    for _ in 0..100 {
        if fake.content("/virtual/note.txt").as_deref() == Some("hello alpha\n") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        fake.content("/virtual/note.txt").as_deref(),
        Some("hello alpha\n"),
        "the daemon must hold the edited bytes the editor wrote over the wire"
    );
    // And only *now* — after the ack — does the buffer read clean.
    for _ in 0..100 {
        if !modified(&rpc).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !modified(&rpc).await,
        "the modified flag clears only after the daemon acks the write"
    );
}

/// `:wq` defers its quit until the write acks, then exits — and the daemon has the
/// bytes (so the quit really did wait for the save to land, not race ahead of it).
#[tokio::test]
async fn wq_saves_over_the_wire_then_quits() {
    let fake = DaemonFs::with("/virtual/q.txt", "one\n");
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/q.txt").await;
    await_lines(&rpc, &["one"]).await;

    feed(&rpc, "ggIzzz <Esc>");
    feed(&rpc, ":wq<CR>");

    // The editor quits — but only after the off-tick write acks (the quit is replayed
    // in the ack handler). Either signal — an `nxvim_exit` notification or the closed
    // channel — counts.
    let quit = {
        let timeout = Duration::from_secs(2);
        loop {
            match tokio::time::timeout(timeout, incoming.recv()).await {
                Ok(None) => break true,
                Ok(Some(Incoming::Notification { method, .. })) if method == "nxvim_exit" => {
                    break true
                }
                Ok(Some(_)) => continue,
                Err(_) => break false,
            }
        }
    };
    assert!(quit, "`:wq` quits once the off-tick write acks");
    assert_eq!(
        fake.content("/virtual/q.txt").as_deref(),
        Some("zzz one\n"),
        "the `:wq` write landed on the daemon before the quit fired"
    );
}

/// A **failing** daemon write surfaces loudly and **cancels** the `:wq` quit: the
/// editor stays running, the buffer stays modified, and the daemon never gets the
/// bytes. Proves the quit is gated on a *successful* ack, not fired optimistically.
#[tokio::test]
async fn failing_write_cancels_the_quit_and_keeps_the_buffer_modified() {
    let fake = DaemonFs {
        fail_writes: true,
        ..DaemonFs::with("/virtual/f.txt", "data\n")
    };
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/f.txt").await;
    await_lines(&rpc, &["data"]).await;

    feed(&rpc, "ggIx<Esc>");
    feed(&rpc, ":wq<CR>");

    // Watch the notification stream over a bounded window. A failed write must *not*
    // exit the editor (no `nxvim_exit` / channel close), and it must surface the
    // failure *loudly* — a redraw's message line carries it. Classify both signals in
    // one drain so the exit-watch doesn't swallow the failure redraw before we see it.
    let mut exited = false;
    let mut saw_failure = false;
    let deadline = Duration::from_millis(1200);
    loop {
        match tokio::time::timeout(deadline, incoming.recv()).await {
            Ok(None) => {
                exited = true;
                break;
            }
            Ok(Some(Incoming::Notification { method, params })) => {
                if method == "nxvim_exit" {
                    exited = true;
                    break;
                }
                if method == "redraw" && message_of(&params).contains("failed") {
                    saw_failure = true;
                }
            }
            Ok(Some(_)) => {}
            // Quiet window elapsed: the editor is still running (the expected outcome).
            Err(_) => break,
        }
    }
    assert!(!exited, "a failing `:wq` write must not quit the editor");
    assert!(
        saw_failure,
        "the write failure must be surfaced loudly on the message line"
    );

    // The buffer is still dirty (the failed write never cleared it) and the daemon
    // never stored the bytes.
    assert!(
        modified(&rpc).await,
        "a failed write leaves the buffer modified"
    );
    assert_eq!(
        fake.content("/virtual/f.txt").as_deref(),
        Some("data\n"),
        "the daemon must not hold any bytes from the failed write"
    );
}
