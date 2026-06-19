//! The bottom **panel**: a transient, focus-locked overlay over an ordinary buffer.
//!
//! A panel is not a widget and carries no bespoke content/navigation state — it is an
//! ordinary `nomodifiable` buffer shown in a bottom split (vim's `botright`), with two
//! properties layered on top:
//!
//! - **Displace**: opening shrinks the main window into the rows above; closing collapses
//!   the split and restores the layout. This rides the existing
//!   [`Editor::open_bottom_window`] / [`Editor::remove_window`].
//! - **Hard focus lock**: while a panel is open, focus is pinned to its window — the guard
//!   in [`Editor::focus_window`] refuses to move focus anywhere else, so `<C-w>` navigation
//!   (cycle / directional), `nvim_set_current_win`, and mouse focus are all inert. Only an
//!   explicit close ([`Editor::close_panel`]) dismisses it.
//!
//! Everything *inside* the panel is plain buffer behavior: motions navigate, search works,
//! and any activation key (`<CR>` to select, `q` / `<Esc>` to dismiss) is an ordinary
//! buffer-local keymap installed by a `FileType` autocmd — never special-cased in the input
//! loop. Built-in listings (`:messages`, `:registers`, `:ls`, `:marks`, …) mount here via
//! [`Editor::open_scratch_listing`] / [`Editor::open_buffer_listing`]; scripts mount their
//! own via `nx.panel.open`.

use super::*;

/// The open panel's window, plus the window to restore focus to when it closes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PanelState {
    /// The window holding the panel buffer (the only window focus may rest on while
    /// the panel is up).
    pub window: WindowId,
    /// The window that was focused when the panel opened, refocused on close so the
    /// panel feels transient (open → interact → dismiss → back where you were).
    pub prev_window: WindowId,
    /// A gap (cells) from the editor edges, so a scripted panel needn't kiss the
    /// border. The panel is a tiled bottom window with no native inset concept, so
    /// [`Editor::relayout`] applies this as a one-off shrink of the panel window's
    /// rect after layout. `top` is unused (the panel's top edge is its height).
    /// `Margin::default()` (no gap) for the built-in listings.
    pub margin: Margin,
}

impl Editor {
    /// Mount `buf` as the panel in a bottom overlay `height` rows tall and lock focus to
    /// it. If a panel is already open, reuse its window — swapping in `buf` — rather than
    /// stacking a second overlay (so opening `:registers` over an open `:messages` panel
    /// just re-targets the one window). Otherwise remember the focused window and open a
    /// fresh bottom split. The caller has already loaded `buf`'s content and flipped it
    /// `nomodifiable`.
    pub(crate) fn open_panel(&mut self, buf: BufferId, height: usize) {
        if let Some(p) = self.panel {
            if self.window(p.window).is_some() {
                // `set_current_window` is permitted here (target == the panel window);
                // `switch_buffer` then re-targets that window's buffer in place.
                self.set_current_window(p.window);
                self.switch_buffer(buf);
                return;
            }
            // The panel window vanished out from under us (e.g. `:only`); remount.
            self.panel = None;
        }
        let prev_window = self.windows.current;
        let window = self.open_bottom_window(buf, height);
        self.panel = Some(PanelState {
            window,
            prev_window,
            margin: Margin::default(),
        });
    }

    /// Whether `id` is a registered panel display buffer (one of the named-panel registry's
    /// reused buffers). Such buffers are surfaces, not documents: excluded from `:ls` /
    /// buffer navigation and never shown in a normal window — see
    /// [`Editor::panel_buffers`].
    pub(crate) fn is_panel_buffer(&self, id: BufferId) -> bool {
        self.panel_buffers.iter().any(|(_, b)| *b == id)
    }

    /// The reused display buffer for panel `name`, minting (and registering) it on first
    /// use. Naming is what makes panels unique — the same `name` always returns the same
    /// buffer, so re-opening replaces its content in place.
    fn named_panel_buffer(&mut self, name: &str) -> BufferId {
        if let Some(b) = self
            .panel_buffers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| *b)
        {
            if self.buffers.map.contains_key(&b) {
                return b;
            }
            // A stale registry entry (its buffer was deleted) — drop it and re-mint below.
            self.panel_buffers.retain(|(n, _)| n != name);
        }
        let id = self.add_buffer(Buffer::empty());
        self.panel_buffers.push((name.to_string(), id));
        id
    }

    /// Open the **named panel** `name`: load `lines` into its registry buffer (reused, so a
    /// re-open replaces the content), flip it `nomodifiable`, tag `filetype` (whose
    /// `FileType` autocmd installs the buffer-local activation maps), and mount it as a
    /// focus-locked bottom overlay `height` rows tall with the cursor on line `cursor`
    /// (0-based, clamped). The single home every built-in listing and `nx.panel.open` flow
    /// through.
    pub fn open_named_panel(
        &mut self,
        name: &str,
        lines: Vec<String>,
        cursor: usize,
        filetype: &str,
        height: usize,
    ) {
        let buf = self.named_panel_buffer(name);
        // `load_str_into` edits the rope directly (not through the `modifiable()`
        // chokepoints), so flipping `nomodifiable` after is safe and refuses only the
        // user's later edits.
        self.load_str_into(buf, Some(name.to_string()), &lines.join("\n"));
        self.buffers.get_mut(buf).buffer.options.modifiable = false;
        self.set_filetype(buf, filetype);
        self.open_panel(buf, height);
        let last = self.buffer().line_count().saturating_sub(1);
        self.cursor = Cursor {
            line: cursor.min(last),
            col: 0,
        };
        self.ensure_visible();
    }

    /// Mount a **scripted panel** (`nx.panel.open{ name?, lines, filetype?, height? }`): a
    /// named panel (default name `[Panel]`) whose `filetype` (default `nxpanel`, whose
    /// `FileType` autocmd maps `q`/`<Esc>` to dismiss; a plugin passing its own filetype
    /// wires its own keys) drives any behavior. Behavior is attached the same way every
    /// listing does it — a `FileType` autocmd over an ordinary buffer, never a callback.
    /// `height` is an [`Extent`] (rows or a `vh`/`%` fraction of the editor height,
    /// resolved here); `margin` is a gap from the editor edges (the panel stays
    /// bottom-anchored but needn't kiss the border). `None` height ⇒ the default
    /// listing height.
    pub fn open_script_panel(
        &mut self,
        name: Option<String>,
        lines: Vec<String>,
        filetype: Option<String>,
        height: Option<Extent>,
        margin: Margin,
    ) {
        // Resolve a fractional height against the editor screen height (`vh`), like
        // a fractional float resolves against the viewport. The bottom panel then
        // reflows proportionally on resize, as every tiled window does.
        let height = height
            .map(|e| e.resolve(self.height))
            .unwrap_or(super::buffers::LISTING_HEIGHT)
            .max(1);
        self.open_named_panel(
            name.as_deref().unwrap_or("[Panel]"),
            lines,
            0,
            filetype.as_deref().unwrap_or("nxpanel"),
            height,
        );
        // Record the requested gap and re-lay so the inset (applied at the end of
        // `relayout`) takes effect on this first frame.
        if let Some(p) = self.panel.as_mut() {
            p.margin = margin;
        }
        self.relayout();
        self.ensure_visible();
    }

    /// Paint highlight group `group` across the full width of each *current-buffer*
    /// line whose `flags` entry is set, as range extmarks in the reserved
    /// [`LISTING_HL_NS`](crate::extmark::LISTING_HL_NS). Built for the listing panels
    /// (`:messages` flags its error lines `ErrorMsg`): call it right after the listing
    /// is mounted, when its buffer is current and freshly loaded. The reload that backs
    /// a re-open clears the namespace's old marks, so each open repaints cleanly.
    pub(crate) fn highlight_listing_lines(&mut self, flags: &[bool], group: &str) {
        let buf = self.current_buffer_id();
        let Some(b) = self.buffer_of_mut(buf) else {
            return;
        };
        for (line, _) in flags.iter().enumerate().filter(|(_, on)| **on) {
            if line >= b.line_count() {
                break;
            }
            let start = b.line_start(line);
            let end = start + b.line_len(line);
            b.extmarks.set(
                crate::extmark::LISTING_HL_NS,
                None,
                start,
                Some(end),
                Some(group.to_string()),
                crate::extmark::DEFAULT_PRIORITY,
                None,
            );
        }
    }

    /// Dismiss the open panel (a no-op if none): collapse its overlay, restoring the
    /// layout, and refocus the window the panel sprang from if it still lives. Clearing
    /// [`Editor::panel`] *first* is what lets the subsequent focus moves through the
    /// [`focus_window`](Editor::focus_window) lock.
    pub fn close_panel(&mut self) {
        let Some(p) = self.panel.take() else { return };
        if self.window(p.window).is_some() {
            self.remove_window(p.window);
        }
        if self.window(p.prev_window).is_some() {
            self.set_current_window(p.prev_window);
        }
        self.ensure_visible();
    }

    /// Whether a panel is currently open. Backs the `nxvim_panel_is_open` RPC and the
    /// focus lock.
    pub fn panel_is_open(&self) -> bool {
        self.panel.is_some()
    }

    /// The open panel's window, if any — the one window [`focus_window`](Editor::focus_window)
    /// may land on while a panel is up.
    pub(crate) fn panel_window(&self) -> Option<WindowId> {
        self.panel.map(|p| p.window)
    }

    /// The window currently holding the **hard focus lock**, if any: the topmost grabbing
    /// `nx.view` modal float ([`Editor::view_float_lock`], a stack — focus pins to the
    /// innermost), else the bottom panel. The single source the
    /// [`focus_window`](Editor::focus_window) guard consults so every focus path is pinned to
    /// one overlay window until it is dismissed.
    pub(crate) fn focus_lock_window(&self) -> Option<WindowId> {
        self.view_float_lock
            .last()
            .copied()
            .or_else(|| self.panel_window())
    }
}
