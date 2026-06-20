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
//! trick tab pages use — see `tabs.rs`), and every non-live tree parks in its own
//! tab slot: each layer (main + each open dock) carries its own [`TabStack`], and
//! a non-focused layer's active tab parks its tree there. Crossing layers
//! ([`Editor::switch_layer`]) is therefore a tree swap, after which every existing
//! window command "just works" within the now-live layer.

use super::*;
use crate::editor::windows::Window;
use crate::options::WindowOptions;

impl Editor {
    /// Whether the dock on `side` is **open and visible**: it has a [`TabStack`] in
    /// [`Editor::dock_tabs`] *and* is not [hidden](Editor::dock_hidden). This is the
    /// visibility predicate every layout / render / mouse / focus-cross site reads —
    /// a hidden dock reads as not-open here so it's excluded from all of them, while
    /// its parked content stays resolvable through the `dock_tabs`-reading helpers
    /// ([`Editor::stack`], [`Editor::layer_tree`], [`Editor::parked_trees_mut`]).
    /// Always prefer this over inspecting the `Option`. For "does any state exist
    /// (visible *or* hidden)" use [`Editor::dock_exists`].
    pub(crate) fn dock_is_open(&self, side: DockSide) -> bool {
        self.dock_exists(side) && !self.dock_hidden[side.idx()]
    }

    /// Whether the dock on `side` has state at all — a [`TabStack`] in
    /// [`Editor::dock_tabs`], whether visible or hidden. The lifecycle guards
    /// (`open`/`close`/`focus`/`hide`/`show`) test this so they still act on a
    /// *hidden* dock; visibility decisions use [`Editor::dock_is_open`] instead.
    pub(crate) fn dock_exists(&self, side: DockSide) -> bool {
        self.dock_tabs[side.idx()].is_some()
    }

    /// The tab stack backing `layer`: [`Editor::main_tabs`] for `Main`, the dock's
    /// stack for `Dock(side)` (`None` when that dock is closed).
    pub(crate) fn stack(&self, layer: Layer) -> Option<&TabStack> {
        match layer {
            Layer::Main => Some(&self.main_tabs),
            Layer::Dock(s) => self.dock_tabs[s.idx()].as_ref(),
        }
    }

    /// Mutable [`Editor::stack`].
    pub(crate) fn stack_mut(&mut self, layer: Layer) -> Option<&mut TabStack> {
        match layer {
            Layer::Main => Some(&mut self.main_tabs),
            Layer::Dock(s) => self.dock_tabs[s.idx()].as_mut(),
        }
    }

    /// The **focused** layer's tab stack — the one whose active tab is live on
    /// [`Editor::windows`]. Always present (an open layer always has a stack), so
    /// the interactive tab commands (`:tabnew`/`gt`/`:tabclose`/…) read it directly
    /// to act on whatever region currently holds focus.
    pub(crate) fn focused_stack(&self) -> &TabStack {
        self.stack(self.focused_layer)
            .expect("the focused layer always has a tab stack")
    }

    /// Mutable [`Editor::focused_stack`].
    pub(crate) fn focused_stack_mut(&mut self) -> &mut TabStack {
        let layer = self.focused_layer;
        self.stack_mut(layer)
            .expect("the focused layer always has a tab stack")
    }

    /// Park the tree live on [`Editor::windows`] into slot `(from_layer, from_tab)`
    /// and swap the tree parked at `(to_layer, to_tab)` onto `windows`. The single
    /// tree move shared by [`Editor::switch_layer`] (different layer) and
    /// `switch_tab` (same layer, different tab): `from` must be the currently-live
    /// slot (its stored tree `None`) and `to` a parked slot (tree `Some`). Updates
    /// neither `focused_layer` nor any `current` index nor the layout — the caller
    /// sequences those around it.
    pub(crate) fn swap_live_tree(&mut self, from: (Layer, usize), to: (Layer, usize)) {
        let incoming = self
            .slot_tree_mut(to.0, to.1)
            .take()
            .expect("swap_live_tree target slot must hold a parked tree (Some) before the swap");
        let outgoing = std::mem::replace(&mut self.windows, incoming);
        *self.slot_tree_mut(from.0, from.1) = Some(outgoing);
    }

    /// The `tree` `Option` of tab `tab` in `layer` — the parking slot
    /// [`Editor::swap_live_tree`] moves trees in and out of.
    fn slot_tree_mut(&mut self, layer: Layer, tab: usize) -> &mut Option<WindowTree> {
        &mut self
            .stack_mut(layer)
            .expect("slot_tree_mut on an open layer")
            .tabs[tab]
            .tree
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

    /// The tree backing `layer`'s **active** tab — the live [`Editor::windows`]
    /// when `layer` is focused, the active tab's parked tree otherwise. `None` for
    /// a dock that isn't open.
    pub(crate) fn layer_tree(&self, layer: Layer) -> Option<&WindowTree> {
        if self.focused_layer == layer {
            return Some(&self.windows);
        }
        let stack = self.stack(layer)?;
        stack.tabs[stack.current].tree.as_ref()
    }

    /// Mutable [`Editor::layer_tree`].
    pub(crate) fn layer_tree_mut(&mut self, layer: Layer) -> Option<&mut WindowTree> {
        if self.focused_layer == layer {
            return Some(&mut self.windows);
        }
        let stack = self.stack_mut(layer)?;
        let current = stack.current;
        stack.tabs[current].tree.as_mut()
    }

    /// The tree of tab `idx` in `layer`: the live [`Editor::windows`] for the
    /// focused layer's active tab, the parked slot tree otherwise. `None` for a
    /// closed dock or an out-of-range index. Resolves any `(layer, tab)` to its
    /// tree across both swap dimensions.
    pub(crate) fn layer_tab_tree(&self, layer: Layer, idx: usize) -> Option<&WindowTree> {
        let stack = self.stack(layer)?;
        if idx == stack.current && self.focused_layer == layer {
            Some(&self.windows)
        } else {
            stack.tabs.get(idx).and_then(|s| s.tree.as_ref())
        }
    }

    /// Mutable [`Editor::layer_tab_tree`]: the tree of tab `idx` in `layer` (the
    /// live [`Editor::windows`] for the focused layer's active tab, the parked slot
    /// otherwise). Used by the buffer-delete sweep to rebind windows across every
    /// layer and tab off a freed buffer.
    pub(crate) fn layer_tab_tree_mut(
        &mut self,
        layer: Layer,
        idx: usize,
    ) -> Option<&mut WindowTree> {
        let live = {
            let stack = self.stack(layer)?;
            idx == stack.current && self.focused_layer == layer
        };
        if live {
            Some(&mut self.windows)
        } else {
            let stack = self.stack_mut(layer)?;
            stack.tabs.get_mut(idx).and_then(|s| s.tree.as_mut())
        }
    }

    /// Every **parked** window tree — every tab of every layer whose tree isn't
    /// the one live on [`Editor::windows`]: all inactive tabs of every layer, plus
    /// the active tab of each *non-focused* layer. The live tree is excluded (its
    /// slot is `None`), so callers that also touch `self.windows` see no double
    /// visit. Used by edits that must ride every background tree (jumplists, …).
    pub(crate) fn parked_trees_mut(&mut self) -> impl Iterator<Item = &mut WindowTree> {
        std::iter::once(&mut self.main_tabs)
            .chain(self.dock_tabs.iter_mut().flatten())
            .flat_map(|stack| stack.tabs.iter_mut())
            .filter_map(|slot| slot.tree.as_mut())
    }

    /// The layer (and its tree) that owns window `id`, scanning every open layer.
    /// `None` if no open window across the main tree and all docks has that id.
    pub(crate) fn tree_of_window(&self, id: WindowId) -> Option<(Layer, &WindowTree)> {
        self.open_layers()
            .into_iter()
            .filter_map(|l| self.layer_tree(l).map(|t| (l, t)))
            .find(|(_, t)| t.try_get(id).is_some())
    }

    /// The [`Window`] for `id` in whichever open layer owns it (the main tree or
    /// any open dock), or `None` if no open layer holds it. This is the
    /// layer-aware counterpart to `self.windows.get`, which only sees the
    /// *current* layer and panics on an id that lives in another layer — exactly
    /// what happens when code iterates the cross-layer [`Editor::window_ids`].
    pub(crate) fn window(&self, id: WindowId) -> Option<&Window> {
        self.tree_of_window(id).and_then(|(_, t)| t.try_get(id))
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
        let prev = self.focused_layer;
        self.stash_focused_view();
        // The live tree parks into the outgoing layer's active slot; the incoming
        // layer's active slot (currently parked) becomes live.
        let from_tab = self
            .stack(self.focused_layer)
            .expect("the focused layer always has a stack")
            .current;
        let to_tab = self
            .stack(target)
            .expect("switch_layer target must be an open layer")
            .current;
        self.swap_live_tree((self.focused_layer, from_tab), (target, to_tab));
        self.focused_layer = target;
        if let Layer::Dock(s) = target {
            self.last_dock = s;
        }
        // Auto-hide: a dock marked `autohide` collapses as soon as focus leaves it.
        // Its tree is already parked by the swap above, so this is just the flag; the
        // `relayout()` below reflects the now-hidden band. Re-entrancy is safe —
        // `hide_dock` reaches here via `switch_layer(Main)` and would set the same
        // flag idempotently.
        if let Layer::Dock(s) = prev {
            if self.dock_options[s.idx()].auto_hide {
                self.dock_hidden[s.idx()] = true;
            }
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
        if self.dock_exists(side) {
            // Already present (visible or hidden): un-hide it, honor the new size,
            // ensure the requested buffer shows, and focus it.
            self.dock_hidden[side.idx()] = false;
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
        // Give the new dock a one-tab stack whose tree is live, park the outgoing
        // layer's active tree into its own slot, install the dock tree as live, and
        // enter it.
        self.stash_focused_view();
        let tab_id = self.alloc_tab_id();
        self.dock_tabs[side.idx()] = Some(TabStack::live(tab_id));
        let outgoing = std::mem::replace(&mut self.windows, tree);
        let from_tab = self
            .stack(self.focused_layer)
            .expect("the focused layer always has a stack")
            .current;
        *self.slot_tree_mut(self.focused_layer, from_tab) = Some(outgoing);
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
        if !self.dock_exists(side) {
            return;
        }
        if self.focused_layer == Layer::Dock(side) {
            self.switch_layer(Layer::Main);
        }
        // Drop the dock's whole tab stack (every tab's tree); the buffers each
        // showed stay loaded in the store, like closing a window.
        self.dock_tabs[side.idx()] = None;
        self.dock_sizes[side.idx()] = 0;
        self.dock_hidden[side.idx()] = false;
        self.relayout();
        self.ensure_visible();
    }

    /// Focus the dock on `side` (`nx.dock.focus` / `:DockFocus`, and the
    /// `<C-w><C-w>` directional cross). Focusing a hidden dock un-hides it first
    /// (you can't focus what isn't shown). A no-op if the dock isn't present.
    pub(crate) fn focus_dock(&mut self, side: DockSide) {
        if self.dock_exists(side) {
            self.dock_hidden[side.idx()] = false;
            self.switch_layer(Layer::Dock(side));
        }
    }

    /// Hide the dock on `side` — collapse it from view while keeping its whole
    /// [`TabStack`] parked (content, internal splits, tab pages, cursor and scroll
    /// all survive), the toggle / auto-hide counterpart of [`Editor::close_dock`]
    /// (which drops the stack). If it is the focused layer, focus crosses back to
    /// main first so the hidden dock is a *parked* layer, never the live
    /// [`Editor::windows`]. A no-op if the dock isn't currently visible. Backs
    /// `nx.dock.hide` / `:DockHide`.
    pub(crate) fn hide_dock(&mut self, side: DockSide) {
        if !self.dock_is_open(side) {
            return;
        }
        if self.focused_layer == Layer::Dock(side) {
            self.switch_layer(Layer::Main);
        }
        self.dock_hidden[side.idx()] = true;
        self.relayout();
        self.ensure_visible();
    }

    /// Show (un-hide) the dock on `side` and focus it, restoring the content it had
    /// when hidden. A no-op if the dock isn't present (toggling has nothing to
    /// restore; open a fresh dock with [`Editor::open_dock`] instead). Backs
    /// `nx.dock.show` / `:DockShow`.
    pub(crate) fn show_dock(&mut self, side: DockSide) {
        if !self.dock_exists(side) {
            return;
        }
        self.dock_hidden[side.idx()] = false;
        self.focus_dock(side);
        self.relayout();
    }

    /// Toggle the dock on `side`: a visible dock is hidden, a hidden one is shown
    /// (with its preserved content), and an absent side is reported (toggle has no
    /// size/buffer to mint a fresh dock from — that's [`Editor::open_dock`]'s job).
    /// Backs `nx.dock.toggle` / `:DockToggle`.
    pub(crate) fn toggle_dock(&mut self, side: DockSide) {
        if self.dock_is_open(side) {
            self.hide_dock(side);
        } else if self.dock_exists(side) {
            self.show_dock(side);
        } else {
            self.echo(format!("nx.dock: no dock on {} to toggle", side.keyword()));
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

    /// `nx.layer.focus(target)` / `nx.layer.main()` — move focus to a layer by
    /// name: `"main"` crosses back to the main editor area
    /// ([`ensure_main_layer`](Editor::ensure_main_layer)), a dock side keyword
    /// (`"left"`/`"right"`/`"top"`/`"bottom"`) focuses that dock
    /// ([`focus_dock`](Editor::focus_dock)). An unknown name is reported (no silent
    /// fallback). The Main-layer cross is what lets a dock plugin (a file tree) send
    /// focus back to the editor after opening a file.
    pub fn focus_layer_named(&mut self, target: &str) {
        if target == "main" {
            self.ensure_main_layer();
        } else if DockSide::from_keyword(target).is_some() {
            self.focus_dock_named(target);
        } else {
            self.echo(format!("E474: Invalid layer: {target}"));
        }
    }

    /// `nx.dock.toggle(side)` — toggle a dock's visibility by side keyword.
    pub fn toggle_dock_named(&mut self, side: &str) {
        match DockSide::from_keyword(side) {
            Some(s) => self.toggle_dock(s),
            None => self.echo(format!("E474: Invalid dock side: {side}")),
        }
    }

    /// `nx.dock.hide(side)` — hide a dock (keep its content) by side keyword.
    pub fn hide_dock_named(&mut self, side: &str) {
        match DockSide::from_keyword(side) {
            Some(s) => self.hide_dock(s),
            None => self.echo(format!("E474: Invalid dock side: {side}")),
        }
    }

    /// `nx.dock.show(side)` — show (un-hide) and focus a dock by side keyword.
    pub fn show_dock_named(&mut self, side: &str) {
        match DockSide::from_keyword(side) {
            Some(s) => self.show_dock(s),
            None => self.echo(format!("E474: Invalid dock side: {side}")),
        }
    }

    /// Whether the dock on `side` (keyword) is present but hidden — the string-keyed
    /// read for the server / RPC boundary, distinguishing hidden from closed.
    pub fn dock_is_hidden_named(&self, side: &str) -> bool {
        DockSide::from_keyword(side)
            .is_some_and(|s| self.dock_exists(s) && self.dock_hidden[s.idx()])
    }

    /// Whether the dock on `side` (keyword) is open — the string-keyed
    /// [`Editor::dock_is_open`] for the server's `nx._docks` mirror.
    pub fn dock_is_open_named(&self, side: &str) -> bool {
        DockSide::from_keyword(side).is_some_and(|s| self.dock_is_open(s))
    }

    /// Set a dock's reserved size (columns for left/right, rows for top/bottom),
    /// floored at 1, then relayout. The shared core of the `size` dock option and
    /// the mouse edge-drag resize (`mouse_resize_drag`). [`Editor::dock_bands`]
    /// clamps the stored value down at render time if the main area would vanish.
    pub(crate) fn set_dock_size(&mut self, side: DockSide, size: usize) {
        self.dock_sizes[side.idx()] = size.max(1);
        self.relayout();
        self.ensure_visible();
    }

    /// `nx.dock.opt(side).<name> = <number>` — set a numeric dock option by side
    /// keyword: `showtabline` (0/1/2, the per-dock tabline override), `laststatus`
    /// (0/1/2/3, the per-dock statusline override), or `size` (the dock's reserved
    /// width/height, kept in `dock_sizes`). An unknown side or option is reported,
    /// never silently ignored.
    pub fn set_dock_option_num(&mut self, side: &str, name: &str, value: i64) {
        let Some(s) = DockSide::from_keyword(side) else {
            self.echo(format!("E474: Invalid dock side: {side}"));
            return;
        };
        match name {
            "showtabline" => self.dock_options[s.idx()].showtabline = Some(value.clamp(0, 2) as u8),
            "laststatus" => self.dock_options[s.idx()].laststatus = Some(value.clamp(0, 3) as u8),
            "size" => self.dock_sizes[s.idx()] = value.max(1) as usize,
            "autohide" => self.dock_options[s.idx()].auto_hide = value != 0,
            other => return self.echo(format!("E474: unknown dock option: {other}")),
        }
        self.relayout();
        self.ensure_visible();
    }

    /// `nx.dock.opt(side).<name> = <string>` — set a string dock option by side
    /// keyword: `title` (a fixed strip label) or `winhighlight` (a per-window
    /// highlight-group remap, `"Normal:NormalSB,EndOfBuffer:Hidden"`, applied to
    /// every window in the dock). A malformed `winhighlight` entry (no `:` or an
    /// empty side) is reported rather than silently dropped — the value is still
    /// stored so the well-formed pairs take effect.
    pub fn set_dock_option_str(&mut self, side: &str, name: &str, value: String) {
        let Some(s) = DockSide::from_keyword(side) else {
            self.echo(format!("E474: Invalid dock side: {side}"));
            return;
        };
        match name {
            "title" => self.dock_options[s.idx()].title = value,
            "winhighlight" => {
                let bad = crate::WinHl::parse_reporting(&value).1;
                if !bad.is_empty() {
                    self.echo(format!("nx.dock: ignoring malformed winhighlight: {bad:?}"));
                }
                self.dock_options[s.idx()].winhighlight = value;
            }
            other => return self.echo(format!("E474: unknown dock option: {other}")),
        }
        self.relayout();
    }

    /// The dock-option values for `side` (keyword), for the `nx._dock_opts` read
    /// surface: `(showtabline_override, title, size)`. `None`/empty/`0` where unset
    /// or the side is invalid.
    pub fn dock_option_values(&self, side: &str) -> (Option<u8>, String, usize) {
        match DockSide::from_keyword(side) {
            Some(s) => {
                let o = &self.dock_options[s.idx()];
                (o.showtabline, o.title.clone(), self.dock_sizes[s.idx()])
            }
            None => (None, String::new(), 0),
        }
    }

    /// A dock's title (the `nx.dock` strip label), for the view projection. Empty
    /// when unset or the dock isn't open.
    pub(crate) fn dock_title(&self, side: DockSide) -> &str {
        &self.dock_options[side.idx()].title
    }

    /// The effective `'winhighlight'` remap for a window in `region` carrying
    /// window-local options `wo`, parsed for the view projection. The window's own
    /// `winhighlight` wins when set; otherwise a window in a dock inherits that
    /// dock's `winhighlight`; otherwise the remap is empty (the common case — most
    /// windows rename nothing). Parsed here on each redraw, mirroring how
    /// `'fillchars'` is parsed lazily — the strings are short and windows are few.
    pub(crate) fn effective_winhighlight(
        &self,
        region: crate::view::WindowRegion,
        wo: &WindowOptions,
    ) -> crate::WinHl {
        use crate::view::WindowRegion;
        if !wo.winhighlight.is_empty() {
            return crate::WinHl::parse(&wo.winhighlight);
        }
        let dock = match region {
            WindowRegion::Main => None,
            WindowRegion::DockLeft => Some(DockSide::Left),
            WindowRegion::DockRight => Some(DockSide::Right),
            WindowRegion::DockTop => Some(DockSide::Top),
            WindowRegion::DockBottom => Some(DockSide::Bottom),
        };
        match dock.map(|s| &self.dock_options[s.idx()].winhighlight) {
            Some(s) if !s.is_empty() => crate::WinHl::parse(s),
            _ => crate::WinHl::default(),
        }
    }

    /// Every **hidden** dock as `(side, label)` in [`DockSide::ALL`] order, for the
    /// collapsed-dock indicator. The label is the dock's `title` if set, else the
    /// side keyword (`"left"`/…). Empty when no dock is hidden. A hidden dock keeps
    /// its content parked; this is the only on-screen hint that it still exists, so
    /// the client paints these as clickable chips on the idle command-line row and
    /// [`Editor::hidden_chip_at`] maps a click back to the side to re-show.
    pub(crate) fn hidden_dock_chips(&self) -> Vec<(DockSide, String)> {
        DockSide::ALL
            .into_iter()
            .filter(|&s| self.dock_exists(s) && self.dock_hidden[s.idx()])
            .map(|s| {
                let title = self.dock_title(s);
                let label = if title.is_empty() {
                    s.keyword().to_string()
                } else {
                    title.to_string()
                };
                (s, label)
            })
            .collect()
    }
}
