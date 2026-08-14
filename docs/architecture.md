# bemtvi architecture

bemtvi is a modal, vim-style editor written in Rust: vim's editing language
(keystrokes, modes, ex-commands) on an idiomatic, rust-native, fully-async,
client-server design. bemtvi is its own editor, not a
[neovim](https://neovim.io) build: editing behavior tracks vim/neovim's
observable behavior, but every API is bemtvi's own — configuration and
extensibility live in the `btv.*` Lua namespace
([the `btv` API](specs/2026-06-11-native-plugin-api.md);
[ADR 0002](decisions/0002-native-plugin-system.md)) — and the `vim.*` that
remains is small and closed: a whitelist of muscle-memory aliases over `btv`
(`vim.g`, `vim.o`, …, plus the highlight-registration helpers a colorscheme
touches), so config and colorschemes can be written in familiar terms. They are
aliases, not a separate API.

A pristine copy of neovim is vendored at [`vendor/neovim`](../vendor/neovim) (a
shallow git submodule) and used purely as a behavioral and source-layout
reference. bemtvi does not link against or embed any neovim code.

---

## Guiding principles

1. **Editing compatibility first.** Keystrokes, modes, ex-commands, and options
   should match vim/neovim's observable behavior. When in doubt, the reference
   in `vendor/neovim`
   is the source of truth. *Note:* bemtvi does **not** aim for neovim
   *UI/client* wire-compatibility — there is no `ext_linegrid` protocol and
   external neovim GUIs are not a target. The client↔server protocol is bemtvi's
   own.
2. **A native plugin system; `btv.*` is the only API.**
   Extensibility is bemtvi's own provider-based plugin API
   ([the `btv` design](specs/2026-06-11-native-plugin-api.md),
   [ADR 0002](decisions/0002-native-plugin-system.md)): the server owns every
   UI surface and the frame; plugins supply data and behavior, asynchronously.
   Configuration is the same namespace: `init.lua` is written against `btv.*`.
   A closed whitelist of **muscle-memory aliases** (`vim.g`, `vim.o`/`vim.opt`,
   `vim.cmd`, `vim.keymap.set`, autocmd / user-command / highlight
   registration, `vim.notify`, the pure `vim.tbl_*`-style helpers, … — the
   canonical list is [ADR 0002](decisions/0002-native-plugin-system.md))
   maps 1:1 onto the same `btv` objects, so config can be written in familiar
   muscle-memory terms — aliases, not an API;
   beyond them there is no `vim.*` API. bemtvi does not host neovim plugins —
   they are
   imperative programs written
   against neovim's runtime model (synchronous re-entrant state, blocking
   reads, libuv as a public API, frame-time render hooks), which this
   snapshot + effect-queue, client-server design deliberately is not.
   Colorschemes are bemtvi's own: a colorscheme is Lua that registers highlight
   groups through the `btv` highlight API (and its `vim.*` aliases), nothing
   more. Supporting legacy Vimscript
   (`.vim` plugins, the `eval.c` language) is likewise an explicit non-goal.
3. **Dogfood the plugin API: first-party features are `btv` plugins.**
   Everything that *can* reasonably be built as an `btv.*` Lua plugin *is* built
   as one. bemtvi's own surfaces — the fuzzy picker, completion, statusline
   segments, snippets, tree docks, and the features layered on top of them — are
   the plugin API's first and most demanding consumer, shipped as bundled `btv`
   plugins rather than as bespoke Rust. If a feature can't be expressed against
   `btv.*`, that is a gap in the API to close, not a reason to reach behind it —
   so the API is proven by the editor that depends on it. The line is the one
   the architecture already draws: Rust owns the pure core, the frame /
   renderer, and the native engines (treesitter, LSP, regex) — a Lua plugin
   can't *be* the renderer or the synchronous core; the orchestration, UX, and
   behavior composed on top of those primitives are Lua. "Makes sense" is the
   only escape hatch, and it means a genuine engine/frame/perf constraint, not
   convenience.
4. **Client-server, always.** The editor is a headless server; every UI is a
   client. There is no "embedded-only" code path.
5. **Async and responsive.** The UI never blocks on the editor and the editor
   never blocks on the UI. Slow work on one side cannot freeze the other.
6. **Rust-native, not a transliteration.** We mirror neovim's *organization*
   and *behavior*, not its C. We use a rope, ownership, enums, async tasks, and
   crates instead of globals, longjmp, and libuv callbacks.

---

## Crate layout

The workspace is split into crates that map onto neovim's `src/nvim/`
subsystems:

| bemtvi crate     | neovim counterpart                                   | responsibility                                                        |
| --------------- | ---------------------------------------------------- | -------------------------------------------------------------------- |
| `bemtvi-core`    | `buffer.c`, `normal.c`, `ops.c`, `edit.c`, `ex_docmd.c`, `undo.c`, `option.c` | The editor model: buffers, modes, motions, operators, ex-commands, undo, and the renderable `View`. **Pure & synchronous.** |
| `bemtvi-rpc`     | `msgpack_rpc/`                                        | Async msgpack-RPC transport (bemtvi's own protocol; msgpack is just the framing). |
| `bemtvi-server`  | `main.c`, `event/`, `api/`                            | The headless server: owns the core + Lua, hosts the `nvim_*` API, runs the async main loop. A library, embedded on its own thread by the `bemtvi` / `bemtvi-gui` binaries; the `--daemon` role reuses it as the remote fs/process half of the edit-host split. |
| `bemtvi-lua`     | `lua/`                                                | Embedded Lua runtime (vendored PUC Lua 5.4, the single backend) and the `btv.*` Lua prelude (plus its bounded `vim.*` alias layer). |
| `bemtvi-tui`     | `tui/`                                                | The terminal UI **client**. A thin RPC client; owns no editor state. |
| `bemtvi-ts`      | `tree_sitter/`                                       | The **in-process treesitter engine**: an ordinary library that loads installable grammars and parses incrementally, implementing `bemtvi-core`'s `SyntaxEngine` trait. Heavy C deps (`tree-sitter`, `libloading`) live here only. |
| `bemtvi-lsp`     | `lsp/`                                                | The native **LSP client**: protocol, transport, manager — bemtvi's own stdio spawning, driving the in-core editing features. `bemtvi-server/src/lsp/` is the editing-loop glue on top. |
| `bemtvi-regex`   | `regexp.c`, `regexp_nfa.c`                            | The **vendored vim regexp engine** compiled as C (engine-global mutex): Lua's `vim.regex` / `vim.fn.substitute`, and `/` search + `:s` under `:set regexsyntax=vim`. |
| `bemtvi-view`    | (UI layer)                                           | Frontend-neutral decode/input layer (`View`, `Style`, `Key`, `notation`, paste encoding) shared by the native clients. |
| `bemtvi-gui`     | (a GUI frontend)                                     | The native GUI **client** on winit + wgpu + glyphon; the GUI sibling of `bemtvi-tui`, consuming the same `View`. |
| `bemtvi-edithost`| —                                                    | The fully client-side **WebAssembly** build of the whole editor (core + Lua + server tick) in a Web Worker; excluded from the Cargo workspace. See [*The web build*](#the-web-build--a-fully-client-side-webassembly-editor). |
| `bemtvi-test-harness` | —                                               | The shared black-box integration-test harness (a `publish = false` dev-dependency of `bemtvi` and `bemtvi-server`): spawns a real server over an in-process RPC pipe and drives it as a UI client would. See [*Testing philosophy*](#testing-philosophy). |
| `bemtvi`         | the `nvim` entry point                               | Wires an embedded server + the TUI client together over RPC. |

Dependency direction is strictly one-way:

```
        bemtvi (bin)
        /         \
 bemtvi-server   bemtvi-tui
  / | | \         /    \
core rpc lua ts  rpc  view
      \_______/
       tree-sitter
```

The diagram shows the principal spine; the same one-way graph also carries the
edges it elides — `bemtvi` and `bemtvi-server` both onto `bemtvi-lsp`, `bemtvi-lua`
onto `bemtvi-ts` (the treesitter query/control bridge), `bemtvi-gui` onto
`bemtvi-server` + `bemtvi-view`, and `bemtvi-lua` onto `bemtvi-regex` (with
`bemtvi-core`'s optional `vim-regex` feature — enabled by `bemtvi-server` —
putting the same engine behind `:set regexsyntax=vim`).

The treesitter engine is a normal crate dependency now: `bemtvi-server`
constructs it and installs it on the editor (which owns a `Box<dyn SyntaxEngine>`
defined in `bemtvi-core`), then queries it **synchronously** at redraw. See
[*Syntax highlighting*](#syntax-highlighting-treesitter) below and the design at
[`docs/specs/2026-06-06-in-process-treesitter-and-indentation-design.md`](specs/2026-06-06-in-process-treesitter-and-indentation-design.md).

`bemtvi-core` has no async, no I/O beyond file read/write, and no transport
dependencies. That keeps the hard part — vim semantics — testable and portable,
and lets every front end share identical behavior.

---

## Client-server model

```
┌──────────────────────────┐         msgpack-RPC          ┌──────────────────────────┐
│  Client (bemtvi-tui)      │  ───── btv_input ─────────▶   │  Server (bemtvi-server)   │
│  • crossterm input       │  ◀──── redraw events ─────── │  • bemtvi-core (model)    │
│  • paints the grid       │  ───── btv_command ───────▶   │  • bemtvi-lua (btv.*)      │
│  • owns NO editor state  │  ◀──── responses ──────────  │  • nvim_* API surface    │
└──────────────────────────┘                              └──────────────────────────┘
        main thread                                              its own thread
```

The server is authoritative. The client sends input as vim key-notation
(`"i"`, `"<Esc>"`, `"<C-w>"`, …) and renders whatever grid the server pushes. A
client could be terminated and reconnected, or several clients could attach to
one server, without the server caring — exactly like neovim.

### Embedded vs. remote

The default `bemtvi` invocation runs an **embedded** server: a headless editor on
its own OS thread, and the TUI client on the main thread, connected by an
in-process [`tokio::io::duplex`] pipe. The boundary is the same RPC every client
speaks, so the embedded and edit-host-split cases are *one code path*. Putting
the server on a separate thread (with its own single-threaded runtime) means UI
rendering can never stall editor processing, and vice versa.

There is deliberately **no** "whole editor runs remote, thin client local" role.
That topology — every keystroke round-tripping to a server on the far side of the
wire — is structurally laggy, and bemtvi has a better answer for editing on another
machine: open an SSH session and run `bemtvi` there (the editor is then fully local
to that host), or use the edit-host split below.

The **inverse remote** topology — the *edit-host / daemon split*, where the editor +
Lua run locally and only an `bemtvi --daemon` serving fs/process lives on the remote
— moves the network boundary *below* the editor, so typing, motions, operators, and
undo are all local (zero round-trips) and only fs/process/watch/LSP traffic crosses
the wire. The local half (`bemtvi --connect-daemon [file]`, or a `bemtvi://…` URI for
the QUIC transport) is the same embedded editor as the default role, with its host
seams (`bemtvi_core::HostFs`, `bemtvi_server::HostProc`, the async `HostFsAsync`, the
LSP transport, and the Lua-facing `bemtvi_lua::LuaFs`) pointed at the daemon instead
of the local disk. The **GUI** also reaches this split at runtime via `:connect` —
`:connect [user@]host[:port][/file]` spawns `ssh … bemtvi --daemon` over stdio (with
interactive prompts routed to a native dialog via `SSH_ASKPASS`, so auth works from a
windowed launch with no tty), and `:connect bemtvi://…` dials the QUIC listener. The
editor stays local; `:connect` swaps onto a *fresh local server* whose seams point at
the daemon and re-attaches the same window (`bemtvi_gui::run`'s session loop), since the
editor transport is always the in-process duplex. See
[the edit-host plan](plans/2026-06-09-edit-host-and-browser-lua.md). It forces a
**split-brain filesystem rule** for the Lua bridge, decided up front: the
*project-facing* fs API — the async `btv.fs.*` (there are no synchronous Lua fs
builtins) — routes
through the `LuaFs` seam — local disk by default, the remote daemon in a split — so
file-picker previewers, root detection, and VCS-status providers see the *project*; while raw Lua
`io.*`/`os.*`, `require`/`package.path`, the runtimepath (`nvim_get_runtime_file`), and
`stdpath` stay **local** by default — plugins and their caches live on the local machine
(the divergence from VS Code's remote-extension-host model). A session opts into the
daemon's config + plugins (and shada) with `--remote-config` — then the runtimepath is
fetched over the wire and materialized into a local per-process cache; the browser, which
has no local disk, is always remote-config. See [the edit-host split](edit-host-split.md).

Because the editor is local, a **dropped connection must not tear the session down** — the
buffers, undo, cursor, windows, and Lua state all live on the local side and would be lost
with it. So an ssh/stdio daemon link is **reconnectable**: a supervisor on a dedicated link
thread (`bemtvi_server::connect_daemon_reconnecting`) keeps the seam handles the editor holds
fixed and swaps the *connection beneath them* (a `LinkRpc` cell per leg group). On EOF (a
laptop sleeping past QUIC's idle timeout, an ssh hop dropping) it auto-retries with bounded
backoff (`ReconnectPolicy`: 5 attempts over 0.5 → 8 s), re-spawning `ssh … bemtvi --daemon`;
on success the seams rebind and editing continues. If the budget is spent it parks and tells
the user to run `:reconnect` (`:disconnect` drops it on demand). The link's `DaemonStatus`
(`Connected` / `Reconnecting{attempt,max}` / `Disconnected`) rides a `watch` channel into the
run loop, which mirrors it to `btv.daemon.status()` and fires a `User DaemonStatusChanged`
autocmd so a statusline component can render it (green / yellow / red). A re-dialed daemon is
a **fresh process** that knows nothing of the prior session, so on a genuine reconnect the
editor **re-syncs the seams** off the tick: it re-opens LSP servers against the new connection
(from each server's cached spawn), re-arms every file watch *carrying the buffer's disk
baseline* so the daemon detects a file changed during the outage and pushes it (the unmodified
buffer autoreloads; a locally-edited one is a conflict and is left alone), and surfaces that
remote terminals/jobs were lost (their PTYs died with the link). Daemon-side session survival
(persisting terminals/jobs across a drop) is explicitly out of scope. See
[the daemon-reconnect plan](plans/2026-06-29-daemon-reconnect.md). *(QUIC and the web/wasm
edit-host stay one-shot for now — the ssh path covers the reported sleep/wake case; their
reconnect is a later phase.)*

### Async design

Both sides run on single-threaded tokio runtimes (the editor core, like
neovim's, is single-threaded; concurrency comes from async I/O, not parallel
mutation):

- `bemtvi-rpc::connect` spawns independent reader and writer tasks, so encoding,
  decoding, and socket back-pressure never block the consumer.
- The **client** multiplexes terminal input and incoming redraws with
  `tokio::select!`. Keystrokes are sent the instant they arrive; redraws are
  painted as they come.
- The **server** processes one RPC message at a time against the (non-`Send`)
  editor and Lua state, while the RPC tasks keep the wire moving underneath it.

The editor and Lua state are intentionally `!Send` and live on a single thread,
which is why the server gets its own thread/runtime rather than being spawned
onto a shared pool.

#### Multi-source scheduling & event ordering

The server's `tokio::select!` loop (`bemtvi-server::run`) multiplexes **ten**
event sources against the single-threaded editor: RPC input from the UI, the LSP
manager, the async-runtime actor (`evloop.rs` — timers and child processes),
terminal child output, off-tick file opens, write-acks, and `:cd` completions
from the daemon fs seam, `:TSInstall` grammar-build completions, daemon
file-watch events, and daemon link-status changes. The
first three are the always-on primary sources; the rest service the terminal,
edit-host/daemon, and treesitter-install features. Treesitter highlighting is
*not* among them — the engine runs in-process and is queried synchronously
during `redraw`, so it needs no channel or arm. Each source is an mpsc
channel (the link status a `watch`); the
matching async actor (a `Send` background task) only ever ferries ids / bytes /
durations back, never the `!Send` editor or Lua state. This is bemtvi's analog of
neovim's main-thread + worker-thread model, where workers hand results to the one
editor thread by enqueuing events.

Two ordering properties hold, and one is a deliberate divergence worth recording:

- **Serialization (preserved).** Every `select!` arm body runs to completion
  before the next loop iteration — the off-tick arms are fully synchronous, and
  each ends in the *settle contract* (`apply_lua_effects` → `run_pending` →
  `redraw`) so a callback's deferred work converges and repaints at a
  redraw-followed point, never "too early". Two events can never interleave their
  mutations: neovim's "editor logic never runs concurrently with itself"
  guarantee, enforced here by the crate boundary (`bemtvi-core` is pure/sync) and
  the `!Send` VM rather than by neovim's runtime `recursive`-abort.
- **Per-source order (FIFO).** Each channel delivers in arrival order, and the
  per-arm coalescing (`while try_recv()`) drains a burst in order before one
  repaint.
- **Cross-source order (divergence).** When events from *different* sources are
  ready in the same poll, plain `tokio::select!` picks a ready branch
  **pseudo-randomly**. Neovim's parent/child `MultiQueue` instead imposes a
  deterministic relative order by enqueue time. We accept the weaker guarantee:
  the random pick buys anti-starvation fairness, and because every arm fully
  settles, the nondeterminism is limited to *which independent source lands
  first* — never to interleaving or corruption. A timer-vs-keystroke wall-clock
  race is inherently timing-dependent in neovim too.

**The `biased;` option (possible future change).** `tokio::select!` accepts a
leading `biased;` that switches branch selection from random to **top-to-bottom
in declaration order**. Adding it would make cross-source scheduling
*deterministic* — the closest analog to neovim's multiqueue ordering — and turn
the arm declaration order into an explicit **priority** (e.g. input first, so
keystrokes are never delayed behind background timers/LSP). It is intentionally
**not** enabled today because:

- the current random selection is the simpler default and provides fairness for
  free, and no observed workload depends on cross-source ordering (each arm
  settles independently, so the relative order of background sources carries no
  correctness dependency);
- `biased;` makes the developer responsible for starvation — a branch that is
  *perpetually* ready (e.g. a sustained input flood, or a tight self-re-arming
  timer) would starve every lower-priority branch, whereas random selection
  cannot. Our per-arm burst-coalescing bounds this in practice, but it is a real
  footgun the default avoids.

Adopt `biased;` if a future need arises: a reproducibility requirement (a test or
behavior that must see input drained before a same-tick timer), or a responsiveness
bug where background work visibly preempts input. The change is one line plus a
deliberate arm ordering — recommended order **input → LSP → loop
(timers/processes)** (user-facing first) — and must be paired with a starvation
review of the now-highest-priority arm.

---

## Protocols

### RPC framing (`bemtvi-rpc`)

A standard msgpack-RPC framing — chosen because it's a good async binary
protocol, **not** for neovim interop. Messages are msgpack arrays:

- Request: `[0, msgid, method, params]`
- Response: `[1, msgid, error, result]`
- Notification: `[2, method, params]`

The client-protocol verbs — the ones a UI actually speaks (input, attach,
resize) — are `btv_*`: `btv_input`, `btv_input_mouse`, `btv_ui_attach`,
`btv_ui_try_resize`, `btv_command`. A **paste** rides `btv_input` too, as one
batch wrapped in `<PasteStart>` … `<PasteEnd>` (what
[`bemtvi_view::encode_paste`](../crates/bemtvi-view/src/keys.rs) emits). The
brackets mark the payload as text the user already had rather than keys they
typed, and the server *collects* what they enclose instead of dispatching it: in
ordinary Insert mode the whole payload goes into core as a **single** edit
(`Editor::paste_literal`), so it is one rope insert and one settle, and — never
having become keys — it cannot trip a mapping, a snippet jump or a completion
confirm on the way in. Modes whose behavior is inherently per-character (Normal,
the command line, a terminal job, a grabbing menu, Replace's overtype) replay the
payload key by key *inside* the span, where the insert-mode helpers — auto-indent,
soft tabs, auto-pairs — still stand down; only the speed is lost. Either way the
span is recorded around the payload, so a `.` of a change containing a paste
re-enters paste mode rather than re-indenting what it replays. Because it is only
notation, every transport (local, daemon, browser) gets all of this for free. The
editing-API methods keep the familiar
neovim spelling (`nvim_buf_get_lines`, `nvim_open_win`, `nvim_win_set_cursor`,
…) as muscle-memory names, but they are bemtvi's own methods with bemtvi's own
semantics (a read-heavy subset is also aliased into Lua as `vim.api.*`; the
window/tab *mutation* verbs are RPC-only).

### View protocol (UI)

The core projects editor state into a [`View`](../crates/bemtvi-core/src/view.rs):
a **list of windows** plus the global chrome. Each `WindowView` carries one
window's `rect`, focus flag, visible text rows, cursor, selection/search spans,
gutter numbers, and status-line data (file name, modified flag, ruler); the
`View` adds the inter-split `separators` and the **global** fields one editor has
(mode label, command line, message, and the list-overlay `menu`). The server
sends it as a single
`redraw` notification carrying one msgpack map (a `windows` array + a
`separators` array + the global keys). With one window the list has a single
entry spanning the whole text area, so the frame is identical to the pre-windows
view. (See [*Windows*](#windows).)

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
- a `chrome` map of editor-region → `style_id` for the text background
  (`Normal`), the number gutter (`LineNr`, `CursorLineNr`), the cursor line
  (`CursorLine`), the visual selection (`Visual`), search (`Search`,
  `IncSearch`), the status line (`StatusLine`), errors (`ErrorMsg`), the
  end-of-buffer filler (`EndOfBuffer`), and float chrome (`FloatBorder`,
  `NormalFloat`, `FloatTitle`).

The server still owns *which* cells are in a group (byte offsets resolved to
screen columns via the same tab/wide-char `virtcol` the selection uses); it now
*also* resolves group → style. The client is a dumb truecolor renderer: it paints
the `Normal` background across the text area, themes the gutter/selection/status
from `chrome`, and colors each span from its `style_id`. When a span carries no
resolved style (no colorscheme loaded), the client falls back to a small built-in
theme, so default startup looks exactly as before. (See
[*Syntax highlighting*](#syntax-highlighting-treesitter).)

The same split governs the **number column**: each `WindowView` carries the
per-row 1-based buffer line numbers (`numbers`, `None` for `~` filler rows), the
`number`/`relativenumber` option flags, and the gutter width (`number_width`,
sized like vim's `numberwidth`). `number`/`relativenumber` are **window-local** —
a `WindowOptions` lives on each window, set via `:set`/`:setlocal`/`vim.wo`, and a
split inherits them from the window it splits off — so two windows onto the *same*
buffer can show different gutters. The core owns *what* each line's number is; the
client renders the gutter as its own ratatui widget — a horizontal split off the
left of the text area — and decides *how* it looks, computing the relative
offsets and the hybrid absolute-on-cursor-line formatting from that data. Text,
selection, and cursor columns are all measured from the text sub-area, so they
stay gutter-agnostic.

The **client owns chrome layout**, but the *window* rects come from the core.
The client paints each `WindowView` at its `rect` — splitting off that window's
gutter, drawing its text/selection/search, and a status line on its bottom row —
then draws the `separators` between splits and reserves the bottom row for the
global command/message line (the panel, being an ordinary bottom-split window,
is just another `WindowView`). The terminal cursor is drawn only in the
`focused` window. Because the core lays out the windows (vertical splits divide
width), the client reports **both** dimensions of the windows area on
`btv_ui_attach`/`btv_ui_try_resize`. There is still no grid, no cell encoding,
and no `ext_linegrid`.

**Folds collapse in the projection, not the buffer.** A *fold* is a per-window
property — vim's model, so the same buffer folds independently in two windows — but
the fold *structure* (a nested set of inclusive line ranges) derives from buffer
content. The core keeps the two separate: a per-window `FoldState` (which folds
exist and which are closed) plus, for the computed sources, a structure cache keyed
on `(changedtick, method, …)`. A closed fold simply drops its hidden lines from the
per-row vectors (`lines`/`numbers`/`highlights`/…) and emits **one** placeholder row
with the fold's text, so the client renders it as a single line with no protocol
change — the collapsed-fold shape the parallel-array projection already had. Every
caller that walks visible lines (motion, scroll, the cursor, redraw, and the
linewise operators that act on a whole closed fold) goes through the same
`fold_line_start`/`fold_line_end` helpers, so none hand-rolls fold skipping. The
fold **structure** comes from one of five sources behind a shared spine
(`fold.rs`): `manual` (`zf`/`:fold`), `foldmethod=indent`, `foldmethod=marker`
(folds bounded by the literal `'foldmarker'` strings, default `{{{`/`}}}`),
`foldmethod=expr` (the native tree-sitter foldexpr bemtvi recognizes as a marker and
computes from `folds.scm`, or a generic Lua `'foldexpr'` the server evaluates per
line with `v:lnum` and pushes in), and LSP `textDocument/foldingRange` (selected by
the `btv.lsp.foldexpr` marker, requested per buffer and pushed in). The generic-expr
and LSP are *external* sources: bemtvi-core can't run Lua or talk to a language server, so the
server computes them out-of-band and pushes the result into a per-buffer store the
core builds the same nested structure from — and manual folds (with their closed
state) round-trip through shada, restored into the window that reopens the file.
The `foldcolumn` gutter is a client-rendered column (like the number gutter)
carrying the `+`/`-`/`│` markers. See `docs/plans/2026-06-25-tree-sitter-folds.md`.

**Floating windows are a second, on-top layer.** Each `WindowView` carries
`floating`, `border` (`none`/`single`/`rounded`/`double`/`solid`), and `title`.
The list is ordered tiled-windows-first, then the floats bottom-to-top by
`(zindex, id)` — the same order `nvim_list_wins` reports — so the client renders
in two passes: it tiles the non-floating windows and their separators, then
overlays the floats in list order. Each float is made opaque (`Clear`s the cells
it covers), draws its border + title, and paints its own gutter/text/status one
cell inside the border; a focused float owns the terminal cursor. The completion
pmenu stays the highest layer, above the floats. The core sizes a bordered
float's content `lines` to the inset (`rect` minus one cell each side) so the
projection and the painted box agree; the float's outer `rect` is what the client
draws the border around. (See [*Windows*](#windows).)

---

## Text model

Buffers are backed by a [ropey](https://docs.rs/ropey) 2.0 rope (`bemtvi-core`'s
`Buffer`). Indices are **byte offsets** — ropey 2.0's native metric, and the
same column model vim uses — with lines tracked via ropey's `LineType::LF_CR`
(so both Unix `\n` and DOS `\r\n` files split correctly). Editing operations
snap byte ranges to char boundaries (`floor`/`ceil_char_boundary`) so a
multi-byte character can never be split; for ASCII this is all a no-op. The key
invariant: **the rope always ends with a trailing `\n`**, so an empty buffer is
`"\n"` (one empty line) and the editable line count is `rope.len_lines() - 1`.
The phantom final line is never displayed or edited.

That phantom `\n` is bemtvi's spelling of the implicit newline vim's line model puts
after *every* line, including the last — the same model, not a different one. It
follows that the rope cannot say whether the file on disk really ended with one, so
that fact lives in two buffer-local options instead: `'endofline'` (set from the
bytes on read) and `'fixendofline'` (on by default, as in vim — a write supplies a
missing terminator unless you opt out). `Buffer::document_text()` derives **the bytes
the buffer represents** from the rope plus `'endofline'`, and it is the single
definition every consumer uses: `to_save_bytes` writes it (so a `'nofixendofline'`
file round-trips byte for byte, and a 0-byte file stays 0 bytes), and the LSP layer
sends it as the document text and reads server edit ranges against it. See
`docs/plans/2026-07-26-endofline.md`.

The rope is **always UTF-8** (mirroring neovim, whose internal encoding is UTF-8);
the on-disk byte form is a separate concern named by the buffer's `'fileencoding'`.
All charset conversion lives at the byte↔rope seam (`bemtvi-core`'s `encoding`
module): on read, `decode_to_rope` tries `'fileencodings'` in order — BOM sniff,
strict UTF-8, then a `latin1` (windows-1252) fallback that, being a *total,
bijective* single-byte codec, opens any byte stream (latin1, utf-16, even
invalid-UTF-8) and round-trips it exactly; on write, `encode_from_str` converts the
rope back to `'fileencoding'` (re-emitting the BOM when `'bomb'` is set) and **fails
loud** (`E513`) on a character the target can't represent rather than corrupting the
file. Every read path (local `Buffer::from_file`, the daemon replica, …) funnels
through the one decoder and every write through the one encoder, so a file behaves
identically however it's reached.

Motion steps by **grapheme cluster** and the cursor's display column is computed
as a **virtual column** (wide characters via `unicode-width`, tabs expanded to
the buffer's `tabstop`), carried in the `View` as `cursor_screen_col`. `cursor.col`
remains a byte offset (what `nvim_win_get_cursor` returns); the TUI expands tabs
when painting so glyphs line up with that virtual column.

Undo is a **branching undo tree** of full-rope snapshots (cheap thanks to
ropey's structural sharing): undoing then making a new edit forks a branch
rather than discarding the old future, so every past state stays reachable.
`u` / `<C-r>` walk parent / newest-child, `:undo {N}` jumps to any seq across
branches, and `vim.fn.undotree()` projects the tree in neovim's dict shape —
closer to neovim's `undo.c` than the original two-stack model.

---

## Buffers

The editor holds **multiple open buffers** and switches the one window between
them. `bemtvi-core`'s `Editor` separates the two concerns vim keeps apart:

- **Buffer state** (the "file"): the rope text, path, `modified`,
  `changedtick`, the edit journal, **and** per-buffer undo/redo history. These
  live in an `OpenBuffer` (the text `Buffer` plus its branching undo tree and the
  cursor/scroll position saved while the buffer is not current), stored in a
  `BufferStore` keyed by a monotonic, 1-based `BufferId` that is never reused.
- **Window state** (the "view"): the live cursor, scroll `top`, mode, and
  pending-input state stay on `Editor`, alongside `current` (the shown buffer)
  and `alternate` (vim's `#`). The register and the search options are still
  **global**, but options come in three scopes, mirroring vim:
  - The indentation options (`tabstop` / `shiftwidth` / `softtabstop` /
    `expandtab`) are **buffer-local** — a `BufferOptions` lives on each `Buffer`,
    set via `:set`/`:setlocal`/`vim.bo`, so two buffers can indent differently.
    bemtvi's defaults differ from vim's: `tabstop` is 4, and
    `shiftwidth`/`softtabstop` follow it via their `0`/`-1` sentinels
    (`softtabstop → shiftwidth → tabstop`), so one knob sets the indent width.
  - The number-gutter options (`number` / `relativenumber`) are **window-local** —
    a `WindowOptions` lives on each window, set via `:set`/`:setlocal`/`vim.wo`
    (and `nvim_win_{get,set}_option` / scoped `nvim_{get,set}_option_value`), and
    a split inherits them from the window it splits off, so two windows onto the
    same buffer can show different gutters.

  Both non-global scopes are **global-local**, as in vim: alongside each
  instance's local value there is a *global* one (`Editor::buf_opts_global` /
  `win_opts_global`) that `:setglobal` / `vim.go` / `vim.opt_global` read and
  write, `:set` / `vim.opt` write **together with** the local, and `:setlocal` /
  `vim.bo` / `vim.wo` leave alone. For buffers the global value is a *seed* —
  `Editor::add_buffer` (the crate's sole buffer-creation funnel) is what makes a
  config's `:set tabstop=3` reach files opened later rather than only the buffer
  that was current while `init.lua` ran. For windows it is not a seed (a split
  copies the window it came from); it is what seeds a window minted with **no**
  source to copy — a dock, the quickfix tab. A handful of buffer options carry no
  global value at all — the four the *read* decides (`fileencoding`, `bomb`,
  `fileformat`, `endofline`), the `modifiable` marker, and the two nouns derived
  per buffer (`filetype`, `ts_highlight`) — and `:setglobal` on one fails loud
  (`E5100`) rather than storing a value nothing reads. `options::has_global_tier`
  is the single place that question is answered: the ex path rejects by it, and
  the Lua surfaces derive their routing tables from it through the injected
  option catalog. See `docs/plans/2026-08-01-global-local-options.md`.

`Editor::buffer()` / `buffer_mut()` resolve the current buffer through the
store, so the editing code is oblivious to how many buffers are open. There is
always at least one buffer; deleting the last leaves a fresh `[No Name]`.

The surface is the usual vim set: `:e` (open-or-switch, reusing the throwaway
`[No Name]`), `:enew`, `:ls`/`:buffers`, `:b{N|name|#}`, `:bnext`/`:bprev`/
`:bfirst`/`:blast`, `:bdelete`/`:bwipeout`, `<C-^>`, and multi-buffer
`:wall`/`:qall`. The RPC layer mirrors neovim's `nvim_list_bufs`,
`nvim_get_current_buf`, `nvim_set_current_buf`, `nvim_create_buf`,
`nvim_buf_get_name`, and a buffer-addressed `nvim_buf_get_lines`.

`:q` is **window-aware** (see [*Windows*](#windows)): with more than one window
open it closes the current window; only on the *last* window is it a real editor
quit, which — like `:qa` — refuses when a modified buffer would be lost,
switching the window to that buffer and reporting `E37` (so you see what's
blocking), matching neovim's last-window behavior with `hidden` buffers. `:q!` /
`:qa!` exit unconditionally.

The treesitter engine tracks each buffer independently: it keeps a parse tree +
shadow text per `BufferId` (the editor owns the engine), the server memoizes the
projected spans per `(BufferId, changedtick, viewport)`, and a `:bdelete` forgets
both — so switching back to a buffer paints from its live parse instead of
re-opening. (See [*Syntax highlighting*](#syntax-highlighting-treesitter).)

---

## Windows

A **window** is a viewport onto a buffer; splitting creates more of them, tiled
by a layout tree. bemtvi mirrors the buffer split: just as buffer state was
factored out of `Editor`, **window state** is now multiplied. `Editor` holds a
`WindowTree` — a `BTreeMap<WindowId, Window>` keyed by a monotonic, never-reused
`WindowId`, plus a `Node` tree (`Leaf(WindowId)` | `Split { dir, children,
sizes }`) arranging them and a `current` (focused) id. Each `Window` binds a
`BufferId` and, *while not focused*, stashes its `saved_cursor`/`saved_top`; the
focused window's live cursor/scroll stay on `Editor` (so the whole motion/
operator state machine is untouched). The current buffer is **derived** from the
current window — `:b`/`:e` rebind the focused window's buffer.

- **The core owns layout.** `WindowTree::layout(total)` divides the area: an
  `HSplit` stacks children (dividing height, a `─` separator row between each), a
  `VSplit` places them side by side (dividing width, a `│` column between).
  `sizes` are normalized to cells on every layout, so resizing is plain cell
  arithmetic and a terminal resize rescales proportionally. Each leaf's text
  height is `rect.height - 1` (its own status line).
- **Surface.** Splits: `:split`/`:vsplit`/`:new`/`:vnew` and `<C-w>s`/`<C-w>v`.
  Focus: `<C-w>h/j/k/l` (spatial), `<C-w>w`/`<C-w>W` (cyclic). Move:
  `<C-w>H/J/K/L` swaps the focused window's buffer (and its view) with the spatial
  neighbor on that edge, focus following the buffer (`swap_window_dir`). Close: `<C-w>c`/
  `:close`, `<C-w>o`/`:only`, `:hide`, `<C-w>q`/`:q`. Sizing: `<C-w>=`,
  `<C-w>+`/`-`/`<`/`>` (with counts), `<C-w>_`/`<C-w>|`, `:resize`/`:vertical
  resize`. `focus_window` is the window analogue of the buffer switch: it stashes
  the outgoing view, restores the incoming one (cursor re-clamped), and clears
  transient state.
- **RPC / Lua.** `nvim_list_wins`, `nvim_get_current_win`/`nvim_set_current_win`,
  `nvim_win_get_buf`/`set_buf`, `nvim_win_get_cursor`/`set_cursor` (window-handled,
  `0` = current), `nvim_win_get_width`/`height` + setters, `nvim_win_close`,
  `nvim_win_get_config`/`nvim_win_get_position`, and `nvim_open_win` (both the
  split form and the float form). Window *reads* (plus `nvim_set_current_win`)
  are aliased into Lua and resolve against the `btv._wins` mirror the server
  pushes before each Lua entry; the window *mutation* verbs are **RPC-only** —
  internally they queue a `WindowOp` drained into the core (the same queue the
  private `btv._open_win`/`btv._win_*` bridges feed for `btv.view` and the UI
  primitives), but no public Lua `nvim_open_win`/`nvim_win_set_*` binding
  exists.
- **Floating windows.** A float is a `Window` the layout tree does **not** own: it
  lives in `WindowTree.floats` (ids kept sorted by `(zindex, id)`), carries a
  `FloatConfig` (`relative` editor/win/cursor, `anchor`, `row`/`col`, `width`/
  `height`, `zindex`, `focusable`, `border`, `title`), and is positioned
  absolutely by a second `layout()` pass after the tiled rects are known — so it
  steals no space from its siblings and paints on top. `nvim_open_win` with a
  non-empty `relative` opens one (an RPC verb, via `WindowOp::OpenFloat`; from
  Lua, floats come from `btv.view:mount{ float = … }` / `btv.ui.float`);
  the client draws it as an opaque, bordered, titled overlay above the tiled
  layout (see [*View protocol*](#view-protocol-ui)). Focus, the window list, and
  close already span floats because they key off `WindowId`.
  `nvim_win_set_config`/`get_config` move, resize, restyle, and convert a window
  between float and split. Unsupported config values (`relative="mouse"`, an
  unknown `border`) fail **loud** rather than silently falling back to a tiled
  split.
  **Edge semantics** (matching neovim): `:q` on a focused float closes only the
  float and never quits — the "last window" quit rule counts **tiled** windows
  only, so closing the last tiled window quits even with floats open, and a tiled
  window can't be closed down to floats-only. `:only`/`<C-w>o` close every float
  too. `<C-w>w`/`<C-w>W` cyclic focus includes **focusable** floats (in z-order,
  after the tiled windows) and skips non-focusable ones, though
  `nvim_set_current_win` can focus either explicitly; the spatial `<C-w>h/j/k/l`
  moves stay within the tiled grid. Closing a window also closes every float
  anchored to it (`relative="win"`, transitively). A terminal resize re-runs the
  float pass, re-clamping `editor`-relative floats back on-screen. The lifecycle
  diff fires `WinNew`/`WinEnter`/`WinClosed` for floats and `WinResized` when
  `set_config` changes a float's size.
- **Permanent docks.** A **dock** is a global (cross-tab) editable window region
  pinned to a screen edge — bemtvi's VSCode-style side bars and bottom tray. It
  reuses the tab-swap trick along a second axis: `Layer::{Main, Dock(Left | Right |
  Top | Bottom)}`, with the *focused* layer's tree always live on `Editor::windows`
  (the rest park per *Tab pages* above), so `split`/`close`/`focus`/editing/redraw
  act on whatever layer holds focus with zero retargeting. `<C-w><C-w>` is the
  **layer-switch** prefix: `<C-w><C-w>{h,j,k,l}` crosses focus *spatially* between
  the regions — full-width top/bottom bands around a `left|main|right` middle band —
  stepping to the open region in that direction and wrapping past the far edge
  (`cross_dir_candidates`/`cross_dir_target`; a no-op when nothing in that direction
  is open), `<C-w><C-w>{H,J,K,L}` *moves* the focused buffer to the layer on that edge (the
  dock on that side — a no-op if it is closed — or back to main from a dock; the
  source window falls back to a sibling/empty buffer of its own layer via
  `move_buffer_to_layer`), and `<C-w><C-w>{cmd}` crosses then runs the command,
  while a single `<C-w>` stays in the focused layer. The layer-switch chord is **mode-independent** — a small
  state machine (`Editor::dock_chord_intercept`, run ahead of the per-mode
  dispatch in `input`) reaches the docks from insert, visual, command and
  terminal mode too, leaving the source mode cleanly before it crosses; only
  Normal/MultiCursor keep the grammar's own `<C-w>` path (which also owns the
  single-`<C-w>` window commands). A lone `<C-w>` in those modes is held one key
  and replayed on a non-chord follow-up, so its original meaning is preserved.
  The round trip is **mode-transparent**: every cross parks the source window's
  mode (`Window::resume`) and the target window resumes its own parked mode
  (`execute_window_layer` → `enter_window` → `reestablish_mode`), so popping over
  to a dock and back lands you in the same insert/visual/terminal state you left,
  the cursor where it was — while ordinary window/tab/mouse focus changes still
  land in Normal (the resume is gated on a one-shot the chord sets). `relayout` carves the four edge bands (each reserving a
  separator cell toward the main area, clamped so the main rect keeps ≥1 col/row)
  and lays every layer's tree out at origin `(0,0)` in its own region; `View`
  carries the band sizes and tags each window with its `WindowRegion`, and each
  client maps region → absolute origin (the core owns *which* cells, the client
  owns *where*). Surface: the `btv.dock.*` Lua table
  (`open{side,size?,buf?,title?,showtabline?,autohide?}` / `close` / `focus`, plus the
  per-dock option scope `btv.dock.opt(side)`) and the `:DockOpen`/`:DockClose`/
  `:DockFocus` ex-commands, queued as a `DockOp` drained into the core. Mouse:
  `hit_test` resolves a click across **every** region (the focused layer plus each
  parked dock tree, via `region_geoms`), so a left-click in any dock focuses it and
  places the cursor — `set_current_window` crosses to that layer first. A dock can
  also be **hidden** (toggle / auto-hide): `dock_hidden[side]` collapses it from view
  while keeping its whole `TabStack` parked, so its splits/tabs/cursor/text all return
  when shown again — distinct from *closed* (which drops the trees). `dock_is_open` is
  the visibility predicate (= present **and** not hidden) that every layout / render /
  mouse / enumeration site reads, while the tree-resolution helpers read `dock_tabs`
  directly so a hidden dock's content stays addressable. `btv.dock.toggle`/`hide`/`show`
  (and `:DockToggle`/`:DockHide`/`:DockShow`) drive it; the per-dock `autohide` option
  hides a dock the moment focus leaves it (a hook in `switch_layer`, the one chokepoint
  for every focus cross). A hidden dock isn't invisible: `View.hidden_docks` carries a
  label per collapsed dock, which each client paints as a clickable `▸{label}` chip on
  the idle command-line row (`hidden_chip_at` maps a click back to `show_dock`).
  The **buffer list is per-layer**: each buffer carries the window layer it was last
  shown in (`OpenBuffer::layer`, set by `set_cur_buffer`/`set_window_buffer`), so `:ls`
  and `:bnext`/`:bprev` list only the focused region's buffers, closing a document
  falls back to a sibling in the *same* layer (never pulling a dock's buffer into the
  main area), and `btv.buf.list{focused=true}` exposes the focused-layer list to Lua
  (`nvim_list_bufs` stays global). (Design:
  [`docs/plans/2026-06-14-permanent-docked-panels.md`](plans/2026-06-14-permanent-docked-panels.md),
  [`docs/plans/2026-06-14-dock-toggle-autohide.md`](plans/2026-06-14-dock-toggle-autohide.md).)
- **Autocmds.** `WinNew`/`WinEnter`/`WinLeave`/`WinClosed`/`WinResized` fire from
  the same server-side lifecycle diff as the buffer events, ordered
  `WinLeave → BufLeave/BufEnter → WinEnter` around a focus change.
- **Shared per buffer.** Two windows onto one buffer share its `SyntaxState`,
  diagnostics, and undo — each just projects a different `(top, height)` slice.
  The register, command line, and message line stay **global**; the
  number-gutter options (`number` / `relativenumber`) are **window-local** (a
  `WindowOptions` per window, set via `:set`/`:setlocal`/`vim.wo`).

**Horizontal scrolling** rides `WindowOptions` too: each window tracks a
`leftcol` (the first visible screen column, the horizontal analog of the vertical
`top`) and, under `nowrap`, scrolls sideways to keep the cursor visible — governed
by the window-local `sidescroll` / `sidescrolloff`. The core decides `leftcol`
(`ensure_visible_horizontal`, called on the same beat as the vertical
`ensure_visible`); the client paints from that offset, leaving the number gutter
fixed. (Design: [`docs/plans/2026-06-07-horizontal-scrolling-and-wrap.md`](plans/2026-06-07-horizontal-scrolling-and-wrap.md).)

**Tab pages** multiply the window layout the same way `BufferStore` multiplied
the buffer — and they do it **per region**: the main area and *each* open dock
(see *Permanent docks* below) carry their own independent tab stack. Each layer
owns a `TabStack { tabs: Vec<TabSlot>, current }`, so `Editor` holds `main_tabs`
plus `dock_tabs: [Option<TabStack>; 4]`. The *focused* layer's active tab is the
live `WindowTree` on `Editor::windows`; every other `(layer, tab)` tree parks in
its `TabSlot`, so across the whole editor exactly **one** slot is `None` (the live
one) — the same invariant tabs always kept, now spanning two axes. A switch
(`gt`/`gT`/`:tabnext`/`nvim_set_current_tabpage`) is a `mem::swap` of the live
tree with the target's stash, then re-enters the incoming tab's focused window —
the tab analogue of `focus_window`, so the entire editing machine is untouched.
`:tabnew`/`:tabedit`/`<C-w>T` create one (window ids minted globally off
`Editor::next_win_id` so they never collide across any layer/tab); `:tabclose` /
`:tabonly` refuse main's final tab, `:q` on a tab's last window closes the *tab*,
and closing a **dock's** last tab closes the dock. Tab ops act on the **focused**
region — `gt` in a focused dock cycles only that dock's tabs — while the
`nvim_tabpage_*` API stays main-only (it crosses to main first). Reads resolve
against a `btv._tabs` mirror and `nvim_set_current_tabpage` queues a `TabOp`, the
same "Lua queues, core mutates" flow as windows; the lifecycle diff fires
`TabNew`/`TabLeave`/`TabEnter`/`TabClosed`, bracketing the window events
(`TabLeave → WinLeave → … → WinEnter → TabEnter`).

**Tablines are per region too.** Each region draws its own tabline: the main
area's is the editor's top bar (below a top dock, if any); each dock's is the
first row of its band, with the tree laid out below it. The `View` carries a
`RegionTablines { main, docks[4] }` — each a `Vec<TabView>` (focused buffer name +
modified flag + window count), an active index, and the dock's title — gated by
each region's own `showtabline` (a dock may override the global option, or force
its strip on with a non-empty title), and the server's `relayout` reserves
`tabline_rows_for(layer)` per region. A left-click on any region's tabline cell
switches that region to the clicked tab and focuses it: `Editor::region_tabline_at`
reconstructs the same per-region band geometry the clients paint, then maps the
cell (past a dock's title prefix) to a `(layer, tab)`. (Design:
[`docs/plans/2026-06-07-tab-pages.md`](plans/2026-06-07-tab-pages.md),
[`docs/plans/2026-06-14-per-region-tablines.md`](plans/2026-06-14-per-region-tablines.md).)

Already on `WindowOptions`: the number gutter (`number`/`relativenumber`/
`numberwidth`), the
cursor-line highlight (`cursorline`), soft word-wrap (`wrap`, with its `gj`/`gk`
display motions, `breakindent`/`showbreak`), the horizontal-scroll options, and
the later arrivals — `scrolloff`, `signcolumn`, `colorcolumn`, `foldcolumn`,
`fillchars`, `winhighlight`.
Still pending: the long tail of vim's window-local options beyond those.
Floating windows are otherwise complete (model,
paint, dynamic config, edge semantics); the remaining float fidelity knobs
(`style="minimal"`, `footer`, `bufpos`, `relative="mouse"`) grow as a consumer
demands them. All four `laststatus` modes ship (`0` never, `1` only with ≥2
windows, `2` per-window default, `3` a single global status line).

---

## The panel (transient bottom overlay)

Multi-line, browsable output — `:messages`, `:ls`/`:buffers`, `:registers`,
`:marks`, `:jumps`, `:changes`, and scripted listings — lives in a **panel**: a
transient, focus-locked overlay shown in a bottom split. A panel is **not** a
bespoke widget with its own content model; it is an ordinary `nomodifiable`
buffer in a `botright` split, with two properties layered on top. It is
bemtvi-native, closest in spirit to neovim's quickfix window.

- **It's an ordinary buffer; the core adds only displace + focus-lock.** `Editor`
  holds an `Option<PanelState>` — just the panel's `window`, the `prev_window` to
  refocus on close, and an edge `margin` — and *no* content/cursor/scroll state
  (that all lives in the buffer and its `WindowView`). `open_panel` mounts a
  buffer in a bottom split (reusing `open_bottom_window` / `remove_window`, which
  displace the main window into the rows above) and **hard-locks focus** to it: a
  guard in `Editor::focus_window` refuses to move focus anywhere else, so `<C-w>`
  navigation, `nvim_set_current_win`, and mouse focus are all inert until
  `close_panel` dismisses it. Opening a second listing over an open panel
  re-targets the one window rather than stacking overlays.
- **Everything inside is plain buffer behavior.** Motions navigate, search works,
  and long lines wrap (`:messages` just sets `'wrap'` on the panel window) —
  because the panel *is* a real window onto a real buffer, not because the input
  loop special-cases it. The activation and dismiss keys are **buffer-local
  keymaps installed by a `FileType` autocmd**, never hard-coded: the prelude's
  `FileType btvlisting/btvbuffers/btvpanels/btvmessages/btvpanel` maps bind `q`/`<Esc>` to
  `btv.panel.close`, and a per-listing `<CR>` action (e.g. `btv.buffers.actions.open`
  reads the bufnr off the cursor row and switches) is an ordinary `default` map
  that a user map overrides — rebindable the standard way.
- **Built-in listings mount here.** `:registers`, `:marks`, `:jumps`, `:changes`
  go through `Editor::open_scratch_listing(name, lines, cursor)` (filetype
  `btvlisting`); `:ls`/`:buffers` through `Editor::open_buffer_listing` (filetype
  `btvbuffers`, whose `<CR>` switches buffer); the named-panel list through
  `btvpanels`; `:messages` through `btvmessages`, whose `C` map
  (`btv.messages.actions.clear` → `:messages clear`) empties the log and
  re-renders the panel in place. Each opens scrolled to a chosen cursor line —
  `:messages` to the newest line, `:ls` to the current buffer.
- **A message history feeds it.** `Editor::echo` is the one place a user-facing
  message is set; it records each line in a `messages` history (the backing store
  for `:messages`) as well as showing it on the message line. A message spanning
  several lines is fenced above and below with a `MESSAGE_SEPARATOR` row, so a
  block (a `:=` table dump, a traceback) stays visibly one entry in the flat
  history; adjacent blocks share their divider. The server routes
  its own messages (errors, captured `print`/`nvim_echo`) through the same call.
- **It's scriptable.** A plugin mounts its own panel with
  `btv.panel.open{ name?, lines, filetype?, height?, margin? }` and dismisses it
  with `btv.panel.close()` — queued as a `PanelOp` drained by the server (the same
  "Lua queues, core mutates" flow as `vim.cmd`/`nvim_set_hl`). `name` (default
  `[Panel]`) makes the panel unique, so re-opening replaces its content;
  `filetype` (default `btvpanel`, whose ftplugin maps `q`/`<Esc>` to close) lets a
  plugin pass its own filetype and wire its own keys. The only RPC method is the
  read-only `bemtvi_panel_is_open` (clients use it for chrome); there is no
  separate panel content/cursor RPC because the panel *is* a window — its text,
  cursor, and scroll ride the ordinary `WindowView`, and the redraw carries no
  special `panel` map.
- **Mouse.** A left-click in the panel window focuses its line like any window
  (focus-lock keeps you inside the panel). It is one gesture in the editor's
  broader, server-owned mouse support (click, drag-select, multi-click,
  shift-extend, wheel scroll, divider drag, the `'mousemodel'` right-click menu,
  middle-click paste, and tabline clicks), forwarded by both clients as
  `btv_input_mouse` with the server owning the hit-test.

The same core hit-test also drives the **floating list overlays** — the insert
completion popup, the fuzzy picker, the promptless `btv.ui.select`, and the
command-line wildmenu. Their box geometry lives in one place,
`Editor::menu_geom` (`editor/menu.rs`, shared with the server's `redraw` menu
projection), and `Editor::menu_hit` inverts it: it offsets the box by the focused
window's screen origin (or, for the wildmenu, the command-line frame) and the
client border convention to map a clicked global cell back to the list row painted
there. A click highlights a row, a click on the already-highlighted row
accepts/confirms it (a click off a picker box cancels it), and the wheel moves the
highlight or scrolls a picker preview — so every front end (TUI, GUI, web) gets
overlay mouse for free by forwarding the same raw `btv_input_mouse` cell, with no
client-side geometry. (Command-line-mode mouse needs `c` in `'mouse'`; the default
`nvi` omits it.)

---

## Lua

bemtvi embeds **vendored PUC Lua 5.4** via [mlua] — the single backend. Scripts run
**inside the server**, exactly as in neovim, and influence the editor through the
same mechanisms RPC clients use. The VM loads the full safe stdlib **plus
`debug`** (real plugins call `debug.getinfo` to locate their own install dir, and
neovim exposes it). There is no backend toggle: `lua54` is baked into the shared
`[workspace.dependencies].mlua` (`vendored`), so every crate that links mlua —
and the wasm edit-host — shares one backend. LuaJIT was dropped (it never
compiled to the wasm target); the prelude ships a `bit` library shim since PUC
has no `bit` *table* (5.4's native bitwise operators notwithstanding). 5.4 over
the old 5.1 baseline brings real 64-bit integers, native bitwise operators, the
`utf8` library, **yieldable `pcall`** (so `pcall` can wrap a coroutine-yielding
`btv.await`), and a generational GC; the one stdlib removal that touched the
prelude — `loadstring` (folded into `load` in 5.2) — is restored by a one-line
`loadstring = loadstring or load` shim at VM creation in `runtime.rs`.

**Effects flow through queues.** `vim.cmd(...)` / `vim.api.nvim_command(...)`
queue ex-commands; `print(...)` / `vim.api.nvim_echo(...)` capture output;
`vim.api.nvim_set_hl(...)` queues highlight-group definitions. After each chunk
runs, the server drains those queues into the (pure, synchronous) core — Lua
never mutates the editor directly. The end-state is for `vim.api.nvim_*` to call
the very same API functions remote clients invoke (`Lua → API → core`).

**A config runtime.** bemtvi resolves a config dir and
**runtimepath** the way neovim does (`$BEMTVI_CONFIG` / `$XDG_CONFIG_HOME/bemtvi` /
`~/.config/bemtvi`, plus `pack/*/start/*` discovery and `$BEMTVI_RUNTIMEPATH`
for tests), seeds `package.path` from it so `require` resolves config and
colorscheme modules, and sources `<config>/init.lua` at startup — before the
first frame. The Lua
surface is provided as a bundled **Lua prelude**
(the `bemtvi-lua/src/prelude/` modules, the analogue of neovim's `runtime/lua/vim/`):
`vim.tbl_*`, `vim.split`, `vim.inspect`, `vim.g`/`vim.o`/`vim.opt`/`vim.env`,
`vim.notify`, `vim.log`, user commands, and autocmds; env-touching helpers
(`vim.fn.stdpath`, …) are Rust-backed, and there are no synchronous fs
builtins — filesystem access is the async `btv.fs`. `:colorscheme <name>`
sources `colors/<name>.lua` off the runtimepath and fires the `ColorScheme`
autocmd. A colorscheme is just Lua: its `setup()` compiles a highlight table
(typically cached as Lua bytecode under `stdpath("cache")`), and `load()`
populates the highlight registry via the `btv` highlight API (its `nvim_set_hl`
alias). See the [README](../README.md#configuration) to set one up.

**Plugins persist isolated state.** A plugin opts into cross-session storage with
`btv.shada.plugin()`: a key/value handle (JSON values) that lives in a dedicated
table of the active shada store, walled off from the core registers/marks/history.
The namespace is *assigned, not chosen* — derived from the calling code's
runtimepath/plugin directory (via `debug.getinfo`), so a plugin reaches only its
own slice and can't name another's. It rides the ordinary shada flush cadence; see
the [`btv.shada.plugin` section](specs/2026-06-11-native-plugin-api.md) and
`examples/plugin-shada/`.

The editor's scripting namespace is its own: config files and plugins target
the `btv.*` API ([ADR 0002](decisions/0002-native-plugin-system.md);
[the `btv` design](specs/2026-06-11-native-plugin-api.md)). The lasting `vim.*`
is exactly one thing: a closed whitelist of **muscle-memory aliases** —
variables / options / env (`vim.g`, `vim.o`/`vim.opt`, scoped variants),
`vim.cmd` / `vim.keymap.set`, the declarative registrations (autocmds, user
commands, the `nvim_set_hl` highlight helper colorschemes use),
the pure helpers (`vim.tbl_*`,
`vim.split`, `vim.inspect`, …), and the callback-shaped async (`vim.notify`,
`vim.schedule`/`vim.defer_fn`, `vim.ui.*`) — process spawning is the
promise-based `btv.run`, not a `vim.system` alias —
mapping 1:1 onto the `btv` equivalents so config can be written in familiar
muscle-memory terms (the
canonical list: [ADR 0002](decisions/0002-native-plugin-system.md)). The
prelude's
broader vim-shaped surface is donor code
for the `btv` build-out: what serves bemtvi's objectives is refactored under
`btv.*`, the rest is deleted (see the roadmap).

---

## Syntax highlighting (treesitter)

bemtvi is **treesitter-native only** — there is no regex/`syntax.vim` highlighter.
All highlighting comes from [tree-sitter](https://tree-sitter.github.io) grammars
and their `highlights.scm` queries, parsed **in-process**:

- **In-process, synchronous.** The editor owns the parser (a `Box<dyn
  SyntaxEngine>` whose trait lives in `bemtvi-core`, implemented by `bemtvi-ts`) and
  queries it during `redraw`, so spans are correct in the **same frame** as the
  keypress — no worker process, no RPC, no async catch-up frame. This is neovim's
  posture: a buggy grammar (compiled C) can segfault the editor, a risk accepted
  because grammars are user-installed and stable, bounded by a **parse deadline**
  (a per-parse wall-clock budget; on expiry the last good tree is kept, costing
  one frame of stale highlights rather than a hang). It also drives treesitter
  *indentation* and *injections*, both of which need a synchronously-queryable tree.
- **Installable grammars.** Grammars are not bundled; they load dynamically by
  filetype from a data directory using tree-sitter's standard on-disk layout
  (`<data>/parser/<lang>.so`, `<data>/queries/<lang>/highlights.scm`), so any
  standard tree-sitter grammar + query set is drop-in usable.
- **Incremental parsing.** The engine keeps a shadow buffer and a persistent
  parse tree per buffer; it applies only **edit deltas** (`InputEdit`) drained
  from the `Buffer` edit journal in `bemtvi-core` (`changedtick` + `BufferEdit`s),
  so per-edit cost scales with the edit, not the file — huge files stay
  responsive.

The `View`/`redraw` carries the result as a per-row `highlights` array (see the
*View protocol* above): screen-column spans tagged with a capture-group name and
a resolved `style_id`. The server owns *which* cells are which group **and**
resolves group → concrete style (a colorscheme's `nvim_set_hl` table, or the
capture-fallback chain); the client paints the truecolor it is handed, falling
back to a small built-in theme only when no colorscheme resolved a span. Full
designs:
[in-process treesitter](specs/2026-06-06-in-process-treesitter-and-indentation-design.md)
(superseding the original [worker-based design](specs/2026-06-01-syntax-highlighting-design.md))
and
[the catppuccin colorscheme](specs/2026-06-01-catppuccin-colorscheme-design.md).

### Treesitter control — declarative buffer state, not a parser API

The native engine above *is* bemtvi's treesitter. There is no Lua parser/AST
platform: per [ADR 0002](decisions/0002-native-plugin-system.md) the vendored
neovim `vim.treesitter` Lua (the `LanguageTree` / `get_parser` / `TSNode`
machinery and the Rust primitives that backed it) was **deleted** — it existed
only to host third-party neovim plugins, a non-goal. There is no parser-facing
`btv.treesitter` API: the `btv.treesitter` table holds only the `foldexpr` marker
for `foldmethod=expr` (with `vim.treesitter.foldexpr` as its one muscle-memory
alias — the rest of `vim.treesitter.*` is absent). What
remains is a tiny control surface over the engine, and it is **declarative buffer
state**, not a verb API:

- **Highlight control is the two-noun model**: `btv.bo.filetype` chooses the
  language, `btv.bo.ts_highlight` chooses whether the engine paints it. There is no
  `start`/`stop` verb — setting the filetype and flipping `ts_highlight` *is* how
  you start/stop highlighting. `:set filetype` / `:setf` write the same per-buffer
  override, so there is a no-Lua path too, straight from the ex line.
- **Query customization is the native bridge `btv._btv_set_ts_query(lang, name,
  text|nil)`** — it queues a `TsOp::SetQuery` the server pushes straight onto the
  engine, installing a `highlights` / `injections` / `indents` override (a
  replace, `nil` to drop). There is no `;extends` / `after-queries` / runtimepath
  *merge*: base queries come from the engine's data-dir files, an override
  replaces them.

Injections are engine-native: the engine runs the resolved `injections` query
over the live tree and parses each region with its child grammar, per-edit and
synchronous (see [injections](specs/2026-06-08-treesitter-injections-design.md)).

The boundary this section embodies — **native engine for editor behavior, a
Lua scripting layer on top** — is the same one LSP follows (a native
async server under a Lua control surface), recorded in
[ADR 0001](decisions/0001-native-engines-vendored-lua-apis.md) and carried
forward by [ADR 0002](decisions/0002-native-plugin-system.md) (which retires
the vendored vim-named spelling in favor of `btv.*`). ADR 0001
also names the *bridge pattern* (the treesitter query bridge, LSP semantic tokens)
by which a scripting API is wired to the native engine underneath, projecting
into
the extmark highlight layer rather than into core's synchronous path.

---

## Cross-platform & the GUI

bemtvi targets all major OSes (Linux, macOS, Windows). The dependency choices are
deliberately portable: `crossterm` for the terminal, `ropey`, `tokio`, and
`rmpv` are all cross-platform, and the in-process transport uses no OS-specific
IPC.

The terminal client is built on [ratatui](https://ratatui.rs) (over crossterm).
Because every front end is just a client of bemtvi's own RPC, a **native GUI** —
notably a non-terminal GUI on Windows — is just another client crate consuming
the same `View` protocol, with zero changes to the server or core.

That claim is now load-bearing: **`bemtvi-gui` is a native GUI client**
([`crates/bemtvi-gui`](../crates/bemtvi-gui)) on **winit + wgpu + glyphon**. It is
the GUI sibling of `bemtvi-tui` and reuses the same frontend-neutral
[`bemtvi-view`](../crates/bemtvi-view) decode/input layer (`View`, `Style`, `Key`,
`notation`, `encode_text`, `encode_paste`) — the seam the view crate was extracted
for. The
`bemtvi-gui` *binary* embeds a server on its own thread exactly like the default
`bemtvi` binary, joined by the same in-process duplex RPC; the only difference from
the TUI is the client. winit owns the main thread (its loop is not async), so the RPC runs on a
separate IO thread that decodes each `redraw` into a `View` and forwards it to
the event loop via an `EventLoopProxy`, while input goes the other way on a cloned
`Rpc` handle (`notify` is synchronous, no runtime). Rendering is a monospace
**cell grid**: a tiny solid-quad wgpu pipeline paints the backgrounds, selection,
search, status bars and cursor, with a glyphon text layer (syntax-colored from the
server's resolved `styles`) on top; the cell size is measured once from the font.

**Scope.** It now paints essentially the whole `View` the TUI does — the tiled
windows (text, number/relativenumber gutter with `CursorLineNr`, the diagnostic
sign column), floats with borders + titles, the split separators, the tabline
(built-in or custom), per-window and global (`laststatus=3`) status lines, the
completion pmenu (with its doc preview), the `:messages`/`:ls` panel, the
command/message line, visual + secondary (multi-cursor) selections, search /
incsearch, LSP diagnostic underlines + signs + inline virtual text, **inline LSP
inlay hints** (spliced into the row's shaped text so following glyphs — and the
column-keyed selection / search / diagnostic / cursor overlays — shift right by
the inserted width, the GUI analogue of the TUI's inline splice), the secondary
multi-cursors, and the text style attributes (bold/italic via bold and
italic faces, underline/strikethrough/reverse as quads) — plus **pixel-smooth
scrolling**: the focused window slides the server's scroll-gesture band at a
fractional (sub-pixel) line offset driven by the client clock, paced per frame from
winit's `about_to_wait` (where the TUI animates at whole-row granularity, the GPU
client interpolates `top` without rounding). Input reaches parity too:
vim-notation keys, system-clipboard paste, native open/save dialogs, and **mouse**
— left click / drag-select / release, wheel scroll, right-click
(`'mousemodel'`), middle-click paste, and the floating-overlay gestures (the
completion popup, picker, `select`, wildmenu, and panel), sent as the same
`btv_input_mouse` the TUI uses (the server owns the hit-test, so the GUI forwards a
raw cell and carries no overlay geometry of its own). Wide-char + emoji rendering
landed too (CJK cell-snapping, an emoji mask/scale pass that keeps the grid). Still
deferred: undercurl is drawn as a plain underline.
Because the GUI can't be black-box
tested over RPC the way the TUI's paint is (it needs a GPU), only the pure,
frontend-specific translation layers have Tier-1 tests — the winit→notation input
(`crates/bemtvi-gui/tests/keys.rs`), the pointer math
(`crates/bemtvi-gui/tests/mouse.rs`), and the `:connect` target / `bemtvi://` URI / SSH
askpass parsing (`crates/bemtvi-gui/tests/remote.rs`); the rendered frame, and the live
`:connect` session swap, are validated by running it.

### The web build — a fully client-side WebAssembly editor

**`bemtvi-edithost` runs the editor *in the browser*, with no server**
([`crates/bemtvi-edithost`](../crates/bemtvi-edithost)). Where the native clients move
only the UI off the editor, this moves the whole thing into a browser tab — and not
just the core: `bemtvi-core` **+ the PUC Lua 5.4 VM + the full server tick** (the
reusable synchronous `EditHost`, autocmds, mirrors, redraw projection) compile to
`wasm32-unknown-emscripten` and run client-side. It's the edit-host split
(§*Embedded vs. remote*) taken to its limit: the local half is a browser tab, the
fs/process half is OPFS or a remote daemon over WebTransport.

- **The editor lives in a Web Worker.** `web/worker.mjs` is the single `!Send` thread
  that owns core + Lua. It loads the wasm module (`dist/eh.mjs`), constructs the real
  `EditHost` behind a wasm `HostEffects` (`WasmEffects`, `src/lib.rs`), and runs the
  production tick. The UI thread (`web/index.html`) is the renderer + input layer, and
  the two talk over `postMessage` / a shared ring.
- **Interop is emscripten `ccall`/`cwrap`, not wasm-bindgen.** `src/lib.rs` exposes
  `#[no_mangle] extern "C"` exports — `eh_new` / `eh_input` / `eh_input_mouse` /
  `eh_source_lua` / `eh_exec_lua` / `eh_redraw_json` / `eh_lines` / the fs + shada legs
  / `eh_free*` — and the redraw comes back as a JSON return value. Because it links the
  C-heavy lua54 backend + vim-regex, the final link is `emcc`, not wasm-bindgen.
- **The renderer consumes the same `redraw` the native clients do.** `web/index.html`
  paints the server `redraw` frame as **HTML/CSS** (a per-cell-span DOM renderer —
  windows/gutter/status/tabline/panel/pmenu, selection + cursor-shape classes, smooth
  scroll), the browser analogue of `bemtvi-tui`'s layout, and translates a browser
  `KeyboardEvent` to vim key-notation + mouse gestures to `eh_input_mouse`. It exposes
  a `window.__bemtvi` hook (`feed` / `mouse` / `execLua` / `lines` / `frame` / …) so a
  headless browser can drive it.
- **The run loop parks on `Atomics.wait`, and timers fire without Asyncify.** When the
  page is cross-origin isolated the Worker runs a blocking loop parked on a
  `SharedArrayBuffer` input ring, waking on a keystroke **or** the next timer deadline;
  the same wait that blocks on input fires Worker-side timers (`vim.defer_fn` /
  `btv.timer`) via `eh_set_clock` / `eh_next_deadline` / `eh_tick_timers` — one
  mechanism. Without cross-origin isolation it falls back to a `postMessage`-driven
  loop (input works, timers don't fire).
- **The browser is the filesystem — three legs.** Open/save persist to **OPFS**
  (serverless; shada is one JSON blob in OPFS), to **real local files** via the **File
  System Access API** (`:eo` / `:wo` / bare `:w` on a bound path), or to a real
  `bemtvi --daemon` over **WebTransport** (a JS msgpack-RPC client, `web/rpc.mjs`,
  reached with `?daemon=bemtvi://…`). All three ride the same off-tick `HostEffects` fs
  seam the native edit-host split uses; the Worker fulfills fs requests between ticks.
- **Lua config runs — plugins included.** Unlike the old core-only web build,
  `init.lua` is sourced at startup: options / keymaps / autocmds / user commands /
  highlights apply. There is no on-disk runtimepath, so multi-file plugins ship
  *amalgamated*: `web/amalgamate-plugins.mjs` bundles each plugin tree into
  `package.preload` under a per-plugin synthetic runtimepath root, so `require` and
  shada attribution behave as they do natively. **LSP works over the daemon wire**
  (the sync `SyncLspClient` replaces the async manager on wasm — `web/verify-lsp.mjs`);
  only the *native* treesitter engine is off the wasm build: syntax
  **highlighting** is done JS-side via web-tree-sitter
  (`web/highlight.js` + the generated `web/vendor/` grammars), and `:TSInstall`
  fetches a prebuilt `.wasm` grammar.
- **Excluded from the workspace.** It targets `wasm32-unknown-emscripten` and links C
  via `emcc`, so it is in the root Cargo.toml's `[workspace] exclude` (the host
  `cargo build/test/clippy --workspace` never touches it) and pins its own
  dependencies. Built via `crates/bemtvi-edithost/build.sh` (cargo → `emcc` link →
  `dist/eh.{mjs,wasm}`, plus the tree-sitter highlighter assets generated once in the
  crate's `treesitter/` tooling dir and copied into `web/vendor/`). Deployed as static
  files (see `netlify.toml`); the one hard requirement is cross-origin isolation
  (COOP/COEP) for the `SharedArrayBuffer`.

---

## Testing philosophy

bemtvi **does not use unit tests.** We test *functionality* — what the editor
does for a user — not internal code structure. Coverage is layered cheap →
faithful, so the broad, fast tiers localize most failures and the slow PTY tier
stays thin:

- **RPC / `View` integration** ([`crates/bemtvi-server/tests/editing.rs`](../crates/bemtvi-server/tests/editing.rs))
  start a real server, connect over real RPC, send vim key-notation via
  `btv_input`, and assert on observable results: buffer contents
  (`nvim_buf_get_lines`), cursor, bytes written to disk, and the semantic
  `redraw` `View`. They treat the editor as a black box and exercise the whole
  stack (RPC → server → core → Lua) end to end.
- **Tier 1 — client paint & key translation** ([`crates/bemtvi-tui/tests/`](../crates/bemtvi-tui/tests/))
  render a known `View` into a cell grid via ratatui's `TestBackend`
  (`bemtvi_tui::paint`) and assert on the painted cells, and test the
  crossterm-`KeyEvent`→key-notation translation (`bemtvi_tui::encode_key`)
  directly. Fast and fully deterministic — no process, no timing.
- **Tier 2 — full-stack screen** ([`crates/bemtvi/tests/screen.rs`](../crates/bemtvi/tests/screen.rs))
  drive the real server in-process, capture the real `redraw`, paint it with the
  real client, and assert on the cell grid — the deterministic "what the user
  sees" workhorse. Also asserts the non-blocking guarantee (a UI that never
  drains redraws can't stall the editor).
- **Tier 3 — PTY smoke** ([`crates/bemtvi/tests/e2e.rs`](../crates/bemtvi/tests/e2e.rs))
  drive the actual `bemtvi` binary through a pseudo-terminal (`portable-pty`),
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
- Lua 5.4 scripting running inside the server.
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
- A branching undo tree of full-rope snapshots (cheap via ropey's structural
  sharing) rather than neovim's diff-based `undo.c` change records — same
  branching semantics (`:undo {N}`, `vim.fn.undotree()`), different storage.
- **In-process treesitter** with installable grammars and incremental parsing —
  like neovim, but kept off `bemtvi-core` behind a `SyntaxEngine` trait (so the
  pure core never links tree-sitter) and bounded by a parse deadline (see
  [*Syntax highlighting*](#syntax-highlighting-treesitter)).

**Not yet implemented (roadmap).** The big-ticket items below; the granular
`vim.*` gaps and the silent approximations live in
[*Known approximations & missing features*](known-approximations.md).

- **The native plugin system (`btv.*`)** — the headline extensibility item:
  server-owned UI surfaces (completion engine, fuzzy picker, statusline
  segments, snippet engine, tree docks) with plugins as async, declarative
  *providers* registered through manifest-declared contributions, plus the
  built-in package manager. Design:
  [the native plugin API](specs/2026-06-11-native-plugin-api.md); decision:
  [ADR 0002](decisions/0002-native-plugin-system.md). Suggested build order is
  in the spec (picker → completion → statusline/snippets/tree). **Landed so far:**
  the fuzzy **picker** (`btv.picker`, with a preview pane), the **completion
  engine** (`btv.complete`, buffer + lsp + snippets sources), the **snippet
  engine** (`btv.snippet` — LSP snippet-syntax parsing, the tabstop session with
  `<Tab>`/`<S-Tab>` navigation and mirrored placeholders; see
  [the snippet plan](plans/2026-06-15-btv-snippet-engine.md)), the **statusline
  segment registry** (`btv.statusline` — the lualine-shaped surface: built-in
  segments resolved natively each frame plus custom `btv.statusline.segment{}`
  providers re-rendered only on declared events / `invalidate`, composed through the
  shared `%`-format [`layout`](../crates/bemtvi-core/src/statusline.rs) so clients
  paint it unchanged; see
  [the segment plan](plans/2026-06-15-btv-statusline-segments.md)), **viewport
  decorations** (`btv.decor` — off-tick providers woken once per visible-range
  change that publish generation-gated extmarks; a published mark takes the **same
  option vocabulary** `btv.buf.set_extmark` takes and is validated + lowered by the
  same code, so a provider draws highlights, gutter signs, virtual text/lines and
  line backgrounds alike — what `publish` adds is the *lifecycle* (the generation
  gate and the wholesale namespace republish), not a mark shape of its own;
  see [the decor plan](plans/2026-06-15-btv-decor-viewport-decorations.md)), the
  **floating-widget UI layer** (`btv.ui.input`/`select`/`confirm`/`float`,
  promise-based; see
  [the content-float plan](plans/2026-06-15-btv-ui-float-content-float.md)), and
  the **tree docks** (`btv.dock` — VSCode-style permanent edge panels with
  per-region tablines; see
  [the docked-panels plan](plans/2026-06-14-permanent-docked-panels.md)). Every
  widget's keys are rebindable through the real keymap engine
  ([configurable widget keys](plans/2026-06-16-configurable-widget-keys.md)).
  The **built-in package manager** has landed too: `btv.plugins` — declarative
  specs, async git install, eager + lazy (`cmd`/`event`/`ft`/`keys`) activation,
  `:PluginSync`, and the `:Plugins` dashboard.
- **Treesitter control.** `:TSInstall` / `:TSUpdate` / `:TSInstallInfo` have
  **landed**: the native arm fetches + compiles each grammar into the data dir
  off the editor thread (`bemtvi_ts::install`, with a pinned checksum-verified Zig
  fetched on demand when no system `cc`/`clang`/`gcc`/`zig`/`$BEMTVI_CC` is found),
  and the browser arm fetches a *prebuilt* `.wasm` grammar instead. The
  `:set`-driven highlight toggle has landed too. (Residual `:TSInstall` edges —
  grammars needing `tree-sitter generate`, no install-from-`HEAD` — are tracked in
  [*Known approximations*](known-approximations.md).) Treesitter **injections**
  have landed (engine-native, see
  [*Syntax highlighting*](#syntax-highlighting-treesitter)). Treesitter is the
  **native engine**; control is declarative buffer state (see
  [*Treesitter control*](#treesitter-control--declarative-buffer-state-not-a-parser-api)):
  highlight on/off + language are buffer nouns (`btv.bo.filetype` /
  `btv.bo.ts_highlight`, also reachable from `:set`), query customization is the
  native bridge `btv._btv_set_ts_query`, and **injections** are engine-native. There
  is no Lua parser/AST platform (the vendored `vim.treesitter` Lua was deleted — ADR 0002);
  **Lua-driven indent remains the one deferred item on this axis.**
- **Window-local options.** Multiple **windows** (splits, the layout tree,
  per-window view state, the `<C-w>` family, and the `nvim_win_*` RPC + Lua-read surface),
  **floating windows** (`nvim_open_win` with `relative`, the z-ordered overlay
  layer, `nvim_win_set_config`, and the `:q`/`:only`/focus/autocmd edge
  semantics), and **tab pages** (a `Vec<TabSlot>` deriving the active
  `WindowTree`, the tabline, `gt`/`:tab*`/`<C-w>T`, the `Tab*` autocmds, the
  `nvim_tabpage_*` Lua surface, and `showtabline`) are all implemented — see
  [*Windows*](#windows). What remains on this axis is the long tail of vim's
  window-local options (`colorcolumn`, `scrolloff`, `signcolumn`, `foldcolumn`,
  `fillchars`, and `winhighlight` are already wired).
- **The `btv.*` config surface** — `init.lua` targets bemtvi's own API
  ([ADR 0002](decisions/0002-native-plugin-system.md)); the prelude's current
  vim-shaped spelling is donor code, refactored under `btv` where it serves
  bemtvi's objectives and deleted where it doesn't, with the muscle-memory
  aliases as the only lasting `vim.*`. What the runtime already does: the runtimepath,
  `require`, `init.lua`,
  `nvim_set_hl`, `:colorscheme`, and `vim.keymap.set`/`vim.api.nvim_set_keymap`
  (a per-mode withhold/replay matcher in `bemtvi-server/src/keymap.rs`; multi-key
  built-ins fire instantly even under a colliding user prefix, via the shared
  command grammar `bemtvi_core::command_status` the matcher consults) are in place
  — enough to load a full colorscheme end to end (see [*Lua*](#lua)).
  The **LSP and diagnostics surface** is native too: the `bemtvi-lsp` crate
  (client, protocol, manager, transport) does bemtvi's own stdio spawning and
  drives the in-core editing features, and `btv.lsp` is its Lua control surface
  (server registration, enable, on-attach) — per
  [ADR 0002](decisions/0002-native-plugin-system.md). (Design background: the
  [LSP support design](specs/2026-06-02-lsp-support-design.md) and the
  [completion plan](plans/2026-06-05-lsp-completion.md).)

  **What does *not* work yet** is tracked canonically — both the *silent
  approximations* (a feature that looks whole but isn't) and the *loud gaps*
  (functions that raise `not implemented` rather than fake a value, per the
  no-silent-stubs rule) — in
  [**Known approximations & missing features**](known-approximations.md). That
  doc explains how to enumerate them straight from the code (`grep -rn
  'INCOMPLETE:'` for approximations, the `btv._notimpl` raises / runtime
  `btv._notimpl_hits` scoreboard for loud gaps) and lists the absent subsystems
  that have no call site to tag — the bulk of vim's options beyond the subset
  bemtvi honors (the registry is `canonical()` in
  `crates/bemtvi-core/src/editor/options.rs`: the window-local gutter / wrap /
  scroll options, the buffer-local indentation options, and a steadily growing
  set besides; many others are not wired) and richer diagnostic
  surfaces. (Blocking reads — `vim.fn.input` / `vim.fn.confirm` / `vim.fn.getcharstr`
  / `vim.wait` and the coroutine pump that hosted them — are **not** part of the
  `btv` model: nothing in it blocks the editor, so
  the only prompt surface is the promise-based `btv.ui.input` / `btv.ui.select`,
  with callback-shaped `vim.ui.*` aliases.)
  Legacy Vimscript (`eval.c`) is **not** on the roadmap — see guiding principle 2.
- A broad options surface. `:set` exists and honors the search booleans, the
  **window-local** number-gutter options `number` / `relativenumber`, the
  cursor-line highlight `cursorline`, and soft word-wrap `wrap`
  (`breakindent`/`showbreak`) (also via `:setlocal` / `vim.wo` /
  `nvim_win_{get,set}_option`) and the window-local
  horizontal-scroll options `sidescroll` / `sidescrolloff` (via `:set`), and the
  **buffer-local** indentation options `tabstop` / `shiftwidth` / `softtabstop` /
  `expandtab` and `commentstring` — the comment template the `gc`/`gcc` operator
  reads, set per buffer or defaulted from the filetype for the ~20 most common
  languages (all also via `:setlocal` / `vim.bo`); scoped
  `nvim_{set,get}_option_value` routes to the right scope. The bulk of vim's
  options are still missing.
  Named/numbered/special **registers** (`:registers`, `setreg`/`getreg`, the
  system clipboard `"+`/`"*`) and **marks** (buffer-local `a`–`z`, global file
  marks `A`–`Z`, the automatic special marks, `:marks`, and `'{mark}` ex-ranges)
  are both done; what remains here is macros and the `:map`-family
  ex-commands (intentionally postponed — keymaps are set via `vim.keymap.set` /
  `nvim_set_keymap`).
  **Code folding** is done across all four sources — manual (`zf`/the `z` family),
  `foldmethod=indent`, `foldmethod=expr` (the native tree-sitter foldexpr and a
  generic per-line Lua `'foldexpr'`), and LSP `textDocument/foldingRange` — sharing
  one per-window fold model (see *Folds* under the view section below). (The interactive `/` / `?` cursor search — `n`/`N`,
  `hlsearch`/`incsearch`, the search options — and `:s` substitution, which
  shares search's canonical-regex engine, are both done; see
  [the search design](specs/2026-06-02-search-design.md) and
  `docs/plans/2026-06-07-substitute-command.md`.)
- **Per-buffer user commands.** *Done.* `nvim_buf_create_user_command(buffer, …)`
  stores into a per-bufnr registry (`btv._buf_user_commands[bufnr][name]`, the
  command analogue of the buffer-local `btv._keymaps`): `btv._resolve_user_command`
  gives a buffer-local command precedence over a global of the same name and hides
  it from other buffers, dispatch routes through the editor's authoritative current
  bufnr, `nvim_buf_get_commands` returns a buffer's locals, and a wiped buffer's
  locals (commands *and* keymaps) are purged via `btv._cleanup_buffer` so a reused
  bufnr can't inherit them.
- **An async Lua runtime (event loop).** *Landed* (see
  [the async-runtime plan](plans/2026-06-06-async-lua-runtime.md)). A `Send` background actor
  (`crates/bemtvi-server/src/evloop.rs`, modeled on `LspManager`) owns timers and
  child processes; on completion it sends a typed `LoopEvent` back to the single
  server thread, which runs the matching Lua callback by id (the `btv._cb_fns`
  registry, the keymap-callback shape applied to async work). `vim.schedule`
  defers to convergence, `vim.defer_fn` fires on wall-clock time, and process
  completions settle their promises off-tick (there is no `vim.system` —
  spawning is the promise-based `btv.run`). neovim's libuv-as-public-API surface
  — `vim.uv` / `vim.loop`, both the **handle** primitives
  (`new_timer`/`new_check`/`new_fs_event`/`spawn`) and the synchronous `fs_*` /
  scalars — is **not** part of the `btv` model and is absent entirely.
  The `btv` async primitives (`btv.run` / `btv.timer` / `btv.fs`) are built on this
  machinery ([ADR 0002](decisions/0002-native-plugin-system.md)).
- The `vim.*` glue, kept only as far as colorschemes need
  ([ADR 0002](decisions/0002-native-plugin-system.md)).
  (The backend is vendored PUC Lua 5.4, the single baked-in backend — see
  [*Lua*](#lua).)

[`tokio::io::duplex`]: https://docs.rs/tokio/latest/tokio/io/fn.duplex.html
[mlua]: https://docs.rs/mlua
