# bemtvi

bemtvi (pronounced ben-tee-vee) is a modal, vim-style editor **100% vibe-coded**
using Claude Code — modal editing, Lua plugins, treesitter highlighting, and LSP,
built on a fully async, client-server design.

bemtvi is a headless, asynchronous editor **server** with thin UI **clients**
talking over msgpack-RPC. The editor logic lives in one place; the
terminal UI and the GPU GUI are just two clients of the same protocol.
It speaks vim at the keyboard: keystrokes, modes, ex-commands, and options
track [vim/neovim](https://neovim.io)'s observable editing behavior. Everything
else is bemtvi's own: configuration and plugins target bemtvi's `btv.*` Lua API
([design](docs/specs/2026-06-11-native-plugin-api.md)), where the server owns
every UI surface and plugins provide data and behavior. Colorschemes are
bemtvi's own too — pure-Lua modules that fill the highlight registry through the
`btv` highlight API.

Test the client-only live demo at https://bemtvi-demo.netlify.app. Use the `:eo`
command to open a local file or `:luao` to load a local lua file as config.
Use `:setf LANG` to activate treesitter if not auto detected or `:TSInstall` to
install the highlighter for your chosen language.

---

## Quick start

Grab a pre-built binary from the [**latest release**](https://github.com/bemtvi/bemtvi/releases/latest)
(or the rolling [`edge`](https://github.com/bemtvi/bemtvi/releases/tag/edge)
prerelease built from `main`). Binaries are published for five targets:

| OS      | Terminal editor (`bemtvi`)       | GUI (`bemtvi-gui`)                |
| ------- | ------------------------------- | -------------------------------- |
| Linux   | `x86_64` / `aarch64` (`.tar.gz`, glibc ≥ 2.17) | `x86_64` / `aarch64` (`.tar.gz` or `.AppImage`) |
| macOS   | `x86_64` / `aarch64` (`.pkg`, signed & notarized) | `x86_64` / `aarch64` (`.dmg`)   |
| Windows | `x86_64` (`.zip`)               | `x86_64` (`.zip`)                |

Then run it on a file (the argument is optional):

```sh
bemtvi file.txt        # terminal editor
bemtvi-gui file.txt    # native GUI (winit + wgpu)
```

Downloads ship with checksums and SLSA build provenance — see
[docs/verifying-downloads.md](docs/verifying-downloads.md) to verify them.

> **Truecolor terminal recommended.** bemtvi emits 24-bit color escapes; use a
> modern terminal with truecolor support.

> **Distinct modified keys need the kitty keyboard protocol.** Chords the legacy
> terminal encoding can't tell apart — `<C-i>` vs `<Tab>`, `<C-m>` vs `<CR>`,
> `<C-[>` vs `<Esc>`, and combos like `<S-CR>` / `<C-CR>` — are only distinguishable
> when the terminal speaks the [kitty keyboard
> protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/). bemtvi turns it on
> automatically when the terminal advertises support: **kitty**, **Ghostty**, and
> **foot** enable it by default, but **WezTerm** needs `enable_kitty_keyboard = true`.
> Without it those chords fold onto their legacy twin
> (`<C-i>` acts as `<Tab>`), exactly like classic vim — so bindings still work, they
> just can't be told apart. (Unlike neovim, bemtvi's terminal layer only speaks the
> kitty protocol, not xterm's older `modifyOtherKeys`.) Force the decision either way
> with `BEMTVI_KITTY_KEYBOARD=1` / `=0`.

> **The clipboard works over ssh.** `"+y` / `"*y` use a real host clipboard tool
> when one can actually reach you (`pbcopy`, `wl-copy`, `xclip`/`xsel` with a
> display, `tmux`). On a remote machine none can — so bemtvi asks your *terminal*
> to set its own clipboard with an [OSC
> 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h4-Operating-System-Commands)
> escape, which travels back down the ssh connection. It is used only when the
> terminal advertises support (queried at startup), so a yank never silently
> copies into a void — with one exception: inside **tmux or screen** the
> multiplexer answers the query itself and can only speak for itself, so its
> silence is taken as "yes" (both forward the escape outward by default). Force
> the decision either way with `BEMTVI_OSC52=1` / `=0`.
> Pasting *into* bemtvi from another app on your machine is your terminal's own
> paste (`Ctrl+Shift+V`, middle-click) — reading the clipboard back over OSC 52 is
> a round trip most terminals refuse, so `"+p` pastes what this session copied.

The terminal editor is the whole thing in one binary: it embeds the server on
its own thread and runs the client on the main thread, joined over the same
msgpack-RPC the UI clients speak.

---

## Build it yourself

You need a [Rust toolchain](https://rustup.rs). Then:

```sh
# Build and run (the file argument is optional)
cargo build --release
./target/release/bemtvi file.txt

# …or straight from cargo
cargo run -p bemtvi -- file.txt

# the native GUI
cargo run -p bemtvi-gui -- file.txt
```

### Web build (`bemtvi-edithost`) — runs entirely in the browser

`bemtvi-edithost` compiles the **whole editor** — `bemtvi-core` + the PUC Lua 5.4
VM + the full server tick — to **WebAssembly** (via emscripten) and drives it in a
**Web Worker**, **fully client-side, no server**. The page is the renderer and
input layer; the editor (including your `init.lua`) runs in the tab. File open/save
go through the browser's **File System Access API** (`:eo` / `:wo`, the in-browser
analogue of the GUI's dialogs) or persist to **OPFS** — so you **really edit local
files** (no upload, no backend), and a static host is enough to put it online. It
can also reach a real `bemtvi --daemon` over **WebTransport** for remote access,
vscode server style.

It's a separate, wasm-targeted crate, deliberately **excluded from the Cargo
workspace** (so `cargo build/test --workspace` never touches it). Build it with
its own script (needs the emscripten `emcc` and Node):

```sh
rustup target add wasm32-unknown-emscripten
crates/bemtvi-edithost/build.sh                      # → dist/eh.{mjs,wasm} + web/vendor/
node crates/bemtvi-edithost/web/serve.mjs            # cross-origin-isolated dev server
```

See [`crates/bemtvi-edithost/README.md`](crates/bemtvi-edithost/README.md) for the
full architecture (the Worker run loop, the OPFS/daemon filesystem legs, and the
client-side tree-sitter highlighter).

---

## What works today

- **Modal editing** — normal / insert / visual / visual-line,
  motions, operators, counts, dot-repeat, and a basic ex-command surface.
- **A branching undo tree** — `u` / `<C-r>`, `:undo {N}` to jump across branches,
  and `vim.fn.undotree()`, backed by cheap full-rope snapshots.
- **Multiple buffers, windows, tabs, and floats** — `:e`/`:b`/`:ls`, the
  `<C-w>` split family (`:split`/`:vsplit`, focus, resize, close), tab pages
  (`gt`/`:tabnew`/`<C-w>T`), and floating windows (`nvim_open_win` with borders
  and titles). All four `laststatus` modes ship.
- **Soft word-wrap & smooth scrolling** — `wrap` lays long lines across screen
  rows with display-line motions (`gj`/`gk`, `g0`/`g^`/`g$`), and scrolling
  animates smoothly across screen rows. The window-local
  `number`/`relativenumber`, `numberwidth`, `signcolumn`, `cursorline`,
  `breakindent`/`showbreak`, and horizontal-scroll options are honored.
- **Folds** — `zo`/`zc`/`za`, `zR`/`zM`, `zj`/`zk`, and a fold column, with
  four fold methods selected by `foldmethod`: `manual` (`zf`), `indent`,
  `marker` (literal `{{{`/`}}}`), and `expr` — whose `foldexpr` can be the
  native tree-sitter expression (`v:lua.btv.treesitter.foldexpr()`), a generic
  Lua expression, or the LSP marker driving `textDocument/foldingRange`.
- **Search & substitute** — interactive `/` and `?` with `n`/`N`, `hlsearch`,
  and `incsearch`, plus `:s` with the `g`/`i`/`I`/`n`/`c` flags. Two
  interchangeable regex dialects, selected by the `'regexsyntax'` option: **PCRE**
  (the default) — standard perl-compatible patterns via the Rust `regex` crate,
  with `$0`/`$1`/`${name}` replacements — or **`vim`**, the real vim "magic"
  dialect (`\(\)` groups, `\<`/`\>`, `\zs`/`\ze`, `&`/`\1` back-refs, the
  `\u`/`\U` case modifiers, …) matched by the embedded vim regexp engine.
- **Command-line completion** — `<Tab>` on the `:` line opens a wildmenu over
  commands, file paths, options, and plugin/user commands, with a docs sidebar
  and live selection preview.
- **Registers & marks** — named/numbered/special registers, the system clipboard
  (`"+`/`"*`), buffer-local and global marks, the special marks, and `'{mark}`
  ranges.
- **Treesitter highlighting, in-process** — incremental parsing per buffer,
  installable grammars, and `:TSInstall <lang>` to fetch + compile a grammar on
  demand. A full tree-scripting Lua API (parsers, queries, predicates,
  injections) runs on bemtvi's primitives.
- **A real Lua config runtime** — vendored PUC Lua 5.4 running *inside*
  the server: a config dir + runtimepath, `require`/`init.lua`, keymaps, user
  commands, autocmds, an async event loop (timers, process spawn, scheduling),
  and a **colorscheme runtime** — a colorscheme is a pure-Lua module that fills
  the highlight registry (see [Intentional deviations](#intentional-deviations-these-will-not-change)).
- **LSP & diagnostics** — servers are configured and started from user Lua
  config; completion, hover, go-to, references,
  rename, diagnostics (underline / virtual text / signs / float), semantic
  tokens, and inlay hints are wired.
- **Mouse support** — click, drag-select, multi-click, wheel scroll, divider
  drag, `'mousemodel'` menus, and middle-click paste, in both clients.
- **The `btv.*` native plugin surfaces** — the server-owned extensibility API is
  built: a completion engine (`btv.complete`, with buffer / LSP / snippet
  sources), a fuzzy picker (`btv.picker`, with a preview pane), a snippet engine
  (`btv.snippet`, LSP snippet syntax + tabstop navigation), composable statusline
  segments (`btv.statusline`, lualine-shaped, per-window), viewport decorations
  (`btv.decor`, off-tick providers driving extmarks / virtual text), and the
  floating-widget UI layer (`btv.ui.input`/`select`/`confirm`/`float`,
  promise-based). Every widget's keys are rebindable through the real keymap
  engine. See [`examples/`](examples).
- **In-buffer terminals** — a PTY-backed terminal buffer (`:terminal`) with a
  vt100 emulation layer, end-to-end backpressure for runaway output, and
  scrollback, in both native clients and the web build.
- **Quickfix, location lists & docks** — a quickfix list with `errorformat`
  parsing, window-local location lists (`:lopen`/`:lvimgrep`), named and
  dynamic (function-sourced) lists with picker integration, and VSCode-style
  permanent edge docks (`btv.dock`) with per-region tablines and spatial
  `<C-w><C-w>` navigation.
- **Workspaces & a remote daemon** — `bemtvi --workspace <dir>` opens a directory
  as a session that persists its tabs, windows, unnamed buffers, and
  per-workspace option overrides (`btv.wso`) across restarts. `bemtvi --daemon`
  runs the editor as a headless server a thin client attaches to over
  QUIC/WebTransport (local *or* remote, vscode-server style), choosing local or
  remote config on connect.
- **Image previews** — opening an image buffer renders the picture inline:
  ratatui-image in the terminal, a wgpu textured quad in the GUI, and an
  out-of-band `<img>` in the web build (`btv.o.imagepreview`).

---

## Vim, by the keystroke

If you know vim, your muscle memory transfers. Concretely, what's wired today:

- **Motions** — `h j k l`, words (`w W b B e E`), line (`0 ^ $`), find-char
  (`f F t T` + `; ,`), go-to-line (`gg G`), search (`/ ? n N`, plus `* # g* g#`),
  marks (`` ` ``/`'`), and the soft-wrap display motions (`gj gk g0 g^ g$`).
  Viewport `z`-commands (`zt zz zb`, `z.`/`z-`) reposition the screen.
- **Text objects** — `iw aw iW aW`, quotes (`i" a"`, `i' a'`, and backtick),
  brackets (`i( a(`, `i{ a{`, `i[ a[`, `i< a<`, plus `ib`/`ab` and `iB`/`aB`),
  paragraph (`ip ap`), sentence (`is as`), and the tree-sitter objects
  (`if`/`af` function, `ia`/`aa` argument, `ic`/`ac` comment, `it`/`at`
  type/class).
- **Operators** — `d c y`, paste (`p P`), shift (`>>`/`<<`, `>motion`/`<motion`,
  `>ip`), reindent (`=`, `==`, `=motion`, `gg=G`), comment toggle (`gc`/`gcc`,
  with a per-filetype `commentstring`), replace (`r`), case-toggle (`~`), join
  (`J`), and the line shortcuts (`x X D C s`). Counts and dot-repeat (`.`) work
  throughout.
- **Visual modes** — charwise (`v`) and linewise (`V`), `o`/`O` to swap ends,
  operators and text objects over a selection. (Blockwise `<C-v>` is a
  deliberate non-goal — use [multi-cursor](#notable-additions) instead.)
- **Registers** — named (`"a`–`"z`), append (`"A`–`"Z`), the yank/delete ring
  (`"0`–`"9`), small-delete (`"-`), black-hole (`"_`), the read-only specials
  (`"%` `"/` `":` `".`), and the system clipboard (`"+` `"*`).
- **Marks & jumps** — `m` / `` ` `` / `'`, the automatic special marks, the
  jumplist (`<C-o>` / `<C-i>`), and the changelist (`g;` / `g,`).
- **Insert mode** — `<C-r>` register insert (`<C-r><C-w>` for the word under the
  cursor), auto-indent, native completion (`<C-n>`/`<C-p>`/`<C-y>`), and
  snippets with tabstop navigation.
- **Ex commands** — `:e :w :q` (and the split/tab variants), `:s` (with the
  `g i I n c` flags), `:g`/`:v`, `:d :m :t`, `:normal`, `:put`, `:undo`/`:redo`,
  `:set`/`:setlocal`, the listings (`:marks :registers :jumps :changes
  :messages`), and `:vimgrep`/`:lvimgrep`.

Not yet wired (see the [roadmap](#not-yet-implemented-roadmap)): `%` match-pair,
the paragraph/sentence motions (`{ } ( )`), the screen motions (`H M L`), the
`gu`/`gU`/`g~` case operators, `gq` reflow, HTML tag objects (in bemtvi `it`/`at`
are the tree-sitter type/class object), and macros (`q`/`@`).

---

## Notable additions

Things bemtvi has that neovim doesn't:

- **Helix mode (selection-first editing).** An opt-in `noun→verb` model
  ([`docs/features/helix-mode.md`](docs/features/helix-mode.md)) alongside the vim
  grammar: `:helix` (or `btv.helix.enable()`) enters `HELIX`, where every cursor is
  a persistent `anchor..head` selection, motions re-select as you move (`w`
  selects the next word), and verbs act on the selection immediately with no
  operator-pending wait (`wd` deletes a word). Multi-selection is the default;
  match mode (`m` + tree-sitter text objects), surround, regex-select (`s`/`S`),
  per-selection insert/paste, and content rotate/align are all there. The engine
  is native; the key layout is the bundled `btv.helix` plugin, so every verb is
  rebindable by name (`btv.helix.actions.*`).
- **Multi-cursor (Helix-style placement mode).** `<A-c>` enters a `MULTICURSOR`
  placement mode and drops a cursor at the current position. There, motions move
  only the active cursor — you navigate (including `/`-search) and drop more
  cursors with `c`, or place a run with `{count}c{motion}` (e.g. `10cj`). `<Esc>`
  keeps the placed cursors and returns to Normal, where motions and edits act on
  **every** cursor at once; a second `<Esc>` collapses back to one. Cursors are
  extmarks, so they stay correct across edits and ride undo/redo for free.
- **A native GUI client.** `bemtvi-gui` (winit + wgpu) is a first-party GPU
  client — not a third-party frontend — rendering the same `View` the terminal
  client does. It wires the OS's **native file dialogs** into the command line
  (a GUI-only affordance; the server stays unaware):
  - The `…o` *open* family — `:eo`, `:spo`, `:vso`, `:tabeo`, `:newo`, `:vnewo`,
    and a bare `:e` (an alias of `:eo`) — pops a system **open** dialog, then
    runs the base command (`:e`/`:sp`/`:vs`/`:tabe`/…) on the chosen file,
    preserving its edit / split / tab semantics.
  - `:wo` (and a bare `:w` on an unnamed buffer) pops a system **save** dialog
    and writes to the chosen path.

---

## Configuration

bemtvi reads a Lua config. On startup it resolves a **config
directory** — the first of `$BEMTVI_CONFIG`, `$XDG_CONFIG_HOME/bemtvi`, or
`~/.config/bemtvi` — and sources `<config>/init.lua` before the first frame. The
**runtimepath** is that dir plus every `pack/*/start/*` entry under it, so a
colorscheme checkout is drop-in:

```
~/.config/bemtvi/
├── init.lua                      # sourced at startup
└── pack/
    └── plugins/
        └── start/
            └── catppuccin/       # a plugin; its lua/ and colors/ are found here
```

```lua
-- ~/.config/bemtvi/init.lua
require("catppuccin").setup({ flavour = "mocha" })
vim.cmd.colorscheme("catppuccin")
```

The editor's own config API is the `btv.*` namespace
([design](docs/specs/2026-06-11-native-plugin-api.md)). The only `vim.*` is a
closed whitelist of **muscle-memory aliases** (`vim.g`,
`vim.o`/`vim.opt`, `vim.cmd`, `vim.keymap.set`, autocmds, `vim.notify`, and
friends — 1:1 over `btv`, so config can be written in familiar spellings like
`vim.g.mapleader = " "` and `vim.o.number = true`; the
full whitelist is in
[ADR 0002](docs/decisions/0002-native-plugin-system.md)). A colorscheme reaches
for a handful of those aliases (notably the `nvim_set_hl` highlight helper) and
nothing more.

### Runnable examples

The [`examples/`](examples) directory has ~85 self-contained, end-to-end-verified
configs — one per feature (treesitter, LSP, floats, registers, tabs, mouse,
statusline, completion, picker, snippets, decor, docks, quickfix, image
previews, …). Each is a config dir you point bemtvi at:

```sh
BEMTVI_CONFIG=examples/treesitter cargo run -p bemtvi -- examples/treesitter/sample.rs
```

### Writing plugins

A plugin is **pure Lua over the `btv.*` API** — a `lua/<name>/init.lua` module that
exposes `setup(opts)` and wires keymaps, commands, autocmds, and UI through `btv.*`
(the server owns the surfaces; the plugin supplies data and behavior). Install one
by declaring it with the built-in manager and running `:PluginSync`:

```lua
btv.plugins({
  { "bemtvi/bemtvi-keys-helper",
    config = function() require("bemtvi-keys-helper").setup({}) end },
})
```

Plugins are testable end-to-end with the native framework — write `test/*_spec.lua`
and run `bemtvi --test-plugin .`. See the full guides:
[**Writing bemtvi plugins**](docs/plugin-authoring.md) and
[**Testing bemtvi plugins**](docs/specs/2026-06-19-lua-plugin-testing.md).

Writing one with a coding agent? Install
[**bemtvi-plugin-skills**](https://github.com/bemtvi/bemtvi-plugin-skills)
(`npx skills add https://github.com/bemtvi/bemtvi-plugin-skills`) — agent skills
covering the `btv.*` model and one surface each, so the agent doesn't reach for
neovim APIs that don't exist here.

---

## How it's built

The authoritative design doc is **[docs/architecture.md](docs/architecture.md)** —
read it first for the crate layout, the client-server model, the RPC + `View`
protocols, the rope text model, the Lua bridge, treesitter, LSP, and the roadmap.

The short version:

| Crate            | Responsibility                                                              |
| ---------------- | --------------------------------------------------------------------------- |
| `bemtvi-core`     | The editor model — buffers, modes, motions, operators, ex-commands, undo, and the renderable `View`. **Pure & synchronous.** |
| `bemtvi-server`   | The headless server — owns the core + Lua, hosts the `nvim_*` API, runs the async main loop. |
| `bemtvi-rpc`      | Async msgpack-RPC transport (bemtvi's own protocol).                         |
| `bemtvi-lua`      | Embedded PUC Lua 5.4 runtime, the `btv.*` API prelude, and the `vim.*` glue.  |
| `bemtvi-ts`       | The in-process treesitter engine (loads grammars, parses incrementally).    |
| `bemtvi-lsp`      | The native LSP client (protocol, transport, manager) — bemtvi's own stdio spawning. |
| `bemtvi-regex`    | The vendored vim regexp engine (`regexp.c` as C), shared by search and `:s`. |
| `bemtvi-tui`      | The terminal UI client (ratatui + crossterm). Owns no editor state.         |
| `bemtvi-gui`      | Native GUI client (winit + wgpu + glyphon).                                 |
| `bemtvi-edithost` | Fully client-side WebAssembly build of the whole editor (core + Lua + server tick, in a Web Worker; no server). Excluded from the workspace. |
| `bemtvi-view`     | Frontend-neutral decode/input layer shared by the native clients.           |
| `bemtvi`          | The `nvim`-style entry point: wires an embedded server + the TUI client.    |

The editor core is pure, synchronous, and `!Send`; all async, RPC, and Lua live
above it, so every front end shares identical editing behavior.

---

## Development

```sh
cargo build                                          # debug build of the whole workspace
cargo test --workspace                               # everything is a black-box integration test
cargo test -p bemtvi-server --test editing <name>     # run a single test by substring

cargo fmt --all                                      # format
cargo clippy --all-targets -- -D warnings            # lint
```

- **The Lua backend is fixed at vendored PUC Lua 5.4** (mlua's `lua54`, baked
  into the shared `mlua` dep) — there is no backend feature to select or thread.
  Lint and test on default features.
- **A pre-commit hook** runs `cargo fmt --check` + `cargo clippy -D warnings`.
  After a fresh clone, run `pre-commit install` once.
- **Tests are black-box and end-to-end** — they start a real server over RPC,
  feed vim key-notation, and assert on buffer contents / cursor / the redraw.
  There are no unit tests by design; see *Testing philosophy* in the
  architecture doc.

---

## Notable deviations & missing features

bemtvi tracks vim/neovim's **observable editing behavior**, but it is a fresh
rust-native implementation, not a port — so some things differ by design and many
things simply aren't built yet. The canonical, always-current list of gaps lives
in **[docs/known-approximations.md](docs/known-approximations.md)** (and, more
precisely, in `INCOMPLETE:` comments and `btv._notimpl` raises in the code). The
highlights:

### Intentional deviations (these will not change)

- **A native plugin system — `btv.*` is the only API.**
  Configuration and plugins target bemtvi's own `btv.*` API, built around its
  snapshot + effect-queue, client-server design. Colorschemes are bemtvi's
  own — Lua modules that fill the highlight registry through the `btv` highlight
  API. A closed whitelist of muscle-memory aliases (`vim.g`, `vim.o`/`vim.opt`,
  `vim.cmd`, `vim.keymap.set`, autocmds / user commands / `nvim_set_hl`,
  `vim.notify`, the `vim.tbl_*`-style helpers, … — canonical list in
  [ADR 0002](docs/decisions/0002-native-plugin-system.md))
  maps 1:1 onto `btv` so config can be written in familiar spellings —
  see [the design](docs/specs/2026-06-11-native-plugin-api.md) and
  [ADR 0002](docs/decisions/0002-native-plugin-system.md).
- **No Vimscript.** Legacy Vimscript (`.vim` files, the
  `eval.c` language) is an explicit non-goal. `vim.fn.*` is a hand-written
  set of helper aliases, not an interpreter. Colorschemes must be Lua.
- **Clients render a semantic `View`.** There is no `ext_linegrid` / grid
  protocol; clients receive a semantic `View` and lay out their own widgets. The
  client-protocol verbs are `btv_*` (`btv_input`, `btv_ui_attach`, …); the
  editing-API methods keep neovim-faithful spellings (`nvim_buf_get_lines`, …)
  as muscle-memory names, but the whole surface is bemtvi's own protocol.
- **Rope-backed, byte-indexed buffers** with a strict trailing-newline
  invariant, and a **branching undo tree of full-rope snapshots** — cheap to
  snapshot, with full branching semantics.
- **Treesitter-only highlighting.** There is no regex / `syntax.vim`
  highlighter — all highlighting comes from treesitter grammars.
- **No visual-block mode** (`<C-v>`). Charwise and linewise visual modes are
  supported; blockwise selection is a deliberate non-goal, not a roadmap item.
  Use the [multicursor mode](#notable-additions) instead.
- **Different indent defaults.** `tabstop` defaults to **4**, with
  `shiftwidth=0` and `softtabstop=-1` (both "follow tabstop"), so one knob drives
  the indent width.
- **Cross-source event ordering is non-deterministic** (random `select!` pick for
  fairness) rather than neovim's deterministic multiqueue order — TBD if it affects
  correctness, or only which independent background source lands first.

### Not yet implemented (roadmap)

- **Macros** — `q` record / `@` replay aren't built.
- **Some motions.** `%` match-pair, the paragraph/sentence motions
  (`{` `}` `(` `)`), and the screen motions (`H` `M` `L`) aren't wired yet.
- **More window-local options.** `wrap`, `number`/`relativenumber`,
  `numberwidth`, `signcolumn`, `cursorline`, `colorcolumn`,
  `breakindent`/`showbreak`, the fold options
  (`foldenable`/`foldcolumn`/`foldlevel`), and the horizontal-scroll
  options are honored; the rest (`cursorcolumn`, `list`, …) are not.
- **A broad options surface.** `:set` knows ~75 options — the search booleans,
  the window options above, the buffer-local indentation options plus
  `commentstring`, the fold / mouse / statusline / encoding families, and
  more — and fails loud (`E518`) on anything else; the bulk of vim's hundreds
  of options are missing. (The Lua surface is lenient instead: a `vim.o`/`btv.o`
  write to an unmodeled name is recorded with a warning but inert.)
- **The `gu`/`gU`/`g~` case operators and the `:map`-family ex-commands.** Case
  is toggled via `~`; keymaps are set via `vim.keymap.set` / `nvim_set_keymap`.
  Both intentionally postponed.
- **LSP & treesitter edges** — semantic tokens and inlay hints are real but
  approximate (one group per cell, whole-document only, no range requests);
  Lua-driven treesitter indent (`indentexpr`) is deferred. Each approximation is
  tagged at its call site.

### No silent stubs

A core project rule: **everything unimplemented fails loud.** An unsupported path
raises with the name of what's missing (`bemtvi: not implemented: <name>`) rather
than quietly returning a fake value — so a half-working feature never masquerades
as a whole one. You can enumerate exactly what a given config trips at runtime.

---

## Vibe-coded

I started this project on a whim just to test the limits of current day vibe-coding.
I wanted to see how far I could go, but I didn't expect the answer to turn out to be
as far as I wanted. I got so excited I decided to speed run it for enough features
to use it as my daily driver. I love neovim and I use(d?) it as my daily driver, so
bemtvi is heavily inspired by it and my favorite plugins.

I've been using Claude Code with Opus 4.8 on the high effort most of the time and the
experience has been pretty frustration free. Claude only ever got stuck in one specific
feature, but I switched to max effort and asked it to refine the plan, then it
managed to advance. Of course Claude didn't do all of it by itself, I'm a pretty
seasoned software engineer, so I constantly steered it in the "right" direction and
I also made all architecture decisions, although with Claude's help. I also didn't
review the code, apart from skimming it while it was being generated as a coarse
sanity check, and interrupting and correcting Claude on egregious mistakes. At times
I had 4 Claude Code instances implementing 4 features in parallel, thankfully Claude
doesn't care about conflicts. lol. I had to signup for the Max 20x plan and almost
maxed out the weekly limit, that's how fast I was going.

I intend to keep this code-base entirely 100% free of human written code. Create an
issue if you have feature requests or bug reports. Claude doesn't care about the quality of
your message, so write whatever you want. lol

I won't accept PRs, only feature requests and bug reports by text description.

---

## License

[Apache License 2.0](LICENSE).

`vendor/neovim` is a git submodule kept purely as a behavioral/source-layout
reference — it is never built or linked, and is not needed to build bemtvi.
