# Async `nx.lsp.*` verbs — return promises so LSP actions can be sequenced

**Goal.** Make the `nx.lsp.*` language verbs (`definition`, `references`, `hover`,
`format`, `rename`, `code_action`, `document_symbol`, …) return **promises** that
settle when the action's LSP round-trip is complete and its effect has been
applied/presented. This lets a user chain:

```lua
nx.lsp.format():next(function()
  return nx.lsp.rename("Foo")      -- runs only after format's edits land
end):next(function()
  nx.cmd("write")
end)

nx.lsp.references():next(function(items)  -- items = the resolved locations
  -- ...
end)
```

**User decisions (2026-07-23):**
- **Scope:** *all* round-tripping verbs become async (edit-appliers + navigation +
  hover + symbols), not just `format`/`rename`.
- **Interactive completion:** an interactive verb's promise settles **after the
  effect is applied** — `code_action` resolves once the user picks *and* the
  chosen action's edit is applied; cancelling the menu resolves `nil` (no effect).
  A multi-result navigation reply (references → picker) resolves with the
  **resolved item list** when the reply lands (the "effect" there is delivering
  the results; the user then browses the picker at their leisure).

## Contract

- Each verb returns an `nx.promise`.
- The promise **resolves** (never rejects) — resolve-only avoids the
  unhandled-rejection warning when a verb is used bare as a keymap RHS
  (`nx.keymap.set("n", "gd", nx.lsp.definition)`), which is the common case. A
  benign no-op (no server attached, reply superseded by a newer request, cursor
  moved / buffer changed before the reply, empty result, user cancelled) resolves
  `nil`. Genuine transport errors don't currently surface to Lua and stay out of
  scope.
- Resolve **value**:
  - navigation (`definition`/`declaration`/`type_definition`/`implementation`/
    `references`): a list of `{ text, path, row, col }` (1-based row/col — the same
    shape `nx.lsp._show_locations` items use); a single-jump goto resolves a
    1-element list.
  - symbols (`document_symbol`/`workspace_symbol`): the same `{ text, path, row,
    col }` item list.
  - `hover` / `signature_help`: the shown text as a string (`nil` if empty).
  - `format` / `rename` / `code_action`: `nil` (mutations — the effect is the
    buffer change).

## Mechanism (reuse what exists)

- `ReqToken.cb_id` (already present, `0` = no promise) carries the callback id from
  the issuing verb down to the reply. `PendingLspReq` gains a `cb_id` field so a
  **superseding** request of the same `kind` can settle the one it replaces.
- Settlement reuses `CallbackArgs::LspReply { err, result }` → `nx._run_cb(id,
  false, err, result)`. A new `EditHost::settle_lsp_promise(cb_id, result)` helper
  runs the callback (when `cb_id != 0`) and then `apply_lua_effects()` — mirroring
  `on_client_request_reply`, so a verb chained inside a `:next` handler has its
  queued `LspOp` drained and issued in the same convergence.
- Lua: a small `lsp_promise(issue)` helper in `prelude/lsp.lua` allocates a
  `nx._next_cb_id()`, stores a resolver in `nx._cb_fns`, calls `issue(id)`, and
  returns `nx.promise.new(...)`. Each verb becomes
  `function() return lsp_promise(function(id) nx._lsp_buf(KIND, id) end) end`.

### Settlement points in `on_lsp_reply` (`crates/nxvim-server/src/lsp/request.rs`)

- Top-of-function drops (unknown kind, no pending, **generation mismatch**): do
  **not** settle — the superseded request was already settled at
  `register_lsp_request` time, and a second reply for an already-handled kind has
  no live promise.
- Per-arm staleness drops (`buffer_changed` / `cursor_moved` / `tick_changed`):
  settle `token.cb_id` with `Ok(Null)` (benign no-op) before returning.
- Successful apply: settle with the marshalled result value.
- Issue-time "no server attached" (`lsp_target_or_echo` → `None`): settle
  `Ok(Null)` — the request never goes, so no reply will ever settle it.

`register_lsp_request(kind, cb_id)`: before overwriting an existing pending of the
same `kind`, if its `cb_id != 0`, settle it `Ok(Null)` (superseded). Store the new
`cb_id` in both `PendingLspReq` and the returned `ReqToken`.

## Phases

### Phase 1 — plumbing + single-reply verbs  ← this change
Everything except `code_action`'s interactive apply.

- `nxvim-lua`: add `cb_id: u64` to `LspOp::{BufRequest, Format, Rename,
  WorkspaceSymbol}`; thread it in `install.rs` (`_lsp_buf(kind, cb_id)`,
  `_lsp_buf_format(cb_id)`, `_lsp_buf_rename(name, cb_id)`,
  `_lsp_workspace_symbol(query, cb_id)`).
- `nxvim-server`: `register_lsp_request(kind, cb_id)`; `PendingLspReq.cb_id`;
  `settle_lsp_promise`; settle in `apply_lsp_op` arms + `request_lsp*` no-server
  paths + `on_lsp_reply` arms; build the location/symbol item list for the resolve
  value (extract the item-building from `open_locations_panel`/`apply_lsp_symbols`
  so it feeds both the picker and the promise); resolve `hover`/`signature_help`
  with their text.
- `prelude/lsp.lua`: `lsp_promise` helper; make `definition`/`declaration`/
  `type_definition`/`implementation`/`references`/`hover`/`signature_help`/
  `document_symbol`/`format` return promises. `rename` / `workspace_symbol` chain
  their `nx.ui.input` prompt into the request promise (cancel → resolve `nil`).
  `code_action` unchanged this phase (still fire-and-forget) — noted as Phase 2.
- Tests (`crates/nxvim-server/tests/lsp*`): use the LSP mock to assert
  (a) `format():next(...)` runs its continuation only after the edits apply;
  (b) `references()` resolves with the location items;
  (c) a superseded request settles its promise `nil`;
  (d) no-server resolves `nil` without hanging.

### Phase 2 — `code_action` interactive resolve  ← DONE
Threaded `cb_id` through the code-action flow: the `CodeAction` request carries it →
the `CodeActions` reply **stashes** it onto the chooser (`EditHost.code_action_cb`,
set in `show_code_actions` alongside `pending_code_action`) rather than settling →
`apply_code_action` settles it on the picked action's terminal branch (eager edit,
edit+command, or the empty "no edit" case), or hands it to `resolve_code_action` for
a lazy action so the `ResolvedCodeAction` reply settles it once the edit applies. A
cancelled chooser (`menu_results` `None` in `effects.rs`) settles `nil`; a superseding
`code_action` settles the prior chooser's promise `nil`. On the wasm edit-host (no
confirm→apply path) `show_code_actions` settles `nil` immediately. `code_action_cb` is
an **unconditional** field (not `#[cfg(native)]`) so the non-cfg-gated
`apply_code_action` compiles under `--no-default-features`.

**Mirror-refresh fix (applies to all verbs):** `settle_lsp_promise` now calls
`push_buf_mirror()` before running the resolver. The LSP-reply path (format/rename/
resolve) already got a fresh mirror at the next `run_pending` entry before its
microtask drained, but `apply_code_action` edits the buffer **mid-`run_pending`**
(after that entry push), so an eager code-action's continuation would read a stale
mirror (`vim.api.nvim_buf_get_lines` = pre-edit). Pushing in `settle_lsp_promise`
gives a uniform guarantee: a verb's continuation reads the *applied* effect. Cheap
(`changedtick`-gated).

## Non-goals
- Rejecting on transport error (resolve-only contract; revisit if needed).
- Changing the surfaces the verbs drive (jump/picker/float/menu are unchanged).
- `vim.lsp.buf.*` alias behavior beyond forwarding the new promise return.
