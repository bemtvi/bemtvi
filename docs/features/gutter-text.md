# Gutter text

A plugin can give each line a column of its own text, between the line numbers
and the text: a diff's old and new line numbers, a blame hash and author, a
side-by-side view's own numbering. It is the `gutter_text` extmark key, so it
uses extmarks' namespaces, gravity and `view:set_decor`, and it moves with its
line as the text is edited.

```lua
local ns = btv.ns.create("my-diff")
btv.buf.set_extmark(0, ns, 4, 0, {
  gutter_text = { { "  12 ", "DiffDelete" }, { "     " } },
  line_hl_group = "DiffDelete",
})
```

A window reads, left to right: fold column, signs, line numbers, gutter text,
text.

## What it does

- **The column is not part of the line.** The text keeps its own columns, so a
  sideways scroll under `'nowrap'` leaves the column where it is, a click lands
  on the character under the pointer, `$` scrolls by the text area that is
  left, and `'wrap'` wraps into it. (Inline virtual text at column 0, the
  usual way to fake this, gets all four wrong.)
- **One width for the whole buffer.** The column is as wide as the widest
  `gutter_text` in the buffer, measured in screen cells, so a wide character
  in a blame column still lines up. A shorter one is padded on the right. The
  width comes from the whole buffer rather than the visible rows, so the text
  doesn't shift as you scroll past a long entry. A plugin that wants numbers
  right-aligned pads them itself. The column is never wider than what the
  window has left beside its fold, sign and number columns: text past that is
  cut off.
- **First row only.** Like a line number, it shows on a wrapped line's first
  display row; continuation rows, virtual lines and `~` filler rows leave the
  column blank. A closed fold shows its first line's.
- **One mark per line wins.** Several marks on one line don't concatenate, so
  the column can't fall out of line: the highest `priority` wins, then the
  higher mark id. Within one namespace that is the mark placed later, unless
  you reuse an explicit `id`; across namespaces, give the marks different
  priorities.
- **Colours.** Each chunk's group is honoured as given, through the window's
  `'winhighlight'`. A chunk without one paints like the line numbers:
  `LineNr`, or `CursorLineNr` on the cursor line. A `line_hl_group` tint runs
  under the column too, so a diff's tint reaches its numbers.

## The rules

- `gutter_text` is a non-empty list of `{ text, group? }` chunks. Anything else
  raises, naming the key and the chunk: an empty list, a hole in the list, a
  named key beside it, or a third slot in a chunk. So does a newline, a tab or
  any other control character in the text.
- It works from `btv.buf.set_extmark`, `view:set_decor` and a `btv.decor`
  provider's `publish` alike, since the three share one option vocabulary.
- A `btv.decor.expr` block can't paint it: that frame-time surface has its own
  key list, and `gutter_text` isn't on it.
- One column per window. Two plugins wanting a column each (blame beside a
  diff) is not supported yet.
- The column is not clickable on its own: a click there behaves like a click on
  a line number, and the pointer stays the arrow.

## Testing it

In a `btv.test` spec, `t:decor(row).gutter` is the row's text as sent (the
chunks joined, before padding) and `t:gutter().gutter_text_width` is the
column's width. `t:screen()` doesn't show it, since it isn't buffer text.

## Example

`examples/gutter-text/` paints old and new numbers over a unified diff and a
blame column with a CJK author:

```sh
BEMTVI_CONFIG=examples/gutter-text cargo run -p bemtvi -- examples/gutter-text/sample.txt
bemtvi --test-plugin examples/gutter-text
```

## Cost

Reading the column's width costs nothing per frame: the extmark store keeps a
count of marks per width as marks are set, deleted and cleared. A buffer with
no `gutter_text` skips the column's projection entirely. With some, the visible
marks come from the extmark store's position index, as every other layer's do, so
marks off screen cost a frame nothing.
