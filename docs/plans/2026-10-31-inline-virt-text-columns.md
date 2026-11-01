# Inline virtual text takes columns everywhere it shows

**The gap.** Inline virtual text (`virt_text_pos = "inline"`) "takes its cells and pushes
the rest of the line right" when a client draws it (`crates/bemtvi-gpu/src/render.rs:1431`).
Nothing else counts those cells. The click is one consequence, written up in
`BUG-inline-virt-text-click.md` beside this plan. This plan covers two more, found while
building gutter text on the `gutter-text` branch (that branch's
`docs/plans/2026-10-31-gutter-text.md`, Phase 0, probed with 8 inline cells at column 0):

1. **A sideways scroll drops it.** Under `'nowrap'`, once `leftcol` passes a placement's
   anchor, every client stops drawing it, and the text after it jumps left by its width.
2. **Wrapping doesn't count it.** A line that fits the text area by its own cells stays one
   row, though it paints wider. The cells past the right edge are lost.

Read at `258a32e4`, the commit this branch sits on.

## How it goes wrong today

- **The scroll.** Each client skips a placement whose anchor column is left of `leftcol`:
  - GUI: `virt_inline_shift` keeps only `p.col >= leftcol`
    (`crates/bemtvi-gpu/src/render.rs:5217`), and so does the splice that draws it (`:5396`).
  - Terminal: `emit_inline_virt` returns early on `placement.col < leftcol`
    (`crates/bemtvi-tui/src/render.rs:2267`), and `virt_cursor_shift` filters the same way
    (`:2314`).
  - Browser: the row walk skips every cell left of `leftcol` (`web/index.html:2178`) before it
    emits the placements anchored on that cell, so they go with it.

  So the placement is all or nothing. While its anchor is on screen it pushes the text right;
  one column later it is gone and the text snaps back by its width.
- **The wrap.** The core cuts soft-wrap segments by the line's own cells
  (`unicode::window_wrap_segments`, called from the view projection in `view.rs`), and the
  server clips every overlay to those segments (`RowSeg`, `redraw.rs`). Inline text is spliced
  into its row by the client after the segments are fixed, so the row overflows by its width.
- **The core's scrolling.** `$`, the cursor-follow scroll and `window_max_leftcol`
  (`editor/mouse.rs`) measure the line without the inline cells too. On a line that only
  overflows because of them, `$` can leave the cursor drawn past the right edge.

## What neovim does

Inline virtual text is part of the line's screen columns. `charsize_regular` counts it in a
line's virtual columns (`vendor/neovim/src/nvim/plines.c:75`, and `:496` for the cursor
offset), so wrapping and the cursor both see it. When `leftcol` falls inside a placement,
`drawline.c` skips that many of its cells, as it would text (`drawline.c:973`–`991`), and the
rest of the placement shows. Scrolling past the whole placement hides it and the text after
it stays where those cells put it.

That is the model to match: one screen-column space per line, text cells and inline cells
together, used by drawing, wrapping, scrolling and clicking alike.

## The design

### One list of splices per line

A line's inline cells are a list of `(anchor text column, width)` splices, from its inline
`virt_text` marks in the order projection already uses (`(start byte, priority, id)`,
`virt_text_for` in `crates/bemtvi-server/src/extmarks.rs`). The core can build it: the marks
are core extmarks. One function produces it, and every consumer below calls that function,
so drawing and arithmetic can't disagree.

The click fix needs the same list (its *Fix* section), and so does this plan. Build it once,
as the first step, and have the click fix use it too.

**Inlay hints** are spliced the same way but live in the server, outside the core. The click
plan already raises this. One answer for both: the server hands the core each visible line's
hint splices when it projects a frame, the way it already pushes the rendered sign width back
(`Editor::set_window_sign_width`), and the core merges them into the list. Decide this with the
click fix, not twice.

**Cost.** Building a line's list means finding the inline marks anchored on it. On `main` at
`258a32e4` that is a walk over every mark in the buffer. The `gutter-text` branch adds a
position index to the extmark store (`ExtmarkStore::starting_in`, its
`docs/plans/2026-10-31-extmark-position-index.md`) that answers it in O(log marks + marks on
the line). Rebase onto it first if it has landed; otherwise this must not ship a per-line
walk, since wrapping asks for every visible line on every frame.

### Wrapping

`window_wrap_segments` takes the line's splices and counts their widths when it decides where
a row ends. Segments stay in **text** columns, as the overlay projections expect: a row that
holds a splice just covers fewer text columns. That leaves `RowSeg` and every server overlay
unchanged, and the client keeps splicing the placement into the row that holds its anchor.

A placement wider than what's left of its row moves to the next row whole, with its anchor
character, unless it starts the row, in which case it is clipped there. Neovim wraps the
virtual text itself across rows instead. Check `vendor/neovim` and decide which; moving it
whole is simpler and never splits a label.

### Scrolling

`leftcol` becomes a column in the line's full screen-column space, text and splices
together, as neovim's `w_leftcol` is. Each client then clips a placement instead of skipping
it: with `leftcol` inside a placement, its first `leftcol - anchor` cells are dropped and the
rest drawn, and the text after it doesn't move. The core's `$`, cursor-follow scroll and
`window_max_leftcol` measure lines with their splices.

This changes what `leftcol` means for every line with inline text, so `winsaveview` /
`winrestview` and `btv.win.set_leftcol` need checking: neovim's value counts the virtual
text too, so matching it is right.

### Where it lands

- `bemtvi-core`: the splice list, wrapping, the scroll arithmetic, and the click (the bug
  file's fix).
- The three clients: clip a placement at `leftcol` rather than drop it. That's the filter in
  `virt_inline_shift` and the splice in the GUI, `emit_inline_virt` and `virt_cursor_shift`
  in the terminal, and the row walk in `web/index.html`.

## Tests first

Each bug gets a failing test before its fix, per the project's bug-fix rule.

In `crates/bemtvi-server/tests/`, black-box with the harness:

- **Wrap.** 80 columns, no gutter: a 76-cell line with 8 inline cells at column 0 takes two
  rows (`numbers` reads `[1, 1, 2]`). Fails today; it reads `[1, 2]`.
- **Wrap, mid-line.** A placement near the end of a row moves to the next row with its
  anchor, and the row before it ends short.
- **Scroll.** Under `'nowrap'`, a 300-cell line with 8 inline cells at column 0: `$` sets
  `leftcol` to 8 more than without the mark, and the cursor's screen column is the last text
  column.
- **Partial scroll.** `btv.win.set_leftcol` to 3 with the cursor past it: the frame still
  carries the placement, and the client tests below show 5 of its cells.

In the client suites:

- `crates/bemtvi-tui/tests/paint.rs`: with `leftcol = 3` and the placement at column 0, the
  row starts with the placement's last 5 cells, then the text; with `leftcol = 10`, the text
  starts at text column 2.
- `crates/bemtvi-gpu/tests/`: the same through `virt_inline_shift` and the splice.
- The browser: a `verify-inline-virt-scroll.mjs` beside the other `verify-*.mjs` checks.

## Order

1. The splice list, with the click fix on top of it (that bug's tests first).
2. Wrapping.
3. Scrolling, core then clients.

Each step ends with the suite green and a commit.

## Questions

- Whether `leftcol` in `winsaveview` should count virtual text (neovim's does).

## Decided in step 1

- **Inlay hints join the list per buffer, not per frame.** The server pushes a buffer's whole
  hint list to the core whenever it changes (`sync_inlay_splices`, beside the Lua mirror push),
  rather than each visible line's hints as a frame projects. Wrapping has to count rows before
  a frame exists and for lines off screen, so a frame's push would always lag it by one.
- **`virt_text_hide` no longer applies to inline marks**, which is neovim's rule too. Otherwise
  selecting a row would reflow it once wrapping counts its cells.

## Decided in step 2

- **A placement moves to the next row whole**, where neovim would split it across rows, so a
  label is never cut. `Buffer::inline_splices` now uses `ExtmarkStore::starting_in`.
- **A placement wider than a whole row hides its anchor character.** It starts the row and is
  clipped there, the character it's drawn before falls past the edge, and the text after that
  wraps below and shows. The cursor can sit on that hidden character. Fixing it means clipping
  the placement to leave a cell, which the wire doesn't do today.

## Decided in step 3

- **The server clips, not each client.** `leftcol` counts text and inline cells together, but
  clients use it in dozens of places as a text column for the whole window. So the server
  applies it (`scroll_inline_rows`): it drops hidden splices, trims the one the scroll falls
  inside, and sends `row_leftcol`, the text column each row paints from, when any row differs.
  Clients read that per row and keep their other arithmetic, so there is one implementation.
- **A pan's frames clip on the client.** A slide's column changes every frame and the server
  knows only its two ends, so the band carries inline cells whole and each frame applies the
  interpolated column: `bemtvi_view::clip_band_row` for the GUI and terminal, `clipBandRows` in
  `web/index.html`. Same model as the server's `row_scroll`, so the last frame paints what the
  settled window does.
- **`'colorcolumn'` stays in `leftcol` space**, so a ruler doesn't bend around inline text.
