# Unicode-aware buffer navigation and display

**Status:** approved design, pending implementation
**Date:** 2026-05-30
**Scope:** `bemtvi-core`, `bemtvi-server`, `bemtvi-tui`, integration tests

## Problem

bemtvi's rope stores UTF-8 correctly and its editing *primitives* already snap to
char boundaries (`floor`/`ceil_char_boundary`, `len_utf8`, `advance_chars`), so
multibyte text is never corrupted and typing `café` or `日本語` inserts fine. The
breakage is entirely in **navigation** and **display**:

1. **Horizontal motion moves by bytes, not characters.** `h`/`l`/`Space`/arrows
   compute `col ± count` in *bytes* and then snap back to a char boundary, so the
   cursor gets *stuck* on a multibyte character — on `néon` you cannot `l` from
   `é` to `o`. This is the most visible "it's broken."
2. **The displayed cursor lands in the wrong cell.** `View.cursor_col` is a byte
   offset, but the TUI uses it directly as a terminal cell column. The text
   itself renders correctly (ratatui is `unicode-width` aware), but the cursor
   drifts right of the real character — further for wide chars (CJK/emoji span 2
   cells).
3. **Word motions misclassify continuation bytes.** `w`/`b`/`e`/`word_*` walk
   byte-by-byte and `char_at` returns `' '` (Blank) for non-boundary bytes, so a
   multibyte word character looks like it contains blanks and word boundaries are
   wrong on non-ASCII words.
4. **Vertical motion's desired column and `$` are byte-based**, so `j`/`k` don't
   keep a stable *screen* column over multibyte/wide/tab text.

This was a known gap: `docs/architecture.md` notes "Display still assumes one
cell per byte/char … cursor placement for non-ASCII text is approximate for now,"
and the roadmap lists "Wide-character / tab-width aware display and cursor
placement."

## Goals

- Cursor **movement** steps by **grapheme cluster** (user-perceived character):
  `e` + combining accent, flag emoji, and ZWJ emoji sequences each move and
  delete as one unit, matching modern neovim.
- Cursor **display** lands on the correct terminal cell, accounting for
  **wide characters** (CJK/emoji = 2 cells) and **tabs** (`tabstop`-aware virtual
  columns).
- `j`/`k`/`$` preserve a stable **screen column** across wide/tab/multibyte text.
- Word motions respect grapheme clusters and classify by the base character.
- All correct behavior is verified by black-box integration tests.

## Non-goals

- `:set tabstop` / the options system. `tabstop` is a constant (`8`) until
  options land; today `:set` is a stub.
- Bidirectional / RTL text and Unicode normalization (NFC/NFD folding).
- Reconciling terminals that disagree with `unicode-width`. We standardize on
  `unicode-width`, which is exactly what ratatui uses to render, so core's
  computed columns and the painted glyphs agree by construction.
- `nvim_win_get_cursor` semantics: it continues to return the **byte** column.

## The model

**`cursor.col` stays a byte offset within the line.** It is the rope's native
metric, the invariant the codebase is built on, and exactly what
`nvim_win_get_cursor` reports. We do not change that contract. Three distinct
"column" concepts are separated explicitly:

| concept            | meaning                                  | used for                                  |
| ------------------ | ---------------------------------------- | ----------------------------------------- |
| **byte column**    | byte offset within the line (`cursor.col`) | rope indexing, `nvim_win_get_cursor`, ruler |
| **grapheme step**  | next/prev grapheme-cluster boundary      | `h`/`l`/`x`/word motion units             |
| **virtual column** | screen cells before a byte offset (wide + tab aware) | `j`/`k`/`$` desired column, cursor placement |

Motion *steps by grapheme*; the editor *stores a byte offset*; display and
vertical-motion memory *use virtual columns*. The byte↔grapheme↔virtual
conversions are pure functions over a line string.

### Where the byte→cell conversion lives

Core is the single source of truth for column semantics (vim's column model
belongs there); the TUI stays thin. Concretely:

- **Core** computes grapheme motion, virtual columns for `j`/`k`/`$`, and the
  cursor's **screen-cell column**, which it carries in the `View` as a new field
  `cursor_screen_col`. The existing `View.cursor_col` keeps its **byte** meaning
  (drives the ruler, matches the API).
- **The TUI** places the terminal cursor at `cursor_screen_col`, and **expands
  tabs to spaces at the same `tabstop`** when painting text lines, so ratatui's
  rendered width matches core's virtual columns. The ruler shows the byte column.

The only coupling this imposes is that the client renders tabs at the tabstop
core assumes — satisfied by the client expanding tabs itself.

## Component changes

### `bemtvi-core`

**New `unicode` module** — pure, synchronous helpers operating on a line `&str`
(lines are cheap to materialize via `Buffer::line`, which sidesteps ropey
chunk-boundary handling for within-line motion):

- `next_grapheme(line, byte) -> usize` / `prev_grapheme(line, byte) -> usize` —
  grapheme-cluster boundaries (via `unicode-segmentation`), clamped to the line.
- `floor_grapheme(line, byte) -> usize` — snap a byte offset down to a grapheme
  boundary (never land between base + combining mark).
- `virtcol(line, byte, tabstop) -> usize` — screen cells before `byte`: wide
  chars via `unicode-width`'s `UnicodeWidthStr`, tabs advance to the next
  multiple of `tabstop`.
- `byte_at_virtcol(line, target, tabstop) -> usize` — inverse, for `j`/`k`
  landing: the grapheme-boundary byte whose virtual column best matches `target`
  (vim semantics: land on the grapheme covering the target cell).
- `TABSTOP: usize = 8` constant.

**Dependencies:** add `unicode-width` and `unicode-segmentation` (already present
transitively via ratatui) to `[workspace.dependencies]` pinned `=x.y.z`, and pull
into `bemtvi-core` with `<dep>.workspace = true`. Both are pure/synchronous and do
not violate core's purity rule (they are computational, like `ropey`).

**Editor changes (`editor.rs`):**

- Horizontal motions (`h`/`l`/`Space`/`Left`/`Right`, normal *and* insert mode)
  step by grapheme cluster; `count` counts graphemes. Fixes the stuck-on-`é` bug.
- `desired_col` becomes a **virtual** column. `input()`'s post-action update sets
  it from the cursor's virtcol; `settle_desired_col` lands the cursor on the
  grapheme nearest the remembered virtual column. `$` stickiness (`desired_eol`)
  unchanged in spirit, now over virtual columns.
- `word_forward`/`word_backward`/`word_end` iterate grapheme clusters and
  classify by the cluster's base character, so continuation bytes are no longer
  seen as blanks.
- `set_cursor_char`/`snap_cursor` floor to **grapheme** boundaries;
  `first_non_blank` returns an unambiguous byte offset (today it returns a char
  count that happens to equal the byte offset only because leading blanks are
  ASCII).
- Per-character operators that already use `advance_chars` (`x`, `r`, `~`) move to
  grapheme stepping so they cover a whole cluster.

**View (`view.rs`):** add `cursor_screen_col: usize` (virtual column of the
cursor on its line). `cursor_col` stays the byte column. Update the module doc
(which currently says "one display cell per byte").

### `bemtvi-server`

- `redraw()` adds `cursor_screen_col` to the notification map.
- `nvim_win_get_cursor` unchanged (byte column).

### `bemtvi-tui`

- Cursor placement uses `cursor_screen_col` instead of `cursor_col`.
- Expand tabs → spaces at `tabstop` (8) when rendering the text lines, so painted
  widths line up with core's virtual columns.
- Mirror the new `cursor_screen_col` field in the client-side `View` and
  `update()`.
- Ruler keeps the byte column (`cursor_col + 1`). *Nice-to-have:* show
  `byte-screen` when the two differ, as vim does (e.g. `1,5-9`). Optional.

## Data flow

```
key → core: motion steps by grapheme → cursor.col (bytes) updated
                                     → desired_col kept as virtcol
core.view():  cursor_col       = cursor.col           (bytes)
              cursor_screen_col = virtcol(line, col)  (cells)
server.redraw(): both fields → redraw map
tui.render():  expand tabs in lines → paint; cursor at cursor_screen_col
nvim_win_get_cursor: cursor.col (bytes) — unchanged
```

## Error handling / edge cases

- Byte offsets handed to the rope are always grapheme- (hence char-) aligned, so
  no panics from mid-codepoint slicing.
- Empty line: virtcol 0, motions no-op at the single position.
- Cursor on a wide char: terminal places the cursor at the char's first cell
  (standard).
- Combining mark with no base / lone continuation: `floor_grapheme` keeps the
  cursor on a valid boundary; `char_at` non-boundary fallback is retained as a
  defensive default.
- Trailing-newline invariant and `line_len` (byte length) are unchanged; clamps
  continue to use byte lengths, with grapheme/virtual conversions layered on top.

## Testing

Black-box integration tests in `crates/bemtvi-server/tests/editing.rs`, using the
existing `start`/`feed`/`lines`/`cursor` helpers, plus **one new helper** that
reads the latest `redraw` notification from the client's `incoming` channel and
returns `cursor_screen_col` — this is how display-column correctness is asserted
end-to-end.

- **Multibyte motion:** insert `néon`; `0` then repeated `l` moves the byte column
  `0 → 1 → 3 → 4` (was stuck at `1`). `h` reverses it.
- **Grapheme delete:** `e` + combining acute (`e\u{0301}`, 3 bytes, one grapheme);
  `x` deletes the whole cluster; `l` moves byte column `0 → 3`.
- **Wide char display:** insert `日本語`; after one `l`, `cursor_screen_col == 2`
  while the byte column is `3`.
- **Tabs:** with a leading `\t`, the following character's `cursor_screen_col`
  is `8`; `j`/`k` keep the screen column across a line containing a tab.
- **Word motion:** `dw` over `héllo wörld` deletes `héllo ` (boundary correct
  despite multibyte).
- **Regression:** existing ASCII tests stay green (grapheme/virtual paths are a
  no-op for ASCII).

## Affected files

- `Cargo.toml` — pin `unicode-width`, `unicode-segmentation`.
- `crates/bemtvi-core/Cargo.toml` — add the two deps.
- `crates/bemtvi-core/src/unicode.rs` — new helpers.
- `crates/bemtvi-core/src/editor.rs` — grapheme motion, virtual desired column,
  word motions, snapping.
- `crates/bemtvi-core/src/view.rs` — `cursor_screen_col`.
- `crates/bemtvi-server/src/lib.rs` — plumb `cursor_screen_col`.
- `crates/bemtvi-tui/src/lib.rs` — cursor placement, tab expansion, mirror field.
- `crates/bemtvi-server/tests/editing.rs` — new tests + redraw helper.
- `docs/architecture.md` — update the "one cell per byte" caveat once done.
