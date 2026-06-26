# Port the file explorer to a pure-Lua plugin

Status: **COMPLETE — all phases landed 2026-06-25.** Phase 1: the general mouse-mapping
primitive (explorer double-click rides a buffer-local `<2-LeftMouse>` map). Phase 2:
the `BufReadCmd` directory-open hook. Phase 3: the pure-Lua explorer plugin. Phase 4:
the cutover — the Lua explorer is the default, the Rust explorer engine
(`editor/explorer.rs`, `BufferKind::Directory`, `from_dir*`, `load_dir_into`, the
`enter_dir`/`apply_explorer_action`/`_explorer_action` chain) is deleted, and the tests
drive the Lua explorer. The file explorer is now a pure-`nx.*` plugin.

Make the in-window directory listing (vim's netrw, `filetype=nxdir`) a **pure-Lua
plugin** built on the `nx.*` API, deleting the Rust explorer engine. This follows the
prime dogfood directive (every feature that can reasonably be an `nx.*` Lua plugin IS
one; Rust keeps only core/frame/engines) and ADR 0002. The explorer is the canonical
"netrw-as-plugin" case, so it should not have a bespoke Rust engine.

The port is gated on **two general primitives that don't exist yet** — a Lua-facing
mouse-mapping surface and a directory-open hook. Both are broadly useful beyond the
explorer, so they are built as general `nx.*` / autocmd surfaces, not explorer shims.

## Why it isn't free today

A grep of `nxvim-lua` confirms the gaps:

1. **No Lua mouse surface at all.** `on_mouse` / `nx.mouse` / `<LeftMouse>` return
   nothing — mouse events are entirely core-handled (`editor/mouse.rs`) and never
   reach Lua. Click-to-open cannot be pure Lua until a mouse primitive exists.
2. **No directory-open hook.** "This path is a directory → list it" is decided in the
   core in three places — `ex.rs:1860` (`:e dir` → `enter_dir`), `mod.rs:1379`
   (startup arg), and `lifecycle.rs:55` (the off-tick daemon read). There is no
   `BufReadCmd`-style event a Lua plugin can register to *claim* the read (netrw's
   mechanism in real vim).

Everything else is already Lua-able: `nx.fs` reads directories asynchronously and
already works over the daemon/remote (`fs.lua`, `daemon_luafs`); `nx.view` is the
established plugin-owned `nomodifiable` buffer with activation maps; navigation maps
and the new directory-row highlighting (`prelude/explorer.lua`, an `nx.decor`
provider) are already Lua.

## Current Rust surface to be removed at cutover

- `crates/nxvim-core/src/buffer.rs` — `Buffer::from_dir` / `from_dir_entries`,
  `BufferKind::Directory`, `Buffer::dir()`.
- `crates/nxvim-core/src/editor/explorer.rs` — the whole file (`enter_dir`,
  `explorer_open_entry`, `explorer_up`, `is_explorer_buffer`,
  `apply_explorer_action`).
- `crates/nxvim-core/src/editor/buffers.rs:798` `load_dir_into` + the `enqueue_open`
  directory branch; `lifecycle.rs:55` / `:101` the off-tick listing build.
- The `:e dir` / startup `is_dir()` branches (`ex.rs`, `mod.rs`).
- The interim `is_explorer_buffer()` double-click special-case in `mouse.rs` (added
  2026-06-25; superseded by Primitive A).

Kept and reused by the Lua plugin:

- `prelude/explorer.lua` — the `nx.decor` highlight provider (filetype `nxdir`).
- The `FileType nxdir` keymap wiring (`prelude/keymap.lua`) — folds into the plugin.

## Target architecture

The explorer becomes a Lua module (`prelude/explorer.lua`, extended — or a bundled
first-party plugin) that:

1. registers a **`BufReadCmd`** handler claiming directory paths (Primitive B);
2. on a claimed open, reads entries via `nx.fs` (async; works locally and over the
   daemon) and renders them into an **`nx.view`** buffer (`nomodifiable`, owned);
3. installs buffer-local **`<CR>` / `-` / `<2-LeftMouse>`** maps (Primitive A gives
   the mouse one) for open / parent / open-on-click;
4. highlights rows via the existing `nx.decor` provider.

No `BufferKind::Directory`; the listing is a `BufferKind::View`. The "modified" bug is
structurally gone — a view is `nomodifiable`, never "modified relative to a backing
store".

## Primitive A — mouse mappings (`<LeftMouse>` / `<2-LeftMouse>` / …)

neovim-faithful and **reuses the keymap engine** (the "reuse existing APIs" directive):
mouse gestures become mappable keys. A buffer-local `<2-LeftMouse>` map on the listing
gives click-to-open with no explorer-specific code.

- Teach the keymap key model to parse `<LeftMouse>`, `<2-LeftMouse>`, `<3-LeftMouse>`,
  `<RightMouse>`, `<MiddleMouse>` (drag/release deferred).
- In `Editor::mouse` left-press: keep today's focus + cursor placement, then look up a
  mapping for the count-appropriate mouse key in the current buffer/context. If bound,
  dispatch its rhs through the existing map-execution path and consume; else fall
  through to the current selection behavior. Cursor-placement-before-map matches
  neovim.
- v1 scope: cursor placement is the only position signal (enough for the explorer).
  `getmousepos()` / `v:mouse_*` exposure is **out of scope** for v1 (note it as a
  follow-up for general handlers that act away from the cursor).
- Tests: a buffer-local `<2-LeftMouse>` Lua map fires on double-click; `<LeftMouse>`
  on single click; an unmapped buffer keeps today's selection behavior; the gate
  (`'mouse'`) still applies.

## Primitive B — directory-open hook (`BufReadCmd`)

Add **`BufReadCmd`** as a supported autocmd event (neovim-faithful; the general
"replace the default read" hook netrw rides).

- Fire it on the buffer-read path (`lifecycle.rs` / `buffers.rs` open) with the path
  as `<amatch>`/`<afile>` *before* the core's default read. If a handler ran (matched),
  the core **skips** its default read — the handler owns filling the buffer.
- The Lua explorer registers `BufReadCmd` with pattern `*` and bails unless
  `isdirectory(amatch)` (exactly netrw), so it claims only directories; ordinary file
  reads are untouched.
- Tests: a trivial Lua `BufReadCmd` handler claims a path and fills the buffer, and
  the core does not also read it; a non-matching handler leaves file reads unchanged.

## Phasing (commit + pause for review between phases)

- **Phase 1 — Primitive A (mouse maps). ✅ DONE 2026-06-25.** `<LeftMouse>` /
  `<2-LeftMouse>` / `<3-LeftMouse>` / `<RightMouse>` / `<MiddleMouse>` parse as
  `KeyCode::Mouse { button, clicks }` (`core/input.rs`). A left press places the
  cursor (the `<LeftMouse>` default) and queues a `MouseClick` on the core; the server
  drains it (`resolve_mouse_clicks`, wired on **both** the native dispatch and the
  wasm `EditHost::mouse` paths) and either fires the bound `<n-LeftMouse>` map
  (`Keymaps::lookup_mouse`) or runs the default word/line escalation
  (`Editor::mouse_apply_default_select`, deferred so a map can suppress it). The
  explorer double-click is now a buffer-local `<2-LeftMouse>` default map in the
  `FileType nxdir` autocmd; the interim `mouse.rs` special-case is gone. Tested in
  `tests/mouse.rs` (general) and `tests/editing/explorer.rs` (the explorer consumer).
  - **Follow-up — modifiers + right/middle buttons. ✅ DONE 2026-06-26.** All three
    buttons are now mappable with modifiers: `<C-LeftMouse>` / `<A-LeftMouse>`,
    `<RightMouse>`, `<MiddleMouse>`, `<S-LeftMouse>`, and combos. A `MouseClick` carries
    its `row`/`col`/`stamp_ms` so the right / middle / shift-left presses defer their
    *whole* default behaviour (the `'mousemodel'` dispatch, the `"*` paste, the
    selection-extend) behind the keymap lookup via the new `Editor::mouse_apply_default`
    — a bound `<…Mouse>` map suppresses the default, exactly like left. A plain / ctrl /
    alt left still places the cursor *eagerly* (so `<C-LeftMouse>` → go-to-definition
    works on the click); a mapped right/middle leaves the cursor put and reads the click
    via `getmousepos()` (below). Multi-click is still left-only (`<2-RightMouse>` not yet
    counted). Tested in `tests/mouse.rs` (5 new: ctrl-left places + fires, modifier
    distinguishes the map, right / middle / shift-left fire-and-suppress-default).
  - **Follow-up — `getmousepos()`. ✅ DONE 2026-06-26.** `vim.fn.getmousepos()` (canonical
    `nx.getmousepos()`) returns the last mouse event's position as a dict —
    `screenrow`/`screencol` (1-based global cell), `winid`, `winrow`/`wincol` (1-based,
    window-relative, gutter included), `line`/`column` (1-based buffer line + byte column,
    0 off a window's text), `coladd` (always 0). The core records the last event's cell
    (`Editor.last_mouse`, set in `Editor::mouse`) and `Editor::mouse_pos()` resolves it
    through the same `hit_test` the gestures use (window-relative via `window_screen_pos`,
    so chrome above the window is excluded). The server mirrors it to `nx._mouse_pos` in
    `push_buf_mirror` — which `fire_mapping` runs *before* a mapping's RHS, so a
    `<RightMouse>` / `<MiddleMouse>` map reads the *clicked* cell even though right/middle
    don't move the cursor. Tested in `tests/mouse.rs` (4 new: clicked position, zero
    before any click, window-relative excludes a tabline, a `<RightMouse>` map reads the
    click without moving the cursor).
  - Still deferred: drag/release as mappable gestures (`<LeftDrag>` / `<LeftRelease>`),
    `v:mouse_*` (set by `getchar()` mouse reads), and right/middle multi-click.
- **Phase 2 — Primitive B (`BufReadCmd`). ✅ DONE 2026-06-25.** A file open is now
  **deferred** (enqueued as a `PendingOpen`) instead of read inline whenever a
  `BufReadCmd` handler is registered (`Editor::should_defer_open` = `host_fs_offtick ||
  bufreadcmd_active`; the server mirrors `bufreadcmd_active` from its `au_active_events`
  cache). The server's `drain_pending_opens` fires `BufReadCmd` via `fire_buf_read_cmd`
  *before* the default load: a handler **claims** the read by returning a truthy value
  (`nx._fire_read_cmd` — a per-path decision, so a `pattern = "*"` handler claims
  directories but declines files), and a claim skips the default read and settles the
  handler's buffer fill (`apply_lua_effects` drains its `nvim_buf_set_lines` op). An
  unclaimed open falls through — over the wire off-tick, or via the new
  `Editor::load_pending_open` locally. The fire sits at the **shared** drain seam, so it
  covers native-sync, daemon, and the fully-client wasm build (which always defers). The
  common no-handler config defers nothing and is byte-for-byte unchanged. `run_command`
  now refreshes the autocmd cache so a `:e` issued before the first keystroke sees a
  startup-registered handler. Tested in `tests/bufreadcmd.rs` (claim, decline-passthrough,
  naming). Known wrinkle: a claimed buffer's `BufReadPost` may fire on the empty buffer
  *before* `BufReadCmd` (emit runs before the drain) — harmless here; the explorer
  re-sets its filetype. Web runtime verification (Playwright) rides with Phase 3/4.
- **Phase 3 — the Lua explorer plugin. ✅ DONE 2026-06-25.** `prelude/explorer.lua`
  gained the explorer itself, opt-in via `nx.explorer.enable()` (registered in parallel
  with the core one — both pass the same observable behaviour). Its `BufReadCmd` handler
  (`pattern = "*"`) claims a directory open and **declines files**, deciding per path on
  a new **`args.isdir`** the server computes (a sync `std::fs` stat in `fire_buf_read_cmd`,
  threaded through `fire_autocmd_cmd` → `nx._fire_read_cmd`) — the live Lua fs surface is
  async, so the handler can't re-stat synchronously. On a claim it reads entries with
  `nx.fs.readdir` (async; local disk or, over a daemon, the wire), renders them the same
  way core `from_dir_entries` does (`../`, dirs-first suffixed `/`, case-insensitive),
  fills the **opened** buffer (its name is the directory path, so `:ls`/statusline match),
  locks it `nomodifiable` **and clears `modified`** (the fill is a read, not an edit — so
  no `[+]`), marks it `nxdir` (drives the decor + the buffer-local maps), and installs the
  activation maps. `<CR>`/`<2-LeftMouse>` open the entry under the cursor, `-` goes up;
  descend / open / parent are `:edit <path>` + `bwipeout` of the old listing, so the
  window is reused and the count is unchanged (descend in place; opening a file destroys
  the listing) — the netrw behaviour, with no `nvim_buf_set_name`/`delete` needed.
  Supporting changes: a local `:e dir` now **defers** to `BufReadCmd` when a handler is
  registered (`ex_edit`, gated on `bufreadcmd_active && !host_fs_offtick`), and
  `vim.bo.modified` became a settable buffer option (vim's `:set [no]modified`), the
  general clear a plugin-filled read buffer needs. Tested in `tests/explorer_lua.rs`
  (list / descend / open-file / parent / read-only-and-unmodified). Commit.
  - **Daemon claim deferred to Phase 4.** `args.isdir` is a *local* stat, so off-tick
    it is `false` and the off-tick directory keeps the **core** explorer for now (the
    `nx.fs` *fill* works over the wire — verified by the existing daemon fs suites — but
    the synchronous claim decision off-tick needs either a sync remote dir-check or a
    server-side classify-before-fire, which lands with the cutover). No regression: the
    off-tick path is untouched.
- **Phase 4 — cutover + delete. ✅ DONE 2026-06-25.** The explorer is on by default
  (`nx.explorer.enable()` called at the bottom of `prelude/explorer.lua`), so a `:e dir`
  always defers to `BufReadCmd` and the plugin claims it. Deleted: `editor/explorer.rs`
  (whole file), `BufferKind::Directory` + `Buffer::dir()`, `Buffer::from_dir` /
  `from_dir_entries`, `Editor::load_dir_into`, the `nx._explorer_action` bridge +
  `explorer_actions` queue + its server drain, the `ex_edit` directory branch, the
  `open_or_named_with` directory-listing branch, and the server's `load_dir_replica` /
  `load_dir_replica_wasm`. The `FileType nxdir` autocmd (prelude/keymap.lua) now installs
  the plugin's pure-Lua `<CR>`/`-`/`<2-LeftMouse>` maps.
  - **Navigation is stateless.** `nx.explorer._open`/`_up` derive the directory from the
    buffer name and the entry from the row text (a trailing `/` marks a sub-directory), so
    they work regardless of *who* filled the listing — which is what lets the local and
    off-tick fill paths diverge without two navigation code paths.
  - **Two fill paths, one shape.** A **local** `:e dir` is claimed by the `BufReadCmd`
    handler, which reads entries with `nx.fs` and renders them (Lua `render`). A **remote**
    directory (daemon / web) is filled **server-side** from the entries the off-tick fetch
    already read (`EditHost::load_dir_listing` → `nxvim_core::dir_listing`) — because the
    `nx.fs` op and the file-open fetch are *separate daemon legs* and re-reading via
    `nx.fs` can't be relied on to reach the same remote. Both produce a `nomodifiable`,
    `filetype=nxdir` buffer named for the directory, so the stateless nav + decor are
    identical. (This supersedes the Phase-3 plan of off-tick re-read via `BufReadCmd`,
    which the daemon test proved can't reach a HostFsAsync-only remote.)
  - **Startup.** A local `nxvim somedir` can't list at construction (no Lua VM yet), so
    `open_or_named_with` leaves an empty buffer named for the directory and **enqueues**
    the open; `drain_pending_opens` now refreshes the autocmd cache first, so the
    just-registered explorer handler claims it once `init.lua`/the prelude have sourced.
  - **Supporting primitive:** `vim.bo.modified` is a settable buffer option (vim's
    `:set [no]modified`), used by both fill paths to mark the read-not-an-edit listing
    clean. Tests: `tests/editing/explorer.rs` (15, migrated to poll the async fill) and
    `tests/daemon_explorer.rs` (5) drive the Lua explorer; full workspace suite green.

## Disposition of the 2026-06-25 interim changes

The three changes made before this plan was chosen:

- **Highlighting** (`prelude/explorer.lua` + decor + `runtime.rs`): **keep** — it is
  already the Lua approach and is reused unchanged by the ported plugin.
- **Modified-flag fix** (`explorer.rs` `mark_clean` after `mark_resync`): keep as a
  correct interim fix; becomes moot at Phase 4 when `enter_dir` is deleted (a view is
  never "modified").
- **Mouse double-click** (`mouse.rs` `is_explorer_buffer` special-case + tests):
  **interim** — replaced in Phase 1 by the general `<2-LeftMouse>` mapping.

## Risks / open questions

- **`BufReadCmd` scope.** Full neovim `*Cmd` semantics are broad; v1 implements only
  what the read path needs (fire-before-read, consume-skips-default). Drag/release
  mouse maps and `v:mouse_*` are explicitly deferred.
- **Daemon/off-tick parity.** The Lua explorer must fill asynchronously over the
  daemon. `nx.fs` already does remote `read_dir`, but Phase 3 must verify the
  view-fill timing matches the current off-tick `load_dir_into` behavior (the
  cross-tick winid caveat — use `nx.on_next_tick`/`nx.wait_for`, never re-armed
  `nx.schedule`).
- **Buffer identity/name.** The current listing's buffer shows the directory path as
  its name; the `nx.view` must reproduce that (`view_name`) so `:ls` and the
  statusline read the same.
```
