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
| `nxvim-ts`      | `lua/vim/treesitter/`, `tree_sitter/`                | The **in-process treesitter engine**: an ordinary library that loads installable grammars and parses incrementally, implementing `nxvim-core`'s `SyntaxEngine` trait. Heavy C deps (`tree-sitter`, `libloading`) live here only. |
| `nxvim`         | the `nvim` entry point                               | Wires an embedded server + the TUI client together over RPC. |

Dependency direction is strictly one-way:

```
        nxvim (bin)
        /         \
 nxvim-server   nxvim-tui
  / | | \           \
core rpc lua ts     rpc
      \_______/    /
       tree-sitter
```

The treesitter engine is a normal crate dependency now: `nxvim-server`
constructs it and installs it on the editor (which owns a `Box<dyn SyntaxEngine>`
defined in `nxvim-core`), then queries it **synchronously** at redraw. See
[*Syntax highlighting*](#syntax-highlighting-treesitter) below and the design at
[`docs/specs/2026-06-06-in-process-treesitter-and-indentation-design.md`](specs/2026-06-06-in-process-treesitter-and-indentation-design.md).

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

#### Multi-source scheduling & event ordering

The server's `tokio::select!` loop (`nxvim-server::run`) multiplexes **three**
event sources against the single-threaded editor: RPC input from the UI, the LSP
manager, and the async-runtime actor (`evloop.rs` — timers and child processes).
Treesitter is *not* one of them — it runs in-process and is queried synchronously
during `redraw`, so highlighting needs no channel or arm. Each source is an mpsc
channel; the
matching async actor (a `Send` background task) only ever ferries ids / bytes /
durations back, never the `!Send` editor or Lua state. This is nxvim's analog of
neovim's main-thread + worker-thread model, where workers hand results to the one
editor thread by enqueuing events — see
[neovim's threading model](neovim-threading-model.md) for the reference design.

Two ordering properties hold, and one is a deliberate divergence worth recording:

- **Serialization (preserved).** Every `select!` arm body runs to completion
  before the next loop iteration — the off-tick arms are fully synchronous, and
  each ends in the *settle contract* (`apply_lua_effects` → `run_pending` →
  `redraw`) so a callback's deferred work converges and repaints at a
  redraw-followed point, never "too early". Two events can never interleave their
  mutations: neovim's "editor logic never runs concurrently with itself"
  guarantee, enforced here by the crate boundary (`nxvim-core` is pure/sync) and
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
a **list of windows** plus the global chrome. Each `WindowView` carries one
window's `rect`, focus flag, visible text rows, cursor, selection/search spans,
gutter numbers, and status-line data (file name, modified flag, ruler); the
`View` adds the inter-split `separators` and the **global** fields one editor has
(mode label, command line, message, panel). The server sends it as a single
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
then draws the `separators` between splits and reserves the bottom rows for the
global command/message line and panel. The terminal cursor is drawn only in the
`focused` window. Because the core lays out the windows (vertical splits divide
width), the client reports **both** dimensions of the windows area on
`nvim_ui_attach`/`nvim_ui_try_resize`. There is still no grid, no cell encoding,
and no `ext_linegrid`.

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
them. `nxvim-core`'s `Editor` separates the two concerns vim keeps apart:

- **Buffer state** (the "file"): the rope text, path, `modified`,
  `changedtick`, the edit journal, **and** per-buffer undo/redo history. These
  live in an `OpenBuffer` (the text `Buffer` plus its undo stacks and the
  cursor/scroll position saved while the buffer is not current), stored in a
  `BufferStore` keyed by a monotonic, 1-based `BufferId` that is never reused.
- **Window state** (the "view"): the live cursor, scroll `top`, mode, and
  pending-input state stay on `Editor`, alongside `current` (the shown buffer)
  and `alternate` (vim's `#`). The register and the search options are still
  **global**, but options come in three scopes, mirroring vim:
  - The indentation options (`tabstop` / `shiftwidth` / `softtabstop` /
    `expandtab`) are **buffer-local** — a `BufferOptions` lives on each `Buffer`,
    set via `:set`/`:setlocal`/`vim.bo`, so two buffers can indent differently.
    nxvim's defaults differ from vim's: `tabstop` is 4, and
    `shiftwidth`/`softtabstop` follow it via their `0`/`-1` sentinels
    (`softtabstop → shiftwidth → tabstop`), so one knob sets the indent width.
  - The number-gutter options (`number` / `relativenumber`) are **window-local** —
    a `WindowOptions` lives on each window, set via `:set`/`:setlocal`/`vim.wo`
    (and `nvim_win_{get,set}_option` / scoped `nvim_{get,set}_option_value`), and
    a split inherits them from the window it splits off, so two windows onto the
    same buffer can show different gutters.

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
by a layout tree. nxvim mirrors the buffer split: just as buffer state was
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
  Focus: `<C-w>h/j/k/l` (spatial), `<C-w>w`/`<C-w>W` (cyclic). Close: `<C-w>c`/
  `:close`, `<C-w>o`/`:only`, `:hide`, `<C-w>q`/`:q`. Sizing: `<C-w>=`,
  `<C-w>+`/`-`/`<`/`>` (with counts), `<C-w>_`/`<C-w>|`, `:resize`/`:vertical
  resize`. `focus_window` is the window analogue of the buffer switch: it stashes
  the outgoing view, restores the incoming one (cursor re-clamped), and clears
  transient state.
- **RPC / Lua.** `nvim_list_wins`, `nvim_get_current_win`/`nvim_set_current_win`,
  `nvim_win_get_buf`/`set_buf`, `nvim_win_get_cursor`/`set_cursor` (window-handled,
  `0` = current), `nvim_win_get_width`/`height` + setters, `nvim_win_close`,
  `nvim_win_get_config`/`nvim_win_get_position`, and `nvim_open_win` (both the
  split form and the float form). The Lua bindings follow the established "Lua
  queues, core mutates" flow: window *reads* resolve against the `vim._wins`
  mirror the server pushes before each Lua entry; window *mutations* queue a
  `WindowOp` drained into the core after the chunk.
- **Floating windows.** A float is a `Window` the layout tree does **not** own: it
  lives in `WindowTree.floats` (ids kept sorted by `(zindex, id)`), carries a
  `FloatConfig` (`relative` editor/win/cursor, `anchor`, `row`/`col`, `width`/
  `height`, `zindex`, `focusable`, `border`, `title`), and is positioned
  absolutely by a second `layout()` pass after the tiled rects are known — so it
  steals no space from its siblings and paints on top. `nvim_open_win` with a
  non-empty `relative` opens one (RPC and Lua, the latter via `WindowOp::OpenFloat`);
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
- **Autocmds.** `WinNew`/`WinEnter`/`WinLeave`/`WinClosed`/`WinResized` fire from
  the same server-side lifecycle diff as the buffer events, ordered
  `WinLeave → BufLeave/BufEnter → WinEnter` around a focus change.
- **Shared per buffer.** Two windows onto one buffer share its `SyntaxState`,
  diagnostics, and undo — each just projects a different `(top, height)` slice.
  The register, command line, message line, and panel stay **global**; the
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
the buffer: `Editor::windows` stays the *active* tab's live `WindowTree`, while
inactive tabs stash their tree in a `Vec<TabSlot>` (`current_tab` indexes the
active one). A switch (`gt`/`gT`/`:tabnext`/`nvim_set_current_tabpage`) is a
`mem::swap` of the live tree with the target's stash, then re-enters the incoming
tab's focused window — the tab analogue of `focus_window`, so the entire editing
machine is untouched (`self.windows` is always the active layout). `:tabnew` /
`:tabedit` / `<C-w>T` create one (window ids are minted globally off
`Editor::next_win_id` so they never collide across tabs); `:tabclose` / `:tabonly`
refuse the final tab, and `:q` on a tab's last window closes the *tab* while
others remain. The `View` carries a `Vec<TabView>` (focused buffer name +
modified flag + window count) and the active index, gated — along with the
reserved top row the server's `relayout` subtracts — by the global `showtabline`
(0/1/2) through one `tabline_visible` check. The lifecycle diff fires
`TabNew`/`TabLeave`/`TabEnter`/`TabClosed`, bracketing the window events
(`TabLeave → WinLeave → … → WinEnter → TabEnter`); the `nvim_tabpage_*` reads
resolve against a `vim._tabs` mirror and `nvim_set_current_tabpage` queues a
`TabOp`, the same "Lua queues, core mutates" flow as windows. (Design:
[`docs/plans/2026-06-07-tab-pages.md`](plans/2026-06-07-tab-pages.md).)

Still pending: **line wrapping** (`wrap` — the display-row projection; Phase 2 of
the horizontal-scroll plan) and **more window-local options** (`cursorline`, …)
beyond the number gutter and the scroll options that already ride
`WindowOptions`. Floating windows are otherwise complete (model,
paint, dynamic config, edge semantics); the remaining float fidelity knobs
(`style="minimal"`, `footer`, `bufpos`, `relative="mouse"`) grow as a consumer
demands them. All four `laststatus` modes ship (`0` never, `1` only with ≥2
windows, `2` per-window default, `3` a single global status line).

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
- **Long lines wrap, they don't clip.** The panel is full-width with no
  horizontal scroll, so `panel_view` **word-wraps** each entry to the panel width
  when it projects (breaking on spaces, hard-breaking an over-long run; counted in
  screen cells, so tabs/wide chars line up). Wrapping is display-only: the
  `cursor`/`top` stay *logical-entry* indices, so `j`/`k`/`<CR>`/jump targets still
  address whole entries and a long hover/message/location row is laid out across
  rows instead of being cut at the right edge. The projection carries a
  `cursor_span` (how many display rows the selected entry occupies) so the client
  highlights the whole wrapped entry as one focused line, and the vertical
  scroll-into-view is display-row aware so a tall entry's last row stays visible.
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
(the `nxvim-lua/src/prelude/` modules, the analogue of neovim's `runtime/lua/vim/`):
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
and their `highlights.scm` queries, parsed **in-process**:

- **In-process, synchronous.** The editor owns the parser (a `Box<dyn
  SyntaxEngine>` whose trait lives in `nxvim-core`, implemented by `nxvim-ts`) and
  queries it during `redraw`, so spans are correct in the **same frame** as the
  keypress — no worker process, no RPC, no async catch-up frame. This is neovim's
  posture: a buggy grammar (compiled C) can segfault the editor, a risk accepted
  because grammars are user-installed and stable, bounded by a **parse deadline**
  (a per-parse wall-clock budget; on expiry the last good tree is kept, costing
  one frame of stale highlights rather than a hang). It also unblocks treesitter
  *indentation* and the future `vim.treesitter` Lua API, both of which need a
  synchronously-queryable tree.
- **Installable grammars.** Grammars are not bundled; they load dynamically by
  filetype from a data directory laid out exactly like neovim's
  (`<data>/parser/<lang>.so`, `<data>/queries/<lang>/highlights.scm`), so an
  existing nvim-treesitter tree is drop-in usable.
- **Incremental parsing.** The engine keeps a shadow buffer and a persistent
  parse tree per buffer; it applies only **edit deltas** (`InputEdit`) drained
  from the `Buffer` edit journal in `nxvim-core` (`changedtick` + `BufferEdit`s),
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

### The `vim.treesitter` Lua platform

Parallel to (not a replacement for) the redraw highlighter above, nxvim exposes
neovim's **`vim.treesitter` Lua API** so treesitter-consuming plugins (textobjects,
AST/query tools, query-driven motions) run unmodified. It mirrors neovim's own
split — a small bespoke C-equivalent layer with neovim's real Lua on top:

- **Bespoke primitives in Rust** (`nxvim-ts/src/lua.rs`, behind the crate's `lua`
  feature): the `TSParser`/`TSTree`/`TSNode`/`TSQuery` userdata, `TSQuery:inspect`,
  and `vim._create_ts_querycursor` — the analogue of neovim's
  `src/nvim/lua/treesitter.c`. The cursor is ported over the raw `tree_sitter::ffi`
  so matches are returned **unfiltered** (predicates evaluate in Lua, bug-for-bug
  with upstream). The node/tree lifetime over the Lua boundary is reconciled by
  co-owning an `Rc<TreeInner>` and erasing the borrow to `'static` (sound because
  trees are immutable snapshots).
- **Vendored neovim Lua on top** (`nxvim-lua/src/vendor/nvim/`, Apache-2.0, kept
  verbatim with a provenance header): `vim/treesitter/*.lua` + the `vim.func` /
  `vim.F` / `vim._core.util` / `vim.pos._util` helpers, embedded into
  `package.preload` so it ships in the binary with no dependency on the
  `vendor/neovim` submodule. `nxvim-lua/src/prelude/treesitter.lua` wires it onto
  the primitives.
- **The snapshot seam.** Unlike neovim's live buffer handle, nxvim's Lua bridge is
  a snapshot + effect queue. `TSParser:parse(bufnr)` reads the pushed
  `vim._bufs[bufnr]` lines, and a buffer-sourced `LanguageTree` re-reads that
  snapshot on every `:parse()` (a full reparse — there is no `nvim_buf_attach`).
  String parsers (`get_string_parser`) keep their incremental trees.

This is **additive**: a `LanguageTree` is created only when a plugin calls
`get_parser`, so buffers without a treesitter consumer pay nothing; one that has a
consumer pays a second parse (the Rust engine's + Lua's). `vim.treesitter.start` /
`stop` are wired as ADR 0001 bridge #1: they toggle the **native** engine for a
buffer (a `lang` override) rather than running neovim's Lua decoration-provider
highlighter on the redraw hot path, so a highlight-only buffer still parses once.
Lua-driven indent and injections remain non-goals for now. Full design:
[the `vim.treesitter` Lua platform](specs/2026-06-07-vim-treesitter-lua-platform.md).

The boundary this section embodies — **native engine for editor behavior,
vendored neovim Lua API for plugins** — is the same one LSP follows (a native
async server under the vendored `vim.lsp`), and it is recorded as a standing
decision in [ADR 0001](decisions/0001-native-engines-vendored-lua-apis.md). That
ADR also names the *bridge pattern* (`vim.treesitter.start`, LSP semantic tokens)
by which a vendored API is wired to the native engine underneath, projecting into
the extmark highlight layer rather than into core's synchronous path.

---

## Cross-platform & the future GUI

nxvim targets all major OSes (Linux, macOS, Windows). The dependency choices are
deliberately portable: `crossterm` for the terminal, `ropey`, `tokio`, and
`rmpv` are all cross-platform, and the in-process transport uses no OS-specific
IPC.

The terminal client is built on [ratatui](https://ratatui.rs) (over crossterm).
Because every front end is just a client of nxvim's own RPC, a **native GUI** —
notably a non-terminal GUI on Windows — is just another client crate consuming
the same `View` protocol, with zero changes to the server or core.

That claim is now load-bearing: **`nxvim-gui` is a prototype native GUI client**
([`crates/nxvim-gui`](../crates/nxvim-gui)) on **winit + wgpu + glyphon**. It is
the GUI sibling of `nxvim-tui` and reuses the same frontend-neutral
[`nxvim-view`](../crates/nxvim-view) decode/input layer (`View`, `Style`, `Key`,
`notation`, `encode_paste`) — the seam the view crate was extracted for. The
`nxvim-gui` *binary* embeds a server on its own thread exactly like the `nxvim`
binary, joined by the same in-process duplex RPC; the only difference is the
client. winit owns the main thread (its loop is not async), so the RPC runs on a
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
incsearch, LSP diagnostic underlines + signs + inline virtual text, the
secondary multi-cursors, and the text style attributes (bold/italic via bold and
italic faces, underline/strikethrough/reverse as quads) — plus **pixel-smooth
scrolling**: the focused window slides the server's scroll-gesture band at a
fractional (sub-pixel) line offset driven by the client clock, paced per frame from
winit's `about_to_wait` (where the TUI animates at whole-row granularity, the GPU
client interpolates `top` without rounding). Input reaches parity too:
vim-notation keys, system-clipboard paste, native open/save dialogs, and **mouse**
— left click / drag-select / release, wheel scroll, right-click
(`'mousemodel'`), middle-click paste, and the pmenu / panel overlay gestures, sent
as the same `nvim_input_mouse` the TUI uses (the server owns the hit-test). Still
deferred: wide-char column fidelity (a char index stands in for a screen column),
and undercurl is drawn as a plain underline. Because the GUI can't be black-box
tested over RPC the way the TUI's paint is (it needs a GPU), only the pure,
frontend-specific translation layers have Tier-1 tests — the winit→notation input
(`crates/nxvim-gui/tests/keys.rs`) and the pointer/overlay math
(`crates/nxvim-gui/tests/mouse.rs`); the rendered frame is validated by running it.

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
- A branching undo tree of full-rope snapshots (cheap via ropey's structural
  sharing) rather than neovim's diff-based `undo.c` change records — same
  branching semantics (`:undo {N}`, `vim.fn.undotree()`), different storage.
- **In-process treesitter** with installable grammars and incremental parsing —
  like neovim, but kept off `nxvim-core` behind a `SyntaxEngine` trait (so the
  pure core never links tree-sitter) and bounded by a parse deadline (see
  [*Syntax highlighting*](#syntax-highlighting-treesitter)).

**Not yet implemented (roadmap).** The big-ticket items below; the granular
`vim.*` gaps and the silent approximations live in
[*Known approximations & missing features*](known-approximations.md).

- `:TSInstall`-style grammar fetch & compile (grammars are loaded from the data
  dir today; installing them there is manual / a follow-up), treesitter
  injections, and a `:set`-driven highlight toggle. The **`vim.treesitter` Lua
  platform** itself is in place — `get_parser(buf):parse()`, `get_string_parser`,
  and `query.parse` + `iter_captures`/`iter_matches` with predicates run neovim's
  vendored Lua on bespoke Rust primitives (see [*The `vim.treesitter` Lua
  platform*](#the-vimtreesitter-lua-platform)); `vim.treesitter.start` / `stop`
  toggle the native engine per buffer (ADR 0001 bridge #1), while injections and
  Lua-driven indent remain deferred.
- **Window-local options.** Multiple **windows** (splits, the layout tree,
  per-window view state, the `<C-w>` family, and the `nvim_win_*` / Lua API),
  **floating windows** (`nvim_open_win` with `relative`, the z-ordered overlay
  layer, `nvim_win_set_config`, and the `:q`/`:only`/focus/autocmd edge
  semantics), and **tab pages** (a `Vec<TabSlot>` deriving the active
  `WindowTree`, the tabline, `gt`/`:tab*`/`<C-w>T`, the `Tab*` autocmds, the
  `nvim_tabpage_*` Lua surface, and `showtabline`) are all implemented — see
  [*Windows*](#windows). What remains on this axis is more window-local options
  (`wrap`, `cursorline`, …).
- A broader Lua `vim.*` API surface. The runtimepath, `require`, `init.lua`,
  `nvim_set_hl`, `:colorscheme`, and `vim.keymap.set`/`vim.api.nvim_set_keymap`
  (a per-mode withhold/replay matcher in `nxvim-server/src/keymap.rs`; multi-key
  built-ins fire instantly even under a colliding user prefix, via the shared
  command grammar `nxvim_core::command_status` the matcher consults) are in place
  — enough to run the real catppuccin colorscheme unmodified (see [*Lua*](#lua)).
  So is the **`vim.lsp`/`vim.diagnostic` surface**: a server is configured and
  started entirely from user Lua (`vim.lsp.config`/`vim.lsp.enable`, with the
  built-in server table removed), `vim.lsp.buf.*` and `vim.diagnostic.*` drive the
  native features, and `LspAttach`/`on_attach` wire buffer-local LSP keymaps off
  `client.server_capabilities` — verified against the vendored nvim-lspconfig (see
  the [LSP support design](specs/2026-06-02-lsp-support-design.md)).
  **All ~400 vendored `lsp/<server>.lua` configs LOAD and START unmodified, and
  the nxvim-side gap between merely *starting* a server and actually *driving* it
  — the careful distinction the LSP completion plan tracked — is now closed (all
  eight phases landed).** The config-resolution surface is real and
  regression-tested (`crates/nxvim-lua/tests/lspconfig_configs.rs` loads every
  config and resolves its `root_dir` + `cmd`): `vim.system`/`vim.json`/`vim.uv`
  (`fs_stat`, `os_homedir`, `cwd`, `fs_realpath`, `os_uname`), the `vim.fn`
  filesystem/process helpers (`executable`, `exepath`, `glob`, `resolve`,
  `getpid`, …), `vim.fs.root` with neovim 0.11 priority-tier markers, `vim.iter`
  over iterators, `vim.version`, `vim.tbl_get`/`tbl_flatten`, and the real
  `lspconfig.util` framework module (required by ~33 configs). The many configs
  whose `cmd` is a `function(dispatchers, config)` builder returning
  `vim.lsp.rpc.start({argv}, …)` (ts_ls, eslint, jsonls, biome, tailwindcss, … —
  20-plus) resolve to a real argv because nxvim does its own stdio spawning. Once
  a server is up, the editing loop — `vim.lsp.buf.*`, `vim.diagnostic.*`, and
  capability-gated `on_attach` keymaps — genuinely works.

  Concretely, the config's `settings` / `init_options` / `capabilities` are
  forwarded at `initialize`, the lifecycle hooks (`before_init` / `on_init` /
  `on_exit`) fire, and the deferred-callback surface (`vim.lsp.util.*`,
  `client:request`, `vim.ui.*`, the buffer/window getters) is real — see
  [the LSP completion plan](plans/2026-06-05-lsp-completion.md) for the phase-by-phase route
  it took from "starts" to "works". Two whole configs are skipped: gdscript (a non-stdio
  TCP transport, `vim.lsp.rpc.connect`) and powershell_es (needs a user-only
  `bundle_path`).

  **What does *not* work yet** is tracked canonically — both the *silent
  approximations* (a feature that looks whole but isn't) and the *loud gaps*
  (functions that raise `not implemented` rather than fake a value, per the
  no-silent-stubs rule) — in
  [**Known approximations & missing features**](known-approximations.md). That
  doc explains how to enumerate them straight from the code (`grep -rn
  'INCOMPLETE:'` for approximations, the `vim._notimpl` raises / runtime
  `vim._notimpl_hits` scoreboard for loud gaps) and lists the absent subsystems
  that have no call site to tag — the
  `vim.treesitter` Lua API, the bulk of vim's options beyond the handful nxvim
  honors (window-local `number`/`relativenumber` + the horizontal-scroll
  `sidescroll`/`sidescrolloff`, the buffer-local indentation options, and global
  `showtabline` are wired; `wrap`/`cursorline`/… are not), a per-buffer command
  registry, and richer diagnostic surfaces. (The **synchronous prompts**
  `vim.fn.input` /
  `vim.fn.confirm` are now implemented: a pumped entry — a `:lua` chunk, keymap,
  or user command — runs its Lua inside a coroutine via `vim._pump`, so the prompt
  `coroutine.yield`s to park the chunk on the command line and the result resumes
  it inline. See `examples/sync-prompts/`.) Legacy Vimscript (`eval.c`) is **not**
  on the roadmap — see guiding principle 2.
- A broad options surface. `:set` exists and honors the search booleans, the
  **window-local** number-gutter options `number` / `relativenumber` (also via
  `:setlocal` / `vim.wo` / `nvim_win_{get,set}_option`) and the window-local
  horizontal-scroll options `sidescroll` / `sidescrolloff` (via `:set`), and the
  **buffer-local** indentation options `tabstop` / `shiftwidth` / `softtabstop` /
  `expandtab` (also via `:setlocal` / `vim.bo`); scoped
  `nvim_{set,get}_option_value` routes to the right scope. The bulk of vim's
  options are still missing.
  Named/numbered/special **registers** (`:registers`, `setreg`/`getreg`, the
  system clipboard `"+`/`"*`) and **marks** (buffer-local `a`–`z`, global file
  marks `A`–`Z`, the automatic special marks, `:marks`, and `'{mark}` ex-ranges)
  are both done; what remains here is folds, macros, and the `:map`-family
  ex-commands (intentionally postponed — keymaps are set via `vim.keymap.set` /
  `nvim_set_keymap`). (The interactive `/` / `?` cursor search — `n`/`N`,
  `hlsearch`/`incsearch`, the search options — and `:s` substitution, which
  shares search's canonical-regex engine, are both done; see
  [the search design](specs/2026-06-02-search-design.md) and
  `docs/plans/2026-06-07-substitute-command.md`.)
- **Per-buffer user commands.** User commands live in one global registry, so
  `nvim_buf_create_user_command(buffer, …)` currently registers *globally*
  (the buffer argument is ignored) — enough for an `on_attach` that defines a
  convenience command (e.g. rust_analyzer's `:LspCargoReload`) to load without
  error, but the command then exists everywhere rather than only in its buffer.
  A per-buffer command registry (the command analogue of the buffer-local
  options on `Buffer` / buffer-local keymaps, which `vim._keymaps` already
  scopes) is the fix.
- **An async Lua runtime (event loop).** *Landed* (see
  [the async-runtime plan](plans/2026-06-06-async-lua-runtime.md)). A `Send` background actor
  (`crates/nxvim-server/src/evloop.rs`, modeled on `LspManager`) owns timers and
  child processes; on completion it sends a typed `LoopEvent` back to the single
  server thread, which runs the matching Lua callback by id (the `vim._cb_fns`
  registry, the keymap-callback shape applied to async work). `vim.schedule`
  defers to convergence, `vim.defer_fn`/`vim.uv` timers fire on wall-clock time,
  and `vim.system`'s `on_exit` fires off-tick. The `vim.uv`/`vim.loop` surface
  beyond timers (`new_pipe`, TCP, event-based `fs_*`) still grows as plugins demand
  it — e.g. `vim.lsp.rpc.connect`'s TCP transport (the skipped gdscript config).
- LuaJIT (in place of vendored Lua 5.1) and the full `vim.*` standard library.
- A native, non-terminal GUI client (e.g. for Windows).

[`tokio::io::duplex`]: https://docs.rs/tokio/latest/tokio/io/fn.duplex.html
[mlua]: https://docs.rs/mlua
