//! The editor state machine: turns keys and ex-commands into buffer mutations.
//!
//! This is the rust-native analogue of neovim's `normal.c` / `ops.c` /
//! `edit.c` / `ex_docmd.c`. It is fully synchronous and owns no I/O beyond
//! reading/writing files through [`Buffer`]. The async server feeds it input
//! and reads back state; it never blocks.

use std::cmp::{max, min};
use std::path::PathBuf;

use crate::buffer::Buffer;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::unicode;
use crate::view::View;

/// A cursor position within the current buffer (0-indexed line and column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Default)]
struct Register {
    text: String,
    linewise: bool,
}

#[derive(Clone)]
struct Snapshot {
    text: ropey::Rope,
    cursor: Cursor,
}

#[derive(Debug, Clone, Copy)]
enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// How a motion places the cursor when used as plain movement (not as an
/// operator's range). This is what drives vim's `curswant` column memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveAxis {
    /// Horizontal move: the resulting column becomes the new desired column.
    Horizontal,
    /// `$`/`End`: stick to end-of-line until a horizontal move clears it.
    EndOfLine,
    /// `gg`/`G`/etc.: jump to a line's first non-blank; resets desired column.
    LineAnchor,
    /// `j`/`k`: change line but keep the remembered desired column.
    VerticalKeep,
}

struct MotionResult {
    target: usize,
    kind: MotionKind,
    axis: MoveAxis,
}

/// The complete editor state for a single buffer/window.
pub struct Editor {
    pub buffer: Buffer,
    pub mode: Mode,
    pub cursor: Cursor,
    /// First visible buffer line (vertical scroll offset).
    pub top: usize,
    /// Command-line contents (text after the leading `:`).
    pub cmdline: String,
    /// Transient status message (the bottom line when not in command mode).
    pub message: String,
    pub should_quit: bool,

    width: usize,
    height: usize,
    /// Remembered target column for vertical motion (vim's `curswant`).
    desired_col: usize,
    /// When set, vertical motion sticks to end-of-line (set by `$`).
    desired_eol: bool,
    /// Per-key: the action just handled was a vertical/keep motion, so the
    /// remembered column must be preserved rather than recomputed.
    preserve_desired: bool,
    /// Per-key: the action just handled requests end-of-line stickiness (`$`).
    eol_request: bool,
    register: Register,

    // pending normal-mode state
    count: Option<usize>,
    op_count: Option<usize>,
    operator: Option<char>,
    gpending: bool,
    pending_replace: bool,
    /// Set when an undo snapshot has already been taken for the current edit
    /// "session" (e.g. an insert), so we group the whole session into one undo.
    snapshot_taken: bool,
    visual_anchor: Cursor,

    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,

    /// Lua chunks queued by `:lua`, drained by the server's Lua runtime.
    pub lua_queue: Vec<String>,
}

impl Editor {
    pub fn new() -> Self {
        Editor::with_buffer(Buffer::empty())
    }

    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        Ok(Editor::with_buffer(Buffer::from_file(path.into())?))
    }

    fn with_buffer(buffer: Buffer) -> Self {
        Editor {
            buffer,
            mode: Mode::Normal,
            cursor: Cursor::default(),
            top: 0,
            cmdline: String::new(),
            message: String::new(),
            should_quit: false,
            width: 80,
            height: 24,
            desired_col: 0,
            desired_eol: false,
            preserve_desired: false,
            eol_request: false,
            register: Register::default(),
            count: None,
            op_count: None,
            operator: None,
            gpending: false,
            pending_replace: false,
            snapshot_taken: false,
            visual_anchor: Cursor::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            lua_queue: Vec::new(),
        }
    }

    // ----- public API used by the server -----------------------------------

    /// Resize the *text viewport*. The client owns the screen layout and tells
    /// us only how tall the text area is (status/command lines are the client's
    /// own regions), so the whole height here is editable rows.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.ensure_visible();
    }

    /// Feed a single key into the editor.
    pub fn input(&mut self, key: Key) {
        self.preserve_desired = false;
        self.eol_request = false;

        match self.mode {
            Mode::Insert | Mode::Replace => self.handle_insert(key),
            Mode::Command => self.handle_command(key),
            _ => self.handle_normal(key),
        }

        // Update vim's `curswant`: vertical motions keep the remembered column,
        // every other action recomputes it from where the cursor landed.
        if !self.preserve_desired {
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = self.eol_request;
        }
        self.ensure_visible();
    }

    /// Run an ex-command directly (the `nvim_command` API entry point).
    pub fn command(&mut self, cmd: &str) {
        self.execute_ex(cmd);
        self.desired_col = self.cursor_virtcol();
        self.desired_eol = false;
        self.ensure_visible();
    }

    /// Editable lines as owned strings (the `nvim_buf_get_lines` entry point).
    pub fn lines(&self) -> Vec<String> {
        self.buffer.lines()
    }

    /// Produce a [`View`] of the current state for a text viewport of the given
    /// size. The client renders the view's regions with its own widgets.
    pub fn view(&mut self, width: usize, height: usize) -> View {
        self.resize(width, height);
        View::from_editor(self)
    }

    pub(crate) fn dims(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub(crate) fn text_height(&self) -> usize {
        self.height.max(1)
    }

    // ----- normal / visual mode --------------------------------------------

    fn handle_normal(&mut self, key: Key) {
        self.message.clear();

        // `r{char}` waits for one key, then overwrites; `r<Esc>` cancels.
        if self.pending_replace {
            self.pending_replace = false;
            if key.code != KeyCode::Esc {
                if let Some(c) = key.as_char() {
                    let count = self.effective_count();
                    self.replace_char(c, count);
                }
            }
            self.reset_pending();
            return;
        }

        // Escape cancels pending state / leaves visual mode.
        if key.code == KeyCode::Esc {
            self.operator = None;
            self.count = None;
            self.op_count = None;
            self.gpending = false;
            if self.mode.is_visual() {
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            return;
        }

        // Count accumulation. `0` is a motion unless a count is already started.
        if let Some(c) = key.as_char() {
            if c.is_ascii_digit() && !(c == '0' && self.count.is_none()) {
                let d = c as usize - '0' as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + d);
                return;
            }
        }

        // `g` prefix (gg, etc.).
        if !self.gpending {
            if let Some('g') = key.as_char() {
                self.gpending = true;
                return;
            }
        }

        // Operator + motion resolution.
        let raw = if self.operator.is_some() {
            self.op_count.or(self.count)
        } else {
            self.count
        };
        let count = self.effective_count();

        if let Some(m) = self.resolve_motion(key, count, raw) {
            self.gpending = false;
            if let Some(op) = self.operator.take() {
                self.apply_operator(op, m);
                self.reset_pending();
            } else {
                // Movement (normal or visual); selection follows the cursor.
                self.apply_movement(m);
                self.count = None;
            }
            return;
        }

        self.gpending = false;
        self.handle_normal_command(key, count);
    }

    fn handle_normal_command(&mut self, key: Key, count: usize) {
        // With an operator pending, the only thing that lands here is a doubled
        // operator (dd/cc/yy); anything else cancels the pending operator.
        if let Some(op) = self.operator {
            if key.as_char() == Some(op) {
                self.begin_operator(op);
            } else {
                self.reset_pending();
            }
            return;
        }

        // Ctrl-keyed scrolling.
        if key.ctrl {
            match key.code {
                KeyCode::Char('d') => self.scroll_half(true),
                KeyCode::Char('u') => self.scroll_half(false),
                KeyCode::Char('f') => self.scroll_page(true),
                KeyCode::Char('b') => self.scroll_page(false),
                KeyCode::Char('r') => self.redo(),
                _ => {}
            }
            self.reset_pending();
            return;
        }

        let c = match key.as_char() {
            Some(c) => c,
            None => {
                self.reset_pending();
                return;
            }
        };

        // Visual-mode operators act on the selection immediately.
        if self.mode.is_visual() {
            match c {
                'd' | 'x' => {
                    self.visual_operate('d');
                    return;
                }
                'y' => {
                    self.visual_operate('y');
                    return;
                }
                'c' | 's' => {
                    self.visual_operate('c');
                    return;
                }
                'v' => {
                    self.mode = Mode::Visual;
                    return;
                }
                'V' => {
                    self.mode = Mode::VisualLine;
                    return;
                }
                ':' => {
                    self.enter_command();
                    return;
                }
                _ => {}
            }
        }

        match c {
            'i' => self.enter_insert_at(self.cursor.col),
            'I' => {
                let col = self.first_non_blank(self.cursor.line);
                self.enter_insert_at(col);
            }
            'a' => {
                let col = (self.cursor.col + 1).min(self.line_len());
                self.enter_insert_at(col);
            }
            'A' => self.enter_insert_at(self.line_len()),
            'o' => self.open_line(true),
            'O' => self.open_line(false),
            'x' => self.delete_under_cursor(count),
            'X' => self.delete_before_cursor(count),
            'D' => self.delete_to_eol(),
            'C' => {
                self.delete_to_eol();
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            's' => {
                self.delete_under_cursor(count);
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            'd' | 'c' | 'y' => {
                self.begin_operator(c);
                return;
            }
            'r' => {
                self.pending_replace = true;
                return;
            }
            'p' => self.paste(true, count),
            'P' => self.paste(false, count),
            'u' => self.undo(),
            'J' => self.join_lines(count.max(2)),
            '~' => self.toggle_case(count),
            'v' => {
                self.mode = Mode::Visual;
                self.visual_anchor = self.cursor;
            }
            'V' => {
                self.mode = Mode::VisualLine;
                self.visual_anchor = self.cursor;
            }
            ':' => self.enter_command(),
            _ => {}
        }
        self.reset_pending();
    }

    fn begin_operator(&mut self, op: char) {
        if self.operator == Some(op) {
            // Doubled operator: linewise over `count` lines.
            let count = self.effective_count();
            let last = self.cursor.line + count - 1;
            let target = self
                .buffer
                .line_start(last.min(self.buffer.line_count().saturating_sub(1)));
            // axis is unused for the operator path, but the field is required.
            let m = MotionResult {
                target,
                kind: MotionKind::Linewise,
                axis: MoveAxis::LineAnchor,
            };
            self.operator = None;
            self.apply_operator(op, m);
            self.reset_pending();
        } else {
            self.operator = Some(op);
            self.op_count = self.count.take();
        }
    }

    // ----- motions ----------------------------------------------------------

    fn resolve_motion(&self, key: Key, count: usize, raw: Option<usize>) -> Option<MotionResult> {
        let line = self.cursor.line;
        let last_line = self.buffer.line_count().saturating_sub(1);

        let kc = key.code;
        let ch = key.as_char();

        if self.gpending {
            if ch == Some('g') {
                let target_line = raw.map(|n| n - 1).unwrap_or(0).min(last_line);
                return Some(MotionResult {
                    target: self.buffer.line_start(target_line),
                    kind: MotionKind::Linewise,
                    axis: MoveAxis::LineAnchor,
                });
            }
            return None;
        }

        let motion = match (kc, ch) {
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
            (_, Some('0')) | (KeyCode::Home, _) => MotionResult {
                target: self.buffer.byte_at(line, 0),
                kind: MotionKind::Exclusive,
                axis: MoveAxis::Horizontal,
            },
            (_, Some('^')) => {
                let col = self.first_non_blank(line);
                MotionResult {
                    target: self.buffer.byte_at(line, col),
                    kind: MotionKind::Exclusive,
                    axis: MoveAxis::Horizontal,
                }
            }
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
            (KeyCode::Down, _) | (_, Some('j')) | (_, Some('\r')) => {
                let l = (line + count).min(last_line);
                MotionResult {
                    target: self.buffer.line_start(l),
                    kind: MotionKind::Linewise,
                    axis: MoveAxis::VerticalKeep,
                }
            }
            (KeyCode::Up, _) | (_, Some('k')) => {
                let l = line.saturating_sub(count);
                MotionResult {
                    target: self.buffer.line_start(l),
                    kind: MotionKind::Linewise,
                    axis: MoveAxis::VerticalKeep,
                }
            }
            (_, Some('G')) => {
                let l = raw.map(|n| n - 1).unwrap_or(last_line).min(last_line);
                MotionResult {
                    target: self.buffer.line_start(l),
                    kind: MotionKind::Linewise,
                    axis: MoveAxis::LineAnchor,
                }
            }
            (_, Some('w')) | (_, Some('W')) => {
                let mut idx = self.cursor_char();
                // Special case: `cw` on a non-blank acts like `ce` — it changes
                // to the end of the word without swallowing the trailing space.
                if self.operator == Some('c')
                    && idx <= self.last_char_idx()
                    && char_class(self.char_at(idx)) != CharClass::Blank
                {
                    for _ in 0..count {
                        idx = self.word_end(idx);
                    }
                    MotionResult {
                        target: idx,
                        kind: MotionKind::Inclusive,
                        axis: MoveAxis::Horizontal,
                    }
                } else {
                    for _ in 0..count {
                        idx = self.word_forward(idx);
                    }
                    MotionResult {
                        target: idx,
                        kind: MotionKind::Exclusive,
                        axis: MoveAxis::Horizontal,
                    }
                }
            }
            (_, Some('b')) | (_, Some('B')) => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_backward(idx);
                }
                MotionResult {
                    target: idx,
                    kind: MotionKind::Exclusive,
                    axis: MoveAxis::Horizontal,
                }
            }
            (_, Some('e')) | (_, Some('E')) => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_end(idx);
                }
                MotionResult {
                    target: idx,
                    kind: MotionKind::Inclusive,
                    axis: MoveAxis::Horizontal,
                }
            }
            _ => return None,
        };
        Some(motion)
    }

    /// Apply a motion as plain cursor movement, maintaining vim's `curswant`.
    fn apply_movement(&mut self, m: MotionResult) {
        match m.axis {
            MoveAxis::Horizontal => {
                self.set_cursor_char(m.target);
                self.clamp_cursor();
            }
            MoveAxis::EndOfLine => {
                self.set_cursor_char(m.target);
                self.clamp_cursor();
                self.eol_request = true;
            }
            MoveAxis::LineAnchor => {
                let line = self.buffer.byte_to_line(m.target.min(self.last_char_idx()));
                self.cursor.line = line;
                self.cursor.col = self.first_non_blank(line);
                self.clamp_cursor();
            }
            MoveAxis::VerticalKeep => {
                let line = self.buffer.byte_to_line(m.target.min(self.last_char_idx()));
                self.cursor.line = line;
                self.settle_desired_col(false);
                self.preserve_desired = true;
            }
        }
    }

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

    // ----- operators --------------------------------------------------------

    fn apply_operator(&mut self, op: char, m: MotionResult) {
        let cur = self.cursor_char();
        let (lo, hi, linewise, first_line) = match m.kind {
            MotionKind::Exclusive => (min(cur, m.target), max(cur, m.target), false, 0),
            MotionKind::Inclusive => (min(cur, m.target), max(cur, m.target) + 1, false, 0),
            MotionKind::Linewise => {
                let l1 = self.cursor.line;
                let l2 = self.buffer.byte_to_line(m.target.min(self.last_char_idx()));
                let (a, b) = (min(l1, l2), max(l1, l2));
                let lo = self.buffer.line_start(a);
                let hi = self
                    .buffer
                    .line_start((b + 1).min(self.buffer.line_count()));
                (lo, hi, true, a)
            }
        };
        if lo >= hi {
            return;
        }
        match op {
            'y' => {
                self.yank_range(lo, hi, linewise);
                if linewise {
                    self.cursor.line = first_line;
                } else {
                    self.set_cursor_char(lo);
                }
                self.clamp_cursor();
            }
            'd' => {
                self.yank_range(lo, hi, linewise);
                self.delete_range(lo, hi);
                if linewise {
                    self.cursor.line = first_line.min(self.buffer.line_count().saturating_sub(1));
                    self.cursor.col = self.first_non_blank(self.cursor.line);
                } else {
                    self.set_cursor_char(lo);
                }
                self.clamp_cursor();
            }
            'c' => {
                self.yank_range(lo, hi, linewise);
                if linewise {
                    self.delete_range(lo, hi);
                    let at = self
                        .buffer
                        .line_start(first_line.min(self.buffer.line_count().saturating_sub(1)));
                    self.buffer.text.insert_char(at, '\n');
                    self.buffer.normalize();
                    self.cursor.line = first_line;
                    self.cursor.col = 0;
                } else {
                    self.delete_range(lo, hi);
                    self.set_cursor_char_insert(lo);
                }
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            _ => {}
        }
    }

    fn visual_operate(&mut self, op: char) {
        let (lo, hi, linewise, first_line) = self.visual_range();
        self.push_undo();
        self.yank_range(lo, hi, linewise);
        match op {
            'd' => {
                self.delete_range(lo, hi);
                if linewise {
                    self.cursor.line = first_line.min(self.buffer.line_count().saturating_sub(1));
                    self.cursor.col = self.first_non_blank(self.cursor.line);
                } else {
                    self.set_cursor_char(lo);
                }
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            'y' => {
                if linewise {
                    self.cursor.line = first_line;
                } else {
                    self.set_cursor_char(lo);
                }
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            'c' => {
                if linewise {
                    self.delete_range(lo, hi);
                    let at = self
                        .buffer
                        .line_start(first_line.min(self.buffer.line_count().saturating_sub(1)));
                    self.buffer.text.insert_char(at, '\n');
                    self.buffer.normalize();
                    self.cursor.line = first_line;
                    self.cursor.col = 0;
                } else {
                    self.delete_range(lo, hi);
                    self.set_cursor_char_insert(lo);
                }
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            _ => {}
        }
        self.reset_pending();
    }

    fn visual_range(&self) -> (usize, usize, bool, usize) {
        let a = self.visual_anchor;
        let b = self.cursor;
        if self.mode == Mode::VisualLine {
            let (la, lb) = (min(a.line, b.line), max(a.line, b.line));
            let lo = self.buffer.line_start(la);
            let hi = self
                .buffer
                .line_start((lb + 1).min(self.buffer.line_count()));
            (lo, hi, true, la)
        } else {
            let ca = self.buffer.byte_at(a.line, a.col);
            let cb = self.buffer.byte_at(b.line, b.col);
            let lo = min(ca, cb);
            let hi = max(ca, cb) + 1;
            (lo, hi.min(self.last_char_idx().max(lo + 1)), false, 0)
        }
    }

    // ----- editing primitives ----------------------------------------------

    fn yank_range(&mut self, lo: usize, hi: usize, linewise: bool) {
        let (lo, hi) = self.snap_range(lo, hi);
        if lo >= hi {
            return;
        }
        self.register = Register {
            text: self.buffer.text.slice(lo..hi).to_string(),
            linewise,
        };
    }

    /// Remove `[lo, hi)` bytes, recording undo and keeping the buffer invariant.
    fn delete_range(&mut self, lo: usize, hi: usize) {
        let (lo, hi) = self.snap_range(lo, hi);
        if lo >= hi {
            return;
        }
        self.push_undo();
        self.buffer.text.remove(lo..hi);
        self.buffer.normalize();
        self.buffer.modified = true;
    }

    /// Clamp a byte range into bounds and onto grapheme boundaries, so a
    /// motion-derived endpoint can never split a cluster (a no-op for ASCII).
    fn snap_range(&self, lo: usize, hi: usize) -> (usize, usize) {
        let hi = hi.min(self.buffer.len_bytes());
        let lo = self.grapheme_floor_abs(lo.min(hi));
        let hi = self.grapheme_ceil_abs(hi);
        (lo, hi)
    }

    fn delete_under_cursor(&mut self, count: usize) {
        let len = self.line_len();
        if len == 0 {
            return;
        }
        let lo = self.cursor_char();
        let line_end = self.buffer.byte_at(self.cursor.line, len);
        let (hi, _) = self.advance_graphemes(lo, count, line_end);
        self.yank_range(lo, hi, false);
        self.delete_range(lo, hi);
        self.clamp_cursor();
    }

    fn delete_before_cursor(&mut self, count: usize) {
        if self.cursor.col == 0 {
            return;
        }
        let new_col = self.cursor.col.saturating_sub(count);
        let lo = self.buffer.byte_at(self.cursor.line, new_col);
        let hi = self.cursor_char();
        self.yank_range(lo, hi, false);
        self.delete_range(lo, hi);
        self.cursor.col = new_col;
        self.clamp_cursor();
    }

    fn delete_to_eol(&mut self) {
        let len = self.line_len();
        let lo = self.cursor_char();
        let hi = self.buffer.byte_at(self.cursor.line, len);
        if lo < hi {
            self.yank_range(lo, hi, false);
            self.delete_range(lo, hi);
        }
        self.clamp_cursor();
    }

    fn replace_char(&mut self, c: char, count: usize) {
        let len = self.line_len();
        let lo = self.cursor_char();
        let line_end = self.buffer.byte_at(self.cursor.line, len);
        let (hi, crossed) = self.advance_graphemes(lo, count, line_end);
        // `r` does nothing unless `count` whole characters remain on the line.
        if crossed < count {
            return;
        }
        self.push_undo();
        self.buffer.text.remove(lo..hi);
        let repl: String = std::iter::repeat(c).take(count).collect();
        self.buffer.text.insert(lo, &repl);
        self.buffer.modified = true;
        self.cursor.col =
            (lo - self.buffer.line_start(self.cursor.line)) + (count - 1) * c.len_utf8();
        self.clamp_cursor();
    }

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

    fn join_lines(&mut self, count: usize) {
        let joins = count.saturating_sub(1).max(1);
        self.push_undo();
        for _ in 0..joins {
            if self.cursor.line + 1 >= self.buffer.line_count() {
                break;
            }
            let cur_len = self.line_len();
            let eol = self.buffer.byte_at(self.cursor.line, cur_len);
            // Remove the newline and any leading whitespace of the next line.
            let next_start = self.buffer.line_start(self.cursor.line + 1);
            let mut ws_end = next_start;
            while ws_end < self.last_char_idx() {
                let c = self.char_at(ws_end);
                if c == ' ' || c == '\t' {
                    ws_end += 1;
                } else {
                    break;
                }
            }
            self.buffer.text.remove(eol..ws_end);
            // Insert a single separating space unless the line was empty.
            if cur_len > 0 {
                self.buffer.text.insert_char(eol, ' ');
            }
            self.cursor.col = cur_len;
        }
        self.buffer.normalize();
        self.buffer.modified = true;
        self.clamp_cursor();
    }

    fn open_line(&mut self, below: bool) {
        self.push_undo();
        if below {
            let at = self.buffer.byte_at(self.cursor.line, self.line_len());
            self.buffer.text.insert_char(at, '\n');
            self.cursor.line += 1;
        } else {
            let at = self.buffer.line_start(self.cursor.line);
            self.buffer.text.insert_char(at, '\n');
        }
        self.buffer.normalize();
        self.cursor.col = 0;
        self.buffer.modified = true;
        self.mode = Mode::Insert;
        self.snapshot_taken = true;
    }

    fn paste(&mut self, after: bool, count: usize) {
        if self.register.text.is_empty() {
            return;
        }
        self.push_undo();
        if self.register.linewise {
            let at = if after {
                self.buffer
                    .line_start((self.cursor.line + 1).min(self.buffer.line_count()))
            } else {
                self.buffer.line_start(self.cursor.line)
            };
            let chunk = self.register.text.repeat(count);
            self.buffer.text.insert(at, &chunk);
            self.buffer.normalize();
            self.cursor.line = if after {
                self.cursor.line + 1
            } else {
                self.cursor.line
            };
            self.cursor.col = self.first_non_blank(self.cursor.line);
        } else {
            let len = self.line_len();
            let cur = self.cursor_char();
            let line_end = self.buffer.byte_at(self.cursor.line, len);
            // Paste *after* lands past the whole grapheme under the cursor, never
            // between a base char and its combining mark.
            let at = if after && len > 0 {
                self.next_grapheme_idx(cur).min(line_end)
            } else {
                cur
            };
            let chunk = self.register.text.repeat(count);
            // Byte length of the chunk's final grapheme, so the cursor lands on
            // it (not on a trailing combining mark) — vim leaves it on the last
            // pasted character.
            let last_len = chunk.len() - unicode::prev_grapheme(&chunk, chunk.len());
            let end = at + chunk.len();
            self.buffer.text.insert(at, &chunk);
            self.set_cursor_char(end.saturating_sub(last_len));
        }
        self.buffer.normalize();
        self.buffer.modified = true;
        self.clamp_cursor();
    }

    // ----- insert mode ------------------------------------------------------

    fn enter_insert_at(&mut self, col: usize) {
        self.push_undo();
        self.snapshot_taken = true;
        self.cursor.col = col.min(self.line_len());
        self.mode = Mode::Insert;
    }

    fn handle_insert(&mut self, key: Key) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                if self.cursor.col > 0 {
                    let s = self.buffer.line(self.cursor.line);
                    self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
                }
                self.clamp_cursor();
                self.snapshot_taken = false;
            }
            KeyCode::Enter => {
                let at = self.cursor_char();
                self.buffer.text.insert_char(at, '\n');
                self.cursor.line += 1;
                self.cursor.col = 0;
                self.buffer.modified = true;
            }
            KeyCode::Backspace => self.insert_backspace(),
            KeyCode::Tab => {
                let at = self.cursor_char();
                self.buffer.text.insert_char(at, '\t');
                self.cursor.col += 1;
                self.buffer.modified = true;
            }
            KeyCode::Left => {
                let s = self.buffer.line(self.cursor.line);
                self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
            }
            KeyCode::Right => {
                let s = self.buffer.line(self.cursor.line);
                self.cursor.col = unicode::next_grapheme(&s, self.cursor.col).min(s.len());
            }
            KeyCode::Up => self.move_vertical(-1, true),
            KeyCode::Down => self.move_vertical(1, true),
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
            _ => {}
        }
    }

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

    // ----- command-line mode ------------------------------------------------

    fn enter_command(&mut self) {
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.message.clear();
        self.reset_pending();
    }

    fn handle_command(&mut self, key: Key) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.cmdline.clear();
            }
            KeyCode::Enter => {
                let cmd = std::mem::take(&mut self.cmdline);
                self.mode = Mode::Normal;
                self.execute_ex(&cmd);
            }
            KeyCode::Backspace if self.cmdline.pop().is_none() => {
                self.mode = Mode::Normal;
            }
            KeyCode::Char(c) => self.cmdline.push(c),
            _ => {}
        }
    }

    fn execute_ex(&mut self, raw: &str) {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return;
        }
        if let Ok(n) = cmd.parse::<usize>() {
            let line = n
                .saturating_sub(1)
                .min(self.buffer.line_count().saturating_sub(1));
            self.cursor.line = line;
            self.cursor.col = self.first_non_blank(line);
            return;
        }

        let (name, bang, args) = split_ex(cmd);
        match name {
            "w" | "write" => self.ex_write(args),
            "q" | "quit" => self.ex_quit(bang),
            "wq" | "x" | "xit" | "exit" => {
                self.ex_write(args);
                self.should_quit = true;
            }
            "qa" | "qall" | "quita" | "quitall" => self.ex_quit(bang),
            "wa" | "wall" => self.ex_write(""),
            "wqa" | "xa" | "xall" => {
                self.ex_write("");
                self.should_quit = true;
            }
            "e" | "edit" => self.ex_edit(args, bang),
            "lua" => self.lua_queue.push(args.to_string()),
            "set" | "se" => self.message = format!("(set {args} — not yet implemented)"),
            "noh" | "nohlsearch" => {}
            other => self.message = format!("E492: Not an editor command: {other}"),
        }
    }

    fn ex_write(&mut self, args: &str) {
        let path = if args.is_empty() {
            None
        } else {
            Some(PathBuf::from(args))
        };
        match self.buffer.write(path) {
            Ok((bytes, lines)) => {
                let name = self
                    .buffer
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.message = format!("\"{name}\" {lines}L, {bytes}B written");
            }
            Err(e) => self.message = e.to_string(),
        }
    }

    fn ex_quit(&mut self, bang: bool) {
        if self.buffer.modified && !bang {
            self.message = "E37: No write since last change (add ! to override)".to_string();
        } else {
            self.should_quit = true;
        }
    }

    fn ex_edit(&mut self, args: &str, bang: bool) {
        if self.buffer.modified && !bang {
            self.message = "E37: No write since last change (add ! to override)".to_string();
            return;
        }
        if args.is_empty() {
            self.message = "E32: No file name".to_string();
            return;
        }
        match Buffer::from_file(args) {
            Ok(buf) => {
                self.buffer = buf;
                self.cursor = Cursor::default();
                self.top = 0;
                self.undo_stack.clear();
                self.redo_stack.clear();
            }
            Err(e) => self.message = e.to_string(),
        }
    }

    // ----- undo / redo ------------------------------------------------------

    fn push_undo(&mut self) {
        if self.snapshot_taken {
            return;
        }
        self.undo_stack.push(Snapshot {
            text: self.buffer.text.clone(),
            cursor: self.cursor,
        });
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            self.redo_stack.push(Snapshot {
                text: self.buffer.text.clone(),
                cursor: self.cursor,
            });
            self.buffer.text = snap.text;
            self.cursor = snap.cursor;
            self.buffer.modified = true;
            self.clamp_cursor();
        } else {
            self.message = "Already at oldest change".to_string();
        }
    }

    fn redo(&mut self) {
        if let Some(snap) = self.redo_stack.pop() {
            self.undo_stack.push(Snapshot {
                text: self.buffer.text.clone(),
                cursor: self.cursor,
            });
            self.buffer.text = snap.text;
            self.cursor = snap.cursor;
            self.buffer.modified = true;
            self.clamp_cursor();
        } else {
            self.message = "Already at newest change".to_string();
        }
    }

    // ----- cursor / scrolling helpers --------------------------------------

    fn cursor_char(&self) -> usize {
        self.buffer.byte_at(self.cursor.line, self.cursor.col)
    }

    fn char_at(&self, idx: usize) -> char {
        // Non-boundary bytes (inside a multi-byte char) read as blank rather
        // than panicking; cursor/operator positions are kept on boundaries.
        self.buffer.text.get_char(idx).unwrap_or(' ')
    }

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

    /// Snap the cursor column down to the nearest grapheme boundary (a no-op for
    /// ASCII), so byte offsets handed to the rope are always valid.
    fn snap_cursor(&mut self) {
        let s = self.buffer.line(self.cursor.line);
        self.cursor.col = unicode::floor_grapheme(&s, self.cursor.col.min(s.len()));
    }

    fn last_char_idx(&self) -> usize {
        // The trailing '\n' is never a valid cursor position.
        self.buffer.len_bytes().saturating_sub(1)
    }

    fn line_len(&self) -> usize {
        self.buffer.line_len(self.cursor.line)
    }

    fn first_non_blank(&self, line: usize) -> usize {
        let s = self.buffer.line(line);
        s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count()
    }

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

    fn move_vertical(&mut self, delta: i64, allow_eol: bool) {
        let new = (self.cursor.line as i64 + delta).max(0) as usize;
        self.cursor.line = new.min(self.buffer.line_count().saturating_sub(1));
        self.settle_desired_col(allow_eol);
        self.preserve_desired = true;
    }

    /// Place the cursor on the current line at the remembered desired *virtual*
    /// column (or end-of-line when `$`-sticky), clamped to the line and a grapheme
    /// boundary.
    fn settle_desired_col(&mut self, allow_eol: bool) {
        let s = self.buffer.line(self.cursor.line);
        // Furthest valid resting byte: past-end for insert/allow_eol, otherwise
        // the start of the last grapheme (normal mode can't rest past EOL).
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

    fn clamp_cursor(&mut self) {
        let last_line = self.buffer.line_count().saturating_sub(1);
        if self.cursor.line > last_line {
            self.cursor.line = last_line;
        }
        let len = self.line_len();
        let max_col = if self.mode.is_insert() {
            len
        } else {
            len.saturating_sub(1)
        };
        if self.cursor.col > max_col {
            self.cursor.col = max_col;
        }
        self.snap_cursor();
    }

    fn scroll_half(&mut self, down: bool) {
        let half = (self.text_height() / 2).max(1);
        if down {
            self.move_vertical(half as i64, false);
        } else {
            self.move_vertical(-(half as i64), false);
        }
        self.clamp_cursor();
    }

    fn scroll_page(&mut self, down: bool) {
        let page = self.text_height().saturating_sub(2).max(1);
        if down {
            self.move_vertical(page as i64, false);
        } else {
            self.move_vertical(-(page as i64), false);
        }
        self.clamp_cursor();
    }

    fn ensure_visible(&mut self) {
        let th = self.text_height();
        if self.cursor.line < self.top {
            self.top = self.cursor.line;
        } else if self.cursor.line >= self.top + th {
            self.top = self.cursor.line + 1 - th;
        }
    }

    // ----- pending-state bookkeeping ---------------------------------------

    fn effective_count(&self) -> usize {
        self.op_count.unwrap_or(1) * self.count.unwrap_or(1)
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.op_count = None;
        self.operator = None;
        self.gpending = false;
        self.pending_replace = false;
    }
}

impl Default for Editor {
    fn default() -> Self {
        Editor::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Blank,
    Word,
    Punct,
}

fn char_class(c: char) -> CharClass {
    if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
        CharClass::Blank
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Split an ex-command into `(name, bang, args)`.
fn split_ex(cmd: &str) -> (&str, bool, &str) {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let name = &cmd[..i];
    let mut bang = false;
    if i < bytes.len() && bytes[i] == b'!' {
        bang = true;
        i += 1;
    }
    let args = cmd[i..].trim();
    (name, bang, args)
}
