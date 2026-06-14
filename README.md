# nxvim

A modal, vim-style editor **100% vibe-coded in less than two weeks**
using Claude Code — modal editing, Lua config, treesitter highlighting, and
LSP, built on a fully async, client-server design.

nxvim is a headless, asynchronous editor **server** with thin UI **clients**
talking over nxvim's own msgpack-RPC. The editor logic lives in one place; the
terminal UI and the GPU GUI are just two clients of the same protocol.
It speaks vim at the keyboard: keystrokes, modes, ex-commands, and options
track [neovim](https://neovim.io)'s observable behavior. Everything else is
nxvim's own: configuration and plugins target nxvim's `nx.*` Lua API
([design](docs/specs/2026-06-11-native-plugin-api.md)), where the server owns
every UI surface and plugins provide data and behavior — and a bounded
compatibility glue runs **real neovim colorschemes unmodified**, the one
neovim plugin surface nxvim ships.

> **Status: early but substantial.** Day-to-day modal editing, splits, tabs,
> floating windows, treesitter highlighting, a real Lua config runtime, and LSP
> all work. Good enough to be a daily driver.

Test the client-only (no lua) live demo at https://nxvim-demo.netlify.app.
Use :setf LANG to activate treesitter if not auto detected. Don't forget to
checkout out the [multicursor mode](#notable-additions)!

---

## Quick start

Grab a pre-built binary from the [**latest release**](https://github.com/davidrios/nxvim/releases/latest)
(or the rolling [`edge`](https://github.com/davidrios/nxvim/releases/tag/edge)
prerelease built from `main`). Binaries are published for five targets:

| OS      | Terminal editor (`nxvim`)       | GUI (`nxvim-gui`)                |
| ------- | ------------------------------- | -------------------------------- |
| Linux   | `x86_64` / `aarch64` (`.tar.gz`, musl-static) | `x86_64` / `aarch64` (`.tar.gz` or `.AppImage`) |
| macOS   | `x86_64` / `aarch64` (`.pkg`, signed & notarized) | `x86_64` / `aarch64` (`.dmg`)   |
| Windows | `x86_64` (`.zip`)               | `x86_64` (`.zip`)                |

Then run it on a file (the argument is optional):

```sh
nxvim file.txt        # terminal editor
nxvim-gui file.txt    # native GUI (winit + wgpu)
```

Downloads ship with checksums and SLSA build provenance — see
[docs/verifying-downloads.md](docs/verifying-downloads.md) to verify them.

> **Truecolor terminal recommended.** nxvim emits 24-bit color escapes; use a
> modern terminal with truecolor support.

The terminal editor is the whole thing in one binary: it embeds the server on
its own thread and runs the client on the main thread, joined over the same
msgpack-RPC the UI clients speak.

---

## Build it yourself

You need a [Rust toolchain](https://rustup.rs). Then:

```sh
# Build and run (the file argument is optional)
cargo build --release
./target/release/nxvim file.txt

# …or straight from cargo
cargo run -p nxvim -- file.txt

# the native GUI
cargo run -p nxvim-gui -- file.txt
```

### Web build (`nxvim-edithost`) — runs entirely in the browser

`nxvim-edithost` compiles the **whole editor** — `nxvim-core` + the PUC Lua 5.1
VM + the full server tick — to **WebAssembly** (via emscripten) and drives it in a
**Web Worker**, **fully client-side, no server**. The page is the renderer and
input layer; the editor (including your `init.lua`) runs in the tab. File open/save
go through the browser's **File System Access API** (`:eo` / `:wo`, the in-browser
analogue of the GUI's dialogs) or persist to **OPFS** — so you **really edit local
files** (no upload, no backend), and a static host is enough to put it online. It
can also reach a real `nxvim --daemon` over **WebTransport** for filesystem access.

It's a separate, wasm-targeted crate, deliberately **excluded from the Cargo
workspace** (so `cargo build/test --workspace` never touches it). Build it with
its own script (needs the emscripten `emcc` and Node):

```sh
rustup target add wasm32-unknown-emscripten
crates/nxvim-edithost/build.sh                      # → dist/eh.{mjs,wasm} + web/vendor/
node crates/nxvim-edithost/web/serve.mjs            # cross-origin-isolated dev server
```

See [`crates/nxvim-edithost/README.md`](crates/nxvim-edithost/README.md) for the
full architecture (the Worker run loop, the OPFS/daemon filesystem legs, and the
client-side tree-sitter highlighter).

Because there's no server, this build is the editor **core** only: modal editing,
ex-commands, undo, search/substitute, registers/marks, multiple buffers, splits,
and tab pages. The features that live in `nxvim-server` — **Lua config,
treesitter syntax highlighting, and LSP** — are not part of a client-only build.

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
- **Search & substitute** — interactive `/` and `?` with `n`/`N`, `hlsearch`,
  and `incsearch`, plus `:s` with the `g`/`i`/`I`/`n`/`c` flags (shared regex
  engine).
- **Registers & marks** — named/numbered/special registers, the system clipboard
  (`"+`/`"*`), buffer-local and global marks, the special marks, and `'{mark}`
  ranges.
- **Treesitter highlighting, in-process** — incremental parsing per buffer,
  installable grammars, and `:TSInstall <lang>` to fetch + compile a grammar on
  demand. A full tree-scripting Lua API (parsers, queries, predicates,
  injections) runs on nxvim's primitives.
- **A real Lua config runtime** — Lua 5.1 (LuaJIT by default) running *inside*
  the server: a config dir + runtimepath, `require`/`init.lua`, keymaps, user
  commands, autocmds, an async event loop (timers, process spawn, scheduling),
  and the compatibility glue that runs **real, unmodified neovim
  colorschemes** — e.g.
  [catppuccin](https://github.com/catppuccin/nvim) — the one neovim plugin
  surface nxvim ships (see [Intentional deviations](#intentional-deviations-these-will-not-change)).
- **LSP & diagnostics** — servers are configured and started from user Lua
  config; completion, hover, go-to, references,
  rename, diagnostics (underline / virtual text / signs / float), semantic
  tokens, and inlay hints are wired.
- **Mouse support** — click, drag-select, multi-click, wheel scroll, divider
  drag, `'mousemodel'` menus, and middle-click paste, in both clients.

---

## Notable additions

Things nxvim has that neovim doesn't:

- **Multi-cursor (Helix-style placement mode).** `<A-c>` enters a `MULTICURSOR`
  placement mode and drops a cursor at the current position. There, motions move
  only the active cursor — you navigate (including `/`-search) and drop more
  cursors with `c`, or place a run with `{count}c{motion}` (e.g. `10cj`). `<Esc>`
  keeps the placed cursors and returns to Normal, where motions and edits act on
  **every** cursor at once; a second `<Esc>` collapses back to one. Cursors are
  extmarks, so they stay correct across edits and ride undo/redo for free.
- **A native GUI client.** `nxvim-gui` (winit + wgpu) is a first-party GPU
  client — not a third-party frontend — rendering the same `View` the terminal
  client does. It wires the OS's **native file dialogs** into the command line
  (a GUI-only affordance; the server stays unaware):
  - The `…o` *open* family — `:eo`, `:spo`, `:vso`, `:tabeo`, `:newo`, `:vnewo`,
    and a bare `:e` (an alias of `:eo`) — pops a system **open** dialog, then
    runs the base command (`:e`/`:sp`/`:vs`/`:tabe`/…) on the chosen file,
    preserving its edit / split / tab semantics.
  - `:wo` (and a bare `:w` on an unnamed buffer) pops a system **save** dialog
    and writes to the chosen path.
- **A browsable message panel.** `:messages` and `:ls` open a navigable,
  bottom-docked panel (an nxvim-native surface, not a vim window) that plugins
  can also drive as a general output / picker surface. This was initially built
  as a development testing aid but it ended up being useful in itself.

---

## Configuration

nxvim reads a Lua config. On startup it resolves a **config
directory** — the first of `$NXVIM_CONFIG`, `$XDG_CONFIG_HOME/nxvim`, or
`~/.config/nxvim` — and sources `<config>/init.lua` before the first frame. The
**runtimepath** is that dir plus every `pack/*/start/*` entry under it, so a
colorscheme checkout is drop-in:

```
~/.config/nxvim/
├── init.lua                      # sourced at startup
└── pack/
    └── plugins/
        └── start/
            └── catppuccin/       # a plugin; its lua/ and colors/ are found here
```

```lua
-- ~/.config/nxvim/init.lua
require("catppuccin").setup({ flavour = "mocha" })
vim.cmd.colorscheme("catppuccin")
```

The editor's own config API is the `nx.*` namespace
([design](docs/specs/2026-06-11-native-plugin-api.md)), and `vim.*` appears in
two bounded places: a closed whitelist of **muscle-memory aliases** (`vim.g`,
`vim.o`/`vim.opt`, `vim.cmd`, `vim.keymap.set`, autocmds, `vim.notify`, and
friends — 1:1 over `nx`, so the declarative lines of an existing neovim config
like `vim.g.mapleader = " "` and `vim.o.number = true` work unmodified; the
full whitelist is in
[ADR 0002](docs/decisions/0002-native-plugin-system.md)) and the
**colorscheme glue** that runs neovim colorschemes unmodified.

### Runnable examples

The [`examples/`](examples) directory has ~30 self-contained, end-to-end-verified
configs — one per feature (treesitter, LSP, floats, registers, tabs, mouse,
statusline, …). Each is a config dir you point nxvim at:

```sh
NXVIM_CONFIG=examples/treesitter cargo run -p nxvim -- examples/treesitter/sample.rs
```

---

## How it's built

The authoritative design doc is **[docs/architecture.md](docs/architecture.md)** —
read it first for the crate layout, the client-server model, the RPC + `View`
protocols, the rope text model, the Lua bridge, treesitter, LSP, and the roadmap.

The short version:

| Crate            | Responsibility                                                              |
| ---------------- | --------------------------------------------------------------------------- |
| `nxvim-core`     | The editor model — buffers, modes, motions, operators, ex-commands, undo, and the renderable `View`. **Pure & synchronous.** |
| `nxvim-server`   | The headless server — owns the core + Lua, hosts the `nvim_*` API, runs the async main loop. |
| `nxvim-rpc`      | Async msgpack-RPC transport (nxvim's own protocol).                         |
| `nxvim-lua`      | Embedded Lua 5.1 runtime and the `vim.*` standard library.                  |
| `nxvim-ts`       | The in-process treesitter engine (loads grammars, parses incrementally).    |
| `nxvim-tui`      | The terminal UI client (ratatui + crossterm). Owns no editor state.         |
| `nxvim-gui`      | Native GUI client (winit + wgpu + glyphon).                                 |
| `nxvim-edithost` | Fully client-side WebAssembly build of the whole editor (core + Lua + server tick, in a Web Worker; no server). Excluded from the workspace. |
| `nxvim-view`     | Frontend-neutral decode/input layer shared by the native clients.           |
| `nxvim`          | The `nvim`-style entry point: wires an embedded server + the TUI client.    |

The editor core is pure, synchronous, and `!Send`; all async, RPC, and Lua live
above it, so every front end shares identical editing behavior.

---

## Development

```sh
cargo build                                          # debug build of the whole workspace
cargo test --workspace                               # everything is a black-box integration test
cargo test -p nxvim-server --test editing <name>     # run a single test by substring

cargo fmt --all                                      # format
cargo clippy --all-targets -- -D warnings            # lint
```

- **Do not use `--all-features`.** The Lua backend is a Cargo feature with
  mutually-exclusive variants (`luajit` default, `lua51` alternative); enabling
  both breaks the `mlua-sys` build. Lint/test on default features. To check PUC
  Lua deliberately: `--no-default-features --features lua51`.
- **A pre-commit hook** runs `cargo fmt --check` + `cargo clippy -D warnings`.
  After a fresh clone, run `pre-commit install` once.
- **Tests are black-box and end-to-end** — they start a real server over RPC,
  feed vim key-notation, and assert on buffer contents / cursor / the redraw.
  There are no unit tests by design; see *Testing philosophy* in the
  architecture doc.

---

## Notable deviations & missing features

nxvim aims for **observable** neovim compatibility, but it is a fresh
rust-native implementation, not a port — so some things differ by design and many
things simply aren't built yet. The canonical, always-current list of gaps lives
in **[docs/known-approximations.md](docs/known-approximations.md)** (and, more
precisely, in `INCOMPLETE:` comments and `vim._notimpl` raises in the code). The
highlights:

### Intentional deviations (these will not change)

- **A native plugin system — colorschemes are the only neovim plugin surface.**
  nxvim does not host neovim plugins: they are imperative programs written
  against neovim's runtime model (blocking reads, libuv as a public API,
  frame-time render hooks), which nxvim's snapshot + effect-queue,
  client-server design deliberately is not. A bounded `vim.*` glue runs Lua
  **colorschemes** (pure `nvim_set_hl` data, e.g. catppuccin) unmodified;
  configuration and everything else target nxvim's own `nx.*` API, with a
  closed whitelist of muscle-memory aliases (`vim.g`, `vim.o`/`vim.opt`,
  `vim.cmd`, `vim.keymap.set`, autocmds / user commands / `nvim_set_hl`,
  `vim.notify`, the `vim.tbl_*`-style helpers, … — canonical list in
  [ADR 0002](docs/decisions/0002-native-plugin-system.md))
  mapping 1:1 onto `nx` so the declarative portion of an existing neovim
  config works unmodified —
  see [the design](docs/specs/2026-06-11-native-plugin-api.md) and
  [ADR 0002](docs/decisions/0002-native-plugin-system.md).
- **No Vimscript.** Legacy Vimscript (`.vim` files, the
  `eval.c` language) is an explicit non-goal. `vim.fn.*` is a hand-written
  compatibility shim, not an interpreter. Colorschemes must be Lua.
- **Not a neovim UI host.** There is no `ext_linegrid` / grid protocol, and
  attaching external neovim GUIs is not a goal. Clients receive a semantic `View`
  and lay out their own widgets. The RPC method names look like `nvim_*` but are
  nxvim's own protocol, not a compatibility surface.
- **Rope-backed, byte-indexed buffers** with a strict trailing-newline invariant
  (closer to vim's own byte-column model), and a **branching undo tree of
  full-rope snapshots** instead of neovim's diff-based `undo.c` — same branching
  semantics, different storage.
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

- **The `nx` API** — the `nx.*` config surface, the provider-based plugin API,
  and its server-owned surfaces (completion engine, fuzzy picker, statusline
  segments, snippet engine, tree docks) plus the built-in package manager:
  designed ([spec](docs/specs/2026-06-11-native-plugin-api.md)), not yet
  built. Until it lands, config rides the prelude's interim vim-shaped
  helpers, which are refactored into `nx` where useful and deleted where not.
- **Folds and macros** — not built.
- **Line wrapping** (`wrap`) and most **window-local options** (`cursorline`, …)
  beyond `number`/`relativenumber` and the horizontal-scroll options.
- **A broad options surface.** `:set` honors the search booleans, the
  number-gutter and horizontal-scroll window options, and the buffer-local
  indentation options — but the bulk of vim's hundreds of options are missing
  (writes to unsupported options are recorded but inert).
- **The `:map`-family ex-commands** — keymaps are set via `vim.keymap.set` /
  `nvim_set_keymap` instead (intentionally postponed).
- **Per-buffer user commands** — `nvim_buf_create_user_command` currently
  registers globally (the buffer argument is ignored).
- **Async primitives under `nx`** — `nx.spawn`/`nx.timer`/`nx.fs` will expose
  the existing timer/process machinery; the interim `vim.uv` timer surface
  does not grow (it is not part of the colorscheme glue).
- **LSP & treesitter edges** — semantic tokens and inlay hints are real but
  approximate (one group per cell, whole-document only, no range requests);
  Lua-driven treesitter indent (`indentexpr`) is deferred. Each approximation is
  tagged at its call site.

### No silent stubs

A core project rule: **everything unimplemented fails loud.** An unsupported path
raises with the name of what's missing (`nxvim: not implemented: <name>`) rather
than quietly returning a fake value — so a half-working feature never masquerades
as a whole one. You can enumerate exactly what a given config trips at runtime.

---

## Vide-coded

I started this project on a whim just to test the limits of current day vibe-coding.
I wanted to see how far I could go, I didn't expect the answer to turn out to be
as far as I wanted. I got so excited I decided to speed run it for enough features
to use it as my daily driver. I love neovim and I use(d?) it as my daily driver, so
nxvim keeps what I actually wanted from it — vim's editing language, Lua config, and
my colorscheme — and goes its own way on everything else, including a plugin system
designed for its own architecture. Just for the fun of it, I also
decided to implement a major feature that has been in the community wishlist for a 
long time, multiple cursors (see a previous section). That turned out to be much cooler
than I expected!

I've been using Claude Code with Opus 4.8 on the high effort most of the time and the
experience has been pretty frustration free. Claude only ever got stuck in one specific
feature, but I switched to max effort and asked it to refine the plan, then it
managed to advance. Of course Claude didn't do all of it by itself, I'm a pretty
seasoned software engineer, so I constantly steered it in the "right" direction and
I also made all architecture decisions, although with Claude's help. I also didn't
review the code, apart from skimming it while it was being generated as a coarse
sanity check, and interrupting and correcting Claude on eggregious mistakes. At times
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
reference — it is never built or linked, and is not needed to build nxvim.
