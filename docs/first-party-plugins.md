# First-party plugins

The surest way to learn the `nx.*` API is to read code that already ships on it.
nxvim is its own plugin API's first and most demanding consumer: its first-party
plugins use the **same public `nx.*` surface a third party would**, with no
privileged access. That makes them honest reference implementations — if a
behavior can be built in one of these, it can be built in yours.

They live in their own repositories under
[`github.com/davidrios`](https://github.com/davidrios) and are each installable
with the [built-in manager](plugin-authoring.md):

```lua
nx.plugins({
  { "davidrios/nxvim-lspconfig",
    config = function() require("nxvim-lspconfig").setup({}) end },
})
```

## The catalogue

Each is paired below with the neovim plugin it echoes and the `nx.*` surfaces
worth studying it for.

| Plugin | What it is | Read it to learn |
| ------ | ---------- | ---------------- |
| [**nxvim-lspconfig**](https://github.com/davidrios/nxvim-lspconfig) | Ready-made language-server configs (port of `nvim-lspconfig`) | The smallest, most data-driven plugin: `nx.lsp.config` / `nx.lsp.enable`, inlay hints, and the LSP buffer verbs. Start here. |
| [**nxvim-tree**](https://github.com/davidrios/nxvim-tree) | Dockable file explorer — the official tree (sibling of `nvim-tree`) | A read-only content surface with `nx.view`, docking via `nx.open` / `nx.layer`, async filesystem walks (`nx.fs` + `nx.async` / `nx.await`), decorations and `nx.hl.define`, in-view keymaps. |
| [**nxvim-keys-helper**](https://github.com/davidrios/nxvim-keys-helper) | Live popup of the keys that can follow what you've typed (a which-key) | Reacting to partial key sequences with `nx.on_key_pending`, building a floating popup with `nx.component`, debounce (`nx.utils.debounce`), and width-aware layout (`nx.str.displaywidth`). |
| [**nxvim-dap**](https://github.com/davidrios/nxvim-dap) | Debug Adapter Protocol client (sibling of `nvim-dap`) | The richest example: duplex transports (`nx.process`, `nx.socket`) carrying a framed RPC protocol, signs, a sidebar dock and REPL over `nx.view` / `nx.ui`, and cross-tick scheduling (`nx.on_next_tick`). |
| [**nxvim-diff**](https://github.com/davidrios/nxvim-diff) | Meld-style side-by-side diff viewer | Linked windows with synchronized scroll (`nx.win.set_topline` / `set_cursor`), shelling out via `nx.run`, and per-line highlight decorations. |
| [**nxvim-workspace**](https://github.com/davidrios/nxvim-workspace) | VSCode-style project workspaces | Committable project-local JSON config (`nx.json`), layout save/restore through `nx.shada`, async file IO (`nx.fs`), and `nx.user_command`. |
| [**nxvim-help**](https://github.com/davidrios/nxvim-help) | Vim-style `:help` | A navigable read-only `nx.view` surface, runtime-file lookup (`nx.runtime_file`), tag jumping, and syntax highlighting — built entirely on `nx.*` with no core changes. |

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
