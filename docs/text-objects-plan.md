# Implementation plan: `i`/`a` text objects

Goal: make vim text objects work as operator motions and as visual-mode
selections — `diw`, `viw`, `ciw`, `daw`, `di"`, `ci(`, `va{`, etc. The
mechanism is shared between operator-pending and visual mode, exactly as in
vim.

This document is the implementation contract. All code lives in
`crates/nxvim-core/src/editor.rs`; tests go in
`crates/nxvim-server/tests/editing.rs`.

## Scope

Phase 1 (this plan):

- **Word objects:** `iw` `aw` (word-class, via `char_class`) and `iW` `aW`
  (faithful `WORD` — whitespace-delimited, punctuation/word boundary ignored).
- **Bracket/pair objects:** `i(` `i)` `ib` `a(` `a)` `ab`, `i{` `i}` `iB`
  `a{` `a}` `aB`, `i[` `i]` `a[` `a]`, `i<` `i>` `a<` `a>`
- Counts on word objects (`d2aw`, `2iw` in visual). Counts on pair objects
  (`2di(` → expand `n` levels out) are **out of scope** for phase 1; a count
  on a pair object is accepted and ignored.

Phase 2 (implemented): **quote objects** — `i"` `a"` `i'` `a'`
`` i` `` `` a` `` — confined to the cursor's line, with backslash escaping
(`\"` is not a delimiter; `\\` is a literal backslash). A dangling odd quote
(`"trib"uto"`) pairs with the previous quote so either side is selectable.

Phase 3 (implemented): **paragraph objects** (`ip`/`ap`, linewise — blank-line
delimited blocks) and **sentence objects** (`is`/`as`, charwise — `.`/`!`/`?`
plus closing `)]"'` and whitespace, bounded by the paragraph). Both honor
counts. This required threading a `linewise` flag through `text_object_range` /
`apply_text_object` (paragraphs select whole lines via `VisualLine` /
`apply_operator_to_range(.., true, ..)`).

Explicitly out of scope: tag objects (`it`/`at`).

## Background: how the existing machinery works

The key fact that shapes this design: today every operator range is derived
**relative to the cursor**. `apply_operator` (editor.rs:1316) takes a
`MotionResult { target, kind, .. }` and builds the range as
`min(cursor, target) .. max(cursor, target)` (+1 for inclusive). That works
for motions because the cursor is always one endpoint of the range.

**Text objects break that assumption** — the cursor sits *inside* the object
(`diw` with the cursor in the middle of a word deletes the whole word on both
sides). So a text object cannot be expressed as a `MotionResult.target`; it
must produce an **explicit absolute byte range**, and the operator must be
applied to that range directly, bypassing the cursor-relative computation.

Relevant existing pieces we reuse unchanged:

- `char_class` (editor.rs:3273) → `Blank | Word | Punct`. The basis for word
  objects.
- `next_grapheme_idx` / `prev_grapheme_idx` (editor.rs:3048/3061) — buffer-wide
  grapheme stepping over absolute byte offsets, newline-aware.
- `cursor_char` (editor.rs:3036), `char_at` (editor.rs:3040),
  `last_char_idx` (editor.rs:3123), `byte_to_line` / `line_start` / `line` /
  `byte_at` on `Buffer`.
- `yank_range` / `delete_range` (push undo internally), `set_cursor_char`,
  `set_cursor_char_insert`, `clamp_cursor`.

## Design

### 1. New pending state

Add one field to `Editor` (near `operator`/`pending_replace`, editor.rs:472):

```rust
/// Set to `'i'` or `'a'` after a text-object introducer is seen while an
/// operator is pending or in visual mode; the next key is the object kind.
pending_textobject: Option<char>,
```

Initialise to `None` in `Editor::new` (editor.rs:~569) and in the other
struct literal at editor.rs:~2742. Clear it in:

- `reset_pending` (editor.rs:3240) — add `self.pending_textobject = None;`
- the `Esc` branch of `handle_normal` (editor.rs:904) — add the same line.

### 2. Dispatch: starting a text object

`i`/`a` only introduce a text object when an operator is pending **or** we are
in visual mode. In plain normal mode they remain insert/append. This is the
natural vim disambiguation and needs no lookahead.

In `handle_normal`, after the `g`-prefix block and **before** the count-aware
`resolve_motion` call (i.e. between editor.rs:931 and 933), add:

```rust
// `i`/`a` introduce a text object when an operator is pending or we're in
// visual mode. (In plain normal mode they stay insert/append.)
if self.pending_textobject.is_none()
    && (self.operator.is_some() || self.mode.is_visual())
{
    if let Some(c @ ('i' | 'a')) = key.as_char() {
        self.pending_textobject = Some(c);
        return;
    }
}
```

Placing this before `resolve_motion`/`handle_normal_command` is important:
with an operator pending, the existing `handle_normal_command` would otherwise
hit its `_ => self.reset_pending()` arm (editor.rs:981) and cancel the
operator.

### 3. Dispatch: resolving a text object

At the **top** of `handle_normal`, after the `pending_replace` block and after
the `Esc` block (so Esc still cancels), before count accumulation, add:

```rust
// Second key of a text object (`iw`, `a"`, `i(` …).
if let Some(ia) = self.pending_textobject.take() {
    let count = self.effective_count();
    if let Some(obj) = key.as_char() {
        if let Some((lo, hi)) = self.text_object_range(ia, obj, count) {
            self.apply_text_object(lo, hi);
            return;
        }
    }
    // Unknown object char (or Esc handled above): cancel like vim.
    if self.mode.is_visual() {
        self.gpending = false;
    } else {
        self.reset_pending();
    }
    return;
}
```

Note `effective_count()` must be read before the dispatch clears counts — it
multiplies `op_count * count`, so `d2aw` and `2daw` both give 2.

### 4. Applying a resolved range

```rust
/// Apply the pending operator (or extend the visual selection) to an explicit
/// charwise byte range `[lo, hi)` produced by a text object.
fn apply_text_object(&mut self, lo: usize, hi: usize) {
    if self.mode.is_visual() {
        // Set the selection to the object: anchor at the first char, cursor on
        // the last char (inclusive). Stay in (charwise) visual mode.
        if self.mode == Mode::VisualLine {
            self.mode = Mode::Visual;
        }
        self.set_visual_span(lo, hi);
        self.gpending = false;
        return;
    }
    if let Some(op) = self.operator.take() {
        self.apply_operator_to_range(op, lo, hi, false, 0);
    }
    self.reset_pending();
}
```

`set_visual_span(lo, hi)` is a small helper: set `visual_anchor` to the cursor
position for byte `lo`, and the live `cursor` to byte `hi`'s last grapheme
(`prev_grapheme_idx(hi)`), via the existing line/col conversion (mirror
`set_cursor_char`, but also writing `visual_anchor`). Guard the empty case
(`lo == hi`, e.g. `i"` on `""`): leave anchor == cursor at `lo`.

### 5. Refactor `apply_operator` to expose a range entry point

Extract the operator body (the `match op { 'y' | 'd' | 'c' }` at
editor.rs:1337–1369) into:

```rust
/// Apply `op` to the absolute byte range `[lo, hi)`. `linewise`/`first_line`
/// control linewise settling; charwise callers pass `(false, 0)`.
fn apply_operator_to_range(
    &mut self, op: char, lo: usize, hi: usize, linewise: bool, first_line: usize,
) {
    if lo >= hi {
        return;
    }
    // ... exact body moved from apply_operator (the 'y'/'d'/'c' match) ...
}
```

Then `apply_operator` keeps only the cursor-relative range computation
(editor.rs:1317–1333) and calls `apply_operator_to_range(op, lo, hi, linewise,
first_line)`. This is a pure refactor — no behavior change for existing
motions — and gives the text-object path an undo-correct, cursor-independent
entry point (`delete_range`/`yank_range` push undo internally, as they do for
`dw` today).

## Text-object algorithms

Single dispatcher:

```rust
/// Compute the absolute charwise byte range `[start, end)` for a text object.
/// `ia` is `'i'` (inner) or `'a'` (a/around). Returns `None` if no object.
fn text_object_range(&self, ia: char, obj: char, count: usize) -> Option<(usize, usize)> {
    match obj {
        'w' => self.word_object(ia, count, false), // word-class spans
        'W' => self.word_object(ia, count, true),  // WORD: whitespace-delimited
        '(' | ')' | 'b' => self.pair_object(ia, '(', ')'),
        '{' | '}' | 'B' => self.pair_object(ia, '{', '}'),
        '[' | ']' => self.pair_object(ia, '[', ']'),
        '<' | '>' => self.pair_object(ia, '<', '>'),
        _ => None,
    }
}
```

### Word objects (`iw`/`aw`, `iW`/`aW`)

`iw`/`aw` use the three-way `char_class` (a run of `Word` and a run of `Punct`
are distinct objects). `iW`/`aW` use a **two-way** classification —
`Blank` vs `NonBlank` — so a `WORD` is any maximal run of non-whitespace
regardless of punctuation. Capture this with a `big` flag threaded through the
span helper:

```rust
/// `[start, end)` of the maximal run around `idx` of chars sharing its class.
/// `big = false` uses the 3-way `char_class`; `big = true` collapses to
/// Blank vs NonBlank (vim `WORD`). Buffer-wide; stops at the phantom newline.
fn class_span(&self, idx: usize, big: bool) -> (usize, usize) { /* grapheme walk */ }
```

`word_object(ia, count, big)` then drives `class_span(.., big)` identically for
both; only the classifier differs.

- **`iw`/`iW`**: start from `class_span(cursor, big)`. For a count `n`, extend by
  consuming `n-1` further adjacent spans (each adjacent span is the next
  `class_span` starting at the current `end`). vim counts every span —
  word, then whitespace, then word… — so `2iw` over `foo bar` is `foo `
  (word + space run).
- **`aw`**: take the word span, then **append the trailing whitespace span**
  if the next char is `Blank`; if there is no trailing whitespace (object ends
  at EOL/EOF), instead **prepend the leading whitespace span**. If the cursor
  starts on whitespace, `aw` is the whitespace span + the following word span.
  Counts extend by additional word+space units.

Edge cases: empty line (cursor on `\n`) → object is empty or the newline only;
return `None` for `iw` on an empty buffer line if there's nothing, matching
vim's "no word" (acceptable to return the single position → no-op).

### Quote objects (`i"`/`a"`, `'`, `` ` ``)

vim restricts quote objects to the **current line**. Algorithm:

1. Take the current line's text and its `line_start` base offset.
2. Scan the line left-to-right collecting quote-char positions (skip escaped
   `\"` — phase 1 may skip the escape rule and just pair raw quotes; note it).
3. Pair them (1st–2nd, 3rd–4th, …). Choose the pair that **encloses the
   cursor**, else the **first pair that starts at/after the cursor** (vim
   seeks forward to the next quote on the line).
4. `i"` → `(open+1 .. close)` (between the quotes). `a"` → `(open ..
   close+1)`, then include trailing whitespace after the closing quote if any,
   else leading whitespace before the opening quote (vim's `a"` whitespace
   rule).
5. No pair found → `None`.

All offsets are absolute (`line_start + rel`).

### Bracket / pair objects (`i(`/`a(`, `{}`, `[]`, `<>`)

Works across lines. Find the innermost pair enclosing the cursor:

```rust
fn pair_object(&self, ia: char, open: char, close: char) -> Option<(usize, usize)> {
    let open_idx = self.find_unmatched_open(open, close, self.cursor_char())?;
    let close_idx = self.find_match_close(open, close, open_idx)?;
    Some(match ia {
        'i' => (self.next_grapheme_idx(open_idx), close_idx), // between brackets
        _   => (open_idx, self.next_grapheme_idx(close_idx)), // include brackets
    })
}
```

- `find_unmatched_open`: walk backward from the cursor (inclusive: if the
  cursor is **on** an `open`, that's the match), maintaining a depth counter
  that increments on `close` and decrements on `open`; return the index where
  depth goes negative (the enclosing open). Stop at buffer start → `None`.
- `find_match_close`: walk forward from `open_idx`, depth +1 on `open`, -1 on
  `close`; return the index where depth hits 0.
- Cursor sitting **on** a `close`: handled because the backward walk starting
  at the cursor sees the `close` first (depth +1) and then must find an extra
  `open` — which is correct for the enclosing pair. (Verify with a test like
  `f)di(`.)

`i(` empty pair `()` → `(open+1 .. open+1)` empty range → operator is a no-op,
visual selects nothing; guard `lo == hi` in callers (already handled by
`apply_operator_to_range`'s `lo >= hi` early return and the visual empty
guard).

## Tests (`crates/nxvim-server/tests/editing.rs`)

Follow the existing idiom (`start`, `feed`, `lines`, `cursor`; visual
selection via `latest_view` + `view_selection`). Representative cases:

Word objects:
- `diw_deletes_word_under_cursor`: `ifoo bar baz<Esc>0w` then `diw` → `foo  baz`.
- `daw_deletes_word_and_trailing_space`: same buffer, `0w` `daw` → `foo baz`.
- `ciw_changes_word`: `0ciwqux<Esc>` → `qux bar baz`.
- `viw_selects_word` (visual): assert selection columns via `view_selection`.
- `d2aw_deletes_two_words` (count).
- `diw_on_whitespace_deletes_run`.
- `diw_on_punctuation_run`: `a..b` style (`Punct` span distinct from `Word`).
- `diW_spans_punctuation`: `foo.bar baz` → `diW` on `foo.bar` deletes the
  whole `foo.bar` (WORD ignores the `.` boundary), unlike `diw`.

Pair objects:
- `di_paren_inside`: `i(foo)<Esc>0` `di(` → `()`.
- `da_paren_includes_parens`: → empty line.
- `ci_brace_nested`: `i{a{b}c}<Esc>` cursor on inner → `di{` deletes `b`.
- `di_paren_on_close_bracket`: cursor on `)` still deletes inside.
- `vi_bracket_selects` (visual): `[x]`.
- `da_angle`: `<a>` → ``.
- `b_and_B_aliases`: `dib` ≡ `di(`, `diB` ≡ `di{`.

Disambiguation guard:
- `i_in_normal_mode_still_inserts`: ensure adding the dispatch didn't break a
  bare `i`.
- `a_in_normal_mode_still_appends`.

## Edge cases / invariants checklist

- All ranges are absolute byte offsets on grapheme boundaries; rely on the
  grapheme-stepping helpers, never raw `+1`, except where stepping over a known
  ASCII bracket/quote (those are single-byte — fine, but prefer
  `next_grapheme_idx` for uniformity).
- Respect `last_char_idx()` — never let a range include the trailing phantom
  `\n` (the rope invariant from CLAUDE.md).
- Cursor settling after operator reuses `apply_operator_to_range`'s existing
  arms (`set_cursor_char(lo)` for delete/yank, insert-park for change).
- `pending_textobject` participates in `reset_pending` and `Esc` so partial
  sequences (`di<Esc>`, `vi` then a bogus key) cancel cleanly.
- No new dependencies; no async; stays inside `nxvim-core` (pure/sync per
  CLAUDE.md).

## Known limitations (phase 1, as built)

- **Block objects don't search forward.** Like vim, `di(` etc. require the
  cursor to be inside or on one of the brackets; with the cursor before the
  pair on the line, nothing happens. (nxvim has no `f`/`t` find-char motion
  yet, so tests position the cursor with `l`.)
- **Linewise promotion (implemented).** An inner block object (`i(`/`i{`/`i[`/
  `i<`) promotes to linewise when the inner range is whole lines — the open
  bracket ends its line and the close bracket starts its line (modulo
  whitespace) — selecting the lines between and leaving the bracket lines. In
  visual mode the object stays charwise, and `a(`-style outer objects are
  always charwise, matching vim.
- **Quote escaping is a fixed backslash.** The quote scan honors `\` as the
  escape (vim's `quoteescape`), so `\"` is skipped and `\\"` closes; the escape
  char is not configurable.
- **Paragraph blank lines are empty lines.** A line counts as a separator only
  if it has zero length; whitespace-only lines are part of the paragraph.
- **Sentence detection is approximate.** Terminator is `.`/`!`/`?` + optional
  closing `)]"'` + whitespace/EOL; sentences are bounded by the paragraph and
  do not implement vim's abbreviation/`J`-join nuances.
- **Tag objects** (`it`/`at`) are not implemented.

## Suggested commit slicing

1. Refactor: extract `apply_operator_to_range` (no behavior change).
2. Plumb `pending_textobject` state + dispatch + `apply_text_object` +
   `set_visual_span`, with **word objects** (`iw`/`aw`/`iW`/`aW`) only. Tests
   for words.
3. Bracket/pair objects (`()`,`{}`,`[]`,`<>`, `b`/`B` aliases) + tests.
4. Quote objects (`"`, `'`, `` ` ``) + tests.
5. Paragraph (`ip`/`ap`, linewise) + sentence (`is`/`as`) objects; thread a
   `linewise` flag through the apply path.
