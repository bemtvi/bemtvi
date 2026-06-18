//! **Plugin-owned views** (`nx.view`) — a read-only, plugin-controlled content
//! surface that can be mounted in a dock or a split: an ordinary [`Buffer`] (whose
//! [`kind`](Buffer::kind) is [`BufferKind::View`](crate::BufferKind::View)) shown in
//! any window, whose lines a plugin replaces wholesale and whose `<CR>` dispatches to
//! a Lua `on_select` callback.
//!
//! Like the directory listing ([`explorer`](super::explorer)) and the quickfix
//! window, a view is an **ordinary `nomodifiable` buffer in a window** (vim's model):
//! normal-mode keys — motions, search, `:` — flow through the grammar unchanged, and
//! text-mutating keys are refused at the [`modifiable`](Editor::modifiable)
//! chokepoints (the buffer's kind is [`BufferKind::View`](crate::BufferKind::View), so
//! [`Buffer::read_only`] is true), so the plugin-owned content can't be corrupted. Its
//! one special key —
//! `<CR>` → [`apply_view_action`](Editor::apply_view_action)`("confirm")` → the Lua
//! `on_select` — is an ordinary **buffer-local default keymap** installed at view
//! creation (`nx._install_view_keymaps`), not a special `input()` branch. Decoration
//! (icons / indent guides / signs) rides the ordinary extmark layer
//! (`nvim_buf_set_extmark`), so it needs nothing here. See
//! docs/plans/2026-06-16-unify-special-buffer-kinds.md.

use super::*;
use crate::buffer::{Buffer, BufferKind};
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
    /// In a floating window (`v:mount{ float = … }`). `win` is the float; `prev` is the
    /// window focused at mount, refocused on unmount when `grab` so a modal float feels
    /// transient (open → interact → dismiss → back where you were). `grab` records whether
    /// this float holds the hard focus lock ([`Editor::view_float_lock`]).
    Float {
        win: WindowId,
        prev: WindowId,
        grab: bool,
    },
}

impl Editor {
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
        buf.kind = BufferKind::View(id);
        let buf_id = self.add_buffer(buf);
        // A view's filetype drives treesitter / decoration *and* names the widget for
        // its `FileType` autocmd; default it to `nxview` (the widget identity) when the
        // plugin gives none, so `:set ft?` and user `FileType` autocmds see something.
        // (The view's `<CR>` → `on_select` map is installed server-side at create, not
        // off the filetype — see the prelude's `nx._install_view_keymaps`.)
        let filetype = if filetype.is_empty() {
            "nxview"
        } else {
            &filetype
        };
        self.set_filetype(buf_id, filetype);
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
        // The content is plugin-derived and has no disk backing, so the view is never
        // "modified" relative to a backing store — but `mark_resync` (the wholesale-
        // rewrite bookkeeping) sets the flag. Clear it, exactly as the terminal mirror
        // does after its own rewrite, so a view never shows `[+]` or blocks `:qa` with
        // E37 ("no write since last change"), as if it wanted saving.
        ob.buffer.modified = false;
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

    /// `v:mount{ float = { … } }` — show view `id` in a floating window placed by
    /// `config`, and focus it. When `grab`, the float hard-locks focus (the
    /// [`focus_window`](Editor::focus_window) guard pins focus to it, like the panel) until
    /// the view is unmounted, and unmount restores the window focused at mount; a non-grab
    /// float is an ordinary focusable float that `<C-w>` can leave. Remounting moves the
    /// view to the fresh float. A no-op for an unknown id.
    pub fn mount_view_float(&mut self, id: u64, config: FloatConfig, grab: bool) {
        let Some(buf) = self.view_buffer(id) else {
            return;
        };
        self.unmount_view(id);
        let prev = self.windows.current;
        // `open_float_window(enter = true)` focuses the float *before* we arm the lock, so
        // its own focus move is permitted (the panel opens the same way).
        let win = self.open_float_window(buf, config, true);
        if grab {
            self.view_float_lock = Some(win);
        }
        if let Some(v) = self.views.get_mut(&id) {
            v.mount = Some(ViewMount::Float { win, prev, grab });
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
            Some(ViewMount::Float { win, .. }) => {
                // A float overlays whatever is below it (no layer switch); focus it directly.
                let win = *win;
                if self.windows.try_get(win).is_some() {
                    self.set_current_window(win);
                }
            }
            None => {}
        }
    }

    /// `v:set_cursor(line)` — focus the window showing view `id` (like `:focus`) and
    /// move its cursor to 1-based `line`, clamped to the view's line count (column 0,
    /// like every view cursor). The reveal / find-file primitive — the one sanctioned
    /// cursor *write* for a view, whose cursor is otherwise plain normal-mode motion. A
    /// no-op for an unknown / unmounted id (nothing to focus or position).
    pub fn set_view_cursor(&mut self, id: u64, line1: usize) {
        if self.views.get(&id).and_then(|v| v.mount.as_ref()).is_none() {
            return;
        }
        self.focus_view(id);
        // `focus_view` made the view's window current, so `self.cursor` / `last_line`
        // now address the view buffer.
        self.cursor.line = line1.saturating_sub(1).min(self.last_line());
        self.cursor.col = 0;
        self.desired_col = 0;
        self.desired_eol = false;
        self.clamp_cursor();
        self.ensure_visible();
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
            Some(ViewMount::Float { win, prev, grab }) => {
                // Release the focus lock *before* closing / refocusing, exactly as
                // `close_panel` does — otherwise the guard would refuse the restore.
                if grab && self.view_float_lock == Some(win) {
                    self.view_float_lock = None;
                }
                if self.windows.try_get(win).is_some() {
                    self.close_window_by_id(win, true);
                }
                // A grabbing float is modal: return to where it sprang from. A non-grab
                // float leaves focus wherever `remove_window` landed it (the user may have
                // moved on), matching its non-locking nature.
                if grab && self.windows.try_get(prev).is_some() {
                    self.set_current_window(prev);
                }
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

    /// Apply a named `view` action, dispatched by a view buffer-local keymap (the
    /// default `<CR>` map installed at view creation by `nx._install_view_keymaps`, or
    /// a plugin override) while a `nx.view` buffer is focused. `confirm` records a
    /// `<CR>` select on the cursor line for the server to deliver to the view's Lua
    /// `on_select`. An unknown name fails loud per the no-silent-stub rule. Navigation
    /// (`j`/`k`/`gg`/`G`…) is ordinary normal-mode motion on the `nomodifiable` view
    /// now, so `confirm` is the only action here.
    pub fn apply_view_action(&mut self, action: &str) -> Result<(), String> {
        self.message.clear();
        match action {
            "confirm" => {
                if let Some(id) = self.buffer().view_id() {
                    self.view_selects.push((id, self.cursor.line));
                }
                Ok(())
            }
            other => Err(format!("unknown view action {other:?}")),
        }
    }
}
