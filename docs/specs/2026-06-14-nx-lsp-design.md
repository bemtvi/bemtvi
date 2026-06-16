# `nx.lsp` — the native LSP control surface

**Status:** **Phase A landed (2026-06-16); Phases B–C pending.** Re-introduces
nxvim's LSP *control surface* in the `nx.*` namespace, after the nx break
([`e9bb90c`](#background)) deleted the old `vim.lsp` Lua clone. The LSP
**engine** is untouched and carries forward whole: the
[`nxvim-lsp` crate + the `lsp/` server subtree](2026-06-02-lsp-support-design.md)
and every one of its engine Decisions 1–7. This document is the
[ADR 0002](../decisions/0002-native-plugin-system.md) / [native plugin
API](2026-06-11-native-plugin-api.md) application to LSP: *what the user and
plugins write*, and how it drives the intact engine seam.

It supersedes the Lua-surface half of
[the LSP support design](2026-06-02-lsp-support-design.md) (its Phase 7 —
`vim.lsp.*` / `vim.lsp.buf.*` / `vim.lsp.config` / lspconfig compat). The
engine half of that document (Decisions 1–7, the `nxvim-lsp` crate, the
per-buffer server state, document sync over the edit journal) is **unchanged
and authoritative**; this spec does not restate it.

## Background

Commit `e9bb90c` ("canonical nx.* namespace") completed the ADR 0002 break by
deleting the vendored neovim Lua surface: `prelude/lsp.lua` (the `vim.lsp.*`
clone, ~1200 lines), `prelude/nvim_api.lua`, and `prelude/compat.lua` (the
lspconfig compatibility layer). It left intact:

- **`nxvim-lsp`** — `LspManager`, the per-`(language, root)` child servers, the
  `LspCommand`/`LspEvent` channel pair, `ProviderCaps`, the protocol layer.
- **`nxvim-server/src/lsp/`** — `sync` / `completion` / `diagnostics` / `edit` /
  `inlay` / `semantic` / `request`, still declared (`mod lsp`) and driven in the
  event loop (`on_lsp_events`).
- **The queued-effect seam** — `LspOp` in `nxvim-lua/src/ops.rs`
  (`Start`, `BufRequest{kind}`, `Format`, `Rename`, `CodeAction`,
  `ClientRequest`, `ClientNotify`, `ApplyWorkspaceEdit`, `ShowDocument`,
  `SemanticTokensEnable`/`Config`/`Refresh`, `InlayHintEnable`). Its
  doc-comments still name `vim.lsp.*`; the seam itself is exactly what this
  surface drives. *(The comments get re-pointed at `nx.lsp` as part of Phase A.)*
- **`nx.diagnostic`** — already migrated and nx-native (`get` / `goto_next` /
  `goto_prev` / `setloclist` / `open_float` / `config` / `set` / `reset`), fed by
  `textDocument/publishDiagnostics`. **The diagnostics surface is done** and out
  of scope here except where LSP feeds it.

So "bring back LSP" is **re-introducing the control surface, nx-shaped, against
an engine that never left.** The dangling `_lsp_buf_*` fail-loud labels in
`prelude/state.lua` are placeholders left for this surface (Phase A renames them
to the flattened verbs below).

## Guiding principles (from ADR 0002 + the plugin-API rules)

1. **Registrations are data** (plugin-API rule 5). Server configs are a
   declarative, mergeable registry — built with code if you like (a *dynamic
   declaration* is still a declaration), not an imperative start sequence.
2. **Nothing blocks** (rule 3). Root resolution, the rename prompt, the
   code-action pick are all callback-shaped. No `getcharstr`, no `vim.wait`. This
   also keeps the PUC Lua 5.1 backend supported by construction.
3. **Prefer the noun** (the treesitter precedent, ADR 0002 §4 / plugin-API
   *"Treesitter highlighting is buffer state"*). Semantic tokens and inlay hints
   are **buffer state**, not start/stop verbs — introspectable, idempotent,
   shada-serializable.
4. **Dogfood the shared engines** (ADR 0002 §5). LSP does **not** own UI. Its
   results flow into the server-owned surfaces: hover → `nx.ui.float`, locations
   → `nx.picker`, code actions → `nx.ui.select`, completion → `nx.complete`,
   diagnostics → `nx.diagnostic` / the decoration layer.
5. **Fail loud** (no-silent-stubs). A request for a capability the server doesn't
   advertise echoes an error naming the method; it never silently no-ops.

## The surface

### Server configuration — `nx.lsp.config` / `nx.lsp.enable`

An **accumulating, deep-merged registry**, modeled on neovim's `vim.lsp.config`
(the user explicitly prefers it: configs compose across files/plugins, and a
config can be built dynamically). Function-call form only — no
`nx.lsp.config[name] = {…}` table-assignment sugar.

```lua
-- defaults inherited by every server (the "*" base)
nx.lsp.config("*", {
  capabilities = my_caps,
  root_markers = { ".git" },
})

-- a named server; call as many times as you like — each call deep-merges
nx.lsp.config("rust_analyzer", {
  cmd          = { "rust-analyzer" },
  filetypes    = { "rust" },
  root_markers = { "Cargo.toml", "rust-project.json" },
})
nx.lsp.config("rust_analyzer", {                       -- merges into the above
  settings = { ["rust-analyzer"] = { check = { command = "clippy" } } },
})

nx.lsp.enable({ "rust_analyzer", "lua_ls" })           -- activate
```

**Merge semantics: exactly neovim's** — `vim.tbl_deep_extend("force", …)`:
maps merge recursively, **lists replace** (a later `filetypes` overwrites, not
appends), scalars overwrite. The resolved config a server starts with is
`"*"` ⊕ `config(name, …)` (in call order) — computed in Lua at `enable`/attach
time, so the `LspOp::Start` seam underneath still receives **one
fully-resolved** config and is unchanged.

`enable(names)` registers the named servers' filetype triggers; **the engine
owns the FileType → start dispatch** (neovim wires an internal autocmd here —
nxvim keeps it declarative engine state instead, so it behaves identically under
the wasm edit-host). On the first buffer whose `filetype` matches an enabled
server, the engine resolves the root and fires `LspOp::Start`. `nx.lsp.disable`
is the inverse.

**Root resolution is async** (principle 2): `root_markers` is an upward search
the engine runs through the fs seam (works under the wasm edit-host), or
`root_dir = function(bufnr, done) … done(dir) end` for the callback escape
hatch. `None` ⇒ the file's directory, as today.

**Bundled presets ship as pre-registered `config()` data** — an nx-native
replacement for lspconfig, reusing the existing `lsp/<server>.lua` config-file
pattern (`nxvim-lua/src/luafs.rs` already sources these). A user only
`enable("rust_analyzer")`s; `cmd` / `filetypes` / `root_markers` come from the
preset and can be overridden by their own `config()` call.

`nx.lsp.start(cfg, { bufnr })` remains available as the low-level, un-merged
direct start (the raw `LspOp::Start`) for advanced/manual use.

### Language features — flattened verbs, server-owned UI

Flattened under `nx.lsp.*` (the user's choice — no `.buf` sub-namespace; nxvim
has no competing `nx.lsp.win`/etc. grouping to disambiguate). Each is a thin
verb over the surviving op; the **server decides where the answer lands**:

| Verb | Op | Result surface |
| --- | --- | --- |
| `nx.lsp.hover()` | `BufRequest{Hover}` | `nx.ui.float` |
| `nx.lsp.signature_help()` | `BufRequest{SignatureHelp}` | `nx.ui.float` |
| `nx.lsp.definition()` | `BufRequest{Definition}` | 1 hit → jump; N → `nx.picker` |
| `nx.lsp.declaration()` | `BufRequest{Declaration}` | ″ |
| `nx.lsp.type_definition()` | `BufRequest{TypeDefinition}` | ″ |
| `nx.lsp.implementation()` | `BufRequest{Implementation}` | ″ |
| `nx.lsp.references()` | `BufRequest{References}` | `nx.picker` |
| `nx.lsp.rename(name?)` | `Rename{new_name}` | `name` nil → `nx.ui.input` prompt, then apply `WorkspaceEdit` |
| `nx.lsp.code_action()` | `CodeAction` | `nx.ui.select` of actions; apply on choose |
| `nx.lsp.format()` | `Format` | apply edits |
| `nx.lsp.document_symbol()` | *new* `BufRequest{DocumentSymbol}` | `nx.picker` |
| `nx.lsp.workspace_symbol(q)` | *new* `BufRequest{WorkspaceSymbol}` | `nx.picker` |

The first seven `BufRequest` kinds (Definition, Declaration, TypeDefinition,
Implementation, References, Hover, SignatureHelp) plus `Rename` / `CodeAction` /
`Format` **already exist** in `LspReqKind` / `LspOp` — Phase A drives them with
no engine change. `document_symbol` / `workspace_symbol` need a new
`LspReqKind` + `LspRequest` variant pair (small, additive); they are most useful
*with* a picker anyway, so they land in **Phase C** alongside the picker, not
Phase A.

The point of departure from neovim: there each verb builds its own quickfix /
float in Lua; here they are verbs over `LspOp::BufRequest{kind}` and the **server
owns the surface**. Until `nx.picker` lands (build-order step 2), multi-location
results use the existing panel/loclist path — the same code as today, re-pointed
to the picker in Phase C with no API change.

Verbs are user-bound (typically in `on_attach`):

```lua
nx.lsp.config("*", {
  on_attach = function(client, bufnr)            -- runs at settle, never at frame time
    local function map(k, fn) nx.keymap.set("n", k, fn, { buffer = bufnr }) end
    map("gd", nx.lsp.definition)
    map("gr", nx.lsp.references)
    map("K",  nx.lsp.hover)
    map("<leader>rn", nx.lsp.rename)
    map("<leader>ca", nx.lsp.code_action)
  end,
})
```

### Completion — a built-in `nx.complete` source

LSP completion is **not** a verb here; it is the built-in `"lsp"` source of the
`nx.complete` engine (native plugin API, worked example 1), fanned out with
generation tokens. There is no `nx.lsp.completion` / omnifunc. Lands with the
completion engine (build-order step 3); see Phase C.

### Semantic tokens & inlay hints — buffer nouns, not verbs

Direct application of principle 3 (the treesitter precedent). The neovim verbs
become declarative buffer/editor options the engine reads:

| Option | Replaces (neovim) | Backed by op |
| --- | --- | --- |
| `nx.bo.lsp_semantic_tokens` (per-buffer) | `vim.lsp.semantic_tokens.start` / `stop` | `SemanticTokensEnable` |
| `nx.o.lsp_semantic_tokens` (editor-wide gate, default on) | `…semantic_tokens.enable(bool)` | `SemanticTokensConfig` |
| `nx.bo.lsp_inlay_hints` (per-buffer, default off) | `vim.lsp.inlay_hint.enable(bool, {bufnr})` | `InlayHintEnable` |

Writing the option drives the op; reading it answers "is this on for this
buffer?" and round-trips through shada/session. The engine projection
(`lsp/semantic.rs`, `lsp/inlay.rs`) is unchanged — no Lua at frame time. A
manual refresh stays a verb (`nx.lsp.semantic_tokens_refresh(bufnr)` →
`SemanticTokensRefresh`) since "drop the cache and re-request" has no readable
state to model as a noun.

### Diagnostics

Already `nx.diagnostic.*` and nx-native. LSP `publishDiagnostics` feeds it; this
spec adds nothing. `nx.lsp.*` and `nx.diagnostic.*` stay distinct namespaces (as
in neovim), since diagnostics outlive any LSP client (a linter or
`nx.diagnostic.set` can publish without a server).

### Introspection & escape hatch

```lua
local clients = nx.lsp.clients({ bufnr = 0 })          -- snapshot tables (reads the mirror)
local c = clients[1]
c:request("workspace/executeCommand", params, function(err, res) … end)  -- ClientRequest
c:notify("$/setTrace", { value = "verbose" })          -- ClientNotify
```

Client handles are **snapshots** carrying `:request` (callback-shaped, generation
token, stale replies dropped — engine Decision 3) and `:notify`. `on_attach`
receives the same handle. `nx.lsp.request`/`nx.lsp.notify` are sugar resolving
the buffer's primary client. These are the generic path for protocol features
nxvim has no first-class verb for; an unimplemented or uncapable request fails
loud (principle 5).

## Mapping to the engine seam (nothing new in `nxvim-lsp`)

Every Phase A/B verb and option above drives an **existing** `LspOp` variant
(the seven position-family `LspReqKind`s, `Format` / `Rename` / `CodeAction`, the
semantic-token / inlay-hint toggles) — **no engine change**. The only feature
needing an additive engine change is document/workspace symbols (a new
`LspReqKind` + `LspRequest` pair), deferred to Phase C with the picker. The Phase
A/B work is: (1) a new `prelude/lsp.lua` that builds the merge registry and
queues the right ops, (2) re-point the `LspOp` doc-comments and the `_lsp_buf_*`
placeholders, (3) the buffer-option nouns in `options.rs` wired to the toggle
ops. The `async-lsp`-driven manager, the
per-server loops, document sync over the edit journal, and the reply-as-event
generation tokens (engine Decisions 1–7) are untouched.

## Phases

Riding the [native plugin API](2026-06-11-native-plugin-api.md) build order
(`nx.lsp` is step 1; picker is step 2; completion is step 3):

- **Phase A — parity, on machinery that all exists. _(done 2026-06-16.)_** New
  `prelude/lsp.lua`: `nx.lsp.config` (merge registry, `"*"` base, neovim
  deep-merge), `nx.lsp.enable` / `disable` with engine-side FileType dispatch, the
  flattened language verbs routing to **today's** surfaces (jump / loclist / float /
  `nx.ui.select` / `nx.ui.input`), `nx.lsp.request` / `notify` / `clients`,
  `on_attach` / `on_init` / `on_exit`. Restores the deleted feature set, nx-shaped.
  Mostly Lua over the intact `LspOp` seam; bundled presets re-land as `config()`
  data. The mirror hooks (`_set_client` / `_run_on_init` / `_run_on_exit` /
  `_remove_client`) that `nxvim-server` hard-calls are now defined (they were
  dangling). Covered by `crates/nxvim/tests/lsp_config.rs` (merge precedence,
  enable→FileType→Start, a verb's reply on its surface, `"*"` inheritance,
  `on_attach` + `clients`, no-client request fails loud).
- **Phase B — the nouns.** `nx.bo.lsp_semantic_tokens` / `nx.o.lsp_semantic_tokens`
  / `nx.bo.lsp_inlay_hints` in `options.rs`, wired to the toggle ops; the verbs
  survive only as the option writes.
- **Phase C — engine integration.** Re-route locations into `nx.picker` and add
  the `"lsp"` source to `nx.complete`, as those engines land (steps 2–3) — no
  `nx.lsp` API change for the existing verbs, only the result surface moves. This
  is also where `document_symbol` / `workspace_symbol` land (the new
  `LspReqKind` + `LspRequest` pair), since symbols want the picker.

## Testing (black-box, per the no-unit-test rule)

Drive keys / `exec_lua` against a real server over the in-process pipe; assert on
`nvim_buf_get_lines` / cursor / the `redraw` view. `nxvim-lsp::mock` stands in
for a language server (the deleted LSP suites used exactly this), so tests are
hermetic and need no `rust-analyzer` on the box. Coverage to re-land with each
phase:

- **A:** `config` merge precedence (`"*"` ⊕ named, repeated calls, list-replace);
  `enable` → FileType → `Start` for a matching buffer; each verb's op fires and
  its result lands on the expected surface; `rename` prompt flow; `code_action`
  select flow; capability-absent request fails loud.
- **B:** the option nouns toggle the projection on/off and round-trip through a
  read; the editor-wide gate hides paint everywhere.
- **C:** an LSP location opens the picker; the `"lsp"` completion source produces
  matched menu entries live.

## Open questions

- **`config("*", …)` vs. per-filetype defaults** — neovim has only the global
  `"*"`; do we also want a `config` keyed by filetype group? (Deferred; `"*"` +
  named covers the common case.)
- **`enable` granularity** — buffer-local enable (`enable(names, { bufnr })`) in
  addition to global? Neovim added this recently; low priority.
