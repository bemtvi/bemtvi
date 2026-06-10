//! The off-tick save path — the server half of the daemon save wire
//! (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3e).
//!
//! In a daemon session core does **not** write through the synchronous
//! [`HostFs`](nxvim_core::HostFs) — that would park the single editor thread (the
//! one that processes input *and* paints) on the network. Instead `:w` snapshots the
//! buffer at command time into a [`PendingSave`] (see `nxvim-core`), and this module
//! pushes those bytes over the [`fs_write`](crate::daemon) wire *off the editor tick*
//! and finalizes the buffer's saved-state only on the daemon's ack. So a slow remote
//! write never freezes typing — the keystroke/redraw path keeps serving the (now
//! `[+]`-marked) buffer the whole time.
//!
//! The contract this implements (from the plan's *save slice*):
//! - **Snapshot at command time.** The rope bytes are captured in `:w` (core's
//!   `enqueue_save`); edits made while the write is in flight never tear into them.
//! - **Ack-gated state.** `modified` clears, the new [`FileStat`] is stamped, and the
//!   `written` echo fires only when the daemon acks — never optimistically at send.
//! - **Quit waits for the ack.** `:wq` / `:x` defer their quit (`PendingSave::then_quit`)
//!   until the write acks; a failed write cancels the quit and surfaces loudly.
//! - **Per-buffer serialization.** Overlapping `:w`s on one buffer queue in order
//!   (snapshot order = wire order); a failed earlier write fails the queue loudly
//!   rather than letting a later snapshot paper over it.

use std::io;

use nxvim_core::{BufferId, FileStat, PendingSave};

use crate::Server;

/// A finished off-tick write, delivered from the spawned write task back to the
/// server's `select!` loop. Carries the originating [`PendingSave`] (its `bytes` are
/// emptied — they moved into the write call) plus `bytes_len` for the `written` echo,
/// and the daemon's result: the new [`FileStat`] on success, or a loud error.
pub(crate) struct SaveDone {
    save: PendingSave,
    /// The snapshot's byte length, kept for the echo since `save.bytes` was moved into
    /// the write call.
    bytes_len: usize,
    result: io::Result<Option<FileStat>>,
}

impl Server {
    /// Route this tick's deferred writes (core's `take_pending_saves`) onto the wire,
    /// serialized per buffer: a buffer with no write in flight dispatches immediately;
    /// a buffer that already has one queues this behind it (snapshot order = wire
    /// order). Called at the tail of [`run_pending`](Server::run_pending), so every
    /// `:w` — typed, `vim.cmd('w')`, or from a user command — is caught after the
    /// editor converges.
    pub(crate) fn drain_pending_saves(&mut self) {
        for save in self.editor.take_pending_saves() {
            if self.saves_inflight.contains(&save.buffer) {
                self.saves_queued
                    .entry(save.buffer)
                    .or_default()
                    .push_back(save);
            } else {
                self.dispatch_save(save);
            }
        }
    }

    /// Push one snapshot over the `fs_write` wire off the editor tick: mark its buffer
    /// in-flight, move the bytes into the async write, and deliver the result back to
    /// the loop via `save_done_tx`. The editor thread returns immediately — it never
    /// awaits the network here.
    fn dispatch_save(&mut self, mut save: PendingSave) {
        let Some(fs) = self.host_fs_async.clone() else {
            // Off-tick save mode is only ever enabled alongside a daemon fs, so this
            // is unreachable in practice; fail loud rather than drop the write.
            self.editor
                .echo("nxvim: off-tick save with no daemon filesystem".to_string());
            return;
        };
        self.saves_inflight.insert(save.buffer);
        let bytes = std::mem::take(&mut save.bytes);
        let bytes_len = bytes.len();
        let path = save.path.display().to_string();
        let tx = self.save_done_tx.clone();
        tokio::spawn(async move {
            let result = fs.write(path, bytes).await;
            let _ = tx.send(SaveDone {
                save,
                bytes_len,
                result,
            });
        });
    }

    /// Apply a finished off-tick write: on success, finalize the buffer's saved-state
    /// (the ack-gated `modified`/`FileStat`/`save_tick`), echo `written`, replay any
    /// deferred `:wq` / `:x` quit, and dispatch the next queued write for that buffer.
    /// On failure, surface it loudly and **fail the buffer's whole queue** — a later
    /// snapshot must never silently stand in for a write that didn't land. A replayed
    /// quit may set `should_quit`; the `select!` arm checks it after this returns.
    pub(crate) fn apply_save_done(&mut self, done: SaveDone) {
        let SaveDone {
            save,
            bytes_len,
            result,
        } = done;
        self.saves_inflight.remove(&save.buffer);
        match result {
            Ok(stat) => {
                self.editor
                    .finalize_save(save.buffer, save.path.clone(), stat);
                self.editor.echo(format!(
                    "\"{}\" {}L, {}B written",
                    save.path.display(),
                    save.lines,
                    bytes_len,
                ));
                // The deferred half of `:wq` / `:x`: now that the bytes are on the
                // remote and the buffer is clean, replay the quit (`:q` / `:q!`). A
                // clean buffer means no E37 — exactly as a synchronous `:wq` would.
                if let Some(bang) = save.then_quit {
                    self.run_command(if bang { "q!" } else { "q" });
                }
                // This buffer's slot freed: send its next queued write, if any.
                if let Some(next) = self.next_queued_save(save.buffer) {
                    self.dispatch_save(next);
                }
            }
            Err(e) => {
                self.editor.echo(format!(
                    "nxvim: write of \"{}\" failed: {e}",
                    save.path.display()
                ));
                // Fail the rest of this buffer's queue rather than letting a later
                // snapshot paper over the gap; cancel their deferred quits too (an
                // unflushed write must never be abandoned by an exiting editor).
                if let Some(queued) = self.saves_queued.remove(&save.buffer) {
                    for q in queued {
                        self.editor.echo(format!(
                            "nxvim: write of \"{}\" abandoned (an earlier write failed)",
                            q.path.display()
                        ));
                    }
                }
            }
        }
    }

    /// Pop the next queued write for `buffer`, removing the queue once it empties.
    fn next_queued_save(&mut self, buffer: BufferId) -> Option<PendingSave> {
        let queue = self.saves_queued.get_mut(&buffer)?;
        let next = queue.pop_front();
        if queue.is_empty() {
            self.saves_queued.remove(&buffer);
        }
        next
    }
}
