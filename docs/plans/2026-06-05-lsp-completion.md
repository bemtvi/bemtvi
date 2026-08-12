# Making nvim-lspconfig *actually* work — completion plan

> **Status: COMPLETE.** All eight phases (0–8) landed; this document is now
> *history*, not a live to-do. The remaining gaps are the per-phase
> *approximations* each phase recorded — tracked canonically in code
> (`INCOMPLETE:` / `btv._notimpl`) and summarized in
> [`docs/known-approximations.md`](../known-approximations.md), which the code
> wins over if they ever disagree.

## Why this document exists

The Phase-7b work made all ~400 vendored `lsp/<server>.lua` configs **load and
start without crashing**. That is necessary but **not sufficient**: a config can
start a server and still not *work*, because bemtvi currently:

- **drops the config's `settings` / `init_options` / `capabilities`** — they are
  never sent at `initialize`, so a server runs on defaults no matter what the
  config (or user) set;
- **never calls the config lifecycle hooks** (`before_init`, `on_init`,
  `on_exit`) — e.g. rust_analyzer copies `settings → initializationOptions` in
  `before_init`, which never runs;
- **stubs out a swathe of the API** (`vim.lsp.util.*`, `client:request`,
  `vim.ui.*`, buffer/window getters) so server-specific commands, handlers, and
  floating-preview features silently no-op;
- **swallows config-load errors** (`lsp_base_config` `pcall`s the chunk and
  degrades to `{}`), so a config that hits a gap disappears silently.

The guiding principle for the rest of the work, set in Phase 0: **fail loud.**
A not-yet-implemented function raises with its own name instead of returning a
fake value, so every gap a real config hits is visible and trackable rather than
quietly wrong.

This plan is divided into self-contained phases. Each is sized to be picked up
and implemented in a single focused session without the others loaded. Phases
list their dependencies; later phases assume earlier ones landed.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Phase 0 — Fail loud: stubs raise `not implemented` ✅

**Goal.** Replace every *hollow* stub (returns fake/empty data or silently
no-ops) with a raise that names the function, and record each hit so the gaps
are trackable. Functions that faithfully perform the operation (even if
synchronously) stay and are listed as *known approximations*.

**Why.** Silent degradation is worse than a crash here: it makes a broken server
look configured. Loud failures turn "we think it works" into a concrete, ranked
list of what to build (the later phases).

**Scope (files).** `crates/bemtvi-lua/src/prelude.lua` only.

**Approach.**
- Add `btv._notimpl(name)`: records `name` into `btv._notimpl_hits` (a set, for
  introspection / a future `:checkhealth`) and `error("bemtvi: not implemented: "
  .. name, 2)`.
- Convert these hollow stubs to call it:
  - `vim.lsp.util.*` (make_position_params, make_text_document_params,
    make_given_range_params, locations_to_items, get_effective_tabstop,
    open_floating_preview, apply_workspace_edit, show_document)
  - `vim.lsp.omnifunc`, `vim.lsp.rpc.connect`, the `get_clients` client
    `:request`
  - `vim.ui.select` / `input` / `open`
  - `vim.api.nvim_get_current_win`, `nvim_win_get_cursor`, `nvim_buf_is_loaded`,
    `nvim_buf_get_lines`, `nvim_buf_set_lines`, `nvim_set_option_value`
  - `vim.fn.bufnr`, `setreg`, `setqflist`, `confirm`
  - `vim.uri_to_bufnr`, `vim.defer_fn`
  - `vim.bo`: option *writes* and non-`filetype` reads raise (the `filetype`
    read stays real — it backs `root_dir` filetype checks)
- **Keep (real or faithful for every input bemtvi produces):**
  `vim.api.nvim_get_current_buf` (snapshot = the real current buffer),
  `vim.fn.finddir` (real `vim.fs` search), `vim.schedule` (runs
  immediately — a safe "soon" in the synchronous model). List these in the doc as
  known approximations, not gaps. (`vim.fn.substitute` was an identity stub here at
  Phase 0; it is now a **real vim-regex engine** — see `bemtvi-lua/src/vimregex.rs`.)

**Tests.**
- The config sweep (`lspconfig_configs.rs`) still passes: the raising stubs are
  not on the load/`root_dir`/`cmd` path. Add `gdscript` to its allowlist
  (top-level `vim.lsp.rpc.connect` now raises → TCP transport is a known gap).
- Add a prelude test asserting a representative stub raises with its name.

**Known approximations (kept — real or faithful for every input bemtvi produces,
NOT gaps).**
- `vim.schedule(fn)` — runs `fn` inline (a safe "soon" in the synchronous model).
- `vim.api.nvim_get_current_buf` — returns the real current-buffer snapshot.
- `vim.fn.finddir` — real `vim.fs` upward directory search.
- `vim.fn.substitute` — was an identity stub at Phase 0; **since reimplemented** as
  a real vim-regex substitution (vim magic dialect + replacement syntax →
  `regex`-crate, `bemtvi-lua/src/vimregex.rs`), no longer an approximation.
- `vim.bo[buf].filetype` / `vim.bo.filetype` — the one faithful `vim.bo` read
  (snapshot-backed); it backs the `root_dir` filetype checks. Every other `vim.bo`
  read and all writes raise.

**Done when.** ✅ Every hollow stub raises a named error via `btv._notimpl`; the
config sweep (`lspconfig_configs.rs`) is green with the documented allowlist
(`powershell_es`, `gdscript`); `btv._notimpl_hits` accumulates hit names; a
prelude test (`prelude_notimpl.rs`) asserts a representative stub raises with its
name and records the hit while the faithful neighbours stay real.

**Depends on.** Nothing.

---

## Phase 1 — Stop swallowing config-load & start failures ✅

**Goal.** Make a config that errors at load (now possible after Phase 0) or a
server that fails to start **visible**, not degraded-to-`{}`.

**Why.** `lsp_base_config` does `pcall(chunk)` and returns `{}` on failure, so a
config that hits a not-implemented gap vanishes. With Phase 0 raising, this
swallowing actively hides the signal.

**Scope.** `prelude.lua` (`lsp_base_config`, `lsp_resolve_cmd`), a small report
surface. Optionally a `vim.lsp` health/report function.

**Approach.**
- When `lsp_base_config`'s chunk errors, record `{name, error}` into
  `btv._lsp_load_errors` and echo a one-line warning (don't hard-crash the whole
  editor — one broken server shouldn't wedge startup — but make it loud).
- When `lsp_resolve_cmd` skips a server (non-argv / builder threw), record
  `{name, reason}` into `btv._lsp_skipped` instead of silently returning.
- Add `vim.lsp._report()` (and wire a `:LspInfo`-style command later) listing:
  enabled servers, which started, which were skipped/failed and why, and the
  `btv._notimpl_hits` set.

**Tests.** A config that references a not-implemented symbol at load surfaces in
`btv._lsp_load_errors`; a non-argv cmd surfaces in `btv._lsp_skipped`.

**Done when.** ✅ `lsp_base_config` records a present-but-failing config (unreadable
/ parse error / runtime error / non-table return) into `btv._lsp_load_errors` and
echoes a one-line warning instead of degrading to `{}`; `lsp_resolve_cmd` returns
`nil, reason` on a throwing builder, and `lsp_start_resolved` / `vim.lsp.start`
record a skip into `btv._lsp_skipped` (deduped, with a reason) instead of a bare
`return`. `vim.lsp._report()` enumerates `enabled` / `started` / `load_errors` /
`skipped` / `notimpl_hits`. Covered by `lsp_report.rs`. (A `:LspInfo`-style command
that renders `_report()` is left as a later follow-up — the data surface is here.)

**Depends on.** Phase 0.

---

## Phase 2 — Forward `settings` / `init_options` / `capabilities` ✅

**Goal.** Send the config's `settings`, `init_options`, and merged
`capabilities` to the server, so it runs *configured*, not on defaults.

**Why.** This is the single biggest "starts but doesn't work" gap. Today
`InitializeParams` (manager.rs:607) sets only `process_id`, `root_uri`,
`capabilities: client_capabilities()`; `vim.lsp.start` forwards only
`{name,cmd,root_dir,filetype,bufnr}`. Everything a config configures is dropped.

**Scope.** `prelude.lua` (`vim.lsp.start` / `lsp_start_resolved`), `lib.rs`
(`btv._lsp_start` signature, `LspOp::Start` fields), `bemtvi-server` (apply path),
`bemtvi-lsp/manager.rs` (`InitializeParams`, post-init `didChangeConfiguration`).

**Approach.**
- Thread `settings`, `init_options`, `capabilities` from the resolved config
  through `btv._lsp_start` → `LspOp::Start` as JSON (reuse the `vim.json`/serde
  bridge → `serde_json::Value`).
- In `manager.rs`: set `InitializeParams.initialization_options =
  init_options (or settings fallback)`, and **merge** the config's
  `capabilities` over `client_capabilities()`.
- After `initialized`, send `workspace/didChangeConfiguration { settings }`.

**Tests.** ✅ `the_config_settings_init_options_and_capabilities_reach_the_server`
(`crates/bemtvi/tests/lsp.rs`): a config sets a sentinel in each of `settings` /
`init_options` / `capabilities`; the mock records the handshake and we assert
`init_options` → `initializationOptions`, the config's `capabilities` deep-merged
over the base (sentinel present *and* base `positionEncodings` survive), and
`settings` → `workspace/didChangeConfiguration`.

**Done when.** ✅ The resolved config's `settings` / `init_options` /
`capabilities` ride `btv._lsp_start` → `LspOp::Start` (as `serde_json::Value` via
the `lua_to_json` bridge, empties dropped) → `ServerSpawn` → `run_server_once`:
`initialization_options = init_options or settings`, `capabilities` = base
deep-merged with the config's (malformed → logged, base used — loud, not silent),
and `workspace/didChangeConfiguration { settings }` after `initialized`.

**Depends on.** Phase 0 (clean baseline). Independent of the event loop.

---

## Phase 3 — Lifecycle hooks: `before_init` / `on_init` / `on_exit` ✅

**Goal.** Call the config's lifecycle hooks at the right moments so configs that
shape init params or react to the server (e.g. rust_analyzer's
`before_init` copying `settings['rust-analyzer'] → initializationOptions`) work.

**Why.** Several configs *only* become correct through `before_init`; without it
Phase 2's forwarding still misses what they compute.

**Scope.** A Rust→Lua call path (the server invokes a Lua function around the
handshake), `prelude.lua` (store hooks per client), `manager.rs` (call points).

**Approach.**
- Before sending `initialize`, call `before_init(init_params, config)` in Lua,
  let it mutate a params table, read it back into `InitializeParams`.
- After `initialize` result, call `on_init(client, result)`.
- On exit, call `on_exit(code, signal, client)`.
- Requires a synchronous Rust→Lua call with a params round-trip (msgpack/JSON).

**Tests.** ✅ `before_init_shapes_the_initialize_params` (rust_analyzer-style
`settings → initializationOptions`), `on_init_runs_with_the_real_initialize_result`
(hook sees the raw result + client), `on_exit_runs_with_the_exit_code` (clean
exit → `code == 0`), all in `crates/bemtvi/tests/lsp.rs`.

**Done when.** ✅ `before_init(init_params, config)` runs *synchronously on the
editor thread* just before the start is queued (no event loop needed) and its
mutations to `init_params.initializationOptions` / `.capabilities` / `config.settings`
feed the Phase-2 forwarding. `on_init(client, result)` fires when the `Initialized`
event lands, carrying the raw `InitializeResult` (now threaded as JSON on the
event). `on_exit(code, signal, client)` fires on `ServerExited` with the child's
exit status (captured in the manager; `signal` is unix-only), while the client is
still registered. A throwing hook is recorded in `btv._lsp_hook_errors` (surfaced
by `vim.lsp._report`) and echoed, never fatal.

*Approximations:* a `config.cmd` mutation inside `before_init` is not honored (the
cmd is already resolved by then); `on_exit` does not fire on an intentional
shutdown (only on a server exit / crash), since that path registers no client.

**Depends on.** Phase 2 (params plumbing).

---

## Phase 4 — Async runtime / event loop ✅ (foundational)

**Landed.** Implemented in full as its own four-phase effort —
[`docs/plans/2026-06-06-async-lua-runtime.md`](2026-06-06-async-lua-runtime.md). The event-loop
actor is `crates/bemtvi-server/src/evloop.rs`; the deferred-callback registry is
`btv._cb_fns` (Lua) driven by `LuaRuntime::run_callback(id, keep, args)` (Rust);
the queue is `Shared.loop_ops` / `take_loop_ops`. Phase 5 below plugs into that
**callback-dispatch primitive** rather than inventing its own — see its note.

**Goal.** A real scheduler so deferred work runs off-tick: `vim.schedule`
genuinely defers, `vim.defer_fn(fn, ms)` honors the delay, `vim.system` runs the
child asynchronously with an off-tick `on_exit`, and `vim.uv` timers exist.

**Why.** Root cause behind several Phase-0 raises and the synchronous-`vim.system`
caveat. Unblocks real `client:request` (Phase 5) and removes the
"blocks the server thread" limitation.

**Scope.** `bemtvi-server` main loop (a task/callback queue the Lua VM drains),
`bemtvi-lua` (a Rust→Lua deferred-callback registry), `prelude.lua`
(`vim.schedule`/`defer_fn` re-pointed at it, `vim.uv` timer funcs).

**Approach.** Add a server-side queue of pending Lua callbacks (keyed by id) that
the main loop services between RPC messages; `vim.system`/timers register a
callback id, and completion enqueues it. Carefully keep the single-threaded,
one-message-at-a-time invariant.

**Tests.** `vim.defer_fn` runs after the tick, not inline; an async
`vim.system` callback fires on a later tick.

**Done when.** Deferred callbacks run off-tick; `vim.system` no longer blocks.

**Depends on.** Phase 0. (Large; the pivot for the back half.)

---

## Phase 5 — Real `client:request` + response handlers ✅

**Goal.** `client:request(method, params, handler, bufnr)` issues a real LSP
request and routes the response back to the Lua `handler` — enabling
server-specific commands (`:LspCargoReload`, organize-imports,
switchSourceHeader) and config `handlers`.

**Why.** These are dead today (the `:request` stub returns false). Many configs'
value-add lives here.

**Scope.** `bemtvi-lsp` (issue arbitrary request, correlate response),
`bemtvi-server` (route response → Lua callback via Phase 4's queue),
`prelude.lua` (real client `:request`/`:notify`, `handlers` dispatch).

**Approach.** Generic request bridge: Lua queues `{method, params, handler_id}`;
the manager sends it; the response enqueues `handler(err, result)` on the
callback queue. Wire config `handlers[method]` into the response path.

> **Seam (from the async-runtime plan).** The callback-dispatch primitive already
> exists: register the handler with `btv._next_cb_id()` (Lua) — the same registry
> `vim.schedule`/timers/`vim.system` use — and thread the id through the `LspOp`
> alongside the existing `ReqToken` (`crates/bemtvi-lsp/src/manager.rs` already
> correlates replies by token). On `LspEvent::Reply`, `on_lsp_event` runs the
> handler via the callback dispatcher. **This plan owns only the
> LSP-reply *payload*:** add a `CallbackArgs::LspReply { err, result }` variant in
> `bemtvi-lua` (next to `None`/`Process`) so `run_callback` can hand the handler its
> `(err, result)`. The `loop_events`/`lsp_events` arms already call `settle_events`,
> so a handler that defers via `vim.cmd`/`vim.schedule` is driven to convergence
> off-tick — the gap this plan closed.

**Tests.** ✅ `client_request_round_trips_a_custom_method` (request reaches the
server with its params; the JSON result reaches the Lua handler off-tick),
`client_request_unsupported_method_fails_loud` (an unknown method sets the
handler's `err` and never reaches the server),
`client_notify_reaches_the_server` (a generic notification arrives with its
params) — all in `crates/bemtvi/tests/lsp.rs`, driven through the scripted mock's
new `custom_replies` field.

**Done when.** ✅ `client:request(method, params, handler, bufnr)` and
`client:notify(method, params)` are real on every client table (`get_client_by_id`
/ `on_attach`'s client and `get_clients`), routing through
`LspOp::ClientRequest`/`ClientNotify` → `LspRequest::Raw`/`LspNotify::Raw` →
`LspReply::Raw` → `CallbackArgs::LspReply` → the handler `(err, result, ctx)`. A
no-explicit-handler request falls back to the config's `handlers[method]` then
`vim.lsp.handlers[method]`. The reply's `cb_id` rides on `ReqToken` (0 for the
typed native requests); raw replies bypass the cursor/buffer staleness machinery.
The `:request` `_notimpl` stub in `get_clients` is gone.

*The async-lsp dynamic-method constraint.* async-lsp 0.2.4's `ServerSocket::request`
is generic over a compile-time `lsp_types::request::Request` whose `METHOD` is a
`const &'static str`, so a truly arbitrary runtime method can't be sent through
its public API (the outgoing-request channel is private; no raw/dyn entry exists,
and the pinned offline dep can't be bumped). The `dyn_requests!` / `dyn_notifications!`
macros in `crates/bemtvi-lsp/src/manager.rs` bridge it: one zero-sized `Request`/
`Notification` type per supported method (all uniform `serde_json::Value` in and
out, since the editor only relays JSON to/from Lua) plus a runtime `match` on the
method string. The table covers every standard LSP method plus the named
server-specific ones (`rust-analyzer/*`, clangd `switchSourceHeader`, …); an
**unknown** method fails loud (the handler's `err` names it) and is a one-line
table addition away from support. This is the one deliberate approximation versus
"any arbitrary method," consistent with the no-silent-stubs rule.

**Depends on.** Phase 4 (callback queue).

---

## Phase 6 — Buffer / window Lua API ✅

**Goal.** Real `nvim_buf_get_lines`/`set_lines`/`is_loaded`, buffer-local options
(`vim.bo` backed by a store), `nvim_win_*`, and cursor access — the surface
`vim.lsp.util.*` and handlers need.

**Why.** Phase 7 (`vim.lsp.util.*`) and many handlers manipulate buffer text and
window/cursor state, which Lua currently can't touch.

**Scope.** `bemtvi-lua` (expose core buffer/window ops to Lua via the effect
queue + synchronous getters), `bemtvi-core`/`bemtvi-server` (the backing ops),
`prelude.lua` (un-stub the Phase-0 raises).

**Approach.** Extend the Lua↔core bridge with read getters (lines, cursor, win)
and queued mutations (set_lines), plus a per-buffer option store for `vim.bo`.

**Tests.** ✅ In `crates/bemtvi-server/tests/editing.rs`, driven through
`nvim_exec_lua`: `set_lines`→`get_lines` round-trips within one chunk (write-through
agrees with the real apply, confirmed by the native RPC read of the rope); negative
/ ranged `get_lines`; append / replace-all / delete; the fresh-empty-buffer
phantom-newline guard; a non-current buffer edited by id; `nvim_win_get_cursor`
tracks the real cursor; `nvim_get_current_win` is the stable handle;
`nvim_buf_is_loaded` true/false; a `vim.bo[buf].x = v` (and `nvim_set_option_value`)
write reads back while `filetype` still resolves; strict-indexing raises loud.

**Done when.** ✅ The synchronous getters (`nvim_buf_get_lines`/`is_loaded`,
`nvim_win_get_cursor`, `nvim_get_current_win`, `vim.fn.bufnr`) read the Rust→Lua
buffer mirror (`btv._bufs` / `btv._cur_cursor` / current-window handle) the server
refreshes via `LuaRuntime::set_buf_mirror` ← `Server::push_buf_mirror` before every
Lua entry that can read buffer/cursor state (top of `run_pending`, before
`eval_to_value`, before `run_keymap`/`run_keymap_expr`, and folded into the autocmd
`set_buf_snapshot` sites); the per-buffer line arrays are `changedtick`-gated so the
cursor-moved-no-edit path stays cheap. `nvim_buf_set_lines` writes through to the
`btv._bufs` mirror (so read-after-write within a chunk is consistent) and queues
`BufOp::SetLines` → `Server::apply_buf_op`, which normalizes the neovim line range,
converts it to a byte range against the real rope, applies it via
`Editor::apply_edits_to`, then flushes the buffer's LSP `didChange` via
`sync_lsp_buffer` (the must-not-omit step for a non-current buffer). `vim.bo` /
`nvim_set_option_value` are backed by a per-buffer store (`btv._bo_store`). The
Phase-0 `btv._notimpl` raises for all of the above are gone.

*Known approximations:* `nvim_win_get_cursor(win)` ignores `win` (single-window
bemtvi, handle `1000`); the `vim.bo` store is *observable* but not yet wired to
editor behavior (only `filetype` is behavior-backing, read from the snapshot);
`set_lines` can't produce a buffer without a final newline (`normalize()` always
re-adds the phantom `\n` — no `nofixeol`); each `nvim_buf_set_lines` is its own undo
step (no `undojoin` coalescing), matching `apply_workspace_edit`.

**Depends on.** Phase 0; benefits from Phase 4.

---

## Phase 7 — `vim.lsp.util.*` real implementations ✅

**Goal.** Implement the LSP utility helpers configs call in on_attach/handlers:
`make_position_params` (cursor + `offset_encoding`), `make_text_document_params`,
`make_given_range_params`, `locations_to_items` (→ loclist items),
`open_floating_preview` (→ bemtvi panel/float), `apply_workspace_edit`
(multi-buffer edits), `show_document` (jump).

**Why.** Turns the Phase-0 `vim.lsp.util.*` raises into working features that
config-shipped commands depend on.

**Scope.** `prelude.lua` + the buffer/window API (Phase 6) + the panel/float
surface; `apply_workspace_edit` reuses the rename/workspace-edit path in
`manager.rs`.

**Tests.** ✅ In `crates/bemtvi-server/tests/editing.rs` (driven through
`nvim_exec_lua`): `make_position_params_reflects_the_cursor_and_encoding` (real
cursor, byte→UTF-16 vs UTF-8 distinguished), `byte_to_position_char_handles_surrogate_pairs`
(4-byte char → 2 UTF-16 units / 1 UTF-32 codepoint),
`make_given_range_params_converts_marks_to_an_exclusive_range`,
`locations_to_items_builds_sorted_loclist_items` (sorted, `text` from the open
buffer), `get_effective_tabstop_prefers_shiftwidth_then_tabstop`,
`open_floating_preview_opens_a_real_float` (was a panel placeholder; now a real
cursor float — see `make_position_params_honors_the_window_arg` too),
`apply_workspace_edit_edits_the_open_buffer` (native RPC `lines` confirms the
rope), `show_document_jumps_the_cursor_to_the_location`,
`show_document_external_location_raises`.

**Done when.** ✅ The `vim.lsp.util.*` raises are gone, replaced by real
implementations: the param builders (`make_position_params` /
`make_text_document_params` / `make_given_range_params`) read the Phase-6 cursor /
buffer mirror and convert byte columns to the offset encoding via the shared
`btv._byte_to_position_char` / `btv._position_char_to_byte` UTF-8 walkers (utf-16
default, surrogate-aware); `locations_to_items` emits sorted loclist items with the
byte `col` and `text` from the open buffer backing each URI; `get_effective_tabstop`
reads the `vim.bo` store (shiftwidth → tabstop → 8); `open_floating_preview` shows
its lines in bemtvi's panel; `apply_workspace_edit` queues `LspOp::ApplyWorkspaceEdit`
→ `serde_json::from_value::<WorkspaceEdit>` → the exported `normalize_workspace_edit`
→ the native `Server::apply_workspace_edit`; `show_document` queues
`LspOp::ShowDocument` → `Server::jump_to_lsp_location` (open + cursor jump).

*Multi-buffer / disk follow-up (now closed):* `make_text_document_params` names
*any* open buffer, current or not (`nvim_buf_get_name` resolves a non-current
bufnr from the full buffer mirror); `locations_to_items` reads a location's `text`
from disk (memoized per file) when no buffer backs it, so a result list spanning
unvisited files shows real previews; and `apply_workspace_edit` loads an unopened
file into a buffer and applies the edit there (in memory, left modified for `:wa`,
as neovim's `apply_text_edits` does — never a straight-to-disk write), reporting
any URI it can't open rather than silently skipping it. A freshly-loaded target
buffer (no server of its own) takes the originating — current — server's encoding.

*Known approximations:* each `apply_workspace_edit` call is its own undo step (no
`undojoin` coalescing); an `external = true` `show_document` raises (no
external-open surface); `vim.uri_to_bufnr` stays a Phase-0 raise (no Lua-side
buffer-creating registry yet).

**Depends on.** Phase 6 (buffer/window), the panel surface.

---

## Phase 8 — `vim.ui.*` + server command dispatch ✅

**Goal.** `vim.ui.select`/`input` via bemtvi's panel/prompt, and dispatch of
server `workspace/executeCommand` + config `commands` (so `:Format`-style and
code-action commands run).

**Why.** Completes the interactive surface (code-action pickers, rename input,
server commands) the remaining configs use.

**Scope.** `prelude.lua` (`vim.ui.*` → panel/prompt), command registry +
`executeCommand` request, `vim.lsp.commands` dispatch.

**Tests.** ✅ In `crates/bemtvi-server/tests/editing.rs` (driven through
`nvim_command`/`nvim_input`): `vim_ui_select_routes_the_pick_to_on_choice`,
`vim_ui_select_format_item_renders_the_rows` (the panel shows `format_item`'s
text while `on_choice` gets the original item),
`vim_ui_input_hands_the_typed_line_to_on_confirm` (the label is projected into the
redraw; `<CR>` delivers the text), `vim_ui_input_default_prefills_and_is_editable`,
`vim_ui_input_cancel_hands_nil` (`<Esc>` → `on_confirm(nil)`). In
`crates/bemtvi/tests/lsp.rs`: `execute_command_relays_to_the_server` (no client-side
handler → `workspace/executeCommand` reaches the mock with its params),
`execute_command_runs_a_client_side_command` (a `vim.lsp.commands` handler runs and
the command is *not* relayed), `a_code_action_command_runs_via_execute_command` (a
bare-`Command` action dispatches `workspace/executeCommand` instead of the old
"command unsupported" echo).

**Done when.** ✅ The `vim.ui.*` raises are gone, replaced by real surfaces:
`vim.ui.select` lists choices in bemtvi's panel and routes the `<CR>` pick to
`on_choice(item, index)` (`opts.format_item` renders rows, the original item is
handed back); `vim.ui.input` opens a command-line prompt — a new
`CmdlineKind::Prompt` in `bemtvi-core` (the label rides the `View` as
`cmdline_prompt`, the typed line / `nil`-on-cancel flow back through
`Editor::prompt_results` → `Server::pending_ui_input` → `LuaRuntime::run_ui_input`
→ the `on_confirm` callback off-tick); `vim.ui.open` spawns the platform opener
(`open` / `xdg-open`, via `btv._ui_opener`) through the async `vim.system`. Command
dispatch goes through `vim.lsp._dispatch_command(client_id, command)` (shared by
`vim.lsp.buf.execute_command` and the native code-action path): a registered
`vim.lsp.commands[name]` handler wins client-side, else the command is relayed as a
`workspace/executeCommand` `client:request` (Phase 5). The native
`Server::apply_code_action` now applies an action's `edit` *and* dispatches its
`command` (via the new `CodeActionData.command` + `LuaRuntime::run_lsp_command`
Rust→Lua bridge), instead of echoing "command unsupported".

With the prompt machinery now in place, the surfaces that previously *required* a
name because no input existed prompt for it: `vim.lsp.buf.rename()` with no name
(the bare-RHS `vim.keymap.set('n','<leader>rn',vim.lsp.buf.rename)` case) and
`:LspRename` with no argument both open a `vim.ui.input` prompt prefilled with the
symbol under the cursor (`vim.lsp._cursor_word`, neovim's `<cword>`) and rename on
confirm, instead of echoing E471. Covered by
`lsp_rename_with_no_name_prompts_prefilled_with_the_cword` and
`vim_lsp_buf_rename_no_arg_prompts_via_lua`.

*Known approximations:* `vim.ui.select` does not deliver `on_choice(nil)` when the
panel is dismissed without a pick (`q`) — the panel has no cancel event; a real
pick is faithful. Only one `vim.ui.input` prompt is open at a time (a single
command line); if several are queued in one tick the last wins (loud single-prompt
limitation, not a silent drop). `vim.ui.open` is unauthenticated platform spawn
(no per-call success check beyond `vim.system`'s). A code action's `command` that
*resolves* lazily (a command only on the resolved action) is dispatched on the
eager/bare path only; a resolve-then-command chain is a follow-up.

**Depends on.** Phases 5 (requests) and 7 (util).

---

## Suggested order

`0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8`. Phases 2 and 3 deliver the most
"starts → works" value early and need no event loop; Phase 4 is the pivot the
back half (5, 7, 8) builds on. After each phase, the set of `btv._notimpl_hits`
a real config triggers shrinks — that set is the running scoreboard.
