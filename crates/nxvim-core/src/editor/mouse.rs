//! Mouse input: hit-testing a global screen cell back to a window and buffer
//! position, and the gesture handlers.
//!
//! The editor owns the whole pipeline (matching neovim's single-grid model and
//! nxvim's "the core owns *which* cells" split — see `docs/architecture.md`): a
//! [`MouseEvent`] carries a global, zero-based screen cell, and [`Editor::mouse`]
//! resolves it to a window + buffer position itself, so every front end only has
//! to forward the raw cell. The inverse map ([`Editor::hit_test`]) is the exact
//! reverse of the forward layout the [`crate::view`] projection computes — the
//! same chrome offset, window rects, number gutter, horizontal scroll, and
//! tab/wide-char [`virtcol`](crate::unicode::virtcol) math, run backwards.

use super::*;
use crate::input::{MouseAction, MouseButton, MouseEvent};

/// In-flight left-button selection: the multi-click counter (vim's
/// `check_multiclick` — a same-cell repeat within `'mousetime'` escalates the
/// selected unit) plus the anchor a drag extends from. One value spans a whole
/// press → drag → release gesture and persists into the gap before the next press
/// so a quick same-cell repeat is counted as a double-/triple-click.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MouseSelect {
    /// Screen cell of the press, to detect a same-cell repeat.
    row: usize,
    col: usize,
    /// Time of the press (ms, server-stamped), for the `'mousetime'` window.
    stamp_ms: u64,
    /// Click count: 1 = char, 2 = word, 3 = line. Capped at 3 — vim's quad-click
    /// blockwise selection awaits a blockwise Visual mode (not yet in nxvim).
    count: u8,
    /// What the drag pivots around: the press point (single click), or the whole
    /// word / line first selected (so a drag extends by whole units).
    anchor: SelectAnchor,
}

/// The anchored extent a left-drag extends from, set by the press by click count.
#[derive(Debug, Clone, Copy)]
enum SelectAnchor {
    /// Single click: the press position. Visual is not entered until the first
    /// drag (vim's `<LeftMouse>` then `<LeftDrag>`).
    Char(Cursor),
    /// Double click: the byte range `[lo, hi)` of the word under the press.
    Word { lo: usize, hi: usize },
    /// Triple click: the (0-based) line the press landed on.
    Line(usize),
}

/// Where a screen cell landed once hit-tested. Only the variants the implemented
/// phases act on are produced; the rest of the surface (separators, the tabline,
/// the panel) grows here as later phases wire those regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseTarget {
    /// A buffer cell in window `win`: 0-based buffer `line` and byte `col`. A
    /// click in the number gutter resolves to `col = 0` on that line.
    Text {
        win: WindowId,
        line: usize,
        col: usize,
    },
    /// The window's status row (its bottom line). No-op until split-resize drag
    /// lands; modeled now so a status-line click doesn't fall through to the text
    /// below it.
    StatusLine { win: WindowId },
}

impl Editor {
    /// Apply a mouse gesture. A no-op when `'mouse'` does not enable the current
    /// mode (vim-faithful — the gesture is simply ignored, not an error). Only the
    /// gestures implemented so far act; the rest are no-ops until their phase.
    pub fn mouse(&mut self, ev: MouseEvent) {
        if !self.mouse_enabled() {
            return;
        }
        match (ev.button, ev.action) {
            // In multi-cursor placement mode a left-click *toggles* a cursor at the
            // clicked cell — drop one if it's bare, remove it if one is there — the
            // mouse form of the `c` placement command. Drag/release don't place.
            (MouseButton::Left, MouseAction::Press) if self.mode == Mode::MultiCursor => {
                self.mouse_toggle_cursor(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Drag | MouseAction::Release)
                if self.mode == Mode::MultiCursor => {}
            // Shift+left-press extends the selection to the click (vim's
            // `<S-LeftMouse>` under the default `popup_setpos` mousemodel) instead
            // of placing the cursor and starting fresh.
            (MouseButton::Left, MouseAction::Press) if ev.shift => {
                self.mouse_left_extend(ev.row, ev.col, ev.stamp_ms)
            }
            (MouseButton::Left, MouseAction::Press) => {
                self.mouse_left_press(ev.row, ev.col, ev.stamp_ms)
            }
            (MouseButton::Left, MouseAction::Drag) => self.mouse_left_drag(ev.row, ev.col),
            // Release ends the drag but keeps `mouse_select` so the next press can
            // still see this one for multi-click counting; vim keeps the selection.
            (MouseButton::Left, MouseAction::Release) => {}
            // The wheel, the other buttons, and bare moves are wired in later
            // phases; ignore them for now.
            _ => {}
        }
    }

    /// Left-press: focus the clicked window, place the cursor, and start a
    /// selection sized by the click count — single = char (vim's `<LeftMouse>`),
    /// double = the word, triple = the line. A same-cell press within `'mousetime'`
    /// of the last escalates the count; otherwise it resets to one. An active
    /// Visual selection is torn down first (also vim's behavior). For a single
    /// click no selection starts until the first drag; double/triple enter Visual
    /// immediately.
    fn mouse_left_press(&mut self, row: usize, col: usize, stamp_ms: u64) {
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            // A press outside any window clears the gesture (and resets the count).
            self.mouse_select = None;
            return;
        };
        // Focus follows the click; focusing first makes `win` current so the
        // cursor/selection edits below act on the right window's state.
        self.set_current_window(win);
        if self.mode.is_visual() {
            // A click ends any active selection, stamping the `< / `> marks first
            // (the same teardown as Esc — see `command.rs`).
            self.record_visual_marks();
            self.mode = Mode::Normal;
        }
        self.set_window_cursor(win, line, bcol);

        let count = self.next_click_count(row, col, stamp_ms);
        let anchor = match count {
            1 => SelectAnchor::Char(self.cursor),
            2 => self.mouse_select_word(),
            _ => self.mouse_select_line(),
        };
        self.mouse_select = Some(MouseSelect {
            row,
            col,
            stamp_ms,
            count,
            anchor,
        });
    }

    /// Shift+left-press (`<S-LeftMouse>`): extend the selection to the click,
    /// keeping the existing anchor. If a Visual selection is already active the
    /// live end moves to the click (charwise or linewise, matching the current
    /// mode); otherwise a charwise Visual is started from the cursor's current
    /// position to the click. A following plain drag keeps extending in the same
    /// unit. Ignored if the click lands outside the focused window — the selection
    /// it would extend lives there.
    fn mouse_left_extend(&mut self, row: usize, col: usize, stamp_ms: u64) {
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            return;
        };
        if win != self.current_window_id() {
            return;
        }
        if self.mode == Mode::VisualLine {
            // Linewise: keep the anchored line, move the active line to the click.
            let anchor_line = self.visual_anchor.line;
            self.cursor = Cursor { line, col: 0 };
            self.clamp_cursor();
            self.mouse_select = Some(MouseSelect {
                row,
                col,
                stamp_ms,
                count: 3,
                anchor: SelectAnchor::Line(anchor_line),
            });
            return;
        }
        // Charwise: anchor at the current cursor when not already selecting, then
        // move the live end to the click.
        if !self.mode.is_visual() {
            self.visual_anchor = self.cursor;
            self.mode = Mode::Visual;
        }
        let anchor = self.visual_anchor;
        self.set_window_cursor(win, line, bcol);
        self.mouse_select = Some(MouseSelect {
            row,
            col,
            stamp_ms,
            count: 1,
            anchor: SelectAnchor::Char(anchor),
        });
    }

    /// Left-click in [`Mode::MultiCursor`]: move the primary to the clicked cell
    /// and toggle a secondary cursor there — the mouse form of the `c` placement
    /// command, so clicking a bare cell drops a cursor and clicking a placed one
    /// removes it ([`place_cursor_here`](Editor::place_cursor_here) does the
    /// toggle; [`record_placement_undo`](Editor::record_placement_undo) makes it a
    /// single `u` step, exactly like keyboard `c`). Ignored outside the focused
    /// window — the cursor set lives in its buffer. Clears any pending drag
    /// selection so a stray drag can't start a Visual here.
    fn mouse_toggle_cursor(&mut self, row: usize, col: usize) {
        self.mouse_select = None;
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            return;
        };
        if win != self.current_window_id() {
            return;
        }
        self.set_window_cursor(win, line, bcol);
        self.record_placement_undo();
        self.place_cursor_here();
    }

    /// The click count for a press at screen cell `(row, col)` stamped `stamp_ms`:
    /// one more than the previous (capped at 3) when it repeats the same cell
    /// within `'mousetime'`, else 1. Mirrors `check_multiclick`
    /// (`vendor/neovim/src/nvim/os/input.c`).
    fn next_click_count(&self, row: usize, col: usize, stamp_ms: u64) -> u8 {
        match self.mouse_select {
            Some(p)
                if p.row == row
                    && p.col == col
                    && stamp_ms.saturating_sub(p.stamp_ms) <= self.options.mousetime as u64 =>
            {
                (p.count + 1).min(3)
            }
            _ => 1,
        }
    }

    /// Double-click: select the word under the cursor as a charwise Visual,
    /// returning its byte range as the drag anchor. Uses the same `iskeyword`-class
    /// run as `iw` ([`class_span`](Editor::class_span)).
    fn mouse_select_word(&mut self) -> SelectAnchor {
        let (lo, hi) = self.class_span(self.cursor_char(), false);
        self.mode = Mode::Visual;
        self.set_visual_span(lo, hi);
        SelectAnchor::Word { lo, hi }
    }

    /// Triple-click: select the cursor's line as a linewise Visual, returning the
    /// line index as the drag anchor.
    fn mouse_select_line(&mut self) -> SelectAnchor {
        let line = self.cursor.line;
        self.mode = Mode::VisualLine;
        self.visual_anchor = Cursor { line, col: 0 };
        self.cursor = Cursor { line, col: 0 };
        SelectAnchor::Line(line)
    }

    /// Left-drag: extend the selection from its press anchor to the drag cell, in
    /// the unit the press chose — charwise for a single click, by whole words for a
    /// double click, by whole lines for a triple. Ignored if no press is in flight
    /// or the drag leaves the window it started in.
    fn mouse_left_drag(&mut self, row: usize, col: usize) {
        let Some(sel) = self.mouse_select else {
            return;
        };
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            return;
        };
        // The selection lives in the window the press focused; a drag that wanders
        // into another window doesn't hijack it.
        if win != self.current_window_id() {
            return;
        }
        match sel.anchor {
            SelectAnchor::Char(anchor) => {
                // The first drag after a single click enters charwise Visual,
                // anchored where the press landed; later drags just move the end.
                if !self.mode.is_visual() {
                    self.visual_anchor = anchor;
                    self.mode = Mode::Visual;
                }
                self.set_window_cursor(win, line, bcol);
            }
            SelectAnchor::Word { lo, hi } => self.mouse_extend_word(line, bcol, lo, hi),
            SelectAnchor::Line(anchor_line) => self.mouse_extend_line(line, anchor_line),
        }
    }

    /// Word-wise drag: grow the selection to cover whole words from the anchor word
    /// `[a_lo, a_hi)` to the word under the drag cell. Dragging forward keeps the
    /// anchor at the word's start; dragging back past it pivots — the anchor flips
    /// to the word's last char and the cursor leads at the far word's start (vim).
    fn mouse_extend_word(&mut self, line: usize, bcol: usize, a_lo: usize, a_hi: usize) {
        self.mode = Mode::Visual;
        let at = self.buffer().byte_at(line, bcol);
        let (b_lo, b_hi) = self.class_span(at, false);
        if b_lo >= a_lo {
            // Forward (or within the anchor word): anchor at the word's start, the
            // cursor on the last char of whichever word reaches furthest right.
            let end = self.prev_grapheme_idx(a_hi.max(b_hi));
            self.set_visual_chars(a_lo, end);
        } else {
            // Backward: anchor on the anchor word's last char, cursor at the far
            // word's start.
            let anchor = self.prev_grapheme_idx(a_hi);
            self.set_visual_chars(anchor, b_lo);
        }
    }

    /// Line-wise drag: extend the linewise Visual from the anchor line to the line
    /// under the drag cell. Direction is handled by the selection projection, which
    /// orders anchor and cursor, so this only moves the live end.
    fn mouse_extend_line(&mut self, line: usize, anchor_line: usize) {
        self.mode = Mode::VisualLine;
        self.visual_anchor = Cursor {
            line: anchor_line,
            col: 0,
        };
        self.cursor = Cursor { line, col: 0 };
        self.clamp_cursor();
    }

    /// Set a charwise Visual selection with the anchor at byte `anchor` and the
    /// live cursor at byte `cursor` (both clamped to grapheme boundaries) — unlike
    /// [`set_visual_span`](Editor::set_visual_span), the anchor may sit *after* the
    /// cursor, which a backward word-drag needs.
    fn set_visual_chars(&mut self, anchor: usize, cursor: usize) {
        self.set_cursor_char(anchor);
        self.visual_anchor = self.cursor;
        self.set_cursor_char(cursor);
    }

    /// Whether `'mouse'` enables mouse input for the current mode. `a` enables
    /// every mode; otherwise the mode's own char must be present (`n`/`v`/`i`/`c`).
    fn mouse_enabled(&self) -> bool {
        let m = &self.options.mouse;
        if m.contains('a') {
            return true;
        }
        let flag = match self.mode {
            // MultiCursor is a normal-like placement mode (its `mode()` code is
            // `n`), so it gates on the same `n` flag.
            Mode::Normal | Mode::MultiCursor => 'n',
            Mode::Visual | Mode::VisualLine => 'v',
            Mode::Insert | Mode::Replace => 'i',
            Mode::Command => 'c',
        };
        m.contains(flag)
    }

    /// Resolve a **global** screen cell `(row, col)` to a [`MouseTarget`], or
    /// `None` if it lands on no actionable region (the tabline, a separator, the
    /// panel, or outside every window). This is the reverse of the forward layout:
    /// strip the top chrome (the tabline row), find the window under the cell,
    /// then turn the window-relative cell into a buffer line/col through that
    /// window's scroll offset, number gutter, and tab/wide-char column math.
    fn hit_test(&self, row: usize, col: usize) -> Option<MouseTarget> {
        // The windows area sits below the tabline (when shown); the bottom chrome
        // (panel, command line) is already excluded by the window rects, so a cell
        // there simply matches no window. Columns are not inset (the area is full
        // width).
        let top_chrome = self.tabline_rows();
        if row < top_chrome {
            return None; // the tabline — a later phase resolves tab clicks
        }
        let (win, rel_x, rel_y) = self.window_at(col, row - top_chrome)?;

        // The window's content geometry, read for `win` whether or not it is the
        // focused window (its live offset if focused, its stashed one otherwise).
        let (top, leftcol) = self.window_scroll(win)?;
        let opts = self.window_options(win)?;
        let buf_id = self.window_buffer(win)?;
        let (_, text_height) = self.window_content_size(win)?;
        if rel_y >= text_height {
            // Below the text body: the status row (the last content line).
            return Some(MouseTarget::StatusLine { win });
        }

        let buf = &self.buffers.get(buf_id).buffer;
        let line_count = buf.line_count();
        // A click below the last line lands on the last line (vim's behavior).
        let line = (top + rel_y).min(line_count.saturating_sub(1));

        let gutter = self.number_width_for(opts, line_count);
        let col = if rel_x < gutter {
            // The number column: place the cursor at the line's start.
            0
        } else {
            // Screen column within the text, undoing the horizontal scroll, then
            // mapped back to a byte offset (rounding a between-cells click to the
            // nearest grapheme). `set_window_cursor`'s clamp pulls a past-EOL
            // result onto the last char in Normal mode.
            let screen_col = (rel_x - gutter) + leftcol;
            let text = buf.line(line);
            crate::unicode::byte_at_virtcol(&text, screen_col, buf.options.effective_tabstop())
        };
        Some(MouseTarget::Text { win, line, col })
    }

    /// Find the window whose on-screen content area contains the windows-area cell
    /// `(x, y)`, returning its id and the cell made **content-relative** (past a
    /// bordered float's border). Floats are tested first, top-most by z-order
    /// (`floats` is sorted bottom-to-top), then the tiled windows; this matches
    /// the paint order so the cell resolves to the window drawn on top. `None`
    /// when the cell is on a separator or outside every window.
    fn window_at(&self, x: usize, y: usize) -> Option<(WindowId, usize, usize)> {
        let probe = |id: WindowId| -> Option<(WindowId, usize, usize)> {
            let w = self.windows.get(id);
            // A bordered float spends one cell per side on its border; its content
            // is the rect inset by one. Tiled windows and borderless floats use the
            // whole rect.
            let inset = matches!(&w.float, Some(c) if c.border != BorderStyle::None) as usize;
            let r = w.rect;
            let x0 = r.x + inset;
            let y0 = r.y + inset;
            let x1 = (r.x + r.width).saturating_sub(inset);
            let y1 = (r.y + r.height).saturating_sub(inset);
            (x >= x0 && x < x1 && y >= y0 && y < y1).then(|| (id, x - x0, y - y0))
        };
        self.windows
            .floats
            .iter()
            .rev()
            .copied()
            .chain(self.windows.leaves())
            .find_map(probe)
    }
}
