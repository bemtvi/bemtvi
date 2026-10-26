# Bundled colorschemes

The schemes in this directory are embedded in the binary (see
`BUILTIN_COLORSCHEMES` in `crates/bemtvi-server/src/excmd.rs`), so
`:colorscheme <name>` works with no config, on the wasm build too. A
`colors/<name>.lua` on the runtimepath shadows the bundled one.

| Scheme | Origin |
| --- | --- |
| `bemtvi` | bemtvi's own default, drawn from the app icon. Apache-2.0, like the rest of bemtvi. |
| `neutral` | bemtvi's One Dark. Apache-2.0, like the rest of bemtvi. |
| `default` | not a file: an alias for `bemtvi` (`BUILTIN_COLORSCHEME_ALIASES`). |
| everything else | ported from neovim, below |

## Ported from neovim

Every other file is a port of a scheme neovim ships in `runtime/colors/`,
taken from the `vendor/neovim` submodule at `70cfeabe23` (v0.12.0-794). They
are the work of the people below, not of bemtvi. All but `neovim` come from
Vim's colorscheme collection (<https://github.com/vim/colorschemes>), and each
port keeps its original's header (name, description, author, maintainer,
licence, last change) verbatim at the top of the file.

| Scheme | Author | Maintainer | Licence |
| --- | --- | --- | --- |
| `blue` | Steven Vertigan | Steven Vertigan | Vim |
| `catppuccin` | The Catppuccin Community (Latte for light, Mocha for dark) | Mao-Yining | none stated; ships with Vim and neovim |
| `darkblue` | Bohdan Vlasyuk | | Vim |
| `delek` | David Schweikert | David Schweikert | Vim |
| `desert` | Hans Fugal | Hans Fugal | Vim |
| `elflord` | Ron Aaron | Ron Aaron | Vim |
| `evening` | Bram Moolenaar | Steven Vertigan | Vim |
| `habamax` | Maxim Kim | | Vim |
| `industry` | Shian Lee | Shian Lee | Vim |
| `koehler` | Ron Aaron | Ron Aaron | Vim |
| `lunaperche` | Maxim Kim | | none stated; ships with Vim and neovim |
| `morning` | Bram Moolenaar | Bram Moolenaar | Vim |
| `murphy` | Ron Aaron | Ron Aaron | Vim |
| `neovim` | The Neovim Project (neovim's own theme, `default.vim` there, `:help dev_theme`) | The Neovim Project | Apache-2.0 |
| `pablo` | Ron Aaron | Ron Aaron | Vim |
| `peachpuff` | David Nečas (Yeti) | David Nečas (Yeti) | Vim |
| `quiet` | Maxence Weynans | Maxence Weynans | Vim |
| `retrobox` | Maxim Kim, ported from Lifepillar's gruvbox8 (after morhetz's gruvbox) | | none stated; ships with Vim and neovim |
| `ron` | Ron Aaron | Ron Aaron | Vim |
| `shine` | Yasuhiro Matsumoto | Yasuhiro Matsumoto | Vim |
| `slate` | Ralph Amissah | Ralph Amissah | Vim |
| `sorbet` | Maxence Weynans | Maxence Weynans | Vim |
| `torte` | Thorsten Maerz | Thorsten Maerz | Vim |
| `unokai` | k-37 (after Wimer Hazenberg's Monokai for TextMate) | | none stated; ships with Vim and neovim |
| `vim` | Bram Moolenaar (Vim's default colours, neovim's `vim.lua`) | The Vim Project | Vim |
| `wildcharm` | Maxim Kim | | Vim |
| `zaibatsu` | Romain Lafourcade | | Vim |
| `zellner` | Ron Aaron | Ron Aaron | Vim |

Each Vim scheme starts by sourcing `vim.lua`, whose `:hi clear` restores
neovim's built-in default groups (`src/nvim/highlight_group.c`, Apache-2.0)
first, so the ported files also carry those defaults under the scheme's own
groups. [`LICENSE.txt`](LICENSE.txt) is neovim's licence file unmodified: the
Apache License 2.0 followed by the Vim license, which covers the Vim-licensed
schemes and requires that its text travel with them.

### What the port changed

The originals are Vimscript, and bemtvi does not run Vimscript.
`scripts/gen-nvim-colorschemes.py` loads each one in real neovim (the vendored
runtime, `'termguicolors'` on, once per `'background'`), reads back the
highlight table neovim ends up with, and writes it out as `btv.hl.define`
calls. So each file holds the colours neovim paints in a truecolor UI.
neovim's own default groups are compiled into the `nvim` binary rather than
read from the runtime, so they come from the binary that ran the script (its
version is in each file's header). The
changes, all made by that script:

- The 256/16/8-colour terminal fallbacks (`cterm*`) are dropped; bemtvi paints
  in truecolor.
- `underdouble` becomes `underline` and `standout` becomes `reverse` (bemtvi
  has one underline style and no standout). `blend` and `nocombine` are dropped.
- Link targets are respelled in the target group's own case (`PMenu` →
  `Pmenu`), because bemtvi's group names are case-sensitive and neovim's are
  not.
- neovim's internal `Nvim*` cmdline-expression groups are left out, as are
  `g:terminal_color_*` (bemtvi's terminal does not read them).
- neovim's own theme is renamed from `default` to `neovim`, since `default`
  in bemtvi means bemtvi's own scheme.
- A scheme that follows `'background'` branches on `btv.o.background`, and
  `:set background=` reloads it, as in vim. A scheme that sets `'background'`
  sets `btv.o.background`.

To pick up a newer neovim, bump the submodule and rerun the script (it needs
`nvim` on `PATH`). Don't edit the generated files by hand.
