# Terminal in a buffer/window — implementation plan

## Context

nxvim has no way to run a shell or interactive program inside the editor. This adds
a neovim-style `:terminal` — a PTY-backed buffer whose contents are driven by a live
child process, with interactive input, ANSI/256-color rendering, scrollback, window-resize
→ PTY resize, and a *terminal-normal* mode for scrolling/yanking output. It works in the
native clients (PTY local) **and** in the browser (PTY on the `nxvim --daemon`, over
WebTransport).

The work follows the architecture's established split: a **native engine** (PTY transport +
a vt100 terminal emulator + per-cell color projection) under a **thin `nx.terminal` Lua
control surface** — the treesitter/LSP shape ([ADR 0001](../decisions/0001-native-engines-vendored-lua-apis.md),
[ADR 0002](../decisions/0002-native-plugin-system.md): "native engine for editor behavior, a
Lua scripting layer on top"). `nxvim-core` stays pure/synchronous.

### A cleaner layering (key realization from the edit-host work)

PTY work splits into two parts with different homes:

- **Byte transport** (read/write PTY bytes) — must be off the editor thread / async. Native:
  a `portable-pty` Send actor. Web: forwarded to the daemon over WebTransport (the daemon
  runs the real PTY).
- **Emulation** (bytes → vt100 grid → buffer lines + color spans) — pure CPU, no I/O. This
  lives in the **synchronous `EditHost`** (server-side, on the editor thread), so it is
  **shared by both the native server and the wasm build** (the web build reuses the same
  `EditHost` / redraw projection). `vt100` is pure Rust and compiles to wasm.

So the emulator is transport-agnostic; only the leg that ships raw bytes differs per build.

This reuses what already exists rather than inventing machinery:

1. **`portable-pty` (=0.9.0) + `vt100` (=0.16.2)** — already pinned in root `Cargo.toml`
   (used today only by the e2e harness; see `crates/nxvim/tests/e2e.rs`).
2. **The `pending_*` core→server seam** (`take_pending_opens`/`saves`/… in
   `crates/nxvim-server/src/effects.rs`, results re-entering via `inbound.rs`) — for the
   core→server terminal ops.
3. **The redraw highlight model** — per-row spans `[start,end,group,style_id]` + a deduped
   `StyleTable` palette (`redraw.rs`/`treesitter.rs::highlights_for`). A terminal grid's
   per-cell fg/bg/attrs is fully expressible here with **zero wire changes**; TUI/GUI/web
   clients need no edits.
4. **The Phase 6d proc-over-WebTransport leg** (commit 524d003) — the browser already spawns
   daemon processes: `RpcClient`, push routing, the async-park `liveProcs` gating, the wasm
   `HostEffects` seam (`proc_spawn`/`proc_kill`/`has_remote_proc`, `eh_take_proc_requests`,
   inbound `eh_proc_spawned`/`eh_proc_exited`), and the daemon's `serve_proc_daemon_on`. The
   web terminal adds a *streaming* sibling leg next to it.

## Design overview

```
key (Terminal mode) ─▶ core: Key→bytes ─▶ pending_terminal(Send) ─┐
:terminal (nx.terminal) ─▶ core: open_terminal ─▶ pending_terminal(Open) ─┤
                                                                  ▼ drained in settle
                                  ┌──────────────── transport leg (per build) ────────────────┐
                                  │ native: TerminalManager (portable-pty Send actor)          │
                                  │ web:    WebTransport → daemon's native terminal engine      │
                                  └─────────────────────────── raw PTY bytes ───────────────────┘
                                                                  ▼ inbound (both builds)
              EditHost terminal store: feed vt100::Parser ─▶ grid+scrollback ─▶ editor.terminal_update
                                                                  ▼
              redraw.rs: terminal buffer ⇒ grid cells → highlight spans + styles palette (shared)
```

## Phase 1 — Core: mode, buffer type, input, outbound queue (`nxvim-core`, pure/sync)

- **`src/mode.rs`** — add `Mode::Terminal` (label `"TERMINAL"`, short code). Terminal-insert:
  keystrokes go to the child.
- **`src/buffer.rs`** — terminal marker on `Buffer`, mirroring the `dir: Option<PathBuf>`
  explorer precedent (e.g. `terminal: Option<TerminalId>`). Non-file: `:w` refuses, `modified`
  stays false, not disk-backed; lines mirror the screen+scrollback text.
- **`src/editor/mod.rs`** —
  - `pending_terminal: Vec<TerminalOp>` + `take_pending_terminal()` (documented with the other
    `pending_*` fields ~line 770). `TerminalOp` = `Open { buf, argv, cwd, rows, cols }` |
    `Send { buf, bytes }` | `Kill { buf }`.
  - Inbound API: `terminal_update(buf, lines, cursor_row, cursor_col)` (replace screen lines +
    cursor; no undo, no `modified`) and `terminal_closed(buf, code)`.
  - `open_terminal(argv, cwd)`: create a terminal buffer in the current window, size `rows`/`cols`
    from the window text rect (core owns layout), set `Mode::Terminal`, enqueue `Open`.
  - **Input routing** in `input()` (~line 1067): before the mode `match`, when current buffer is a
    terminal and `mode == Terminal`, translate `Key`→bytes (pure `key_to_terminal_bytes`:
    printables, `\r`, `\x7f`, control bytes, arrows→`ESC[A/B/C/D`, …) and enqueue `Send`.
    `<C-\><C-n>` → `Mode::Normal` (terminal-normal, read-only navigable); `i`/`a`/`A` re-enter
    `Mode::Terminal`. Mode transitions are intrinsic to the mode machine (like Insert), so core.

## Phase 2 — Server-side vt100 engine, shared by native + wasm (`nxvim-server`, feature-agnostic)

- **New module `src/terminal.rs`** — an `EditHost`-owned `HashMap<BufferId, TermEmu>` where
  `TermEmu { parser: vt100::Parser, scrollback, last_size }`. Compiles in **both** builds
  (no `portable-pty`, no async here — pure emulation). Methods:
  - `terminal_feed(buf, &[u8])`: `parser.process(bytes)`, read `screen()`, build line strings +
    cursor, call `editor.terminal_update(...)`. Accumulate scrolled-off rows (Phase 6).
  - `terminal_resize(buf, rows, cols)`: `parser.set_size` (web also forwards to the daemon PTY).
  - grid accessor for the projector (Phase 4).
  - (Verify exact `vt100` 0.16 API during impl — `Parser::new/process/screen/set_size`,
    `Screen::cell/cursor_position`, `Cell::fgcolor/bgcolor/bold/...`; e2e.rs shows the pattern.)

## Phase 3 — Native PTY transport (`nxvim-server`, `#[cfg(feature = "native")]`)

- **`src/terminal.rs` (native section)** — `TerminalManager`, a `Send` actor modeled on
  `EventLoop` (`evloop.rs`) / `LspManager`: own command channel + a `TermEvent` result channel,
  per `BufferId` a `{ master, child, writer }`. `Open`: `native_pty_system().openpty(PtySize)`,
  `slave.spawn_command`, take writer + clone reader; a **blocking** reader thread
  (`spawn_blocking`) loops `read()` → `TermEvent::Data { buf, bytes }`, EOF → `Exit { buf, code }`.
  `Write`/`Resize`(`master.resize`)/`Kill`.
- **`src/lib.rs`** — construct `TerminalManager` beside `EventLoop`/`LspManager`; new
  `tokio::select!` arm `Some(ev) = term_events.recv() => host.on_term_events(...)`.
- **`src/effects.rs`** — drain `self.editor.take_pending_terminal()` in the settle/fixpoint,
  translating `TerminalOp` → `TerminalCommand` to the actor.
- **`src/inbound.rs`** — `on_term_events`: `Data` → `host.terminal_feed(buf, &bytes)`
  (Phase 2) then settle/redraw (coalesce a burst with `while try_recv()`); `Exit` →
  `editor.terminal_closed`. Resize: at redraw compare each terminal window's text rect to
  `last_size` and send `Resize` when it changed (server-side detection).

## Phase 4 — Color projection (`nxvim-server/src/redraw.rs` + `treesitter.rs`, shared)

- In `highlights_for` (`treesitter.rs:103+`): if the buffer has a `TermEmu`, **skip treesitter**
  and project the grid — per row emit per-cell `(fg,bg,sp,attrs)` from `Cell`, coalescing adjacent
  identical styles into spans `[start,end,group,style_id]`, interning each `Style` via the existing
  `StyleTable`. No new wire fields; clients render via their existing styling path. Runs in both
  builds (the wasm redraw projection is the same `EditHost` code).

## Phase 5 — Thin `nx.terminal` Lua control surface

- A bundled Lua control module (`nxvim-lua/src/prelude/`, exposed as `nx.terminal`) registers
  **`:terminal` / `:term [cmd]`** and calls `nx.terminal.open(opts)`. Per "Lua queues, core
  mutates", `nx.terminal.open` queues a Lua op drained in `effects.rs` into
  `Editor::open_terminal(argv, cwd)` (default shell from `$SHELL`/`%COMSPEC%` resolved
  server-side). `:terminal` routes through the existing unknown-cmd → `deferred_commands` →
  Lua-user-command path, so no core ex-command arm is needed. Mode keys stay intrinsic (Phase 1).

## Phase 6 — Scrollback (before the web phase; required to ship)

- **`src/terminal.rs`** — `TermEmu` keeps a `scrollback: VecDeque<ScrollLine>` capped at a limit
  (neovim's `scrollback`, default 10000). As rows scroll off the vt100 screen top, push them
  (text + per-cell styles) into `scrollback`; `terminal_update` projects `scrollback + visible
  screen` as the buffer's lines so terminal-normal `j`/`k`/`gg`/`G`/`<C-u>`/`<C-d>`/search/yank
  traverse history, while terminal-insert keeps the view pinned to the live bottom. The cursor
  maps into the screen region (offset by scrollback length).
- **Phase 4 projection** extended to color scrollback rows from their stored cell styles (the
  visible screen reads live `vt100` cells; scrolled-off rows read the captured styles).
- Verify the precise `vt100` 0.16 scrollback API (`Parser::new(rows, cols, scrollback)` /
  `Screen::set_scrollback`) vs. accumulating scrolled-off rows ourselves; pick whichever gives
  faithful per-cell styles for history, and capture the choice here.

  **Decision (implemented):** vt100 owns the scrollback (`scrollback_len = SCROLLBACK_CAP = 10000`,
  matching neovim); accumulating rows into its internal `VecDeque` is cheap. The buffer **always**
  mirrors the full `history ++ live screen` (like neovim) — *not* a screen-only-while-live /
  full-while-browsing flip. An earlier lazy design did flip, and was fast, but made the cursor and
  line numbers jump across `<C-\><C-n>` / `i` (the live screen-only buffer numbered the input line
  ~58 while the materialized buffer numbered it ~200, and the cursor didn't follow its content) —
  user-visibly broken. Always-full keeps them stable.

  Speed without the per-row cost comes from two rules:
  - **Project once per repaint, not per PTY chunk.** `terminal_feed` only runs the vt100 parser;
    `on_term_events` collects the buffers fed in a batch and calls `terminal_project` once after the
    (byte-budgeted) drain. So a flood is one projection per frame, not per chunk.
  - **Re-read the history mirror only when the scrollback actually scrolled.** `terminal_project`
    checks vt100's scrollback length against `last_held`; unchanged ⇒ rewrite only the live-screen
    region (`replace_from = history.len()`) — steady typing stays `O(screen)`. Changed ⇒ re-read
    the retained scrollback **text** via `read_scrollback_text` (vt100's view-window, paged) and
    splice from `replace_from = 0`. The text read is bounded by the cap (≤10000 rows); a flood does
    it once per frame (frames bounded by the 256 KiB repaint batching), so it stays fast.

  **History is text-only (monochrome); the live screen keeps full color (Phase 4).** Capturing
  per-cell styles for the whole scrollback every frame was measured at ~32s (debug) / unusable for
  500k — the per-cell reads explode. Dropping scrollback color is the deliberate trade; the visible
  live screen, where color matters most, is unaffected. Measured always-full: 100k ≈ 0.18s, 500k ≈
  0.67s (release), buffer capped near 10k rows. `Editor::terminal_update` takes `(replace_from,
  tail)` and splices that region, enabling the screen-only-region rewrite on no-scroll frames.

## Phase 7 — Web terminal over the daemon (`nxvim-edithost` + daemon, wasm-gated)

Extends the Phase 6d proc leg into a **streaming** terminal leg. The daemon (`nxvim --daemon`,
native) runs the real PTY via Phase 3's `TerminalManager`; the browser owns the vt100 emulation
(Phase 2 `EditHost`, shared) and the rendering.

- **Outbound (Sink → wasm FFI → worker → RpcClient → daemon):** wasm `HostEffects` gains terminal
  seam methods (`term_open`/`term_write`/`term_resize`/`term_kill`, gated by a
  `has_remote_proc`-style check — serverless OPFS still **fails loud** in the tick, a connected
  daemon enqueues). `apply` of `TerminalOp` routes through them (mirroring `apply_loop_op`'s
  Spawn/Kill branch). New `Sink` queues + `eh_take_terminal_requests`; `worker.mjs`
  `drainTerminalRequests` sends over the existing `RpcClient` (`web/rpc.mjs`).
- **Inbound (daemon pushes → worker → wasm FFI → EditHost):** new push kinds `term_data` /
  `term_exit` routed like 6d's `proc_*` pushes; FFI `eh_terminal_data(buf, ptr, len)` /
  `eh_terminal_exit(buf, code)` call `host.terminal_feed` / `editor.terminal_closed` (the wasm
  twins of the native `on_term_events` arm). The async-park machinery must keep the reader live
  while a terminal is open (extend the `liveProcs`/`armedWatches` gate with `liveTerms`).
- **Daemon (`nxvim-server`):** a `serve_terminal_daemon_on` analogous to `serve_proc_daemon_on`
  — the daemon's `TerminalManager` streams `Data`/`Exit` back as pushes and accepts
  write/resize/kill. (The daemon already runs the native engine from Phases 2–3.)
- **Native gating:** non-web `:terminal` continues to use the local `TerminalManager`. The
  serverless browser (OPFS, no daemon) reports `:terminal` is unavailable, loud.

## Out of scope / deferred (note in code, no silent stubs)

- Cursor-shape styling in the terminal cell, `TermOpen`/`TermClose` autocmds, `:terminal` split
  flags, the public `nx.spawn`/`nx.terminal` API polish beyond the funnel. Windows conpty works
  via portable-pty but is verified only on macOS/Linux here.

## Verification

Black-box RPC integration tests (`crates/nxvim-server/tests/terminal.rs`, `#[cfg(feature =
"native")]`, POSIX commands for hermeticity; add a "poll `nvim_buf_get_lines` until expected /
timeout" helper since PTY output is async — reuse the harness redraw-polling style, `serial_lock`):

- **Output**: terminal running `printf 'hello\n'`; poll until a line is `hello` + `[Process
  exited 0]`.
- **Interactive**: `cat`; `nvim_input("ihello\r")`; poll until the buffer echoes `hello`;
  `<C-\><C-n>` → assert `nvim_get_mode` normal + navigable.
- **Color**: `printf '\033[31mred\033[0m\n'`; assert the window's `redraw` carries a span whose
  resolved style has red `fg` (grid→palette projection).
- **Scrollback**: emit > screen-height lines; `<C-\><C-n>`, `gg`, assert the earliest line is
  reachable and `G` returns to the live bottom.
- **Mode/label**: `redraw` `mode_label == "TERMINAL"` in terminal-insert.

Web (Phase 7): a headless-Chromium harness mirroring `web/verify-proc.mjs` (real `nxvim --daemon
--listen` + WebTransport) — open a terminal, round-trip `printf` output to the rendered frame,
echo `cat` input, and confirm a kill ends it.

App run: `cargo run -p nxvim` → `:terminal` → interactive `ls`/resize/`<C-\><C-n>` scroll. Full
suite `cargo test --workspace`; `cargo clippy --all-targets -- -D warnings`; `cargo fmt --all`.
For the web build, `crates/nxvim-edithost/build.sh` + its verify harness.
