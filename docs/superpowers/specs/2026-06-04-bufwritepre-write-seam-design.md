# `BufWritePre` write seam — design seed

**Date:** 2026-06-04
**Status:** Planned — **needs design.** This is a seed capturing the problem and a
code-grounded direction, split out of
[`2026-06-04-autocmd-lifecycle-design.md`](2026-06-04-autocmd-lifecycle-design.md)
during its sanity check. Flesh it into a full phased plan before executing.

**Depends on:** the autocmd lifecycle doc (Phases 1–2: `vim._fire` buffer args, the
`vim._cur_buf` snapshot, `fire_autocmd_buf`/`set_buf_snapshot`).
**Consumer:** format-on-save (its own design; this only lays the pre-write seam).

## Why this is its own doc

The autocmd lifecycle doc emits buffer/mode events by **diffing observable editor
state after the fact** — it can reconstruct "the buffer changed" or "we entered
insert" from the settled result. `BufWritePre` is categorically different: it is a
**pre-action** hook that must run Lua *before* a side effect (the disk write) that
has already happened by the time any state diff could observe it. After-the-fact
diffing cannot model it. It needs the write itself to cooperate, which is a core
change — out of scope for a doc whose whole premise is keeping core untouched.

## The trap (what does *not* work)

The original plan said "the server intercepts `:w`/`:write`/`:wq`/`:x`/`:wa` before
delegating to core." **It can't.** Both ways a write reaches disk run entirely
*inside* core, before the server regains control:

- **Interactive** `:w<CR>`: `editor.input(<CR>)` → `handle_command` Enter branch
  (`editor.rs:2502`) → `execute_ex` (`editor.rs:3033`) → `ex_write` (`editor.rs:3096`)
  → `Buffer::write` (`buffer.rs:276`) → `std::fs::write`.
- **RPC** `nvim_command(":w")`: `Server::run_command` (`lib.rs:362`) →
  `editor.command()` (`editor.rs:838`) → `execute_ex` → `ex_write` → `Buffer::write`.

In both, `Server::run_pending()` (where Lua effects drain) runs only *after* the
file is written. A server-side intercept could catch the RPC form but would **miss
interactive `:w<CR>` entirely** — the primary way users save, and what
`feed(rpc, ":w<CR>")` exercises in tests. The command string is assembled
character-by-character in core's command-line buffer and executed without ever
surfacing to the server as "a write." So the seam must live in **core**, not the
server.

## Direction: core-side write deferral

nxvim already has the exact pattern this needs — core records an intent and the
server fulfills it after regaining control. Three existing precedents:

- **`pending_sleep`** — `:sleep` sets `self.pending_sleep = Some(ms)`
  (`editor.rs:3077`); the server consumes `take_sleep()` (`editor.rs:776`) after
  dispatch (`lib.rs:219`).
- **`deferred_commands`** — an ex-command core doesn't recognize is pushed
  (`editor.rs:3092`) and the server drains it in `run_pending` (`lib.rs:437`).
- **`should_quit`** — `ex_quit` sets the flag (`editor.rs:3129`/`3147`); the server
  reads it post-dispatch (`lib.rs:175`).

Model `BufWritePre` the same way. Sketch:

1. **Core defers the write.** `ex_write` stops calling `Buffer::write` inline.
   Instead it pushes a `PendingWrite { buffer_id, path, then_quit, force }` onto a
   new queue (analogous to `deferred_commands`).
2. **Server fulfills it** in the `run_pending` fixpoint (so a callback's queued
   `vim.cmd` / future buffer edits drain in the same loop): for each pending write,
   push the `vim._cur_buf` snapshot, fire `BufWritePre` via `fire_autocmd_buf`, run
   `apply_lua_effects()` so callback edits land on the buffer, **then** perform the
   write through a core method (e.g. `editor.do_write(id, path)`) that does the disk
   I/O, sets `saved_seq` (`editor.rs:3106`), and echoes the `"N L, B written"` line.

## Hazards to design around

- **`:wq`/`:x` ordering.** `execute_ex` runs `ex_write` then `ex_quit` inline for the
  combined forms (`editor.rs:3049`). `ex_quit` checks `self.buffer().modified`
  *synchronously* (`editor.rs:3132`). If the write is merely *queued*, the buffer is
  still `modified`, so `:wq` reports `E37` and refuses to quit. Fix: carry
  `then_quit` on `PendingWrite` and have the **server** perform the quit *after* a
  successful deferred write — don't call `ex_quit` inline for `wq`/`x`/`wqa`/`xa`.
  A *failed* write must abort the quit (vim semantics), so the success/quit decision
  belongs server-side where the write result is known.
- **`:wall`/`:wa` is multi-buffer.** It writes every modified buffer
  (`ex_write_all`, `editor.rs:3213`). Each written buffer should fire its own
  `BufWritePre` with its own snapshot — queue one `PendingWrite` per buffer.
- **No Lua buffer-edit primitive exists.** `BufWritePre`'s headline value is
  "callbacks may edit the buffer (e.g. trim trailing whitespace, format) and the
  edit lands on disk." Today a Lua callback has **no way to edit buffer text**: the
  Lua surface has no `nvim_buf_set_lines`/`nvim_buf_set_text` (only ~5 `vim.api.nvim_*`
  functions, none for buffers), and core implements no text-mutating ex-command
  (`:s`, `:normal`, `:d` are all absent). So this seam needs a companion: a minimal
  queued buffer-edit primitive (`nvim_buf_set_lines`/`set_text`, draining to core
  like `vim.cmd`), or it can't even be tested end-to-end.
- **No-autocmd fast path.** When no `BufWritePre` is registered, observable behavior
  must be byte-for-byte unchanged (file written, `saved_seq` set, same echo). The
  server can short-circuit straight to the write; the deferral path only matters when
  a hook exists. Guard regressions with the existing write tests in
  `crates/nxvim-server/tests/{editing,buffers}.rs`.

## Tests (when designed)

Black-box in `crates/nxvim-server/tests/` (per the no-unit-test rule):

- `:w` (typed via `nvim_input(":w<CR>")` **and** via `nvim_command`) fires
  `BufWritePre` before the file is written.
- A `BufWritePre` callback that edits the buffer (via the new buffer-edit primitive)
  is reflected in the **bytes on disk** — proves pre-write ordering + edit-then-write.
- `:wq` writes then quits; a `BufWritePre` that fails/aborts the write leaves the
  buffer modified and does **not** quit.
- With no `BufWritePre` registered, `:w`/`:wq`/`:wall` behave exactly as today.

## Open questions

- Should deferral be unconditional, or only when a `BufWritePre` autocmd is
  registered (server tells core, or core always defers and the server decides)?
- Scope of the buffer-edit primitive for this round (`set_lines` only vs `set_text`)?
- `BufWritePost` (and `BufWrite`) — fire after the successful write in the same path,
  or defer to a later round?
