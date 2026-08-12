//! **Plugin-owned views** (`btv.view`) — a read-only, plugin-controlled content
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
//! creation (`btv._install_view_keymaps`), not a special `input()` branch. Decoration
//! (icons / indent guides / signs) rides the ordinary extmark layer
//! (`nvim_buf_set_extmark`), so it needs nothing here. See
//! docs/plans/2026-06-16-unify-special-buffer-kinds.md.

use super::*;
use crate::buffer::{Buffer, BufferKind};
use ropey::Rope;

/// One live `btv.view` surface, tracked in [`Editor::views`] by its Lua handle id.
pub(crate) struct ViewState {
    /// The backing read-only buffer (carries `view: Some(id)`).
    pub buf: BufferId,
    /// How the view is currently shown, or `None` while unmounted.
    pub mount: Option<ViewMount>,
    /// The `(namespace, id)` the plugin chose for cross-session persistence
    /// (`btv.view.create{ persist = }`), or `None` for an ephemeral view. Core round-trips
    /// this opaque pair through the workspace session: on capture it tags the view's slot;
    /// on restore it reserves the slot and hands the id back to the owning plugin, which
    /// keyed its own `btv.shada.plugin()` store by `id` and rebuilds the content. Core never
    /// stores the view's lines — only this pair — so persistence stays content-agnostic.
    /// Independent of `'workspacepersistunnamed'` (that governs editable scratch).
    pub persist: Option<(String, String)>,
}

/// Where a [`ViewState`] is mounted — the window region `focus` / `unmount` act on.
pub(crate) enum ViewMount {
    /// In the permanent dock on this side (`v:mount{ dock = … }`).
    Dock(DockSide),
    /// In a split window of the main editor area (`v:mount{ split = … }`).
    Split(WindowId),
    /// As the sole window of its own tab page (`v:mount{ tab = true }`) — opened with
    /// [`new_tab`](Editor::new_tab) so the view *fills* a fresh tab instead of splitting
    /// one (no leftover empty window). `unmount` closes the whole tab, so a plugin laying
    /// several views into one tab (the first via `tab`, the rest via `split`) tears the
    /// lot down by closing the tab-mounted one.
    Tab { tab: TabId, win: WindowId },
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
    /// `btv.view.create{ name, filetype }` — register view `id` and mint its backing
    /// read-only buffer (marked `view: Some(id)`, with `filetype` applied for
    /// treesitter / decoration). Idempotent: a second create on a live id is a
    /// no-op, so a re-run config doesn't strand a second buffer. The buffer is
    /// created off-screen (no window) — it becomes visible only on
    /// [`mount_view`](Editor::mount_view). `persist` is the `(namespace, id)` the view opts
    /// into cross-session restore with (`None` ⇒ ephemeral), stored on the [`ViewState`].
    pub fn create_view(
        &mut self,
        id: u64,
        name: String,
        filetype: String,
        persist: Option<(String, String)>,
    ) {
        if self.views.contains_key(&id) {
            return;
        }
        let mut buf = Buffer::empty();
        buf.kind = BufferKind::View(id);
        // The view's display name (statusline / tab label); a view has no file path, so
        // without this it reads `[No Name]`. Empty → `None` (left nameless).
        buf.view_name = (!name.is_empty()).then_some(name);
        let buf_id = self.add_buffer(buf);
        // A view's filetype drives treesitter / decoration *and* names the widget for
        // its `FileType` autocmd; default it to `btvview` (the widget identity) when the
        // plugin gives none, so `:set ft?` and user `FileType` autocmds see something.
        // (The view's `<CR>` → `on_select` map is installed server-side at create, not
        // off the filetype — see the prelude's `btv._install_view_keymaps`.)
        let filetype = if filetype.is_empty() {
            "btvview"
        } else {
            &filetype
        };
        self.set_filetype(buf_id, filetype);
        self.views.insert(
            id,
            ViewState {
                buf: buf_id,
                mount: None,
                persist,
            },
        );
    }

    /// The `(namespace, id)` view `vid` opted into cross-session persistence with, or
    /// `None` for an ephemeral view / unknown id. Used by the session capture to tag a
    /// persisted view's slot ([`Editor::capture_layout`]).
    pub(crate) fn view_persist_of(&self, vid: u64) -> Option<(String, String)> {
        self.views.get(&vid).and_then(|v| v.persist.clone())
    }

    /// The backing buffer of view `id` (the `btv._view_buf` mirror / extmark target),
    /// or `None` if no such view is live.
    pub fn view_buffer(&self, id: u64) -> Option<BufferId> {
        self.views.get(&id).map(|v| v.buf)
    }

    /// The window currently showing view `id` (the `btv._view_win` mirror, the `ctx.wo`
    /// target), or `None` if unmounted. Resolved from the mount: a split/float stores its
    /// window directly; a dock view is its dock layer's focused window. The id is validated
    /// against the live window set so a stale split window reports `None`.
    pub fn view_window(&self, id: u64) -> Option<WindowId> {
        let win = match self.views.get(&id)?.mount.as_ref()? {
            ViewMount::Float { win, .. } | ViewMount::Split(win) | ViewMount::Tab { win, .. } => {
                *win
            }
            ViewMount::Dock(side) => self.layer_tree(Layer::Dock(*side))?.current,
        };
        self.window(win).map(|_| win)
    }

    /// The `btv.view` Rust→Lua mirror snapshot: `(view id, backing buffer number,
    /// 1-based cursor line, window id)` per live view, for `btv._view_buf` /
    /// `btv._view_line` / `btv._view_win`. The line is the current cursor's when the view is
    /// the focused buffer (so `v:line()` reads it during an action), else `1` — a parked
    /// dock view's window cursor isn't on the live `self.cursor`, and reads only happen
    /// while focused. The window is `0` when the view is unmounted.
    pub fn view_mirror(&self) -> Vec<(u64, u64, u64, u64)> {
        self.views
            .iter()
            .map(|(&id, v)| {
                let line1 = if v.buf == self.cur_buffer() {
                    self.cursor.line as u64 + 1
                } else {
                    1
                };
                let win = self.view_window(id).map(|w| w.0).unwrap_or(0);
                (id, v.buf.0, line1, win)
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

    /// `v:mount{ tab = true }` — show view `id` as the sole window of a **new tab**,
    /// and focus it. Built on [`new_tab`](Editor::new_tab), so the view fills the fresh
    /// tab directly — no split, no leftover empty window (the friction a `tabnew` +
    /// split + `:only` dance would have). A plugin builds a multi-pane tab by mounting
    /// the first view with `tab` and the rest with `split`; [`unmount_view`] on the
    /// tab-mounted one closes the whole tab. A no-op for an unknown id.
    pub fn mount_view_tab(&mut self, id: u64) {
        let Some(buf) = self.view_buffer(id) else {
            return;
        };
        self.unmount_view(id);
        let options = self.windows.cur().options.clone();
        self.new_tab(buf, options);
        let tab = self.current_tab_id();
        let win = self.current_window_id();
        if let Some(v) = self.views.get_mut(&id) {
            v.mount = Some(ViewMount::Tab { tab, win });
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
        // Create the float UNFOCUSED first. For a grab modal we then push it onto the lock
        // STACK *before* focusing it: the focus guard pins focus to the topmost modal, so a
        // new modal must already be the top for the guard to permit focusing it — otherwise
        // an outer modal already on the stack would refuse the move (and the float would open
        // behind it). Non-grab floats just focus normally (no lock to satisfy).
        let win = self.open_float_window(buf, config, false);
        if grab {
            self.view_float_lock.push(win);
        }
        self.set_current_window(win);
        self.ensure_visible();
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
            Some(ViewMount::Tab { tab, win }) => {
                // Switch to the view's tab (it may not be active), then focus its window.
                let (tab, win) = (*tab, *win);
                self.ensure_main_layer();
                if self.tab_is_valid(tab) {
                    self.set_current_tabpage(tab);
                }
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
        let Some(buf) = self.view_buffer(id) else {
            return;
        };
        if self.views.get(&id).and_then(|v| v.mount.as_ref()).is_none() {
            return;
        }
        self.focus_view(id);
        // `focus_view` focuses the view's *layer*, but a dock's focused window can have
        // drifted to a different buffer (e.g. the dock was reused across sessions while
        // the view kept its mount), which would point `self.cursor` at the wrong window.
        // Re-assert the view's buffer in the focused window so the position always lands
        // on the view itself — never silently on whatever else the dock was showing.
        if self.cur_buffer() != buf {
            let win = self.current_window_id();
            self.set_window_buffer(win, buf);
        }
        // The focused window now addresses the view buffer, so `self.cursor` / `last_line`
        // do too.
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
            Some(ViewMount::Tab { tab, .. }) => self.close_tab_by_id(tab),
            Some(ViewMount::Split(win)) if self.windows.try_get(win).is_some() => {
                self.close_window_by_id(win, true);
            }
            Some(ViewMount::Float { win, prev, grab }) => {
                // Pop this modal off the focus-lock stack *before* closing / refocusing,
                // exactly as `close_panel` clears its lock first — otherwise the guard would
                // refuse the restore. `retain` (not `pop`) tolerates an out-of-order close.
                if grab {
                    self.view_float_lock.retain(|w| *w != win);
                }
                if self.windows.try_get(win).is_some() {
                    self.close_window_by_id(win, true);
                }
                // A grabbing float is modal: return to where it sprang from — which is the
                // modal *below* it in the stack (each modal's `prev` is whatever was focused
                // when it opened), so focus pops down the stack. A non-grab float leaves
                // focus wherever `remove_window` landed it, matching its non-locking nature.
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
    /// default `<CR>` map installed at view creation by `btv._install_view_keymaps`, or
    /// a plugin override) while a `btv.view` buffer is focused. `confirm` records a
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

    /// Adopt the reserved restore slot `win` for view `view_id` — the `place(view)` step of
    /// an [`btv.view.on_restore`](crate) handler, driven by `btv.view._adopt`. Retargets the
    /// placeholder window (minted by [`Editor::build_layout`] for a `view_persist` leaf) to
    /// the view's backing buffer, records the mount so `:focus`/`:unmount` resolve it, drops
    /// the now-orphaned placeholder buffer, and clears the pending claim. Handles a slot in
    /// any **open** layer — the main active tab or a dock (the dogfood cases) — and,
    /// best-effort, a slot parked in an inactive tab (the content lands; cross-tab focus is
    /// imperfect). A no-op if the view or the reserved window is gone — the claim is dropped
    /// either way, so it never lingers as an uncollapsible orphan.
    pub fn adopt_view(&mut self, view_id: u64, win: WindowId) {
        let Some(buf) = self.view_buffer(view_id) else {
            self.pending_view_restores.retain(|p| p.win != win);
            return;
        };
        // Resolve the slot against an open layer first (so `set_window_buffer` relayouts and
        // handles the focused window), extracting the layer + placeholder buffer up front so
        // no borrow of `self` outlives the mutating calls below.
        let open_slot = self
            .tree_of_window(win)
            .map(|(layer, t)| (layer, t.get(win).buffer));
        if let Some((layer, placeholder)) = open_slot {
            self.set_window_buffer(win, buf);
            // A dock-layer slot is a Dock mount; a main-layer slot (a split, or the sole
            // window of a tab) is a Split mount — closing a sole-window tab closes the tab
            // anyway, so Split's semantics suffice.
            let mount = match layer {
                Layer::Dock(side) => ViewMount::Dock(side),
                Layer::Main => ViewMount::Split(win),
            };
            if let Some(v) = self.views.get_mut(&view_id) {
                v.mount = Some(mount);
            }
            if placeholder != buf {
                self.delete_buffer(placeholder, true);
            }
        } else {
            // The slot is parked in an inactive tab: swap the buffer in place so the content
            // lands where it was rather than stranding a placeholder. Cross-tab focus via the
            // Split mount is imperfect (it assumes the active tab) — an accepted edge.
            let mut placeholder = None;
            for t in self.parked_trees_mut() {
                if let Some(w) = t.try_get_mut(win) {
                    placeholder = Some(w.buffer);
                    w.buffer = buf;
                    break;
                }
            }
            if let Some(placeholder) = placeholder {
                self.set_buffer_layer(buf, Layer::Main);
                if let Some(v) = self.views.get_mut(&view_id) {
                    v.mount = Some(ViewMount::Split(win));
                }
                if placeholder != buf {
                    self.delete_buffer(placeholder, true);
                }
            }
        }
        self.pending_view_restores.retain(|p| p.win != win);
    }

    /// Collapse the reserved slots of every persisted view no plugin adopted (its owner is
    /// gone, or it registered no `btv.view.on_restore`): close each unclaimed window — a dock
    /// shuts, a split collapses — the same fate as a restored file window whose file
    /// vanished. Called once after the restore dispatch drains. Clears the pending list.
    pub fn collapse_unclaimed_view_restores(&mut self) {
        for p in std::mem::take(&mut self.pending_view_restores) {
            match self.tree_of_window(p.win) {
                Some((Layer::Dock(side), _)) => self.close_dock(side),
                Some((Layer::Main, _)) => {
                    self.close_window_by_id(p.win, true);
                }
                // Parked in an inactive tab: closing it needs a tab switch, and an unclaimed
                // view there is a rare edge (a multi-tab persisted view whose plugin is
                // gone). Left as the placeholder rather than churning the focused tab.
                None => {}
            }
        }
    }
}
