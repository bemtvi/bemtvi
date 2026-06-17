//! Tab-page lifecycle, navigation, and the `:tab…`/`:tabnew`/`:tabclose` commands.

use super::*;
use crate::mode::Mode;
use crate::options::WindowOptions;

/// The count after a `+`/`-` in a relative tab argument (`:tabmove +2`,
/// `:tabclose -`): an empty string means `1` (a bare `+`/`-`), a positive integer
/// is itself, anything else is `None`. The sign is the caller's (it stripped the
/// prefix and picks the direction).
fn parse_rel_count(rest: &str) -> Option<usize> {
    if rest.is_empty() {
        Some(1)
    } else {
        rest.parse::<usize>().ok()
    }
}

impl Editor {
    /// The active tab's id (the `nvim_get_current_tabpage` target).
    pub fn current_tab_id(&self) -> TabId {
        self.main_tabs.tabs[self.main_tabs.current].id
    }

    /// Every tab id in tabline order (the `nvim_list_tabpages` order).
    pub fn tab_ids(&self) -> Vec<TabId> {
        self.main_tabs.tabs.iter().map(|t| t.id).collect()
    }

    /// Number of open tab pages (always ≥ 1).
    pub fn tab_count(&self) -> usize {
        self.main_tabs.tabs.len()
    }

    /// Whether `id` names an open tab (`nvim_tabpage_is_valid`).
    pub fn tab_is_valid(&self, id: TabId) -> bool {
        self.main_tabs.tabs.iter().any(|t| t.id == id)
    }

    /// A tab's 1-based position in the tabline (`nvim_tabpage_get_number`), or
    /// `None` if `id` names no open tab.
    pub fn tab_number(&self, id: TabId) -> Option<usize> {
        self.main_tabs
            .tabs
            .iter()
            .position(|t| t.id == id)
            .map(|i| i + 1)
    }

    /// The window layout backing tab `id`: the live tree for the active tab, the
    /// stashed tree otherwise. `None` if `id` names no open tab.
    fn tab_tree(&self, id: TabId) -> Option<&WindowTree> {
        let idx = self.main_tabs.tabs.iter().position(|t| t.id == id)?;
        if idx == self.main_tabs.current {
            // The active tab's main tree is live on `self.windows` — unless a dock
            // is focused, in which case it is parked in the main layer's active tab
            // slot. `layer_tree` resolves either.
            self.layer_tree(Layer::Main)
        } else {
            self.main_tabs.tabs[idx].tree.as_ref()
        }
    }

    /// Every window id in tab `id`, in the same order [`Editor::window_ids`] uses
    /// (`nvim_tabpage_list_wins`). `None` if `id` names no open tab.
    pub fn tab_window_ids(&self, id: TabId) -> Option<Vec<WindowId>> {
        let tree = self.tab_tree(id)?;
        let mut ids = tree.leaves();
        ids.extend(tree.floats.iter().copied());
        Some(ids)
    }

    /// The buffer shown in each window of tab `id`, parallel to
    /// [`Editor::tab_window_ids`] (same order). Lets the server mirror an
    /// **inactive** tab's window→buffer mapping — which the global window mirror
    /// (current tab only) can't supply — so `vim.fn.tabpagebuflist` resolves every
    /// tab, not just the focused one. `None` if `id` names no open tab.
    pub fn tab_window_buffers(&self, id: TabId) -> Option<Vec<crate::BufferId>> {
        let tree = self.tab_tree(id)?;
        let mut ids = tree.leaves();
        ids.extend(tree.floats.iter().copied());
        Some(ids.into_iter().map(|w| tree.get(w).buffer).collect())
    }

    /// The focused window of tab `id` (`nvim_tabpage_get_win`). `None` if `id`
    /// names no open tab.
    pub fn tab_current_window(&self, id: TabId) -> Option<WindowId> {
        Some(self.tab_tree(id)?.current)
    }

    /// The active tab's index in tabline order (the highlighted cell).
    pub(crate) fn current_tab_index(&self) -> usize {
        self.main_tabs.current
    }

    /// Every window id across **all** tab pages of **all** layers, in layer order
    /// (main first, then docks by [`DockSide::ALL`]) then tab order then in-tab
    /// layout order (`nvim_list_wins`, which spans every tabpage in neovim). Each
    /// dock's *inactive* tabs are listed too (their windows exist though unpainted),
    /// mirroring how main's inactive tabs are listed. A **hidden** dock contributes
    /// nothing — like [`Editor::window_ids`] it is excluded from the active window
    /// set, so a toggled-away dock leaves `nvim_list_wins` until shown again. Within
    /// a tab the order matches [`Editor::tab_window_ids`].
    pub fn all_window_ids(&self) -> Vec<WindowId> {
        let mut ids = Vec::new();
        for layer in std::iter::once(Layer::Main).chain(DockSide::ALL.map(Layer::Dock)) {
            if let Layer::Dock(s) = layer {
                if !self.dock_is_open(s) {
                    continue; // closed or hidden — not in the active window set.
                }
            }
            let Some(stack) = self.stack(layer) else {
                continue;
            };
            for idx in 0..stack.tabs.len() {
                let tree = self
                    .layer_tab_tree(layer, idx)
                    .expect("an in-range tab of an open layer always has a tree");
                ids.extend(tree.leaves());
                ids.extend(tree.floats.iter().copied());
            }
        }
        ids
    }

    /// One [`TabLabel`] per tab of the **main** layer (the neovim global tabline) —
    /// empty when its tabline is hidden. Shorthand for
    /// [`tab_labels_for`](Self::tab_labels_for)`(Layer::Main)`.
    pub(crate) fn tab_labels(&self) -> Vec<TabLabel> {
        self.tab_labels_for(Layer::Main)
    }

    /// One [`TabLabel`] per tab of `layer`, in tabline order — empty when that
    /// layer's tabline is hidden ([`Editor::tabline_visible_for`]) or the layer is
    /// closed. Each label names the tab's focused window's buffer, whether it is
    /// modified, and the tab's window count. The active tab's tree may be live on
    /// [`Editor::windows`] (when `layer` is focused) or parked in its slot.
    pub(crate) fn tab_labels_for(&self, layer: Layer) -> Vec<TabLabel> {
        if !self.tabline_visible_for(layer) {
            return Vec::new();
        }
        let Some(stack) = self.stack(layer) else {
            return Vec::new();
        };
        (0..stack.tabs.len())
            .map(|idx| {
                let tree = self
                    .layer_tab_tree(layer, idx)
                    .expect("an in-range tab of an open layer always has a tree");
                let buf_id = tree.get(tree.current).buffer;
                let buf = self.buffers.get(buf_id);
                let name = buf
                    .buffer
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "[No Name]".to_string());
                TabLabel {
                    name,
                    modified: buf.buffer.modified,
                    window_count: tree.count(),
                }
            })
            .collect()
    }

    /// Every region's tabline (labels + active index), for the per-region [`View`]
    /// projection: the main editor area plus each of the four docks. A closed dock
    /// (or any region whose tabline is hidden) yields empty labels, which the
    /// client renders as no tabline for that region.
    pub(crate) fn region_tablines(&self) -> crate::view::RegionTablines {
        let region = |layer: Layer| crate::view::RegionTabline {
            tabs: self
                .tab_labels_for(layer)
                .into_iter()
                .map(crate::view::tab_label_to_view)
                .collect(),
            current: self.stack(layer).map_or(0, |s| s.current),
            title: match layer {
                Layer::Dock(s) => self.dock_title(s).to_string(),
                Layer::Main => String::new(),
            },
        };
        crate::view::RegionTablines {
            main: region(Layer::Main),
            docks: [
                region(Layer::Dock(DockSide::Left)),
                region(Layer::Dock(DockSide::Right)),
                region(Layer::Dock(DockSide::Top)),
                region(Layer::Dock(DockSide::Bottom)),
            ],
        }
    }

    /// Switch region `layer` to its tab at `idx`, moving focus into that region
    /// first (a tabline click both selects the tab and focuses its window, like
    /// vim's global tabline click — `switch_tab` always acts on the focused
    /// layer's stack). A no-op when the layer is closed or `idx` is out of range
    /// (an already-active tab of the now-focused region falls through `switch_tab`'s
    /// own no-op). Backs the per-region tabline mouse click ([`Editor::mouse`]).
    pub(crate) fn focus_region_tab(&mut self, layer: Layer, idx: usize) {
        let Some(stack) = self.stack(layer) else {
            return;
        };
        if idx >= stack.tabs.len() {
            return;
        }
        match layer {
            Layer::Main => self.ensure_main_layer(),
            Layer::Dock(side) => self.focus_dock(side),
        }
        self.switch_tab(idx);
    }

    /// Switch the **main** region to its tab page `n` (1-based) and focus it — the
    /// `%nT` tabline click ([`crate::statusline::ClickAction::Tab`]). A no-op when
    /// `n` is `0` or past the last main tab. Reuses [`Editor::focus_region_tab`], so
    /// it crosses focus back to main first (vim's tabline click both selects and
    /// focuses).
    pub fn select_main_tab(&mut self, n: usize) {
        if let Some(idx) = n.checked_sub(1) {
            self.focus_region_tab(Layer::Main, idx);
        }
    }

    /// Make tab `id` the active tab (`nvim_set_current_tabpage`). A no-op if `id`
    /// is already active or names no open tab. The neovim tabpage API addresses the
    /// **main** layer's tabs, so focus crosses back to main first (a focused dock
    /// would otherwise have `switch_tab` act on its own stack).
    pub fn set_current_tabpage(&mut self, id: TabId) {
        if let Some(idx) = self.main_tabs.tabs.iter().position(|t| t.id == id) {
            self.ensure_main_layer();
            self.switch_tab(idx);
        }
    }

    /// Stash the live view position (`cursor`/`top`/`leftcol`) of the focused
    /// window back into its [`Window`], so a later [`Editor::enter_window`] (in
    /// this tab, or on return to it) restores it. The shared prelude of every
    /// focus / tab switch.
    pub(crate) fn stash_focused_view(&mut self) {
        // Stash the focused window's secondary multi-cursors first — finalizing
        // placement may snap the primary onto a placed cursor, which the view
        // stash below must capture.
        self.stash_secondary_cursors();
        let (cursor, top, leftcol) = (self.cursor, self.top, self.leftcol);
        let w = self.windows.cur_mut();
        w.saved_cursor = cursor;
        w.saved_top = top;
        w.saved_leftcol = leftcol;
    }

    /// Make the tab at `target` (an index into the **focused** layer's [`TabStack`])
    /// the active one: stash the live window layout into the outgoing tab's slot,
    /// swap the incoming tab's stashed layout onto [`Editor::windows`], and re-enter
    /// its focused window. The tab analogue of [`Editor::focus_window`] — keeping
    /// `self.windows` always the active layout means the whole editing machine is
    /// untouched. Acts on whatever layer holds focus (so `gt` in a dock cycles only
    /// that dock's tabs). A no-op for the current tab or an out-of-range index.
    fn switch_tab(&mut self, target: usize) {
        let layer = self.focused_layer;
        let (cur, len) = {
            let stack = self.focused_stack();
            (stack.current, stack.tabs.len())
        };
        if target == cur || target >= len {
            return;
        }
        // Stash the outgoing tab's live view into its focused window, then swap the
        // live tree into its slot and the target tab's parked tree onto `windows`.
        self.stash_focused_view();
        self.swap_live_tree((layer, cur), (layer, target));
        self.focused_stack_mut().current = target;
        // Lay the now-live tree out for the current area, then enter its focused
        // window (restores its buffer + view, clears transient state). A second
        // relayout settles any cursor-relative float now that the cursor is live.
        self.relayout();
        let cur = self.windows.current;
        self.enter_window(cur);
        if !self.windows.floats.is_empty() {
            self.relayout();
        }
    }

    /// `:tabnew` / `:tabedit` / `<C-w>T` core: open a new tab — a fresh single
    /// window bound to `buf` with `options` — directly after the current tab, and
    /// make it active. The outgoing tab's layout is stashed first. The caller
    /// follows up (e.g. `:enew` for an empty `:tabnew`, `:edit` for a file).
    pub(crate) fn new_tab(&mut self, buf: BufferId, options: WindowOptions) {
        // A new tab installs a fresh tree in the *focused* layer's stack, directly
        // after its active tab — so `:tabnew` in a dock adds a dock tab.
        self.stash_focused_view();
        let new_win = self.alloc_window_id();
        let id = self.alloc_tab_id();
        let tree = WindowTree::with_window(new_win, buf, options);
        let outgoing = std::mem::replace(&mut self.windows, tree);
        let stack = self.focused_stack_mut();
        let insert_at = stack.current + 1;
        stack.tabs[stack.current].tree = Some(outgoing);
        stack.tabs.insert(insert_at, TabSlot { id, tree: None });
        stack.current = insert_at;
        // The new window is live; sync the editor's live buffer + view to it and
        // lay the (now one-row-shorter, if the tabline just appeared) area out.
        self.set_cur_buffer(buf);
        self.cursor = Cursor::default();
        self.top = 0;
        self.leftcol = 0;
        self.mode = Mode::Normal;
        self.reset_pending();
        self.scroll_from = None;
        self.pending_scroll = None;
        self.message.clear();
        self.relayout();
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// `:tab split` — open the *current* buffer in a new tab, preserving the
    /// focused window's cursor/scroll. Unlike [`Editor::new_tab`] (which resets
    /// the view, as `:tabnew` does), this clones the live view into its own tab,
    /// matching vim's "a split made into a tab page".
    pub(crate) fn tab_split(&mut self) {
        let buf = self.current_buffer_id();
        let options = self.windows.cur().options.clone();
        let (cursor, top, leftcol) = (self.cursor, self.top, self.leftcol);
        self.new_tab(buf, options);
        self.cursor = cursor;
        self.top = top;
        self.leftcol = leftcol;
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// Focus window `win` in the tab at index `tab_idx`, switching tabs first if
    /// it is not already active. Backs `:drop` / `:tab drop` when the requested
    /// file is already shown somewhere.
    pub(crate) fn goto_tab_window(&mut self, tab_idx: usize, win: WindowId) {
        // `:drop` targets a *main* tab (its search scans the main layer), so cross
        // back to main before switching tabs there.
        self.ensure_main_layer();
        if tab_idx != self.main_tabs.current {
            self.switch_tab(tab_idx);
        }
        self.set_current_window(win);
    }

    /// `gt` / `:tabnext` — go to the next tab (wrapping), or, with a count, to that
    /// absolute 1-based tab number (`{count}gt`, `:tabnext {count}`).
    pub(crate) fn goto_tab_next(&mut self, count: Option<usize>) {
        let stack = self.focused_stack();
        let n = stack.tabs.len();
        let target = match count {
            Some(c) => c.saturating_sub(1).min(n - 1),
            None => (stack.current + 1) % n,
        };
        self.switch_tab(target);
    }

    /// `gT` / `:tabprevious` — go `count` tabs back (default 1), wrapping. Acts on
    /// the focused layer (a dock cycles its own tabs).
    pub(crate) fn goto_tab_prev(&mut self, count: Option<usize>) {
        let stack = self.focused_stack();
        let n = stack.tabs.len();
        let back = count.unwrap_or(1) % n;
        let target = (stack.current + n - back) % n;
        self.switch_tab(target);
    }

    /// `:tabclose[!]` / the last-window `:q` close: close the **current** tab page
    /// of the focused layer (its whole layout; the buffers stay loaded in the
    /// store). On the layer's last tab, main refuses (vim's `E784`) and a dock
    /// closes entirely. The argument form lives in [`Editor::close_tab_cmd`].
    pub(crate) fn close_tab(&mut self) {
        if self.close_last_tab_guard() {
            return;
        }
        self.close_tab_at(self.focused_stack().current);
    }

    /// `:tabclose[!] [N]` — close tab page `arg` (`N` 1-based, `+N`/`-N` relative,
    /// `$` last, empty = current) of the focused layer, handling its last tab as
    /// [`Editor::close_tab`] does (`E784` for main, close the dock for a dock) and
    /// reporting a malformed or out-of-range target (`E474`).
    pub(crate) fn close_tab_cmd(&mut self, arg: &str) {
        if self.close_last_tab_guard() {
            return;
        }
        match self.resolve_tab_arg(arg) {
            Some(target) => self.close_tab_at(target),
            None => self.echo(format!("E474: Invalid argument: {arg}")),
        }
    }

    /// When the focused layer is down to its **last** tab, handle a close request
    /// that would remove it: main errors (`E784`), a dock closes entirely
    /// ([`Editor::close_dock`]). Returns `true` when it handled the request (the
    /// caller then stops); `false` when more than one tab remains and a normal
    /// per-tab close should proceed.
    fn close_last_tab_guard(&mut self) -> bool {
        if self.focused_stack().tabs.len() > 1 {
            return false;
        }
        match self.focused_layer {
            Layer::Main => self.echo("E784: Cannot close last tab page"),
            Layer::Dock(s) => self.close_dock(s),
        }
        true
    }

    /// Close the tab page at index `target` of the focused layer (assumed valid,
    /// with `tabs.len() > 1`). Closing the **active** tab promotes a neighbor's
    /// stashed tree to live (the tab to the right, or the last tab); closing an
    /// **inactive** tab just drops its stashed slot, leaving the live layout
    /// untouched. Buffers stay loaded.
    fn close_tab_at(&mut self, target: usize) {
        let cur = self.focused_stack().current;
        if target == cur {
            // The closing tab's tree is live on `self.windows`; its slot is empty.
            // Drop the slot, then replace the live tree with a surviving tab's stash.
            let stack = self.focused_stack_mut();
            stack.tabs.remove(target);
            let next = target.min(stack.tabs.len() - 1);
            let incoming = stack.tabs[next]
                .tree
                .take()
                .expect("a surviving tab is inactive, so holds its stashed layout");
            stack.current = next;
            self.windows = incoming;
            self.relayout();
            let cur = self.windows.current;
            self.enter_window(cur);
            if !self.windows.floats.is_empty() {
                self.relayout();
            }
        } else {
            // An inactive tab: its stashed tree is dropped with the slot; the live
            // layout is unaffected. The active index shifts left if the closed tab
            // was before it. Re-lay since the tabline row may vanish (down to one).
            let stack = self.focused_stack_mut();
            stack.tabs.remove(target);
            if target < stack.current {
                stack.current -= 1;
            }
            self.relayout();
            self.ensure_visible();
        }
    }

    /// `:tabmove [N]` — move the **current** tab page within the tabline. No arg or
    /// `$` makes it last; `0` makes it first; a bare `N` (1-based, counted *before*
    /// the move, per vim) moves it to just after tab `N`; `+N` / `-N` shift it `N`
    /// places right / left (clamped, not wrapped). The active layout stays live —
    /// only its [`TabSlot`]'s position in `tabs` changes, so there is no
    /// stash/restore and the windows area is untouched.
    pub(crate) fn move_tab(&mut self, arg: &str) {
        let arg = arg.trim();
        let n = self.focused_stack().tabs.len();
        if n <= 1 {
            return;
        }
        let c = self.focused_stack().current;
        // Destination 0-based index, computed against the pre-move array.
        let dest = if arg.is_empty() || arg == "$" {
            n - 1
        } else if let Some(rest) = arg.strip_prefix('+') {
            match parse_rel_count(rest) {
                Some(k) => (c + k).min(n - 1),
                None => return self.echo(format!("E474: Invalid argument: {arg}")),
            }
        } else if let Some(rest) = arg.strip_prefix('-') {
            match parse_rel_count(rest) {
                Some(k) => c.saturating_sub(k),
                None => return self.echo(format!("E474: Invalid argument: {arg}")),
            }
        } else {
            match arg.parse::<usize>() {
                // `0` → first; `N` → after tab N. With N counted before the move,
                // a pivot at or past the current tab lands the tab *at* the pivot's
                // old index (everything after it shifts left on removal), else just
                // after it.
                Ok(0) => 0,
                Ok(num) if num <= n => {
                    let pivot = num - 1;
                    if c <= pivot {
                        pivot
                    } else {
                        pivot + 1
                    }
                }
                _ => return self.echo(format!("E474: Invalid argument: {arg}")),
            }
        };
        if dest == c {
            return;
        }
        let stack = self.focused_stack_mut();
        let slot = stack.tabs.remove(c);
        stack.tabs.insert(dest, slot);
        stack.current = dest;
    }

    /// Resolve a tab-selecting ex argument to a 0-based index into `tabs`: empty →
    /// current, `$` → last, `+N`/`-N` → relative to the current tab (clamped), a
    /// bare `N` → the 1-based tab number. `None` for a malformed or out-of-range
    /// argument. (Shared by the argument forms of `:tabclose`; `:tabmove` has its
    /// own "after tab N" placement rule and parses inline.)
    fn resolve_tab_arg(&self, arg: &str) -> Option<usize> {
        let arg = arg.trim();
        let stack = self.focused_stack();
        let n = stack.tabs.len();
        if arg.is_empty() {
            return Some(stack.current);
        }
        if arg == "$" {
            return Some(n - 1);
        }
        if let Some(rest) = arg.strip_prefix('+') {
            return Some((stack.current + parse_rel_count(rest)?).min(n - 1));
        }
        if let Some(rest) = arg.strip_prefix('-') {
            return Some(stack.current.saturating_sub(parse_rel_count(rest)?));
        }
        let num: usize = arg.parse().ok()?;
        (1..=n).contains(&num).then_some(num - 1)
    }

    /// `:tabonly` — close every tab page but the current one (their buffers stay
    /// loaded). A no-op when only one tab is open. The kept tab's live layout is
    /// untouched; only the tabline-row reservation may change, so we re-lay.
    pub(crate) fn tab_only(&mut self) {
        let stack = self.focused_stack_mut();
        if stack.tabs.len() <= 1 {
            return;
        }
        let kept = stack.tabs.remove(stack.current);
        stack.tabs.clear();
        stack.tabs.push(kept);
        stack.current = 0;
        self.relayout();
        self.ensure_visible();
    }

    /// `<C-w>T` — move the focused window to a new tab page. The window leaves its
    /// current tab (a neighbor expands to fill the freed area) and becomes the only
    /// window of a fresh tab, carrying its buffer and view position. Fails (no-op)
    /// when it is the only window in the tab, as vim does.
    pub(crate) fn window_to_new_tab(&mut self) {
        if self.windows.leaves().len() <= 1 {
            return;
        }
        // Capture the moving window's buffer, options, and live view before it is
        // removed from this tab.
        let cur = self.windows.current;
        let buf = self.windows.get(cur).buffer;
        let options = self.windows.get(cur).options.clone();
        let (cursor, top, leftcol) = (self.cursor, self.top, self.leftcol);
        // Remove it here; a survivor takes focus and the live view becomes theirs.
        self.remove_window(cur);
        // Open the new tab around the captured buffer, then restore the moved
        // window's own view into its (now live) single window.
        self.new_tab(buf, options);
        self.cursor = cursor;
        self.top = top;
        self.leftcol = leftcol;
        self.clamp_cursor();
        self.ensure_visible();
    }

    /// Mint a fresh, never-reused tab id.
    pub(crate) fn alloc_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        id
    }
}
