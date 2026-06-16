# Multi-cursor — design

**Status:** implemented. Scope is **single-buffer multi-editing** with a dedicated
placement mode: drop N cursors, then have motions, operators, **visual mode**, and
all the insert-entry / open-line / paste keys act on every cursor at once. This is
a Helix/kakoune/Sublime-style multi-cursor built on top of nxvim's existing vim
grammar, not a selection-first rewrite.

The editing surface is now essentially complete: motions, the `d`/`y`/`c`/`=`
operators, text objects, `x`/`X`/`D`/`C`/`s`/`J`/`~`/`r`, insert-mode typing +
`Enter`/`Backspace`, `a`/`A`/`i`/`I`, `o`/`O`, `p`/`P` with **per-cursor
registers**, and a full **per-cursor visual mode** (including visual `o`). The one
deliberate non-goal that remains is the `self.cursor` → cursor-set refactor, which
is **explicitly not planned** — see *Non-goals* for why the replay model makes it
unnecessary.

The load-bearing claim, validated: **nxvim already had the hard part.** Keeping N
cursor positions correct as an edit at one of them shifts the bytes under the
others is the classic multi-cursor nightmare — and it is exactly what the
[extmark layer](2026-06-07-extmark-decoration-layer-design.md) already solves.
Secondary cursors are stored as extmarks, so the buffer's single edit choke point
shifts them all for free.

## The two-phase model

A real terminal has one cursor, and vim has one `self.cursor`. Rather than fight
that everywhere at once, multi-cursor is split into two phases with a mode
boundary between them:

1. **Placement** (`Mode::MultiCursor`) — entered with `<A-c>`. Motions move only
   the **active** (primary) cursor; you navigate — including `/`-search — and drop
   *secondary* cursors at points of interest. The active cursor is recolored so
   you can see you're placing.

2. **Editing** (back in `Mode::Normal`) — `<Esc>` leaves placement; the dropped
   cursors persist. Now motions and operators (`w`, `dw`, `x`, `cw`, typing, …)
   apply at **every** cursor at once. A second `<Esc>` collapses back to one.

On leaving, the primary becomes an ordinary edit cursor, so it must land *on* a
placed cursor — otherwise a spot the primary merely **navigated** to (motions move
only the primary while placing) would silently become a phantom edit cursor.
`finish_multicursor` therefore snaps the primary onto the nearest placed cursor
(ties → topmost) when it sits off every one, *then* dedups the mark under it. The
final set is exactly the cursors you dropped — no extra one where navigation
happened to stop.

This keeps the two hard problems apart. Placement is pure navigation + a "remember
this spot" gesture (no propagation machinery). Editing is "replay this command at
each cursor" (no anchor/selection-state machinery). Neither phase has to solve
the other's problem.

```
Normal ──<A-c>──▶ MultiCursor ──<Esc>──▶ Normal (cursors live) ──<Esc>──▶ Normal
                  │  motions move primary           │  motions/edits → all cursors
                  │  c / {n}c{motion} drop cursors  │
                  └─ /search keeps cursors          └─ /search or n clears them
```

## Why cursors are extmarks

A secondary cursor is a **point extmark** in a reserved namespace,
[`extmark::CURSOR_NS`](../../crates/nxvim-core/src/extmark.rs) (`u32::MAX`, far
above any `nvim_create_namespace` id). It carries no `hl_group`/`end`, so it
renders nothing through the highlight layer and is filtered out of the
user-facing `nvim_buf_get_extmarks` mirror.

Storing them this way buys, for free:

- **Edit-shift.** Every insert/remove funnels through
  [`Buffer::record`](../../crates/nxvim-core/src/buffer.rs), which calls
  `ExtmarkStore::shift`. So an edit at any cursor keeps all the others' anchors
  correct, across every edit path, with no bespoke fix-up code.
- **Undo/redo carry.** Extmarks ride the per-node undo snapshot, so cursors come
  back on undo (with one wrinkle the undo section resolves).

Per-cursor **visual mode** adds a second reserved namespace,
[`extmark::ANCHOR_NS`](../../crates/nxvim-core/src/extmark.rs) (`u32::MAX - 1`):
each secondary cursor's visual anchor is a point extmark there, paired to its
`CURSOR_NS` head **by the same id**, so the two are looked up and shifted together.
Like cursors, anchors ride the edit choke point and render nothing through the
highlight layer.

The **primary** cursor stays as `self.cursor` — the existing single-cursor field
that ~197 call sites read — with its visual anchor in `self.visual_anchor`. We
deliberately did *not* refactor that away; secondary cursors are an additive layer
(see *Non-goals* for the standing decision against unifying them).

## Mode

[`Mode::MultiCursor`](../../crates/nxvim-core/src/mode.rs) is a placement mode.
Its `label()` is `"MULTICURSOR"` (drives the status line and the client's
`View::is_multicursor()`); its `short_code()` is `"n"`, so `mode()`-checking
scripts read it as normal mode (there is no vim equivalent to expose). It is not
visual and not insert, so the input dispatch routes it through `handle_normal` —
it reuses the whole normal grammar, with the differences below.

### Mode-specific keymaps

Placement mode has its **own** keymap bucket so a map can fire *only while
placing*. In [`keymap.rs`](../../crates/nxvim-server/src/keymap.rs),
`mode_key(MultiCursor)` returns `'m'` (diverging from `short_code`'s `"n"`), so
the matcher selects a dedicated `'m'` trie rather than the normal one. A user
declares one with the `'m'` mode code:

```lua
vim.keymap.set('m', '<Tab>', function() ... end)  -- only in MULTICURSOR
```

The mapping is **isolated**, matching vim's mode separation:

- A plain `'n'` (normal) map does **not** fire while placing, and an `'m'` map
  does **not** fire in normal mode. `mode_buckets("m")` → `['m']` and `"n"` →
  `['n']` keep the two tries disjoint.
- The all-mode `''` (vim's `:map`) still covers placement —
  `mode_buckets("")` → `['n', 'v', 'V', 'm']` — since `''` means *every*
  normal-ish mode.
- The built-in placement grammar is untouched: whenever the `'m'` trie has no
  match for a key it passes straight through to `handle_normal`, so `h`/`j`/`c`/
  `{count}c{motion}`/`<Esc>` work exactly as before. (Placement maps shadow only
  the keys they actually bind.)

This is the editing-phase counterpart's natural symmetry: the **editing** phase
is plain `Mode::Normal`, so normal maps already apply there and propagate through
the replay sweep; the **placement** phase now gets its own addressable mode.

A runnable playground lives in
[`examples/multicursor`](../../examples/multicursor) (`NXVIM_CONFIG=examples/multicursor
cargo run -p nxvim -- examples/multicursor/sample.txt`): the same key mapped in
both `'n'` and `'m'`, a placement string-RHS map (`<Tab>` → `wc`), an `''`
all-mode map, and a normal-only map shown inert while placing.

## Grammar

| Keys | In `MultiCursor` |
|---|---|
| `<A-c>` / `<M-c>` | Enter placement mode + drop a cursor at the active position |
| motions (`h`/`j`/`w`/`/`…/`n`) | Move **only** the active cursor |
| `c` | **Toggle** a cursor at the active cell — drop if empty, clear if occupied |
| `c{motion}` | Move by `{motion}` and drop a cursor there (`cj` = one line down) |
| `{count}c{motion}` | Count = motion *distance* — `3cj` drops cursors on relative lines 0–3 (4 cursors), matching where `3j` lands |
| `cc` (`{count}cc`) | Drop one cursor per line over `count` lines |
| `<Esc>` | Finish placement → Normal (cursors persist); cancels a half-typed `{n}c…` first |

The spawn key is **Alt+c**, not Helix's bare `C` (taken by vim's change-to-EOL).
On macOS the terminal must send Option as Meta (Terminal.app: *Use Option as Meta
key*; iTerm2: *Left Option = Esc+*) for `<A-c>` to arrive.

`c`'s dual nature falls out of the vim operator grammar cleanly: a bare `c` (no
count, no operator) resolves to `NormalCmd::PlaceCursor` and toggles immediately;
a counted/`c{motion}` enters the operator-pending path and places along the
motion. `cc` is the doubled-operator (linewise) form. All three funnel through
`place_cursor_here`, so the toggle is consistent everywhere a cursor is dropped.

## Propagation: replay, don't restructure

The editing phase replays an ordinary single-cursor command at each cursor. Two
primitives in
[`editor/multicursor.rs`](../../crates/nxvim-core/src/editor/multicursor.rs):

- **`for_each_cursor(f)`** — runs `f` once at the primary and once per secondary,
  parking `self.cursor` at each in turn so the normal effect helpers operate
  unchanged. It parks the primary as a temporary extmark too, so *all* cursors
  shift uniformly through the edit choke point, and visits them
  **highest-byte-first** (an edit at one cursor only shifts anchors at or after
  it, so a not-yet-visited lower cursor stays valid). Undo-neutral.
- **`edit_each_cursor(f)`** — `for_each_cursor` wrapped in **one** undo group;
  keeps the insert-session snapshot if `f` enters insert (`cw`/`s`).

Both short-circuit to plain `f(self)` when `cursors_active()` is false —
`has_secondary_cursors() && mode != MultiCursor` — so placement mode and the
common no-cursor case pay nothing.

Crucially, a **motion is re-resolved at each cursor**: `w` lands at a different
byte for each. So the Motion / TextObject / doubled-operator arms in
[`command.rs`](../../crates/nxvim-core/src/editor/command.rs) call per-cursor
helpers (`apply_motion_once`, `apply_text_object_once`,
`apply_doubled_operator_once`) that re-resolve and apply without resetting the
pending state until the sweep is done. Wired:

- motions, `dw`/`yw`/`cw`/`=w`, `dd`/`yy`/`cc`, `diw`/`ci"`,
  `x`/`X`/`D`/`C`/`s`/`J`/`~`/`r`;
- insert-mode typing **and** `Enter`/`Backspace` (each runs through
  `for_each_cursor`; the insert session already holds the snapshot, so no
  `edit_each_cursor` wrap);
- the insert-entry keys `a`/`A`/`i`/`I` (via `enter_insert_each`, which moves every
  cursor to its own target column — line-end for `A`, first-non-blank for `I` — and
  enters insert *before* the sweep so the EOL append column survives the clamp);
- `o`/`O` (`edit_each_cursor(|ed| ed.open_line(below))` — `open_line`'s `push_undo`
  is snapshot-gated, so it coalesces under the one group);
- `p`/`P` with **per-cursor registers** (see *Per-cursor yank and paste*);
- visual-mode motions and operators (see *Per-cursor visual mode*).

The propagation guard is `mode == Mode::Normal || mode.is_visual()` (so editing
propagates in both Normal and visual; only placement mode is single-cursor).

After every sweep, `merge_overlapping_cursors` collapses cursors that converged
onto the same cell (e.g. `0` or `gg`), including a secondary that lands on the
primary — so a motion can't leave a pile of coincident marks each acting on the
same spot.

## Undo restores pre-edit positions

Cursors must come back on undo **where they were before the undone edit** — not
where the edit shifted them. Two subtleties make the naïve approach wrong:

1. The node you undo *to* is often the original, snapshotted **before** any cursor
   was placed — its frozen extmark set is empty, so a plain restore wipes the
   cursors.
2. That node's `snap.cursor` (the primary) is stale for the same reason.

Fix, in [`undo.rs`](../../crates/nxvim-core/src/editor/undo.rs): at edit-start,
`push_undo` → `refresh_undo_cursor_marks` **bakes the live cursor positions**
(primary `snap.cursor` + the `CURSOR_NS` marks) into the snapshot of the node
we'll undo back to (`UndoTree::set_cur_snapshot_cursors`). A normal restore then
brings every cursor back to its pre-edit spot. This is gated on
`has_secondary_cursors`, so single-cursor undo — and its existing
cursor-placement semantics — is completely untouched. (An earlier approach that
preserved the *live/post-edit* positions across the restore was wrong and was
replaced.)

## Per-cursor visual mode

Each secondary cursor carries its **own** selection. Entering visual (`v`/`V`)
over a placed set calls `begin_visual_anchors`, which drops an `ANCHOR_NS` mark at
each `CURSOR_NS` head (a 1-wide selection per cursor, like vim's `v`); the primary
keeps its anchor in `self.visual_anchor`. Visual motions then propagate through the
same `for_each_cursor` (the guard is widened to include `mode.is_visual()`), moving
each head while its anchor stays put — so every selection extends independently.

`for_each_cursor` is **anchor-aware**: when the mode is visual it parks the
primary's anchor as a mark too, and restores `self.visual_anchor` from each
cursor's paired anchor before running `f`, so an operator brackets *that* cursor's
selection. (The visual flag is captured up front, because an editing `f` like
visual `c` flips the mode to Insert mid-sweep.)

Operators on a multi-cursor selection route through
[`operators.rs`](../../crates/nxvim-core/src/editor/operators.rs):
`visual_operate` → `visual_operate_multi` → `edit_each_cursor(|e|
e.visual_operate_once(op, linewise))`, one undo group, each cursor bracketing its
own `anchor..head` range.

**Visual `o`/`O`** moves the cursor to the other end of the selection
(`visual_swap_ends`): the primary swaps `cursor`/`visual_anchor`, and each
secondary swaps its `CURSOR_NS` head with its paired `ANCHOR_NS` anchor. The span
is unchanged — only which end is movable. (Without this, `o` fell through the
normal grammar to `OpenBelow`; there is no visual-block mode yet, so `O` aliases
`o`.)

`<Esc>` in visual collapses the selections (`clear_anchor_marks`) but **keeps the
cursor heads** — a second `<Esc>` in Normal collapses those.
`clear_secondary_cursors` and `merge_overlapping_cursors` drop paired anchors
alongside their heads, so no orphan anchor lingers.

## Per-cursor yank and paste

A multi-cursor `yy`/`yw`/`diw`/… captures **each cursor's own text**, not just the
last (the old "unnamed register, last write wins"). `edit_each_cursor` opens a
collector around the sweep; `yank_range`/`delete_yank_range` push each slice
(`collect_cursor_register(range_start, …)`); on sweep end the slices are sorted by
position into `Editor::cursor_registers` (ascending document order, so entry *i*
belongs to the *i*-th cursor by position). The unnamed register is still written
normally, so single-cursor paste and `:reg` are unaffected.

`p`/`P` with a cursor set (`paste_multi`): when `cursor_registers.len()` still
matches the live cursor count, each cursor pastes its **own** captured text (the
positions are paired to the registers ascending; the sweep visits highest-byte
first, so a paste never shifts a lower not-yet-visited cursor). When the counts
*don't* match — a single-source yank, or the set changed — every cursor pastes the
active register instead (vim's plain `p`, broadcast). `paste` was split into
`paste` / `paste_text(text, linewise, count, after)` so both paths share the body;
the whole multi-paste undoes as one step.

## Search and `n` clear the cursors

A committed search — `/`, `?`, `n`, `N`, `*`, `#` — in **Normal** mode is treated
as navigating away, which abandons the multi-cursor session and collapses to the
primary. In **placement** mode the same search instead *navigates to* a match so
you can drop a cursor there, so cursors are kept. One guard at the top of
`run_search` ([`search.rs`](../../crates/nxvim-core/src/editor/search.rs)):

```rust
if self.mode != Mode::MultiCursor {
    self.clear_secondary_cursors();
}
```

`run_search` is the commit-only path; the incsearch *preview* uses a separate
side-effect-free core, so mid-typing never triggers the clear.

## Rendering

A terminal has one real cursor (the primary, placed via `set_cursor_position`),
so the extras are painted as cells.

- The server projects each focused-window secondary cursor into the redraw as a
  `cursors` array of `[row, screen_col]` pairs
  ([`view.rs`](../../crates/nxvim-core/src/view.rs) →
  [`redraw.rs`](../../crates/nxvim-server/src/redraw.rs)); the client parses it
  into `WindowView.secondary_cursors`
  ([`nxvim-view`](../../crates/nxvim-view/src/view.rs)).
- [`render_secondary_cursors`](../../crates/nxvim-tui/src/render.rs) paints each
  placed cursor as a **reverse-video** cell — and tracks the mode-driven cursor
  *shape*: insert/replace → underline (a bar can't be drawn in one cell),
  everything else → reverse-video, so a mode change propagates to every cursor.
- In placement mode the **active** cursor cell is recolored amber
  (`MULTICURSOR_PRIMARY_BG`), distinct from the placed ones, signaling "dropping
  cursors."
- In visual mode each secondary cursor's **selection** is projected too:
  `WindowView.secondary_selection` (a per-row multi-span list, mirroring `search`)
  built by `secondary_selection_spans`, sent as `secondary_selection` in the
  redraw, parsed in nxvim-view, and painted with the same `Visual` style as the
  primary's selection. The primary's selection stays in `selection`.
- Both are skipped mid-scroll-animation (like search/diagnostics), where
  interpolated positions wouldn't line up.

## Key files

- [`crates/nxvim-core/src/editor/multicursor.rs`](../../crates/nxvim-core/src/editor/multicursor.rs)
  — `add_cursor`/`place_cursor_here`/`finish_multicursor`,
  `for_each_cursor`/`edit_each_cursor` (incl. the per-cursor register collector),
  `merge_overlapping_cursors`, `clear_secondary_cursors`, and the visual helpers
  `begin_visual_anchors`/`clear_anchor_marks`/`visual_swap_ends`/
  `secondary_selections`.
- [`command.rs`](../../crates/nxvim-core/src/editor/command.rs) — the grammar (the
  `<A-c>` and `c` arms, the per-cursor replay helpers, propagation guards, the
  visual-mode `o`/`O`→`VisualSwapEnds` routing, the `<Esc>` finish-vs-collapse
  logic).
- [`insert.rs`](../../crates/nxvim-core/src/editor/insert.rs) —
  `enter_insert_each` (per-cursor `a`/`A`/`i`/`I`), `insert_newline` (per-cursor
  `Enter`), the per-cursor `<Esc>` backstep.
- [`operators.rs`](../../crates/nxvim-core/src/editor/operators.rs) —
  `visual_operate_multi`/`visual_operate_once`, `paste_multi`/`paste_text`, and
  the `collect_cursor_register` hook in `yank_range`/`delete_yank_range`.
- [`mode.rs`](../../crates/nxvim-core/src/mode.rs) — `Mode::MultiCursor`.
- [`cmdline.rs`](../../crates/nxvim-core/src/editor/cmdline.rs) —
  `cmdline_return_mode`, so `/`-search and `:` return to placement mode.
- [`undo.rs`](../../crates/nxvim-core/src/editor/undo.rs) — pre-edit cursor baking.
- [`extmark.rs`](../../crates/nxvim-core/src/extmark.rs) — `CURSOR_NS` (heads),
  `ANCHOR_NS` (per-cursor visual anchors).
- Rendering: [`view.rs`](../../crates/nxvim-core/src/view.rs),
  [`redraw.rs`](../../crates/nxvim-server/src/redraw.rs),
  [`nxvim-view`](../../crates/nxvim-view/src/view.rs),
  [`render.rs`](../../crates/nxvim-tui/src/render.rs).

## Testing

Black-box, per the project convention.
[`tests/editing/multicursor.rs`](../../crates/nxvim-server/tests/editing/multicursor.rs)
drives `nvim_input` and asserts on `nvim_buf_get_lines` and the redraw `cursors`
array. Coverage includes:

- **Placement:** mode label, navigate-only-while-placing, `c`/`2cj`/`2cw`
  placement (count inclusive of the start), `c`-toggle, overlap-merge,
  search-and-place (placement) vs search-clears (normal), placement undo/redo.
- **Editing:** `<Esc>` keeps-then-edits, double-`<Esc>` collapse, one-step undo,
  undo-restores-pre-edit-position, and the `<Esc>`-backsteps-every-cursor fix.
- **Per-cursor keys:** `A`/`a`/`I` insert-entry positioning, `o`/`O` open-line,
  insert `Enter`/`Backspace`, per-cursor `yy`/`yiw` + `p`, the paste broadcast
  fallback, and paste-undoes-as-one.
- **Visual:** charwise/linewise/`c` operators per cursor, motion-extends-render,
  `<Esc>`-keeps-cursors, and `o` swaps ends at every cursor.

Single-cursor visual `o` is pinned in
[`tests/editing/core_editing.rs`](../../crates/nxvim-server/tests/editing/core_editing.rs).
Rendering is pinned in
[`nxvim-tui/tests/paint.rs`](../../crates/nxvim-tui/tests/paint.rs): placed-cursor
reverse-video, shape-follows-mode, and the active-cursor recolor.

## Done since the first slice

Everything originally deferred for the editing surface has since landed: per-cursor
**visual mode** (incl. visual `o`), insert `Enter`/`Backspace`, the insert-entry
keys `a`/`A`/`i`/`I`, `o`/`O`, and `p`/`P` with **per-cursor registers** (replacing
last-write-wins). See the sections above.

## Non-goals

- **The `self.cursor` → cursor-set refactor — explicitly not planned.** The idea
  would be to replace "one primary `self.cursor` + secondary extmarks driven by the
  replay sweep" with a first-class `CursorSet` operated on directly. We are not
  doing it: the replay model (`for_each_cursor`/`edit_each_cursor`) now covers the
  whole editing surface, and each feature was only a few lines of routing — the
  per-feature wiring cost is small. The load-bearing hard part (positions correct
  across edits) is already free via extmarks; a `CursorSet` wouldn't improve it.
  And most of the ~197 `self.cursor` sites genuinely *want* to be singular: the
  viewport/scroll follows one cursor, the terminal/GUI has one real cursor, plus
  `curswant`, the jumplist, and the `^`/`` `[ ``/`` `] `` marks. No feature is
  blocked by its absence — textbook YAGNI. **Revisit only if** the sweep's stateful
  special-cases keep multiplying (today: highest-byte-first visit order, the
  register-collector side-channel, merge/clamp-after-sweep), or you want genuinely
  per-cursor *singular* state — e.g. each cursor's own `curswant` across `j`/`k`, or
  per-cursor search — which is awkward in the replay model.
- **Cross-buffer / multi-window cursors.** Cursors are per-buffer and shown only in
  the focused window.
- **Visual-block corners.** With no visual-block mode, `O` aliases `o` rather than
  jumping to the opposite *corner*.
