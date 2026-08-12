# The btv.* model

bemtvi has its **own**, purpose-built plugin system. Every editor API lives in the
`btv.*` namespace (config and plugins alike), with a small set of familiar `vim.*`
names aliased to `btv.*` for convenience. This page explains
the model that shapes the whole API; [Writing plugins](plugin-authoring.md) is the
hands-on how-to, and the **btv.\* API Reference** chapter lists every function.

The model in one sentence:

> **The server owns every UI surface and the frame; plugins are async, declarative
> *providers* of data and behavior.**

Where neovim hands you buffer primitives and redraw hooks and says *"draw your own
completion menu,"* bemtvi hands you a completion engine and says *"give it items."*

## Compared to neovim's plugin model

neovim's plugin model fits neovim's architecture. A plugin there is an **imperative
program written against the editor's runtime** — synchronous, re-entrant editor
access; blocking reads (`getcharstr`, `vim.wait` pumping the loop); libuv as a public
API (`vim.uv` timers / processes); frame-time render hooks (decoration providers
running Lua inside redraw); and the open-ended `vim.fn` inventory. The plugins that
define that ecosystem's UX — completion menus, fuzzy pickers, statuslines, popups,
tree sidebars — **own frame time and input loops**, and neovim's single-process,
synchronous core is built to let them.

bemtvi's architecture is different (see [Architecture](architecture.md)), so its
plugin model is too. `bemtvi-core` is pure and synchronous, Lua influences the editor
only through **snapshot reads + queued effects** drained at a settle point, and a
client-server boundary puts the **server in charge of the frame** and every UI
surface. Those properties — a pure core, a frame no script can stall, identical
behavior across every front end including the serverless WebAssembly build — are the
point of the design, and they call for plugins that are **async, declarative
providers** rather than imperative programs. Same goal (rich, scriptable UX); a model
shaped by a different runtime.

## The five rules

The model is five rules — each one a property the architecture already enforces
internally; the API just makes it the documented contract:

1. **Reads are snapshots.** `btv.buf.lines(b)` and friends read the state pushed at
   Lua entry. Documented, not disguised as live access.
2. **Writes are queued effects.** Applied at the settle point, not instantly. The
   buffer mutations (`btv.buf.set_lines` / `btv.buf.set_text`) validate loud up
   front, queue the edit, and return a promise that fulfils once it has landed.
3. **Nothing blocks, ever.** No wait-pumps, no blocking reads, no uv handles.
   Anything that waits returns a **promise** you `btv.await` inside `btv.async`, or —
   for streaming — an async-iterator (`btv.run_stream` + `btv.await_each`). See
   [Async & promises](async.md).
4. **No frame-time Lua.** Plugins publish decorations / segments / items whenever
   they like; the server folds them into the next frame. **A plugin cannot make
   redraw slow.**
5. **Registrations are data.** A provider registers with a name + schema and is
   called with a context carrying a **generation token**; it emits through the
   context's sink (`ctx.push`) and signals completion by returning. Stale async
   responses are dropped by the engine.

Because Lua influences the editor through the **same queues RPC clients use**, every
`btv.*` registration has an RPC twin in principle — out-of-process providers, in any
language, are the same surface (later). The in-process Lua host is v1.

## Providers, not programs

The inversion rule 5 implies: the **server owns the engine**, and a plugin is a thin
*source* / *segment* / *provider* plugged into it. This is what keeps plugins small
and the frame safe — the hot path (rendering, navigation, matching, input grab)
lives in Rust, written once, and the plugin only supplies data.

| The neovim "shape" | In bemtvi |
| --- | --- |
| Completion menu (the nvim-cmp shape) | `btv.complete` engine; plugins are **sources** |
| Statusline (the lualine shape) | `btv.statusline`; plugins register **segments** |
| Fuzzy finder (the telescope shape) | `btv.picker` engine; plugins are **sources** |
| Snippets (the LuaSnip shape) | `btv.snippet` engine (LSP grammar, tabstops) |
| File tree / sidebar / dashboard — a bespoke plugin UI in neovim | First-class content surfaces: `btv.view` in an `btv.dock`, `btv.component` for reactive ones |
| Decoration provider | `btv.decor` — viewport-scoped, recomputed off the frame |

The same applies to the UI itself. In neovim, plugin UIs are **bespoke** — a file
tree, a popup, a dashboard is each hand-built from buffer and window primitives, so
every plugin reinvents rendering, navigation, and input handling. bemtvi ships those
as **first-class APIs**: [`btv.view`](features/ui-primitives.md) (dockable content
surfaces), floating windows, the `btv.ui` widgets, and the reactive `btv.component` —
so a plugin *describes* a UI instead of drawing one.

## vim.* aliases

The editor API is `btv.*`. For convenience, a small set of familiar `vim.*` names are
provided as **thin aliases** over their `btv.*` equivalents, so common config reads in
muscle-memory spellings — `vim.g.mapleader`, `vim.o.number = true`, a
`vim.keymap.set` block, an `nvim_create_autocmd` block, `vim.cmd.colorscheme` —
without learning a new vocabulary first. They're aliases, not a second API: the same
objects, with `btv` semantics (snapshot reads, queued effects, settle-point
callbacks).

The aliased names ([ADR 0002](decisions/0002-native-plugin-system.md) has the
canonical list):

- **Variables / options / env** — `vim.g` / `vim.b` / `vim.w`, `vim.o` / `vim.opt` /
  `vim.opt_local` / `vim.bo` / `vim.wo`, `vim.env`.
- **Dispatch & keymaps** — `vim.cmd`, `vim.keymap.set` / `del`.
- **Pure helpers** — `vim.tbl_*`, `vim.split`, `vim.trim`, `vim.startswith` /
  `endswith`, `vim.list_extend`, `vim.deepcopy`, `vim.inspect`, `vim.json`.
- **Declarative registrations** — a *partial* `vim.api` of `nvim_create_autocmd` /
  `augroup` / `del` / `clear` (→ `btv.on`), `nvim_create_user_command`
  (→ `btv.command`), and `nvim_set_hl` (→ `btv.hl.define`).
- **Callback-shaped async** — `vim.notify`, `vim.schedule`, `vim.defer_fn`, and
  `vim.ui.input` / `select` (process spawning is `btv.run` / `btv.run_stream` —
  there is no `vim.system`).
- **Treesitter foldexpr** — `vim.treesitter.foldexpr`, neovim's spelling of the
  native `'foldexpr'` marker (`btv.treesitter.foldexpr`). Highlighting itself is
  declarative buffer state — the `btv.bo.filetype` / `btv.bo.ts_highlight` nouns —
  not a function call.

The list is intentionally small — these convenience spellings, and nothing more;
everything else (LSP, treesitter, processes, the filesystem, …) is `btv.*`.

### Colorschemes are data, not plugins

A colorscheme is **pure data** — a table of highlight-group definitions, registered
through the `btv` highlight API (the `nvim_set_hl` alias above). It never touches the
runtime model, so it crosses the snapshot/effect boundary intact: sourcing one is
just running Lua that fills the highlight registry. It uses the same `vim.*` aliases
as any other config — there's no plugin host and no special case.

## Dogfooding the btv.* API

The split: the **core provides primitives** — the engines and UI surfaces, in Rust:
completion, the picker (a float-list widget), statusline, snippets, `btv.view` /
`btv.dock` / floats, `btv.decor`, plus the tree-sitter / LSP / regex engines — and the
**more complex UI behavior is implemented as plugins** in Lua, composing those
primitives. A file explorer, a which-key popup, statusline extras, a completion or
picker source: each is a plugin, not bespoke Rust.

bemtvi is the plugin API's first and most demanding consumer — its first-party plugins
use the same public `btv.*` API a third party would, with no privileged access. That
keeps the API honest: a feature that can't be expressed against `btv.*` is a gap to
close in it.

## See also

- [Writing plugins](plugin-authoring.md) — the hands-on authoring guide.
- [Async & promises](async.md) — rule 3 in practice (`btv.promise` / `async`/`await`).
- [ADR 0002 — native plugin system](decisions/0002-native-plugin-system.md) — the
  decision record and the canonical list of `vim.*` aliases.
- [The native plugin API design sketch](specs/2026-06-11-native-plugin-api.md) — the
  full surface, the five rules, and six worked examples.
