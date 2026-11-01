# Text a plugin puts in the gutter

**The gap.** A plugin can put one or two cells in the sign column (`sign_text`), and the
editor can number lines, but nothing lets a plugin give each line a column of its own
text. SourceGit's diff (`sourcegit/mitsuami/codeview`, plugin `sourcegit-diff`) needs two
numbers per line, old and new, because a diff's lines aren't the file's. Blame needs a
hash and an author, and a side-by-side view needs each side's own numbers.

Today the diff fakes it with inline virtual text at column 0. Inline virtual text is part
of the line: it "takes its cells and pushes the rest of the line right"
(`bemtvi-gpu/src/render.rs`). So the numbers live in the text area, where they don't
belong, however well it looks at first.

**The feature.** An extmark key, `gutter_text`, that fills a column between the line
numbers and the text. No Lua runs at frame time: like every other decoration, a plugin
publishes marks and the server folds them into the next frame. That rules out neovim's
`'statuscolumn'`, an expression evaluated per row while drawing (the model's rule 4).

```lua
view:set_decor(ns, {
  {
    line = 4,
    col = 0,
    gutter_text = { { " 12", "DiffDelete" }, { "    ", "LineNr" } },
    sign_text = "-",
  },
})
```

The window then reads: fold column, signs, line numbers, gutter text, text.

Surveyed 2026-10-31 at `ef9a4df8`.

## How signs travel today

`gutter_text` follows the same path, so this is the list of places to touch.

- **Lua to core.** `sign_text` is a field of the extmark options
  (`bemtvi-lua/src/ops.rs`) and of `Extmark` (`bemtvi-core/src/extmark.rs`).
- **Projection.** `extmark_sign_cells` (`bemtvi-server/src/extmarks.rs`) walks only the
  marks anchored in the viewport and buckets them by line, then gives one cell per screen
  row (`RowSeg`), which is what makes a wrapped line's continuation rows and a fold come
  out right. `redraw.rs` sends the cells as `diagnostics_signs` and the column's width as
  `sign_width`.
- **Width.** `sign_width` appears in 14 files. The ones that do arithmetic with it are the
  core's cursor and window geometry (`editor/windows.rs` 12 sites, `editor/cursor.rs`,
  `view.rs`), `bemtvi-view/src/hit_test.rs`, and the three renderers.
- **Clients.** The GUI (`bemtvi-gpu/src/render.rs`, 5 sites), the terminal
  (`bemtvi-tui/src/render.rs`) and the browser (`bemtvi-edithost/web/index.html`, 12
  sites). The wire type is `WindowView` (`bemtvi-view/src/view.rs`).
- **Tests.** `t:decor()` reads the same layer (`prelude/test.lua`, the mirror in
  `bemtvi-lua/src/runtime.rs` and `redraw.rs`).

## Decisions

- **An extmark key, not a new API.** It reuses extmarks' namespaces, gravity and
  `set_decor`, and it works over a daemon or in the browser with nothing new, since
  projection is core-shared on both builds. It moves with its line as text is edited.
- **The column's width comes from the whole buffer, not the viewport.** Sign width is
  computed from the visible rows, which is fine for 0 or 2 cells. Text of varying width
  would shift the text left and right while scrolling past a long hash. So the width is
  the widest `gutter_text` in the buffer, which the core keeps up to date as marks change:
  a scan of all marks on each frame would make redraw cost scale with the buffer, which
  the project rule against freezing forbids. A set of per-width counts on the buffer does
  it in O(1) per mark change.
- **Only the first row of a wrapped line shows it,** as line numbers do. Continuation rows
  and rows of virtual lines leave the column blank. A closed fold shows its first line's.
- **Several marks on a line:** the highest `priority` wins, then the higher mark id (within
  one namespace, the one placed later unless an explicit `id` is reused), as signs do. They don't concatenate, because a column can't line up if two plugins each add
  to it. Two plugins wanting a column each is a later question (below).
- **Display cells, measured as the rest of the editor measures them** (`bemtvi-width`), so
  a wide name in a blame column still lines up. The client pads a shorter line's text on
  the right. A plugin that wants numbers right-aligned pads them itself, as the example above
  does: it knows what they mean.
- **The cursor line and `winhighlight`:** a chunk's highlight group is honoured as given,
  through `winhl` as signs are. A chunk with no group uses `LineNr`, or `CursorLineNr` on
  the cursor line. `line_hl_group` tints the column too, as vim's `'culopt'` number does
  not: a diff's tint should run under its numbers. To confirm by eye in Phase 3.
- **Not clickable yet.** A click there behaves like a click on a line number, and the
  pointer stays the arrow (`hit_test.rs` counts the column as gutter). A click action is
  a separate feature that blame will want.

## Phases

Each phase ends with the suite green and a commit, and pauses for review.

### Phase 0: confirm what the fake costs

Write the failing tests the feature should pass, against inline virtual text, so the plan's
claim rests on behaviour. A probe on 2026-10-31 was inconclusive: `btv.win.set_leftcol` on
a view with a long line left `t:screen()` and `t:decor()` unchanged, so either the setter
or the test views don't show a horizontal scroll, and that needs an answer first (a bug in
its own right if `set_leftcol` does nothing on a view). Check:

- the numbers stay on screen as a long line scrolls sideways;
- `$`, `0` and mouse clicks land on the right column of the text;
- wrapping (`'wrap'` on) doesn't count the numbers' cells on the line.

If all three already hold, say so here and reconsider how much of this feature is needed.

**Result (2026-10-31).** None of the three holds. Probed with a throwaway `btv.test`
spec: every line carries an 8-cell inline mark `" 12  13 "` at column 0, in a window
with a 4-cell number gutter, and each figure is checked against the same buffer with no
marks.

- **Sideways scroll drops the numbers.** `$` on a 300-cell line under `nowrap` scrolls
  to `leftcol = 224`. Both the GUI (`virt_inline_shift`) and the terminal
  (`render.rs` around `inline` placements) drop an inline placement whose column is left
  of `leftcol` (`p.col >= leftcol`), so the numbers vanish as soon as the view scrolls.
- **Clicks land 8 cells right.** A click on the painted `w` of `hello world` (screen
  column 14) puts the cursor on byte 10, the `d`. With no marks a click resolves
  `col - 4`, so the hit-test ignores the inline cells. The core's cursor screen column
  ignores them too (`btv.screen.col()` reads 5 after `0` with marks or without them).
  The GUI shifts the drawn caret by the inline width itself, so the caret looks right
  while the core's arithmetic is wrong.
- **Wrapping doesn't count the numbers, so text falls off the edge.** A line of 76
  cells in a 76-cell text area stays one row, though it paints as 84. The last 8 cells
  go past the right edge.

On `set_leftcol`: it isn't broken. `t:screen()` carries each row's whole text and the
client does the clipping, so a sideways scroll shows only in `t:view().leftcol`. The
first probe's `set_leftcol(0, 20)` was undone because the cursor sat at column 0, which
the scroll would have hidden. With the cursor at column 150, `set_leftcol(0, 120)` holds.

### Phase 1: core and projection

- `gutter_text` on the extmark options and `Extmark`, validated loudly (a chunk that isn't
  `{ text, group? }` raises, naming the key). An empty list, a newline, a tab or any
  other control character in it raises too.
- The per-buffer width counts, updated wherever marks are added, moved off or cleared,
  undo included.
- `extmark_gutter_cells` beside `extmark_sign_cells`, and `gutter_text` and
  `gutter_text_width` on the window's frame and `WindowView`, in the band payload too, so a
  smooth scroll carries them.
- Every width sum in the core and `hit_test.rs` includes the new column.
- `t:decor()` gains a `gutter` field.

Tests, black-box through the server (`bemtvi-server/tests/gutter_text.rs`): the frame's
cells for a plain line, a wrapped line, a closed fold and a virtual line; priority; width
following the widest mark as it's added and cleared, including one off screen; marks
following edits and undo; the cursor's screen column and mouse hits with the column on;
an invalid mark's error. A many-marks test that shows the frame's cost doesn't grow with
marks outside the viewport.

**Done.** One change from the above: the frame's cost does grow with marks outside the
viewport, by the same walk every extmark layer already makes. The store keys marks by
id, not position (see `ExtmarkStore::shift` for why), so finding the visible ones visits
them all. Measured at 20 000 marks, a frame costs ~3.5x a bare buffer whether the marks
are signs, highlights or gutter text. What the column adds is O(1): its width comes from
the per-width counts, and a buffer with no `gutter_text` skips the projection. So the
test pins the column against the sign layer at the same mark count instead. A position
index for every layer would be its own piece of work: it is now
[its own plan](2026-10-31-extmark-position-index.md).

The wire sends each row's chunks unpadded, with a group-less chunk's style as `nil`; the
client pads and colours it (Phase 2), since it already knows `LineNr`, `CursorLineNr`
and the cursor line.

### Phase 2: the three clients

The GUI, the terminal and the browser each paint the column, with its smooth-scroll band
where they have one. A shared fit helper in `bemtvi-view/src/fit.rs` beside `gutter_cell`
keeps the padding in one place, as the mirror comment on `push_gutter` asks. The browser
gets a `verify-gutter-text.mjs` beside `verify-view-sign.mjs`.

**Done.** The browser's `index.html` can't call the Rust helper, so it carries a twin,
`fitGutterText`, measured with the same width tables (`clusterWidth`) and named in the
helper's doc comment so the two are found together.

### Phase 3: example and docs

`examples/gutter-text/`: an `init.lua` with type-this / see-that notes and its own
`test/*_spec.lua`, showing old and new numbers over a small diff, and a blame column. A page
in `docs/features/`, and the docstrings in `prelude/` that the book generates from, as valid
markdown. Check the look by eye in the GUI and the terminal.

**Done**, except the GUI by eye: a `bemtvi-gui` window started from the agent's shell
doesn't show up in a screen capture, so the GUI is covered by its tests and the
terminal and browser runs only. The terminal was checked in tmux: the diff numbers,
the column staying put through a sideways scroll, and the blame column with a CJK
author all line up. `btv.decor.expr` doesn't take `gutter_text`; the feature page says
so.

### Phase 4: use it

Outside this repository: `sourcegit-diff` swaps its inline virtual text for `gutter_text`,
and its spec reads `t:decor(row).gutter`. Then blame and side-by-side can build on it.

## Questions for review

- **The name.** `gutter_text` says where it goes, and sits well beside `sign_text`.
- **One column or several.** This plan has one per window, so one plugin owns it. Old and
  new numbers are one column to it, built from two chunks. If blame and a diff ever need to
  show at once, marks could carry a `gutter_column` name. Nothing here prevents adding that,
  and nothing needs it yet.
- **Where a sign goes.** Signs stay to the left of the numbers, as they are today, so a
  diff reads `- 12 13 text`. Upstream SourceGit shows old and new numbers with no sign, and
  the tint says the rest, so `sourcegit-diff` may drop its sign anyway.
