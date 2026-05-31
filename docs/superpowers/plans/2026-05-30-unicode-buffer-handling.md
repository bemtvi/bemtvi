# Unicode-aware Buffer Navigation and Display — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cursor movement step by grapheme cluster and make cursor display/`j`/`k` honor wide characters and tabs, so non-ASCII text in the buffer behaves correctly.

**Architecture:** `cursor.col` stays a byte offset within its line (the rope's metric and what `nvim_win_get_cursor` returns). A new pure `nxvim-core::unicode` module converts between byte offset, grapheme boundary, and virtual (screen) column over a line `&str`. Motions step by grapheme; vertical motion and `$` remember a virtual column; the `View` gains `cursor_screen_col` (cells) for terminal placement while `cursor_col` stays the byte column for the ruler/API. The TUI expands tabs when painting so its widths match core's.

**Tech Stack:** Rust, `ropey` 2.0, `unicode-segmentation`, `unicode-width`, ratatui, msgpack-RPC.

**Testing note (project convention overrides default TDD):** Per `CLAUDE.md` and `docs/architecture.md`, this repo has **no unit tests** — behavior is verified end-to-end through black-box integration tests in `crates/nxvim-server/tests/editing.rs` (`start`/`feed`/`lines`/`cursor`, plus the `latest_view` redraw helper). Behavioral tasks below follow TDD at that integration level (failing test → implement → pass). Two foundation tasks (the `unicode` module and the internal grapheme helpers) have no direct behavioral surface; they are verified by `cargo build` **and** by the existing ASCII integration suite staying green (a real regression gate, since grapheme/virtual math is a no-op for ASCII).

---

## File structure

- `Cargo.toml` — pin `unicode-width`, `unicode-segmentation` in `[workspace.dependencies]`.
- `crates/nxvim-core/Cargo.toml` — pull both deps into the core crate.
- `crates/nxvim-core/src/unicode.rs` — **new** pure column-math helpers.
- `crates/nxvim-core/src/lib.rs` — declare `pub mod unicode;`.
- `crates/nxvim-core/src/editor.rs` — grapheme helpers, grapheme motion, virtual desired column, word motion, snapping.
- `crates/nxvim-core/src/view.rs` — add `cursor_screen_col`.
- `crates/nxvim-server/src/lib.rs` — plumb `cursor_screen_col` into the redraw map.
- `crates/nxvim-tui/Cargo.toml` — add `unicode-width`.
- `crates/nxvim-tui/src/lib.rs` — place cursor at `cursor_screen_col`, expand tabs when rendering.
- `crates/nxvim-server/tests/editing.rs` — new tests + a `view_u64` helper.
- `docs/architecture.md` — drop the "one cell per byte" caveat once done.

---

## Task 1: Dependencies and the `unicode` module

**Files:**
- Modify: `Cargo.toml` (`[workspace.dependencies]`)
- Modify: `crates/nxvim-core/Cargo.toml`
- Create: `crates/nxvim-core/src/unicode.rs`
- Modify: `crates/nxvim-core/src/lib.rs`

- [ ] **Step 1: Pin the two crates in the workspace manifest**

In `Cargo.toml`, under `[workspace.dependencies]`, after the `mlua` line, add:

```toml
unicode-width = "=0.2.2"
unicode-segmentation = "=1.13.2"
```

(These versions already resolve in `Cargo.lock` via ratatui, so no new download.)

- [ ] **Step 2: Add the deps to the core crate**

In `crates/nxvim-core/Cargo.toml`, under `[dependencies]` (after `ropey.workspace = true`), add:

```toml
unicode-width.workspace = true
unicode-segmentation.workspace = true
```

- [ ] **Step 3: Create the `unicode` module**

Create `crates/nxvim-core/src/unicode.rs` with exactly:

```rust
//! Unicode-aware column math over a single line of text.
//!
//! Cursor columns are stored as byte offsets (the rope's native metric and
//! vim's column model), but *movement* steps by grapheme cluster and *display*
//! accounts for wide characters and tabs. These pure helpers convert between
//! byte offset, grapheme boundary, and virtual (screen) column over a line
//! `&str`. They are a no-op fast path for ASCII.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Width of a tab stop in cells. A constant until the options system (`:set
/// tabstop`) exists.
pub const TABSTOP: usize = 8;

/// Byte offset of the grapheme boundary immediately after `byte` (clamped to
/// the end of `line`). Returns `line.len()` when there is no following grapheme.
pub fn next_grapheme(line: &str, byte: usize) -> usize {
    let byte = floor_grapheme(line, byte);
    line[byte..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(line.len(), |(i, _)| byte + i)
}

/// Byte offset of the grapheme boundary immediately before `byte` (clamped to 0).
pub fn prev_grapheme(line: &str, byte: usize) -> usize {
    let byte = floor_grapheme(line, byte);
    line[..byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// Snap `byte` down to the nearest grapheme-cluster boundary (a no-op for ASCII
/// or when already on a boundary). Returns `line.len()` for `byte >= line.len()`.
pub fn floor_grapheme(line: &str, byte: usize) -> usize {
    if byte >= line.len() {
        return line.len();
    }
    let mut last = 0;
    for (i, _) in line.grapheme_indices(true) {
        if i > byte {
            break;
        }
        last = i;
    }
    last
}

/// Virtual (screen-cell) column of byte offset `byte`: the cells occupied by
/// `line[..byte]`, with tabs expanding to the next multiple of `tabstop` and
/// wide characters counting as two (via `unicode-width`).
pub fn virtcol(line: &str, byte: usize, tabstop: usize) -> usize {
    let byte = floor_grapheme(line, byte);
    let mut col = 0;
    for g in line[..byte].graphemes(true) {
        col += grapheme_width(g, col, tabstop);
    }
    col
}

/// Byte offset of the grapheme whose cell span covers virtual column `target`
/// (or `line.len()` when `target` is at or past the end). Used to land vertical
/// motion on the column nearest the remembered one.
pub fn byte_at_virtcol(line: &str, target: usize, tabstop: usize) -> usize {
    let mut col = 0;
    for (i, g) in line.grapheme_indices(true) {
        let w = grapheme_width(g, col, tabstop);
        if col + w > target {
            return i;
        }
        col += w;
    }
    line.len()
}

/// Cells occupied by a single grapheme starting at virtual column `col`.
fn grapheme_width(g: &str, col: usize, tabstop: usize) -> usize {
    if g == "\t" {
        tabstop - (col % tabstop)
    } else {
        UnicodeWidthStr::width(g)
    }
}
```

- [ ] **Step 4: Declare the module**

In `crates/nxvim-core/src/lib.rs`, add `pub mod unicode;` to the module list (after `pub mod mode;`):

```rust
pub mod buffer;
pub mod editor;
pub mod input;
pub mod mode;
pub mod unicode;
pub mod view;
```

- [ ] **Step 5: Build**

Run: `cargo build -p nxvim-core`
Expected: compiles cleanly (no warnings).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/nxvim-core/Cargo.toml crates/nxvim-core/src/unicode.rs crates/nxvim-core/src/lib.rs
git commit -m "$(printf 'feat(core): add unicode column-math helpers\n\nGrapheme boundaries and virtual (screen) columns over a line, tab- and\nwide-char aware. Pure; used by editor motion and view next.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 2: Editor grapheme helpers and snapping (foundation, no behavior change)

Adds buffer-wide grapheme stepping/snapping and routes the existing snap points through it. For ASCII every byte is a grapheme boundary, so behavior is unchanged — the existing integration suite is the regression gate.

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs`

- [ ] **Step 1: Import the module**

At the top of `crates/nxvim-core/src/editor.rs`, add to the `use crate::...` block:

```rust
use crate::unicode;
```

- [ ] **Step 2: Add buffer-wide grapheme helpers**

In `impl Editor`, in the `// ----- cursor / scrolling helpers -----` section (next to `char_at`), add:

```rust
/// Byte offset one grapheme-cluster forward from `idx` over the whole buffer.
/// The trailing `\n` of each line is itself a single-byte grapheme.
fn next_grapheme_idx(&self, idx: usize) -> usize {
    let line = self.buffer.byte_to_line(idx);
    let start = self.buffer.line_start(line);
    let s = self.buffer.line(line);
    let rel = idx - start;
    if rel < s.len() {
        start + unicode::next_grapheme(&s, rel)
    } else {
        (idx + 1).min(self.buffer.len_bytes())
    }
}

/// Byte offset one grapheme-cluster backward from `idx` over the whole buffer.
fn prev_grapheme_idx(&self, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let line = self.buffer.byte_to_line(idx);
    let start = self.buffer.line_start(line);
    let s = self.buffer.line(line);
    let rel = idx - start;
    if rel == 0 {
        idx - 1
    } else {
        start + unicode::prev_grapheme(&s, rel.min(s.len()))
    }
}

/// Snap an absolute byte offset down to a grapheme boundary.
fn grapheme_floor_abs(&self, idx: usize) -> usize {
    let line = self.buffer.byte_to_line(idx);
    let start = self.buffer.line_start(line);
    let s = self.buffer.line(line);
    let rel = idx.saturating_sub(start).min(s.len());
    start + unicode::floor_grapheme(&s, rel)
}

/// Snap an absolute byte offset up to a grapheme boundary.
fn grapheme_ceil_abs(&self, idx: usize) -> usize {
    let floored = self.grapheme_floor_abs(idx);
    if floored >= idx {
        floored
    } else {
        self.next_grapheme_idx(floored)
    }
}

/// Virtual (screen) column of the cursor on its current line.
fn cursor_virtcol(&self) -> usize {
    let s = self.buffer.line(self.cursor.line);
    unicode::virtcol(&s, self.cursor.col, unicode::TABSTOP)
}
```

- [ ] **Step 3: Route `snap_cursor` through the grapheme floor**

Replace the whole `snap_cursor` method:

```rust
/// Snap the cursor column down to the nearest grapheme boundary (a no-op for
/// ASCII), so byte offsets handed to the rope are always valid.
fn snap_cursor(&mut self) {
    let s = self.buffer.line(self.cursor.line);
    self.cursor.col = unicode::floor_grapheme(&s, self.cursor.col.min(s.len()));
}
```

- [ ] **Step 4: Grapheme-floor in `set_cursor_char` and `set_cursor_char_insert`**

Append `self.snap_cursor();` as the last statement of **both** `set_cursor_char` and `set_cursor_char_insert`, so the final bodies read:

```rust
fn set_cursor_char(&mut self, idx: usize) {
    let idx = self
        .buffer
        .text
        .floor_char_boundary(idx.min(self.last_char_idx()));
    let line = self.buffer.byte_to_line(idx);
    self.cursor.line = line;
    self.cursor.col = idx - self.buffer.line_start(line);
    self.snap_cursor();
}

fn set_cursor_char_insert(&mut self, idx: usize) {
    let idx = self
        .buffer
        .text
        .floor_char_boundary(idx.min(self.buffer.len_bytes()));
    let line = self.buffer.byte_to_line(idx);
    self.cursor.line = line;
    self.cursor.col = idx - self.buffer.line_start(line);
    self.snap_cursor();
}
```

- [ ] **Step 5: Route `snap_range` through grapheme floor/ceil**

Replace the whole `snap_range` method:

```rust
/// Clamp a byte range into bounds and onto grapheme boundaries, so a
/// motion-derived endpoint can never split a cluster (a no-op for ASCII).
fn snap_range(&self, lo: usize, hi: usize) -> (usize, usize) {
    let hi = hi.min(self.buffer.len_bytes());
    let lo = self.grapheme_floor_abs(lo.min(hi));
    let hi = self.grapheme_ceil_abs(hi);
    (lo, hi)
}
```

- [ ] **Step 6: Make `first_non_blank` return an unambiguous byte offset**

Replace the whole `first_non_blank` method:

```rust
fn first_non_blank(&self, line: usize) -> usize {
    let s = self.buffer.line(line);
    s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count()
}
```

- [ ] **Step 7: Build, then run the existing suite as a regression gate**

Run: `cargo build -p nxvim-core`
Expected: compiles cleanly.

Run: `cargo test -p nxvim-server --test editing`
Expected: all existing tests PASS (ASCII behavior unchanged).

- [ ] **Step 8: Commit**

```bash
git add crates/nxvim-core/src/editor.rs
git commit -m "$(printf 'refactor(core): grapheme-aware cursor snapping helpers\n\nAdd buffer-wide grapheme step/floor/ceil helpers and route snap_cursor,\nset_cursor_char(_insert), and snap_range through them. No behavior change\nfor ASCII; foundation for grapheme motion.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 3: `View.cursor_screen_col` and server plumbing

Carry the cursor's screen-cell column so clients can place the terminal cursor correctly. `cursor_col` keeps its byte meaning.

**Files:**
- Modify: `crates/nxvim-core/src/view.rs`
- Modify: `crates/nxvim-server/src/lib.rs`
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Add a `view_u64` test helper**

In `crates/nxvim-server/tests/editing.rs`, right after the existing `view_str` function, add:

```rust
fn view_u64(view: &[(Value, Value)], key: &str) -> u64 {
    view_get(view, key).and_then(Value::as_u64).unwrap_or(0)
}
```

- [ ] **Step 2: Write the failing tests**

In `crates/nxvim-server/tests/editing.rs`, add:

```rust
#[tokio::test]
async fn screen_column_accounts_for_wide_characters() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>"); // each CJK char is 3 bytes wide, 2 cells wide
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");
    // Cursor rests on the last char 本: byte column 3, screen column 2.
    assert_eq!(view_u64(&view, "cursor_col"), 3);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 2);
}

#[tokio::test]
async fn screen_column_expands_tabs_to_the_next_tabstop() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i\tx<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    // Cursor on 'x' at byte column 1; the leading tab puts it at screen col 8.
    assert_eq!(view_u64(&view, "cursor_col"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 8);
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p nxvim-server --test editing screen_column`
Expected: both FAIL — `cursor_screen_col` is absent, so `view_u64` returns 0 (2 ≠ 0, 8 ≠ 0).

- [ ] **Step 4: Add the field to the core `View`**

In `crates/nxvim-core/src/view.rs`, add the field to the struct (after `pub cursor_col: usize,`):

```rust
    pub cursor_col: usize,
    /// Cursor's screen-cell column on its line (wide-char and tab aware). Used
    /// by clients to place the terminal cursor; `cursor_col` stays the byte
    /// column for the ruler and `nvim_win_get_cursor`.
    pub cursor_screen_col: usize,
```

Then update the module-doc line that currently says "One display cell per byte for now — no wide-char/tab-width handling yet." to:

```rust
//! Columns are byte offsets (ropey's native metric and vim's column model);
//! `cursor_screen_col` additionally carries the cursor's screen-cell column,
//! accounting for wide characters and tabs.
```

- [ ] **Step 5: Populate the field in `from_editor`**

In `crates/nxvim-core/src/view.rs`, inside `from_editor`, compute the screen column before the `View { ... }` literal:

```rust
        let cursor_screen_col = {
            let line = ed.buffer.line(ed.cursor.line);
            crate::unicode::virtcol(&line, ed.cursor.col, crate::unicode::TABSTOP)
        };
```

and add `cursor_screen_col,` to the `View { ... }` literal, right after `cursor_col: ed.cursor.col,`:

```rust
            cursor_col: ed.cursor.col,
            cursor_screen_col,
```

- [ ] **Step 6: Plumb it through the redraw map**

In `crates/nxvim-server/src/lib.rs`, in `redraw()`, add an entry to the `map` vector right after the `cursor_col` entry:

```rust
            (
                Value::from("cursor_col"),
                Value::from(view.cursor_col as u64),
            ),
            (
                Value::from("cursor_screen_col"),
                Value::from(view.cursor_screen_col as u64),
            ),
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p nxvim-server --test editing screen_column`
Expected: both PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/nxvim-core/src/view.rs crates/nxvim-server/src/lib.rs crates/nxvim-server/tests/editing.rs
git commit -m "$(printf 'feat(core): carry cursor screen column in the View\n\nAdd cursor_screen_col (wide-char/tab-aware cells) alongside the byte\ncursor_col, and plumb it through the redraw map. Verified by wide-char\nand tab integration tests.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 4: Horizontal motion by grapheme (`h`, `l`, `$`)

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs`
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Write the failing test**

In `crates/nxvim-server/tests/editing.rs`, add:

```rust
#[tokio::test]
async fn horizontal_motion_steps_over_multibyte_chars() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "in\u{e9}on<Esc>"); // "néon": n é(2 bytes) o n
    feed(&rpc, "0");
    assert_eq!(cursor(&rpc).await, (1, 0)); // 'n'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 1)); // 'é'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 3)); // 'o' — skipped é's second byte
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 4)); // last 'n'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 4)); // stays put at end of line
    feed(&rpc, "hh");
    assert_eq!(cursor(&rpc).await, (1, 1)); // back across 'o' and onto 'é'
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p nxvim-server --test editing horizontal_motion_steps_over_multibyte_chars`
Expected: FAIL — after the first `l` the cursor sticks at column 1 (cannot pass `é`).

- [ ] **Step 3: Implement grapheme stepping for `h`, `l`, `$`**

In `crates/nxvim-core/src/editor.rs`, inside `resolve_motion`, replace the three relevant arms of the `match (kc, ch)`.

Replace the `h` / Left / Backspace arm with:

```rust
            (KeyCode::Left, _) | (_, Some('h')) | (KeyCode::Backspace, _) => {
                let s = self.buffer.line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::prev_grapheme(&s, col);
                }
                MotionResult {
                    target: self.buffer.byte_at(line, col),
                    kind: MotionKind::Exclusive,
                    axis: MoveAxis::Horizontal,
                }
            }
```

Replace the `l` / Right / Space arm with:

```rust
            (KeyCode::Right, _) | (_, Some('l')) | (_, Some(' ')) => {
                let s = self.buffer.line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::next_grapheme(&s, col);
                }
                MotionResult {
                    target: self.buffer.byte_at(line, col),
                    kind: MotionKind::Exclusive,
                    axis: MoveAxis::Horizontal,
                }
            }
```

Replace the `$` / End arm with:

```rust
            (_, Some('$')) | (KeyCode::End, _) => {
                let l = (line + count - 1).min(last_line);
                let s = self.buffer.line(l);
                let col = unicode::prev_grapheme(&s, s.len());
                MotionResult {
                    target: self.buffer.byte_at(l, col),
                    kind: MotionKind::Inclusive,
                    axis: MoveAxis::EndOfLine,
                }
            }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p nxvim-server --test editing horizontal_motion_steps_over_multibyte_chars`
Expected: PASS.

- [ ] **Step 5: Run the whole editing suite (regression)**

Run: `cargo test -p nxvim-server --test editing`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-server/tests/editing.rs
git commit -m "$(printf 'fix(core): h/l/$ step by grapheme cluster\n\nHorizontal motions advance to the next/previous grapheme boundary instead\nof by one byte, so the cursor no longer sticks on multibyte characters.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 5: `x` and `r` operate on whole graphemes

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs`
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/nxvim-server/tests/editing.rs`, add:

```rust
#[tokio::test]
async fn x_deletes_a_whole_grapheme_cluster() {
    let (rpc, _incoming) = start(None).await;
    // 'e' + combining acute accent (one grapheme, three bytes) followed by 'x'.
    feed(&rpc, "ie\u{0301}x<Esc>");
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn x_deletes_a_wide_char_and_leaves_the_rest() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>");
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec!["本"]);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p nxvim-server --test editing x_deletes`
Expected: FAIL — `x` deletes one byte, leaving a broken/partial character (the assertion on the resulting line mismatches).

- [ ] **Step 3: Rename `advance_chars` to `advance_graphemes` and step by grapheme**

In `crates/nxvim-core/src/editor.rs`, replace the whole `advance_chars` method with:

```rust
/// Advance `count` grapheme clusters forward from byte offset `from`, never
/// passing `limit`. Returns the new offset and how many clusters were crossed.
fn advance_graphemes(&self, mut from: usize, count: usize, limit: usize) -> (usize, usize) {
    let mut crossed = 0;
    while crossed < count && from < limit {
        let next = self.next_grapheme_idx(from).min(limit);
        if next == from {
            break;
        }
        from = next;
        crossed += 1;
    }
    (from, crossed)
}
```

- [ ] **Step 4: Update the two callers**

In `delete_under_cursor`, change:

```rust
        let (hi, _) = self.advance_chars(lo, count, line_end);
```

to:

```rust
        let (hi, _) = self.advance_graphemes(lo, count, line_end);
```

In `replace_char`, change:

```rust
        let (hi, crossed) = self.advance_chars(lo, count, line_end);
```

to:

```rust
        let (hi, crossed) = self.advance_graphemes(lo, count, line_end);
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p nxvim-server --test editing x_deletes`
Expected: both PASS.

- [ ] **Step 6: Run the whole editing suite (regression)**

Run: `cargo test -p nxvim-server --test editing`
Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-server/tests/editing.rs
git commit -m "$(printf 'fix(core): x and r act on whole grapheme clusters\n\nadvance_chars becomes advance_graphemes (steps by grapheme), so x/r cover\nbase+combining and wide characters as one unit.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 6: Insert-mode editing by grapheme

Backspace/Delete/Replace/Left/Right/Esc step by grapheme rather than by byte.

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs`
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Write the failing test**

In `crates/nxvim-server/tests/editing.rs`, add:

```rust
#[tokio::test]
async fn insert_backspace_deletes_a_whole_grapheme() {
    let (rpc, _incoming) = start(None).await;
    // Type "aé" then backspace once: the whole 'é' (2 bytes) must go, not half.
    feed(&rpc, "ia\u{e9}");
    feed(&rpc, "<BS>");
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a"]);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p nxvim-server --test editing insert_backspace_deletes_a_whole_grapheme`
Expected: PASS or FAIL depending on byte split — run it and observe. (Backspace currently floors to a *char* boundary; for a precomposed `é` that already deletes the whole 2-byte char, so this specific case may pass. Keep the test: it locks the behavior, and the implementation below also fixes base+combining clusters which a char-floor would split.) If it passes already, still apply Step 3 to cover combining clusters, then re-run.

- [ ] **Step 3: Make insert-mode edits grapheme-aware**

In `crates/nxvim-core/src/editor.rs`, in `handle_insert`, replace these arms.

`Esc` arm — step back one grapheme instead of one byte (snap already protected it, but make it explicit), and drop the now-redundant `desired_col` line (it is recomputed in `input()`):

```rust
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                if self.cursor.col > 0 {
                    let s = self.buffer.line(self.cursor.line);
                    self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
                }
                self.clamp_cursor();
                self.snapshot_taken = false;
            }
```

`Backspace` — handled in `insert_backspace`; see Step 4.

`Left` / `Right`:

```rust
            KeyCode::Left => {
                let s = self.buffer.line(self.cursor.line);
                self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
            }
            KeyCode::Right => {
                let s = self.buffer.line(self.cursor.line);
                self.cursor.col = unicode::next_grapheme(&s, self.cursor.col).min(self.line_len());
            }
```

`Delete`:

```rust
            KeyCode::Delete => {
                let len = self.line_len();
                if self.cursor.col < len {
                    let at = self.cursor_char();
                    let s = self.buffer.line(self.cursor.line);
                    let end = self.buffer.line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer.text.remove(at..end);
                    self.buffer.modified = true;
                }
            }
```

`Char(c)` — when overwriting in Replace mode, remove a whole grapheme:

```rust
            KeyCode::Char(c) => {
                let at = self.cursor_char();
                if self.mode == Mode::Replace && self.cursor.col < self.line_len() {
                    let s = self.buffer.line(self.cursor.line);
                    let end = self.buffer.line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer.text.remove(at..end);
                }
                self.buffer.text.insert_char(at, c);
                self.cursor.col += c.len_utf8();
                self.buffer.modified = true;
            }
```

- [ ] **Step 4: Make `insert_backspace` step by grapheme**

Replace the whole `insert_backspace` method:

```rust
fn insert_backspace(&mut self) {
    if self.cursor.col > 0 {
        let at = self.cursor_char();
        let start = self.buffer.line_start(self.cursor.line);
        let s = self.buffer.line(self.cursor.line);
        let prev_col = unicode::prev_grapheme(&s, self.cursor.col);
        self.buffer.text.remove(start + prev_col..at);
        self.cursor.col = prev_col;
        self.buffer.modified = true;
    } else if self.cursor.line > 0 {
        let prev_len = self.buffer.line_len(self.cursor.line - 1);
        let join_at = self.buffer.byte_at(self.cursor.line - 1, prev_len);
        self.buffer.text.remove(join_at..join_at + 1);
        self.cursor.line -= 1;
        self.cursor.col = prev_len;
        self.buffer.modified = true;
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p nxvim-server --test editing insert_backspace_deletes_a_whole_grapheme`
Expected: PASS.

- [ ] **Step 6: Run the whole editing suite (regression)**

Run: `cargo test -p nxvim-server --test editing`
Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-server/tests/editing.rs
git commit -m "$(printf 'fix(core): insert-mode editing steps by grapheme\n\nBackspace/Delete/Left/Right/Replace and the Esc back-step move by whole\ngrapheme clusters instead of bytes.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 7: Word motions classify by base character over graphemes

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs`
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Write the failing test**

In `crates/nxvim-server/tests/editing.rs`, add:

```rust
#[tokio::test]
async fn dw_deletes_a_multibyte_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ih\u{e9}llo w\u{f6}rld<Esc>"); // "héllo wörld"
    feed(&rpc, "0dw");
    assert_eq!(lines(&rpc).await, vec!["w\u{f6}rld"]);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p nxvim-server --test editing dw_deletes_a_multibyte_word`
Expected: FAIL — `word_forward` walks byte-by-byte and misreads `é`/`ö` continuation bytes as blanks, so the deleted range is wrong.

- [ ] **Step 3: Rewrite the three word-motion helpers to step by grapheme**

In `crates/nxvim-core/src/editor.rs`, replace the whole `word_forward`, `word_backward`, and `word_end` methods with:

```rust
fn word_forward(&self, mut idx: usize) -> usize {
    let last = self.last_char_idx();
    if idx >= last {
        return idx;
    }
    let start = char_class(self.char_at(idx));
    if start != CharClass::Blank {
        while idx < last && char_class(self.char_at(idx)) == start {
            idx = self.next_grapheme_idx(idx);
        }
    }
    while idx < last && char_class(self.char_at(idx)) == CharClass::Blank {
        idx = self.next_grapheme_idx(idx);
    }
    idx
}

fn word_backward(&self, mut idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    idx = self.prev_grapheme_idx(idx);
    while idx > 0 && char_class(self.char_at(idx)) == CharClass::Blank {
        idx = self.prev_grapheme_idx(idx);
    }
    if idx == 0 {
        return 0;
    }
    let cls = char_class(self.char_at(idx));
    while idx > 0 {
        let prev = self.prev_grapheme_idx(idx);
        if char_class(self.char_at(prev)) != cls {
            break;
        }
        idx = prev;
    }
    idx
}

fn word_end(&self, mut idx: usize) -> usize {
    let last = self.last_char_idx();
    if idx >= last {
        return idx;
    }
    idx = self.next_grapheme_idx(idx);
    while idx < last && char_class(self.char_at(idx)) == CharClass::Blank {
        idx = self.next_grapheme_idx(idx);
    }
    let cls = char_class(self.char_at(idx));
    while idx < last {
        let next = self.next_grapheme_idx(idx);
        if next > last || char_class(self.char_at(next)) != cls {
            break;
        }
        idx = next;
    }
    idx
}
```

(`char_at(idx)` reads the base scalar of the grapheme because `idx` is always a grapheme boundary now, so `char_class` classifies the cluster by its base character.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p nxvim-server --test editing dw_deletes_a_multibyte_word`
Expected: PASS.

- [ ] **Step 5: Run the whole editing suite (regression)**

Run: `cargo test -p nxvim-server --test editing`
Expected: all PASS (existing `cw_changes_a_word` etc. still green).

- [ ] **Step 6: Commit**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-server/tests/editing.rs
git commit -m "$(printf 'fix(core): word motions step by grapheme cluster\n\nw/b/e iterate grapheme clusters and classify by base character, so word\nboundaries are correct inside multibyte words.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 8: Virtual desired column for `j`/`k`/`$` (and `~`)

Make vertical motion remember a *screen* column and land on the nearest grapheme.

**Files:**
- Modify: `crates/nxvim-core/src/editor.rs`
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Write the failing test**

In `crates/nxvim-server/tests/editing.rs`, add:

```rust
#[tokio::test]
async fn vertical_motion_keeps_screen_column_across_wide_chars() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i日本x<Esc>");  // screen columns: 日@0, 本@2, x@4
    feed(&rpc, "oabcdef<Esc>"); // an ASCII line below it
    feed(&rpc, "gg");           // line 1, on 日
    feed(&rpc, "l");            // → 本, byte col 3, screen col 2
    assert_eq!(cursor(&rpc).await, (1, 3));
    feed(&rpc, "j");            // down: screen col 2 → byte col 2 ('c')
    assert_eq!(cursor(&rpc).await, (2, 2));
    feed(&rpc, "k");            // back up: screen col 2 → byte col 3 (本)
    assert_eq!(cursor(&rpc).await, (1, 3));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p nxvim-server --test editing vertical_motion_keeps_screen_column_across_wide_chars`
Expected: FAIL — `desired_col` is a byte column, so `j` lands on byte 2 of line 1 region math incorrectly / the wrong cell mapping.

- [ ] **Step 3: Make `desired_col` a virtual column where it is set**

In `crates/nxvim-core/src/editor.rs`, in `input()`, replace:

```rust
        if !self.preserve_desired {
            self.desired_col = self.cursor.col;
            self.desired_eol = self.eol_request;
        }
```

with:

```rust
        if !self.preserve_desired {
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = self.eol_request;
        }
```

In `command()`, replace:

```rust
        self.desired_col = self.cursor.col;
```

with:

```rust
        self.desired_col = self.cursor_virtcol();
```

- [ ] **Step 4: Make `settle_desired_col` map a virtual column back to a byte column**

Replace the whole `settle_desired_col` method:

```rust
/// Place the cursor on the current line at the remembered desired *virtual*
/// column (or end-of-line when `$`-sticky), clamped to the line and a grapheme
/// boundary.
fn settle_desired_col(&mut self, allow_eol: bool) {
    let s = self.buffer.line(self.cursor.line);
    let max_byte = if allow_eol {
        s.len()
    } else {
        unicode::prev_grapheme(&s, s.len())
    };
    let target = if self.desired_eol {
        max_byte
    } else {
        unicode::byte_at_virtcol(&s, self.desired_col, unicode::TABSTOP).min(max_byte)
    };
    self.cursor.col = unicode::floor_grapheme(&s, target);
}
```

- [ ] **Step 5: Make `~` advance by grapheme**

Replace the whole `toggle_case` method:

```rust
fn toggle_case(&mut self, count: usize) {
    if self.cursor.col >= self.line_len() {
        return;
    }
    self.push_undo();
    for _ in 0..count {
        if self.cursor.col >= self.line_len() {
            break;
        }
        let idx = self.cursor_char();
        let c = self.char_at(idx);
        let swapped: String = if c.is_uppercase() {
            c.to_lowercase().collect()
        } else {
            c.to_uppercase().collect()
        };
        self.buffer.text.remove(idx..idx + c.len_utf8());
        self.buffer.text.insert(idx, &swapped);
        let s = self.buffer.line(self.cursor.line);
        self.cursor.col = unicode::next_grapheme(&s, self.cursor.col);
    }
    self.buffer.modified = true;
    self.clamp_cursor();
}
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p nxvim-server --test editing vertical_motion_keeps_screen_column_across_wide_chars`
Expected: PASS.

- [ ] **Step 7: Run the whole editing suite (regression)**

Run: `cargo test -p nxvim-server --test editing`
Expected: all PASS (including `vertical_motion_preserves_desired_column` and `dollar_sticks_to_end_of_line_through_j`, which are ASCII and unaffected).

- [ ] **Step 8: Commit**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-server/tests/editing.rs
git commit -m "$(printf 'fix(core): j/k/$ remember a virtual (screen) column\n\ndesired_col becomes a virtual column; settle_desired_col maps it back to\nthe nearest grapheme, so vertical motion is stable across wide/tab text.\n~ advances by grapheme.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 9: TUI cursor placement and tab expansion

The text already renders width-correctly via ratatui; place the terminal cursor at `cursor_screen_col` and expand tabs so painted widths match core's virtual columns. (No PTY/e2e harness exists yet, so this task is verified by build + the existing core-level screen-column tests from Task 3; the change is mechanical.)

**Files:**
- Modify: `crates/nxvim-tui/Cargo.toml`
- Modify: `crates/nxvim-tui/src/lib.rs`

- [ ] **Step 1: Add `unicode-width` to the TUI crate**

In `crates/nxvim-tui/Cargo.toml`, under `[dependencies]` (after `ratatui.workspace = true`), add:

```toml
unicode-width.workspace = true
```

- [ ] **Step 2: Import the trait and define the tab stop**

In `crates/nxvim-tui/src/lib.rs`, add to the imports near the top:

```rust
use unicode_width::UnicodeWidthChar;
```

and add a constant next to `CHROME_ROWS`:

```rust
/// Tab stop width in cells. Must match `nxvim_core::unicode::TABSTOP` so the
/// painted text lines up with the server's reported screen columns.
const TABSTOP: usize = 8;
```

- [ ] **Step 3: Mirror the `cursor_screen_col` field**

In the client-side `struct View`, add the field after `cursor_col: u16,`:

```rust
    cursor_col: u16,
    cursor_screen_col: u16,
```

In `View::update`, after the line that sets `self.cursor_col`, add:

```rust
        self.cursor_col = map_u64(map, "cursor_col") as u16;
        self.cursor_screen_col = map_u64(map, "cursor_screen_col") as u16;
```

- [ ] **Step 4: Place the terminal cursor at the screen column**

In `render`, in the non-command branch, change:

```rust
        frame.set_cursor_position((text_area.x + view.cursor_col, text_area.y + view.cursor_row));
```

to:

```rust
        frame.set_cursor_position((
            text_area.x + view.cursor_screen_col,
            text_area.y + view.cursor_row,
        ));
```

- [ ] **Step 5: Expand tabs when rendering text lines**

In `render_text`, build the `Text` from tab-expanded lines:

```rust
fn render_text(frame: &mut Frame, area: Rect, view: &View) {
    let text = Text::from(
        view.lines
            .iter()
            .map(|l| Line::from(expand_tabs(l)))
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}

/// Expand tabs to spaces at `TABSTOP`, tracking display width so wide characters
/// before a tab advance the column correctly. No-op for tab-free lines.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + TABSTOP);
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let spaces = TABSTOP - (col % TABSTOP);
            out.push_str(&" ".repeat(spaces));
            col += spaces;
        } else {
            out.push(ch);
            col += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}
```

- [ ] **Step 6: Build the whole workspace**

Run: `cargo build --workspace`
Expected: compiles cleanly.

- [ ] **Step 7: Commit**

```bash
git add crates/nxvim-tui/Cargo.toml crates/nxvim-tui/src/lib.rs
git commit -m "$(printf 'feat(tui): place cursor at screen column and expand tabs\n\nUse cursor_screen_col for terminal cursor placement and expand tabs to\nspaces at tabstop 8 so painted widths match the server.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Task 10: Final verification and docs

**Files:**
- Modify: `docs/architecture.md`

- [ ] **Step 1: Update the architecture doc**

In `docs/architecture.md`, in the *Text model* section, replace the parenthetical:

```
(Display still assumes one cell per byte/char — no wide-char or tab-width
handling yet — so cursor placement for non-ASCII text is approximate for now.)
```

with:

```
Motion steps by **grapheme cluster** and the cursor's display column is computed
as a **virtual column** (wide characters via `unicode-width`, tabs expanded to a
fixed `tabstop` of 8), carried in the `View` as `cursor_screen_col`. `cursor.col`
remains a byte offset (what `nvim_win_get_cursor` returns); the TUI expands tabs
when painting so glyphs line up with that virtual column.
```

In the *Not yet implemented (roadmap)* list, remove the line:

```
- Wide-character / tab-width aware display and cursor placement.
```

(Optionally note that `:set tabstop` — making the tab width configurable — is still pending, under the existing options roadmap line.)

- [ ] **Step 2: Format**

Run: `cargo fmt --all`
Expected: no diff, or formatting applied. If applied, re-stage.

- [ ] **Step 3: Lint**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Full test suite**

Run: `cargo test --workspace`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add docs/architecture.md
git commit -m "$(printf 'docs: record grapheme/virtual-column text handling\n\nUpdate the text-model caveat and roadmap now that wide-char/tab-aware\nmovement and display are implemented.\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Self-review

**Spec coverage:**
- Grapheme movement (`h`/`l`/`x`/`r`/insert edits) → Tasks 4, 5, 6. ✅
- Wide-char + tab display column → Tasks 1, 3, 9. ✅
- `j`/`k`/`$` virtual-column stability → Task 8. ✅
- Word motions over graphemes → Task 7. ✅
- `cursor.col` stays byte / `nvim_win_get_cursor` unchanged → Tasks 3, 8 (no change to the API arm). ✅
- TUI thin, tabs rendered at the same tabstop → Task 9. ✅
- Non-goals (options/`:set tabstop`, bidi, normalization) → untouched; `TABSTOP` constant noted in Tasks 1, 9, 10. ✅
- Tests incl. a redraw-reading helper → `view_u64` in Task 3; behavioral tests in Tasks 3–8. ✅

**Type/name consistency:** `unicode::{next_grapheme, prev_grapheme, floor_grapheme, virtcol, byte_at_virtcol, TABSTOP}` defined in Task 1 and used verbatim thereafter. Editor helpers `next_grapheme_idx`, `prev_grapheme_idx`, `grapheme_floor_abs`, `grapheme_ceil_abs`, `cursor_virtcol`, `advance_graphemes` defined in Tasks 2/5 and used consistently. `View.cursor_screen_col` / redraw key `"cursor_screen_col"` / TUI `cursor_screen_col` all match. ✅

**Placeholder scan:** no TBD/TODO; every code step shows complete code. ✅
