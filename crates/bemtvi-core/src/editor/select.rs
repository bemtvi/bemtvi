//! Select mode (vim's `v_CTRL-G`) — the P6 snippet-engine primitive.
//!
//! A byte range is highlighted like a charwise Visual selection ([`Mode::Select`]
//! reuses [`Editor::visual_anchor`]/[`Editor::cursor`] and renders through
//! [`Editor::rendered_visual_mode`]), but the keys mean something different: a
//! printable char, `<CR>`, or `<BS>` **replaces** the whole selection — the range
//! is deleted and Insert mode is entered with that input — which is the
//! type-over-default behavior a snippet engine wants when it jumps onto a
//! placeholder `${1:default}`. It is a *primitive*, not a muscle-memory mode: there
//! is no Normal-mode keystroke to enter it, only [`btv.win.select_range`] (backed by
//! [`Editor::select_range_in_window`]), the Select sibling of `btv.win.set_cursor`.
//!
//! Unlike the Visual command grammar (`d`/`y`/`c`/motions), Select's keys route
//! through [`Editor::handle_select`] straight from [`Editor::input`], so it is
//! deliberately *not* [`Mode::is_visual`] — only its rendered selection borrows the
//! Visual machinery.

use super::*;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;

impl Editor {
    /// Enter Select mode over the 0-based, end-exclusive byte range
    /// `(s_row, s_col)..(e_row, e_col)` in window `id` — the core behind
    /// `btv.win.select_range`. Focuses `id` first (the selection is the focused
    /// editor's state), clamps the range into the buffer, and anchors a charwise
    /// selection: [`Editor::visual_anchor`] at the start, [`Editor::cursor`] on the
    /// **last** selected char (inclusive, like Visual). An empty range (`start ==
    /// end`) has nothing to select, so it degrades to the empty-placeholder path —
    /// caret at the start, Insert mode — matching what a snippet engine does for an
    /// empty tabstop.
    pub fn select_range_in_window(
        &mut self,
        id: WindowId,
        s_row: usize,
        s_col: usize,
        e_row: usize,
        e_col: usize,
        escape_insert: bool,
    ) {
        if self.windows.try_get(id).is_none() {
            return;
        }
        if id != self.windows.current {
            self.set_current_window(id);
        }
        let lo = self.clamp_to_byte(s_row, s_col);
        let hi = self.clamp_to_byte(e_row, e_col);
        self.select_bytes(lo, hi, escape_insert);
    }

    /// Enter Select mode over the byte range `[lo, hi)` of the current buffer — the
    /// window-agnostic core shared by [`select_range_in_window`](Self::select_range_in_window)
    /// and the snippet engine (which selects a placeholder default so the first
    /// keystroke replaces it). `escape_insert` sets where `<Esc>` lands (Insert past
    /// the selection vs. Normal on its head). An empty / inverted range highlights
    /// nothing: it parks the caret at `lo` in Insert — the empty-placeholder path — so
    /// callers can hand it any range uniformly.
    pub(crate) fn select_bytes(&mut self, lo: usize, hi: usize, escape_insert: bool) {
        // A completion popup and Select mode are incompatible input contexts — in Select
        // the popup's `<C-n>`/`<C-p>` don't route to it — so a popup left open (e.g. a
        // plugin snippet engine jumping to the next tabstop while completion is up) would
        // just linger, following the cursor. Close it before entering Select.
        self.close_completion();
        self.select_escape_insert = escape_insert;
        if hi <= lo {
            self.mode = Mode::Insert;
            self.set_cursor_char_insert(lo);
            self.desired_col = self.cursor_virtcol();
            self.ensure_visible();
            return;
        }
        self.visual_anchor = self.cursor_at_byte(lo);
        // The cursor sits on the last selected char (the Visual selection is
        // inclusive of the char under the cursor); `hi` is one past it.
        self.cursor = self.cursor_at_byte(hi - 1);
        self.select_linewise = false;
        self.mode = Mode::Select;
        self.clamp_cursor();
        self.desired_col = self.cursor_virtcol();
        self.ensure_visible();
    }

    /// Clamp a `(row, col)` onto a real byte offset in the current buffer — `row`
    /// into `[0, line_count)`, `col` into that line's byte length — so an
    /// out-of-range argument from Lua lands somewhere sane rather than off the end.
    fn clamp_to_byte(&self, row: usize, col: usize) -> usize {
        let last = self.buffer().line_count().saturating_sub(1);
        let row = row.min(last);
        let col = col.min(self.buffer().line_len(row));
        self.buffer().byte_at(row, col)
    }

    /// Handle one key in [`Mode::Select`]. A printable char / `<CR>` / `<BS>` /
    /// `<Del>` replaces the selection (delete it, enter Insert, then apply the
    /// input); `<Esc>` keeps the selected text and parks the caret in Insert at the
    /// end of it; anything else leaves Select for Normal and is re-dispatched there
    /// (so a stray motion collapses the selection and moves, never silently
    /// vanishes).
    pub(crate) fn handle_select(&mut self, key: Key) {
        self.message.clear();
        // While a snippet session is live, the jump keys (`<Tab>`/`<S-Tab>`) move to
        // the next/previous tabstop straight from a selected placeholder — skipping it
        // without editing, keeping its default — rather than falling through to the
        // Normal-mode re-dispatch below (which would end the session's Insert context).
        if let Some(dir) = self.snippet_jump_for(&key) {
            self.snippet_jump(dir);
            return;
        }
        // `<C-g>` toggles back to Visual, keeping the selection and its shape (the
        // Visual → Select half is `NormalCmd::ToggleVisualSelect`).
        if key.ctrl && key.code == KeyCode::Char('g') {
            self.mode = if self.select_linewise {
                Mode::VisualLine
            } else {
                Mode::Visual
            };
            return;
        }
        match key.code {
            // `<Esc>`: keep the default. Where it lands is the caller's choice (the
            // `on_escape` option): Insert past the selection so a snippet engine can
            // keep editing the placeholder, or Normal — vim's `v_CTRL-G` — for a
            // generic select-and-replace consumer (a rename widget) and the `gh`/`gH`
            // keyboard entries.
            KeyCode::Esc if !key.ctrl && !key.alt => {
                if self.select_escape_insert {
                    self.select_keep_and_insert();
                } else {
                    self.select_keep_and_normal();
                }
            }
            // A printable replaces the selection, then types itself.
            KeyCode::Char(_) if !key.ctrl && !key.alt => {
                self.select_replace();
                self.handle_insert(key);
            }
            // `<CR>` replaces then splits the line; `<BS>`/`<Del>` replace with
            // nothing (the deletion of the selection is the whole effect, as in vim).
            KeyCode::Enter => {
                self.select_replace();
                self.handle_insert(Key::new(KeyCode::Enter));
            }
            KeyCode::Backspace | KeyCode::Delete => {
                self.select_replace();
                // `<BS>`/`<Del>` clear a placeholder without going through
                // `handle_insert`, so sync the (now-empty) tabstop into its mirrors
                // here — the printable / `<CR>` arms sync via `handle_insert`.
                if self.snippet_active() {
                    self.snippet_sync();
                }
            }
            // Any other key (motion, `<C-*>`, an unmapped named key) ends Select and
            // is handled as an ordinary Normal-mode key from the selection head.
            _ => {
                self.record_visual_marks();
                self.mode = Mode::Normal;
                self.clamp_cursor();
                self.handle_normal(key);
            }
        }
    }

    /// The delete-and-enter-Insert transition shared by every replacing key: stamp
    /// the `` `< ``/`` `> `` marks, then reuse the change operator (`c`) over the
    /// selection as one undo step (folding into an already-open insert group when one
    /// exists, per the P1 grouping rule). Charwise deletes the span and lands in
    /// Insert at its start; linewise (`gH`) replaces whole lines with a fresh indented
    /// line, exactly like `S`/`cc`.
    fn select_replace(&mut self) {
        let linewise = self.select_linewise;
        let (lo, hi, first_line) = self.visual_range_lw(linewise);
        self.record_visual_marks();
        self.push_undo();
        self.snapshot_taken = true;
        self.apply_operator_to_range('c', lo, hi, linewise, first_line);
    }

    /// `<Esc>` in Select with `on_escape = "insert"`: keep the selected text and enter
    /// Insert at its end (one past the last selected char), so typing continues after
    /// the kept default. Opens no undo group — nothing was edited — so a following
    /// keystroke's first edit starts its own.
    fn select_keep_and_insert(&mut self) {
        let (_lo, hi, _first) = self.visual_range_lw(self.select_linewise);
        self.record_visual_marks();
        self.mode = Mode::Insert;
        self.set_cursor_char_insert(hi);
        self.desired_col = self.cursor_virtcol();
    }

    /// `<Esc>` in Select with `on_escape = "normal"`: keep the selected text and drop
    /// to Normal with the caret left on the selection head (vim's `v_CTRL-G` Escape),
    /// the vim-faithful choice a generic select-and-replace consumer wants.
    fn select_keep_and_normal(&mut self) {
        self.record_visual_marks();
        self.mode = Mode::Normal;
        self.clamp_cursor();
    }
}
