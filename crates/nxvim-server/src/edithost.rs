//! The `HostEffects` seam — the boundary the **synchronous editor tick** emits its
//! async/external side effects through (Phase 4, Open Decision #6 option (a):
//! *extract a reusable sync `EditHost`*; see
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! The keystroke → core → redraw path is sync and local in every world (native and
//! wasm); the *only* things that ever reach async or off-thread machinery are a small,
//! bounded set of **outbound effects** the tick fires and forgets: pushing a redraw /
//! notification / response to the client wire, and handing the event-loop actor a
//! timer / process / watch command. This trait names that set. The sync tick calls it
//! through a trait object so the same editing logic can run behind two transports:
//!
//! - **native** ([`NativeEffects`]): the client wire is msgpack-RPC ([`Rpc`]) and the
//!   command sink is the tokio [`EventLoop`] actor — today's behavior, verbatim.
//! - **wasm** (Phase 5, later): the wire is JS interop posting redraws back to the UI
//!   thread, and the command sink is the Worker-side timer wheel / the daemon link.
//!
//! Only **outbound** effects live here. The matching *inbound* events (a child exited,
//! a timer fired, an LSP reply, a file fetched) are owned by the run loop's `select!`
//! and fed into editor-tick methods — that inbound seam is a later slice. This is the
//! first brick of the `EditHost` extraction: the off-tick async machinery moves behind
//! the trait while the (large) editor/Lua surface stays put, so the seam grows from the
//! small side, not by relocating hundreds of `self.editor` call sites at once.

use crate::daemon::{FsRead, HostFsAsync};
use crate::evloop::{EventLoop, LoopCommand};
use crate::save::SaveDone;
use nxvim_core::{BufferId, PendingSave};
use nxvim_lsp::{LspManager, LspNotify, LspRequest, ReqToken, ServerKey, ServerSpawn};
use nxvim_rpc::Rpc;
use rmpv::Value;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// The async-effect boundary the synchronous editor tick emits through. See the
/// module docs for why this is the seam that lets one sync core serve both the native
/// server and the wasm Worker.
pub trait HostEffects {
    /// Push a notification to the attached client (the `redraw` frame, `nxvim_exit`,
    /// scripted panel selects, …).
    fn notify(&mut self, method: &str, params: Vec<Value>);
    /// Answer a client RPC request by msgid (the reply to an `nvim_*` call).
    fn respond(&mut self, id: u64, result: Result<Value, Value>);
    /// Hand a command to the event-loop actor (start/stop a timer, spawn/kill a
    /// child, arm/disarm a native file watch). Fire-and-forget; completions return
    /// as inbound `LoopEvent`s on the run loop's `select!`, not here.
    fn loop_command(&mut self, cmd: LoopCommand);

    /// Off-tick fs — fetch a buffer's bytes over the daemon read leg (a startup /
    /// `:edit` open). Fire-and-forget: the fetched bytes (or a read error) return
    /// *inbound* on the run loop's open arm, not here. A silent no-op when no daemon fs
    /// is wired (the editor tick only enqueues opens in off-tick mode, so the gate at
    /// the call site means this is never reached in a local session).
    fn fs_fetch(&mut self, buffer: BufferId, path: String);

    /// Off-tick fs — push a buffer snapshot over the daemon write leg (the `:w`). Takes
    /// the snapshot's bytes out of `save` and writes them; the daemon's ack (the new
    /// [`FileStat`](nxvim_core::FileStat)) or a loud error returns *inbound* on the save
    /// arm — the buffer's saved-state clears only on that ack. A no-op without a daemon
    /// fs (unreachable; the caller gates on [`Self::has_remote_fs`]).
    fn fs_save(&mut self, save: PendingSave);

    /// Off-tick fs — arm a daemon-side watch on `path` (the `HostWatch` leg); a change
    /// returns *inbound* on the watch arm. A no-op without a daemon fs.
    fn fs_watch(&mut self, path: String);

    /// Off-tick fs — disarm the daemon watch on `path` (the buffer closed / lost its
    /// file). A no-op without a daemon fs.
    fn fs_unwatch(&mut self, path: String);

    /// Whether a daemon (off-tick) filesystem is wired. Gates the editor tick's remote
    /// vs. local branches — the remote watch arming in `sync_buffer_watches` and the
    /// off-tick open/save drains.
    fn has_remote_fs(&self) -> bool;

    /// LSP — ensure `key`'s language server is running (idempotent), spawning it via
    /// `spawn` on first use. Fire-and-forget; the server's notifications and reply
    /// stream return *inbound* on the run loop's `lsp_events` arm, not here.
    fn lsp_ensure(&mut self, key: ServerKey, spawn: ServerSpawn);

    /// LSP — fire-and-forget a document-sync notification (`didOpen` / `didChange` /
    /// `didSave` / `didClose`) at `key`'s server. Dropped if no such server is running.
    fn lsp_notify(&mut self, key: ServerKey, note: LspNotify);

    /// LSP — fire a language-feature request at `key`'s server; its reply returns later
    /// *inbound* as an `LspEvent::Reply` carrying `token` (the editor never awaits the
    /// round-trip). Dropped if no such server is running.
    fn lsp_request(&mut self, key: ServerKey, token: ReqToken, req: LspRequest);
}

/// The native implementation of [`HostEffects`]: the client wire is msgpack-RPC and
/// the command sink is the tokio [`EventLoop`] actor. Holds both transports so the
/// editor tick reaches neither directly — exactly the indirection the wasm build later
/// swaps for JS interop + the daemon link.
pub struct NativeEffects {
    rpc: Rpc,
    evloop: EventLoop,
    /// The daemon filesystem the off-tick read/write/watch legs route through (Phase 3's
    /// [`RemoteHostFs`](crate::RemoteHostFs)); `None` for a local/bare session (no
    /// off-tick fs), where [`Self::has_remote_fs`] is `false` and the editor tick takes
    /// its local branches.
    host_fs_async: Option<Arc<dyn HostFsAsync>>,
    /// Delivery for a finished off-tick read — the run loop's open arm drains it. The
    /// effect spawns the read and forwards `(buffer, path, result)` here.
    open_tx: UnboundedSender<(BufferId, String, io::Result<FsRead>)>,
    /// Delivery for a finished off-tick write — the run loop's save arm drains it. The
    /// effect spawns the write and forwards the [`SaveDone`] (ack-gated saved-state) here.
    save_done_tx: UnboundedSender<SaveDone>,
    /// The LSP command sink — the manager the editor tick fires `ensure` / `notify` /
    /// `request` at. Its inbound event/reply stream (`lsp_events`) is owned by the run
    /// loop's `select!`, not here (the inbound seam is the 4d slice).
    lsp: LspManager,
}

impl NativeEffects {
    pub fn new(
        rpc: Rpc,
        evloop: EventLoop,
        host_fs_async: Option<Arc<dyn HostFsAsync>>,
        open_tx: UnboundedSender<(BufferId, String, io::Result<FsRead>)>,
        save_done_tx: UnboundedSender<SaveDone>,
        lsp: LspManager,
    ) -> Self {
        Self {
            rpc,
            evloop,
            host_fs_async,
            open_tx,
            save_done_tx,
            lsp,
        }
    }
}

impl HostEffects for NativeEffects {
    fn notify(&mut self, method: &str, params: Vec<Value>) {
        self.rpc.notify(method, params);
    }

    fn respond(&mut self, id: u64, result: Result<Value, Value>) {
        self.rpc.respond(id, result);
    }

    fn loop_command(&mut self, cmd: LoopCommand) {
        // `EventLoop::send` lazily spawns the actor on first use, so routing through
        // it (rather than a bare cloned sender) preserves the "no task until first
        // command" property.
        self.evloop.send(cmd);
    }

    fn fs_fetch(&mut self, buffer: BufferId, path: String) {
        let Some(fs) = self.host_fs_async.clone() else {
            return;
        };
        let tx = self.open_tx.clone();
        tokio::spawn(async move {
            let result = fs.read(path.clone()).await;
            let _ = tx.send((buffer, path, result));
        });
    }

    fn fs_save(&mut self, mut save: PendingSave) {
        let Some(fs) = self.host_fs_async.clone() else {
            return;
        };
        // The bytes were snapshotted at command time; move them into the write and keep
        // their length for the `written` echo (the buffer can't be read after the move).
        let bytes = std::mem::take(&mut save.bytes);
        let bytes_len = bytes.len();
        let path = save.path.display().to_string();
        let tx = self.save_done_tx.clone();
        tokio::spawn(async move {
            let result = fs.write(path, bytes).await;
            let _ = tx.send(SaveDone::new(save, bytes_len, result));
        });
    }

    fn fs_watch(&mut self, path: String) {
        if let Some(fs) = &self.host_fs_async {
            fs.watch(path);
        }
    }

    fn fs_unwatch(&mut self, path: String) {
        if let Some(fs) = &self.host_fs_async {
            fs.unwatch(path);
        }
    }

    fn has_remote_fs(&self) -> bool {
        self.host_fs_async.is_some()
    }

    fn lsp_ensure(&mut self, key: ServerKey, spawn: ServerSpawn) {
        self.lsp.ensure_server(key, spawn);
    }

    fn lsp_notify(&mut self, key: ServerKey, note: LspNotify) {
        self.lsp.notify(key, note);
    }

    fn lsp_request(&mut self, key: ServerKey, token: ReqToken, req: LspRequest) {
        self.lsp.request(key, token, req);
    }
}
