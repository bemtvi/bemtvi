# The recommended plugins

nxvim ships **minimal by design** — the editor core has no bundled plugins. Everything
below is optional, installed only if you say so, and removable with one line in your
config.

On a brand-new setup nxvim offers this set once, at startup:

```
╭──  Welcome to nxvim  ────────────────────────────────────────╮
│  nxvim ships minimal by design — no bundled plugins.         │
│                                                              │
│  Install the recommended set? 12 plugins from github.com/…   │
│                                                              │
│    <CR>    Install all of them                               │
│    c       Customize — see every plugin and pick individually │
│    ?       What's in the set — opens this page in a browser  │
│    <Esc>   Skip — :PluginsWelcome reopens this any time      │
╰──────────────────────────────────────────────────────────────╯
```

- `<CR>` installs the whole set.
- `c` opens the checklist: every plugin with its **exact source**, pre-ticked, untickable
  with `<Space>` (`a` toggles all). Only what stays ticked gets installed.
- `?` opens this page.
- `<Esc>` skips. nxvim asks **at most once** ever — a marker under the data dir records
  that it asked — but `:PluginsWelcome` reopens the offer whenever you want it, and
  `:Plugins` opens the manager dashboard.

Whatever you accept is written to a managed `lua/plugins.lua` next to your `init.lua`,
as ordinary [`nx.plugins`](plugin-authoring.md) specs. From then on it is **your** config:
edit, extend, or delete any of it by hand.

## The set

Every one of these is a first-party plugin built on the public `nx.*` API — no
compatibility layer, no privileged access ([The nx.* model](nx-model.md)). Each links to
its own repository, where the full documentation lives; once `nxvim-help` is installed,
the same docs are in the editor as `:help <plugin-name>`.

| Plugin | What you get | Loaded |
| ------ | ------------ | ------ |
| [catppuccin](https://github.com/nxvim/catppuccin-nxvim) | The Catppuccin colorscheme, four flavours | at startup |
| [nxvim-line](https://github.com/nxvim/nxvim-line) | A lualine-style statusline | at startup |
| [nxvim-tree](https://github.com/nxvim/nxvim-tree) | The file-explorer sidebar | on `<leader>e` |
| [nxvim-keys-helper](https://github.com/nxvim/nxvim-keys-helper) | A which-key popup of what can follow the keys you typed | at startup |
| [nxvim-help](https://github.com/nxvim/nxvim-help) | Vim-style `:help`, including every plugin's own docs | at startup |
| [nxvim-lspconfig](https://github.com/nxvim/nxvim-lspconfig) | Ready-made configs for 407 language servers | at startup |
| [nxvim-efmls-configs](https://github.com/nxvim/nxvim-efmls-configs) | ~150 linters & formatters, via `efm-langserver` | at startup |
| [nxvim-snippets](https://github.com/nxvim/nxvim-snippets) | A snippet engine + the friendly-snippets collection | at startup |
| [nxvim-editorconfig](https://github.com/nxvim/nxvim-editorconfig) | A project's `.editorconfig` applied to its buffers | at startup |
| [nxvim-diff](https://github.com/nxvim/nxvim-diff) | Side-by-side diff & merge-conflict viewer | on `:DiffGit` |
| [nxvim-markdown-preview](https://github.com/nxvim/nxvim-markdown-preview) | Live markdown preview in your browser | at startup |
| [nxvim-dap](https://github.com/nxvim/nxvim-dap) | A debugger front end (DAP) | on `<F5>` |

"Loaded" is when the plugin's code actually runs: the lazy ones cost nothing until you
press the key or run the command that triggers them.

### Appearance

**[catppuccin](https://github.com/nxvim/catppuccin-nxvim)** — a native port of
[catppuccin/nvim](https://github.com/catppuccin/nvim): the upstream palettes and
highlight groups, rewritten against `nx.*`. Installed with
`colorscheme catppuccin` already set; `:colorscheme` switches away any time. (nxvim also
runs unmodified **neovim** colorschemes, so this is a starting point, not a lock-in.)

**[nxvim-line](https://github.com/nxvim/nxvim-line)** — configure it the way you'd
configure `lualine.nvim`: sections `a`–`z`, a component library (mode, branch, diff,
diagnostics, filename, filetype, progress, location, LSP, …), themes that recolour by
mode, powerline separators. It compiles that config down onto nxvim's native
`nx.statusline` registry, so the hot path stays in Rust.

### Getting around

**[nxvim-tree](https://github.com/nxvim/nxvim-tree)** — a lazy, watched file tree in a
dock: `<leader>e` toggles it. Full file operations, git status per entry, glob filters,
mouse support, Nerd-Font icons with an ASCII fallback.

**[nxvim-keys-helper](https://github.com/nxvim/nxvim-keys-helper)** — press a prefix
(`<leader>`, `g`, `z`, `<C-w>`) and pause: a popup lists every key that can follow, with
descriptions. It subscribes to nxvim's pending-key oracle rather than intercepting input,
so it never disturbs the sequence you're typing.

**[nxvim-help](https://github.com/nxvim/nxvim-help)** — `:help {topic}` in a read-only
split, with a fuzzy topic picker on `<leader>fh`. Its tag index is merged across the
runtimepath, which is what makes every other plugin's documentation readable in the
editor: install this one and `:help nxvim-tree` works.

### Writing code

**[nxvim-lspconfig](https://github.com/nxvim/nxvim-lspconfig)** — curated `nx.lsp`
configs for 407 language servers, driven onto nxvim's own LSP control surface. It
*configures* servers; it does **not** install them — you still install `gopls`,
`rust-analyzer`, `pyright`, and friends yourself.

**[nxvim-efmls-configs](https://github.com/nxvim/nxvim-efmls-configs)** — presets for
~150 linters and formatters (eslint, stylua, black, luacheck, …) run through
[`efm-langserver`](https://github.com/mattn/efm-langserver), which speaks their output
back over LSP. Installed with `languages = "*defaults"`, so the bundled default tool per
filetype is wired lazily on first open. Needs the `efm-langserver` binary, plus whichever
tools you want it to run.

**[nxvim-snippets](https://github.com/nxvim/nxvim-snippets)** — the LSP snippet grammar
(tabstops, placeholders, choices, mirrors, transforms) with VSCode-format collection
loading. It installs
[friendly-snippets](https://github.com/rafamadriz/friendly-snippets) as a dependency, so
there are snippets for most languages from the start: type a trigger, accept the
completion row with `<C-y>`, jump between tabstops with `<C-j>` / `<C-k>`.

**[nxvim-editorconfig](https://github.com/nxvim/nxvim-editorconfig)** — reads the
`.editorconfig` files above each file you open and applies indentation, line-ending and
charset settings to that buffer. No configuration; switch it off per buffer or globally
with `vim.g.editorconfig = false`.

### Reviewing and debugging

**[nxvim-diff](https://github.com/nxvim/nxvim-diff)** — `:DiffGit` diffs the current file
against git `HEAD`; `:DiffConflict` opens conflict markers as a 3-way diff. Panes scroll
in lockstep, changed lines are tinted, and the Lua API renders any 2- or 3-pane spec you
build.

**[nxvim-markdown-preview](https://github.com/nxvim/nxvim-markdown-preview)** —
`:MarkdownPreview` serves your open markdown buffers over the editor's own HTTP mount and
opens them in your browser, re-rendering as you type (no `:w` needed), with highlighted
code fences and mermaid diagrams. Because it's a mount rather than a bound port, it works
the same in the [browser build](browser-editor.md).

**[nxvim-dap](https://github.com/nxvim/nxvim-dap)** — a Debug Adapter Protocol client:
breakpoints (conditional / hit / log), stepping, a scopes-stack-watches sidebar with
inline value editing, a REPL, and multiple concurrent sessions. `<F5>` or
`:DapToggleBreakpoint` loads it. You supply the adapter (debugpy, codelldb, …) and a
configuration per filetype — its README shows both.

## What you still need to install yourself

Plugins configure tools; they don't ship them. Depending on what you accepted:

- **Language servers** for nxvim-lspconfig (`gopls`, `rust-analyzer`, `lua-language-server`, …).
- **`efm-langserver`** plus the linters/formatters you want, for nxvim-efmls-configs.
- **Debug adapters** for nxvim-dap.
- A **Nerd Font** for the tree's file icons (it falls back to ASCII without one).

Everything else — the picker, completion, tree-sitter highlighting, LSP itself,
quickfix, terminals, docks — is already in nxvim. See
[What nxvim adds](features.md).

## Changing your mind

The set is not special after installation: it is ordinary spec data in your own
`lua/plugins.lua`.

To drop one, delete its entry from `lua/plugins.lua` and run `:PluginClean` (which
removes the checkouts nothing declares any more). To add anything else, declare it the
same way and run `:PluginSync`:

```lua
nx.plugins({
  { "owner/repo", config = function() require("repo").setup() end },
})
```

- `:Plugins` — the dashboard: what's declared, installed, loaded, or drifted, with
  install / update / sync / restore / clean.
- `:PluginsWelcome` — reopen the offer and re-pick from the recommended set. Note it
  **overwrites** the managed `lua/plugins.lua` with your new choice.

To start from nothing, skip the offer (`<Esc>`) — or, if you're writing a config for
others, register your own set with `nx.plugins.recommend{...}`, which replaces nxvim's
default one; `nx.plugins.recommend({})` suppresses the offer entirely.

## Beyond the set

Not offered on first run, but first-party and worth knowing about:

- [**nxvim-workspaces**](https://github.com/nxvim/nxvim-workspaces) — per-project editor
  configuration as a committable `.nxvim/config.json` (language servers, save actions,
  options) instead of hand-written rc Lua. See [Workspaces](features/workspaces.md).
- [**nxvim-remotes**](https://github.com/nxvim/nxvim-remotes) — teaches `:connect` the
  `ssh://` and `container://` schemes, so the filesystem, processes and language servers
  live on a remote while the editing stays local.
- [**nxvim-lspconfig-base**](https://github.com/nxvim/nxvim-lspconfig-base) — the same
  LSP configs trimmed to the most widely-used servers; a smaller alternative to
  nxvim-lspconfig, not a companion to it.

## See also

- [First-party plugins](first-party-plugins.md) — the same plugins read as reference
  implementations: which `nx.*` surfaces each one is worth studying for.
- [Writing plugins](plugin-authoring.md) — the manager's full spec vocabulary
  (lazy triggers, dependencies, pinning, the lockfile) and the authoring guide.
