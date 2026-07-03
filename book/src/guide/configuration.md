# Configuration

nxvim reads a Lua config. On startup it resolves a **config directory** — the
first of `$NXVIM_CONFIG`, `$XDG_CONFIG_HOME/nxvim`, or `~/.config/nxvim` — and
sources `<config>/init.lua` before the first frame. The **runtimepath** is that
dir plus every `pack/*/start/*` entry under it:

```
~/.config/nxvim/
├── init.lua                      # sourced at startup
└── pack/
    └── plugins/
        └── start/
            └── myplugin/       # a plugin; its lua/ and colors/ are found here
```

```lua
-- ~/.config/nxvim/init.lua
require("myplugin").setup()
```

## `nx.*` vs `vim.*`

The editor's own config API is the **`nx.*` namespace** — see the
[API reference](../api/index.md). The only `vim.*` is a closed whitelist of
**muscle-memory aliases** (`vim.g`, `vim.o`/`vim.opt`, `vim.cmd`,
`vim.keymap.set`, autocmds, `vim.notify`, and friends), each a 1:1 alias over its
`nx.*` equivalent, so config can be written in familiar spellings:

```lua
vim.g.mapleader = " "
vim.o.number = true
nx.keymap.set("n", "<leader>w", "<cmd>w<cr>", { desc = "Save" })
```

The full whitelist lives in
[ADR 0002](https://github.com/davidrios/nxvim/blob/main/docs/decisions/0002-native-plugin-system.md).
A neovim colorscheme reaches for a handful of those aliases (notably the
`nvim_set_hl` highlight helper) and nothing more.


## Plugins — the built-in `:Plugins` manager

Dropping a checkout under `pack/*/start/*` works, but the ergonomic path is the
**built-in package manager**: there is no third-party manager layer because the
manager ships with nxvim. You *declare* a set of plugins in `init.lua` with
`nx.plugins{}`; it clones/updates them over the async runtime (driving real
`git`) and loads each one — adds its directory to the runtimepath so `require`
and its `colors/` / `queries/` / `lsp/` resolve without a restart, sources its
`plugin/` scripts, and runs its `config`. Nothing blocks: every step is a
promise, so the UI paints before plugins finish loading.

```lua
-- ~/.config/nxvim/init.lua
nx.plugins({
  -- "owner/repo" shorthand expands to a GitHub clone.
  { "nxvim/nxvim-keys-helper",
    config = function() require("nxvim-keys-helper").setup({}) end },

  -- Lazy-load on a trigger: any of cmd / event / ft / keys makes it lazy.
  { "someone/markdown-tools", ft = "markdown" },

  -- Pin a ref, rename, add dependencies.
  { "owner/repo", tag = "v1.2.0", name = "repo",
    dependencies = { "owner/dep" } },
})
```

Each spec is the repo (`"owner/repo"` shorthand, or `src` / `url`, or a local
`dir`) plus optional fields: `name`, `branch`, `tag` (alias `version`),
`commit`, `dependencies` (alias `deps`), `enabled`, `init` (run before load) and
`config` (run after). Lazy triggers — `cmd`, `event`, `ft`, `keys` — defer
loading until first use; set `lazy = false` to force eager load even with a
trigger. Clones land under the data dir (not your config repo), which the
manager owns.

Run `:Plugins` to open the **dashboard** — a lazy.nvim-style floating UI listing
every declared plugin grouped by load state, with live per-plugin progress (a
spinner while a clone/pull runs, ✓/✗ on finish) and verb keymaps: `I` install ·
`U` update · `S` sync · `X` clean. The same operations are available as
ex-commands:

| Command | Action |
| --- | --- |
| `:PluginSync` | Install missing **and** update existing declared plugins |
| `:PluginInstall` | Clone any declared plugin not yet on disk |
| `:PluginUpdate` | Fast-forward every installed, unpinned plugin |
| `:PluginClean` | Remove cloned dirs no spec declares |
| `:PluginList` | Print a one-line status (installed / loaded / missing) per plugin |
| `:PluginsWelcome` | Reopen the first-run welcome checklist of recommended plugins |

nxvim ships minimal; on a fresh setup the welcome checklist offers a recommended
first-party set pre-ticked. See [Writing plugins](../plugins/authoring.md) for
authoring your own.

## Runnable examples

The [`examples/`](https://github.com/davidrios/nxvim/tree/main/examples)
directory has ~70 self-contained, end-to-end-verified configs — one per feature
(treesitter, LSP, floats, registers, tabs, mouse, statusline, completion,
picker, snippets, decor, docks, quickfix, image previews, …). Each is a config
dir you point nxvim at:

```sh
NXVIM_CONFIG=examples/treesitter cargo run -p nxvim -- examples/treesitter/sample.rs
```
