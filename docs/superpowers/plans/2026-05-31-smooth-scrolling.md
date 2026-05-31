# Smooth (animated) scrolling — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the scroll commands `<C-d>`, `<C-u>`, `<C-f>`, `<C-b>` *slide* the viewport over ~80–160ms (neoscroll.nvim style) instead of teleporting, with the animation driven entirely by the client.

**Architecture:** The server stays authoritative and applies each scroll instantly, but attaches a self-contained `scroll` descriptor to the `redraw` (the from/to viewport+cursor lines, a duration hint, and the band of buffer lines spanning the slide). The client animates against its *local* clock by interpolating `top`/cursor and slicing the band per frame. The steady-state render path is unchanged; only animation reads the new payload. This keeps `nxvim-core` pure/synchronous, keeps the client the only place that knows about *time*, and lets a future GUI render the same descriptor at pixel resolution.

**Tech Stack:** Rust, tokio (single-threaded runtimes), ratatui/crossterm (TUI client), msgpack-RPC (`rmpv`). Black-box integration tests in `crates/nxvim-server/tests/editing.rs`.

**Design source:** `docs/superpowers/specs/2026-05-31-smooth-scrolling-design.md`. Two intentional deviations from the spec, both noted below: (1) the over-scan window lives *inside* the `scroll` payload rather than widening the main `View`, lowering regression risk; (2) the scroll commands are made to move `top` directly (vim-faithful) because today they don't, which would leave the animation with nothing to slide.

---

## File Structure

- `crates/nxvim-core/src/editor.rs` — **modify.** Add `PendingScroll` type + two `Editor` fields; rewrite `scroll_half`/`scroll_page` to move the viewport via a new `scroll_by`; record the gesture at the end of `input`; expose + clear it from `view`.
- `crates/nxvim-core/src/view.rs` — **modify.** Add `ScrollAnim` type and `View::scroll`; build the self-contained window (lines + selection) from `PendingScroll`. Generalize the line/selection builders to an arbitrary `[base, base+count)` range so they serve both the viewport and the window.
- `crates/nxvim-server/src/lib.rs` — **modify.** Serialize `scroll` into the `redraw` notification map.
- `crates/nxvim-server/tests/editing.rs` — **modify.** Add a `redraw`-observation helper and the protocol tests.
- `crates/nxvim-tui/src/lib.rs` — **modify.** Parse the `scroll` payload; add an animation tick to the `select!` loop; interpolate + slice frames; settle/interrupt.

---

## Task 1: Redraw-observation test helper

The black-box suite has no way to read `redraw` notifications yet (tests use `_incoming`). Add a deterministic helper and a baseline assertion to lock the tool in before we change behavior.

**Files:**
- Test: `crates/nxvim-server/tests/editing.rs`

- [ ] **Step 1: Add the helper and accessors** near the other helpers (after the `cursor` fn, around line 90).

```rust
/// Feed `keys`, then deterministically return the `redraw` map the server
/// emitted *for that input*. Works because the server writes one `redraw`
/// after each handled message, in order: we clear anything stale, send the
/// input as a request, then await a second cheap request as a barrier. By the
/// time that second response resolves, the input's `redraw` notification is
/// already queued in `incoming` (it was written before the barrier's
/// response), so the first redraw we drain is the one we want.
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // discard stale redraws
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => return map,
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue,
            Err(_) => panic!("no redraw arrived for {keys:?}"),
        }
    }
}

/// Look up a top-level key in a redraw map.
fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.as_str() == Some(key)).map(|(_, v)| v)
}

/// Number of entries in the redraw's `lines` array.
fn lines_len(map: &[(Value, Value)]) -> usize {
    field(map, "lines").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
}

/// The `scroll` sub-map, or `None` when the redraw carries no scroll gesture.
fn scroll<'a>(map: &'a [(Value, Value)]) -> Option<&'a Vec<(Value, Value)>> {
    match field(map, "scroll") {
        Some(Value::Map(m)) => Some(m),
        _ => None,
    }
}

/// Read a u64 field out of the `scroll` sub-map.
fn scroll_u64(map: &[(Value, Value)], key: &str) -> u64 {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or_else(|| panic!("scroll.{key} missing"))
}

/// Number of entries in `scroll.lines`.
fn scroll_lines_len(map: &[(Value, Value)]) -> usize {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("lines"))
        .and_then(|(_, v)| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Write `n` lines ("line1".."lineN") to a temp file and return its path string.
fn write_n_lines(tag: &str, n: usize) -> String {
    let path = temp_path(tag);
    let body: String = (1..=n).map(|i| format!("line{i}\n")).collect();
    std::fs::write(&path, body).expect("write temp file");
    path.to_string_lossy().into_owned()
}
```

- [ ] **Step 2: Add a baseline test** at the end of the file.

```rust
#[tokio::test]
async fn redraw_has_no_scroll_for_plain_motion() {
    let path = write_n_lines("noscroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "j").await;

    assert!(scroll(&map).is_none(), "a plain `j` must carry no scroll gesture");
    assert_eq!(lines_len(&map), 24, "viewport stays one screen tall");
}
```

- [ ] **Step 3: Run it (green — establishes the observation tool).**

Run: `cargo test -p nxvim-server --test editing redraw_has_no_scroll_for_plain_motion`
Expected: PASS. (No product code changed yet; this proves the helper observes the existing redraw shape, where `scroll` is simply absent.)

- [ ] **Step 4: Commit.**

```bash
git add crates/nxvim-server/tests/editing.rs
git commit -m "test: add redraw-observation helper for scroll gestures"
```

---

## Task 2: Server-side scroll descriptor (core + view + serialization)

Because the suite is black-box, the *only* observable contract is the `redraw` payload — so the core viewport change, the `View` projection, and the server serialization land together as one behavior, verified end to end. Write the tests first.

**Files:**
- Test: `crates/nxvim-server/tests/editing.rs`
- Modify: `crates/nxvim-core/src/editor.rs`
- Modify: `crates/nxvim-core/src/view.rs`
- Modify: `crates/nxvim-server/src/lib.rs:184-237` (the `redraw` fn)

- [ ] **Step 1: Write the failing tests** at the end of `crates/nxvim-server/tests/editing.rs`.

```rust
#[tokio::test]
async fn ctrl_d_emits_half_page_scroll() {
    let path = write_n_lines("cd", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "<C-d>").await;

    // Viewport height 24 → half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 12);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
    assert_eq!(scroll_u64(&map, "base_line"), 0);
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8, within [80,160]
    // Window = |to-from| + height = 12 + 24.
    assert_eq!(scroll_lines_len(&map), 36);
}

#[tokio::test]
async fn ctrl_f_emits_full_page_scroll() {
    let path = write_n_lines("cf", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "<C-f>").await;

    // Full page = height - 2 = 22.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 22);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160); // 22*8=176, clamped to 160
    assert_eq!(scroll_lines_len(&map), 46); // 22 + 24
}

#[tokio::test]
async fn ctrl_u_at_top_is_not_a_scroll() {
    let path = write_n_lines("cu", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Already at the top: top can't move up, so no slide.
    let map = redraw_after(&rpc, &mut incoming, "<C-u>").await;

    assert!(scroll(&map).is_none(), "no viewport movement → no scroll gesture");
}

#[tokio::test]
async fn scroll_window_pads_past_end_of_buffer() {
    let path = write_n_lines("eof", 30);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "<C-f>").await;

    assert_eq!(scroll_u64(&map, "to_top"), 22);
    assert_eq!(scroll_lines_len(&map), 46); // window length is fixed regardless of EOF
    // The 30-line buffer fills rows 0..30; the rest are "~".
    let s = scroll(&map).unwrap();
    let lines = s.iter().find(|(k, _)| k.as_str() == Some("lines")).unwrap().1.as_array().unwrap();
    assert_eq!(lines.last().and_then(Value::as_str), Some("~"));
}
```

- [ ] **Step 2: Run the tests to verify they fail.**

Run: `cargo test -p nxvim-server --test editing ctrl_d_emits_half_page_scroll`
Expected: FAIL — `scroll` is absent, so `scroll_u64`/`scroll_lines_len` panic with "scroll present".

- [ ] **Step 3: Add the `PendingScroll` type and `Editor` fields** in `crates/nxvim-core/src/editor.rs`.

Add this type immediately above `pub struct Editor` (just after the `MoveAxis` block, ~line 61):

```rust
/// A recorded scroll gesture (`<C-d>` / `<C-u>` / `<C-f>` / `<C-b>`) that moved
/// the viewport, handed to the client so it can animate the slide. Lines/columns
/// are absolute buffer lines; `duration_ms` is a suggested pacing the client may
/// clamp or ignore.
#[derive(Clone, Copy)]
pub(crate) struct PendingScroll {
    pub from_top: usize,
    pub to_top: usize,
    pub from_cursor: usize,
    pub to_cursor: usize,
    pub duration_ms: u64,
}
```

Add two fields to `struct Editor` (just after `visual_anchor: Cursor,`, ~line 98):

```rust
    /// Set by a scroll command at the moment it fires: `(top, cursor.line)`
    /// *before* the move. Consumed at the end of `input` to build `pending_scroll`.
    scroll_from: Option<(usize, usize)>,
    /// The scroll gesture from the most recent input, projected into the next
    /// `View` and then cleared (so it animates exactly once).
    pending_scroll: Option<PendingScroll>,
```

Initialize both in `with_buffer` (just after `visual_anchor: Cursor::default(),`, ~line 137):

```rust
            scroll_from: None,
            pending_scroll: None,
```

- [ ] **Step 4: Make scroll commands move the viewport and record the gesture.** In `crates/nxvim-core/src/editor.rs`, replace the existing `scroll_half` and `scroll_page` (lines ~1412-1430) with:

```rust
    fn scroll_half(&mut self, down: bool) {
        let half = (self.text_height() / 2).max(1) as i64;
        self.scroll_by(if down { half } else { -half });
    }

    fn scroll_page(&mut self, down: bool) {
        let page = self.text_height().saturating_sub(2).max(1) as i64;
        self.scroll_by(if down { page } else { -page });
    }

    /// Scroll the viewport by `delta` lines, vim-style: move both `top` and the
    /// cursor together so the cursor keeps its screen row. Records the pre-move
    /// `(top, cursor.line)` in `scroll_from`; `input` turns that into a
    /// `PendingScroll` if `top` actually changed.
    fn scroll_by(&mut self, delta: i64) {
        self.scroll_from = Some((self.top, self.cursor.line));
        let last = self.buffer.line_count().saturating_sub(1) as i64;
        self.top = (self.top as i64 + delta).clamp(0, last) as usize;
        self.move_vertical(delta, false);
        self.clamp_cursor();
    }
```

- [ ] **Step 5: Build the `PendingScroll` at the end of `input`.** In `crates/nxvim-core/src/editor.rs`, in `pub fn input(&mut self, key: Key)`, replace the trailing `self.ensure_visible();` (line ~173) with:

```rust
        self.ensure_visible();

        // If this key was a scroll command that actually moved the viewport,
        // record the gesture for the client to animate.
        if let Some((from_top, from_cursor)) = self.scroll_from.take() {
            if from_top != self.top {
                let dist = from_top.abs_diff(self.top) as u64;
                self.pending_scroll = Some(PendingScroll {
                    from_top,
                    to_top: self.top,
                    from_cursor,
                    to_cursor: self.cursor.line,
                    duration_ms: (dist * 8).clamp(80, 160),
                });
            }
        }
```

- [ ] **Step 6: Expose + clear the gesture from `view`.** In `crates/nxvim-core/src/editor.rs`, replace the body of `pub fn view` (lines ~191-194) with:

```rust
    pub fn view(&mut self, width: usize, height: usize) -> View {
        self.resize(width, height);
        let view = View::from_editor(self);
        self.pending_scroll = None; // animate exactly once
        view
    }
```

And add a crate-visible accessor next to `dims` (after `pub(crate) fn dims`, ~line 198):

```rust
    pub(crate) fn pending_scroll(&self) -> Option<PendingScroll> {
        self.pending_scroll
    }
```

- [ ] **Step 7: Add `ScrollAnim` + `View::scroll` and the window builders.** Replace the entire contents of `crates/nxvim-core/src/view.rs` with:

```rust
//! The renderable view of the editor: semantic regions, not a baked grid.
//!
//! The core no longer lays out a flat screen (status/command lines are not
//! painted into text rows). Instead it produces a [`View`] describing *what* to
//! show in each region, and the client arranges those regions with its own
//! widgets. This keeps layout and styling a UI concern while the core stays the
//! single source of truth for content, scrolling, and cursor placement.
//!
//! Columns are byte offsets (ropey's native metric and vim's column model);
//! `cursor_screen_col` additionally carries the cursor's screen-cell column,
//! accounting for wide characters and tabs.

use crate::editor::Editor;
use crate::mode::Mode;
use crate::unicode;

/// A scroll gesture for the client to animate. Self-contained: it carries its
/// own band of rendered lines (`lines`) and selection spans covering every row
/// visible during the slide, anchored at `base_line`. The client interpolates
/// `from`→`to` against its local clock and slices `lines` per frame; the main
/// `View` fields stay the *destination* viewport for clients that don't animate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnim {
    pub from_top: usize,
    pub to_top: usize,
    pub from_cursor: usize,
    pub to_cursor: usize,
    pub duration_ms: u64,
    /// Buffer-line index of `lines[0]` (= `min(from_top, to_top)`).
    pub base_line: usize,
    /// `|to_top - from_top| + height` rows starting at `base_line`, "~"-padded
    /// past end of buffer.
    pub lines: Vec<String>,
    /// Selection spans aligned with `lines` (same length).
    pub selection: Vec<Option<(usize, usize)>>,
}

/// A snapshot of everything a client needs to draw a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Visible text rows (the text viewport). Empty rows below the buffer are
    /// the literal string `"~"`, as in vim.
    pub lines: Vec<String>,
    /// Cursor position within the text viewport (row relative to the top of the
    /// visible window; `col` is a byte/column offset within the line).
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// Cursor's screen-cell column on its line (wide-char and tab aware). Used
    /// by clients to place the terminal cursor; `cursor_col` stays the byte
    /// column for the ruler and `nvim_win_get_cursor`.
    pub cursor_screen_col: usize,
    /// Uppercase mode name for the status line, e.g. `"NORMAL"`.
    pub mode_label: String,
    /// True while in command-line mode; the cursor then belongs to the command
    /// region, which the client owns.
    pub command_mode: bool,
    /// Command-line contents (text after the leading `:`).
    pub cmdline: String,
    /// Transient status message (shown on the command line when not typing one).
    pub message: String,
    /// File name for the status line (`"[No Name]"` when unset).
    pub file_name: String,
    pub modified: bool,
    /// 1-based cursor line, for the status-line ruler.
    pub cursor_line: usize,
    /// Per visible row (aligned with `lines`), the half-open screen-column span
    /// `[start, end)` to paint as the visual-mode selection, or `None` when that
    /// row carries no selection. All `None` outside visual modes. `end` may
    /// exceed the row's text width to mark a selected newline (one extra cell) or
    /// to fill a linewise selection to the viewport edge.
    pub selection: Vec<Option<(usize, usize)>>,
    /// Present only on a redraw caused by a scroll command that moved the
    /// viewport; carries the data a client needs to animate the slide.
    pub scroll: Option<ScrollAnim>,
}

impl View {
    pub(crate) fn from_editor(ed: &Editor) -> View {
        let (width, height) = ed.dims();
        let line_count = ed.buffer.line_count();

        let lines = window_lines(ed, ed.top, height, line_count);
        let selection = selection_spans(ed, width, line_count, ed.top, height);

        let scroll = ed.pending_scroll().map(|ps| {
            let base_line = ps.from_top.min(ps.to_top);
            let count = ps.from_top.abs_diff(ps.to_top) + height;
            ScrollAnim {
                from_top: ps.from_top,
                to_top: ps.to_top,
                from_cursor: ps.from_cursor,
                to_cursor: ps.to_cursor,
                duration_ms: ps.duration_ms,
                base_line,
                lines: window_lines(ed, base_line, count, line_count),
                selection: selection_spans(ed, width, line_count, base_line, count),
            }
        });

        let file_name = ed
            .buffer
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[No Name]".to_string());

        let cursor_screen_col = {
            let line = ed.buffer.line(ed.cursor.line);
            unicode::virtcol(&line, ed.cursor.col, unicode::TABSTOP)
        };

        View {
            lines,
            cursor_row: ed
                .cursor
                .line
                .saturating_sub(ed.top)
                .min(height.saturating_sub(1)),
            cursor_col: ed.cursor.col,
            cursor_screen_col,
            mode_label: ed.mode.label().to_string(),
            command_mode: ed.mode == Mode::Command,
            cmdline: ed.cmdline.clone(),
            message: ed.message.clone(),
            file_name,
            modified: ed.buffer.modified,
            cursor_line: ed.cursor.line + 1,
            selection,
            scroll,
        }
    }
}

/// Build `count` rendered rows starting at buffer line `base`, padding rows past
/// the end of the buffer with `"~"` (as vim shows below the last line).
fn window_lines(ed: &Editor, base: usize, count: usize, line_count: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(count);
    for row in 0..count {
        let idx = base + row;
        if idx < line_count {
            lines.push(ed.buffer.line(idx));
        } else {
            lines.push("~".to_string());
        }
    }
    lines
}

/// Compute, for each of the `count` rows starting at buffer line `base`, the
/// half-open screen-column span to highlight as the visual selection (or
/// `None`). Returns all-`None` outside visual modes.
fn selection_spans(
    ed: &Editor,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
) -> Vec<Option<(usize, usize)>> {
    let mut spans = vec![None; count];
    if !ed.mode.is_visual() {
        return spans;
    }

    // Order the two ends of the selection by buffer position.
    let a = ed.visual_anchor();
    let c = ed.cursor;
    let (start, end) = if (a.line, a.col) <= (c.line, c.col) {
        (a, c)
    } else {
        (c, a)
    };
    let linewise = ed.mode == Mode::VisualLine;

    for (row, span) in spans.iter_mut().enumerate() {
        let buf_line = base + row;
        if buf_line >= line_count || buf_line < start.line || buf_line > end.line {
            continue;
        }
        let text = ed.buffer.line(buf_line);

        if linewise {
            // Whole line, filled to the viewport edge — as vim paints it.
            *span = Some((0, width));
            continue;
        }

        // Charwise: clip the inclusive [start, end] region to this row.
        let lo = if buf_line == start.line { start.col } else { 0 };
        let start_col = unicode::virtcol(&text, lo, unicode::TABSTOP);
        let end_col = if buf_line == end.line {
            // Include the grapheme under the trailing cursor.
            let hi = unicode::next_grapheme(&text, end.col.min(text.len()));
            unicode::virtcol(&text, hi, unicode::TABSTOP)
        } else {
            // The selection continues onto the next line: highlight the text and
            // one extra cell standing in for the selected newline.
            unicode::virtcol(&text, text.len(), unicode::TABSTOP) + 1
        };
        *span = Some((start_col, end_col));
    }

    spans
}
```

- [ ] **Step 8: Serialize `scroll` into the redraw.** In `crates/nxvim-server/src/lib.rs`, inside `fn redraw`, just before `let map = vec![` (line ~203), build the scroll value:

```rust
        let scroll = match &view.scroll {
            Some(s) => {
                let scroll_lines =
                    Value::Array(s.lines.iter().map(|l| Value::from(l.as_str())).collect());
                let scroll_selection = Value::Array(
                    s.selection
                        .iter()
                        .map(|sp| match sp {
                            Some((start, end)) => Value::Array(vec![
                                Value::from(*start as u64),
                                Value::from(*end as u64),
                            ]),
                            None => Value::Nil,
                        })
                        .collect(),
                );
                Value::Map(vec![
                    (Value::from("from_top"), Value::from(s.from_top as u64)),
                    (Value::from("to_top"), Value::from(s.to_top as u64)),
                    (Value::from("from_cursor"), Value::from(s.from_cursor as u64)),
                    (Value::from("to_cursor"), Value::from(s.to_cursor as u64)),
                    (Value::from("duration_ms"), Value::from(s.duration_ms)),
                    (Value::from("base_line"), Value::from(s.base_line as u64)),
                    (Value::from("lines"), scroll_lines),
                    (Value::from("selection"), scroll_selection),
                ])
            }
            None => Value::Nil,
        };
```

Then add this entry to the `map` vec (after the `selection` entry, ~line 233):

```rust
            (Value::from("scroll"), scroll),
```

- [ ] **Step 9: Run the new tests to verify they pass.**

Run: `cargo test -p nxvim-server --test editing`
Expected: PASS for `ctrl_d_emits_half_page_scroll`, `ctrl_f_emits_full_page_scroll`, `ctrl_u_at_top_is_not_a_scroll`, `scroll_window_pads_past_end_of_buffer`, and the existing suite (including `redraw_has_no_scroll_for_plain_motion`).

- [ ] **Step 10: Lint.**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 11: Commit.**

```bash
git add crates/nxvim-core/src/editor.rs crates/nxvim-core/src/view.rs \
        crates/nxvim-server/src/lib.rs crates/nxvim-server/tests/editing.rs
git commit -m "feat: emit scroll-animation descriptor on viewport scrolls

Scroll commands now move the viewport (top) directly, vim-style, and attach a
self-contained scroll descriptor (from/to top+cursor, duration, and the band of
lines spanning the slide) to the redraw. The client will animate it."
```

---

## Task 3: Client — parse the scroll payload

Mirror the new `scroll` field into the client's `View`. No behavior change yet; this is the data plumbing the animation loop will consume. The TUI is UI/time code, so per the project's testing philosophy it's verified by build + clippy here and manually at the end of Task 4 (no unit tests).

**Files:**
- Modify: `crates/nxvim-tui/src/lib.rs`

- [ ] **Step 1: Add imports** at the top of `crates/nxvim-tui/src/lib.rs` (after the existing `use` block, ~line 25):

```rust
use std::time::Duration;
```

- [ ] **Step 2: Add the `ScrollData` struct** just below the client `View` struct (after its closing brace, ~line 128):

```rust
/// The scroll gesture mirrored from the server's redraw, ready to animate.
/// Line/cursor positions are kept as `f32` for interpolation; `lines`/`selection`
/// are the band covering the slide, anchored at `base_line`.
#[derive(Clone)]
struct ScrollData {
    from_top: f32,
    to_top: f32,
    from_cursor: f32,
    to_cursor: f32,
    duration: Duration,
    base_line: usize,
    lines: Vec<String>,
    selection: Vec<Option<(u16, u16)>>,
}
```

- [ ] **Step 3: Add the field to the client `View`** struct (after `selection: Vec<Option<(u16, u16)>>,`, ~line 127):

```rust
    scroll: Option<ScrollData>,
```

- [ ] **Step 4: Parse it in `View::update`.** Add, at the end of `fn update` (after the `self.selection = ...` assignment, ~line 170):

```rust
        self.scroll = match map_get(map, "scroll") {
            Some(Value::Map(s)) => Some(ScrollData {
                from_top: map_u64(s, "from_top") as f32,
                to_top: map_u64(s, "to_top") as f32,
                from_cursor: map_u64(s, "from_cursor") as f32,
                to_cursor: map_u64(s, "to_cursor") as f32,
                duration: Duration::from_millis(map_u64(s, "duration_ms")),
                base_line: map_u64(s, "base_line") as usize,
                lines: map_get(s, "lines")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                selection: map_get(s, "selection")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|v| match v.as_array() {
                                Some(pair) if pair.len() == 2 => Some((
                                    pair[0].as_u64().unwrap_or(0) as u16,
                                    pair[1].as_u64().unwrap_or(0) as u16,
                                )),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }),
            _ => None,
        };
```

- [ ] **Step 5: Build (warnings expected).**

Run: `cargo build -p nxvim-tui`
Expected: compiles successfully. It **will** emit `dead_code` warnings because `ScrollData` and `View::scroll` are written but not yet read — that is correct; Task 4 consumes them. Do **not** run `clippy -D warnings` here (it would fail on those warnings) and do **not** silence them with `#[allow]`. The strict clippy gate runs in Task 4 Step 5 once the fields are used.

- [ ] **Step 6: Commit.**

```bash
git add crates/nxvim-tui/src/lib.rs
git commit -m "feat(tui): parse scroll-animation descriptor from redraw"
```

---

## Task 4: Client — animate the slide

Drive the animation from the client's local clock: when a redraw carries a `scroll`, interpolate `top`/cursor over its duration, slicing the band per frame; settle on the destination; let any new redraw supersede (the interrupt path).

**Files:**
- Modify: `crates/nxvim-tui/src/lib.rs`

- [ ] **Step 1: Add the `Animation` struct and a lerp helper.** Add the import for `Instant` by changing the Step-1 import from Task 3 to:

```rust
use std::time::{Duration, Instant};
use tokio::time::sleep;
```

Add this struct just below `ScrollData` (~line 140):

```rust
/// An in-flight scroll animation, driven by the client's local clock.
struct Animation {
    from_top: f32,
    to_top: f32,
    from_cursor: f32,
    to_cursor: f32,
    start: Instant,
    duration: Duration,
    base_line: usize,
    lines: Vec<String>,
    selection: Vec<Option<(u16, u16)>>,
}

impl Animation {
    fn new(s: &ScrollData) -> Self {
        Animation {
            from_top: s.from_top,
            to_top: s.to_top,
            from_cursor: s.from_cursor,
            to_cursor: s.to_cursor,
            start: Instant::now(),
            duration: s.duration,
            base_line: s.base_line,
            lines: s.lines.clone(),
            selection: s.selection.clone(),
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
```

- [ ] **Step 2: Wire the animation into the event loop.** In `fn event_loop`, replace the `let mut view = View::default();` line (~line 66) with:

```rust
    let mut view = View::default();
    let mut anim: Option<Animation> = None;
```

Then replace the whole `loop { tokio::select! { ... } }` body (lines ~69-101) with:

```rust
    loop {
        tokio::select! {
            term_event = term_events.next() => match term_event {
                Some(Ok(Event::Key(key))) => {
                    if key.kind != KeyEventKind::Release {
                        if let Some(notation) = encode_key(key) {
                            rpc.notify("nvim_input", vec![Value::from(notation.as_str())]);
                        }
                    }
                }
                Some(Ok(Event::Resize(w, h))) => {
                    rpc.notify(
                        "nvim_ui_try_resize",
                        vec![Value::from(w as u64), Value::from(text_height(h) as u64)],
                    );
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            message = incoming.recv() => match message {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "redraw" => {
                        view.update(&params);
                        // A scroll gesture arms a fresh animation; any other
                        // redraw (e.g. the result of a keypress) supersedes and
                        // clears the in-flight one — the interrupt path.
                        anim = view.scroll.as_ref().map(Animation::new);
                        terminal.draw(|frame| render(frame, &view, anim.as_ref()))?;
                    }
                    "nxvim_exit" => break,
                    _ => {}
                },
                Some(Incoming::Request { id, .. }) => rpc.respond(id, Ok(Value::Nil)),
                None => break,
            },
            // Animation frame tick (~60fps). Disabled when nothing is animating,
            // so the future is never even created in the idle case.
            _ = sleep(Duration::from_millis(16)), if anim.is_some() => {
                if anim.as_ref().is_some_and(|a| a.start.elapsed() >= a.duration) {
                    anim = None; // settle: render the destination view below
                }
                terminal.draw(|frame| render(frame, &view, anim.as_ref()))?;
            },
        }
    }
```

- [ ] **Step 3: Refactor `render` to interpolate when animating.** Replace the existing `fn render` (lines ~175-197) with:

```rust
/// Lay out the three regions and render each with its own widget. When `anim`
/// is present and unfinished, the text area shows an interpolated slice of the
/// scroll band instead of the static viewport.
fn render(frame: &mut Frame, view: &View, anim: Option<&Animation>) {
    let regions = Layout::vertical([
        Constraint::Min(1),    // text area
        Constraint::Length(1), // status line
        Constraint::Length(1), // command line
    ])
    .split(frame.area());
    let (text_area, status_area, cmd_area) = (regions[0], regions[1], regions[2]);

    let height = text_area.height as usize;
    let frame_lines: Vec<String>;
    let frame_sel: Vec<Option<(u16, u16)>>;
    let cursor_row: u16;

    match anim {
        Some(a) => {
            let raw = (a.start.elapsed().as_secs_f32() / a.duration.as_secs_f32()).clamp(0.0, 1.0);
            let t = 1.0 - (1.0 - raw).powi(3); // ease-out cubic
            let top = lerp(a.from_top, a.to_top, t).round() as usize;
            let cur = lerp(a.from_cursor, a.to_cursor, t).round() as usize;
            let off = top.saturating_sub(a.base_line);
            frame_lines = a.lines.iter().skip(off).take(height).cloned().collect();
            frame_sel = a.selection.iter().skip(off).take(height).copied().collect();
            cursor_row = cur.saturating_sub(top) as u16;
        }
        None => {
            frame_lines = view.lines.clone();
            frame_sel = view.selection.clone();
            cursor_row = view.cursor_row;
        }
    }

    render_text(frame, text_area, &frame_lines, &frame_sel);
    render_status(frame, status_area, view);
    render_command(frame, cmd_area, view);

    if view.command_mode {
        let col = cmd_area.x + 1 + view.cmdline.chars().count() as u16;
        frame.set_cursor_position((col, cmd_area.y));
    } else {
        frame.set_cursor_position((
            text_area.x + view.cursor_screen_col,
            text_area.y + cursor_row,
        ));
    }
}
```

- [ ] **Step 4: Make `render_text` take lines + selection directly** (so both the static and animated paths share it). Replace the existing `fn render_text` (lines ~199-212) with:

```rust
fn render_text(frame: &mut Frame, area: Rect, lines: &[String], selection: &[Option<(u16, u16)>]) {
    let width = area.width as usize;
    let text = Text::from(
        lines
            .iter()
            .enumerate()
            .map(|(row, l)| {
                let sel = selection.get(row).copied().flatten();
                highlight_line(l, sel, width)
            })
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(text), area);
}
```

- [ ] **Step 5: Build and lint.**

Run: `cargo build --workspace && cargo clippy --all-targets --all-features -- -D warnings`
Expected: compiles; no warnings.

- [ ] **Step 6: Full test sweep (no regressions).**

Run: `cargo test --workspace`
Expected: all pass, including the Task 1/2 redraw tests.

- [ ] **Step 7: Manual verification** (the animation itself is time-based and not asserted in the suite; this is the spec's intended coverage boundary).

```bash
# Make a long file and open it.
seq 1 500 > /tmp/nxvim_scroll_demo.txt
cargo run -p nxvim -- /tmp/nxvim_scroll_demo.txt
```
In the editor, confirm:
- `<C-f>` / `<C-d>` slide the viewport (a quick eased line-stepped scroll), not an instant jump; `<C-b>` / `<C-u>` slide back.
- Pressing a key mid-slide (e.g. `j`) immediately snaps to the result — no leftover animation.
- `<C-u>` at the very top and `<C-d>` at the very bottom do nothing jarring (no slide when the viewport can't move).
- `j`/`k`, editing, and `:` command line behave exactly as before (no animation, no flicker).
Then `:q` to exit.

- [ ] **Step 8: Commit.**

```bash
git add crates/nxvim-tui/src/lib.rs
git commit -m "feat(tui): animate scroll commands with a local-clock slide

Interpolate top/cursor over the server's suggested duration, slicing the scroll
band per frame with an ease-out curve; settle on the destination view and let
any new redraw supersede (interrupt). Animation lives entirely client-side, so
it stays smooth over remote links and a future GUI can render it sub-cell."
```

---

## Notes carried from the design

- **Configurability** (duration, easing, on/off): out of scope — there is no options system (`:set`) yet. `duration_ms` is a hardcoded `clamp(distance·8, 80, 160)`. When options land, this is the first knob to expose.
- **Visual-selection during a slide** *is* handled: the scroll band carries its own `selection`, so highlighting slides with the text.
- **Cursor column during a slide** uses the destination `cursor_screen_col`; for a pure vertical scroll the column is effectively constant, so this is imperceptible.
- **Not covered by automated tests:** the wall-clock animation (timing/easing/line-stepping). The integration suite asserts the *protocol* (the `scroll` descriptor + band); the visual motion is left to manual check now and the planned PTY e2e tests later. This boundary is intentional.
