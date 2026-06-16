//! **Plugin-owned views** (`nx.view`) — a read-only, plugin-controlled content
//! surface that can be mounted in a dock or a split. The generalization of the
//! bottom [`Panel`](super::Panel) off its bottom-edge assumption: where the panel
//! is a single bottom strip, a view is an ordinary [`Buffer`] (carrying
//! `view: Some(id)`) shown in any window, whose lines a plugin replaces wholesale
//! and whose `<CR>` dispatches to a Lua `on_select` callback.
//!
//! Like the directory listing ([`explorer`](super::explorer)), a view buffer is
//! inert to the editing grammar: [`Editor::input`] routes its normal-mode keys
//! through the `view` keymap bucket ([`Editor::apply_view_action`]) instead of the
//! state machine, so navigation works but text-mutating keys can't corrupt the
//! plugin-owned content. Decoration (icons / indent guides / signs) rides the
//! ordinary extmark layer (`nvim_buf_set_extmark`), so it needs nothing here.

use super::*;
use crate::buffer::Buffer;
use crate::input::Key;
use ropey::Rope;

/// One live `nx.view` surface, tracked in [`Editor::views`] by its Lua handle id.
pub(crate) struct ViewState {
    /// The backing read-only buffer (carries `view: Some(id)`).
    pub buf: BufferId,
    /// How the view is currently shown, or `None` while unmounted.
    pub mount: Option<ViewMount>,
}

/// Where a [`ViewState`] is mounted — the window region `focus` / `unmount` act on.
pub(crate) enum ViewMount {
    /// In the permanent dock on this side (`v:mount{ dock = … }`).
    Dock(DockSide),
    /// In a split window of the main editor area (`v:mount{ split = … }`).
    Split(WindowId),
}

impl Editor {
    /// Whether the current buffer is a plugin-owned `nx.view` surface. When true,
    /// [`Editor::key_context`] reports [`KeyContext::View`] so the matcher routes
    /// normal-mode keys through the `view` keymap bucket
    /// ([`Editor::apply_view_action`]) rather than the editing state machine.
    pub(crate) fn is_view_buffer(&self) -> bool {
        self.buffer().view.is_some()
    }

    /// `nx.view.create{ name, filetype }` — register view `id` and mint its backing
    /// read-only buffer (marked `view: Some(id)`, with `filetype` applied for
    /// treesitter / decoration). Idempotent: a second create on a live id is a
    /// no-op, so a re-run config doesn't strand a second buffer. The buffer is
    /// created off-screen (no window) — it becomes visible only on
    /// [`mount_view`](Editor::mount_view).
    pub fn create_view(&mut self, id: u64, _name: String, filetype: String) {
        if self.views.contains_key(&id) {
            return;
        }
        let mut buf = Buffer::empty();
        buf.view = Some(id);
        let buf_id = self.add_buffer(buf);
        if !filetype.is_empty() {
            self.set_filetype(buf_id, &filetype);
        }
        self.views.insert(
            id,
            ViewState {
                buf: buf_id,
                mount: None,
            },
        );
    }

    /// The backing buffer of view `id` (the `nx._view_buf` mirror / extmark target),
    /// or `None` if no such view is live.
    pub fn view_buffer(&self, id: u64) -> Option<BufferId> {
        self.views.get(&id).map(|v| v.buf)
    }

    /// The `nx.view` Rust→Lua mirror snapshot: `(view id, backing buffer number,
    /// 1-based cursor line)` per live view, for `nx._view_buf` / `nx._view_line`.
    /// The line is the current cursor's when the view is the focused buffer (so
    /// `v:line()` reads it during an action), else `1` — a parked dock view's window
    /// cursor isn't on the live `self.cursor`, and reads only happen while focused.
    pub fn view_mirror(&self) -> Vec<(u64, u64, u64)> {
        self.views
            .iter()
            .map(|(&id, v)| {
                let line1 = if v.buf == self.cur_buffer() {
                    self.cursor.line as u64 + 1
                } else {
                    1
                };
                (id, v.buf.0, line1)
            })
            .collect()
    }

    /// `v:set_lines(lines)` — replace view `id`'s content wholesale, rooting a fresh
    /// undo history at it (the content is plugin-derived, never user-edited, so the
    /// old history is meaningless). Re-clamps the cursor when the view is the
    /// current buffer. A no-op for an unknown id. The buffer-mutation API is absent
    /// from Lua by design (ADR 0002); this is the sanctioned write path for a view's
    /// own lines, owned by the core.
    pub fn set_view_lines(&mut self, id: u64, lines: Vec<String>) {
        let Some(buf) = self.view_buffer(id) else {
            return;
        };
        let mut text = lines.join("\n");
        text.push('\n');
        let is_current = buf == self.cur_buffer();
        let ob = self.buffers.get_mut(buf);
        ob.buffer.text = Rope::from_str(&text);
        ob.buffer.normalize();
        ob.buffer.mark_resync();
        ob.undo = UndoTree::new(&ob.buffer);
        ob.saved_seq = Some(ob.undo.cur_seq());
        if is_current {
            self.cursor.line = self.cursor.line.min(self.last_line());
            self.cursor.col = 0;
            self.desired_col = 0;
            self.desired_eol = false;
            self.clamp_cursor();
            self.ensure_visible();
        }
    }

    /// `v:mount{ dock = side, size = … }` — show view `id` in the dock on `side`,
    /// sized to `size` (its default when `None`), and focus it (the dock takes
    /// focus, like [`open_dock`](Editor::open_dock)). Remounting moves the view to
    /// the new dock. A no-op for an unknown id or side.
    pub fn mount_view_dock(&mut self, id: u64, side: &str, size: Option<usize>) {
        let Some(buf) = self.view_buffer(id) else {
            return;
        };
        let Some(s) = DockSide::from_keyword(side) else {
            self.echo(format!("E474: Invalid dock side: {side}"));
            return;
        };
        self.unmount_view(id);
        self.open_dock(s, size.unwrap_or_else(|| s.default_size()), Some(buf));
        if let Some(v) = self.views.get_mut(&id) {
            v.mount = Some(ViewMount::Dock(s));
        }
    }

    /// `v:mount{ split = "vsplit" | "split" }` — show view `id` in a new split of
    /// the main editor area (vertical for `vsplit`, horizontal otherwise) and focus
    /// it. A no-op for an unknown id.
    pub fn mount_view_split(&mut self, id: u64, vertical: bool) {
        let Some(buf) = self.view_buffer(id) else {
            return;
        };
        self.unmount_view(id);
        // A split must happen in the main layer, never inside a dock.
        self.ensure_main_layer();
        let dir = if vertical {
            SplitDir::Vertical
        } else {
            SplitDir::Horizontal
        };
        self.split(dir);
        let win = self.current_window_id();
        self.set_window_buffer(win, buf);
        if let Some(v) = self.views.get_mut(&id) {
            v.mount = Some(ViewMount::Split(win));
        }
    }

    /// `v:focus()` — move focus to the window showing view `id`: the dock for a
    /// dock-mounted view, the split window for a split-mounted one. A no-op for an
    /// unknown / unmounted id (or a split window the user already closed).
    pub fn focus_view(&mut self, id: u64) {
        match self.views.get(&id).and_then(|v| v.mount.as_ref()) {
            Some(ViewMount::Dock(s)) => {
                let s = *s;
                self.focus_dock(s);
            }
            Some(ViewMount::Split(win)) => {
                let win = *win;
                self.ensure_main_layer();
                if self.windows.try_get(win).is_some() {
                    self.set_current_window(win);
                }
            }
            None => {}
        }
    }

    /// `v:unmount()` — remove view `id` from view (close its dock, or its split
    /// window), leaving the backing buffer alive so a later `mount` reshows it. A
    /// no-op for an unknown / already-unmounted id.
    pub fn unmount_view(&mut self, id: u64) {
        let mount = self.views.get_mut(&id).and_then(|v| v.mount.take());
        match mount {
            Some(ViewMount::Dock(s)) => self.close_dock(s),
            Some(ViewMount::Split(win)) if self.windows.try_get(win).is_some() => {
                self.close_window_by_id(win, true);
            }
            _ => {}
        }
    }

    /// `v:close()` / handle GC — unmount view `id` and drop its backing buffer and
    /// registry entry. A no-op for an unknown id.
    pub fn destroy_view(&mut self, id: u64) {
        self.unmount_view(id);
        if let Some(v) = self.views.remove(&id) {
            self.delete_buffer(v.buf, true);
        }
    }

    /// Apply a named `view` action, dispatched by a `view`-bucket keymap (the
    /// default maps in `prelude/keymap.lua`, or a user override) while a `nx.view`
    /// buffer is focused in normal mode. The vertical motions
    /// (`next`/`prev`/`first`/`last`/`half_down`/`half_up`/`page_down`/`page_up`)
    /// move the cursor; `confirm` records a `<CR>` select on the cursor line for the
    /// server to deliver to the view's Lua `on_select`. An unknown name fails loud
    /// per the no-silent-stub rule. The residual non-map key is `:`/`/`/`?` (handled
    /// in [`Editor::handle_view_text`]); every other editing key is inert.
    pub fn apply_view_action(&mut self, action: &str) -> Result<(), String> {
        self.message.clear();

        if action == "confirm" {
            if let Some(id) = self.buffer().view {
                self.view_selects.push((id, self.cursor.line));
            }
            return Ok(());
        }

        let last = self.last_line();
        let half = (self.text_height() / 2).max(1);
        let page = self.text_height().saturating_sub(2).max(1);
        let cur = self.cursor.line;
        let line = match action {
            "next" => (cur + 1).min(last),
            "prev" => cur.saturating_sub(1),
            "first" => 0,
            "last" => last,
            "half_down" => (cur + half).min(last),
            "half_up" => cur.saturating_sub(half),
            "page_down" => (cur + page).min(last),
            "page_up" => cur.saturating_sub(page),
            other => return Err(format!("unknown view action {other:?}")),
        };
        self.cursor.line = line;
        self.cursor.col = 0;
        self.desired_col = 0;
        self.desired_eol = false;
        self.ensure_visible();
        Ok(())
    }

    /// A view buffer's text fallthrough: the residual non-map key. Only `:`/`/`/`?`
    /// do anything — they open the command line / search through normal handling
    /// (each only switches mode, so the content stays intact). Every other key is
    /// inert: a view is effectively `nomodifiable`. Mirrors
    /// [`Editor::handle_explorer_text`].
    pub(crate) fn handle_view_text(&mut self, key: Key) {
        if matches!(key.as_char(), Some(':') | Some('/') | Some('?')) && !key.ctrl {
            self.message.clear();
            self.handle_normal(key);
        }
    }
}
