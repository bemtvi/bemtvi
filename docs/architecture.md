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
2. **Client-server, always.** The editor is a headless server; every UI is a
   client. There is no "embedded-only" code path.
3. **Async and responsive.** The UI never blocks on the editor and the editor
   never blocks on the UI. Slow work on one side cannot freeze the other.
4. **Rust-native, not a transliteration.** We mirror neovim's *organization*
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
| `nxvim`         | the `nvim` entry point                               | Wires an embedded server + the TUI client together over RPC.         |

Dependency direction is strictly one-way:

```
        nxvim (bin)
        /         \
 nxvim-server   nxvim-tui
   /   |   \         \
core  rpc  lua       rpc
       \____________/
```

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

(Display still assumes one cell per byte/char — no wide-char or tab-width
handling yet — so cursor placement for non-ASCII text is approximate for now.)

Undo is currently snapshot-based (cheap thanks to ropey's structural sharing);
it will move to a change-tree closer to neovim's `undo.c` as editing grows.

---

## Lua

nxvim embeds **Lua 5.1** via [mlua] (`lua51`, vendored) — the dialect LuaJIT,
and therefore neovim, is compatible with. Scripts run **inside the server**,
exactly as in neovim, and influence the editor through the same mechanisms RPC
clients use.

The bridge is currently narrow on purpose: `vim.cmd(...)` and
`vim.api.nvim_command(...)` queue ex-commands; `print(...)` /
`vim.api.nvim_echo(...)` capture output. After each chunk runs, the server
drains those queues into the editor. The end-state is for `vim.api.nvim_*` to
call the very same API functions remote clients invoke (`Lua → API → core`),
making Lua a first-class peer of RPC. Swapping the vendored Lua 5.1 for LuaJIT
is a build-level change isolated to `nxvim-lua`.

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
does for a user — not internal code structure. Two layers:

- **Integration tests** (e.g. [`crates/nxvim-server/tests/editing.rs`](../crates/nxvim-server/tests/editing.rs))
  start a real server, connect over real RPC, send vim key-notation via
  `nvim_input`, and assert on observable results: buffer contents
  (`nvim_buf_get_lines`), bytes written to disk, and the rendered screen. They
  treat the editor as a black box and exercise the whole stack
  (RPC → server → core → Lua) end to end.
- **e2e tests** (planned) will drive the actual `nxvim` binary through a PTY and
  assert on the terminal output a user would really see.

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

**Not yet implemented (roadmap):**

- Syntax highlighting / Treesitter and styled `View` regions.
- Multiple windows, tabs, and buffers; splits and the window layout tree.
- Vimscript (`eval.c`) and a broad Lua `vim.*` API surface.
- Options (`:set`), mappings (`:map`), registers beyond the unnamed register,
  search (`/`, `?`, `:s`), marks, folds, and macros.
- Wide-character / tab-width aware display and cursor placement.
- LuaJIT (in place of vendored Lua 5.1) and the full `vim.*` standard library.
- A native, non-terminal GUI client (e.g. for Windows).
- PTY-driven e2e tests of the binary.

[`tokio::io::duplex`]: https://docs.rs/tokio/latest/tokio/io/fn.duplex.html
[mlua]: https://docs.rs/mlua
