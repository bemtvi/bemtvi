# First-party plugins

The surest way to learn the `btv.*` API is to read code that already ships on it.
bemtvi is its own plugin API's first and most demanding consumer: its first-party
plugins use the **same public `btv.*` surface a third party would**, with no
privileged access. That makes them honest reference implementations — if a
behavior can be built in one of these, it can be built in yours.

They live in their own repositories under
[`github.com/bemtvi`](https://github.com/bemtvi) and are each installable
with the [built-in manager](plugin-authoring.md):

```lua
btv.plugins({
  { "bemtvi/bemtvi-lspconfig",
    config = function() require("bemtvi-lspconfig").setup({}) end },
})
```

## The catalogue

Each is paired below with the neovim plugin it echoes and the `btv.*` surfaces
worth studying it for.

| Plugin | What it is | Read it to learn |
| ------ | ---------- | ---------------- |
| [**bemtvi-lspconfig**](https://github.com/bemtvi/bemtvi-lspconfig) | Ready-made language-server configs (port of `nvim-lspconfig`) | The smallest, most data-driven plugin: curated config tables driven onto `btv.lsp.config` / `btv.lsp.enable`, inlay hints, and the LSP buffer verbs — no neovim compat layer. Start here. |
| [**bemtvi-tree**](https://github.com/bemtvi/bemtvi-tree) | Dockable file explorer — the official tree (sibling of `nvim-tree`) | A read-only `btv.view` surface, docking via `btv.layer.main` / `btv.open`, async filesystem walks with a per-directory watch (`btv.fs` + `btv.async` / `btv.await`), and glyphs / guides / git signs painted as extmarks. |
| [**bemtvi-line**](https://github.com/bemtvi/bemtvi-line) | Lualine-style statusline (sections `a`–`z`, themes, powerline separators) | Composing a rich statusline from `btv.statusline` components, recolour-by-mode themes (`btv.mode` + `btv.hl.define`), git/diff data shelled out via `btv.run`, buffer options through `btv.bo`, and refresh on a `btv.timer`. |
| [**bemtvi-keys-helper**](https://github.com/bemtvi/bemtvi-keys-helper) | Live popup of the keys that can follow what you've typed (a which-key) | Subscribing to the pending-key *oracle* (`btv.on_key_pending`) instead of intercepting input, rendering a popup with `btv.component`, debounce (`btv.utils.debounce`), and width-aware layout (`btv.str.displaywidth`). No blocking key reads. |
| [**bemtvi-dap**](https://github.com/bemtvi/bemtvi-dap) | Debug Adapter Protocol client (sibling of `nvim-dap`) | The richest example: a Content-Length-framed JSON protocol over the duplex `btv.process` primitive, breakpoint / stopped signs as extmarks, a scopes-and-stack sidebar plus REPL on read-only `btv.view` docks, and cross-tick scheduling (`btv.on_next_tick`). |
| [**bemtvi-diff**](https://github.com/bemtvi/bemtvi-diff) | Meld-style side-by-side diff viewer | Read-only `btv.view` panes locked in lockstep through `WinScrolled` + `btv.win.set_topline` / `set_leftcol` / `set_cursor`, with every line tint and intra-line span an extmark. A renderer you feed a diff to. |
| [**bemtvi-help**](https://github.com/bemtvi/bemtvi-help) | Vim-style `:help` | A navigable read-only `btv.view` split, a tag index merged across the runtimepath (`btv.runtime_file`), files read with the promise `btv.fs` API, and fuzzy topic search through the picker. |
| [**bemtvi-editorconfig**](https://github.com/bemtvi/bemtvi-editorconfig) | `.editorconfig` support (port of neovim's built-in `editorconfig.lua`) | The smallest *behavioral* plugin: an async upward directory walk on `btv.fs` inside `btv.async` / `btv.await` (`btv.utils.ancestors` + `btv.fname.modify`), option application through `btv.bo[bufnr]`, and a whole feature driven by four `btv.on` autocmds — including live reload off `BufWritePost`. |

Every one ships its own integration-test suite (most run a real server over the
black-box harness, exactly as [Testing plugins](plugin-testing.md) describes) —
those tests double as worked, runnable usage examples for the surfaces above.

## Bundled in the editor

A plugin that ships *inside* bemtvi and loads by default — also pure `btv.*`, and
browsable in this repository under
[`crates/bemtvi-lua/src/prelude/`](../crates/bemtvi-lua/src/prelude):

- [`plugins.lua`](../crates/bemtvi-lua/src/prelude/plugins.lua) +
  [`plugins_ui.lua`](../crates/bemtvi-lua/src/prelude/plugins_ui.lua) — the
  `btv.plugins` package manager and its dashboard: declarative specs, async `git`
  over `btv.process`, lazy-loading on command/event/filetype/keys, and a full
  floating UI.

## See also

- [Recommended plugins](recommended-plugins.md) — the same plugins from the *user's*
  side: what bemtvi offers on first run, what each gives you, and how to pick a subset.
- [Writing plugins](plugin-authoring.md) — the authoring guide these implement.
- [Testing plugins](plugin-testing.md) — how their test suites are structured.
- [The btv.* model](btv-model.md) — the five rules they all obey; the **btv.\* API
  Reference** chapter lists every surface named above.
