# nxvim architecture

nxvim is a [neovim](https://neovim.io) clone written in Rust. The goal is to be
as compatible with vim/neovim as possible — including Lua extensions — while
adopting an idiomatic, rust-native, fully-async, client-server design.

A pristine copy of neovim is vendored at [`vendor/neovim`](../vendor/neovim) (a
shallow git submodule) and used purely as a behavioral and source-layout
reference. nxvim does not link against or embed any neovim code.

---

## Guiding principles

1. **Editing compatibility first.** Keystrokes, modes, ex-commands, options, and
   the Lua `vim.*` API should match neovim's observable behavior. When in doubt,
   the reference in `vendor/neovim` is the source of truth. *Note:* nxvim does
   **not** aim for neovim *UI/client* wire-compatibility — there is no
   `ext_linegrid` protocol and external neovim GUIs are not a target. The
   client↔server protocol is nxvim's own.
2. **Lua plugins, not Vimscript.** The objective is to run neovim's plugin
   ecosystem — but only plugins written in **Lua**. Supporting legacy Vimscript
   (`.vim` plugins, the `eval.c` language) is an explicit non-goal and is not on
   the roadmap. Compatibility work targets the Lua `vim.*` API surface that
   modern plugins depend on.
3. **Client-server, always.** The editor is a headless server; every UI is a
   client. There is no "embedded-only" code path.
4. **Async and responsive.** The UI never blocks on the editor and the editor
   never blocks on the UI. Slow work on one side cannot freeze the other.
5. **Rust-native, not a transliteration.** We mirror neovim's *organization*
   and *behavior*, not its C. We use a rope, ownership, enums, async tasks, and
   crates instead of globals, longjmp, and libuv callbacks.

---

## Crate layout

The workspace is split into crates that map onto neovim's `src/nvim/`
subsystems:

| nxvim crate     | neovim counterpart                                   | responsibility                                                        |
| --------------- | ---------------------------------------------------- | -------------------------------------------------------------------- |
| `nxvim-core`    | `buffer.c`, `normal.c`, `ops.c`, `edit.c`, `ex_docmd.c`, `undo.c`, `option.c` | The editor model: buffers, modes, motions, operators, ex-commands, undo, and the renderable `View`. **Pure & synchronous.** |
| `nxvim-rpc`     | `msgpack_rpc/`                                        | Async msgpack-RPC transport (nxvim's own protocol; msgpack is just the framing). |
| `nxvim-server`  | `main.c`, `event/`, `api/`                            | The headless server: owns the core + Lua, hosts the `nvim_*` API, runs the async main loop. |
| `nxvim-lua`     | `lua/`                                                | Embedded Lua 5.1 runtime and the `vim.*` standard library.           |
| `nxvim-tui`     | `tui/`                                                | The terminal UI **client**. A thin RPC client; owns no editor state. |
| `nxvim-ts`      | `lua/vim/treesitter/`, `tree_sitter/`                | The **treesitter syntax worker**: a separate, crash-isolated process that loads installable grammars and parses incrementally. Heavy C deps (`tree-sitter`, `libloading`) live here only. |
| `nxvim`         | the `nvim` entry point                               | Wires an embedded server + the TUI client together over RPC. Also re-invokes itself as the syntax worker (`--__ts-worker`). |

Dependency direction is strictly one-way:

```
        nxvim (bin) ───────────────┐
        /         \                │ spawns (process, not a crate edge)
 nxvim-server   nxvim-tui          ▼
   /   |   \         \         nxvim-ts (worker mode)
core  rpc  lua       rpc        /   \
       \____________/        tree-sitter  rpc
```

The syntax worker is a *process* edge, not a crate dependency: `nxvim-server`
spawns and supervises it but never links tree-sitter. See
[*Syntax highlighting*](#syntax-highlighting-treesitter) below and the design at
[`docs/superpowers/specs/2026-06-01-syntax-highlighting-design.md`](superpowers/specs/2026-06-01-syntax-highlighting-design.md).

`nxvim-core` has no async, no I/O beyond file read/write, and no transport
dependencies. That keeps the hard part — vim semantics — testable and portable,
and lets every front end share identical behavior.

---

## Client-server model

```
┌──────────────────────────┐         msgpack-RPC          ┌──────────────────────────┐
│  Client (nxvim-tui)       │  ───── nvim_input ─────────▶ │  Server (nxvim-server)    │
│  • crossterm input        │  ◀──── redraw events ─────── │  • nxvim-core (model)     │
│  • paints the grid        │  ───── nvim_command ───────▶ │  • nxvim-lua (vim.*)      │
│  • owns NO editor state   │  ◀──── responses ─────────── │  • nvim_* API surface     │
└──────────────────────────┘                              └──────────────────────────┘
        main thread                                              its own thread
```

The server is authoritative. The client sends input as vim key-notation
(`"i"`, `"<Esc>"`, `"<C-w>"`, …) and renders whatever grid the server pushes. A
client could be terminated and reconnected, or several clients could attach to
one server, without the server caring — exactly like neovim.

### Embedded vs. remote

The default `nxvim` invocation runs an **embedded** server: a headless editor on
its own OS thread, and the TUI client on the main thread, connected by an
in-process [`tokio::io::duplex`] pipe. Because the boundary is the same RPC used
for remote clients, the embedded and remote cases are *one code path*. Putting
the server on a separate thread (with its own single-threaded runtime) means UI
rendering can never stall editor processing, and vice versa.

### Async design

Both sides run on single-threaded tokio runtimes (the editor core, like
neovim's, is single-threaded; concurrency comes from async I/O, not parallel
mutation):

- `nxvim-rpc::connect` spawns independent reader and writer tasks, so encoding,
  decoding, and socket back-pressure never block the consumer.
- The **client** multiplexes terminal input and incoming redraws with
  `tokio::select!`. Keystrokes are sent the instant they arrive; redraws are
  painted as they come.
- The **server** processes one RPC message at a time against the (non-`Send`)
  editor and Lua state, while the RPC tasks keep the wire moving underneath it.

The editor and Lua state are intentionally `!Send` and live on a single thread,
which is why the server gets its own thread/runtime rather than being spawned
onto a shared pool.

---

## Protocols

### RPC framing (`nxvim-rpc`)

A standard msgpack-RPC framing — chosen because it's a good async binary
protocol, **not** for neovim interop. Messages are msgpack arrays:

- Request: `[0, msgid, method, params]`
- Response: `[1, msgid, error, result]`
- Notification: `[2, method, params]`

The method names happen to use the familiar `nvim_*` spelling (`nvim_input`,
`nvim_command`, `nvim_buf_get_lines`, `nvim_ui_attach`), but they are nxvim's
own methods with nxvim's own semantics — they are not a compatibility surface.

### View protocol (UI)

The core projects editor state into a [`View`](../crates/nxvim-core/src/view.rs):
the visible text rows, the cursor position, and the data a status/command line
need (mode, file name, modified flag, ruler, message, command-line text). The
server sends it as a single `redraw` notification carrying one msgpack map.

The `View` also carries the editor's **styled** regions: `selection`, a per-row
array of half-open screen-column spans `[start, end)` marking the visual-mode
selection (`None` for unselected rows). The core resolves the selection to
screen columns (so wide chars and tabs are already accounted for); `end` may run
one cell past a line's text to mark a selected newline, or to the viewport edge
for a linewise selection. The core owns *which* cells are in it.

**Color ownership lives on the server.** Originally the client owned *how* every
group looked (a hardcoded ANSI theme). A colorscheme (catppuccin) moves that
decision into the editor: a Lua theme defines the concrete color of every
highlight group via `nvim_set_hl` (see [*Lua*](#lua)). So the server now
**resolves** each group to a concrete style and the `redraw` carries styles, not
bare group names — matching real neovim, where highlight groups + `termguicolors`
live in the editor and the UI just paints attributes. Concretely the `redraw`
map carries:

- a per-frame `styles` palette — an array of resolved styles
  `{ fg, bg, sp, bold, italic, … }` with colors as 24-bit `0xRRGGBB` ints,
  deduped so identical styles cost one entry;
- the per-row `highlights` array (aligned with `lines`) of screen-column spans
  `[start, end, group, style_id]`, where `group` is the treesitter capture name
  and `style_id` indexes `styles` (or is `nil` when no colorscheme resolved it);
- a `chrome` map of editor-region → `style_id` for `Normal`, `LineNr`,
  `CursorLineNr`, `Visual`, `StatusLine`, and `EndOfBuffer`.

The server still owns *which* cells are in a group (byte offsets resolved to
screen columns via the same tab/wide-char `virtcol` the selection uses); it now
*also* resolves group → style. The client is a dumb truecolor renderer: it paints
the `Normal` background across the text area, themes the gutter/selection/status
from `chrome`, and colors each span from its `style_id`. When a span carries no
resolved style (no colorscheme loaded), the client falls back to a small built-in
theme, so default startup looks exactly as before. (See
[*Syntax highlighting*](#syntax-highlighting-treesitter).)

The same split governs the **number column**: the `View` carries the per-row
1-based buffer line numbers (`numbers`, `None` for `~` filler rows), the
`number`/`relativenumber` option flags, and the gutter width (`number_width`,
sized like vim's `numberwidth`). The core owns *what* each line's number is; the
client renders the gutter as its own ratatui widget — a horizontal split off the
left of the text area — and decides *how* it looks, computing the relative
offsets and the hybrid absolute-on-cursor-line formatting from that data. Text,
selection, and cursor columns are all measured from the text sub-area, so they
stay gutter-agnostic.

The **client owns layout**. It reserves two rows for chrome and renders three
ratatui-native widgets — text area, status line, command line — with a ratatui
`Layout` (see [`nxvim-tui`](../crates/nxvim-tui/src/lib.rs)). Because layout is
the client's job, the client tells the server only how tall the *text area* is
(`nvim_ui_attach`/`nvim_ui_try_resize` carry the text-viewport height), so the
core scrolls against the right window size. There is no grid, no cell encoding,
and no `ext_linegrid`.

---

## Text model

Buffers are backed by a [ropey](https://docs.rs/ropey) 2.0 rope (`nxvim-core`'s
`Buffer`). Indices are **byte offsets** — ropey 2.0's native metric, and the
same column model vim uses — with lines tracked via ropey's `LineType::LF_CR`
(so both Unix `\n` and DOS `\r\n` files split correctly). Editing operations
snap byte ranges to char boundaries (`floor`/`ceil_char_boundary`) so a
multi-byte character can never be split; for ASCII this is all a no-op. The key
invariant: **the rope always ends with a trailing `\n`**, so an empty buffer is
`"\n"` (one empty line) and the editable line count is `rope.len_lines() - 1`.
The phantom final line is never displayed or edited.

Motion steps by **grapheme cluster** and the cursor's display column is computed
as a **virtual column** (wide characters via `unicode-width`, tabs expanded to a
fixed `tabstop` of 8), carried in the `View` as `cursor_screen_col`. `cursor.col`
remains a byte offset (what `nvim_win_get_cursor` returns); the TUI expands tabs
when painting so glyphs line up with that virtual column.

Undo is currently snapshot-based (cheap thanks to ropey's structural sharing);
it will move to a change-tree closer to neovim's `undo.c` as editing grows.

---

## Buffers

The editor holds **multiple open buffers** and switches the one window between
them. `nxvim-core`'s `Editor` separates the two concerns vim keeps apart:

- **Buffer state** (the "file"): the rope text, path, `modified`,
  `changedtick`, the edit journal, **and** per-buffer undo/redo history. These
  live in an `OpenBuffer` (the text `Buffer` plus its undo stacks and the
  cursor/scroll position saved while the buffer is not current), stored in a
  `BufferStore` keyed by a monotonic, 1-based `BufferId` that is never reused.
- **Window state** (the "view"): the live cursor, scroll `top`, mode, and
  pending-input state stay on `Editor`, alongside `current` (the shown buffer)
  and `alternate` (vim's `#`). The register and options are still **global** —
  buffer-local options are a follow-up.

`Editor::buffer()` / `buffer_mut()` resolve the current buffer through the
store, so the editing code is oblivious to how many buffers are open. There is
always at least one buffer; deleting the last leaves a fresh `[No Name]`.

The surface is the usual vim set: `:e` (open-or-switch, reusing the throwaway
`[No Name]`), `:enew`, `:ls`/`:buffers`, `:b{N|name|#}`, `:bnext`/`:bprev`/
`:bfirst`/`:blast`, `:bdelete`/`:bwipeout`, `<C-^>`, and multi-buffer
`:wall`/`:qall`. The RPC layer mirrors neovim's `nvim_list_bufs`,
`nvim_get_current_buf`, `nvim_set_current_buf`, `nvim_create_buf`,
`nvim_buf_get_name`, and a buffer-addressed `nvim_buf_get_lines`.

`:q` quits the editor, but only when nothing would be lost: with a modified
buffer anywhere it refuses, switches the window to that buffer, and reports
`E37` (so you see what's blocking) — matching neovim's behavior when exiting the
last window with `hidden` buffers. `:q!` exits unconditionally. With one window
`:q` and `:qa` coincide; real windows will split them later.

The treesitter worker tracks each buffer independently: the server keeps a
`SyntaxState` per `BufferId`, routes `ts_highlights` replies by id, and sends
`ts_close` when a buffer is deleted — so switching back to a buffer paints from
its cached parse instead of re-opening. (See
[*Syntax highlighting*](#syntax-highlighting-treesitter).)

What's still missing is **windows**: splits, the layout tree, and per-window
cursors. With one window, each buffer's last cursor/scroll is saved on switch
and restored on return.

---

## The message panel

Multi-line, browsable output — `:messages` (the message history) and `:ls` (the
buffer list) — lives in a **panel**: a bottom-docked, read-only, navigable
region that is explicitly **not** a vim window (there is still one text window
onto one buffer). It is nxvim-native, closest in spirit to neovim's quickfix
window but simpler: a transient overlay that grabs input focus while open.

- **State lives in the core.** `Editor` holds an `Option<Panel>` (title, content
  lines, a cursor line, a scroll `top`, and a requested height). While a panel is
  open, `Editor::input` routes every key to it instead of to the buffer, so the
  usual vertical motions (`j`/`k`/`gg`/`G`/`<C-d>`/`<C-u>`, arrows, `Home`/`End`)
  scroll the panel; `q`/`Q`/`<Esc>` close it and refocus the text window. The
  buffer is untouched throughout. A closed (or replaced) panel is retained as a
  single `last_panel` snapshot, so **`:panelopen`** brings the most recent panel
  back with its content and selection intact — e.g. reopening an LSP references
  list after it was dismissed.
- **Panels can navigate.** A panel may carry a per-line jump target (`set_panel_targets`,
  a location list like LSP references/diagnostics): `<CR>` on a target line
  `jump_to`s it (open-or-switch buffer + set cursor) and closes the panel. The
  targets are part of the `Panel`, so they ride along in the `:panelopen`
  snapshot — a reopened list still jumps. A line without a target falls back to
  the select path below.
- **The editor splits the height it's told.** The client still reports only the
  text-viewport height (terminal minus the two chrome rows); the editor subtracts
  the panel's rows from that, so `text_height()` — and therefore the `lines` it
  projects — already account for the panel. No extra resize round-trip is needed:
  the redraw reports the panel's clamped content height, and the client lays out
  `height + 1` rows (content + a `─ Title ──[X]─` title bar) from it, **below the
  status line** and above the command row, leaving the text area at exactly the
  row count the core projected.
- **A message history feeds it.** `Editor::echo` is the one place a user-facing
  message is set; it records each line in a `messages` history (the backing store
  for `:messages`) as well as showing it on the message line. The server routes
  its own messages (errors, captured `print`/`nvim_echo`) through the same call.
- **It's scriptable.** `Editor::open_panel`/`set_panel_lines`/`set_panel_cursor`/
  `close_panel` are public, exposed two ways: a Lua `vim.panel.open(title, lines)`
  / `set_lines(lines)` / `set_cursor(line)` / `close()` table (queued as
  `PanelOp`s and drained by the server, the same "Lua queues, core mutates" flow
  as `vim.cmd`/`nvim_set_hl`), and the `nxvim_panel_open` / `nxvim_panel_set_lines`
  / `nxvim_panel_set_cursor` / `nxvim_panel_close` (plus `nxvim_panel_is_open`)
  RPC methods, which manipulate the core directly so they work even while the
  panel holds input focus. So a plugin can use the panel as a general output
  surface, not just for `:messages`/`:ls`.
- **It opens on a chosen line.** `open_panel` takes an initial cursor; the panel
  scrolls so that line is visible. Scripts pass it as a fourth argument
  (`vim.panel.open(title, lines, on_select, line)`, 1-based to match the
  `on_select` index, or the `cursor` param on `nxvim_panel_open`, 0-based) and can
  move it later with `set_cursor`. The two built-ins use this: `:messages` opens
  scrolled to the end with the newest line selected, and `:ls` opens with the
  current buffer selected.
- **`<CR>` is a scriptable callback.** Pressing Enter on a line of a
  *select-enabled* panel records `(index, line)` in the core (`panel_selects`);
  the server drains it — the reverse of the queue flow, like an autocmd —
  invoking the Lua `on_select(line, index)` handler (kept in the Lua registry)
  and emitting an `nxvim_panel_select` RPC notification for non-Lua clients.
  Selection is opt-in per panel (`vim.panel.open(title, lines, on_select)` /
  `vim.panel.on_select(fn)`, or `want_select` on `nxvim_panel_open`): the
  built-in `:messages` viewer opts out, so a stale handler never fires on it.
  `:ls` itself rides this path — it opens its panel, then queues
  `vim.panel.on_select(vim._panel_select_buffer)` (a prelude helper that parses
  the buffer number off the selected line, jumps to it, and closes the list), so
  pressing `<CR>` on a listed buffer switches to it.
- **The `[X]` is clickable.** The client enables mouse capture and hit-tests a
  left-click against the title bar's close button (`close_button`), sending the
  close key when hit — the only mouse interaction in the client today.

The redraw carries the panel as a `panel` map (`title`, `lines`, `cursor_row`,
`height`), `Nil` when none is open; the client draws the editing cursor inside
the panel while it has focus.

---

## Lua

nxvim embeds **Lua 5.1** via [mlua] (`lua51`, vendored) — the dialect LuaJIT,
and therefore neovim, is compatible with. Scripts run **inside the server**,
exactly as in neovim, and influence the editor through the same mechanisms RPC
clients use. The VM loads the full safe stdlib **plus `debug`** (real plugins
call `debug.getinfo` to locate their own install dir, and neovim exposes it),
and the prelude ships a LuaJIT-compatible `bit` library since PUC Lua 5.1 lacks
one. The backend is a Cargo feature: `nxvim-lua` exposes `lua51` (default,
vendored PUC Lua 5.1) and `luajit`, threaded up unchanged through `nxvim-server`
and the `nxvim` binary. Build the whole stack on LuaJIT for benchmarking with
`cargo build -p nxvim --no-default-features --features luajit` (likewise
`cargo test -p nxvim-server --no-default-features --features luajit`). The two
mlua version features are mutually exclusive, so `[workspace.dependencies].mlua`
selects only `vendored` and each crate sets `default-features = false` on the
inter-crate deps to keep the default `lua51` from leaking into a `luajit` build.

**Effects flow through queues.** `vim.cmd(...)` / `vim.api.nvim_command(...)`
queue ex-commands; `print(...)` / `vim.api.nvim_echo(...)` capture output;
`vim.api.nvim_set_hl(...)` queues highlight-group definitions. After each chunk
runs, the server drains those queues into the (pure, synchronous) core — Lua
never mutates the editor directly. The end-state is for `vim.api.nvim_*` to call
the very same API functions remote clients invoke (`Lua → API → core`).

**A plugin runtime, not just a bridge.** nxvim resolves a config dir and
**runtimepath** the way neovim does (`$NXVIM_CONFIG` / `$XDG_CONFIG_HOME/nxvim` /
`~/.config/nxvim`, plus `pack/*/start/*` plugin discovery and `$NXVIM_RUNTIMEPATH`
for tests), seeds `package.path` from it so `require` resolves plugin modules,
and sources `<config>/init.lua` at startup — before the first frame. The `vim.*`
surface real plugins reach for is provided as a bundled **Lua prelude**
(`nxvim-lua/src/prelude.lua`, the analogue of neovim's `runtime/lua/vim/`):
`vim.tbl_*`, `vim.split`, `vim.inspect`, `vim.g`/`vim.o`/`vim.opt`/`vim.env`,
`vim.notify`, `vim.log`, user commands, and autocmds; FS/env-touching helpers
(`vim.fn.stdpath`/`getftime`/`mkdir`, …) are Rust-backed. `:colorscheme <name>`
sources `colors/<name>.lua` off the runtimepath and fires the `ColorScheme`
autocmd. This is enough to run the **real, unmodified
[catppuccin](https://github.com/catppuccin/nvim)** colorscheme: dropped onto the
runtimepath, its `setup()` compiles the highlight table to Lua bytecode under
`stdpath("cache")` and `load()` populates the highlight registry via
`nvim_set_hl` — the same mechanics as under neovim. See
[`docs/getting-started.md`](getting-started.md) to set it up.

---

## Syntax highlighting (treesitter)

nxvim is **treesitter-native only** — there is no regex/`syntax.vim` highlighter.
All highlighting comes from [tree-sitter](https://tree-sitter.github.io) grammars
and their `highlights.scm` queries, and is built so it **can never crash, stall,
or even slow the editor**:

- **A separate, supervised process.** Tree-sitter grammars are compiled C; a
  buggy one can *segfault*. So parsing runs in a child process — the `nxvim`
  binary re-invoked as `nxvim --__ts-worker` (`nxvim-ts`). The server is its RPC
  client (same `nxvim-rpc` framing) and **respawns it** if it dies, with a
  circuit breaker against crash-loops. The editor never links tree-sitter and
  never blocks on the worker: redraws go out immediately with whatever spans are
  cached, and the worker's `ts_highlights` replies arrive asynchronously as their
  own redraw.
- **Installable grammars.** Grammars are not bundled; they load dynamically by
  filetype from a data directory laid out exactly like neovim's
  (`<data>/parser/<lang>.so`, `<data>/queries/<lang>/highlights.scm`), so an
  existing nvim-treesitter tree is drop-in usable.
- **Incremental parsing.** The worker keeps a shadow buffer and a persistent
  parse tree per buffer; the editor sends only **edit deltas** (`InputEdit`), so
  per-edit cost scales with the edit, not the file — huge files stay responsive.
  This rides a `Buffer` edit journal in `nxvim-core` (`changedtick` +
  `BufferEdit`s, drained by the server each frame).

The `View`/`redraw` carries the result as a per-row `highlights` array (see the
*View protocol* above): screen-column spans tagged with a capture-group name and
a resolved `style_id`. The server owns *which* cells are which group **and**
resolves group → concrete style (a colorscheme's `nvim_set_hl` table, or the
capture-fallback chain); the client paints the truecolor it is handed, falling
back to a small built-in theme only when no colorscheme resolved a span. Full
designs:
[syntax highlighting](superpowers/specs/2026-06-01-syntax-highlighting-design.md)
and
[the catppuccin colorscheme](superpowers/specs/2026-06-01-catppuccin-colorscheme-design.md).

---

## Cross-platform & the future GUI

nxvim targets all major OSes (Linux, macOS, Windows). The dependency choices are
deliberately portable: `crossterm` for the terminal, `ropey`, `tokio`, and
`rmpv` are all cross-platform, and the in-process transport uses no OS-specific
IPC.

The terminal client is built on [ratatui](https://ratatui.rs) (over crossterm).
Because every front end is just a client of nxvim's own RPC, a **native GUI** —
notably a non-terminal GUI on Windows — is a future client crate (e.g.
`nxvim-gui`) consuming the same `View` protocol, with zero changes to the server
or core. **For now nxvim is terminal-only;** the GUI is explicitly deferred, but
the architecture is built so it slots in without a rewrite.

---

## Testing philosophy

nxvim **does not use unit tests.** We test *functionality* — what the editor
does for a user — not internal code structure. Coverage is layered cheap →
faithful, so the broad, fast tiers localize most failures and the slow PTY tier
stays thin:

- **RPC / `View` integration** ([`crates/nxvim-server/tests/editing.rs`](../crates/nxvim-server/tests/editing.rs))
  start a real server, connect over real RPC, send vim key-notation via
  `nvim_input`, and assert on observable results: buffer contents
  (`nvim_buf_get_lines`), cursor, bytes written to disk, and the semantic
  `redraw` `View`. They treat the editor as a black box and exercise the whole
  stack (RPC → server → core → Lua) end to end.
- **Tier 1 — client paint & key translation** ([`crates/nxvim-tui/tests/`](../crates/nxvim-tui/tests/))
  render a known `View` into a cell grid via ratatui's `TestBackend`
  (`nxvim_tui::paint`) and assert on the painted cells, and test the
  crossterm-`KeyEvent`→key-notation translation (`nxvim_tui::encode_key`)
  directly. Fast and fully deterministic — no process, no timing.
- **Tier 2 — full-stack screen** ([`crates/nxvim/tests/screen.rs`](../crates/nxvim/tests/screen.rs))
  drive the real server in-process, capture the real `redraw`, paint it with the
  real client, and assert on the cell grid — the deterministic "what the user
  sees" workhorse. Also asserts the non-blocking guarantee (a UI that never
  drains redraws can't stall the editor).
- **Tier 3 — PTY smoke** ([`crates/nxvim/tests/e2e.rs`](../crates/nxvim/tests/e2e.rs))
  drive the actual `nxvim` binary through a pseudo-terminal (`portable-pty`),
  send real key bytes, and assert on the parsed terminal screen (`vt100`) a user
  would really see — proving real crossterm decode, real terminal escapes, and
  process startup/args. Deliberately small; the slow/flaky surface. Includes a
  responsiveness check that input typed during a slow editor op (`:sleep`) is
  buffered and applied once the editor wakes.

A bug should be reproducible as "these keystrokes produced the wrong text or
screen," and that is exactly the shape of these tests.

---

## Compared to neovim

**Similarities (by design):**

- Headless, authoritative editor server with thin UI clients.
- Single-threaded editor core; concurrency via async I/O.
- Lua 5.1 scripting running inside the server.
- Source organization mirroring neovim's subsystems (one crate per area).
- Vim modes, motions, operators, counts, registers, and ex-commands.

**Differences (intentional, rust-native):**

- Rust crates and ownership instead of C translation units and globals; no
  libuv (tokio), no longjmp error handling (Result/enums).
- **Not** a neovim UI host: no `ext_linegrid`, no grid protocol, no goal of
  attaching external neovim GUIs. The client gets a semantic `View` and lays out
  ratatui widgets per region itself.
- Rope-backed (ropey 2.0), byte-indexed buffers with a strict trailing-newline
  invariant — closer to vim's own byte-column model.
- Snapshot undo rather than a persistent undo tree — for now.
- **Treesitter highlighting in a separate, crash-isolated process** with
  installable grammars and incremental parsing (see
  [*Syntax highlighting*](#syntax-highlighting-treesitter)) — neovim parses
  in-process on the main loop.

**Not yet implemented (roadmap):**

- `:TSInstall`-style grammar fetch & compile (grammars are loaded from the data
  dir today; installing them there is manual / a follow-up), treesitter
  injections, and a `:set`-driven highlight toggle.
- Multiple **windows**, tabs, and splits; the window layout tree. (Multiple
  *buffers* are implemented — see [*Buffers*](#buffers) — but there is still
  exactly one window onto one buffer.)
- A broader Lua `vim.*` API surface. The runtimepath, `require`, `init.lua`,
  `nvim_set_hl`, and `:colorscheme` are in place — enough to run the real
  catppuccin colorscheme unmodified (see [*Lua*](#lua)) — but the surface grows
  only as plugins demand it. Known gaps for richer plugins: `vim.treesitter` is a
  stub (nxvim highlights out-of-process), `vim.keymap`/`vim.api.nvim_set_keymap`,
  `vim.loop`/`vim.uv`, the per-window API, and the LSP client. Legacy Vimscript
  (`eval.c`) is **not** on the roadmap — see guiding principle 2.
- A broad options surface. `:set` exists, but only `number`/`relativenumber`
  (the line-number column) are honored so far, and options are still global —
  **buffer-local options** are the next gap. Also mappings (`:map`), registers
  beyond the unnamed register, search (`/`, `?`, `:s`), marks, folds, and macros.
- LuaJIT (in place of vendored Lua 5.1) and the full `vim.*` standard library.
- A native, non-terminal GUI client (e.g. for Windows).

[`tokio::io::duplex`]: https://docs.rs/tokio/latest/tokio/io/fn.duplex.html
[mlua]: https://docs.rs/mlua
