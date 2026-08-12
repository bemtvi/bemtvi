# Autocmd lifecycle — design & phased implementation plan

**Date:** 2026-06-04
**Status:** Implemented (Phases 1–3) — foundation work, independent of any feature
branch. Unblocks LSP Phase 7 (`vim.lsp.*`), ftplugin, and buffer-local keymaps.
The `BufWritePre` write seam (which format-on-save hooks) is split into its own
design —
[`2026-06-04-bufwritepre-write-seam-design.md`](2026-06-04-bufwritepre-write-seam-design.md).

All three phases below are landed and covered by `crates/bemtvi-server/tests/autocmds.rs`:
the registration/dispatch bridge (Phase 1), the `BufReadPost`/`FileType`/`BufEnter`
buffer lifecycle events (Phase 2), and `InsertEnter` (Phase 3).

This document is both the design for bemtvi's autocommand event lifecycle **and** a
phase-by-phase implementation plan. Each phase is written to be handed off to a
fresh context window: prerequisites, the exact files it touches, the surface it
adds, the tests that prove it, and a hard "done when" gate. Read the *Design* half
first, then execute the phases in order.

## Goal

bemtvi already has the **spine** of an autocmd system — `vim.api.nvim_create_autocmd`
registers callbacks (`crates/bemtvi-lua/src/prelude.lua`), `btv._fire(event, pattern)`
dispatches them, and `LuaRuntime::fire_autocmd` drives it from Rust
(`crates/bemtvi-lua/src/lib.rs`). It is proven end-to-end: `:colorscheme` works by
firing `ColorScheme` (`crates/bemtvi-server/src/lib.rs`).

But `ColorScheme` is the **only** event the editor ever emits, autocmd callbacks
get **no buffer context**, and augroups don't **clear**. Real configs and the
next wave of features need a real event lifecycle:

- **`FileType`** — the hook ftplugins and `vim.lsp.enable` attach to.
- **`BufReadPost` / `BufEnter`** — per-buffer setup, buffer-local state.
- **`InsertEnter`** — mode-driven behavior.

This phase delivers those four events, buffer-aware callback args, and complete
augroup/registration semantics — the foundation everything else builds on.

`BufWritePre` is deliberately **not** here. It is a true *pre-action* hook on a
core-owned, synchronous write, not a function of observable settled state, so it
needs a different mechanism (core-side write deferral) than the state-diff this doc
establishes. It is its own design:
[`2026-06-04-bufwritepre-write-seam-design.md`](2026-06-04-bufwritepre-write-seam-design.md).

## Design

### How autocmds work today

- **Registration** (`prelude.lua`): `nvim_create_autocmd(event, opts)` appends
  `{id, event, opts}` to `btv._autocmds`; `nvim_create_augroup(name, opts)` stores
  a sequence id in `btv._augroups` and **ignores `opts`** (no `clear`).
- **Dispatch** (`prelude.lua`): `btv._fire(event, pattern)` linearly scans
  `btv._autocmds`, matches on event + `opts.pattern`, and invokes
  `opts.callback({id, event, match, file})` or runs `opts.command`. **No `buf`.**
- **Emission** (`crates/bemtvi-server/src/lib.rs`): `LuaRuntime::fire_autocmd`
  calls `btv._fire`. The **only** caller is `set_colorscheme`.

### The model this phase establishes

**1. The server emits buffer/mode events centrally, by diffing editor state after
each applied input.** `bemtvi-core` stays pure (no Lua, no event types) — so the
server compares the editor's current state against what it last announced and fires
the events the transition implies. The diff runs **per applied input** — after each
`Server::input` key (`editor.input(key)`), and after the `nvim_command` and
`nvim_set_current_buf` dispatch arms — not once per settled message. (Per-key is
what lets a single batched `nvim_input("o…<Esc>")` still fire `InsertEnter` on the
`o`: a once-per-message diff would see only the Normal end-state and miss it.) The
diff is a cheap no-op for the vast majority of keys that change neither buffer nor
mode. A one-time **startup seed** runs after `source_init` (the initial buffer and
the config's autocmds both exist by then) to fire the first buffer's events.

New `Server` tracking fields: `last_buffer_id`, `last_mode`, and an
`announced: HashSet<BufferId>` (buffers that have already had `BufReadPost`/`FileType`).

Ordering on first opening a *file* mirrors neovim closely enough:
`BufReadPost` → `FileType` → `BufEnter`. `BufReadPost` and `FileType` fire **once**
per buffer and **only for file-backed buffers** — a fresh `:enew`/`[No Name]` (and
the bare-`bemtvi` startup buffer) was never read from a file, so it fires only
`BufEnter`. A plain buffer **switch** (no read) fires only `BufEnter`. `FileType`'s
pattern is the filetype derived from the path via `filetype_of` (skipped when it
detects nothing); `BufEnter` fires on **every** entry.

**2. Buffer context via a current-buffer snapshot.** Callbacks need to know *which*
buffer fired (`args.buf`) and resolve it (`vim.api.nvim_buf_get_name(0)`,
`vim.fn.expand('%:p:h')`). Until Lua has a real buffer registry, the server pushes
a small snapshot — `btv._cur_buf = {bufnr, name}` — into the VM immediately before
firing, and `nvim_buf_get_name`/`expand('%')` read it. `btv._fire` gains optional
`buf`/`file` params so `args` carries the real bufnr and path. Existing
`ColorScheme` callers pass nothing and are unaffected.

### Key decisions

- **D1 — Central server-side emission, not core hooks.** Keeps `bemtvi-core` pure
  and gives one place to reason about event ordering. Buffer/mode events are a
  function of *observable editor state*, so the server diffs that state after each
  applied input (model §1) instead of threading an event bus through the synchronous
  core. The diff runs per key/command — not once per settled message — so a
  within-batch transition (enter *and* leave insert inside one `nvim_input`) isn't
  masked; this is the same diff mechanism, only at a finer cadence, so it stays a
  design decision here rather than a separate doc. `BufWritePre` is **not** modeled
  this way: a pre-action write hook can't be reconstructed from after-the-fact state,
  so it gets its own core-deferral design (the write-seam doc).
- **D2 — Snapshot for buffer context.** A `btv._cur_buf` snapshot backs
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

- `crates/bemtvi-lua/src/prelude.lua` — `btv._fire` args; augroup `clear` + per-autocmd
  `group`; `btv._cur_buf` snapshot; `nvim_exec_autocmds`; `nvim_del_autocmd`.
- `crates/bemtvi-lua/src/lib.rs` — `fire_autocmd` gains buffer context
  (`fire_autocmd_buf`/`set_buf_snapshot`); add the **Lua** binding
  `vim.api.nvim_buf_get_name` and the minimal `vim.fn.expand('%'...)`, both backed by
  the snapshot. (Note: a `nvim_buf_get_name` *RPC* method already exists on the server
  — `lib.rs:276`, core-backed — and is separate; the Lua binding is snapshot-backed as
  an interim until a real per-bufnr registry exists.)
- `crates/bemtvi-server/src/lib.rs` — the `emit_lifecycle_events()` diff step
  (`last_buffer_id`, `last_mode`, `announced`), called after each applied input — the
  per-key loop in `Server::input` (`lib.rs:356`), the `nvim_command` and
  `nvim_set_current_buf` arms, and once at startup after `source_init` (`lib.rs:166`);
  new `Server` fields. (No `:w` interception — that's the separate write-seam doc.)
- Tests in `crates/bemtvi-server/tests/autocmds.rs` (new file, sibling to `buffers.rs`)
  — black-box via `nvim_input` / RPC, asserting on observable effects (a callback that
  runs `:` commands or `print`s a marker), per the project's no-unit-test rule. It
  carries its own `start`/`start_with_config`/`feed`/`lines` helpers, copied from the
  `editing.rs`/`buffers.rs` pattern (integration-test files don't share a module).

---

## Phase 1 — Registration & dispatch completeness (pure bridge)

**Goal / value.** Make the autocmd *substrate* complete and correct before the
editor emits anything new: buffer-aware callback args, augroup `clear`, manual
firing. Independently testable with **zero** editor lifecycle wiring, via
`vim.api.nvim_exec_autocmds`.

**Prerequisites.** None.

**Scope (in).**
- `btv._fire(event, pattern, buf, file)` → callback `args = {id, event, match, buf, file}`.
- `btv._cur_buf = {bufnr, name}` snapshot + `btv._set_cur_buf`; add the Lua
  `vim.api.nvim_buf_get_name(bufnr)` binding (0 / snapshot bufnr → snapshot name) and a
  minimal `vim.fn.expand` for `%`, `%:p`, `%:h`, `%:t`. `%` is the path as stored on
  the buffer; `%:p` wants an absolute path, so the snapshot should carry an absolute
  `name` (or `expand` canonicalizes) — for the first cut `%:p` ≈ the stored path is
  acceptable.
- `nvim_create_augroup(name, {clear=true})` clears the group's autocmds; each
  autocmd stores `opts.group`; `nvim_create_autocmd` honors `opts.group` and
  `opts.buffer` (buffer-local match).
- `vim.api.nvim_exec_autocmds(event, opts)` — manual firing (drives the tests).
- `vim.api.nvim_del_autocmd(id)`.

**Scope (out).** Any editor-emitted events (Phases 2–3). A real per-bufnr buffer
registry (snapshot only).

**Tests** (`crates/bemtvi-server/tests/autocmds.rs`).
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
- `emit_lifecycle_events()` — the state diff, called after each applied input: the
  per-key loop in `Server::input` (`lib.rs:356`), the `nvim_command` and
  `nvim_set_current_buf` arms, and a one-time startup seed after `source_init`
  (`lib.rs:166`). *Not* inside `redraw()` — its `view` is already computed before
  `sync_syntax`, and it also fires on syntax-only events where nothing transitioned.
  If the current buffer left the `announced` set: snapshot it, and **only for a
  file-backed buffer** fire `BufReadPost` then `FileType` (pattern = filetype from
  `filetype_of`, only if detected); mark announced. If the current buffer differs from
  `last_buffer_id`, fire `BufEnter`. Each fire pushes the buffer snapshot first and
  drains Lua effects after. (LSP's `sync_lsp` is a future downstream consumer, not
  present today.)
- Covers startup (initial file arg), `:edit`, and buffer switches
  (`nvim_set_current_buf`, `:bnext`, `:b`, `<C-^>`) — the diff is mechanism-agnostic,
  so any path that changes the current buffer is caught.

**Scope (out).** `InsertEnter` (Phase 3); `BufWritePre` (separate write-seam doc).

**Tests.**
- Opening a `.rs` file fires `FileType` with `match == "rust"` and `args.file`
  the path (assert via a callback that records into a buffer / runs a command).
- Switching buffers fires `BufEnter` for the new buffer; `FileType` does **not**
  re-fire for an already-announced buffer.
- A `BufReadPost` callback can read `vim.api.nvim_buf_get_name(0)` and get the path.

**Done when.** The above pass; ordering is `BufReadPost`→`FileType`→`BufEnter` on
first open; gates green.

---

## Phase 3 — Mode event (`InsertEnter`)

**Goal / value.** The mode-transition event, completing the set this doc covers.
Mode-driven plugins (completion, snippet engines) hook `InsertEnter`.

**Prerequisites.** Phase 2.

**Scope (in).**
- `InsertEnter`: `Server` tracks `last_mode`; `emit_lifecycle_events()` fires
  `InsertEnter` when the mode transitions into `Mode::Insert` (covers `i/I/a/A/o/O/C`,
  `cc`/`s`/`S`, etc. without touching the many core insert chokepoints — the diff sees
  the result). Because the diff runs **per applied key** (model §1), a batched
  `nvim_input("o…<Esc>")` still fires it on the `o`.
- *Fidelity note:* neovim also fires `InsertEnter` when entering **Replace** (`R`,
  `Mode::Replace`). This doc fires on `Mode::Insert` only; either extend the condition
  to "enters Insert *or* Replace" or record Replace as a known gap.

**Scope (out).** `InsertLeave`, `TextChanged`, `BufWinEnter`, `BufWritePost`, etc. —
add as consumers appear. `BufWritePre` and format-on-save are their own design (the
write-seam doc).

**Tests.**
- Entering insert via `i` (and via `o`) fires `InsertEnter` exactly once per entry.
  Per-key emission means even `feed("o<Esc>")` fires it on the `o`; a test that leaves
  the editor in insert (feed `i`/`o`, assert, then `<Esc>`) reads most clearly.

**Done when.** The above pass; entering insert from each command fires `InsertEnter`
exactly once; gates green.

---

## Downstream (not in this doc)

- **LSP Phase 7** (`docs/specs/2026-06-02-lsp-support-design.md`) —
  `vim.lsp.enable` installs a `FileType` autocmd; depends on Phase 2.
- **`BufWritePre` write seam + format-on-save**
  ([`2026-06-04-bufwritepre-write-seam-design.md`](2026-06-04-bufwritepre-write-seam-design.md))
  — the pre-write hook this doc deliberately excludes; needs core-side write deferral.
- **Buffer-local keymaps / ftplugin** — depend on Phase 2 + buffer context.
