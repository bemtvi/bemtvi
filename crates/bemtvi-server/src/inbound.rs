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

use bemtvi_core::BufferId;
use bemtvi_lsp::LspEvent;
use bemtvi_rpc::Incoming;
use std::io;
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};

use crate::daemon::{DaemonStatus, FsRead, WatchEvent};
use crate::evloop::LoopEvent;
use crate::save::SaveDone;
use crate::terminal::native::TermEvent;
use crate::{EditHost, InstallOutcome};

/// How many bytes of terminal output to process per [`EditHost::on_term_events`]
/// call before settling + repainting. Bounds the work between repaints so a flood
/// shows visible progress (the live screen scrolls as it loads) and the editor stays
/// responsive to keystrokes, while staying large enough that ordinary output still
/// coalesces into a single repaint.
const TERM_BATCH_BYTES: usize = 256 * 1024;

impl EditHost {
    /// Apply one client message (an `nvim_*` request or notification). Whether it asked the
    /// editor to quit is decided by the run loop's single post-`select!` [`quitting`](Self::quitting)
    /// funnel, not here — a quit may complete *asynchronously* (an `ExitPre`/`VimLeavePre`
    /// handler awaiting a promise) on a later tick driven by a different arm, so no one arm
    /// owns the break.
    pub(crate) async fn on_client_message(&mut self, message: Incoming) {
        self.handle(message).await;
        // Re-arm the debounced shada checkpoint after each message, so a crash loses at
        // most that window rather than the whole session (Phase 5). Skipped once a quit is
        // committed (`should_quit`, or mid-flight through the gated exit sequence) — the
        // clean-exit flush handles that case.
        if !self.editor.should_quit && self.exit_stage.is_none() {
            self.arm_shada_checkpoint();
        }
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
        let mut resume_due = false;
        let mut diag_due = false;
        for event in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            if crate::is_shada_flush_timer(&event) {
                shada_due = true;
            } else if crate::is_diag_debounce_timer(&event) {
                // Typing went quiet: apply the diagnostic update it held. Handled here
                // like the two above — it is the editor's own timer, not a Lua callback.
                diag_due = true;
            } else if crate::is_parse_resume_timer(&event) {
                resume_due = true;
            } else if crate::is_workspace_fs_timeout_timer(&event) {
                // The workspace file-operation watchdog: a `rename`/`delete`/`mkdir`
                // whose fs leg stopped answering. Handled here (not through
                // `on_loop_event`) for the same reason the two above are — it is the
                // editor's own timer, not a Lua callback.
                self.on_workspace_fs_timeout();
                had_real = true;
            } else {
                self.on_loop_event(event);
                had_real = true;
            }
        }
        if shada_due {
            self.shada_checkpoint();
        }
        if diag_due {
            self.on_diag_debounce();
        }
        // Repaint when a real event ran, or when a parse-resume / diagnostic-debounce
        // wake is due: neither changed editor state by itself, but the redraw each
        // triggers is the whole point — resuming the in-flight treesitter parse and
        // painting its new spans, and painting the diagnostics the debounce just
        // applied. A shada-only wake, by contrast, touches nothing visible, so it
        // never forces a frame.
        if had_real || resume_due || diag_due {
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
    /// then settle. A replayed quit may ask the editor to exit — the run loop's single
    /// post-`select!` [`quitting`](Self::quitting) funnel picks that up (the exit may also be
    /// mid-flight through the gated `ExitPre`/`VimLeavePre` sequence).
    pub(crate) fn on_save_dones(&mut self, first: SaveDone, rx: &mut UnboundedReceiver<SaveDone>) {
        self.apply_save_done(first);
        while let Ok(done) = rx.try_recv() {
            self.apply_save_done(done);
        }
        self.settle_events(true);
    }

    /// Coalesce a burst of terminal events (a `:terminal` child's output / exit) into
    /// one settle + repaint: feed each output chunk to the buffer's vt100 emulator
    /// (refreshing its mirrored screen) and record an exit. Output arrives in a stream,
    /// so coalescing a burst keeps a chatty child from repainting per chunk.
    ///
    /// The drain is **bounded** by a byte budget so a flood still repaints as it
    /// loads: under a torrent (`rg` printing 500k matches) the channel refills faster
    /// than we drain it, and draining it dry in one go would block the editor with no
    /// repaint until the flood ended — the screen would look frozen. We instead
    /// process up to [`TERM_BATCH_BYTES`] per call, then settle + repaint and return;
    /// the run loop's `select!` immediately re-fires this arm for the next batch (and
    /// can interleave the user's keystrokes between batches), so the live screen
    /// visibly scrolls while loading.
    pub(crate) fn on_term_events(&mut self, first: TermEvent, rx: &mut Receiver<TermEvent>) {
        let mut budget: usize = 0;
        let mut ev = first;
        // Buffers fed this batch, projected once after the drain (feeding only
        // processes bytes — projecting per chunk would be one full re-read per chunk).
        let mut dirty: Vec<BufferId> = Vec::new();
        loop {
            if let TermEvent::Data { buf, bytes } = &ev {
                budget += bytes.len();
                if !dirty.contains(buf) {
                    dirty.push(*buf);
                }
            }
            self.on_term_event(ev);
            if budget >= TERM_BATCH_BYTES {
                break; // repaint progress; the arm re-fires for the rest
            }
            match rx.try_recv() {
                Ok(next) => ev = next,
                Err(_) => break,
            }
        }
        for buf in dirty {
            self.terminal_project(buf);
        }
        self.settle_events(true);
    }

    /// Apply one terminal event: feed output bytes to `buf`'s emulator (the caller
    /// projects after the batch), or record the child's exit (append the notice,
    /// leave terminal mode) and drop the emulator.
    fn on_term_event(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Data { buf, bytes } => self.terminal_feed(buf, &bytes),
            TermEvent::Exit { buf, code } => {
                // Mirror the child's final screen into the buffer *before* the emulator
                // is dropped. A fast-exiting child's last `Data` and its `Exit` arrive in
                // the same `on_term_events` batch, and the batch's post-drain projection
                // can't read a removed emulator — so without this, that output is fed but
                // never projected (the buffer shows only the exit notice). `terminal_closed`
                // appends the notice after this mirrored content.
                self.terminal_project(buf);
                self.editor.terminal_closed(buf, code);
                // Freeze the grid's colors before dropping the emulator, so the dead
                // buffer's final output keeps its highlighting as a plain buffer.
                self.terminal_freeze(buf);
                self.terminal_remove(buf);
            }
        }
    }

    /// Coalesce a burst of finished off-tick `:cd`s (the daemon `fs_chdir` reply): install
    /// each into `DirState` (or echo its `E344`), then settle + repaint. A burst is rare
    /// (a `:cd` and a focus-driven re-point can land together), but coalescing keeps it to
    /// one frame, like the sibling arms.
    pub(crate) fn on_chdir_dones(
        &mut self,
        first: crate::cwd::ChdirDone,
        rx: &mut UnboundedReceiver<crate::cwd::ChdirDone>,
    ) {
        self.apply_chdir(first);
        while let Ok(done) = rx.try_recv() {
            self.apply_chdir(done);
        }
        self.settle_events(true);
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

    /// Reflect a reconnecting daemon link's [`DaemonStatus`] change into the editor + Lua, off
    /// the editor tick. Maps the native status enum onto the shared phase string, shows the
    /// loud "run `:reconnect`" hint once the auto-retry budget is spent (`Disconnected`), and
    /// delegates the mirror-to-Lua + reconnect resync to the shared
    /// [`apply_daemon_phase`](Self::apply_daemon_phase) (which the wasm edit-host's
    /// `eh_daemon_status` FFI also drives). A genuine reconnect is a `Reconnecting`/`Disconnected`
    /// → `Connected` transition — the *first* `Connected` is the initial connect, no resync.
    pub(crate) fn on_daemon_status(&mut self, status: DaemonStatus) {
        let phase = match status {
            DaemonStatus::Connected => "connected",
            DaemonStatus::Reconnecting { .. } => "reconnecting",
            DaemonStatus::Disconnected => "disconnected",
        };
        let reconnected = matches!(status, DaemonStatus::Connected)
            && matches!(
                self.prev_daemon_status,
                Some(DaemonStatus::Reconnecting { .. }) | Some(DaemonStatus::Disconnected)
            );
        self.prev_daemon_status = Some(status);
        // The `:reconnect` hint is native-only (the wasm edit-host has no such ex-command — its
        // supervisor auto-retries and the user re-`:connect`s), so it lives here, not in the
        // shared `apply_daemon_phase`.
        if matches!(status, DaemonStatus::Disconnected) {
            self.editor
                .echo("daemon disconnected — run :reconnect to restore the link");
        }
        self.apply_daemon_phase(phase, reconnected);
    }

    /// If the editor has asked to quit ([`Editor::should_quit`], set by the gated exit
    /// sequence once its `ExitPre`/`VimLeavePre` handlers have all settled), notify the
    /// client (`bemtvi_exit`) and report `true` so the run loop breaks. The run loop checks
    /// this **once per `select!` iteration**, after whichever arm ran — because a quit can
    /// complete *asynchronously* (a handler awaiting a promise), the settling arm is not
    /// necessarily the input/save arm that started it, so no single arm can own the break.
    pub(crate) fn quitting(&mut self) -> bool {
        if self.editor.should_quit {
            self.fx.notify("bemtvi_exit", vec![]);
            true
        } else {
            false
        }
    }
}
