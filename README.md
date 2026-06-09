# nxvim

A [neovim](https://neovim.io) clone **100% vibe-coded in less than two weeks**
using Claude Code — modal editing, Lua plugins, treesitter highlighting, and
LSP, built on a fully async, client-server design.

nxvim is a headless, asynchronous editor **server** with thin UI **clients**
talking over nxvim's own msgpack-RPC. The editor logic lives in one place; the
terminal UI and the GPU GUI are just two clients of the same protocol.
The goal is to be as compatible with neovim as possible — including running
real, unmodified Lua plugins — while being an idiomatic, rust-native rewrite
rather than a C transliteration.

> **Status: early but substantial.** Day-to-day modal editing, splits, tabs,
> floating windows, treesitter highlighting, a real Lua plugin runtime, and LSP
> all work. Good enough to be a daily driver.

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
its own thread and runs the client on the main thread, joined by the same RPC a
remote client would use.

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

### Web build (`nxvim-web`) — runs entirely in the browser

`nxvim-web` compiles the editor core to **WebAssembly** and runs it **fully
client-side — there is no server**. The page (HTML/CSS, styled with Tailwind) is
the renderer and input layer; the editor itself runs in the tab. File open/save
go through the browser's **File System Access API**, the in-browser analogue of
the GUI's `:eo` / `:wo` dialogs — so you **really edit local files** (no upload,
no backend), and a static host (GitHub Pages, etc.) is enough to put it online.

It's a separate, wasm-targeted crate, deliberately **excluded from the Cargo
workspace** (so `cargo build/test --workspace` never touches it). Build it with
its own script:

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.123   # must match its Cargo.toml
crates/nxvim-web/build.sh                           # → crates/nxvim-web/web/
python3 -m http.server -d crates/nxvim-web/web 8000 # then open http://localhost:8000
```

In the page, `:eo` (or the **Open file** button) opens a file and `:w` / `:wo`
saves it back. Full write-back to the original file needs a Chromium-based
browser (the File System Access API); elsewhere, open still works and save
downloads a copy.

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
  demand. The full `vim.treesitter` Lua API (parsers, queries, predicates,
  injections) runs neovim's own vendored Lua on nxvim's primitives.
- **A real Lua plugin runtime** — Lua 5.1 (LuaJIT by default) running *inside*
  the server, a neovim-style config dir + runtimepath, `require`/`init.lua`,
  `vim.keymap.set`, user commands, autocmds, an async event loop
  (`vim.uv` timers, `vim.system`, `vim.schedule`/`vim.defer_fn`), and enough of
  the `vim.*` surface to run **real, unmodified lua plugins**, such as
  [catppuccin](https://github.com/catppuccin/nvim) colorscheme and
  [telescope.nvim](https://github.com/nvim-telescope/telescope.nvim).
- **LSP & diagnostics** — servers are configured and started from user Lua
  (`vim.lsp.config`/`vim.lsp.enable`); completion, hover, go-to, references,
  rename, diagnostics (underline / virtual text / signs / float), semantic
  tokens, and inlay hints are wired. Install the `nvim-lspconfig` plugin and all
  ~400 of its server configs load and start unmodified.
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

nxvim reads a config the way neovim does. On startup it resolves a **config
directory** — the first of `$NXVIM_CONFIG`, `$XDG_CONFIG_HOME/nxvim`, or
`~/.config/nxvim` — and sources `<config>/init.lua` before the first frame. The
**runtimepath** is that dir plus every `pack/*/start/*` plugin under it, so a
plugin checkout is drop-in:

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

That's the same API you'd write for neovim.

### Runnable examples

The [`examples/`](examples) directory has ~30 self-contained, end-to-end-verified
configs — one per feature (treesitter, LSP, floats, registers, tabs, mouse,
statusline, telescope, …). Each is a config dir you point nxvim at:

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
| `nxvim-web`      | Fully client-side WebAssembly build of the editor core (runs in the browser; no server). Excluded from the workspace. |
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

- **Lua plugins only — no Vimscript.** Legacy Vimscript (`.vim` plugins, the
  `eval.c` language) is an explicit non-goal. `vim.fn.*` is a hand-written
  compatibility shim, not an interpreter. Colorschemes and plugins must be Lua.
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
  Use the multicursor mode instead.
- **Different indent defaults.** `tabstop` defaults to **4**, with
  `shiftwidth=0` and `softtabstop=-1` (both "follow tabstop"), so one knob drives
  the indent width.
- **Cross-source event ordering is non-deterministic** (random `select!` pick for
  fairness) rather than neovim's deterministic multiqueue order — TBD if it affects
  correctness, or only which independent background source lands first.

### Not yet implemented (roadmap)

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
- **`vim.uv`/`vim.loop` beyond timers** — `new_pipe`, TCP, and event-based `fs_*`
  watchers are absent (so the TCP-transport gdscript LSP config is skipped).
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

## License

[Apache License 2.0](LICENSE).

`vendor/neovim` is a git submodule kept purely as a behavioral/source-layout
reference — it is never built or linked, and is not needed to build nxvim.
