# Getting started: config, plugins, and a colorscheme

nxvim reads a user config the way neovim does. This guide sets up a config
directory, installs a Lua plugin, and themes the editor with the real
[catppuccin](https://github.com/catppuccin/nvim) colorscheme — unmodified.

## Build and run

```sh
cargo build --release
./target/release/nxvim file.txt   # the file argument is optional
```

## The config directory

On startup nxvim resolves a **config directory** and a **runtimepath**, then
sources `<config>/init.lua`. The config dir is the first of:

1. `$NXVIM_CONFIG`
2. `$XDG_CONFIG_HOME/nxvim`
3. `~/.config/nxvim`

The runtimepath is the config dir plus every `pack/*/start/*` plugin under it
(neovim's package layout), so a plugin checkout is drop-in. `$NXVIM_RUNTIMEPATH`
(an OS path list) prepends extra entries — handy for pointing at a checkout
without installing it.

```
~/.config/nxvim/
├── init.lua                      # sourced at startup
└── pack/
    └── plugins/
        └── start/
            └── catppuccin/       # a plugin: its lua/ and colors/ are found here
```

## Installing catppuccin

Clone the plugin into a `start` directory so nxvim discovers it automatically:

```sh
mkdir -p ~/.config/nxvim/pack/plugins/start
git clone --depth 1 https://github.com/catppuccin/nvim \
  ~/.config/nxvim/pack/plugins/start/catppuccin
```

## init.lua

```lua
-- ~/.config/nxvim/init.lua
require("catppuccin").setup({
  flavour = "mocha",   -- latte | frappe | macchiato | mocha
})

vim.cmd.colorscheme("catppuccin")
```

That is the same API you would write for neovim. `setup()` compiles the
highlight table to Lua bytecode under `stdpath("cache")`
(`~/.cache/nxvim/catppuccin/`, recompiled only when the config hash changes), and
`:colorscheme catppuccin` sources `colors/catppuccin.lua`, which runs
`require("catppuccin").load()` and fires the `ColorScheme` autocmd. Because the
config is sourced before the first frame, the editor is themed from the moment it
opens.

Open any source file and you should see catppuccin-mocha: a `#1e1e2e` background,
mauve keywords, green strings, a themed line-number gutter, and a `Visual`
selection.

## How the colors reach the screen

A colorscheme defines highlight groups in the **editor** (server-side) via
`nvim_set_hl`; the server resolves each treesitter capture and editor-chrome
region to a concrete 24-bit style and sends those styles in the `redraw`. The TUI
client is a truecolor renderer with a small built-in fallback theme for when no
colorscheme is loaded. See [*Lua*](architecture.md#lua) and
[*View protocol*](architecture.md#view-protocol-ui) in the architecture doc, and
the full design in
[`superpowers/specs/2026-06-01-catppuccin-colorscheme-design.md`](superpowers/specs/2026-06-01-catppuccin-colorscheme-design.md).

## Caveats

- **Truecolor terminal required.** nxvim emits 24-bit color escapes; use a
  terminal with truecolor support (most modern ones).
- **Treesitter is out-of-process and grammars are installed separately.** Token
  colors (keyword/string/…) need a grammar for the filetype in nxvim's data dir;
  editor colors (background, gutter, selection, status) apply regardless. See
  [*Syntax highlighting*](architecture.md#syntax-highlighting-treesitter).
- **Lua-only.** nxvim runs Lua plugins, not Vimscript colorschemes.
