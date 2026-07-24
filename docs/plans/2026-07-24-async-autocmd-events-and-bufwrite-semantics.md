# Async-aware autocmd events + neovim `BufWritePre`/`BufWritePost` semantics

Date: 2026-07-24

## Problem

nxvim fires `BufWritePre` **after** the bytes are already on disk. In `ex_write`
(`core/editor/ex.rs`) the order is `buffer.write()` (disk IO) → `record_write()`,
and the server later fires `BufWritePre` **and** `BufWritePost` together from the
`write_events` queue (`fire_buf_write`, `lifecycle.rs`). So `BufWritePre` is a
post-facto *notification*, not vim's pre-write interception point — a handler that
mutates the buffer (format-on-save, trim trailing whitespace, `insert_final_newline`)
cannot change what was saved.

Neovim's contract:

1. `BufWritePre` fires **before** the buffer is serialized. Handlers may mutate the
   buffer; those mutations are what gets written.
2. The bytes are written.
3. `BufWritePost` fires after.

Two requirements beyond a plain reorder (from the requester):

- **All autocmd events must be async-aware** — a handler may return a promise, and
  the firing path must not drop it (today `nx._fire` discards every handler return
  except the one `*Cmd` truthy-claim path).
- **`BufWritePre` must let its handlers settle before writing** — including *async*
  handlers (e.g. an async LSP format). The write waits for every `BufWritePre`
  handler promise to settle, then serializes.

## Constraints (from the architecture)

- The server is single-threaded and **must never block a tick**. "Wait for the
  handlers to settle" cannot be a synchronous wait; it must be a continuation the
  server runs when a settle callback fires — the exact discipline `settle_lsp_promise`
  / `on_loop_event` → `FsResult` already follow.
- `nxvim-core` stays pure/synchronous. The core can *record intent* and *commit a
  write when told to*, but it cannot re-enter Lua. The server owns firing + awaiting.
- Async settlements land through `on_loop_events` → `on_loop_event`
  (`run_callback` + `apply_lua_effects`) and end with `settle_events(true)` →
  `run_pending`. So a deferred write-commit + `BufWritePost` can be driven from the
  `run_pending` fixpoint, whether the trigger was synchronous (the `:w` keystroke) or
  an async settle a tick later.
- Tier-1 remote rule: the off-tick save path (`enqueue_save` → daemon/OPFS →
  `finalize_save`) must get identical semantics — `BufWritePre` before `enqueue_save`,
  `BufWritePost` on the ack. Verify native **and** `--no-default-features` (wasm).

## Existing machinery to reuse (do not build parallel mechanisms)

- **Callback-id settle bridge**: `nx._next_cb_id()` / `nx._cb_fns[id]` / `nx._run_cb`
  (`prelude/runtime.lua`), settled from Rust by `run_callback(id, keep, args)`
  (`runtime.rs`). Template: `settle_lsp_promise` (`lsp/request.rs`).
- **Promise combinators**: `nx.promise.all_settled` (`prelude/promise.lua`) is exactly
  "resolve once every input has settled, never reject" — the right primitive for
  "await all handlers".
- **`is_promise` convention**: `getmetatable(v) == Promise` inside promise.lua, or the
  duck-typed `type(v) == "table" and type(v.next) == "function"` used by
  `cmdline_complete.lua` / `complete.lua`. Expose one canonical `nx._is_promise`.
- **`*Cmd` claim plumbing** (`nx._fire_read_cmd` + `fire_autocmd_cmd` returning a value
  to Rust) is the shape to generalize — a handler return that Rust consults.
- **The `run_pending` fixpoint** (`effects.rs`) already drains `write_events`,
  `pending_checktime`, scheduled callbacks, etc., and its break-condition gates on
  those queues. New per-tick queues (pre-write intents, settled gates) plug into the
  same loop and its break check.

## Write entrypoints to cover (all of these must get pre→bytes→post ordering)

- `:w` / `:write` → `ex_write(args, bang, None)` (ex.rs:895)
- `:wq` / `:x` / `:xit` / `:exit` → `ex_write(args, bang, Some(bang))` (ex.rs:901)
- `:wa` / `:wall` → `ex_write_all(bang)` (ex.rs:913)
- `:wqa` / `:xa` / `:xall` → `ex_write_quit_all(bang)` (ex.rs:916)
- off-tick ack → `finalize_save` (buffers.rs) — already fires *after* bytes; only the
  `BufWritePre`-before-`enqueue_save` half is missing.

---

## Phase 1 — Two-phase write + **synchronous** `BufWritePre` before the bytes

Goal: fix the ordering for synchronous handlers across **every** write path. No
promises yet — this is the structural change, independently valuable and testable.

### Core (`nxvim-core`)

1. Add a `PreWrite` intent + queue on `Editor`:
   ```rust
   pub struct PreWrite { pub buffer: BufferId, pub path: Option<PathBuf>,
                         pub bang: bool, pub then_quit: Option<bool> }
   // Editor: pending_pre_writes: Vec<PreWrite>
   pub fn take_pending_pre_writes(&mut self) -> Vec<PreWrite>
   pub fn has_pending_pre_writes(&self) -> bool
   ```
2. `ex_write`: keep the resolve-target + disk-changed guard (a refused write fires no
   events, matching vim). Instead of writing, push a `PreWrite`. Do **not** run
   `then_quit` here anymore — it moves to commit.
3. `ex_write_all`: keep the conflict scan (skip/`switch_buffer`+warn on a changed
   file). For each buffer that *would* be written, push a `PreWrite` instead of
   writing; keep the "N buffer(s) written" echo deferred to commit-count, or echo it
   after enqueuing the intents (a later refinement — Phase 1 may keep the summary echo
   approximate and assert per-buffer events instead).
4. New `commit_pre_write(pw: PreWrite)` — the current write body, buffer-targeted:
   sync path does `buffer_mut_of(pw.buffer).write(pw.path, fs)` → `mark_undo_saved`
   → echo → `record_write` (now **post-only**) → `then_quit` ⇒ `ex_quit`; off-tick
   path does `enqueue_save_of(pw.buffer, target, pw.then_quit)`.
5. `finalize_save` keeps calling `record_write` (post-only). Off-tick BufWritePre now
   fires from the pre-write drain before `enqueue_save`, so `finalize_save` must
   **not** re-fire pre.

### Server (`nxvim-server`)

6. Split `fire_buf_write` → **`fire_buf_write_post`** (fires `BufWritePost` only).
   `drain_write_events` calls it (post-only).
7. New `drain_pre_writes`: for each `take_pending_pre_writes`, refresh snapshot+mirror
   (as `fire_buf_write` does), fire `BufWritePre` synchronously
   (`fire_autocmd_buf("BufWritePre", …)` + `apply_lua_effects` so a handler's
   `vim.cmd` mutation lands), then `editor.commit_pre_write(pw)`.
8. Call `drain_pre_writes` inside the `run_pending` fixpoint **before**
   `drain_write_events`; add `has_pending_pre_writes()` to the loop's break condition
   and the `MAX_ROUNDS` drain-clear.
9. Re-entrancy guard: a `BufWritePre` handler that itself issues `:w` on the same
   buffer must not recurse forever — set a "committing write for buf B" flag that
   suppresses a nested `BufWritePre` for B (vim writes with implicit `noautocmd`).
   `MAX_ROUNDS` is the backstop.

### Tests (`crates/nxvim-server/tests/autocmds.rs`, or a new `bufwrite.rs`)

- **Sync mutation lands**: `BufWritePre` handler runs
  `vim.cmd([[%s/\s\+$//e]])` (or appends a line); after `:w`, the on-disk bytes reflect
  the mutation. Mutation-test: remove the reorder ⇒ disk lacks the change.
- Order `pre,post` still holds (existing test).
- `BufWritePre` sees the buffer **modified**; `BufWritePost` sees it clean.
- `:wq` still quits after the (now-deferred) write; a failed write leaves it modified.
- `:wall` fires `BufWritePre`/`BufWritePost` per written buffer.
- Off-tick (`--test daemon_save`): `BufWritePre` fires before the enqueue, `BufWritePost`
  on the ack; mutation in `BufWritePre` is what the daemon receives.

Commit, pause for review.

---

## Phase 2 — Async gate: `BufWritePre` handlers may return promises; the write awaits

Goal: the write waits for every `BufWritePre` handler promise to settle, across ticks,
before committing.

### Lua (`prelude/autocmd.lua`, `runtime.lua`)

1. Expose `nx._is_promise(v)`.
2. Generalize `nx._fire` to **capture** each handler's return value.
3. Add `nx._fire_gated(event, pattern, buf, file, gate_id, data)`: run handlers like
   `nx._fire`, collecting promise returns. If none pending ⇒ return `true`
   (settled synchronously — server commits inline, preserving Phase 1 timing). If any
   ⇒ `nx.promise.all_settled(promises):next(function() nx._au_gate_done(gate_id) end)`
   and return `false`.
4. `nx._au_gate_done(id)` bridge → pushes `id` to a Rust-drained `Shared` queue.

### Rust

5. `fire_autocmd_buf_gated(event, pattern, buf, file, gate_id) -> mlua::Result<bool>`
   (calls `nx._fire_gated`).
6. `Shared.au_gate_done: Vec<u64>` + `nx._au_gate_done` install binding; drain in
   `run_pending`.
7. `EditHost`: `next_gate_id` counter + `pending_gated_writes: HashMap<u64, PreWrite>`.
8. `drain_pre_writes` (from Phase 1) now: fire *gated*. `Ok(true)` ⇒ commit inline
   (Phase 1 path). `Ok(false)` ⇒ stash `PreWrite` under `gate_id`; commit later when
   the gate-done drain fires. `Err` ⇒ report + commit (a throwing `BufWritePre` doesn't
   block the write, as in vim).
9. Gate-done drain in `run_pending`: for each settled `gate_id`, pop
   `pending_gated_writes` and `commit_pre_write`. `settle_events` after an async op
   already runs `run_pending`, so an async settle drives the commit + `BufWritePost`.

### Tests

- Async `BufWritePre`: handler returns `nx.promise.delay(ms):next(mutate)`; after `:w`
  the disk bytes reflect the mutation (proves the write waited). Mutation-test the wait.
- Async handler that mutates via an awaited `nx.fs`/timer round-trip.
- A rejecting `BufWritePre` promise still writes (unhandled-rejection surfaces, write
  proceeds).
- Two `BufWritePre` handlers (one sync, one async) both settle before the write.

Commit, pause for review.

---

## Phase 3 — Docs + example (shipped); `:wall`/`:wqa` deferred

**Shipped:** `examples/format-on-save/` (sync trim-on-save + async `FIXME`→`TODO`
formatter via a `BufWritePre` promise; verified end-to-end with a throwaway harness
test, then removed per the examples convention). `docs/autocmd-events.md` documents the
async-handler contract and the awaited `BufWritePre`.

**Deferred — `:wall`/`:wqa` pre-before-bytes (a genuinely separate, larger piece).**
The blocker is *not* the quit-gating (that could ride the existing `PendingQuitAll`
seq-gate). It is that a *mutating* `BufWritePre` handler (`vim.cmd` `%s`, `nx.lsp.buf.format`)
operates on the **current** buffer, so firing `BufWritePre` for a *non-current* `:wall`
buffer would target the wrong buffer. Doing it correctly needs neovim's `aucmd_prepbuf`:
temporarily make each written buffer current (saving/restoring the window's buffer **and**
the editor-global cursor) without firing spurious `BufEnter`/`BufLeave`. `set_cur_buffer`
swaps the window's buffer but not the cursor, and `switch_buffer` fires the full autocmd
chain — neither is the lightweight swap this needs. An *ordering-only* conversion (fire
before bytes without making the buffer current) would be **worse** than today: a mutating
handler would corrupt the current buffer. So `:wall`/`:wqa` keep firing `BufWritePre`
after the bytes (notifications work; buffer-mutating format-on-save via `:wall` does not) —
tracked as the remaining gap. The overwhelmingly common trigger, `:w`, is fully correct.

**Found along the way (separate bug, not fixed here):** nxvim's vim-regex engine
mishandles `\+` (one-or-more) anchored at `$` — `%s/\s\+$//` does not trim trailing
whitespace, while `%s/\s*$//`, `%s/[ ]*$//`, and `%s/  *$//` all work. The example uses
the working `\s*$` form. Worth a dedicated fix in `nxvim-regex`.

### (Original Phase 3 plan, for reference)

1. Make the general (non-gating) fire path async-aware: a handler that returns a
   promise from `CursorMoved`, `BufEnter`, `FileType`, … has it **tracked** (a `:catch`
   so a rejection reports via the normal unhandled-rejection path) but the editor does
   **not** block — fire-and-forget async. Route `nx._fire` and the `fire_and_drain`
   sites through the promise-aware `nx._fire`.
2. Extend the gate to `:wall`/`:wqa` async (each buffer's `BufWritePre` gates its own
   write; `:wqa` waits for all writes before quitting — reuse the `PendingQuitAll`
   seq-gate shape).
3. Book: document the async-autocmd contract on the autocmd API page (handlers may
   return a promise; `BufWritePre` awaits). Backtick/fence per the book rules.
4. `examples/format-on-save/`: `init.lua` (numbered sections — sync trim-on-save via
   `BufWritePre` + `%s`; async format via `nx.lsp.buf.format()` returning its promise)
   + `sample.txt`. Verify end-to-end, throwaway (no committed example-loading test).
5. Update the memory `editorconfig-builtin-and-bufwritepre-gotcha` — the gotcha is
   fixed; `trim_trailing_whitespace` / `insert_final_newline` are now reachable.

## Out of scope (note, don't silently drop)

- `BufWriteCmd` (a handler *claiming* the whole write) — a natural follow-on to the
  async-claim generalization, but not required here.
- `textwidth`/`max_line_length` editorconfig key (no `textwidth` option exists yet).
