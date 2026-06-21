# 0002 — A native plugin system over `nx.*`

**Status:** accepted (2026-06-11). Records the boundary of nxvim's `vim.*`
muscle-memory aliases and the shape of its extensibility. Companion design:
[the native plugin API (`nx.*`)](../specs/2026-06-11-native-plugin-api.md).
Bounds the scope of [ADR 0001](0001-native-engines-vendored-lua-apis.md) (see
*Relationship to ADR 0001* below).

## Context

nxvim's architecture rests on three commitments (architecture.md):
`nxvim-core` is **pure and synchronous**; Lua influences the editor only
through **snapshot reads + queued effects** drained at a settle point; and the
editor is a **client-server** system where the server owns the frame and every
UI surface a client paints.

neovim plugins are imperative programs written against a different runtime
model: synchronous, re-entrant access to live editor state; blocking reads
(`getcharstr`, `vim.wait` pumping the loop); **libuv as a public API**
(`vim.uv` timers / check handles / processes); **frame-time render hooks**
(decoration providers running Lua inside redraw); and the open-ended
Vimscript-era `vim.fn` inventory. The plugins that define the ecosystem's UX —
completion menus, fuzzy pickers, statuslines, popups — are precisely the ones
that own frame time and input loops.

Hosting that model would mean reimplementing neovim's event loop and renderer
contract underneath someone else's API — surrendering the architectural
properties (a pure core, a frame no script can stall, one behavior across
front ends including the serverless wasm builds) that are the point of the
design.

## Decision

**nxvim has its own plugin system. The only `vim.*` surface is a closed
whitelist of muscle-memory aliases over `nx.*`.**

1. **Extensibility is the native provider API (`nx.*`).** The server owns
   every UI surface and the frame — the completion engine, the fuzzy picker,
   statusline segments, the snippet engine, tree docks — and plugins are
   async, declarative *providers* of data and behavior. Reads are snapshots;
   writes are queued effects; nothing blocks; no Lua runs at frame time;
   registrations are data (with generation tokens, so stale async responses
   are dropped). Because plugins influence the editor through the same queues
   RPC clients use, every registry has an RPC twin in principle —
   out-of-process providers in any language are the same surface. Design:
   [the native plugin API](../specs/2026-06-11-native-plugin-api.md).
2. **Colorschemes are nxvim's own, loaded as Lua data.** A colorscheme is pure
   data — a table of highlight-group definitions registered through the `nx`
   highlight API (its `nvim_set_hl` alias). Because it never touches the runtime
   model, it crosses the snapshot/effect boundary intact: sourcing one is just
   running Lua that fills the highlight registry. The few `vim.*` names a
   colorscheme reaches for (`nvim_set_hl`, `vim.g`, option reads) are part of
   the muscle-memory whitelist below, not a separate surface. There is no goal
   of hosting third-party neovim plugins of any other kind.
3. **Every editor API lives in the `nx.*` namespace — a clean break.** Config
   files are `nx` scripts: `init.lua` is written against the same `nx.*`
   surface plugins use (options, keymaps, events, user commands, LSP setup
   via `nx.lsp`, tree scripting via `nx.treesitter`). There is no `vim.*`
   API and no vendored neovim Lua surface: of the existing `vim.treesitter` /
   `vim.lsp` machinery, what serves nxvim's objectives is refactored into the
   `nx` API, and the rest is deleted. Per the no-silent-stubs rule, anything
   outside the shipped surface fails loud rather than approximating.
4. **A closed whitelist of muscle-memory aliases.** An enumerated set of
   `vim.*` names is kept as thin aliases of their `nx.*` equivalents. The
   admission test: high frequency in real configs, declarative or
   callback-shaped (never blocking, never frame-time), and 1:1 onto an `nx`
   primitive with no semantic emulation. The whitelist — this list is
   canonical; the other docs summarize it:
   - **Variables / options / environment:** `vim.g` / `vim.b` / `vim.w`,
     `vim.o` / `vim.opt` / `vim.opt_local` / `vim.bo` / `vim.wo`, `vim.env`.
   - **Dispatch & keymaps:** `vim.cmd`, `vim.keymap.set` / `vim.keymap.del`.
   - **Pure helpers** (shared functions, no editor contact):
     `vim.tbl_extend` / `vim.tbl_deep_extend` / `vim.tbl_contains`,
     `vim.split`, `vim.trim`, `vim.startswith` / `vim.endswith`,
     `vim.list_extend`, `vim.deepcopy`, `vim.inspect`, `vim.json`.
   - **Declarative registrations:** a **partial `vim.api` table** containing
     exactly `nvim_create_autocmd` / `nvim_create_augroup` /
     `nvim_del_autocmd` / `nvim_clear_autocmds` (→ `nx.on`),
     `nvim_create_user_command` (→ `nx.command`), and `nvim_set_hl`
     (→ `nx.hl.define`) — any other `vim.api` access fails loud — plus
     `vim.filetype.add` (→ `nx.filetype`).
   - **Callback-shaped async** (the neovim APIs already designed this way):
     `vim.notify` (→ `nx.notify`), `vim.schedule` (run at settle),
     `vim.defer_fn` (→ `nx.timer`), `vim.ui.input` / `vim.ui.select`
     (→ `nx.ui.*`), and `vim.system` in its **callback form only**
     (→ `nx.run` / `nx.run_stream`; promise-shaped, never blocking).
   - **Treesitter highlight toggle:** `vim.treesitter.start(buf, lang?)` /
     `vim.treesitter.stop(buf)` — the sole carve-out from "no `vim.treesitter`
     surface." Admitted only because they desugar 1:1 onto declarative buffer
     state, not because the namespace is: `start` sets `nx.bo.filetype` / lang
     and `nx.bo.ts_highlight = true`, `stop` sets `ts_highlight = false` and
     leaves `filetype`. nxvim models *which* language (`filetype`) and
     *whether* the native engine highlights (`ts_highlight`) as two derived
     buffer nouns — readable, idempotent, session-serializable — rather than as
     neovim's two verbs; the verbs are kept only as aliases onto those writes.
     `vim.treesitter.get_parser` / `query.*` / `highlighter.*` and the rest of
     the namespace are **not** admitted (they fail loud) — only the toggle.

   Together these cover the declarative basics — options, globals, keymaps,
   autocmds, user commands, highlights, filetypes, notify — so config can be
   written in familiar muscle-memory spellings (`vim.g.mapleader`,
   `vim.o.number = true`, a `vim.keymap.set` block, an `nvim_create_autocmd`
   block, `vim.cmd.colorscheme`) without learning a new vocabulary first.
   They are aliases, not an API:
   the same objects as `nx`, with `nx` semantics (snapshot reads, queued
   effects, settle-point callbacks), and the list grows only by deliberate
   decision — `vim.fn`, `vim.uv` / `vim.loop`, `vim.wait`, and the rest of
   `vim.api` are not part of it.
5. **First-party features are `nx` plugins — dogfood the API.** Everything that
   can reasonably be built as an `nx.*` Lua plugin is built as one. The
   UI-orchestration surfaces (picker, completion, statusline, snippets, tree
   docks) and the behavior composed on top of them ship as bundled `nx` plugins,
   not as bespoke Rust. nxvim is the plugin API's first and most demanding
   consumer; a feature that can't be expressed against `nx.*` is a gap to close
   in the API, not a reason to reach behind it. Rust keeps only what a plugin
   *cannot* be — the pure synchronous core, the frame / renderer, and the native
   engines (treesitter, LSP, regex); the orchestration and UX layered on those
   primitives are Lua. The "makes sense" carve-out is a genuine engine / frame /
   performance constraint, never mere convenience.
6. **Vimscript remains an explicit non-goal** (unchanged).

## Relationship to ADR 0001

ADR 0001 (native engines underneath, vendored neovim Lua APIs on top) is
**superseded**. Its engine half carries forward unchanged: native engines
(treesitter, LSP) drive editor behavior, synchronously where the editor needs
it, and script-driven results project into the extmark layer at the right
priority — the bridge pattern this decision generalizes into the `nx`
extension contract. Its API half does not carry forward: there is no
`vim.treesitter` or `vim.lsp` surface. The useful machinery behind them —
the treesitter parser/query/cursor primitives in Rust, the LSP client control
paths — is refactored into the `nx.*` API (`nx.treesitter`, `nx.lsp`) in
nxvim's own shape, free of the bug-for-bug-with-upstream constraint that
justified vendoring; the vendored neovim Lua itself is deleted. ADR 0001's
bridge #1 (`vim.treesitter.start` / `stop` → the engine's per-buffer override)
is the worked example of this: the override (`Editor::ts_override`) survives
unchanged, but it is now fed by **declarative buffer state** (`nx.bo.filetype`
/ `nx.bo.ts_highlight`, point 4 above) instead of an imperative `TsOp` from
vendored Lua — the engine seam is kept, the command skin is replaced by a noun.

## Consequences

- **`nx.view` widens the plugin-owned *content* surface — not the entity-mutation
  one.** The buffer-text mutation API is a single, bounded entry point —
  `nx.buf.set_lines` (alias `nvim_buf_set_lines`), an **async** whole-line write
  queued like every other effect and applied after the chunk (it returns a promise
  that settles once the edit is visible), failing loud on a read-only buffer. The rest
  of the lifecycle surface (`set_text`, `nvim_create_buf`, `nvim_buf_delete`, the
  `nvim_buf_attach` change channel) stays absent until a real need lands. But a tree
  dock needs plugin-controlled
  lines, so the panel's "read-only, plugin-owned, line-controlled buffer with a
  `<CR>` handler" is generalized off its bottom-edge assumption into `nx.view`: a
  mountable (dock / split), inert content surface whose lines a plugin replaces
  wholesale (`set_lines`), decorates via the ordinary extmark layer, and whose
  selections dispatch to `on_select`. It is a *new category of read-only content
  buffer*, owned and mutated only by the core through queued ops — not a general
  buffer-mutation API. Same shape as the directory-listing explorer buffer, lifted
  to a first-class plugin primitive.
- **The UI-orchestration surfaces split native seam / Lua feature**: the server
  owns the frame and exposes a *provider registry* (instead of rendering hooks)
  for completion, the picker, statusline segments, snippets, and tree docks; the
  orchestration and UX that drive each registry ship as **bundled `nx` plugins**
  (point 5), so the editor's own features dogfood the API. Rust gets the seam;
  Lua gets the feature. Suggested order in the spec: picker → completion →
  statusline / snippets / tree.
- **The lasting `vim.*` is exactly one thing: the muscle-memory alias
  whitelist.** No `vim.uv`, no `vim.fn` long tail, no
  vim-shaped config surface beyond the aliases. The prelude beyond those is
  donor code for the `nx` build-out: refactored under `nx.*` where it serves
  nxvim's objectives, deleted where it doesn't.
- **Plugin asynchrony lives in `nx`** (`nx.run` / `nx.timer` / `nx.fs` /
  `nx.ui.input`, callback-based). Nothing in the plugin API can block the
  editor, which also keeps the PUC Lua 5.1 backend (no yield across `pcall`)
  fully supported by construction.
- **Distribution is first-party**: manifest-declared contributions
  (activation by first use), a built-in package manager over the async
  runtime — no third-party plugin manager layer.
- **Portability holds**: a provider API that is data-and-queues composes with
  the edit-host split and the wasm builds, where an in-server imperative
  runtime cannot.
