# The async Lua runtime (event loop) — implementation plan

## Why this document exists

Today **all Lua in nxvim runs synchronously on the input tick**. There is no
event loop, so the three primitives a real plugin (and a real `lsp/<server>.lua`
config) reaches for are faked:

| primitive | neovim semantics | nxvim today | where |
| --- | --- | --- | --- |
| `vim.schedule(fn)` | run `fn` on a **later** main-loop turn | runs `fn` **inline**, nested in the caller | `prelude.lua` → `function vim.schedule(fn) fn() end` |
| `vim.defer_fn(fn, ms)` | run `fn` after `ms` on the loop | **raises** `vim._notimpl` | `prelude.lua` → `vim.defer_fn` |
| `vim.system(cmd, …)` | spawn async; `on_exit` fires off-tick | spawns and **blocks the server thread** until exit; `on_exit`/`:wait()` see an already-complete result | `prelude.lua` `vim.system` → Rust `vim._system` (`std::process::Command::output()`) |
| `vim.uv`/`vim.loop` timers | libuv timer handles | **absent** (only `fs_stat`/`os_homedir`/`cwd`/`fs_realpath`/`os_uname` exist, all synchronous) | `nxvim-lua/src/lib.rs` `uv` table |

This is **foundational work**: it is Phase 4 of
[`docs/lsp-completion-plan.md`](lsp-completion-plan.md) and the pivot the back
half of LSP (real `client:request`, `vim.lsp.util.*`, `vim.ui.*`) builds on, and
it unblocks the general `vim.loop`/`vim.uv` surface every async plugin assumes.
The architecture doc already lists it as the next roadmap gap (*"An async Lua
runtime (event loop)"*).

The plan is divided into self-contained phases. **Each is sized to be picked up
and implemented in one focused session without the others loaded.** Phases list
their dependencies; later phases assume earlier ones landed. After each phase the
set of `vim._notimpl_hits` a real config triggers shrinks — that set is the
running scoreboard (the same one the LSP plan uses).

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

| phase | title | status |
| --- | --- | --- |
| 1 | The callback registry + deferred `vim.schedule` (no threads) | ⬜ |
| 2 | The event-loop actor + timers (`vim.defer_fn`, `vim.uv`/`vim.loop`) | ⬜ |
| 3 | Async `vim.system` (off-tick `on_exit`, real `pid`, `kill`) | ⬜ |
| 4 | The async-request seam + robustness + scoreboard cleanup | ⬜ |

---

## The one constraint that shapes everything

**The editor core and the Lua VM are `!Send` and live on a single thread.** The
server runs on a `tokio::runtime::Builder::new_current_thread()` runtime
(`crates/nxvim/src/main.rs`), and processes **one message at a time** against
that non-`Send` state (`architecture.md` → *Async design*). Concurrency comes
from async **I/O**, never parallel mutation.

So the event loop is **not** "run Lua callbacks on background threads." It is:

> A `Send` background **actor task** owns the things that take time (timers,
> child processes). When one completes, it sends a small typed **event** back to
> the single-threaded server over a channel. The server — and *only* the server,
> on its one thread — runs the corresponding Lua callback, then drains effects
> and repaints. The Lua VM never crosses the thread boundary.

This is exactly the pattern already proven twice in the codebase:

- **`SyntaxClient`** (`crates/nxvim-server/src/syntax.rs`) — the treesitter
  worker actor; the main loop selects on its `SyntaxEvent` channel.
- **`LspManager`** (`crates/nxvim-lsp/src/manager.rs`) — `LspManager::new()`
  returns `(LspManager, UnboundedReceiver<LspEvent>)`; a lazily-`tokio::spawn`ed
  `run_supervisor` task owns the child language servers and ferries replies back
  over the `LspEvent` channel, correlated by an opaque `ReqToken`. Commands go
  out fire-and-forget via an `UnboundedSender<LspCommand>`; the editor **never
  awaits** a round-trip.

**The event loop is a third actor of the same shape.** Do not invent a new
concurrency model — copy `LspManager`'s structure (lazy spawn, two unbounded
channels, the event receiver added as a new `tokio::select!` arm in
`nxvim-server::run`).

---

## The current main loop (what we are extending)

`crates/nxvim-server/src/lib.rs` → `run()`:

```rust
loop {
    tokio::select! {
        message = incoming.recv()          => { server.handle(message).await; … }   // RPC from the UI
        Some(event) = syntax_events.recv() => { server.on_syntax_event(event); … }   // treesitter actor
        Some(event) = lsp_events.recv()    => { server.on_lsp_event(event); … }      // LSP actor
    }
}
```

Two mechanisms matter for this plan:

1. **The effect queues (`Shared`, `crates/nxvim-lua/src/lib.rs`).** Lua never
   mutates the editor; it pushes onto `Shared` (`commands`, `output`,
   `highlights`, `panel_ops`, `lsp_ops`). After every Lua chunk the server calls
   `apply_lua_effects()` to drain them into the core. We add **one more queue**
   (`loop_ops`) here.

2. **The convergence driver (`run_pending`, `crates/nxvim-server/src/lib.rs`).**
   A fixpoint loop that drains `editor.lua_queue`, `editor.deferred_commands`,
   and `editor.panel_selects` until nothing new is queued, capped at
   `MAX_ROUNDS = 100` (the recursion guard). Lua-backed work re-enters here. We
   add the **scheduled-callback queue** as one more source inside this loop.

3. **Rust→Lua calls (the bridge).** The server calls back *into* Lua by fetching
   a `vim._*` function and invoking it: `run_keymap(id)` → `vim._run_keymap`,
   `run_user_command(name, args)`, `run_panel_select(index, line)`,
   `run_lsp_on_init(id, result)`, `fire_autocmd_*`. Callbacks live in the Lua
   registry keyed by id (e.g. `vim._keymap_fns[id]`). **The deferred-callback
   registry is one more of these**, and it is the spine of every phase.

---

## Target architecture

```
            ┌──────────────────────────── server thread (single, !Send) ───────────────────────────┐
            │                                                                                        │
  RPC ─────▶│  handle()  ┐                                                                           │
            │            ├─▶ run Lua ─▶ apply_lua_effects() ─▶ drain Shared.loop_ops ──┐             │
  evloop ──▶│  on_loop_event(LoopEvent::{Timer,Process,…})                             │             │
   events   │            └─▶ run vim._run_cb(id) ─▶ apply_lua_effects() ─▶ run_pending()│ ──▶ redraw  │
            │                                                                          │             │
            │                                                LoopCommand  (fire-and-forget, !await)  │
            └────────────────────────────────────────────────────────┼───────────────────────────── ┘
                                                                       ▼
                          ┌──────────────── evloop actor (tokio::spawn, Send) ────────────────┐
                          │  owns: the timer wheel (tokio::time) + child procs (tokio::process)│
                          │  on completion ─▶ LoopEvent { cb_id, payload } ─▶ event channel ───┼──▶ back to server
                          └───────────────────────────────────────────────────────────────────┘
```

**Four mechanisms, introduced across the phases:**

- **A callback registry (Phase 1).** Lua stores deferred functions by integer id
  in `vim._cb_fns[id]`; `vim._next_cb_id()` allocates, `vim._run_cb(id, …)`
  invokes (and drops one-shots). Rust side: `LuaRuntime::run_callback(id, args)`
  — the `run_keymap` analogue. This is reused by *every* later phase.

- **A `Shared.loop_ops` queue (Phase 1).** Lua queues a `LoopOp` (`Schedule`,
  later `TimerStart`/`TimerStop`, `Spawn`/`Kill`) carrying a `cb_id`; the server
  drains it in `apply_lua_effects()` and either services it directly
  (`Schedule`) or forwards it to the actor (timers/procs).

- **The evloop actor (Phase 2).** A `Send` `tokio::spawn`ed task (the
  `LspManager`/`run_supervisor` twin) owning timers and processes, with a
  `LoopCommand` receiver and a `LoopEvent` sender wired as a new `select!` arm.

- **The settle contract (Phase 1, enforced everywhere after).** *Any* entry point
  that runs Lua ends with `apply_lua_effects()` → `run_pending()` → `redraw()`.
  The new event arms obey it; this also closes a latent gap (the `lsp_events` arm
  today calls `on_lsp_event` but **not** `run_pending`, so a `vim.cmd` deferred by
  an `on_init`/`LspAttach` callback isn't driven to convergence off-tick).

---

## Phase 1 — The callback registry + deferred `vim.schedule` ⬜

**Goal.** Make `vim.schedule(fn)` genuinely defer (run after the current work
converges, **not** nested inside the caller), and lay the callback-id plumbing
every later phase reuses. **No threads, no new dependencies** — pure
queue-and-drain, the same shape as `vim.cmd`/`lsp_ops`.

**Why.** This is the spine. Timers (Phase 2), async `vim.system` (Phase 3), and
the request seam (Phase 4) all "register a Lua callback under an id, run it later
by id." Build and test that mechanism once, in isolation, with the simplest
consumer (`schedule`).

**Scope (files).**
- `crates/nxvim-lua/src/prelude.lua` — `vim.schedule`, new `vim._cb_fns` /
  `vim._next_cb_id` / `vim._run_cb`.
- `crates/nxvim-lua/src/lib.rs` — `Shared.loop_ops`, `LoopOp` enum,
  `take_loop_ops()`, `LuaRuntime::run_callback(id)`, the `vim._schedule(id)`
  bridge fn.
- `crates/nxvim-server/src/lib.rs` — a server-side `scheduled: VecDeque<u64>`,
  drained inside `run_pending`; `apply_lua_effects` forwards `LoopOp::Schedule`
  into it.

**Approach.**

1. **Registry (Lua).** In the prelude:
   ```lua
   vim._cb_fns = vim._cb_fns or {}
   vim._cb_seq = 0
   function vim._next_cb_id() vim._cb_seq = vim._cb_seq + 1; return vim._cb_seq end
   -- run a registered callback by id; `keep` true for repeating timers.
   function vim._run_cb(id, keep, ...)
     local fn = vim._cb_fns[id]
     if not keep then vim._cb_fns[id] = nil end   -- one-shot: drop so the registry can't leak
     if fn then return fn(...) end
   end
   function vim.schedule(fn)
     local id = vim._next_cb_id()
     vim._cb_fns[id] = fn
     vim._schedule(id)        -- Rust bridge: push LoopOp::Schedule{id} onto Shared.loop_ops
   end
   ```
   Mirror `vim._keymap_fns`. **Drop one-shot callbacks after they run** (the
   `keep == false` path) so the registry doesn't grow unbounded.

2. **Bridge (Rust).** Add to `Shared`:
   ```rust
   loop_ops: Vec<LoopOp>,
   ```
   and `pub enum LoopOp { Schedule { id: u64 }, /* Phase 2+: TimerStart/Stop, Spawn/Kill */ }`,
   plus `pub fn take_loop_ops(&self) -> Vec<LoopOp>`. Register `vim._schedule`
   (pushes `LoopOp::Schedule { id }`). Add
   `LuaRuntime::run_callback(&self, id: u64) -> mlua::Result<()>` calling
   `vim._run_cb(id, false)` — the `run_keymap` twin.

3. **Drain + drive (server).** In `apply_lua_effects()`, after the `lsp_ops`
   drain, push every `LoopOp::Schedule { id }` onto `self.scheduled`. In
   `run_pending`'s fixpoint loop, add a clause that drains `self.scheduled`:
   ```rust
   for id in std::mem::take(&mut self.scheduled) {
       if let Err(e) = self.lua.run_callback(id) {
           self.editor.echo(format!("E5108: Error in scheduled callback: {e}"));
       }
       self.apply_lua_effects();   // a scheduled fn may itself queue work
   }
   ```
   Include `self.scheduled.is_empty()` in the loop's exit condition, so a
   callback that schedules more work keeps converging (bounded by the existing
   `MAX_ROUNDS`).

**Semantics to be precise about.** `vim.schedule` here defers to **end of the
current convergence** (still the same RPC handler, but no longer nested in the
caller's stack frame). That is the strict improvement the LSP plan asks for and
exactly what the colorscheme's "defer to avoid reentrancy" needs. It is **not**
yet a "later wall-clock loop turn" — Phase 2, once the actor exists, may promote
`vim.schedule` to a true next-turn deferral (`vim.schedule(fn) ≈
vim.defer_fn(fn, 0)`) if a consumer needs it; note this as a known refinement,
not a gap.

**Robustness (start here, hold for all phases).**
- **Error isolation.** A throwing deferred callback is caught, echoed as
  `E5108`, and **never** aborts the drain loop or the server. (The funnel is
  `run_callback`'s caller — one `match` per callback.)
- **Re-entrancy.** A callback that calls `vim.schedule` again must not deadlock
  or busy-loop: it lands in `self.scheduled` and is picked up by the next
  fixpoint iteration, capped by `MAX_ROUNDS` (echo `E132` on overflow, as today).

**Tests** (`crates/nxvim-server/tests/editing.rs`, black-box per conventions).
Because `vim.schedule` defers within the same handler, assert on **ordering**,
not wall-clock:
- A `:lua` chunk that does
  `vim.schedule(function() vim.cmd('normal! Ascheduled') end); vim.cmd('normal! Adirect')`
  leaves the buffer with `direct` applied **before** `scheduled` (proof it ran
  after, not inline). Drive via `nvim_exec_lua` or `nvim_command("lua …")`, then
  `lines(&rpc).await`.
- A scheduled callback that itself schedules another callback runs both (proof
  the fixpoint picks up work queued mid-drain).
- A scheduled callback that `error()`s surfaces an `E5108` message and does
  **not** prevent a second scheduled callback from running.

**Done when.** `vim.schedule(fn)` runs `fn` after the current convergence rather
than inline; the callback registry (`vim._cb_fns` / `_next_cb_id` / `_run_cb`),
`Shared.loop_ops` + `LoopOp::Schedule`, `LuaRuntime::run_callback`, and the
`run_pending` drain all exist and are exercised by the ordering/error tests
above. One-shot callbacks are dropped after firing (no registry leak).

**Depends on.** Nothing (builds on the existing `Shared`/`run_pending` patterns).

---

## Phase 2 — The event-loop actor + timers ⬜

**Goal.** Stand up the background actor and the `tokio::select!` arm, then ship
timers on it: `vim.defer_fn(fn, ms)` honors the delay and fires off-tick, and
`vim.uv`/`vim.loop` timer handles (`new_timer` → `:start`/`:stop`/`:close`,
plus `timer_start`/`timer_stop`) work.

**Why.** This is the actual event loop: the first thing that wakes the server on
**wall-clock time** rather than RPC. `vim.defer_fn` is a Phase-0 `_notimpl`
raise that real configs use for deferred-retry patterns; timers are the most
common `vim.uv` primitive plugins assume.

**Scope (files).**
- **New:** `crates/nxvim-server/src/evloop.rs` — the actor (`EventLoop` handle +
  `run_evloop` task), modeled line-for-line on `crates/nxvim-lsp/src/manager.rs`
  (`LspManager`/`run_supervisor`).
- `crates/nxvim-server/src/lib.rs` — construct it in `run()`
  (`let (evloop, mut loop_events) = EventLoop::new();`), add the
  `Some(event) = loop_events.recv()` arm, add `on_loop_event`, forward
  `LoopOp::TimerStart`/`TimerStop` from `apply_lua_effects`.
- `crates/nxvim-lua/src/lib.rs` — `LoopOp::TimerStart { id, delay_ms, repeat_ms }`
  / `TimerStop { id }`; bridge fns `vim._timer_start` / `vim._timer_stop`.
- `crates/nxvim-lua/src/prelude.lua` — real `vim.defer_fn`; `vim.uv.new_timer`
  (and `timer_start`/`timer_stop`) layered on the bridge; `vim.loop` already
  aliases `vim.uv`.

**Approach.**

1. **The actor** (copy `LspManager`'s skeleton):
   ```rust
   pub struct EventLoop { cmd_tx: UnboundedSender<LoopCommand>,
                          start: Option<(UnboundedReceiver<LoopCommand>, UnboundedSender<LoopEvent>)> }
   pub enum LoopCommand { TimerStart { id: u64, delay: Duration, repeat: Duration },
                          TimerStop { id: u64 }, /* Phase 3: Spawn/Kill */ }
   pub enum LoopEvent   { Timer { id: u64, keep: bool }, /* Phase 3: ProcessExit{…} */ }
   ```
   `EventLoop::new()` returns `(EventLoop, UnboundedReceiver<LoopEvent>)` and
   makes both channels; the `run_evloop` task is `tokio::spawn`ed lazily on the
   first command (as `LspManager` spawns `run_supervisor` on first
   `ensure_server`), so a session that never sets a timer spawns nothing.
   `run_evloop` keeps a `HashMap<u64, JoinHandle<()>>` (or an
   abort registry) of live timers. A one-shot timer: spawn a child task that
   `tokio::time::sleep(delay).await` then `event_tx.send(LoopEvent::Timer{id, keep:false})`.
   A repeating timer (`repeat > 0`): loop `sleep(delay)` then `sleep(repeat)`…
   sending `keep:true` each fire until `TimerStop{id}` aborts the task. **All of
   this is `Send`** — it touches only ids, durations, and channels, never Lua.

2. **Servicing an event (server).**
   ```rust
   Some(event) = loop_events.recv() => {
       server.on_loop_event(event);
       while let Ok(event) = loop_events.try_recv() { server.on_loop_event(event); }
       server.run_pending();            // the settle contract
       server.redraw();
   }
   ```
   `on_loop_event(LoopEvent::Timer{id, keep})` calls
   `self.lua.run_callback_keep(id, keep)` (a `run_callback` variant passing the
   `keep` flag to `vim._run_cb`, so a repeating timer's fn is retained), then
   `apply_lua_effects()`. Coalesce a burst into one repaint, exactly like the
   syntax/LSP arms.

3. **`vim.defer_fn` (prelude).**
   ```lua
   function vim.defer_fn(fn, timeout)
     local id = vim._next_cb_id(); vim._cb_fns[id] = fn
     vim._timer_start(id, timeout, 0)         -- one-shot
     return vim.uv.new_timer_handle(id)       -- a handle with :stop()/:close()
   end
   ```
   Replace the `vim._notimpl("vim.defer_fn")` raise.

4. **`vim.uv` timers (prelude).** `vim.uv.new_timer()` returns a handle table
   carrying a fresh `cb_id`; `handle:start(timeout, repeat, cb)` stores `cb` in
   `vim._cb_fns[id]` and calls `vim._timer_start(id, timeout, repeat)`;
   `handle:stop()` and `handle:close()` call `vim._timer_stop(id)` (and drop the
   fn). Repeat semantics: `keep = repeat > 0`, threaded through the actor.

**Test-time observability.** The black-box harness is fully async
(`#[tokio::test]`, server on its own runtime thread). A timer test:
1. `feed`/`nvim_command` something that calls `vim.defer_fn(set_a_line, 30)`.
2. Immediately `lines(&rpc).await` (a barrier) — assert the line is **not yet**
   present (proof it didn't run inline).
3. `tokio::time::sleep(Duration::from_millis(80)).await`, then `lines(&rpc).await`
   — assert the line **is** present (proof the actor fired it off-tick). The
   server's `select!` processes the `LoopEvent` the moment it arrives, so the
   post-sleep barrier observes the settled state.

Use a small, generous delay (tens of ms) and a longer wait to keep it
non-flaky; mirror the timing tolerance of the existing `:sleep` responsiveness
test in `crates/nxvim/tests/e2e.rs`.

**Tests.**
- `vim.defer_fn(fn, ms)` runs `fn` after the delay, not before (the two-barrier
  pattern above).
- A `vim.uv.new_timer()` with `repeat > 0` fires more than once, and `:stop()`
  halts it (assert the effect count stops growing after stop).
- A throwing timer callback echoes `E5108` and does not wedge the loop (a second
  timer still fires).

**Done when.** `vim.defer_fn` honors its delay and fires off the input tick;
`vim.uv`/`vim.loop` one-shot and repeating timers work and are stoppable; the
`evloop` actor + `loop_events` arm + `on_loop_event` exist and obey the settle
contract; `vim.defer_fn` is off the `_notimpl` list.

**Depends on.** Phase 1 (the registry + `LoopOp` + drain).

---

## Phase 3 — Async `vim.system` ⬜

**Goal.** `vim.system(cmd, opts, on_exit)` spawns the child **without blocking
the server thread**; `on_exit` fires on a later tick with `{code, stdout, stderr}`;
the handle carries a real `pid` and a working `kill()`. The synchronous
`root_dir` shell-out path keeps working.

**Why.** Removes the "a slow command blocks the server" limitation (the caveat
the architecture doc and the LSP plan both call out) and is the first non-timer
consumer of the actor — the template the request seam (Phase 4) follows.

**Scope (files).**
- `crates/nxvim-server/src/evloop.rs` — extend with `LoopCommand::Spawn { id, argv,
  cwd, env }` / `Kill { id }` and `LoopEvent::ProcessExit { id, code, stdout, stderr }`;
  the task spawns via `tokio::process::Command`, awaits `output()`/`wait_with_output()`,
  sends the exit event. (`tokio` already has the `process` feature.)
- `crates/nxvim-lua/src/lib.rs` — `LoopOp::Spawn`/`Kill`; a `vim._system_async(id,
  cmd, cwd, env)` bridge that returns the child **pid** synchronously (so the
  handle is real) while the wait happens in the actor. Keep the existing blocking
  `vim._system` for `:wait()`.
- `crates/nxvim-lua/src/prelude.lua` — rework `vim.system` to register `on_exit`
  under a `cb_id` and go async when an `on_exit` is supplied.

**Approach & the `:wait()` decision.** neovim's `vim.system():wait()` pumps the
event loop until the child exits; replicating that on a single thread is the
sharp edge. Take the **pragmatic split** that keeps every current caller working:

- **`on_exit` given** → async. Register `on_exit` in `vim._cb_fns[id]`, queue
  `LoopOp::Spawn{id,…}`; the actor runs the child and sends `ProcessExit`;
  `on_loop_event` builds the result table and calls `vim._run_cb(id, false, result)`.
  Off-tick, non-blocking.
- **`:wait()` called** → synchronous. Keep today's blocking `vim._system`
  (`std::process::Command::output()`), which is exactly what an `lsp/*.lua`
  `root_dir` needs (`cargo metadata`, `rustc --print sysroot`) and is short by
  construction. `:wait()` calls it and returns the complete result.

Document the divergence from neovim plainly (a handle that was spawned with
`on_exit` and *then* `:wait()`-ed will re-run/!!—avoid by making the handle
remember it was already spawned: if async, `:wait()` blocks on a per-spawn
`oneshot` the actor also fills; if you don't need that fidelity yet, restrict
`:wait()` to the no-`on_exit` form and have the async form's handle `:wait()`
raise a clear "already async" error rather than double-spawn). Pick the simplest
that keeps the config sweep green; the config path uses `:wait()` **without**
`on_exit`, so the blocking branch covers it.

- **`pid`/`kill`.** The async spawn returns the OS pid (the prelude currently
  reads `result.pid`, which `vim._system` never sets — fix that too).
  `handle:kill(signal)` queues `LoopOp::Kill{id}`; the actor signals/aborts the
  child.

**Tests.**
- `vim.system({…}, {}, on_exit)` with an `on_exit` that sets a buffer line: the
  line is absent at the immediate barrier and present after a short wait (the
  off-tick proof, two-barrier pattern from Phase 2).
- A `vim.system(...):wait()` with **no** `on_exit` still returns the complete
  `{code, stdout, stderr}` synchronously (the `root_dir` path — regression
  guard).
- The existing `crates/nxvim-lua/tests/lspconfig_configs.rs` sweep stays green
  (the synchronous `:wait()` branch unchanged for config resolution).

**Done when.** `vim.system` with `on_exit` runs the child asynchronously and
fires `on_exit` off-tick; `:wait()` still resolves synchronously for the config
path; the handle exposes a real `pid` and a working `kill`; the config sweep is
green and the architecture doc's "runs the child process synchronously" caveat
is retired (and the doc updated).

**Depends on.** Phase 2 (the actor + the `ProcessExit` event arm). Reuses Phase
1's registry for `on_exit`.

---

## Phase 4 — The async-request seam + robustness + scoreboard cleanup ⬜

**Goal.** Generalize the "issue async work → off-tick Lua callback" path into the
reusable seam LSP **completion-plan Phase 5** (`client:request`) plugs into,
harden the whole runtime against leaks/re-entrancy/ordering bugs, and tidy the
`vim._notimpl` scoreboard.

**Why.** The loop's *point* is to let other subsystems hand work off and get a
Lua callback back later. LSP `client:request(method, params, handler)` is the
headline consumer (server-specific commands, config `handlers`, `vim.lsp.util`
round-trips). This phase makes that handoff a documented primitive rather than a
per-consumer hack, and pays down the robustness items that are easy to get wrong
under concurrency.

**Scope (files).** `crates/nxvim-lua/src/lib.rs` (the seam API + `run_callback`
variants), `crates/nxvim-server/src/lib.rs` (the central `settle()` helper),
`crates/nxvim-lua/src/prelude.lua` (`vim.schedule_wrap`, any now-implementable
stubs), plus a short note in `docs/lsp-completion-plan.md` Phase 5 pointing at
the seam.

**Approach.**

1. **The seam.** Document and expose the canonical handoff so a subsystem
   (LSP manager, future DAP, future watchers) can route an async completion to a
   Lua callback **without** touching the loop internals:
   - register a handler: `let id = lua.register_callback(fn)` (the Rust-side
     allocator, the twin of the Lua `_next_cb_id`, for callbacks born in Rust);
   - on completion, the owning actor sends its event; `on_<x>_event` calls
     `lua.run_callback_with(id, payload_as_lua)`.
   For LSP specifically: `client:request` queues an `LspOp`-style request
   carrying a `cb_id`; the `LspManager` already correlates replies by `ReqToken`
   (`crates/nxvim-lsp/src/manager.rs`) — thread the `cb_id` alongside the token so
   `LspEvent::Reply` carries it back, and `on_lsp_event` runs
   `vim._run_cb(cb_id, false, err, result)`. (The wiring lives in the LSP plan;
   **this** plan owns the callback-dispatch primitive it calls.)

2. **`vim.schedule_wrap(fn)`** — returns a function that, when called, schedules
   `fn` with its arguments (a common plugin idiom; trivial on Phase 1).

3. **Robustness pass (audit across all phases).**
   - **No registry leaks.** Every one-shot path (`schedule`, `defer_fn`,
     `system` `on_exit`, a `request` handler) drops its `vim._cb_fns[id]` after
     firing; repeating timers drop on `:stop()`/`:close()`. Add a test that a
     long sequence of one-shots leaves `vim._cb_fns` empty.
   - **Ordering.** Two timers with the same deadline, and a `schedule` plus a
     `defer_fn(…, 0)`, fire in a defined, documented order (FIFO by enqueue).
   - **Re-entrancy & the `MAX_ROUNDS` cap.** A callback that re-arms itself every
     tick must not starve RPC: confirm timer re-fires go through the `select!`
     (a fresh loop turn, so RPC is serviced between them), while same-turn
     `schedule` storms are bounded by `MAX_ROUNDS` with the `E132` echo.
   - **The settle contract, centralized.** Factor the repeated
     `apply_lua_effects → run_pending → redraw` tail into one `Server::settle()`
     and call it from `handle()`, the input path, and the syntax/LSP/loop event
     arms — closing the `lsp_events`-arm gap noted earlier and guaranteeing every
     off-tick callback's deferred work converges and repaints.

4. **Scoreboard cleanup.** With the loop in place, the only `_notimpl` entries
   the loop itself removes are `vim.defer_fn` (Phase 2). Re-run a representative
   config and record which remaining `_notimpl_hits` are now *unblocked* for the
   LSP plan's Phases 5/7/8 (e.g. `client:request`-backed paths) versus still
   genuinely pending — keep the running list honest.

**Tests.**
- A Rust-registered callback fired from a simulated async completion runs on a
  later tick with its payload (drive through the mock in
  `crates/nxvim-lsp/src/mock.rs` if wiring the LSP end, else a unit-style
  black-box through `vim.system`).
- `vim.schedule_wrap` defers correctly.
- The leak test (`vim._cb_fns` empty after N one-shots) and the ordering test.

**Done when.** There is a single documented callback-dispatch primitive both
`vim.system`/timers and a Rust subsystem use; `vim.schedule_wrap` works; the
leak/ordering/re-entrancy tests pass; `Server::settle()` is the one tail every
Lua-running entry point shares; and the LSP plan's Phase 5 can reference this
seam instead of inventing its own.

**Depends on.** Phases 1–3. Hands off to `lsp-completion-plan.md` Phase 5.

---

## Suggested order & scoreboard

`1 → 2 → 3 → 4`. Phase 1 is the foundation (registry + drive); Phase 2 is the
actual loop (actor + the wall-clock wake); Phase 3 proves the actor on a second
consumer; Phase 4 generalizes the seam and hardens. After Phase 2, `vim.defer_fn`
leaves the `_notimpl` scoreboard; after Phase 4, the LSP back half is unblocked.

**The running scoreboard is `vim._notimpl_hits`** (introduced in LSP Phase 0).
Re-run a real config after each phase; the set shrinks. This plan directly clears
`vim.defer_fn`; it *unblocks* (for the LSP plan to clear) the deferred-callback
surface — `client:request`, the `vim.lsp.util.*` round-trips, `vim.ui.*`.

---

## Testing appendix — observing async in a black-box harness

The conventions are unchanged (`architecture.md` → *Testing philosophy*): **no
unit tests**; drive a real server over RPC and assert on observable state
(`crates/nxvim-server/tests/editing.rs`, helpers `start`/`feed`/`lines`/`cursor`).
Two patterns cover the async surface:

1. **Deferred-within-a-tick (Phase 1, `vim.schedule`).** The effect lands in the
   *same* RPC handler (at convergence), so it is visible at the next barrier —
   assert on **ordering** (scheduled work applied after direct work), not timing.

2. **Off-tick (Phases 2–4, timers / async `vim.system` / requests).** The effect
   lands on a *later* loop turn driven by the actor. Use the **two-barrier**
   pattern:
   - barrier #1 immediately after the trigger asserts the effect is **absent**
     (it didn't run inline / synchronously);
   - `tokio::time::sleep` past the delay, then barrier #2 asserts the effect is
     **present** (the actor fired it and the server settled).

   `lines(&rpc).await` is the barrier (the harness comment already notes it
   doubles as one). Keep delays generous (tens of ms set, ~2–3× wait) to stay off
   the flaky edge, matching the `:sleep` responsiveness test in
   `crates/nxvim/tests/e2e.rs`.

A guard worth adding once: a callback scheduled/deferred from a "fast" context
must still be applied, and a UI that never drains redraws must not stall the
editor (the non-blocking guarantee the Tier-2 screen tests already assert).

---

## Risks & non-goals

- **Do not make the Lua VM `Send` or move it off-thread.** Every "later" callback
  runs on the server thread when its event is received. The actor only ever
  handles ids, durations, argv, and bytes. If a design pressures you to send Lua
  across threads, it is wrong — re-derive it from the `LspManager` pattern.
- **`:wait()` fidelity is a deliberate approximation** (Phase 3). Full
  loop-pumping `:wait()` is out of scope; the synchronous blocking branch covers
  every current caller (config `root_dir`). Document it; don't gold-plate it.
- **The `vim.uv` surface stays demand-driven.** This plan ships *timers* (the gap
  that blocks the loop) and leaves the rest of libuv (`new_pipe`, `new_async`,
  `fs_*` event-based, TCP) to land as plugins actually demand it — the same
  "grows as plugins demand it" rule the architecture doc states. (`vim.lsp.rpc.connect`'s
  TCP transport — the skipped `gdscript` config — is one such future item, not
  part of this plan.)
- **`MAX_ROUNDS` is shared.** Scheduled-callback storms share the existing
  recursion guard; confirm a legitimate deep-but-finite chain still converges
  under it before raising the cap.
- **Base branch.** This plan assumes the `feature/lsp-integration` substrate (the
  `LspManager` actor, the `lsp_events` arm, the `Shared.lsp_ops` queue) as both
  its reference pattern and its motivating consumer. The same design works on
  `main`, which already has the `syntax_events` actor arm to copy. Build it on or
  after the LSP branch (its Phase 5 is the first external consumer), or develop it
  independently and rebase — the actor/registry/drain mechanisms are orthogonal
  to LSP.
