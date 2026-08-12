//! The daemon wire protocol, filesystem half — the **save path** (edit-host split,
//! Phase 3e of `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Companion to `daemon_fs.rs` (which proved the off-tick *read*). Here a real editor
//! whose async fs is a [`RemoteHostFs`](bemtvi_server::RemoteHostFs) talking to a
//! [`serve_fs_daemon`](bemtvi_server::serve_fs_daemon) over an in-process duplex writes
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

use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{
    await_lines, buf_lines, config_init, exec_lua, feed, message_of, spawn_with_daemon_fs,
    spawn_with_daemon_fs_init, temp_dir, DaemonFs,
};
use tokio::sync::mpsc::UnboundedReceiver;

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

/// Poll the notification stream until a `redraw` carries a message containing `want`,
/// then assert it arrived. The ack-side counterpart of [`await_daemon_content`]: an
/// off-tick write's `written` echo is emitted when the daemon acks, several frames
/// after the `:w`.
async fn await_message_containing(incoming: &mut UnboundedReceiver<Incoming>, want: &str) {
    let deadline = Duration::from_millis(2000);
    let mut seen: Vec<String> = Vec::new();
    while let Ok(Some(Incoming::Notification { method, params })) =
        tokio::time::timeout(deadline, incoming.recv()).await
    {
        if method != "redraw" {
            continue;
        }
        let msg = message_of(&params);
        if msg.contains(want) {
            return;
        }
        if !msg.is_empty() {
            seen.push(msg);
        }
    }
    panic!("no message containing {want:?} arrived; saw {seen:?}");
}

/// Open `/virtual/a.txt`, edit it dirty, then `:edit /virtual/b.txt` and edit *it* dirty
/// — leaving two modified file-backed buffers, both loaded over the wire. Returns the
/// daemon fake and the live client. The edits prepend a marker so the written bytes are
/// content a stub couldn't invent and the local disk can't hold (the `/virtual/...`
/// faithfulness argument the rest of this suite makes).
async fn two_edited_buffers(fail_writes: bool) -> (DaemonFs, Rpc, UnboundedReceiver<Incoming>) {
    let fake = DaemonFs::with_files(&[("/virtual/a.txt", "aaa\n"), ("/virtual/b.txt", "bbb\n")]);
    fake.fail_writes(fail_writes);
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
    // when its seq-set drains). Either an `bemtvi_exit` notification or the closed channel
    // counts.
    let quit = {
        let timeout = Duration::from_secs(2);
        loop {
            match tokio::time::timeout(timeout, incoming.recv()).await {
                Ok(None) => break true,
                Ok(Some(Incoming::Notification { method, .. })) if method == "bemtvi_exit" => {
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
                if method == "bemtvi_exit" {
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

/// An edit made **while the write is in flight** survives the ack: the buffer stays
/// `modified`, because what reached the daemon is the *snapshot*, not what the buffer now
/// holds.
///
/// The snapshot is taken at command time (`PendingSave::bytes`, deliberately, "so edits
/// made while the write is in flight can never tear into what gets persisted") and the ack
/// lands one wire round-trip later — a window the user can type into, and over a real
/// daemon link a long one. Clearing `modified` there reports a buffer as saved whose newest
/// text exists nowhere but memory: `:q` then closes it with no `E37` and the edit is gone.
/// The write itself is fine — the file holds the snapshot — so only the *flag* is wrong.
///
/// Deterministic rather than timing-dependent: [`DaemonFs::hold_writes`] parks the write on
/// the daemon so the edit provably lands inside the window (hence the multi-thread runtime
/// — the parked daemon task must not stall the editor).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_while_the_write_is_in_flight_keeps_the_buffer_modified() {
    let fake = DaemonFs::with("/virtual/note.txt", "one\n");
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/note.txt").await;
    await_lines(&rpc, &["one"]).await;

    // Edit, then save: the write snapshots "two\n" and parks on the daemon.
    feed(&rpc, "ggcctwo<Esc>");
    assert_eq!(await_lines(&rpc, &["two"]).await, vec!["two"]);
    let hold = fake.hold_writes();
    feed(&rpc, ":w<CR>");
    hold.await_parked().await;

    // The user types on, inside the in-flight window.
    feed(&rpc, "ggccthree<Esc>");
    assert_eq!(await_lines(&rpc, &["three"]).await, vec!["three"]);

    // Let the write finish and its ack land.
    hold.release();
    await_message_containing(&mut incoming, "written").await;

    // The daemon holds the snapshot — the write did exactly what it promised...
    await_daemon_content(&fake, "/virtual/note.txt", "two\n").await;
    // ...but "three" never went anywhere, so the buffer is still dirty.
    assert_eq!(buf_lines(&rpc, 0).await, vec!["three"]);
    assert!(
        modified(&rpc).await,
        "an edit made while the write was in flight is unsaved — the ack for the earlier \
         snapshot must not report the buffer clean"
    );
}

/// Over the wire, `:w` fires `BufWritePre` **once**, *before* the bytes are pushed — so
/// a handler's buffer mutation is what the daemon receives (tier-1 remote parity with
/// the local path), and the ack does **not** re-fire `BufWritePre`.
#[tokio::test]
async fn offtick_bufwritepre_fires_once_before_the_wire_write() {
    let dir = temp_dir("daemon_bufwritepre");
    let fake = DaemonFs::with("/virtual/n.txt", "hello\n");
    let mut init = config_init(
        &dir,
        "_G.pre = 0\n\
         vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function() _G.pre = _G.pre + 1; vim.cmd([[%s/hello/HELLO/]]) end })\n",
    );
    init.file = Some("/virtual/n.txt".to_string());
    let (rpc, _incoming) = spawn_with_daemon_fs_init(fake.clone(), init).await;
    await_lines(&rpc, &["hello"]).await;

    feed(&rpc, ":w<CR>");

    // The daemon receives the *mutated* bytes — proof `BufWritePre` ran before the wire
    // write, not after (a stub can't invent "HELLO", and `/virtual/...` isn't local disk).
    await_daemon_content(&fake, "/virtual/n.txt", "HELLO\n").await;
    // …and it fired exactly once (the ack path did not fire a second `BufWritePre`).
    assert_eq!(
        exec_lua(&rpc, "return _G.pre").await.as_i64(),
        Some(1),
        "BufWritePre fires once per off-tick `:w`, before the bytes cross the wire"
    );
}

/// Over the wire, an **async** `BufWritePre` handler is awaited before the bytes are
/// pushed: the write waits for the handler's promise to settle, so its mutation is what
/// the daemon receives — the async format-on-save contract, working remotely too.
#[tokio::test]
async fn offtick_async_bufwritepre_settles_before_the_wire_write() {
    let dir = temp_dir("daemon_async_bufwritepre");
    let fake = DaemonFs::with("/virtual/n.txt", "hello\n");
    let mut init = config_init(
        &dir,
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function()\n\
         \x20   return btv.promise.delay(30):next(function() vim.cmd([[%s/hello/HELLO/]]) end)\n\
         \x20 end })\n",
    );
    init.file = Some("/virtual/n.txt".to_string());
    let (rpc, _incoming) = spawn_with_daemon_fs_init(fake.clone(), init).await;
    await_lines(&rpc, &["hello"]).await;

    feed(&rpc, ":w<CR>");

    // The daemon holds the mutated bytes — the wire write waited for the async handler.
    await_daemon_content(&fake, "/virtual/n.txt", "HELLO\n").await;
}

/// Over the wire, `:wall` fires each buffer's `BufWritePre` before its bytes — with that
/// buffer made current — so a mutating handler targets the *right* buffer even though
/// only one is current. Both daemon files receive their own mutated bytes.
#[tokio::test]
async fn offtick_wall_fires_bufwritepre_per_buffer() {
    let dir = temp_dir("daemon_wall_pre");
    let fake = DaemonFs::with_files(&[("/virtual/a.txt", "aaa\n"), ("/virtual/b.txt", "bbb\n")]);
    let mut init = config_init(
        &dir,
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function() vim.cmd([[%s/$/X/]]) end })\n",
    );
    init.file = Some("/virtual/a.txt".to_string());
    let (rpc, _incoming) = spawn_with_daemon_fs_init(fake.clone(), init).await;
    await_lines(&rpc, &["aaa"]).await;
    // Dirty a, open + dirty b (leaving b current), then write all.
    feed(&rpc, "A1<Esc>");
    feed(&rpc, ":edit /virtual/b.txt<CR>");
    await_lines(&rpc, &["bbb"]).await;
    feed(&rpc, "A2<Esc>");
    feed(&rpc, ":wall<CR>");
    // Each file crosses the wire with its own buffer's `BufWritePre` mutation — so the
    // non-current buffer (a) was made current for its fire, not left targeting b.
    await_daemon_content(&fake, "/virtual/a.txt", "aaa1X\n").await;
    await_daemon_content(&fake, "/virtual/b.txt", "bbb2X\n").await;
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
    // in the ack handler). Either signal — an `bemtvi_exit` notification or the closed
    // channel — counts.
    let quit = {
        let timeout = Duration::from_secs(2);
        loop {
            match tokio::time::timeout(timeout, incoming.recv()).await {
                Ok(None) => break true,
                Ok(Some(Incoming::Notification { method, .. })) if method == "bemtvi_exit" => {
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
    let fake = DaemonFs::with("/virtual/f.txt", "data\n");
    fake.fail_writes(true);
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/f.txt").await;
    await_lines(&rpc, &["data"]).await;

    feed(&rpc, "ggIx<Esc>");
    feed(&rpc, ":wq<CR>");

    // Watch the notification stream over a bounded window. A failed write must *not*
    // exit the editor (no `bemtvi_exit` / channel close), and it must surface the
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
                if method == "bemtvi_exit" {
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

/// `'endofline'` / `'fixendofline'` are tier-1 over the wire: the *document* is
/// assembled core-side (`Buffer::to_save_bytes`, snapshotted when the save is
/// enqueued), so a file read without a trailing newline round-trips through the daemon
/// exactly as it does locally — no per-transport newline handling anywhere.
#[tokio::test]
async fn a_no_eol_file_round_trips_over_the_wire() {
    let fake = DaemonFs::with("/virtual/noeol.txt", "a\nb");
    let (rpc, mut incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/noeol.txt").await;
    await_lines(&rpc, &["a", "b"]).await;

    // The off-tick read detected the missing terminator, same as a local open.
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].endofline").await.as_bool(),
        Some(false),
        "a remote read detects the unterminated last line"
    );

    feed(&rpc, ":set nofixeol<CR>");
    feed(&rpc, "ggIhello <Esc>");
    feed(&rpc, ":w<CR>");
    await_daemon_content(&fake, "/virtual/noeol.txt", "hello a\nb").await;
    // The ack's echo tags it, exactly as the local write's does — the flag rides on the
    // snapshot (`PendingSave::noeol`), so the message describes the bytes that crossed
    // the wire rather than the buffer whenever the ack happened to land.
    await_message_containing(&mut incoming, "[noeol] 2L, 9B written").await;

    // …and with `'fixendofline'` back on, the daemon receives the terminator.
    feed(&rpc, ":set fixeol<CR>");
    feed(&rpc, "GAc<Esc>");
    feed(&rpc, ":w<CR>");
    await_daemon_content(&fake, "/virtual/noeol.txt", "hello a\nbc\n").await;
    await_message_containing(&mut incoming, "\" 2L, 11B written").await;
}

/// The empty-document case over the wire: a 0-byte remote file must not grow to one
/// byte on save (the local `ML_EMPTY` behavior, unchanged by the transport).
#[tokio::test]
async fn an_empty_remote_file_stays_empty_across_a_write() {
    let fake = DaemonFs::with("/virtual/empty.txt", "");
    let (rpc, _incoming) = spawn_with_daemon_fs(fake.clone(), "/virtual/empty.txt").await;
    await_lines(&rpc, &[""]).await;

    feed(&rpc, ":w<CR>");
    await_daemon_content(&fake, "/virtual/empty.txt", "").await;
}
