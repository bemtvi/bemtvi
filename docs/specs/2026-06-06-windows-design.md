# Windows (splits) — phased design

Status: **Complete (Phases 1–5 landed).** This was the implementation plan for
the **windows** feature — multiple viewports onto buffers, created by splitting,
arranged in a layout tree, each with its own cursor and scroll. In vim/neovim a
"window" and a "split" are the same thing: `:split`/`<C-w>s` and
`:vsplit`/`<C-w>v` *create* windows; `<C-w>` navigates and resizes them. So
"windows" here means **splits + the layout tree + per-window view state +
`<C-w>` commands + the `nvim_win_*` API**. **Tabs** (tab pages, each holding its
own window tree) and **floating windows** are explicit follow-ons — see the end.

This is the natural sequel to [multiple buffers](2026-06-01-multiple-buffers-design.md):
that work split *buffer* state (the file) from *window* state (the view) on
`Editor`, leaving "exactly one window onto one buffer." This work multiplies the
window. Read [`docs/architecture.md`](../architecture.md) first — especially
[*Buffers*](../architecture.md#buffers) (the model we mirror) and
[*View protocol*](../architecture.md#protocols) (the protocol that must grow to
carry many windows).

The relevant code is `crates/bemtvi-core/src/editor.rs` (window state is inline on
`Editor` today), `crates/bemtvi-core/src/view.rs` (the single-window `View`),
`crates/bemtvi-server/src/lib.rs` (the `redraw()` projection, the `nvim_*` surface,
per-buffer syntax routing), and `crates/bemtvi-tui/src/render.rs` (the client
layout, which currently lays out one text area + one status line).

---

## Design model

The multiple-buffers work already separated **buffer state** (rope, path,
`modified`, undo, edit journal — in an `OpenBuffer` keyed by `BufferId`) from
**window state** (`cursor`, `top`, `mode`, `desired_col`/`desired_eol`,
`visual_anchor`, scroll gesture — inline on `Editor`). Windows take the second
half and multiply it, exactly as buffers multiplied the first half. The pattern
is identical: the **active** window's view state stays live on `Editor` for the
hot editing path; **inactive** windows stash their cursor/scroll, just as an
inactive `OpenBuffer` stashes `saved_cursor`/`saved_top`.

```
Editor
├── buffers: BufferStore                 // unchanged (id -> OpenBuffer)
├── windows: WindowTree                  // NEW: the split layout + focus
│     ├── current: WindowId              // the focused window
│     └── root: Node                     // Leaf(Window) | HSplit/VSplit([children], sizes)
│           └── Window
│                 ├── buffer: BufferId    // which buffer this window shows
│                 ├── saved_cursor, saved_top   // view pos while NOT focused
│                 └── rect: Rect          // x,y,w,h, computed from the tree
│
├── cursor, top, mode, desired_col, …    // ACTIVE window's live view state
├── register, options, highlights        // GLOBAL (unchanged)
├── cmdline*, message, panel             // GLOBAL — one per editor (unchanged)
└── width, height                        // total text area the tree lays out into
```

Key decisions, each checkable against vim:

- **`WindowId(u64)`**, monotonic, never reused (like `BufferId`). The first
  window is allocated at startup bound to buffer 1.
- **The current buffer is *derived* from the current window.** `Editor::current`
  (the buffer the editing code reaches via `buffer()`) becomes
  `windows.get(current).buffer`. `:b`/`:e` rebind *the current window's* buffer;
  they no longer hold the buffer pointer directly. (Phase 1 keeps a single window,
  so this is a pure indirection with no behavior change.)
- **Active-window view state stays on `Editor`.** `cursor`/`top`/`mode`/
  `desired_col`/`visual_anchor`/scroll are the *focused* window's state, live on
  `Editor` — the entire normal/visual/insert state machine reads/writes them
  unchanged. Switching windows stashes them into the outgoing `Window` and
  restores from the incoming one (the `switch_buffer`/`enter_buffer` dance,
  applied to windows). **This is what keeps the 4949-line `editor.rs` state
  machine untouched** — the churn is at the seams (window switch, projection),
  not in the motions/operators.
- **The core owns the layout tree and rect computation.** Splits, sizes,
  equalization, and `<C-w>` resizing are editor state (you can script and resize
  them), so the tree lives in `bemtvi-core`, matching neovim where the editor owns
  the window layout and the UI just paints. The client keeps owning *chrome* (the
  bottom command/message line) and *how* each window's gutter/status/text looks —
  but *where* each window sits now comes from the core.
- **The client must report text-area width *and* height.** Today it reports only
  height (one column of text). Vertical splits divide width, so the core needs
  both. `width`/`height` already exist on `Editor`; Phase 2 just ensures
  `nvim_ui_attach`/`nvim_ui_try_resize` carry width and the layout uses it.
- **Per-window status lines.** With one window there is one status line at the
  bottom. With splits, **each window draws its own status line at its bottom
  edge** (vim's `laststatus=2` default), and the global bottom row is only the
  command/message line. This is the main client layout change.
- **Syntax/LSP state stays keyed by `BufferId`.** Two windows onto the same
  buffer share one `SyntaxState` and one diagnostic set; each window just projects
  a different `(top, height)` slice of the same spans. No new worker protocol.
- **Command line, message line, and the panel stay global** — one per editor,
  docked at the bottom, exactly as vim shows the cmdline below all splits. They do
  not multiply.

What does **not** change: `bemtvi-core` stays pure/sync; the buffer store, undo,
edit journal, `changedtick`; the register and options (still global — window-local
options like `wrap` are future work alongside buffer-local options). The trailing-
`\n` invariant and byte-offset model are untouched.

---

## Phase 1 — Refactor: window tree with a single window (no behavior change)

**Goal:** introduce the window data model with the tree holding exactly one leaf.
Pure refactor — every existing test passes unchanged, no new user-facing behavior.
This is the largest mechanical churn and is deliberately isolated, mirroring
Phase 1 of the buffers work.

**Changes (`crates/bemtvi-core/src/editor.rs`):**

- Add `WindowId(u64)` (public; the RPC layer and tests name windows by it).
- Add `Window { buffer: BufferId, saved_cursor: Cursor, saved_top: usize, rect:
  Rect }` and a small `Rect { x, y, width, height }` (or reuse an existing one —
  none exists in core today, add a plain struct; do **not** pull in ratatui,
  core stays UI-free).
- Add `WindowTree { nodes, root, current: WindowId, next_id: u64 }`. For Phase 1
  the tree is a single `Leaf(WindowId)`; model `Node` as
  `enum Node { Leaf(WindowId), Split { dir: SplitDir, children: Vec<Node>, sizes:
  Vec<usize> } }` now so Phase 3 only *populates* it. `SplitDir { Horizontal,
  Vertical }`.
- `Editor` gains `windows: WindowTree`; the `current: BufferId` field is
  **replaced** by a `cur_buffer(&self) -> BufferId` accessor that reads
  `windows.current`'s `Window::buffer`. `alternate` (the `#` buffer) stays on
  `Editor` (it is global in vim, not per-window — keep it as is for now).
- `Editor::with_buffer` seeds one `OpenBuffer` (unchanged) **and** one `Window`
  bound to it, set as `windows.current`.
- Route every internal `self.current` read through `cur_buffer()`, and every
  write (the buffer-switch path) through a new `set_cur_buffer(id)` that updates
  the current window's `buffer` field. `buffer()`/`buffer_mut()` resolve through
  it. This is the main churn; it is mechanical and confined to the buffer-switch
  and projection seams, not the motion code (which reads `self.cursor`, untouched).
- Add `cur_win(&self) -> &Window` / `cur_win_mut(&mut self)` accessors and a
  `WindowTree::layout(total: Rect)` that, for a single leaf, assigns the whole
  area — so `text_height()`/`text_width()` can be re-expressed in terms of the
  current window's rect in a later phase without changing the value now.

**Changes (`view.rs`, `server/lib.rs`):** mechanical — any `editor.current`
(buffer id) reader becomes `editor.cur_buffer()`. The `View` is **unchanged** in
Phase 1 (still one window's worth of fields).

**Tests:** none added; the existing `editing.rs` / `buffers.rs` / `screen.rs`
suites are the regression gate. Done when `cargo test --workspace` is green and
`cargo clippy --all-targets -- -D warnings` is clean (default features — see
CLAUDE.md; never `--all-features`).

**Handoff note for Phase 2:** the window tree, ids, and per-window saved view
position now exist but there is exactly one window and the `View` still describes
only it. Phase 2 grows the protocol to carry a *list* of windows so the client
can paint many — still rendering one, identically to today.

---

## Phase 2 — Multi-window View protocol + client render (still one window)

**Goal:** change the `View`/`redraw` to carry a **list of windows**, each with its
rect and its own per-window fields, plus a focused index and split separators; and
change the client to lay out and paint that list with per-window gutters, per-
window status lines, and separators. With exactly one window the output is
**pixel-identical to today** — this isolates the protocol + client churn from the
feature, and the Tier 2 screen tests are the regression gate.

**Changes (`view.rs`):**

- Introduce `WindowView` carrying the per-window fields that are currently flat on
  `View`: `rect`, `lines`, `cursor_row`, `cursor_col`, `cursor_screen_col`,
  `selection`, `search`, `incsearch`, `numbers`, `number`/`relativenumber`/
  `number_width`, `scroll`, `file_name`, `modified`, `cursor_line` (the status-
  line data), and a `focused: bool`.
- `View` keeps the **global** fields (`mode_label`, `command_mode`, `cmdline*`,
  `message`, `pending_replace`, `panel`) and gains `windows: Vec<WindowView>` plus
  `separators: Vec<Separator>` (the inter-split borders, as screen line segments
  with an orientation — the core knows them from the rect layout).
- `View::from_editor` builds the windows list. In Phase 2 it pushes exactly one
  `WindowView` for the current window, computed from the same `top`/`height`/
  `width` math as today (now sourced from `cur_win().rect`).

**Changes (`server/lib.rs`):** `redraw()` projects one `WindowView` per window.
The highlight projection (`highlights_for`) and per-buffer syntax slice must run
**per window** — for each window, look up that window's buffer's `SyntaxState`
and project the spans for *its* `(top, height)`. (One window → unchanged output.)
The `redraw` msgpack map gains a `windows` array (each a sub-map) and a
`separators` array; the per-window fields move under each entry. Bump/extend the
client decode in lockstep.

**Changes (`crates/bemtvi-tui/src/render.rs`, `view.rs`):**

- Decode the `windows` list and separators.
- Replace the single text-area layout with: lay out each `WindowView` at its
  `rect`; within each, split off its gutter (existing `render_gutter`) and paint
  its text/selection/search; draw a **status line at the bottom row of each
  window's rect** (move `render_status` to per-window, fed by that window's
  status fields). Draw separators (vertical `│`, horizontal `─`) between splits.
  The global bottom region keeps only the command/message line and the panel.
- The terminal cursor is drawn only in the **focused** window (`focused: true`),
  at its `cursor_row`/`cursor_screen_col` offset by its rect origin.
- Report **width** as well as height on attach/resize (`nvim_ui_attach` /
  `nvim_ui_try_resize`), so the core lays out vertical splits against real width.

**Tests:** the existing `screen.rs` Tier 2 suite (single window) must stay green —
that is the proof the protocol move is behavior-preserving. Add one Tier 1 paint
test (`crates/bemtvi-tui/tests/paint.rs`) feeding a hand-built two-window `View`
(two stacked rects, a separator, two status lines) and asserting the cell grid —
so the multi-window *renderer* is covered before any core command produces one.

**Handoff note for Phase 3:** the protocol and client can now display any number
of windows in arbitrary rects; the core still only ever emits one. Phase 3 makes
the tree actually split and assigns rects to multiple leaves.

---

## Phase 3 — Splits + focus navigation (the feature becomes real)

**Goal:** create and navigate windows. After this phase a user can `:split` /
`:vsplit`, move focus with `<C-w>` motions, and close windows — fully observable
end-to-end through the Phase 2 renderer.

**Layout (`editor.rs`, `WindowTree`):**

- `WindowTree::layout(total: Rect)` recursively divides the area: an `HSplit`
  stacks children vertically (dividing height, one separator row between each), a
  `VSplit` places them side by side (dividing width, one separator column
  between each), distributing by `sizes` and giving leftover cells to the first
  children (vim's behavior). Each leaf `Window` records its computed `rect`. A
  window's **text height** is `rect.height - 1` (its status line); its text width
  is `rect.width - number_width`.
- `split(dir)`: replace the current leaf with a `Split{dir}` of two leaves — the
  new window is a clone of the current (same buffer, copied cursor/scroll), and
  becomes... (vim: `:split` keeps focus in the **new top/left** window). Allocate
  a `WindowId`, re-layout, re-`ensure_visible` for both.
- `close()` (`<C-w>c` / `:close`): remove the current leaf; collapse its parent
  `Split` if it now has one child; pick the spatially-nearest sibling as the new
  current; re-layout. **Refuse to close the last window** (vim: `E444` / it's a
  quit — see Phase 4 for `:q` semantics).
- `only()` (`<C-w>o` / `:only`): drop every window but the current; tree becomes a
  single leaf spanning the whole area.
- Directional focus `<C-w>h/j/k/l` and cyclic `<C-w>w` / `<C-w>W`: pick the
  target `WindowId` (nearest leaf in that direction by rect geometry; cyclic for
  `w`/`W`), then **`focus_window(id)`**.
- `focus_window(id)`: the window analogue of `enter_buffer` — stash the live
  `cursor`/`top` into the outgoing `Window`'s `saved_cursor`/`saved_top`, set
  `windows.current = id`, restore the incoming window's saved view position,
  `set_cur_buffer(incoming.buffer)`, clamp cursor, `ensure_visible`, leave visual/
  pending state and clear `message` (mirrors `enter_buffer`). Fire `WinLeave`/
  `WinEnter` in Phase 5.

**Keys (`<C-w>` prefix) and ex-commands:**

- Bind the **`<C-w>` window-command prefix** in normal mode. Today `<C-w>` is
  unbound in normal mode (it only deletes a word in *insert* mode — leave that
  untouched), so the prefix is free. Add it to the `parse_step` grammar as a
  two-key sequence (`<C-w>` then one of `s v w W h j k l c o q + - < > = q`),
  reusing the existing pending-prefix machinery and `command_status` so a user
  map starting with `<C-w>` still disambiguates correctly (see the
  [keymap disambiguation design](2026-06-05-keymap-builtin-disambiguation-design.md)).
- Ex-commands in `execute_ex`: `:sp[lit]` (horizontal), `:vs[plit]` (vertical),
  `:new`/`:vnew` (split + `:enew`), `:clo[se]`, `:on[ly]`. Optional `+cmd`/file
  arg: `:split foo.txt` splits then edits `foo.txt` in the new window.

**Tests** (`crates/bemtvi-server/tests/windows.rs`, new file; same `start`/`feed`/
`lines`/`cursor` helpers, plus a `redraw`-based `windows()` reader taking the
**latest** queued redraw — see CLAUDE.md's take-latest rule):

- `<C-w>s` then assert two windows in the `View`, stacked, both on the same
  buffer; `<C-w>v` → side by side.
- Editing in one window is visible in the other when they share a buffer (type in
  the bottom split, assert the top split's `lines` updated — proves shared buffer,
  independent view).
- `<C-w>j`/`<C-w>k` move focus (assert which `WindowView.focused` is true);
  cursor in each window is independent (move in one, focus the other, focus back —
  position restored).
- `<C-w>c` closes the focused window and the survivor expands to full area;
  `<C-w>o` drops all but current.
- `:vsplit foo.txt` puts a *different* buffer in the new window (assert each
  window's `file_name`).

**Handoff note for Phase 4:** windows split, navigate, and close, but they always
split **evenly** and there is no manual resize, and `:q` still quits the whole
editor regardless of window count. Phase 4 adds resizing and the window-aware
quit/equalize semantics.

---

## Phase 4 — Resizing, equalization, and window-aware quit

**Goal:** manual and automatic sizing, and make quit/close commands respect window
count the way vim does.

**Sizing (`editor.rs`, `WindowTree`):**

- `<C-w>=` / `:wincmd =`: equalize — reset every `Split`'s `sizes` to equal shares
  and re-layout.
- `<C-w>+` / `<C-w>-` (height) and `<C-w><` / `<C-w>>` (width), with counts, and
  `:res[ize] {n}` / `:vert res[ize] {n}` (absolute): adjust the relevant ancestor
  `Split`'s `sizes` entry for the current window, clamped so every sibling keeps
  ≥ 1 text row / ≥ 1 column, and re-layout. `<C-w>_` / `<C-w>|` maximize
  height/width of the current window.
- On split/close, default to **equal** sizing (Phase 3 already did this); these
  commands let the user deviate. A terminal resize (`nvim_ui_try_resize`) re-runs
  `layout` against the new total, preserving the relative `sizes` proportions.

**Quit/close semantics (extend the existing `ex_*`):**

- `:q[uit]` now means **close the current window** when more than one is open
  (like `<C-w>c`), and only **quits the editor** when it is the *last* window.
  This finally splits `:q` from `:qa` (the buffers spec noted "real windows will
  later split them"). Keep the `E37` modified-buffer guard: `:q` on the last
  window with unsaved changes still refuses; closing a *non-last* window showing a
  modified buffer is fine (the buffer stays open in the store / other windows).
- `:qa[ll]` / `:qall!` — unchanged (quit the editor; refuse on any modified
  buffer without `!`).
- `:on[ly]` already drops other windows (Phase 3); `:hid[e]` closes the current
  window without unloading its buffer.
- `<C-w>q` = `:q` (close window, or quit if last).

**Panel interaction:** the bottom panel and command line are global and dock below
*all* windows; the layout `total` already excludes the panel rows (via
`text_height()` accounting). Confirm splitting while a panel is open lays windows
out into the reduced area and the panel still spans full width.

**Tests** (extend `windows.rs`): `<C-w>+`/`<C-w>-` change the focused window's row
count (assert each `WindowView.rect.height`); `<C-w>=` re-equalizes after an
uneven resize; a terminal resize preserves proportions; `:q` with two windows
closes one and the editor stays up; `:q` on the last window with a clean buffer
quits; `:q` on the last window with a modified buffer reports `E37` and stays.

**Handoff note for Phase 5:** the full *interactive* windows feature is complete.
What remains is the **RPC/Lua `nvim_win_*` surface** so clients and plugins can
create, query, and drive windows programmatically.

---

## Phase 5 — `nvim_win_*` RPC + Lua API + window autocmds

**Goal:** the programmatic surface, mirroring neovim, so plugins (and tests, and
remote clients) manage windows the same way they manage buffers.

**RPC (`server/lib.rs`, `dispatch`):**

- `nvim_list_wins` → array of window ids (layout order).
- `nvim_get_current_win` / `nvim_set_current_win(id)` → read / `focus_window`.
- `nvim_win_get_buf(id)` / `nvim_win_set_buf(id, buf)` → the window's buffer
  binding (set re-binds without changing focus).
- `nvim_win_get_cursor(id)` / `nvim_win_set_cursor(id, [row,col])` — generalize
  the **existing** `nvim_win_get_cursor` (which reads the one window today at
  `lib.rs:489`) to take a window handle (`0` = current, as neovim does); for a
  non-focused window it reads/writes that window's `saved_cursor`.
- `nvim_win_close(id, force)` → `close()` that window (respecting the modified
  guard unless `force`).
- `nvim_win_get_width`/`get_height`/`set_width`/`set_height` → the window's rect /
  resize.
- `nvim_open_win` (split form only for now — `relative` floats are deferred with
  tabs/floats below): create a split bound to a given buffer.

**Lua (`bemtvi-lua` prelude + the queue/drain seam):** expose the above as
`vim.api.nvim_*` — window *mutations* queue like `vim.cmd` (`WindowOp`s drained
into the core each tick, the established "Lua queues, core mutates" flow), window
*reads* resolve synchronously against a snapshot. Add `vim.api.nvim_win_get_*` /
`set_*`. This is what lets `on_attach`-style plugin code (and statusline/winbar
plugins) reach window state.

**Autocmds (`server`, the autocmd lifecycle):** fire `WinNew` (on split/open),
`WinEnter`/`WinLeave` (on `focus_window`, around the focus change), `WinClosed`
(on close, with the closed window id as `<amatch>`), and `WinResized`. These hook
into the existing autocmd machinery (see the
[autocmd lifecycle design](2026-06-04-autocmd-lifecycle-design.md)); `BufEnter`/
`BufLeave` already fire on the buffer change a window switch causes — make sure
the ordering matches vim (`WinLeave` → `BufLeave` → `BufEnter` → `WinEnter`).

**Tests:** `crates/bemtvi-server/tests/windows.rs` — `nvim_list_wins` after splits;
`nvim_set_current_win` moves focus (assert focused `WindowView`); `nvim_win_get_
cursor`/`set_cursor` on a non-current window reads/writes its saved position;
`nvim_win_close` removes it; a Lua test that `vim.api.nvim_open_win` + autocmd
fires (extend `crates/bemtvi-server/tests/autocmds.rs` for `WinEnter`/`WinClosed`).

---

## Invariants & gotchas to preserve

- **Always ≥ 1 window**; `windows.current` always resolves. Closing the last
  window is a *quit*, never an empty layout. Closing any other collapses its
  parent split.
- **Window ids are monotonic and never reused** (like buffer ids). A closed id
  stays gone.
- **The current buffer is whatever the current window shows** — never store a
  buffer pointer independent of the window after Phase 1. `:b`/`:e` rebind the
  current window's buffer.
- **Active-window view state lives on `Editor`; inactive windows stash it.** Never
  duplicate the live `cursor`/`top` into the `Window` for the focused window —
  it's stashed only on focus *out* (the `enter_buffer`/`focus_window` symmetry).
- **`focus_window` must clamp the restored cursor** to its buffer's current line
  count (the buffer may have shrunk while the window was inactive — same rule as
  `switch_buffer`).
- **Syntax/LSP/diagnostics stay keyed by `BufferId`**, shared across windows onto
  the same buffer; each window only varies the projected `(top, height)` slice.
  Don't key any worker state by window.
- **Register, options, marks are global** (window-local options like `wrap`/
  `cursorline` are future work — note them in the roadmap when Phase 4 lands).
- **The command line, message line, and panel are global**, docked below all
  windows; the window layout area excludes them via `text_height()`.
- **Redraw test helpers take the *latest* queued redraw** (CLAUDE.md) — the new
  `windows()` reader must drain to the most recent frame, and the per-window
  `scroll` gesture is a one-shot transient (keep the latest redraw matching
  `scroll.is_some()`, as `scroll_after` does).
- **Each window's text height is `rect.height - 1`** (its own status line). Off-
  by-one here is the easy bug: the cursor row and `ensure_visible` must measure
  against the per-window text height, not the whole rect.

## Out of scope (explicit follow-ons, their own specs)

- **Tab pages** (`:tabnew`, `gt`/`gT`, the tabline): a tab is a *named window
  tree*; this work gives us the tree, tabs add a `Vec<WindowTree>` + a current-tab
  index + a tabline `View` region and client widget. Sibling to the deferred
  bufferline in the buffers spec.
- **Floating windows** (`nvim_open_win` with `relative`): overlay windows with a
  z-order, anchored rather than tiled. Their own spec, but this work is built to
  carry them at the seams — see [*How floats relate to this work*](#how-floats-relate-to-this-work)
  below for exactly what Phases 1–5 give them for free and what the float spec
  genuinely adds.
- **Window-local options** (`wrap`, `cursorline`, `scrolloff`, `winhighlight`):
  the window analogue of buffer-local options; both are pending.
- **`'laststatus'` modes** (0/1/3 — global statusline): we ship the `laststatus=2`
  per-window status line; the global/conditional variants are a small follow-up.

## How floats relate to this work

Floating windows (`nvim_open_win` with `relative`) are deferred to their own
spec, but they sit on a different axis from everything above, and that distinction
drives several decisions in Phases 1–5. Writing it down here so the seams are
deliberate, not accidental.

**The core distinction: floats are an overlay layer, not a tree node.** The
tiled work builds a `WindowTree` that *divides* the screen into non-overlapping
rects. A float is the opposite — positioned absolutely or *anchored* (relative to
the editor, a window, or the cursor), it *overlaps* the tiled windows and has a
**z-order**. The design call this forces:

> **A float is NOT a variant of the `WindowTree` `Node` enum.** Floats live in a
> separate collection alongside the tree — e.g. `floats: Vec<WindowId>` drawn on
> top of the tiled layout, ordered by `zindex`. Making floats tree nodes would
> fight the tree's "divide the area, no overlaps" invariant on every split,
> resize, and close.

**What Phases 1–5 give floats for free** (this is *why* floats are a follow-on and
not a rewrite — the primitives are already built):

- **The `Window` struct (Phase 1)** — buffer binding + saved cursor + rect +
  `WindowId` — *is* what a float is. A float differs only in how its rect is
  computed (anchored/absolute vs. tiled) and that it draws a border with a
  z-order. Same struct, same id space, same `nvim_list_wins` membership.
- **The multi-window `View` (Phase 2)** already carries `Vec<WindowView>` with
  **explicit per-window rects** and a `focused` flag, and the client already
  paints windows at arbitrary rects. A float is just a `WindowView` whose rect
  *overlaps* its neighbors. This is the reason Phase 2 is a list-of-rects rather
  than a baked-in tree shape: tiled windows are merely the non-overlapping case.
  Painting an overlapping window on top is a small extension (a draw-order field
  + a border box), not a protocol redesign.
- **`focus_window`, `WinEnter`/`WinLeave`, `nvim_win_close`, and the `nvim_win_*`
  getters/setters** (Phases 3 & 5) all key off a `WindowId` and work unchanged
  whether the target is tiled or floating.
- **`nvim_open_win` is the literal seam.** Phase 5 ships only its *split* form;
  the `relative=...` form is where floats plug in, with zero change to the windows
  already built.

**What the float spec genuinely adds** (the distinct surface that justifies a
separate spec rather than a sixth phase):

1. **The non-tree collection + anchored positioning** — `relative` =
   `editor`/`win`/`cursor`/`mouse`, the `row`/`col`/`anchor` (NW/NE/SW/SE) math,
   recomputed on terminal resize and (for cursor-relative floats) on cursor move.
2. **Z-order / draw order** — overlapping floats need `zindex`; the client paints
   the tiled windows first, then floats bottom-to-top.
3. **Border decoration** (`border = single`/`rounded`/…) and float-only config
   (`focusable`, `winblend`, `style = "minimal"`).
4. **Focus-model divergence** — `<C-w>hjkl` is spatial and tiled-only; floats are
   entered via `<C-w>w` cycling (focusable ones) or `nvim_set_current_win`, and
   many (hover) are **non-focusable**. `focus_window` and the directional-motion
   rule both need a float branch.
5. **Consolidating bemtvi's existing hand-rolled overlays** — the completion
   **pmenu**, the **hover doc preview**, and arguably the bottom **panel** are
   bespoke `View` fields with their own client paint paths today. The float
   primitive is the chance to rebuild those on one general overlay path (or share
   it), and it is what unblocks the UI surfaces that *require* real floats —
   fuzzy-finder pickers, key-hint popups, notifications, plugin-manager UIs.

So the tiled-windows work is the foundation floats stand on: the protocol
(Phase 2) and `nvim_open_win` (Phase 5) are intentionally left open at the seams
floats plug into, while the anchoring, z-order, borders, focus rules, and overlay
consolidation are enough distinct surface to be their own spec.

## Roadmap doc update

When Phases 1–5 land, update `docs/architecture.md`: move "Multiple **windows**,
tabs, and splits; the window layout tree" out of *Not yet implemented*; expand the
*Buffers* section (or add a *Windows* sibling) describing the window tree, per-
window view state, and the `nvim_win_*` surface; update the *View protocol*
section to describe the `windows` list + separators + per-window status lines; and
note tabs, floats, and window-local options as the remaining gaps.
