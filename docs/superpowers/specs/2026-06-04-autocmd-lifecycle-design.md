# Autocmd lifecycle — design & phased implementation plan

**Date:** 2026-06-04
**Status:** Planned — foundation work, independent of any feature branch. Unblocks
LSP Phase 7 (`vim.lsp.*`), format-on-save, ftplugin, and buffer-local keymaps.

This document is both the design for nxvim's autocommand event lifecycle **and** a
phase-by-phase implementation plan. Each phase is written to be handed off to a
fresh context window: prerequisites, the exact files it touches, the surface it
adds, the tests that prove it, and a hard "done when" gate. Read the *Design* half
first, then execute the phases in order.

## Goal

nxvim already has the **spine** of an autocmd system — `vim.api.nvim_create_autocmd`
registers callbacks (`crates/nxvim-lua/src/prelude.lua`), `vim._fire(event, pattern)`
dispatches them, and `LuaRuntime::fire_autocmd` drives it from Rust
(`crates/nxvim-lua/src/lib.rs`). It is proven end-to-end: `:colorscheme` works by
firing `ColorScheme` (`crates/nxvim-server/src/lib.rs`).

But `ColorScheme` is the **only** event the editor ever emits, autocmd callbacks
get **no buffer context**, and augroups don't **clear**. Real configs and the
next wave of features need a real event lifecycle:

- **`FileType`** — the hook ftplugins and `vim.lsp.enable` attach to.
- **`BufReadPost` / `BufEnter`** — per-buffer setup, buffer-local state.
- **`InsertEnter`** — mode-driven behavior.
- **`BufWritePre`** — the seam format-on-save will hook (write is core-owned and
  synchronous, so this is also an architectural seam, not just an event).

This phase delivers those five events, buffer-aware callback args, and complete
augroup/registration semantics — the foundation everything else builds on.

## Design

### How autocmds work today

- **Registration** (`prelude.lua`): `nvim_create_autocmd(event, opts)` appends
  `{id, event, opts}` to `vim._autocmds`; `nvim_create_augroup(name, opts)` stores
  a sequence id in `vim._augroups` and **ignores `opts`** (no `clear`).
- **Dispatch** (`prelude.lua`): `vim._fire(event, pattern)` linearly scans
  `vim._autocmds`, matches on event + `opts.pattern`, and invokes
  `opts.callback({id, event, match, file})` or runs `opts.command`. **No `buf`.**
- **Emission** (`crates/nxvim-server/src/lib.rs`): `LuaRuntime::fire_autocmd`
  calls `vim._fire`. The **only** caller is `set_colorscheme`.

### The model this phase establishes

**1. The server emits buffer/mode events centrally, by diffing settled editor
state.** `nxvim-core` stays pure (no Lua, no event types) — so the server, after
each input/command has settled (the redraw path), compares the editor's current
state against what it last announced and fires the events that the transition
implies. One chokepoint, no `fire` calls scattered through core. New `Server`
tracking fields: `last_buffer_id`, `last_mode`, and an `announced: HashSet<BufferId>`
(buffers that have already had `BufReadPost`/`FileType`).

Ordering on first opening a file mirrors neovim closely enough:
`BufReadPost` → `FileType` → `BufEnter`. A plain buffer **switch** (no read) fires
only `BufEnter`. `FileType` fires **once** per buffer (filetype is derived from the
path via `filetype_of`); `BufEnter` fires on **every** entry.

**2. `BufWritePre` is the one event the core write path must cooperate with.**
`:w` is core-owned and synchronous (`Editor::ex_write` → `Buffer::write`), and
core cannot call Lua. So the **server intercepts** `:w`/`:write`/`:wq`/`:x` before
delegating to core: fire `BufWritePre` (callbacks may edit the buffer), drain Lua
effects, *then* let core write. This is deliberately the same seam format-on-save
will later use to turn an async format into a pre-write step.

**3. Buffer context via a current-buffer snapshot.** Callbacks need to know *which*
buffer fired (`args.buf`) and resolve it (`vim.api.nvim_buf_get_name(0)`,
`vim.fn.expand('%:p:h')`). Until Lua has a real buffer registry, the server pushes
a small snapshot — `vim._cur_buf = {bufnr, name}` — into the VM immediately before
firing, and `nvim_buf_get_name`/`expand('%')` read it. `vim._fire` gains optional
`buf`/`file` params so `args` carries the real bufnr and path. Existing
`ColorScheme` callers pass nothing and are unaffected.

### Key decisions

- **D1 — Central server-side emission, not core hooks.** Keeps `nxvim-core` pure
  and gives one place to reason about event ordering. Buffer/mode events are a
  function of *observable settled state*, so a post-settle diff is sufficient and
  avoids threading an event bus through the synchronous core. *Exception:*
  `BufWritePre` (a true pre-action hook) is emitted by intercepting the write
  command in the server.
- **D2 — Snapshot for buffer context.** A `vim._cur_buf` snapshot backs
  `nvim_buf_get_name(0)`/`expand('%')` synchronously during dispatch. No async
  window exists (the core is single-message-at-a-time; `vim.schedule` runs inline),
  so the snapshot can't go stale mid-callback. A real per-bufnr registry is a later
  refinement.
- **D3 — Fire-once vs fire-every.** `BufReadPost`/`FileType` are gated by the
  `announced` set; `BufEnter`/`InsertEnter` fire on every matching transition.
- **D4 — augroup `clear`.** Each autocmd records its `group`; `nvim_create_augroup(name,
  {clear=true})` removes that group's autocmds, so re-sourcing a config doesn't
  double-register. `nvim_create_autocmd` accepts `opts.group`.

### Files

- `crates/nxvim-lua/src/prelude.lua` — `vim._fire` args; augroup `clear` + per-autocmd
  `group`; `vim._cur_buf` snapshot; `nvim_exec_autocmds`; `nvim_del_autocmd`.
- `crates/nxvim-lua/src/lib.rs` — `fire_autocmd` gains buffer context
  (`fire_autocmd_buf`/`set_buf_snapshot`); expose `vim.api.nvim_buf_get_name` and the
  minimal `vim.fn.expand('%'...)` backed by the snapshot.
- `crates/nxvim-server/src/lib.rs` — the central `emit_lifecycle_events()` step
  (state diff: `last_buffer_id`, `last_mode`, `announced`) run after dispatch
  settles; the `:w` intercept for `BufWritePre`; new `Server` fields.
- Tests in `crates/nxvim/tests/` (new `autocmds.rs`) — black-box via `nvim_input` /
  RPC, asserting on observable effects (a callback that runs `:` commands or writes
  a marker), per the project's no-unit-test rule.

---

## Phase 1 — Registration & dispatch completeness (pure bridge)

**Goal / value.** Make the autocmd *substrate* complete and correct before the
editor emits anything new: buffer-aware callback args, augroup `clear`, manual
firing. Independently testable with **zero** editor lifecycle wiring, via
`vim.api.nvim_exec_autocmds`.

**Prerequisites.** None.

**Scope (in).**
- `vim._fire(event, pattern, buf, file)` → callback `args = {id, event, match, buf, file}`.
- `vim._cur_buf = {bufnr, name}` snapshot + `vim._set_cur_buf`; expose
  `vim.api.nvim_buf_get_name(bufnr)` (0 / snapshot bufnr → snapshot name) and a
  minimal `vim.fn.expand` for `%`, `%:p`, `%:h`, `%:t`.
- `nvim_create_augroup(name, {clear=true})` clears the group's autocmds; each
  autocmd stores `opts.group`; `nvim_create_autocmd` honors `opts.group` and
  `opts.buffer` (buffer-local match).
- `vim.api.nvim_exec_autocmds(event, opts)` — manual firing (drives the tests).
- `vim.api.nvim_del_autocmd(id)`.

**Scope (out).** Any editor-emitted events (Phases 2–3). A real per-bufnr buffer
registry (snapshot only).

**Tests** (`crates/nxvim/tests/autocmds.rs`).
- A callback registered for a custom event runs on `nvim_exec_autocmds` and sees
  the right `args.buf`/`args.match` (assert via a command the callback runs).
- `nvim_create_augroup(name, {clear=true})` re-run drops the prior callback (no
  double-fire).
- `nvim_del_autocmd(id)` stops a callback firing.

**Done when.** The above pass; `ColorScheme` still works (back-compat); gates green.

---

## Phase 2 — Buffer lifecycle events (`BufReadPost`, `FileType`, `BufEnter`)

**Goal / value.** The editor emits the per-buffer events. This is the phase that
actually unblocks `vim.lsp.enable` and ftplugins: open a file → `FileType` fires
with buffer context.

**Prerequisites.** Phase 1.

**Scope (in).**
- `Server` tracking: `last_buffer_id`, `announced: HashSet<BufferId>`.
- `emit_lifecycle_events()` run after each input/command settles (in the
  `redraw` path, before `sync_lsp`/`sync_syntax`): if the current buffer left the
  `announced` set, snapshot it and fire `BufReadPost` then `FileType`
  (pattern = filetype from `filetype_of`, only if detected), mark announced; if the
  current buffer differs from `last_buffer_id`, fire `BufEnter`. Each fire pushes
  the buffer snapshot first and drains Lua effects after.
- Covers startup (initial file arg), `:edit`, and buffer switches
  (`nvim_set_current_buf`, `:bnext`, `:b`, `<C-^>`).

**Scope (out).** `InsertEnter`, `BufWritePre` (Phase 3).

**Tests.**
- Opening a `.rs` file fires `FileType` with `match == "rust"` and `args.file`
  the path (assert via a callback that records into a buffer / runs a command).
- Switching buffers fires `BufEnter` for the new buffer; `FileType` does **not**
  re-fire for an already-announced buffer.
- A `BufReadPost` callback can read `vim.api.nvim_buf_get_name(0)` and get the path.

**Done when.** The above pass; ordering is `BufReadPost`→`FileType`→`BufEnter` on
first open; gates green.

---

## Phase 3 — Mode & write events (`InsertEnter`, `BufWritePre`)

**Goal / value.** The remaining two: a mode-transition event and the pre-write
seam. `BufWritePre` is the structural one — it establishes how the synchronous,
core-owned write cooperates with Lua, which format-on-save later reuses.

**Prerequisites.** Phase 2.

**Scope (in).**
- `InsertEnter`: `Server` tracks `last_mode`; `emit_lifecycle_events()` fires
  `InsertEnter` when the mode transitions into `Mode::Insert` (covers `i/I/a/A/o/O/C`
  without touching the multiple core insert chokepoints — the diff sees the result).
- `BufWritePre`: the server intercepts `:w`/`:write`/`:wq`/`:x`/`:wa` before
  delegating to `Editor::ex_write`; fires `BufWritePre` (snapshot pushed), drains
  Lua effects (callbacks may edit the buffer), then runs the core write.

**Scope (out).** `BufWritePost`, `InsertLeave`, `TextChanged`, `BufWinEnter`, etc.
— add as consumers appear. Format-on-save itself (separate design; this only lays
the `BufWritePre` seam).

**Tests.**
- Entering insert via `i` (and via `o`) fires `InsertEnter` exactly once per entry.
- `:w` fires `BufWritePre` before the file is written; a `BufWritePre` callback that
  edits the buffer is reflected in the **written file on disk** (proves pre-write
  ordering and the edit-then-write seam).

**Done when.** The above pass; `:w` semantics otherwise unchanged when no
`BufWritePre` autocmd is registered; gates green.

---

## Downstream (not in this doc)

- **LSP Phase 7** (`docs/superpowers/specs/2026-06-02-lsp-support-design.md`) —
  `vim.lsp.enable` installs a `FileType` autocmd; depends on Phase 2.
- **Format-on-save** — reuses the Phase 3 `BufWritePre` seam.
- **Buffer-local keymaps / ftplugin** — depend on Phase 2 + buffer context.
