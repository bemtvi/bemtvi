# First-party plugins

The surest way to learn the `nx.*` API is to read code that already ships on it.
nxvim is its own plugin API's first and most demanding consumer: its first-party
plugins use the **same public `nx.*` surface a third party would**, with no
privileged access. That makes them honest reference implementations — if a
behavior can be built in one of these, it can be built in yours.

They live in their own repositories under
[`github.com/nxvim`](https://github.com/nxvim) and are each installable
with the [built-in manager](plugin-authoring.md):

```lua
nx.plugins({
  { "nxvim/nxvim-lspconfig",
    config = function() require("nxvim-lspconfig").setup({}) end },
})
```

## The catalogue

Each is paired below with the neovim plugin it echoes and the `nx.*` surfaces
worth studying it for.

| Plugin | What it is | Read it to learn |
| ------ | ---------- | ---------------- |
| [**nxvim-lspconfig**](https://github.com/nxvim/nxvim-lspconfig) | Ready-made language-server configs (port of `nvim-lspconfig`) | The smallest, most data-driven plugin: curated config tables driven onto `nx.lsp.config` / `nx.lsp.enable`, inlay hints, and the LSP buffer verbs — no neovim compat layer. Start here. |
| [**nxvim-tree**](https://github.com/nxvim/nxvim-tree) | Dockable file explorer — the official tree (sibling of `nvim-tree`) | A read-only `nx.view` surface, docking via `nx.layer.main` / `nx.open`, async filesystem walks with a per-directory watch (`nx.fs` + `nx.async` / `nx.await`), and glyphs / guides / git signs painted as extmarks. |
| [**nxvim-line**](https://github.com/nxvim/nxvim-line) | Lualine-style statusline (sections `a`–`z`, themes, powerline separators) | Composing a rich statusline from `nx.statusline` components, recolour-by-mode themes (`nx.mode` + `nx.hl.define`), git/diff data shelled out via `nx.run`, buffer options through `nx.bo`, and refresh on a `nx.timer`. |
| [**nxvim-keys-helper**](https://github.com/nxvim/nxvim-keys-helper) | Live popup of the keys that can follow what you've typed (a which-key) | Subscribing to the pending-key *oracle* (`nx.on_key_pending`) instead of intercepting input, rendering a popup with `nx.component`, debounce (`nx.utils.debounce`), and width-aware layout (`nx.str.displaywidth`). No blocking key reads. |
| [**nxvim-dap**](https://github.com/nxvim/nxvim-dap) | Debug Adapter Protocol client (sibling of `nvim-dap`) | The richest example: a Content-Length-framed JSON protocol over the duplex `nx.process` primitive, breakpoint / stopped signs as extmarks, a scopes-and-stack sidebar plus REPL on read-only `nx.view` docks, and cross-tick scheduling (`nx.on_next_tick`). |
| [**nxvim-diff**](https://github.com/nxvim/nxvim-diff) | Meld-style side-by-side diff viewer | Read-only `nx.view` panes locked in lockstep through `WinScrolled` + `nx.win.set_topline` / `set_leftcol` / `set_cursor`, with every line tint and intra-line span an extmark. A renderer you feed a diff to. |
| [**nxvim-help**](https://github.com/nxvim/nxvim-help) | Vim-style `:help` | A navigable read-only `nx.view` split, a tag index merged across the runtimepath (`nx.runtime_file`), files read with the promise `nx.fs` API, and fuzzy topic search through the picker. |

Every one ships its own integration-test suite (most run a real server over the
black-box harness, exactly as [Testing plugins](plugin-testing.md) describes) —
those tests double as worked, runnable usage examples for the surfaces above.

## Bundled in the editor

A handful of plugins ship *inside* nxvim and load by default — also pure `nx.*`,
and browsable in this repository under
[`crates/nxvim-lua/src/prelude/`](../crates/nxvim-lua/src/prelude):

- [`editorconfig.lua`](../crates/nxvim-lua/src/prelude/editorconfig.lua) —
  `.editorconfig` support: an async `nx.fs` directory walk, its own glob matcher,
  and option application driven by autocmds.
- [`plugins.lua`](../crates/nxvim-lua/src/prelude/plugins.lua) +
  [`plugins_ui.lua`](../crates/nxvim-lua/src/prelude/plugins_ui.lua) — the
  `nx.plugins` package manager and its dashboard: declarative specs, async `git`
  over `nx.process`, lazy-loading on command/event/filetype/keys, and a full
  floating UI.

## See also

- [Writing plugins](plugin-authoring.md) — the authoring guide these implement.
- [Testing plugins](plugin-testing.md) — how their test suites are structured.
- [The nx.* model](nx-model.md) — the five rules they all obey; the **nx.\* API
  Reference** chapter lists every surface named above.
