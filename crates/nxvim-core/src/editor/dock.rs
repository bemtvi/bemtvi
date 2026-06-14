//! Permanent **docks** — global, cross-tab, editable window regions pinned to a
//! screen edge (nxvim's VSCode-style side/bottom panels). A dock holds an
//! ordinary [`WindowTree`], so it can be split internally; it is never disturbed
//! by splits / window-switches / tab-changes in the main editor area.
//!
//! ## The layer model
//!
//! The whole editing machine reads its target window from [`Editor::windows`].
//! Rather than teach `split`/`close`/`focus`/editing/redraw about docks, the
//! *focused* layer's tree is always swapped live onto `Editor::windows` (the same
//! trick tab pages use — see `tabs.rs`), and the non-focused layers' trees are
//! parked in [`Editor::docks`] / [`Editor::main_parked`]. Crossing layers
//! ([`Editor::switch_layer`]) is therefore a tree swap, after which every existing
//! window command "just works" within the now-live layer.

use super::*;
use crate::options::WindowOptions;

impl Editor {
    /// Whether the dock on `side` is open — either parked in [`Editor::docks`] or
    /// currently the focused layer (its tree swapped onto [`Editor::windows`], so
    /// its `docks` slot reads `None`). Always prefer this over the bare `Option`.
    pub(crate) fn dock_is_open(&self, side: DockSide) -> bool {
        self.focused_layer == Layer::Dock(side) || self.docks[side.idx()].is_some()
    }

    /// Every open layer in canonical order: `Main` first, then each open dock by
    /// [`DockSide::ALL`]. Used to iterate trees for relayout / rendering.
    pub(crate) fn open_layers(&self) -> Vec<Layer> {
        let mut out = vec![Layer::Main];
        for side in DockSide::ALL {
            if self.dock_is_open(side) {
                out.push(Layer::Dock(side));
            }
        }
        out
    }

    /// The tree backing `layer` — the live [`Editor::windows`] when `layer` is
    /// focused, the parked tree otherwise. `None` for a dock that isn't open.
    pub(crate) fn layer_tree(&self, layer: Layer) -> Option<&WindowTree> {
        if self.focused_layer == layer {
            return Some(&self.windows);
        }
        match layer {
            Layer::Main => self.main_parked.as_ref(),
            Layer::Dock(s) => self.docks[s.idx()].as_ref(),
        }
    }

    /// Mutable [`Editor::layer_tree`].
    pub(crate) fn layer_tree_mut(&mut self, layer: Layer) -> Option<&mut WindowTree> {
        if self.focused_layer == layer {
            return Some(&mut self.windows);
        }
        match layer {
            Layer::Main => self.main_parked.as_mut(),
            Layer::Dock(s) => self.docks[s.idx()].as_mut(),
        }
    }

    /// The layer (and its tree) that owns window `id`, scanning every open layer.
    /// `None` if no open window across the main tree and all docks has that id.
    pub(crate) fn tree_of_window(&self, id: WindowId) -> Option<(Layer, &WindowTree)> {
        self.open_layers()
            .into_iter()
            .filter_map(|l| self.layer_tree(l).map(|t| (l, t)))
            .find(|(_, t)| t.try_get(id).is_some())
    }

    /// Mutable [`Editor::tree_of_window`]: the tree (live or parked) owning `id`.
    pub(crate) fn tree_of_window_mut(&mut self, id: WindowId) -> Option<&mut WindowTree> {
        for layer in self.open_layers() {
            let owns = self
                .layer_tree(layer)
                .is_some_and(|t| t.try_get(id).is_some());
            if owns {
                return self.layer_tree_mut(layer);
            }
        }
        None
    }

    /// Make `target` the focused layer: park the live tree into its home slot and
    /// swap `target`'s parked tree onto [`Editor::windows`], then re-enter its
    /// focused window. The layer analogue of [`Editor::switch_tab`]; keeping the
    /// focused tree on `windows` means the editing machine is untouched. A no-op
    /// when `target` is already focused; panics only if `target` is a dock that
    /// isn't open (callers guard with [`Editor::dock_is_open`]).
    pub(crate) fn switch_layer(&mut self, target: Layer) {
        if target == self.focused_layer {
            return;
        }
        self.stash_focused_view();
        let incoming = match target {
            Layer::Main => self.main_parked.take(),
            Layer::Dock(s) => self.docks[s.idx()].take(),
        }
        .expect("switch_layer target must be an open, parked layer");
        let outgoing = std::mem::replace(&mut self.windows, incoming);
        match self.focused_layer {
            Layer::Main => self.main_parked = Some(outgoing),
            Layer::Dock(s) => self.docks[s.idx()] = Some(outgoing),
        }
        self.focused_layer = target;
        if let Layer::Dock(s) = target {
            self.last_dock = s;
        }
        self.relayout();
        let cur = self.windows.current;
        self.enter_window(cur);
        if !self.windows.floats.is_empty() {
            self.relayout();
        }
    }

    /// If a dock is the focused layer, cross back to the main tree first. The
    /// shared prelude of every tab operation: `switch_tab`/`new_tab` swap
    /// [`Editor::windows`], which must hold the *main* tree (never a dock's) so a
    /// dock never gets stashed into a tab slot.
    pub(crate) fn ensure_main_layer(&mut self) {
        if self.focused_layer != Layer::Main {
            self.switch_layer(Layer::Main);
        }
    }

    /// Open (or, if already open, just focus) the dock on `side`, sized to `size`
    /// (columns for left/right, rows for top/bottom) and showing `buf` — or a
    /// fresh scratch buffer when `buf` is `None`. The new dock takes focus. Backs
    /// `nx.dock.open` / `:DockOpen`.
    pub(crate) fn open_dock(&mut self, side: DockSide, size: usize, buf: Option<BufferId>) {
        self.dock_sizes[side.idx()] = size.max(1);
        if self.dock_is_open(side) {
            // Already open: honor the new size, ensure the requested buffer shows,
            // and focus it.
            self.focus_dock(side);
            if let Some(buf) = buf {
                let win = self.windows.current;
                self.set_window_buffer(win, buf);
            }
            self.relayout();
            return;
        }
        let buf = buf.unwrap_or_else(|| self.add_buffer(Buffer::empty()));
        let win = self.alloc_window_id();
        let tree = WindowTree::with_window(win, buf, WindowOptions::default());
        // Park the current layer, install the new dock tree as live, and enter it.
        self.stash_focused_view();
        let outgoing = std::mem::replace(&mut self.windows, tree);
        match self.focused_layer {
            Layer::Main => self.main_parked = Some(outgoing),
            Layer::Dock(s) => self.docks[s.idx()] = Some(outgoing),
        }
        self.focused_layer = Layer::Dock(side);
        self.last_dock = side;
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

    /// Close the dock on `side` (its buffers stay loaded, like closing a window).
    /// If it is the focused layer, focus crosses back to the main tree first so
    /// the live [`Editor::windows`] is always valid. A no-op if the dock isn't
    /// open. Backs `nx.dock.close` / `:DockClose`.
    pub(crate) fn close_dock(&mut self, side: DockSide) {
        if !self.dock_is_open(side) {
            return;
        }
        if self.focused_layer == Layer::Dock(side) {
            self.switch_layer(Layer::Main);
        }
        self.docks[side.idx()] = None;
        self.dock_sizes[side.idx()] = 0;
        self.relayout();
        self.ensure_visible();
    }

    /// Focus the dock on `side` (`nx.dock.focus` / `:DockFocus`, and the
    /// `<C-w><C-w>` directional cross). A no-op if the dock isn't open.
    pub(crate) fn focus_dock(&mut self, side: DockSide) {
        if self.dock_is_open(side) {
            self.switch_layer(Layer::Dock(side));
        }
    }

    /// Whether `side`'s dock is open (`nx.dock.is_open`). Public mirror of
    /// [`Editor::dock_is_open`] for the Lua read surface.
    pub fn dock_open(&self, side_idx: usize) -> bool {
        DockSide::ALL
            .get(side_idx)
            .is_some_and(|&s| self.dock_is_open(s))
    }

    // ----- string-keyed public surface (the server / RPC boundary) ----------
    // `DockSide` is crate-private; the server addresses docks by the side keyword
    // the `nx.dock.*` Lua API carries, validated loudly here.

    /// `nx.dock.open{ side, size?, buf? }` — open/focus a dock by side keyword.
    /// An unknown side is reported (no silent fallback).
    pub fn open_dock_named(&mut self, side: &str, size: Option<usize>, buf: Option<BufferId>) {
        match DockSide::from_keyword(side) {
            Some(s) => self.open_dock(s, size.unwrap_or_else(|| s.default_size()), buf),
            None => self.echo(format!("E474: Invalid dock side: {side}")),
        }
    }

    /// `nx.dock.close(side)` — close a dock by side keyword.
    pub fn close_dock_named(&mut self, side: &str) {
        match DockSide::from_keyword(side) {
            Some(s) => self.close_dock(s),
            None => self.echo(format!("E474: Invalid dock side: {side}")),
        }
    }

    /// `nx.dock.focus(side)` — focus a dock by side keyword.
    pub fn focus_dock_named(&mut self, side: &str) {
        match DockSide::from_keyword(side) {
            Some(s) => self.focus_dock(s),
            None => self.echo(format!("E474: Invalid dock side: {side}")),
        }
    }

    /// Whether the dock on `side` (keyword) is open — the string-keyed
    /// [`Editor::dock_is_open`] for the server's `nx._docks` mirror.
    pub fn dock_is_open_named(&self, side: &str) -> bool {
        DockSide::from_keyword(side).is_some_and(|s| self.dock_is_open(s))
    }
}
