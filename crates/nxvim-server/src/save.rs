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

use std::collections::HashSet;
use std::io;

use nxvim_core::{BufferId, FileStat, PendingSave};

use crate::Server;

/// The server-side state for a deferred `:wqa` / `:xa` quit (the multi-buffer save
/// slice): the bang carried from `:wqa!`, and the set of [`PendingSave::seq`]s the
/// batch's `:wall` enqueued and whose acks the quit waits on. The save ack handler
/// removes each seq as it lands; when `waiting` empties it replays `:qa` (the
/// all-buffers-ack-then-quit contract), and a failed write in the batch drops the gate
/// (cancels the quit) — the multi-buffer form of `:wq`'s ack-gated, failure-cancels quit.
pub(crate) struct QuitAllGate {
    /// `:qa!` (true) vs `:qa` (false), replayed once `waiting` empties.
    pub bang: bool,
    /// Outstanding write seqs; the quit fires when this drains to empty.
    pub waiting: HashSet<u64>,
}

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

impl SaveDone {
    /// Build the delivery for a finished off-tick write. The fields stay module-private;
    /// this is the one constructor the native [`HostEffects`](crate::edithost::HostEffects)
    /// write task uses to report a finished `fs_write` back to the save arm.
    pub(crate) fn new(
        save: PendingSave,
        bytes_len: usize,
        result: io::Result<Option<FileStat>>,
    ) -> Self {
        Self {
            save,
            bytes_len,
            result,
        }
    }
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

    /// Record this tick's deferred `:wqa` / `:xa` quit (core's `take_pending_quit_all`)
    /// as the server-side [`QuitAllGate`]. Called right after `drain_pending_saves`, so
    /// the gate's seqs name writes already dispatched this tick; the save ack handler
    /// then drains the gate. A no-op when no batch quit ran. (Core only sets the batch
    /// quit when at least one write was enqueued — an empty `:wqa` quits inline — so the
    /// gate's `waiting` is never empty at birth; it fires only as acks land.)
    pub(crate) fn drain_pending_quit_all(&mut self) {
        if let Some(pqa) = self.editor.take_pending_quit_all() {
            self.quit_all_gate = Some(QuitAllGate {
                bang: pqa.bang,
                waiting: pqa.seqs.into_iter().collect(),
            });
        }
    }

    /// Mark a buffer's snapshot in-flight and hand it to the off-tick write effect
    /// ([`HostEffects::fs_save`](crate::edithost::HostEffects::fs_save)), which pushes
    /// the bytes over the `fs_write` wire and delivers the ack back to the save arm. The
    /// editor thread returns immediately — it never awaits the network here.
    fn dispatch_save(&mut self, save: PendingSave) {
        if !self.fx.has_remote_fs() {
            // Off-tick save mode is only ever enabled alongside a daemon fs, so this
            // is unreachable in practice; fail loud rather than drop the write.
            self.editor
                .echo("nxvim: off-tick save with no daemon filesystem".to_string());
            return;
        }
        self.saves_inflight.insert(save.buffer);
        self.fx.fs_save(save);
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
                // The multi-buffer companion: a `:wqa` / `:xa` waits for *every* write of
                // its batch. Tick this seq off the gate; once the whole set has acked the
                // editor is clean across the batch, so replay `:qa` (the
                // all-buffers-ack-then-quit contract — like the single-buffer `:wq` above
                // but gated on the set, not one save).
                self.advance_quit_all_gate(save.seq);
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
                // A failed write in a `:wqa` / `:xa` batch cancels the whole quit (the
                // multi-buffer form of a failed `:wq` keeping the editor up): drop the
                // gate so the editor stays running with the unsaved buffer intact.
                self.cancel_quit_all_gate(save.seq);
            }
        }
    }

    /// Tick a successfully-acked write's `seq` off the `:wqa` quit gate (if it belongs to
    /// one). When the gate's set drains to empty, every write of the batch has landed and
    /// the editor is clean across it, so replay `:qa` / `:qa!`. A no-op when no batch quit
    /// is pending or this seq isn't part of it (a plain `:w` / `:wq`).
    fn advance_quit_all_gate(&mut self, seq: u64) {
        let Some(gate) = self.quit_all_gate.as_mut() else {
            return;
        };
        if !gate.waiting.remove(&seq) || !gate.waiting.is_empty() {
            return;
        }
        let bang = gate.bang;
        self.quit_all_gate = None;
        self.run_command(if bang { "qa!" } else { "qa" });
    }

    /// Cancel a pending `:wqa` quit because one of its batch's writes failed — drop the
    /// gate so the editor stays up with the unsaved buffer intact (the failure itself is
    /// already surfaced loudly by the caller). A no-op when this seq isn't part of a
    /// pending batch quit.
    fn cancel_quit_all_gate(&mut self, seq: u64) {
        if self
            .quit_all_gate
            .as_ref()
            .is_some_and(|g| g.waiting.contains(&seq))
        {
            self.quit_all_gate = None;
            self.editor
                .echo("nxvim: :wqa aborted (a write failed); editor stays open".to_string());
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
