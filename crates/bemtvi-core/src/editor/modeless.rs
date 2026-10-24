//! The modeless editing model (`'modeless'`): a GUI and web session where a
//! modifiable buffer rests in Insert, so typing always edits and there is no Normal
//! mode to fall into. A read-only buffer (a `btv.view`, the quickfix list, help)
//! keeps resting in Normal, where its keys are buffer-local Normal maps.
//!
//! No new [`Mode`] exists for this. Insert already types and Select already holds a
//! range that typing replaces; what changes is where the editor comes to rest.
//! Rather than teach each of the many paths that drop into Normal (closing the
//! cmdline or a picker, finishing an ex command, a window or buffer switch) about
//! the setting, [`Editor::settle_modeless`] corrects the mode afterwards. The
//! server calls it after every applied key and command, before it diffs the mode
//! for `InsertEnter` / `ModeChanged`, and the core when a mapping or fed span
//! closes. A path that lands in Normal can't strand the user there.
//!
//! See `docs/plans/2026-10-22-modeless-editing.md`.

use super::operators::CLIPBOARD_UNAVAILABLE;
use super::undo::EditKind;
use super::{BufferId, Cursor, Editor, RegKind};
use crate::input::{Key, KeyCode};
use crate::unicode;
use crate::Mode;

/// Which kind of client is attached, as it reported itself at `btv_ui_attach`.
/// `'modeless'` is refused while a [`UiClient::Tui`] is attached: a terminal can't
/// report most of the chords the model needs. A client that says nothing (the test
/// harness, `--test-plugin`, a third-party RPC client) is [`UiClient::Unknown`] and
/// is not refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiClient {
    #[default]
    Unknown,
    Tui,
    Gui,
    Web,
}

impl UiClient {
    /// The client named by the attach capability `client` (`"tui"`, `"gui"`,
    /// `"web"`); anything else is [`UiClient::Unknown`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "tui" => UiClient::Tui,
            "gui" => UiClient::Gui,
            "web" => UiClient::Web,
            _ => UiClient::Unknown,
        }
    }

    /// The name [`UiClient::from_name`] reads, `""` for an unknown client.
    pub fn name(self) -> &'static str {
        match self {
            UiClient::Unknown => "",
            UiClient::Tui => "tui",
            UiClient::Gui => "gui",
            UiClient::Web => "web",
        }
    }
}

impl Editor {
    /// Record which client attached. A terminal client attaching to a session that
    /// has `'modeless'` on turns it off and says so, rather than leaving a model the
    /// client can't drive.
    pub fn set_ui_client(&mut self, client: UiClient) {
        self.ui_client = client;
        if client == UiClient::Tui && self.options.modeless {
            self.set_modeless(false);
            self.echo_err("modeless: turned off, the terminal client has no modeless editing");
        }
    }

    /// Whether `'modeless'` is on.
    pub fn modeless(&self) -> bool {
        self.options.modeless
    }

    /// The client that attached, [`UiClient::Unknown`] before any did.
    pub fn ui_client(&self) -> UiClient {
        self.ui_client
    }

    /// Write `'modeless'` (the `:set` and `btv.o` paths both land here), refusing to
    /// turn it on under a terminal client or inside a Helix session, then move the
    /// mode to match.
    pub(crate) fn set_modeless(&mut self, on: bool) {
        if on {
            if self.ui_client == UiClient::Tui {
                self.echo_err("modeless: not available in the terminal client");
                return;
            }
            if self.helix {
                self.echo_err("modeless: leave Helix mode (:helix) first");
                return;
            }
        }
        let was = self.options.modeless;
        self.global_base
            .set_scalar("modeless", &crate::options::OptionScalar::Bool(on));
        self.recompute_effective_options();
        if was == self.options.modeless {
            return;
        }
        if self.options.modeless {
            self.settle_modeless();
        } else if self.mode.is_insert() && !self.typing_undo_suppressed() {
            // Back to the vim model from where the session rested: leave Insert the
            // ordinary way, so the step closes and `InsertLeave` fires.
            self.handle_insert(Key::new(KeyCode::Esc));
        }
    }

    /// Whether the current window rests in Insert: `'modeless'` is on and its buffer
    /// takes edits. A read-only buffer rests in Normal even then.
    pub(crate) fn rests_in_insert(&self) -> bool {
        self.options.modeless && self.modifiable()
    }

    /// Whether the focused window's cursor is a modeless caret, for a client that
    /// draws its own: the session is typing or selecting.
    pub fn modeless_caret(&self) -> bool {
        self.rests_in_insert() && matches!(self.mode, Mode::Insert | Mode::Select)
    }

    /// Where a modeless selection's caret stands: its head, which may be past the
    /// last selected character (a line's end, the next line's start), where Select's
    /// inclusive cursor can't go. `None` without a modeless selection.
    pub(crate) fn modeless_caret_head(&self) -> Option<Cursor> {
        (self.modeless_caret() && self.mode == Mode::Select).then(|| self.caret_selection().head)
    }

    /// Whether the pointer over the text of a window onto buffer `id` is the text
    /// I-beam: a modeless session, over a buffer it would type into.
    pub fn text_pointer_over(&self, id: BufferId) -> bool {
        self.options.modeless && self.buffer_modifiable(id)
    }

    /// The state a scroll leaves the caret in — see [`ViewDetached`].
    fn view_state(&self) -> ViewState {
        ViewState {
            buffer: self.cur_buffer(),
            cursor: self.cursor,
            tick: self.buffer().changedtick,
            sel: self.nm_sel,
        }
    }

    /// After the wheel scrolled the focused window away from `before` (its `top` and
    /// `leftcol`): in a session that types here, leave the caret where it is, off
    /// screen if need be, and hold the view until the caret's state changes. Returns
    /// `false` (vim's rule then pulls the cursor into the view) anywhere else.
    pub(crate) fn detach_view(&mut self, before: (usize, usize)) -> bool {
        if !self.rests_in_insert() || self.has_secondary_cursors() {
            return false;
        }
        let state = self.view_state();
        self.hold_view(self.windows.current, state, before);
        true
    }

    /// The wheel scrolled `win`, which isn't focused, away from `before`: hold its
    /// view, and its caret, until focus comes back and something changes them. Only
    /// for a session that types there; the caller checked.
    pub(crate) fn detach_unfocused_view(
        &mut self,
        win: super::WindowId,
        buffer: BufferId,
        cursor: Cursor,
        before: (usize, usize),
    ) {
        let state = ViewState {
            buffer,
            cursor,
            tick: self.buffers.get(buffer).buffer.changedtick,
            sel: None,
        };
        self.hold_view(win, state, before);
    }

    /// Record `win`'s held view. A scroll on top of a scroll keeps the view the caret
    /// was first left in; a closed window's record is dropped along the way.
    fn hold_view(&mut self, win: super::WindowId, state: ViewState, before: (usize, usize)) {
        let caret_view = match self.view_detached.get(&win) {
            Some(d) if d.state == state => d.caret_view,
            _ => before,
        };
        let gone: Vec<_> = self
            .view_detached
            .keys()
            .copied()
            .filter(|&id| self.tree_of_window(id).is_none())
            .collect();
        for id in gone {
            self.view_detached.remove(&id);
        }
        self.view_detached
            .insert(win, ViewDetached { state, caret_view });
    }

    /// Bring a view the wheel scrolled away back to the caret, with the smallest
    /// scroll that shows it — for an input method about to compose there. A no-op
    /// when the caret is in view.
    pub fn reveal_caret(&mut self) {
        if self.view_detached.remove(&self.windows.current).is_some() {
            self.ensure_visible();
        }
    }

    /// Whether the view is still scrolled away from the caret: it was, and nothing
    /// has changed since. Drops the hold otherwise, so the caller brings the caret
    /// back into view.
    pub(crate) fn view_stays_detached(&mut self) -> bool {
        let win = self.windows.current;
        match self.view_detached.get(&win) {
            Some(d) if d.state == self.view_state() => true,
            Some(_) => {
                self.view_detached.remove(&win);
                false
            }
            None => false,
        }
    }

    /// Whether the key layout of a modeless session is in force: the session rests
    /// in Insert here and the editor is typing or selecting. The server matches
    /// keys in the `modeless` keymap layer then, ahead of the Insert maps.
    pub fn modeless_keys_active(&self) -> bool {
        self.rests_in_insert() && matches!(self.mode, Mode::Insert | Mode::Replace | Mode::Select)
    }

    /// Whether a typed `<Esc>` keeps Insert: in a modeless session, at the top level.
    /// A replay, `:normal`, a macro, a mapping's right-hand side or keys a plugin fed
    /// still leave Insert on `<Esc>` as they always have, because what follows it in
    /// them is Normal-mode keys.
    pub(crate) fn esc_keeps_insert(&self) -> bool {
        self.rests_in_insert() && !self.typing_undo_suppressed()
    }

    /// Bring the mode to where a modeless session rests: Insert over a modifiable
    /// buffer, Normal over a read-only one. Moves only from a clean Normal (no count,
    /// operator or `i_CTRL-O` in flight) or from an Insert the buffer can't take, and
    /// never inside a replay, mapping or fed span, whose keys expect the mode they
    /// set. A no-op when `'modeless'` is off.
    pub fn settle_modeless(&mut self) {
        if !self.options.modeless || self.typing_undo_suppressed() || self.insert_normal.is_some() {
            return;
        }
        if self.mode != Mode::Select {
            self.nm_sel = None;
        }
        match self.mode {
            Mode::Normal if self.pending.is_clean() && self.rests_in_insert() => {
                self.enter_resting_insert();
            }
            Mode::Insert | Mode::Replace if !self.modifiable() => self.leave_resting_insert(),
            // A selection is Select, never Visual, over a modifiable buffer: a mouse
            // drag or double-click builds a Visual one (the mouse engine's own
            // representation), which becomes Select here. Each drag event rebuilds
            // it from the press, so converting mid-gesture loses nothing.
            Mode::Visual | Mode::VisualLine if self.rests_in_insert() => {
                self.visual_to_caret_selection();
            }
            _ => {}
        }
    }

    /// Turn the Visual selection into the caret selection it covers. A plain drag's
    /// ends are caret positions already (the press point and the pointer); a word or
    /// line selection covers whole characters, so its far end is past the last one.
    fn visual_to_caret_selection(&mut self) {
        let (a, c) = (self.visual_anchor, self.cursor);
        let sel = if self.mode == Mode::VisualLine {
            let (top, bottom) = (a.line.min(c.line), a.line.max(c.line));
            let start = Cursor { line: top, col: 0 };
            let end = if bottom < self.last_line() {
                Cursor {
                    line: bottom + 1,
                    col: 0,
                }
            } else {
                Cursor {
                    line: bottom,
                    col: self.buffer().line_len(bottom),
                }
            };
            if c.line >= a.line {
                CaretSel {
                    anchor: start,
                    head: end,
                }
            } else {
                CaretSel {
                    anchor: end,
                    head: start,
                }
            }
        } else if self.mouse_gesture_count() == Some(1) {
            CaretSel { anchor: a, head: c }
        } else {
            let (ab, cb) = (self.anchor_byte(a), self.anchor_byte(c));
            let lo = self.caret_at_byte(ab.min(cb));
            let hi = self.caret_at_byte(self.next_grapheme_idx(ab.max(cb)));
            if cb >= ab {
                CaretSel {
                    anchor: lo,
                    head: hi,
                }
            } else {
                CaretSel {
                    anchor: hi,
                    head: lo,
                }
            }
        };
        self.record_visual_marks();
        self.mode = Mode::Insert;
        self.set_caret_selection(sel);
    }

    /// Type-over: delete the selection, without touching any register (a GUI
    /// editor's typing never clobbers the clipboard), and leave the caret at its
    /// start in Insert. The deletion opens its own undo step, which the typing
    /// that follows joins.
    pub(crate) fn replace_caret_selection(&mut self) {
        let sel = self.caret_selection();
        let (lo, hi) = self.caret_span(sel);
        self.record_visual_marks();
        if self.nm_sel.is_some() {
            self.mark_undo_point();
        }
        self.begin_replacing_step();
        self.push_undo();
        self.snapshot_taken = true;
        self.buffer_mut().remove(lo..hi);
        self.buffer_mut().modified = true;
        self.nm_sel = None;
        self.mode = Mode::Insert;
        self.set_cursor_char_insert(lo);
        self.desired_col = self.cursor_virtcol();
    }

    /// Open the insert session a modeless buffer rests in, as `i` would: an undo
    /// snapshot, an empty `".` accumulator and fresh typing-step state.
    fn enter_resting_insert(&mut self) {
        self.push_undo();
        self.snapshot_taken = true;
        self.mode = Mode::Insert;
        self.insert_text.clear();
        self.reset_typing_undo();
    }

    /// Close an insert session over a buffer that stopped taking edits (a `FileType`
    /// handler made it `nomodifiable`), leaving it in Normal like `<Esc>` without the
    /// cursor backstep.
    fn leave_resting_insert(&mut self) {
        self.end_snippet();
        self.close_completion();
        if self.undo_batch_depth == 0 {
            self.snapshot_taken = false;
        }
        self.mode = Mode::Normal;
        self.clamp_cursor();
    }
}

/// What a modeless view scrolled away from its caret waits on: the view stays put
/// while all of this is as the scroll left it, and follows the caret again the
/// moment any of it changes — the caret or selection moves, the text changes, the
/// mode or window or buffer does. That is a GUI editor's rule: scrolling never moves
/// the caret, and typing or moving it brings it back into view.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ViewDetached {
    state: ViewState,
    /// The view (`top`, `leftcol`) the caret was in when the wheel took it away, for
    /// a page key to come back to.
    caret_view: (usize, usize),
}

/// The caret's state a [`ViewDetached`] holds the view on. The mode isn't part of
/// it: focus passes through Normal on its way into a window, and a mode change by
/// itself moves nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ViewState {
    buffer: BufferId,
    cursor: Cursor,
    tick: u64,
    sel: Option<CaretSel>,
}

/// A modeless selection in caret terms: `anchor` stays put while `head` moves, and
/// both are caret positions *between* characters (a column may be the line's
/// length, past its last character). The selected text is the byte span between
/// them, end-exclusive. Select mode's own representation is inclusive (the cursor
/// sits on the last selected character), which can't express a selection that ends
/// on a line break or a caret moving left, so the modeless model keeps this and
/// projects it onto `visual_anchor` / `cursor` for drawing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaretSel {
    pub(crate) anchor: Cursor,
    pub(crate) head: Cursor,
}

impl Editor {
    /// The current selection in caret terms. In Insert it is empty, at the cursor.
    /// A Select a plugin entered (a snippet placeholder) has no caret record, so it
    /// is read back from the inclusive range: anchor at the start, head past the end.
    pub(crate) fn caret_selection(&self) -> CaretSel {
        if self.mode != Mode::Select {
            return CaretSel {
                anchor: self.cursor,
                head: self.cursor,
            };
        }
        if let Some(sel) = self.nm_sel {
            return sel;
        }
        let a = self.anchor_byte(self.visual_anchor);
        let c = self.anchor_byte(self.cursor);
        let (lo, hi) = (a.min(c), self.next_grapheme_idx(a.max(c)));
        let (lo, hi) = (self.caret_at_byte(lo), self.caret_at_byte(hi));
        if c >= a {
            CaretSel {
                anchor: lo,
                head: hi,
            }
        } else {
            CaretSel {
                anchor: hi,
                head: lo,
            }
        }
    }

    /// The selection an undo state records, as `(anchor, head)` byte offsets — see
    /// [`Snapshot::selection`].
    pub(crate) fn undo_selection(&self) -> Option<(usize, usize)> {
        let sel = self.nm_sel.filter(|_| self.mode == Mode::Select)?;
        Some((self.anchor_byte(sel.anchor), self.anchor_byte(sel.head)))
    }

    /// The caret position at byte `idx`, which may be a line's end (past its last
    /// character) but never past the last line's.
    pub(crate) fn caret_at_byte(&self, idx: usize) -> Cursor {
        let last = self.last_line();
        let end = self.buffer().byte_at(last, self.buffer().line_len(last));
        let idx = idx.min(end);
        let line = self.buffer().byte_to_line(idx);
        Cursor {
            line,
            col: idx - self.buffer().line_start(line),
        }
    }

    /// Whether the selection covers more than one line.
    pub(crate) fn selection_spans_lines(&self) -> bool {
        let sel = self.caret_selection();
        sel.anchor.line != sel.head.line
    }

    /// The selection's byte span, `[lo, hi)`.
    pub(crate) fn caret_span(&self, sel: CaretSel) -> (usize, usize) {
        let a = self.anchor_byte(sel.anchor);
        let h = self.anchor_byte(sel.head);
        (a.min(h), a.max(h))
    }

    /// Make `sel` the selection: Select mode over it, or Insert with the caret at its
    /// head when it is empty.
    pub(crate) fn set_caret_selection(&mut self, sel: CaretSel) {
        if sel.anchor == sel.head {
            self.collapse_selection_to(sel.head);
            return;
        }
        let (lo, hi) = self.caret_span(sel);
        // Select draws inclusively: from the first selected character to the last,
        // with the cursor on the end the head is at.
        let first = self.caret_at_byte(lo);
        let last = self.caret_at_byte(self.prev_grapheme_idx(hi));
        let forward = self.anchor_byte(sel.head) > self.anchor_byte(sel.anchor);
        let (anchor, cursor) = if forward {
            (first, last)
        } else {
            (last, first)
        };
        self.close_completion();
        self.visual_anchor = anchor;
        self.cursor = cursor;
        self.select_linewise = false;
        self.select_escape_insert = true;
        self.mode = Mode::Select;
        self.nm_sel = Some(sel);
    }

    /// Drop the selection, leaving the caret at `at` in Insert.
    pub(crate) fn collapse_selection_to(&mut self, at: Cursor) {
        if self.mode == Mode::Select {
            self.record_visual_marks();
        }
        self.nm_sel = None;
        self.mode = Mode::Insert;
        self.cursor = at;
        self.clamp_cursor();
    }

    /// Navigation in a modeless session, for Insert and Select alike: arrows,
    /// `<Home>`/`<End>`, `<PageUp>`/`<PageDown>`, with `<C-…>` for words and the
    /// buffer's ends and `<S-…>` to select. Returns `false` for any other key, and
    /// for these keys outside a modeless session or with several cursors (the
    /// multi-cursor engine keeps its own motions).
    pub(crate) fn modeless_nav(&mut self, key: Key) -> bool {
        if !self.rests_in_insert() || self.has_secondary_cursors() {
            return false;
        }
        // The macOS spellings: Command with an arrow goes to the line's edges or the
        // buffer's ends, Option (Alt) with Left/Right moves by word. Each folds onto
        // the key it means, Shift kept.
        let as_key = |code: KeyCode, ctrl: bool| Key {
            code,
            ctrl,
            alt: false,
            shift: key.shift,
            cmd: false,
        };
        let key = if key.cmd {
            match key.code {
                KeyCode::Left => as_key(KeyCode::Home, false),
                KeyCode::Right => as_key(KeyCode::End, false),
                KeyCode::Up => as_key(KeyCode::Home, true),
                KeyCode::Down => as_key(KeyCode::End, true),
                _ => return false,
            }
        } else if key.alt {
            match key.code {
                KeyCode::Left | KeyCode::Right => as_key(key.code, true),
                _ => return false,
            }
        } else {
            key
        };
        let vertical = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
        );
        if !vertical
            && !matches!(
                key.code,
                KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
            )
        {
            return false;
        }
        let sel = self.caret_selection();
        let selecting = self.mode == Mode::Select;
        // An arrow without Shift collapses a selection onto the side it points at,
        // the way every GUI editor does, rather than moving from the head.
        if selecting && !key.shift && !key.ctrl {
            let (lo, hi) = self.caret_span(sel);
            match key.code {
                KeyCode::Left => {
                    let at = self.caret_at_byte(lo);
                    self.collapse_selection_to(at);
                    self.nav_settle_col(vertical);
                    return true;
                }
                KeyCode::Right => {
                    let at = self.caret_at_byte(hi);
                    self.collapse_selection_to(at);
                    self.nav_settle_col(vertical);
                    return true;
                }
                _ => {}
            }
        }
        let head = self.caret_motion(sel.head, key);
        // A page key moves the view as far as the caret, so the caret keeps its row
        // on screen (and the move slides), rather than the view creeping a line to
        // follow it.
        if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            // Scrolled away, a page key pages from the caret, as in a GUI editor: back
            // to the view the caret was left in first.
            if let Some(d) = self.view_detached.remove(&self.windows.current) {
                (self.top, self.leftcol) = d.caret_view;
                self.ensure_visible();
            }
            let th = self.text_height();
            let by = head.line as i64 - sel.head.line as i64;
            let top = (self.top as i64 + by).clamp(0, self.max_scroll_top(th) as i64);
            self.top = top as usize;
        }
        if key.shift {
            self.set_caret_selection(CaretSel {
                anchor: sel.anchor,
                head,
            });
        } else {
            self.collapse_selection_to(head);
        }
        // Keep the sticky column on the caret, not on Select's inclusive cursor.
        if !vertical {
            self.desired_col = self.virtcol_of(head);
            self.desired_eol = false;
        }
        self.preserve_desired = true;
        true
    }

    fn nav_settle_col(&mut self, vertical: bool) {
        if !vertical {
            self.desired_col = self.cursor_virtcol();
            self.desired_eol = false;
            self.preserve_desired = true;
        }
    }

    fn virtcol_of(&self, at: Cursor) -> usize {
        let s = self.buffer().line_cow(at.line);
        unicode::virtcol(&s, at.col, self.tabstop())
    }

    /// Where the caret at `from` goes for navigation key `key`.
    fn caret_motion(&mut self, from: Cursor, key: Key) -> Cursor {
        let last = self.last_line();
        let len = |ed: &Editor, line: usize| ed.buffer().line_len(line);
        match key.code {
            KeyCode::Left if key.ctrl => self.caret_word_left(from),
            KeyCode::Right if key.ctrl => self.caret_word_right(from),
            KeyCode::Left => {
                if from.col > 0 {
                    let s = self.buffer().line_cow(from.line);
                    Cursor {
                        line: from.line,
                        col: unicode::prev_grapheme(&s, from.col),
                    }
                } else if from.line > 0 {
                    Cursor {
                        line: from.line - 1,
                        col: len(self, from.line - 1),
                    }
                } else {
                    from
                }
            }
            KeyCode::Right => {
                if from.col < len(self, from.line) {
                    let s = self.buffer().line_cow(from.line);
                    Cursor {
                        line: from.line,
                        col: unicode::next_grapheme(&s, from.col),
                    }
                } else if from.line < last {
                    Cursor {
                        line: from.line + 1,
                        col: 0,
                    }
                } else {
                    from
                }
            }
            KeyCode::Home if key.ctrl => Cursor { line: 0, col: 0 },
            KeyCode::End if key.ctrl => Cursor {
                line: last,
                col: len(self, last),
            },
            // Smart Home: the first non-blank, or column 0 when already there.
            KeyCode::Home => {
                let fnb = self.first_non_blank(from.line);
                Cursor {
                    line: from.line,
                    col: if from.col == fnb { 0 } else { fnb },
                }
            }
            KeyCode::End => Cursor {
                line: from.line,
                col: len(self, from.line),
            },
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                let page = self.text_height().max(1) as i64;
                let delta = match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -page,
                    _ => page,
                };
                // Up from the first line goes to its start, down from the last to its
                // end, as in a GUI editor.
                let target = from.line as i64 + delta;
                if target < 0 {
                    return Cursor { line: 0, col: 0 };
                }
                if target > last as i64 {
                    return Cursor {
                        line: last,
                        col: len(self, last),
                    };
                }
                let line = target as usize;
                let s = self.buffer().line_cow(line);
                let col = if self.desired_eol {
                    s.len()
                } else {
                    unicode::byte_at_virtcol(&s, self.desired_col, self.tabstop()).min(s.len())
                };
                Cursor {
                    line,
                    col: unicode::floor_grapheme(&s, col),
                }
            }
            _ => from,
        }
    }

    /// `<C-Left>`: back to the start of the word before the caret, over any blanks
    /// in between; at a line's start, to the end of the line above.
    fn caret_word_left(&self, from: Cursor) -> Cursor {
        if from.col == 0 {
            return if from.line > 0 {
                Cursor {
                    line: from.line - 1,
                    col: self.buffer().line_len(from.line - 1),
                }
            } else {
                from
            };
        }
        let s = self.buffer().line_cow(from.line);
        let chars: Vec<(usize, char)> = s[..from.col].char_indices().collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].1.is_whitespace() {
            i -= 1;
        }
        if i > 0 {
            let class = char_class(chars[i - 1].1);
            while i > 0 && !chars[i - 1].1.is_whitespace() && char_class(chars[i - 1].1) == class {
                i -= 1;
            }
        }
        Cursor {
            line: from.line,
            col: chars.get(i).map_or(0, |c| c.0),
        }
    }

    /// `<C-Right>`: forward over any blanks, then to the end of the word after them;
    /// at a line's end, to the start of the line below.
    fn caret_word_right(&self, from: Cursor) -> Cursor {
        let s = self.buffer().line_cow(from.line);
        if from.col >= s.len() {
            return if from.line < self.last_line() {
                Cursor {
                    line: from.line + 1,
                    col: 0,
                }
            } else {
                from
            };
        }
        let rest = &s[from.col..];
        let mut it = rest.char_indices().peekable();
        while it.peek().is_some_and(|(_, c)| c.is_whitespace()) {
            it.next();
        }
        let mut end = it.peek().map_or(rest.len(), |(i, _)| *i);
        if let Some(&(_, first)) = it.peek() {
            let class = char_class(first);
            for (i, c) in it {
                if c.is_whitespace() || char_class(c) != class {
                    end = i;
                    break;
                }
                end = i + c.len_utf8();
            }
        }
        Cursor {
            line: from.line,
            col: from.col + end,
        }
    }
}

/// Whether `code` is one of the navigation keys a modeless session moves with.
pub(crate) fn is_nav_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
    )
}

/// A word character or punctuation, for the GUI word motions: a run of either is
/// one word.
fn char_class(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    /// The named-verb seam the modeless key layout binds to
    /// (`btv._modeless_action(name)` → this), so every key is rebindable by name.
    /// Each verb is one undo step of its own. Unknown names, and any verb while
    /// `'modeless'` is off, fail loud; the server reports the `Err`.
    pub fn apply_modeless_action(&mut self, name: &str) -> Result<(), String> {
        if !self.options.modeless {
            return Err(format!("modeless action {name}: 'modeless' is off"));
        }
        if !matches!(self.mode, Mode::Insert | Mode::Replace | Mode::Select) {
            return Err(format!(
                "modeless action {name}: needs an editable buffer (mode is {})",
                self.mode.short_code()
            ));
        }
        match name {
            "copy" => {
                self.modeless_copy();
            }
            "cut" => {
                if self.modeless_copy() {
                    self.delete_selection_or_line();
                }
            }
            "paste" => self.modeless_paste(),
            "undo" => self.modeless_undo(false),
            "redo" => self.modeless_undo(true),
            "select_all" => {
                let last = self.last_line();
                let head = Cursor {
                    line: last,
                    col: self.buffer().line_len(last),
                };
                self.set_caret_selection(CaretSel {
                    anchor: Cursor { line: 0, col: 0 },
                    head,
                });
            }
            "delete_word_back" => self.delete_word(false),
            "delete_word_forward" => self.delete_word(true),
            "indent" => self.shift_selected_lines(true),
            "outdent" => self.shift_selected_lines(false),
            "duplicate_line" => self.duplicate_lines(),
            "move_line_up" => self.move_lines(false),
            "move_line_down" => self.move_lines(true),
            "search_next" => self.modeless_search_again(true),
            "search_prev" => self.modeless_search_again(false),
            "reveal_caret" => self.reveal_caret(),
            _ => return Err(format!("unknown modeless action: {name}")),
        }
        Ok(())
    }

    /// Find the next (`forward`) or previous match of the last search, from the
    /// selection's far edge so a selected match is never found again, and select it.
    fn modeless_search_again(&mut self, forward: bool) {
        let Some(last_dir) = self.last_search.as_ref().map(|s| s.1) else {
            self.echo_err("E35: No previous regular expression");
            return;
        };
        let sel = self.take_selection();
        let (lo, hi) = self.caret_span(sel);
        self.cursor = self.caret_at_byte(if forward { hi } else { lo });
        let same = forward == (last_dir == super::SearchDir::Forward);
        self.search_repeat(same, 1);
    }

    /// Leave Select for Insert, returning the selection it held, so a verb can run
    /// its edit as an Insert-session step. Undo returns to the selection.
    fn take_selection(&mut self) -> CaretSel {
        let sel = self.caret_selection();
        if self.mode == Mode::Select {
            if self.nm_sel.is_some() {
                self.mark_undo_point();
            }
            self.record_visual_marks();
            self.nm_sel = None;
            self.mode = Mode::Insert;
        }
        sel
    }

    /// The lines a line verb acts on: the selection's, or the caret's. A selection
    /// ending at the start of a line doesn't take that line.
    fn selected_lines(&self, sel: CaretSel) -> (usize, usize) {
        let (a, h) = (sel.anchor, sel.head);
        let (lo, hi) = if (a.line, a.col) <= (h.line, h.col) {
            (a, h)
        } else {
            (h, a)
        };
        let last = if hi.line > lo.line && hi.col == 0 {
            hi.line - 1
        } else {
            hi.line
        };
        (lo.line, last)
    }

    /// Run `edit` as an undo step of its own inside the insert session: the typing
    /// before it closes here. The typing after it opens a new step by itself, since
    /// a typed edit that finds the text changed under it always does.
    fn as_own_step(&mut self, edit: impl FnOnce(&mut Self)) {
        self.end_typing_step();
        edit(self);
        self.buffer_mut().normalize();
    }

    /// Copy the selection to the clipboard, or the caret's whole line when nothing
    /// is selected. Keeps the selection. `false` (and a loud message) with no
    /// clipboard provider.
    fn modeless_copy(&mut self) -> bool {
        if self.clipboard.is_none() {
            self.echo_err(CLIPBOARD_UNAVAILABLE);
            return false;
        }
        let sel = self.caret_selection();
        let (text, kind) = if sel.anchor == sel.head {
            let line = self.cursor.line;
            let start = self.buffer().line_start(line);
            let end = start + self.buffer().line_len(line);
            let mut text = self.buffer().text.slice(start..end).to_string();
            text.push('\n');
            (text, RegKind::Line)
        } else {
            let (lo, hi) = self.caret_span(sel);
            (self.buffer().text.slice(lo..hi).to_string(), RegKind::Char)
        };
        self.clipboard_write(text, kind);
        true
    }

    /// Delete the selection, or the caret's whole line when nothing is selected
    /// (the second half of `cut`).
    fn delete_selection_or_line(&mut self) {
        let sel = self.take_selection();
        if sel.anchor != sel.head {
            let (lo, hi) = self.caret_span(sel);
            self.as_own_step(|ed| {
                ed.buffer_mut().remove(lo..hi);
                ed.buffer_mut().modified = true;
                ed.set_cursor_char_insert(lo);
            });
            return;
        }
        let line = self.cursor.line;
        self.as_own_step(|ed| {
            let start = ed.buffer().line_start(line);
            let next = ed.buffer().line_start(line + 1);
            ed.buffer_mut().remove(start..next);
            ed.buffer_mut().modified = true;
            let line = line.min(ed.last_line());
            ed.cursor = Cursor { line, col: 0 };
            ed.clamp_cursor();
        });
    }

    /// Paste the clipboard over the selection, or at the caret. A whole-line copy
    /// with nothing selected goes in above the caret's line, the caret riding down
    /// with its text.
    fn modeless_paste(&mut self) {
        if self.clipboard.is_none() {
            self.echo_err(CLIPBOARD_UNAVAILABLE);
            return;
        }
        let Some((mut text, kind)) = self.register_text(Some('+')).filter(|(t, _)| !t.is_empty())
        else {
            self.echo_err("E353: Nothing in register +");
            return;
        };
        let sel = self.take_selection();
        if sel.anchor == sel.head && kind == RegKind::Line {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let added = text.matches('\n').count();
            let caret = self.cursor;
            self.as_own_step(|ed| {
                let at = ed.buffer().line_start(caret.line);
                ed.buffer_mut().insert(at, &text);
                ed.buffer_mut().modified = true;
                ed.cursor = Cursor {
                    line: caret.line + added,
                    col: caret.col,
                };
            });
            return;
        }
        let (lo, hi) = self.caret_span(sel);
        self.as_own_step(|ed| {
            ed.buffer_mut().remove(lo..hi);
            ed.buffer_mut().insert(lo, &text);
            ed.buffer_mut().modified = true;
            ed.set_cursor_char_insert(lo + text.len());
        });
        self.desired_col = self.cursor_virtcol();
    }

    /// Undo or redo one step without leaving Insert. The typing step in progress
    /// closes first, so it is the step undone; then the session opens a fresh
    /// group, which records nothing until the next edit and so leaves redo alone.
    /// The state reached comes back with the selection it was left with.
    fn modeless_undo(&mut self, redo: bool) {
        let sel = self.take_selection();
        self.cursor = sel.head;
        self.snapshot_taken = false;
        if redo {
            self.redo();
        } else {
            self.undo();
        }
        self.mode = Mode::Insert;
        self.clamp_cursor();
        if let Some((anchor, head)) = self.undo_state_selection() {
            self.set_caret_selection(CaretSel {
                anchor: self.caret_at_byte(anchor),
                head: self.caret_at_byte(head),
            });
        }
        self.push_undo();
        self.snapshot_taken = true;
        self.reset_typing_undo();
        self.desired_col = self.cursor_virtcol();
    }

    /// `<C-BS>` / `<C-Del>`: delete the selection, or back (forward) to the word
    /// boundary the matching `<C-Left>` (`<C-Right>`) would reach. A deletion like
    /// `<BS>`, so it groups with the deleting around it.
    fn delete_word(&mut self, forward: bool) {
        let sel = self.caret_selection();
        if sel.anchor != sel.head {
            self.replace_caret_selection();
            return;
        }
        let from = self.cursor;
        let to = if forward {
            self.caret_word_right(from)
        } else {
            self.caret_word_left(from)
        };
        let (a, b) = (self.anchor_byte(from), self.anchor_byte(to));
        if a == b {
            return;
        }
        self.before_typed_edit(EditKind::Delete, None);
        self.buffer_mut().remove(a.min(b)..a.max(b));
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.set_cursor_char_insert(a.min(b));
        self.desired_col = self.cursor_virtcol();
        self.after_typed_edit();
    }

    /// `<Tab>` over a multi-line selection, `<S-Tab>` anywhere: shift the lines by
    /// one `'shiftwidth'`. A selection stays selected, grown to whole lines; a caret
    /// keeps its place in the text.
    fn shift_selected_lines(&mut self, right: bool) {
        let had = self.mode == Mode::Select;
        let sel = self.take_selection();
        let (first, last) = self.selected_lines(sel);
        let caret = self.cursor;
        let before = self.buffer().line_len(caret.line);
        self.as_own_step(|ed| ed.shift_lines(first, last, right, 1));
        if had {
            let end = self.buffer().line_len(last);
            self.set_caret_selection(CaretSel {
                anchor: Cursor {
                    line: first,
                    col: 0,
                },
                head: Cursor {
                    line: last,
                    col: end,
                },
            });
        } else {
            let after = self.buffer().line_len(caret.line);
            let col = (caret.col + after).saturating_sub(before).min(after);
            self.cursor = Cursor {
                line: caret.line,
                col,
            };
        }
        self.message.clear();
    }

    /// Duplicate the selected lines (or the caret's) below themselves, moving the
    /// caret or selection onto the copy.
    fn duplicate_lines(&mut self) {
        let had = self.mode == Mode::Select;
        let sel = self.take_selection();
        let (first, last) = self.selected_lines(sel);
        let n = last - first + 1;
        self.as_own_step(|ed| {
            let start = ed.buffer().line_start(first);
            let end = ed.buffer().line_start(last + 1);
            let text = ed.buffer().text.slice(start..end).to_string();
            ed.buffer_mut().insert(end, &text);
            ed.buffer_mut().modified = true;
        });
        self.shift_caret_lines(had, sel, n as i64);
    }

    /// Move the selected lines (or the caret's) one line up or down, past their
    /// neighbor, carrying the caret or selection with them.
    fn move_lines(&mut self, down: bool) {
        let had = self.mode == Mode::Select;
        let sel = self.take_selection();
        let (first, last) = self.selected_lines(sel);
        if (down && last >= self.last_line()) || (!down && first == 0) {
            if had {
                self.set_caret_selection(sel);
            }
            return;
        }
        self.as_own_step(|ed| {
            // Swap the block with the neighbor line: lift the neighbor out and put it
            // on the block's other side.
            let neighbor = if down { last + 1 } else { first - 1 };
            let n_start = ed.buffer().line_start(neighbor);
            let n_end = ed.buffer().line_start(neighbor + 1);
            let text = ed.buffer().text.slice(n_start..n_end).to_string();
            ed.buffer_mut().remove(n_start..n_end);
            let at = if down {
                ed.buffer().line_start(first)
            } else {
                ed.buffer().line_start(last)
            };
            ed.buffer_mut().insert(at, &text);
            ed.buffer_mut().modified = true;
        });
        self.shift_caret_lines(had, sel, if down { 1 } else { -1 });
    }

    /// After a line verb: move the caret, or the selection, `by` lines.
    fn shift_caret_lines(&mut self, had: bool, sel: CaretSel, by: i64) {
        let move_by = |c: Cursor| Cursor {
            line: (c.line as i64 + by).max(0) as usize,
            col: c.col,
        };
        if had {
            self.set_caret_selection(CaretSel {
                anchor: move_by(sel.anchor),
                head: move_by(sel.head),
            });
        } else {
            self.cursor = move_by(sel.head);
            self.clamp_cursor();
        }
    }
}
