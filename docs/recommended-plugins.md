# The recommended plugins

bemtvi ships **minimal by design** — the editor core has no bundled plugins. Everything
below is optional, installed only if you say so, and removable with one line in your
config.

On a brand-new setup bemtvi offers this set once, at startup:

```
╭──  Welcome to bemtvi  ────────────────────────────────────────╮
│  bemtvi ships minimal by design — no bundled plugins.         │
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
- `<Esc>` skips. bemtvi asks **at most once** ever — a marker under the data dir records
  that it asked — but `:PluginsWelcome` reopens the offer whenever you want it, and
  `:Plugins` opens the manager dashboard.

Whatever you accept is written to a managed `lua/plugins.lua` next to your `init.lua`,
as ordinary [`btv.plugins`](plugin-authoring.md) specs. From then on it is **your** config:
edit, extend, or delete any of it by hand.

## The set

Every one of these is a first-party plugin built on the public `btv.*` API — no
compatibility layer, no privileged access ([The btv.* model](btv-model.md)). Each links to
its own repository, where the full documentation lives; once `bemtvi-help` is installed,
the same docs are in the editor as `:help <plugin-name>`.

| Plugin | What you get | Loaded |
| ------ | ------------ | ------ |
| [catppuccin](https://github.com/bemtvi/catppuccin-bemtvi) | The Catppuccin colorscheme, four flavours | at startup |
| [bemtvi-line](https://github.com/bemtvi/bemtvi-line) | A lualine-style statusline | at startup |
| [bemtvi-tree](https://github.com/bemtvi/bemtvi-tree) | The file-explorer sidebar | on `<leader>e` |
| [bemtvi-keys-helper](https://github.com/bemtvi/bemtvi-keys-helper) | A which-key popup of what can follow the keys you typed | at startup |
| [bemtvi-help](https://github.com/bemtvi/bemtvi-help) | Vim-style `:help`, including every plugin's own docs | at startup |
| [bemtvi-lspconfig](https://github.com/bemtvi/bemtvi-lspconfig) | Ready-made configs for 407 language servers | at startup |
| [bemtvi-efmls-configs](https://github.com/bemtvi/bemtvi-efmls-configs) | ~150 linters & formatters, via `efm-langserver` | at startup |
| [bemtvi-snippets](https://github.com/bemtvi/bemtvi-snippets) | A snippet engine + the friendly-snippets collection | at startup |
| [bemtvi-editorconfig](https://github.com/bemtvi/bemtvi-editorconfig) | A project's `.editorconfig` applied to its buffers | at startup |
| [bemtvi-diff](https://github.com/bemtvi/bemtvi-diff) | Side-by-side diff & merge-conflict viewer | on `:DiffGit` |
| [bemtvi-markdown-preview](https://github.com/bemtvi/bemtvi-markdown-preview) | Live markdown preview in your browser | at startup |
| [bemtvi-dap](https://github.com/bemtvi/bemtvi-dap) | A debugger front end (DAP) | on `<F5>` |

"Loaded" is when the plugin's code actually runs: the lazy ones cost nothing until you
press the key or run the command that triggers them.

### Appearance

**[catppuccin](https://github.com/bemtvi/catppuccin-bemtvi)** — a native port of
[catppuccin/nvim](https://github.com/catppuccin/nvim): the upstream palettes and
highlight groups, rewritten against `btv.*`. Installed with
`colorscheme catppuccin` already set; `:colorscheme` switches away any time. (bemtvi also
runs unmodified **neovim** colorschemes, so this is a starting point, not a lock-in.)

**[bemtvi-line](https://github.com/bemtvi/bemtvi-line)** — configure it the way you'd
configure `lualine.nvim`: sections `a`–`z`, a component library (mode, branch, diff,
diagnostics, filename, filetype, progress, location, LSP, …), themes that recolour by
mode, powerline separators. It compiles that config down onto bemtvi's native
`btv.statusline` registry, so the hot path stays in Rust.

### Getting around

**[bemtvi-tree](https://github.com/bemtvi/bemtvi-tree)** — a lazy, watched file tree in a
dock: `<leader>e` toggles it. Full file operations, git status per entry, glob filters,
mouse support, Nerd-Font icons with an ASCII fallback.

**[bemtvi-keys-helper](https://github.com/bemtvi/bemtvi-keys-helper)** — press a prefix
(`<leader>`, `g`, `z`, `<C-w>`) and pause: a popup lists every key that can follow, with
descriptions. It subscribes to bemtvi's pending-key oracle rather than intercepting input,
so it never disturbs the sequence you're typing.

**[bemtvi-help](https://github.com/bemtvi/bemtvi-help)** — `:help {topic}` in a read-only
split, with a fuzzy topic picker on `<leader>fh`. Its tag index is merged across the
runtimepath, which is what makes every other plugin's documentation readable in the
editor: install this one and `:help bemtvi-tree` works.

### Writing code

**[bemtvi-lspconfig](https://github.com/bemtvi/bemtvi-lspconfig)** — curated `btv.lsp`
configs for 407 language servers, driven onto bemtvi's own LSP control surface. It
*configures* servers; it does **not** install them — you still install `gopls`,
`rust-analyzer`, `pyright`, and friends yourself.

**[bemtvi-efmls-configs](https://github.com/bemtvi/bemtvi-efmls-configs)** — presets for
~150 linters and formatters (eslint, stylua, black, luacheck, …) run through
[`efm-langserver`](https://github.com/mattn/efm-langserver), which speaks their output
back over LSP. Installed with `languages = "*defaults"`, so the bundled default tool per
filetype is wired lazily on first open. Needs the `efm-langserver` binary, plus whichever
tools you want it to run.

**[bemtvi-snippets](https://github.com/bemtvi/bemtvi-snippets)** — the LSP snippet grammar
(tabstops, placeholders, choices, mirrors, transforms) with VSCode-format collection
loading. It installs
[friendly-snippets](https://github.com/rafamadriz/friendly-snippets) as a dependency, so
there are snippets for most languages from the start: type a trigger, accept the
completion row with `<C-y>`, jump between tabstops with `<C-j>` / `<C-k>`.

**[bemtvi-editorconfig](https://github.com/bemtvi/bemtvi-editorconfig)** — reads the
`.editorconfig` files above each file you open and applies indentation, line-ending and
charset settings to that buffer. No configuration; switch it off per buffer or globally
with `vim.g.editorconfig = false`.

### Reviewing and debugging

**[bemtvi-diff](https://github.com/bemtvi/bemtvi-diff)** — `:DiffGit` diffs the current file
against git `HEAD`; `:DiffConflict` opens conflict markers as a 3-way diff. Panes scroll
in lockstep, changed lines are tinted, and the Lua API renders any 2- or 3-pane spec you
build.

**[bemtvi-markdown-preview](https://github.com/bemtvi/bemtvi-markdown-preview)** —
`:MarkdownPreview` serves your open markdown buffers over the editor's own HTTP mount and
opens them in your browser, re-rendering as you type (no `:w` needed), with highlighted
code fences and mermaid diagrams. Because it's a mount rather than a bound port, it works
the same in the [browser build](browser-editor.md).

**[bemtvi-dap](https://github.com/bemtvi/bemtvi-dap)** — a Debug Adapter Protocol client:
breakpoints (conditional / hit / log), stepping, a scopes-stack-watches sidebar with
inline value editing, a REPL, and multiple concurrent sessions. `<F5>` or
`:DapToggleBreakpoint` loads it. You supply the adapter (debugpy, codelldb, …) and a
configuration per filetype — its README shows both.

## What you still need to install yourself

Plugins configure tools; they don't ship them. Depending on what you accepted:

- **Language servers** for bemtvi-lspconfig (`gopls`, `rust-analyzer`, `lua-language-server`, …).
- **`efm-langserver`** plus the linters/formatters you want, for bemtvi-efmls-configs.
- **Debug adapters** for bemtvi-dap.
- A **Nerd Font** for the tree's file icons (it falls back to ASCII without one).

Everything else — the picker, completion, tree-sitter highlighting, LSP itself,
quickfix, terminals, docks — is already in bemtvi. See
[What bemtvi adds](features.md).

## Changing your mind

The set is not special after installation: it is ordinary spec data in your own
`lua/plugins.lua`.

To drop one, delete its entry from `lua/plugins.lua` and run `:PluginClean` (which
removes the checkouts nothing declares any more). To add anything else, declare it the
same way and run `:PluginSync`:

```lua
btv.plugins({
  { "owner/repo", config = function() require("repo").setup() end },
})
```

- `:Plugins` — the dashboard: what's declared, installed, loaded, or drifted, with
  install / update / sync / restore / clean.
- `:PluginsWelcome` — reopen the offer and re-pick from the recommended set. Note it
  **overwrites** the managed `lua/plugins.lua` with your new choice.

To start from nothing, skip the offer (`<Esc>`) — or, if you're writing a config for
others, register your own set with `btv.plugins.recommend{...}`, which replaces bemtvi's
default one; `btv.plugins.recommend({})` suppresses the offer entirely.

## Beyond the set

Not offered on first run, but first-party and worth knowing about:

- [**bemtvi-workspaces**](https://github.com/bemtvi/bemtvi-workspaces) — per-project editor
  configuration as a committable `.bemtvi/config.json` (language servers, save actions,
  options) instead of hand-written rc Lua. See [Workspaces](features/workspaces.md).
- [**bemtvi-remotes**](https://github.com/bemtvi/bemtvi-remotes) — teaches `:connect` the
  `ssh://` and `container://` schemes, so the filesystem, processes and language servers
  live on a remote while the editing stays local.
- [**bemtvi-lspconfig-base**](https://github.com/bemtvi/bemtvi-lspconfig-base) — the same
  LSP configs trimmed to the most widely-used servers; a smaller alternative to
  bemtvi-lspconfig, not a companion to it.

## See also

- [First-party plugins](first-party-plugins.md) — the same plugins read as reference
  implementations: which `btv.*` surfaces each one is worth studying for.
- [Writing plugins](plugin-authoring.md) — the manager's full spec vocabulary
  (lazy triggers, dependencies, pinning, the lockfile) and the authoring guide.
