# Async event model: hot-path events, a gated read chain, replay to late subscribers, and the session-restore lifecycle gap

Date: 2026-07-26

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

| Phase | Title | Status |
| --- | --- | --- |
| 1 | Event classification + hot-path events are synchronous-only | ✅ |
| 2 | Registration-site capture on every autocmd | ✅ |
| 3 | Watermark + replay to late subscribers, with a settle budget | ✅ |
| 4 | Read lifecycle decoupled from the current-buffer diff (the restore gap) | ✅ |
| 5 | Chain gating: the per-buffer read-lifecycle state machine | ✅ |
| 6 | Plugin-manager cleanup, docs, example | ✅ |

---

## Problem

Three defects that compound into one user-visible failure: **restore a session,
and the plugins that should configure the restored buffers never run for them.**

### D1 — The read lifecycle only fires for the *current* buffer

`emit_lifecycle_events` (`crates/nxvim-server/src/lifecycle.rs:336`) is a
current-buffer diff: it takes `buf = self.editor.current_buffer_id()`
(`lifecycle.rs:353`) and the `BufReadPost`/`BufNewFile`/`FileType` block
(`lifecycle.rs:628`) is gated on that single buffer's `announced` state. Only
`BufWinEnter` (`lifecycle.rs:741`) walks every window.

A session restore fills *non-current* windows, so those buffers fire
`BufWinEnter` and nothing else. Measured (three files restored into a split
layout, autocmds registered via `client_init_lua` so they are live before the
lifecycle seed):

```
BufReadPost  : scratch_c.py     <- only the focused buffer
FileType     : scratch_c.py
BufEnter     : scratch_c.py
BufWinEnter  : scratch_c.py
BufWinEnter  : scratch_b.lua    <- background windows get ONLY this
BufWinEnter  : scratch_a.txt
```

No `BufAdd` either: restore runs inside `shada_load` (`lib.rs:3685`), which is
*before* `known_buffers` is seeded and `startup_bufs_seeded` is set
(`lib.rs:3793-3794`), so every restored buffer is treated as startup baseline.

It is deferred rather than lost — a restored buffer was never added to
`announced`, so focusing it later fires the read lifecycle then. But everything
`FileType`-driven (LSP attach, treesitter, buffer-local maps) is inert until the
user clicks into the window.

**Neovim does not behave this way.** Measured against nvim 0.12.2 with
`:mksession` + `:source Session.vim`:

```
BufAdd:a.lua  BufAdd:b.py  BufAdd:c.txt
BufReadPost:c.txt   FileType:c.txt   BufEnter:c.txt   BufWinEnter:c.txt
BufReadPost:b.py    FileType:b.py    BufEnter:b.py    BufWinEnter:b.py
BufReadPost:a.lua   FileType:a.lua   BufEnter:a.lua   BufWinEnter:a.lua
```

The mechanism is in the generated session file: it `badd`s every file (listed
but **unloaded**), then per window runs
`if bufexists(...) | buffer b.py | else | edit b.py | endif`. `:buffer` on an
unloaded buffer loads it, which runs the full read lifecycle. Note the rule is
"every buffer that lands in a **window**" — a `badd`-only buffer that never
reaches a window stays unloaded and fires no `FileType`.

### D2 — `ft` / `event` lazy triggers never re-fire

`arm_lazy` (`crates/nxvim-lua/src/prelude/plugins.lua:472-482`) arms the `event`
and `ft` triggers as bare `load_reporting(name)` calls. The `cmd` trigger
re-dispatches the original invocation (`plugins.lua:458`) and `keys` feeds the
key back through the typeahead (`plugins.lua:490`), but `ft`/`event` do neither.

So an `ft`-lazy plugin loads and registers its own `FileType` autocmd — for a
`FileType` event that has already finished dispatching. Its handler never runs
for the buffer that woke it. This is independent of sessions: it breaks on
*every* open.

The comment at `plugins.lua:442-443` claims "the trigger re-fires against the
now-loaded plugin"; the parenthetical that follows only describes `cmd` and
`keys`. The code matches the parenthetical, not the claim.

For reference, lazy.nvim solves this by re-firing the event bound to the
triggering buffer after loading
(`lazy/core/handler/event.lua:161`, `nvim_exec_autocmds(opts.event, { buffer = opts.buffer, ... })`).
Verified: an `ft = "python"` plugin's own handler *does* run for the restored
`b.py`, while an `event = "VeryLazy"` plugin loads after the restore and sees
nothing.

### D3 — Async `config` makes a naive re-fire insufficient

nxvim plugin `config` may be async (`M.load` awaits a promise). By the time an
async config registers its `FileType` autocmd, the event is long gone — and
unlike lazy.nvim we cannot re-fire synchronously right after `load()` returns,
because `load()` returns a *promise*, not a loaded plugin.

More generally: **a handler registered while event E was still in its async tail
never sees E.** That is the defect in its general form, and it is not specific
to the plugin manager — a user's `init.lua` that awaits something and then calls
`nx.on("FileType", ...)` has it too.

---

## What the async event model is today

Exactly one settle-aware path exists:

- `nx._fire` (`crates/nxvim-lua/src/prelude/autocmd.lua:441`) — fire-and-forget.
  A handler's promise is `:catch`ed for error reporting only
  (`track_au_promise`, `autocmd.lua:427`); the editor never waits.
- `nx._fire_gated` (`autocmd.lua:475`) — the only awaited path:
  `gate_id` → `nx._au_gate_done` → server-side `drain_au_gate_done`
  (`lifecycle.rs:1495`).

The gate primitive already has two users — `BufWritePre`
(`fire_buf_write_pre_gated`, `lifecycle.rs:1389`) and the exit chain
(`fire_exit_event_gated`, `lifecycle.rs:1578`) — so the machinery for
"fire, park, resume on settle" exists and is proven. Phase 5 is its third user.
What is missing is policy, not plumbing.

---

## Design

### A. Two event classes; hot-path events are synchronous-only

Classification lives in **one table in Lua dispatch**. Rust names the event and
never needs to know the class, so this requires no cross-crate synchronization.

**Hot-path** (fires per keypress): `CursorMoved`, `CursorMovedI`,
`TextChanged`, `TextChangedI`, `ModeChanged`, `InsertEnter`, `InsertLeave`,
`BufEnter`, `BufLeave`, `WinEnter`, `WinLeave`, `WinScrolled`, `WinResized`.

Hot-path handlers **must be synchronous**. Returning a promise is a hard error
(see *Decisions*). They carry no watermark, no promise tracking, and never gate
anything — so the settle protocol is structurally absent from the input tick
rather than merely cheap there.

**Non-hot-path** (structural, roughly once per buffer): `BufReadPost`,
`BufNewFile`, `FileType`, `BufWinEnter`, `BufAdd`, `BufDelete`, `BufWritePre`,
`BufWritePost`, `VimEnter`, `LspAttach`, `User`, `DirChanged`,
`FileChangedShell`. These may be async, participate in the settle protocol, and
are the only events a chain can gate on.

**This split is what makes chain gating affordable.** Gating is only ever
entered from the rare structural events; the per-keypress events that dominate
`emit_lifecycle_events` can never park it.

### B. The read chain is gated

Neovim's guarantee is that when `BufReadPost` returns, everything it triggered
has completed, so `FileType` fires into a settled world. We reproduce that
explicitly:

```
BufReadPost  →  settle + replay to fixpoint  →  FileType
             →  settle + replay to fixpoint  →  BufEnter
```

`BufEnter` is hot-path, so it never gates *on its own handlers* — but it is
sequenced after the gates ahead of it. Being ordered behind a gate does not make
an event async; its own handlers stay synchronous.

This is what makes async filetype detection in a `BufReadPost` handler
deterministic, rather than relying on the `ft_changed` re-fire
(`lifecycle.rs:654`) to land a diff later.

### C. Watermark + replay to late subscribers

Gating is **not sufficient** for D2/D3, and the distinction matters:

- **Gating** sequences events relative to each other.
- **Replay** re-delivers an event to a handler that registered *during that same
  event's dispatch* — the ft-lazy plugin case, where the gate settles but the
  newly-registered `FileType` handler still never ran for the buffer that woke
  it.

The async analogue of neovim's synchronous guarantee is therefore two-part:
events are ordered, **and** "anyone who shows up during the settle window still
gets the event."

Autocmds already carry a monotonic id (`autocmd_seq`, `autocmd.lua:302`). So:

1. When dispatching a non-hot-path event `(E, buffer B)`, record the current
   `autocmd_seq` as a **watermark**.
2. Collect the handler promises — `track_au_promise` already sees every one.
3. When they all settle, re-dispatch `(E, B)` restricted to
   `au.id > watermark` — only handlers that did not exist at first dispatch.
4. Iterate to a fixpoint (a replayed handler may load another plugin that
   registers more), under an iteration cap.

Why a watermark rather than a plain re-fire: a re-fire re-runs handlers that
already ran and relies on them being idempotent. The watermark is exact, so
**no handler ever sees the same event twice.**

Replay is independently useful for events not in any chain (`User`,
`BufWinEnter`, `LspAttach`), which is why it lands (Phase 3) before gating
(Phase 5) — and why it fixes ft-lazy on ordinary opens well before the chain
work starts.

Events with no buffer (`User`, `DirChanged`) use the same logic keyed on pattern
instead of buffer.

### D. Settle budget — now load-bearing for liveness

The budget applies to the **wait**, not to the handler — a Lua promise cannot be
cancelled. Implemented as
`nx.promise.race({ all_settled(promises), nx.timer(budget) })`.

Once the chain gates, the budget stops being purely diagnostic: **a hung
`BufReadPost` handler must not prevent `FileType`/`BufEnter` from ever firing**,
which would leave the buffer permanently half-initialized. On expiry the chain
advances regardless.

- **On timeout: replay and advance with whatever registered so far.** One slow
  handler must not cost every other subscriber its event, nor wedge the chain.
- **Warn at expiry**, not only on late completion: "handler X exceeded
  {budget}ms; continuing without it". A handler that *never* settles is the
  worse bug and would otherwise emit nothing at all.
- **Warn on eventual settle**: "handler X settled after {n}s". If `autocmd_seq`
  advanced past the watermark in the meantime, run a second replay.
- **Never settles**: remains visible in the expiry warning, plus an
  `nx.autocmd.pending()` introspection listing so a hung handler is inspectable
  rather than inferred.
- **Budget**: global default **500 ms** overridable per-autocmd via
  `{ timeout = ... }`, so a legitimately slow one-time handler (a first LSP
  spawn) does not warn on every open.
- **Warnings must be retrievable, not just displayed.** *(Settled in Phase 3:
  no special routing is needed — Lua `print` / `nx.notify` already reach
  `Editor::echo`, which calls `record_message`, so every warning is in
  `:messages` already. The plan's original `:echomsg` note was unnecessary.)*

Every warning must name the **registration site** (Phase 2). "A `FileType`
handler was slow" is unactionable with N plugins loaded; "`init.lua:47`" is a
fix.

### E. Per-buffer read lifecycle

Fire `BufReadPost`/`BufNewFile`/`FileType` for **every** buffer whose content is
present and which has not yet been announced — not just the current one —
matching neovim's "every buffer that lands in a window" rule.

This lands **before** gating on purpose: once the lifecycle is per-buffer, the
chain is naturally per-buffer state, and the Phase 5 state machine is written
once instead of being built for the current buffer and then generalized to N
concurrent chains.

Blocker to be aware of: `fire_lifecycle` (`lifecycle.rs:1346`) derives the
filetype from `self.editor.buffer().path` — the **current** buffer — so firing
it for a non-current buffer pushes a wrong snapshot into the VM.
`fire_buf_event` (`lifecycle.rs:1313`) is the correctly-shaped helper: it
resolves name and filetype from `buf` itself.

---

## Constraints (from the architecture)

- **The editor must never freeze.** Gating parks a *chain*, never the input
  tick. The user keeps typing while a buffer's chain is in flight; the buffer is
  open, painted and editable, only its setup is incomplete — strictly better
  than today, where the setup never happens at all for background buffers.
- **A gated chain must always terminate.** The budget (Design D) is the
  liveness guarantee, not a nicety.
- **Steady-state cost must be zero on the hot path.** The classification check
  is a table lookup; hot-path events allocate no watermark, track no promises,
  and never enter the state machine.
- **`nxvim-core` stays pure and synchronous.** The core records intent; the
  server owns firing and awaiting; Lua owns dispatch and classification.
- **Tier-1 remote.** Restored buffers in a daemon/wasm session take the off-tick
  load path, so the read lifecycle must fire when the bytes *land*
  (`load_replica_bytes`), not when the open is issued. The existing
  `pending_open` gate (`lifecycle.rs:375`, `lifecycle.rs:753`) is the model to
  follow, and phases 4–5 must be verified in both builds.

---

## Phase 1 — Event classification + hot-path events are synchronous-only ✅

**Goal.** Introduce the hot/non-hot split and make a promise returned from a
hot-path handler a loud error.

**Why first.** Self-contained, and every later phase's cost argument depends on
hot-path events being unable to park anything.

**Scope.** `crates/nxvim-lua/src/prelude/autocmd.lua`.

**Approach.**
- Add `nx._hot_events` — a set of the hot-path event names listed in Design A.
- In `nx._fire`, if the event is hot and a handler returns a promise, raise
  instead of calling `track_au_promise`. The message must carry the escape
  hatch, or it is just an obstacle:

  > `CursorMoved` handlers must be synchronous (registered at `init.lua:47`).
  > Start async work with `nx.schedule` / `nx.on_next_tick` and return nothing.

  (The registration site arrives in Phase 2; until then the message names the
  event and the autocmd id.)
- **Breaking change to audit:** `autocmd.lua:419` currently documents
  async-from-`CursorMoved` as an *intended* pattern and names an LSP request
  from `CursorMoved` as the example. Sweep the prelude and the bundled plugins
  for hot-path handlers that return a promise and drop the `return` — the async
  work still runs, fire-and-forget. Update that comment.

**Tests** (`crates/nxvim-server/tests/autocmds.rs`).
- A `CursorMoved` handler returning a promise raises, and the message names the
  event.
- A `CursorMoved` handler that starts async work without returning it is fine
  and its work still completes.
- A `FileType` handler returning a promise is accepted (non-hot-path).

**Done when.** The suite passes, no bundled plugin returns a promise from a
hot-path handler, and the stale comment is corrected.

---

## Phase 2 — Registration-site capture on every autocmd ✅

**Goal.** Every autocmd record carries the source location where it was
registered.

**Why.** Every warning in Phases 3 and 5 depends on it. Highest
value-per-line in the feature.

**Scope.** `crates/nxvim-lua/src/prelude/autocmd.lua`.

**Approach.**
- In `nx.autocmd.create` (`autocmd.lua:292`; the record is built at
  `autocmd.lua:302`), stash `debug.getinfo(2, "Sl")` →
  `{ src = short_src, line = currentline }` on the record. The idiom already
  exists in the tree at `crates/nxvim-lua/src/prelude/utils.lua:136`.
- Cost is once per **registration**, never per fire.
- Expose it: `nx.autocmd.get` (`autocmd.lua:629`) includes the site, and the
  existing autocmd listing surfaces it.

**Tests.** An autocmd registered at a known line reports that file and line
through `nx.autocmd.get()`.

**Done when.** Sites are captured, exposed, and hot-path errors from Phase 1
name them.

**Follow-ups.** Two, both found re-reviewing the phase:

- **The `nx.autocmd.create` book page went blank.** The site-capture helpers were
  inserted *between* the function's docstring and the function, and
  `book/gen/generate.py`'s `doc_above` takes the `--` block **immediately above**
  the definition — so the whole (long) doc for the primary autocmd API silently
  became `_No documentation comment in the prelude._`. The helpers now sit above
  the docstring. Recorded as rule (4) in CLAUDE.md's prelude-docstring section,
  since nothing catches it but reading the generated page.
- **The `:autocmd` listing did not surface the site** — see Phase 3's follow-ups.

---

## Phase 3 — Watermark + replay to late subscribers, with a settle budget ✅

**Goal.** Implement Design C and D at the level of a *single* event — no chain
sequencing yet.

**Depends on.** Phases 1 and 2.

**Value on its own.** This alone fixes D2/D3 for ordinary opens: an `ft`-lazy
plugin with an async `config` starts working without any chain changes.

**Scope.** `crates/nxvim-lua/src/prelude/autocmd.lua`, plus
`crates/nxvim-lua/src/prelude/promise.lua` only if a combinator is missing
(`all_settled` at `promise.lua:315` and `race` at `promise.lua:343` both exist).

**Approach.**
- `nx._fire` for a non-hot-path event captures the pre-dispatch `autocmd_seq`
  watermark and collects handler promises (in addition to the existing
  `:catch`).
- If no handler returned a pending promise, return exactly as today — no
  behavior change, no extra tick, for the overwhelmingly common case.
- Otherwise arm the settle continuation:
  `race(all_settled(promises), nx.timer(budget))` → replay
  `(E, pattern, buf)` against handlers with `au.id > watermark` only.
- Fixpoint loop with an iteration cap (proposed 8); exceeding it warns and
  stops, naming the event — an unbounded registration loop must fail loud rather
  than spin.
- Budget default 500 ms, per-autocmd override via `opts.timeout`.
- Warnings per Design D, through `:echomsg`, each naming event, buffer and
  registration site.
- `nx.autocmd.pending()` lists in-flight handler promises past their budget.

**Tests** (`crates/nxvim-server/tests/autocmds.rs`).
- A `FileType` handler that asynchronously registers a *second* `FileType`
  handler: the second one runs for the same buffer, exactly once.
- No double-fire: a handler present at first dispatch does not run again on
  replay.
- Budget exceeded → replay still happens with the handlers registered so far,
  and a warning lands in `:messages` naming the site.
- Late settle after the budget → second replay runs and a second warning names
  the elapsed time.
- Fixpoint cap → warns and terminates.
- End-to-end: an `ft = "python"`-lazy plugin with an **async** `config` that
  registers a `FileType python` handler — opening a `.py` file runs that
  handler.

**Done when.** All of the above pass, and a mutation test confirms the
no-double-fire assertion genuinely fails if the watermark filter is removed.

**Follow-up: the watermark had to be one cursor per *fire*, not per round.** Found
re-reviewing the phase. Once a budget expires, a fire has **two** live replay
paths: the timeout replay (which arms further rounds for late subscribers that
are themselves async) and the eventual late-settle replay of the handler that
blew the budget. Each held its own copy of the watermark, advanced it
independently, and so both dispatched the same id range — a handler registered in
between ran **twice**, which is precisely what the watermark exists to prevent
(measured: 2 deliveries). `arm_settle` now threads a shared mutable
`cursor = { hw = … }` through every round descending from one fire, so
"delivered up to" is global to the fire and whichever path reaches a handler
first is the only one that does. Covered by
`a_fires_two_live_replay_paths_still_deliver_at_most_once`.

Phase 2's "the existing autocmd listing surfaces it" was also still outstanding:
`:autocmd` rendered every callback as a bare `<callback>`, which with N handlers
on one event identifies none of them. It now carries the site.

---

## Phase 4 — Read lifecycle decoupled from the current-buffer diff ✅

**Goal.** Fire `BufReadPost`/`BufNewFile`/`FileType` for every buffer that lands
in a window, closing D1.

**Depends on.** Phase 3 (an `ft`-lazy plugin loading async for a restored
background buffer needs replay to deliver its own handler).

**Why before gating.** Once the lifecycle is per-buffer, the Phase 5 state
machine is per-buffer state written once, rather than a current-buffer machine
later generalized to N concurrent chains.

**Scope.** `crates/nxvim-server/src/lifecycle.rs`, and whatever `lib.rs` startup
ordering the restore requires.

**Approach.**
- Extend the every-window walk that already exists for `BufWinEnter`
  (`lifecycle.rs:741`) to also drive the read lifecycle for unannounced buffers,
  keeping neovim's per-buffer order `BufReadPost` → `FileType` → `BufWinEnter`.
- Use `fire_buf_event`-shaped context (name/filetype resolved from `buf`), not
  `fire_lifecycle`, which is current-buffer-coupled (`lifecycle.rs:1346`).
- Preserve the `pending_open` hold (`lifecycle.rs:753`) so a buffer whose bytes
  land later fires once, in order, over the filled buffer — this is also what
  makes the daemon/wasm path work.
- Match neovim's scope rule: buffers that land in a **window** fire; a
  `:badd`-style listed-but-unloaded buffer does not.
- Keep `BufEnter` on the current buffer only (it is hot-path and per-entry).

**Tests** (`crates/nxvim-server/tests/session.rs`, plus `autocmds.rs`).
- Restore a 3-window session; assert `BufReadPost` + `FileType` fire for **all
  three** restored files, in per-buffer order — i.e. the nxvim log matches the
  neovim log captured in *Problem*.
- End-to-end: a lazy plugin with `ft = "python"` and an **async** `config` that
  registers a `FileType python` handler — after restore, that handler has run
  for the restored `.py` buffer without the user focusing its window.
- A buffer already announced is not re-announced on a later window switch.
- Daemon build (`--test daemon_edit`) and `--no-default-features` both covered,
  per the tier-1 rule. *(The `--no-default-features` build compiles — none of the
  chain state is behind the `native` cfg. The daemon half was outstanding until the
  post-Phase-6 review and is now
  `the_gated_read_chain_orders_and_replays_over_the_wire` in `daemon_edit.rs`: a
  remote open creates the buffer empty and the bytes land a tick or more later, so
  the test pins that the async `BufReadPost` handler sees the **fetched** content,
  that `FileType` waits for it to settle rather than firing between `read:start` and
  `read:done`, and that a handler registered during that tail still gets the event.
  Mutation-tested by swapping the stage fires for their ungated twins:
  `read:start,ft:rust,late:rust` — `FileType` jumps the read entirely.)*

**Done when.** Both logs match, the async-lazy end-to-end passes, and the
existing session/autocmd suites are green.

**Landed as.** `announce_displayed_buffers` (`lifecycle.rs`), called just before
the `BufWinEnter` walk so each buffer gets neovim's `BufReadPost` → `FileType` →
`BufWinEnter` in order. Context is resolved from the buffer itself via a new
`fire_buf_lifecycle` (an explicit-pattern `fire_buf_event`, needed because
`FileType`'s `<amatch>` is the filetype, not the name).

Two deliberate divergences from the neovim log, both recorded in the test:

- **`BufEnter` does not fire for the background buffers.** It means "this buffer
  became current", true only of the focused one; it is also hot-path and fired
  per entry by the diff. Neovim fires it only because its session *script*
  really does `wincmd w` into each window in turn — our restore builds the
  layout directly, so there is no entry to report.
- **`BufAdd` still does not fire for restored buffers.** Restore runs inside
  `shada_load`, before `startup_bufs_seeded`, so they are startup baseline.
  Firing it would require treating restored buffers as "new" at seed time, which
  would also fire a spurious `BufAdd` for the startup buffer. Left as a known
  divergence; no handler could have observed it anyway, since restore precedes
  config sourcing.

---

## Phase 5 — Chain gating: the per-buffer read-lifecycle state machine ✅

**Goal.** Implement Design B — `BufReadPost` → settle+replay → `FileType` →
settle+replay → `BufEnter`, per buffer.

**Depends on.** Phases 3 and 4.

**Scope.** `crates/nxvim-server/src/lifecycle.rs`, `crates/nxvim-lua/src/ops.rs`
(if the gate op needs a new variant), `crates/nxvim-lua/src/prelude/autocmd.lua`.

**Approach.**
- Reuse the existing gate primitive — this is its third user after `BufWritePre`
  (`lifecycle.rs:1389`) and the exit chain (`lifecycle.rs:1578`):
  `fire_autocmd_buf_gated` → park under `gate_id` → `drain_au_gate_done`
  (`lifecycle.rs:1495`) resumes.
- Per-buffer chain state: `{ buffer, stage, gate_id }`. A buffer with a chain in
  flight is held out of the announce pass (mirroring the `pending_open` hold at
  `lifecycle.rs:375`) so a keypress mid-chain cannot re-enter it.
- Each stage: fire gated → on settle, run the Phase-3 replay to fixpoint → then
  advance to the next stage. Replay happens *inside* the gate, before advancing,
  so `FileType` genuinely sees a settled world.
- **Synchronous fast path is mandatory.** If a stage's handlers return no
  pending promise, advance inline in the same tick — identical timing to today
  for every config that has no async handlers, which is nearly all of them.
- Budget expiry advances the stage anyway (Design D liveness), with the warning.
- Abandon the chain if the buffer is deleted mid-flight; drop its state.

**Tests** (`crates/nxvim-server/tests/autocmds.rs`).
- An async `BufReadPost` handler that sets `vim.bo.filetype`: `FileType` fires
  **once**, with the handler's filetype, and not with a stale pre-callback one.
- Ordering: an async `BufReadPost` handler completes before any `FileType`
  handler runs.
- `BufEnter` fires after both gates settle.
- No-async config: the whole chain still completes within a single tick (guard
  against a latency regression on the common path).
- A hung `BufReadPost` handler → chain still reaches `FileType` and `BufEnter`
  after the budget, with a warning.
- Buffer deleted mid-chain → no panic, no orphaned gate.
- Both builds, per the tier-1 rule.

**Done when.** Ordering tests pass, the no-async fast path is confirmed
single-tick, and the liveness test proves a hung handler cannot wedge a buffer.

**Landed as.** `ReadStage`/`ReadChain` on `EditHost` (`read_chains` keyed per
buffer, `chain_gates` mapping gate id → buffer), driven by `drive_read_chain`
with `fire_buf_lifecycle_gated` as the per-stage fire. `drain_au_gate_done`
grew a chain branch ahead of the write/exit ones.

`nx._fire_gated` was folded onto the Phase-3 settle protocol rather than kept
separate, so a gated fire also replays to late subscribers and signals its gate
only once that whole fixpoint converges. That is what makes the ordering claim
true: when `FileType` fires, an async `BufReadPost` handler has finished *and*
anything it registered has run. `arm_settle` gained a fire-once `on_done` for
this; a timeout counts as converged, which is the liveness guarantee.

`BufEnter` is deferred onto the chain (`deferred_enter`) only when a chain is
still in flight for that buffer; a chain that completed synchronously is already
out of the map, so the common path fires inline exactly as before. `BufWinEnter`
was later given the same treatment (`deferred_win_enter`) — see *Open questions*.

Two follow-ups landed after the phase, both from re-reviewing it:

- **`BufWinEnter` jumped the chain** (the open question above, answered).
- **A chain outlived its buffer.** The phase's own scope said "abandon the chain
  if the buffer is deleted mid-flight; drop its state", and the buffer-deletion
  cleanup did not. A handler that wipes the buffer it is announcing left the
  `read_chains` entry and its `chain_gates` mapping behind — permanently, if that
  handler never settles, since only the resume path removed them. Now dropped
  with the buffer. Buffer ids are monotonic (`BufferStore::next_id`), so this is
  state hygiene rather than a user-visible bug; the *visible* half — no stage may
  fire over a wiped buffer — is the `buffer_ids` guard in the `Done` arm, which
  `deleting_a_buffer_mid_chain_does_not_panic_or_orphan_the_gate` now asserts
  (without it the wiped buffer gets `BufEnter,BufWinEnter`).

Two more, from a review after Phase 6 landed:

- **A chain also outlived its *read*, and that one broke ordering.** Deletion was
  handled; re-reading was not. `:e!` re-reads a buffer in place, which drops it
  from `announced` and announces it again — potentially from underneath an async
  `BufReadPost` handler still in flight. The new chain simply overwrote the old
  entry, leaving the old chain's gate still mapped to that buffer, so the
  *previous* read's handler settling un-parked the *current* read's chain and drove
  it on: `FileType` fired released by a read that no longer existed, while the read
  that did exist was still running (measured:
  `read1:start,read2:start,read1:done,ft` — `ft` ahead of `read2:done`). The
  chain-start is now the single `begin_read_chain`, which abandons any predecessor
  gate and all, exactly as buffer deletion does — but **carries its deferred tail
  over**. That second half is not symmetry for its own sake: a `BufEnter` /
  `BufWinEnter` parked on the superseded chain records an entry and a first display
  that really happened, and dropping them loses those events outright, because
  nothing re-detects either (the buffer is already current and already in the
  displayed baseline, so neither the `entered` diff nor the `newly_shown` walk sees
  it again). Both halves are covered by
  `re_reading_a_file_mid_chain_abandons_the_previous_chain`; without the carry-over
  the log ends at `ft`, with no `enter,winenter` at all.
- **The gated path swallowed handler rejections.** `nx._fire` attaches
  `track_au_promise` to every non-hot handler promise, so a rejection surfaces named
  for its event; `nx._fire_gated` never did. That was tolerable while the only gated
  events were `BufWritePre` and the exit chain, but folding the read chain onto the
  gated path (this phase) silently moved `BufReadPost`/`BufNewFile`/`FileType` from
  the reporting path to the quiet one — and `all_settled` subscribes with a rejection
  handler of its own, marking the promise handled, so not even the generic
  unhandled-rejection reporter fired. A failing async read handler produced *no
  output at all*. `nx._fire_gated` now tracks like `nx._fire`; the liveness rule is
  unchanged (a rejection still never blocks the gated action). Covered by
  `a_rejecting_async_read_handler_surfaces_its_rejection`.

---

## Phase 6 — Plugin-manager cleanup, docs, example ✅

**Goal.** Remove what replay makes redundant and document the model.

**Scope.** `crates/nxvim-lua/src/prelude/plugins.lua`, book docs,
`examples/async-events/`.

**Approach.**
- ~~Delete the stale re-fire claim at `plugins.lua:442-443`; `ft`/`event`
  triggers keep their naive `load_reporting(name)` and now work by virtue of
  replay.~~ **Wrong, and corrected in Phase 3.** Replay only arms when a handler
  *returns* a pending promise, and `load_reporting` discarded it — so the fire
  saw nothing pending and never replayed. The triggers now return the load
  promise (hot-path events excepted, where returning one raises). That is not
  bespoke re-fire code; it is the trigger participating in the settle protocol,
  which is the contract. The stale comment is already corrected.
- Leave `PluginsLoaded` (`plugins.lua:512`) as-is — it is a useful public
  "everything is ready" hook, not a mechanism this design depends on.
- Book: document the two event classes (which events are hot, why hot handlers
  must be synchronous, the `nx.schedule` escape hatch), the chain's ordering
  guarantee, the replay guarantee, the budget and its warnings, and
  `opts.timeout`. Follow the prelude docstring markdown rules in `CLAUDE.md`.
- `examples/async-events/` — `init.lua` with numbered *type-this / see-that*
  sections: a hot-path handler that correctly defers, an async `BufReadPost`
  handler whose work is visibly complete before `FileType` runs, an async
  `FileType` handler that registers a late subscriber, and a deliberately
  over-budget handler so the warning can be seen in `:messages`. Verify
  end-to-end, but ship **no** Rust test that loads it.

**Done when.** Docs build clean, the example fires end-to-end, and
`cargo test --workspace` is green.

**Landed as.** `arm_lazy`'s comment now describes what each trigger kind actually
does (`cmd` re-dispatches, `keys` replays through the typeahead, `event`/`ft`
return the load promise). Docs went into `docs/autocmd-events.md` — the source
the book page is generated from — as two new sections plus corrected
`BufReadPost`/`FileType`/`BufEnter` rows.

**The example earned its keep.** `examples/async-events` immediately exposed a
real defect: the late-settle warning printed `handler () settled 402ms ...` with
an *empty* site. `unsettled_sites` filters on "not done", and by completion time
every handler is done — so the completion warning, the one place the file:line
matters most, named nobody. Fixed by capturing the sites when the budget blows
and reusing them, plus a fallback to naming every handler of the fire: the
budget timer and the handlers can become ready in the same loop turn and `race`
may still pick the timeout, leaving the precise filter empty. Both are now
asserted in `a_handler_that_settles_late_warns_with_its_elapsed_time`.

---

## Decisions

- **The chain is gated.** The hot/non-hot split exists precisely to make that
  affordable: gating is only ever entered from rare structural events, never
  from the per-keypress events that dominate `emit_lifecycle_events`.
- **Gating and replay are both required.** Gating orders events; replay
  re-delivers an event to handlers that registered during its own dispatch.
  Neither subsumes the other, and D2/D3 need replay specifically.
- **Hot-path handlers returning a promise: hard error, not a warning.** The fix
  is mechanical (drop the `return`; the async work still runs) and "fail loud"
  is a project principle. Cost: it breaks the pattern `autocmd.lua:419`
  currently documents, which Phase 1 audits and updates.
- **The restore gap ships in this plan, not separately.** It is the thing that
  proves the mechanism end-to-end, it shares the acceptance test, and gating
  wants the per-buffer lifecycle underneath it anyway.

## Open questions

- ~~Budget default~~ **Settled: 500 ms.**
- Should the budget be per-event-class (e.g. more generous for `VimEnter`) or
  strictly global-plus-per-autocmd? Starting global; revisit if the default
  proves wrong for one event.
- ~~Does `BufWinEnter` belong inside the gated chain (after `FileType`) or outside
  it?~~ **Settled: sequenced behind the chain, but not a gated stage of it.**
  Shipped after Phase 6, because Phase 5 left it firing from the window walk in
  the same pass that parked the chain — so an async `BufReadPost` handler put it
  *second* (`read:start, bufwinenter, read:done, filetype, bufenter`), ahead of
  the events the chain exists to order. It now defers onto the chain exactly as
  `BufEnter` does (`ReadChain::deferred_win_enter`) and fires last, matching vim.
  It is deliberately **not** a gated stage: nothing follows it, so gating would
  buy only replay-to-late-subscribers, which every non-hot event already gets
  from `nx._fire`'s settle protocol. Covered by
  `bufwinenter_is_sequenced_after_the_chains_gates_too`.
- `nx.autocmd.pending()` may want to be folded into a future `:checkhealth`
  rather than standing alone.
