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

use crate::evloop::{EventLoop, LoopCommand};
use nxvim_rpc::Rpc;
use rmpv::Value;

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
}

/// The native implementation of [`HostEffects`]: the client wire is msgpack-RPC and
/// the command sink is the tokio [`EventLoop`] actor. Holds both transports so the
/// editor tick reaches neither directly — exactly the indirection the wasm build later
/// swaps for JS interop + the daemon link.
pub struct NativeEffects {
    rpc: Rpc,
    evloop: EventLoop,
}

impl NativeEffects {
    pub fn new(rpc: Rpc, evloop: EventLoop) -> Self {
        Self { rpc, evloop }
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
}
