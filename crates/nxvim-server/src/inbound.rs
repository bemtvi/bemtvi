//! The inbound seam — the run loop's `select!` arms translated into editor-tick
//! handlers (Phase 4, the 4d slice; see
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! Counterpart to [`edithost`](crate::edithost) (the *outbound* [`HostEffects`] seam).
//! The keystroke → core → redraw tick fires its effects **outbound** through
//! `HostEffects`; the matching **inbound** events — a child exited, a timer fired, an LSP
//! reply landed, a file was fetched / saved over the daemon wire, a remote file changed —
//! arrive on the run loop's transports and are fed back into the tick here.
//!
//! Each method owns one `select!` arm's whole job: coalesce the channel's burst (the
//! first event plus any already queued behind it, into a single settle + repaint), run
//! the per-event handler (`on_lsp_event` / `on_loop_event` / `apply_open` /
//! `on_install_done` / `apply_save_done` / `on_remote_file_changed`), and settle. The run
//! loop stays a **thin translator**: an arm receives one event and hands the batch here,
//! never touching editor / Lua state itself. That is the property the `EditHost` hoist
//! (the 4e slice) needs — the loop must not reach into the tick's internals (it pokes
//! neither `editor.should_quit` nor `lsp_dirty` nor `fx` directly anymore).
//!
//! [`HostEffects`]: crate::edithost::HostEffects

use nxvim_core::BufferId;
use nxvim_lsp::LspEvent;
use nxvim_rpc::Incoming;
use std::io;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::daemon::{FsRead, WatchEvent};
use crate::evloop::LoopEvent;
use crate::save::SaveDone;
use crate::{EditHost, InstallOutcome};

impl EditHost {
    /// Apply one client message (an `nvim_*` request or notification), then report whether
    /// it asked the editor to quit. The run loop breaks on `true`.
    pub(crate) async fn on_client_message(&mut self, message: Incoming) -> bool {
        self.handle(message).await;
        if self.quitting() {
            return true;
        }
        // Re-arm the debounced shada checkpoint after each message, so a crash loses at
        // most that window rather than the whole session (Phase 5). Skipped when quitting
        // — the clean-exit flush handles that case.
        self.arm_shada_checkpoint();
        false
    }

    /// Coalesce a burst of language-server events (initialize handshakes, published
    /// diagnostics, replies, exits, log messages) into one settle + repaint. A reply
    /// handled by a Lua callback may defer work via `vim.cmd` / `vim.schedule`, so the
    /// settle is gated on `lsp_dirty` to drive that to convergence.
    pub(crate) fn on_lsp_events(&mut self, first: LspEvent, rx: &mut UnboundedReceiver<LspEvent>) {
        self.on_lsp_event(first);
        while let Ok(event) = rx.try_recv() {
            self.on_lsp_event(event);
        }
        let dirty = std::mem::take(&mut self.lsp_dirty);
        self.settle_events(dirty);
    }

    /// Coalesce a burst of event-loop events (timers firing, children exiting, native
    /// fs-watch hits) into one settle + repaint. The matching Lua callbacks run here, on
    /// the one server thread.
    pub(crate) fn on_loop_events(
        &mut self,
        first: LoopEvent,
        rx: &mut UnboundedReceiver<LoopEvent>,
    ) {
        // The shada debounce-timer is multiplexed onto this arm too. Sort each event:
        // its flush is handled here (it needs the store, not a Lua callback), every
        // real event runs its callback via `on_loop_event`.
        let mut had_real = false;
        let mut shada_due = false;
        for event in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            if crate::is_shada_flush_timer(&event) {
                shada_due = true;
            } else {
                self.on_loop_event(event);
                had_real = true;
            }
        }
        if shada_due {
            self.shada_checkpoint();
        }
        // Only settle/repaint when a real event ran; a shada-only wake changed no
        // editor state, so a redraw would be spurious.
        if had_real {
            self.settle_events(true);
        }
    }

    /// Coalesce a burst of off-tick opens (the startup file kept from freezing startup, or
    /// a later `:edit` over the daemon wire): load each into its named replica buffer (or
    /// echo a read error), then settle + repaint.
    pub(crate) fn on_opens(
        &mut self,
        first: (BufferId, String, io::Result<FsRead>),
        rx: &mut UnboundedReceiver<(BufferId, String, io::Result<FsRead>)>,
    ) {
        let (buffer, path, result) = first;
        self.apply_open(buffer, path, result);
        while let Ok((buffer, path, result)) = rx.try_recv() {
            self.apply_open(buffer, path, result);
        }
        self.settle_events(true);
    }

    /// Coalesce a burst of finished `:TSInstall` jobs (a grammar fetched + compiled, or a
    /// failure): reload the grammar so open buffers re-highlight / indent, echo the
    /// outcome, then settle + repaint.
    pub(crate) fn on_installs(
        &mut self,
        first: InstallOutcome,
        rx: &mut UnboundedReceiver<InstallOutcome>,
    ) {
        self.on_install_done(first);
        while let Ok(outcome) = rx.try_recv() {
            self.on_install_done(outcome);
        }
        self.settle_events(true);
    }

    /// Coalesce a burst of finished off-tick writes (the daemon save path): finalize each
    /// buffer's saved-state, echo `written`, and replay any deferred `:wq` / `:x` quit,
    /// then settle. Reports whether a replayed quit asked the editor to exit (the one
    /// non-input arm that can) — the run loop breaks on `true`.
    pub(crate) fn on_save_dones(
        &mut self,
        first: SaveDone,
        rx: &mut UnboundedReceiver<SaveDone>,
    ) -> bool {
        self.apply_save_done(first);
        while let Ok(done) = rx.try_recv() {
            self.apply_save_done(done);
        }
        self.settle_events(true);
        self.quitting()
    }

    /// Coalesce a burst of remote file-change pushes (the daemon `HostWatch` leg):
    /// reconcile each off the editor tick — the remote analogue of the local per-buffer
    /// watch's `FsEvent` handling — then settle + repaint.
    pub(crate) fn on_watch_events(
        &mut self,
        first: WatchEvent,
        rx: &mut UnboundedReceiver<WatchEvent>,
    ) {
        self.on_remote_file_changed(first);
        while let Ok(ev) = rx.try_recv() {
            self.on_remote_file_changed(ev);
        }
        self.settle_events(true);
    }

    /// If the editor asked to quit, notify the client (`nxvim_exit`) and report `true` so
    /// the run loop breaks. The single funnel both quit-capable arms (client input, the
    /// off-tick save ack) check, so the exit notification lives in exactly one place.
    fn quitting(&mut self) -> bool {
        if self.editor.should_quit {
            self.fx.notify("nxvim_exit", vec![]);
            true
        } else {
            false
        }
    }
}
