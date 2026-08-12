# `btv.lsp.code_action(opts)` — kind filter + auto-apply

**Status:** phase 1 (only phase).

`btv.lsp.code_action()` today is unconditionally *interactive*: it requests every action
at the cursor and opens the chooser. That makes it unusable as a **save action** — the
canonical `[organize imports / fixAll, format]` on-write chain — because a chooser pops
on every write, and because there is no way to say *which* action you mean.

Add the two neovim-shaped options that make it non-interactive when it can be:

```lua
btv.lsp.code_action({ context = { only = { "source.fixAll" } }, apply = true })
```

and, declaratively (the shape `bemtvi-workspaces` puts in `.bemtvi/config.json`):

```json
{ "require": "btv.lsp", "method": "code_action",
  "params": { "context": { "only": ["source.fixAll"] }, "apply": true } }
```

## Semantics

- **`context.only`** — a list of code-action kinds. Sent as `context.only` on the
  `textDocument/codeAction` request **and** applied to the reply client-side, so a server
  that ignores `only` (it is a should, not a must) can't turn an `apply` into a chooser.
  Matching follows the LSP kind hierarchy: `"source.fixAll"` matches the kind
  `source.fixAll` *and* `source.fixAll.ruff`. An action with no `kind` never matches a
  filter (it can't be proven to be the one asked for).
- **`apply`** — differentiate a **one-shot** action from one with **options**, which is
  the whole point of the flag:
  - exactly **one** action survives the filter ⇒ apply it directly, no chooser;
  - **more than one** ⇒ open the chooser as usual (the user still picks);
  - **none** ⇒ the existing "No code actions available" message, promise resolves `nil`.
- The returned promise is unchanged: it resolves once the applied action's edit lands
  (through `codeAction/resolve` if the action is lazy), or `nil` on cancel/no-match.
- Unsupported neovim keys (`filter`, `range`, `context.diagnostics`, `context.triggerKind`)
  **fail loud** rather than being silently ignored.

## Touchpoints

| crate | change |
| ----- | ------ |
| `bemtvi-lsp/protocol.rs` | `LspRequest::CodeAction { .., only: Vec<String> }`; `CodeActionData { .., kind: Option<String> }` |
| `bemtvi-lsp/convert.rs` | carry each action's `kind` through the distillation |
| `bemtvi-lsp/dispatch.rs` | send `context.only` (native async client) |
| `bemtvi-lsp/sync_client.rs` | send `context.only` (wasm sync client) |
| `bemtvi-lsp/mock.rs` | **react to** `only`: filter the scripted actions by it; `code_action_ignore_only` scripts a non-compliant server so the client-side filter is testable |
| `bemtvi-server/lsp/mod.rs` | `CodeActionOpts { only, apply }` on `PendingLspReq` (the reply needs both) |
| `bemtvi-server/lsp/request.rs` | `request_lsp_code_action(cb_id, opts)`; carry the opts to the reply |
| `bemtvi-server/lsp/edit.rs` | `show_code_actions` filters, then one-shot-applies or opens the chooser |
| `bemtvi-lua/ops.rs`, `install.rs` | `LspOp::CodeAction { cb_id, only, apply }`; `btv._lsp_buf_code_action(cb_id, only, apply)` |
| `bemtvi-lua/prelude/lsp.lua` | `btv.lsp.code_action(opts)` — parse/validate, fail loud on unsupported keys |

`:LspCodeAction` keeps its no-filter, always-interactive behavior (`opts` default).

## Tests (`crates/bemtvi/tests/lsp_features.rs`, mock LSP)

1. `only` reaches the server — the mock filters on it, so a scripted `quickfix` action is
   *not* offered when `only = {"source.fixAll"}`.
2. `apply = true` with a single match applies the edit with **no menu open**.
3. `apply = true` with two matches opens the chooser (options, not one-shot).
4. Client-side filter: with `code_action_ignore_only` the mock returns everything anyway;
   `only` + `apply` still applies the right one directly.
5. Kind hierarchy: `only = {"source.fixAll"}` matches a `source.fixAll.ruff` action.
6. No match ⇒ "No code actions available", promise resolves `nil`, buffer untouched.
7. The chain still sequences: `code_action({..., apply=true}):next(format)`.
8. A bad `opts` (`filter`, top-level `only`) fails loud.
