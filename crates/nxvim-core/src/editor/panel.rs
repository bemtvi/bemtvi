//! The bottom docked panel (`:messages`, `:ls`, LSP lists): rendering,
//! navigation, and selection.

use super::*;
use crate::unicode;
use crate::view::PanelView;
use std::path::PathBuf;

/// Word-wrap `text` into rows no wider than `width` screen cells, for the bottom
/// [`Panel`] (nxvim's only multi-line, wrap-able surface — long hover docs,
/// messages, and location-list rows). Breaks after the last space that fits so
/// words stay whole, hard-breaking a run longer than `width`; an empty line
/// yields a single empty row so it still occupies one. Width is counted in screen
/// cells (tabs and wide chars via [`unicode::byte_at_virtcol`]), matching how the
/// panel is painted, so a wrapped row never overflows its column.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // Bytes of `rest` that fit in `width` cells (a grapheme boundary).
        let mut take = unicode::byte_at_virtcol(rest, width, unicode::TABSTOP);
        if take == 0 {
            // A single grapheme wider than `width`: take it whole so we progress.
            take = unicode::next_grapheme(rest, 0).max(1);
        }
        if take >= rest.len() {
            rows.push(rest.to_string());
            break;
        }
        match rest[..take].rfind(' ') {
            // Break after the last space that fits (dropped), keeping words whole.
            Some(sp) if sp > 0 => {
                rows.push(rest[..sp].trim_end().to_string());
                rest = &rest[sp + 1..];
            }
            // No usable space: hard-break mid-run at the width.
            _ => {
                rows.push(rest[..take].to_string());
                rest = &rest[take..];
            }
        }
    }
    rows
}

impl Editor {
    /// Open (or replace) the bottom panel with `title` + `lines` and focus it.
    /// The text window shrinks to make room; the cursor is re-clamped so it
    /// stays visible in the reduced viewport. `wants_select` enables `<CR>`
    /// select events on the panel (the scripting `on_select` callback / RPC
    /// notification); the built-in viewer panels pass `false`. `cursor` is the
    /// initially selected line (0-based, clamped to the last line); the panel is
    /// scrolled so it is visible — `:messages` opens on its last line, `:ls` on
    /// the current buffer.
    ///
    /// Public so it can be driven from the scripting surface (`vim.panel.open`
    /// and the `nxvim_panel_open` RPC), as well as the `:messages` / `:ls`
    /// ex-commands.
    pub fn open_panel(
        &mut self,
        title: impl Into<String>,
        lines: Vec<String>,
        wants_select: bool,
        cursor: usize,
    ) {
        let cursor = cursor.min(lines.len().saturating_sub(1));
        // A panel being replaced (without an explicit close first) is still the
        // "last shown" one, so remember it for `:panelopen`.
        if let Some(replaced) = self.panel.take() {
            self.last_panel = Some(replaced);
        }
        self.panel = Some(Panel {
            title: title.into(),
            lines,
            cursor,
            top: 0,
            height: PANEL_HEIGHT,
            wants_select,
            targets: Vec::new(),
        });
        // The panel claimed rows off the bottom: shrink the windows area to match.
        self.relayout();
        self.ensure_visible();
        self.scroll_panel_into_view();
    }

    /// Attach per-line jump targets to the open panel, making it a navigable
    /// location list: `<CR>` on a line whose target is `Some((path, line, col))`
    /// jumps there (and closes the panel) via [`Editor::jump_to`], instead of
    /// firing a select event. The list is indexed in lockstep with the panel's
    /// lines (a shorter list or a `None` entry leaves that line non-navigable). A
    /// no-op when no panel is open. The targets ride along in the `:panelopen`
    /// snapshot, so a reopened list still jumps.
    pub fn set_panel_targets(&mut self, targets: Vec<Option<(PathBuf, usize, usize)>>) {
        if let Some(panel) = self.panel.as_mut() {
            panel.targets = targets;
        }
    }

    /// Enable or disable `<CR>` select events on the open panel
    /// (`vim.panel.on_select`). A no-op when no panel is open.
    pub fn set_panel_on_select(&mut self, wants: bool) {
        if let Some(panel) = self.panel.as_mut() {
            panel.wants_select = wants;
        }
    }

    /// Move the open panel's selection to `index` (0-based, clamped to the last
    /// line) and scroll it into view (`vim.panel.set_cursor` /
    /// `nxvim_panel_set_cursor`). A no-op when no panel is open.
    pub fn set_panel_cursor(&mut self, index: usize) {
        if let Some(panel) = self.panel.as_mut() {
            let last = panel.lines.len().saturating_sub(1);
            panel.cursor = index.min(last);
        }
        self.scroll_panel_into_view();
    }

    /// Move the panel selection to the logical entry occupying display `row`
    /// (0-based, within the visible content) — the mouse-click counterpart to the
    /// keyboard motions (`nxvim_panel_click`). The panel word-wraps, so a display
    /// row maps back to its logical entry by replaying the same wrap walk as
    /// [`Editor::panel_view`], starting from `top`. A row past the last visible
    /// entry selects that last entry. A no-op when no panel is open.
    pub fn set_panel_cursor_by_row(&mut self, row: usize) {
        let height = self.panel_content_height();
        let width = self.panel_width();
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        let mut display = 0;
        let mut logical = panel.top;
        let mut target = logical.min(panel.lines.len().saturating_sub(1));
        while display < height && logical < panel.lines.len() {
            let span = wrap_to_width(&panel.lines[logical], width).len().max(1);
            target = logical;
            if row < display + span {
                break;
            }
            display += span;
            logical += 1;
        }
        panel.cursor = target;
        self.scroll_panel_into_view();
    }

    /// Replace the open panel's content (`vim.panel.set_lines` /
    /// `nxvim_panel_set_lines`), keeping its title and re-clamping the cursor and
    /// scroll to the new content. A no-op when no panel is open.
    pub fn set_panel_lines(&mut self, lines: Vec<String>) {
        if let Some(panel) = self.panel.as_mut() {
            let last = lines.len().saturating_sub(1);
            panel.lines = lines;
            panel.cursor = panel.cursor.min(last);
            panel.top = panel.top.min(last);
            // The content changed, so any per-line jump targets no longer align.
            panel.targets.clear();
        }
        self.scroll_panel_into_view();
    }

    /// Re-derive the open panel's `top` so its `cursor` line stays within the
    /// visible window — the shared scroll step for opening, content swaps, and
    /// keyboard motion. A no-op when no panel is open.
    fn scroll_panel_into_view(&mut self) {
        let height = self.panel_content_height().max(1);
        let width = self.panel_width();
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        if panel.cursor < panel.top {
            // Cursor above the window: pin the window to it.
            panel.top = panel.cursor;
            return;
        }
        // Cursor at/below the window: raise `top` toward `cursor` until the lines
        // `[top..=cursor]`, **word-wrapped**, fit in `height` display rows — so the
        // selected entry's last wrapped row stays visible. (A single entry taller
        // than the panel can't fully fit; the loop stops at `top == cursor`,
        // showing it from its first row.) Wrapping makes this display-row aware,
        // where the old logical-line arithmetic clipped tall entries.
        while panel.top < panel.cursor {
            let rows: usize = panel.lines[panel.top..=panel.cursor]
                .iter()
                .map(|line| wrap_to_width(line, width).len())
                .sum();
            if rows <= height {
                break;
            }
            panel.top += 1;
        }
    }

    /// The panel's content width in screen cells: the full terminal width, since
    /// the panel spans it edge to edge with only a top border (no side borders,
    /// no number gutter). Drives the word-wrap in [`Editor::panel_view`].
    fn panel_width(&self) -> usize {
        self.width.max(1)
    }

    /// Close the panel and return focus to the text window, which grows back.
    /// Public for the scripting surface (`vim.panel.close` /
    /// `nxvim_panel_close`); a no-op when no panel is open. The closed panel is
    /// retained as the `:panelopen` target (content + selection preserved).
    pub fn close_panel(&mut self) {
        if let Some(closed) = self.panel.take() {
            self.last_panel = Some(closed);
        }
        // The windows area grows back into the rows the panel held.
        self.relayout();
        self.ensure_visible();
    }

    /// Reopen the most recently shown panel (`:panelopen`) with its retained
    /// content and selection — e.g. bringing an LSP references list back after it
    /// was dismissed. Returns `false` (and changes nothing) when no panel has been
    /// shown yet, so the caller can report it. The retained snapshot is kept, so
    /// the panel can be reopened again after another close.
    pub fn reopen_last_panel(&mut self) -> bool {
        let Some(panel) = self.last_panel.clone() else {
            return false;
        };
        // Re-clamp to the current content, in case the snapshot was taken
        // mid-keystroke.
        let last = panel.lines.len().saturating_sub(1);
        self.panel = Some(Panel {
            cursor: panel.cursor.min(last),
            top: panel.top.min(last),
            ..panel
        });
        // Reopening reclaims the panel's rows from the windows area.
        self.relayout();
        self.ensure_visible();
        self.scroll_panel_into_view();
        true
    }

    /// Whether a panel is currently open (the `nxvim_panel_is_open` query).
    pub fn panel_is_open(&self) -> bool {
        self.panel.is_some()
    }

    /// The open panel's title, or `None` if no panel is open — lets the server
    /// recognize *which* panel a `<CR>` select came from (e.g. route a select on
    /// the LSP code-action list to applying that action, not the generic
    /// `on_select` path).
    pub fn panel_title(&self) -> Option<&str> {
        self.panel.as_ref().map(|p| p.title.as_str())
    }

    /// Total screen rows the panel occupies (its content plus the one title
    /// row), clamped so the text window always keeps at least one row. `0` when
    /// no panel is open.
    pub(crate) fn panel_rows(&self) -> usize {
        match &self.panel {
            None => 0,
            Some(p) => (p.height + 1).min(self.height.saturating_sub(1)),
        }
    }

    /// The panel's visible content height (its rows minus the title), `0` when
    /// no panel is open or it has been clamped to nothing.
    fn panel_content_height(&self) -> usize {
        self.panel_rows().saturating_sub(1)
    }

    /// Project the panel into the renderable [`PanelView`]: the visible content
    /// **word-wrapped** to the panel width, the selected entry's first display row
    /// and how many rows it spans, and the clamped content height. `None` when no
    /// panel is open. (`pub(crate)` so [`View`] can build it while [`Panel`] stays
    /// private.)
    ///
    /// Wrapping is display-only: `cursor`/`top` remain logical-entry indices (so
    /// `j`/`k`/`<CR>`/jump targets address whole entries), but each entry expands
    /// to one or more display rows here so long text is laid out across rows
    /// instead of clipped at the panel's right edge.
    pub(crate) fn panel_view(&self) -> Option<PanelView> {
        let p = self.panel.as_ref()?;
        let height = self.panel_content_height();
        let width = self.panel_width();

        let mut lines: Vec<String> = Vec::new();
        let mut cursor_row = 0;
        let mut cursor_span = 1;
        let mut logical = p.top;
        while lines.len() < height && logical < p.lines.len() {
            let start = lines.len();
            for row in wrap_to_width(&p.lines[logical], width) {
                if lines.len() >= height {
                    break;
                }
                lines.push(row);
            }
            if logical == p.cursor {
                cursor_row = start;
                cursor_span = lines.len().saturating_sub(start).max(1);
            }
            logical += 1;
        }

        Some(PanelView {
            title: p.title.clone(),
            lines,
            cursor_row,
            cursor_span,
            height,
        })
    }

    /// Apply a named `panel` action, dispatched by a `panel`-bucket keymap (the
    /// default maps in `prelude/keymap.lua`, or a user override) while the bottom
    /// panel is focused. The rebindable operations: `next`/`prev` move the panel
    /// cursor by one, `first`/`last` jump to the ends, `half_down`/`half_up` scroll
    /// a half page, `confirm` resolves the current line (jump to a location-list
    /// target, else a select event on a select-enabled panel), and `close` dismisses
    /// the panel. After a cursor move the panel scrolls to keep the cursor visible.
    /// An unknown name fails loud per the no-silent-stub rule. The panel has no text
    /// fallthrough — an unmapped key is inert (handled in [`Editor::input`]).
    pub fn apply_panel_action(&mut self, action: &str) -> Result<(), String> {
        self.message.clear();

        match action {
            "close" => {
                self.close_panel();
                return Ok(());
            }
            "confirm" => {
                // A navigable location-list line jumps to its target and closes the
                // panel behind the jump. Owned here (the core already has `jump_to`)
                // so the targets travel with the `:panelopen` snapshot and a reopened
                // list still navigates.
                if let Some(Some((path, line, col))) = self
                    .panel
                    .as_ref()
                    .and_then(|p| p.targets.get(p.cursor).cloned())
                {
                    self.close_panel();
                    self.jump_to(&path, line, col);
                    return Ok(());
                }
                // Otherwise `confirm` selects the current line: record it for the
                // server to dispatch to the scripting `on_select` handler. Only for
                // select-enabled panels, so a stale handler can't fire on a built-in
                // `:messages` viewer.
                if let Some(p) = &self.panel {
                    if p.wants_select {
                        if let Some(line) = p.lines.get(p.cursor) {
                            self.panel_selects.push((p.cursor, line.clone()));
                        }
                    }
                }
                return Ok(());
            }
            _ => {}
        }

        let ph = self.panel_content_height().max(1);
        let half = (ph / 2).max(1);
        let Some(panel) = self.panel.as_mut() else {
            return Ok(());
        };
        let last = panel.lines.len().saturating_sub(1);
        match action {
            "next" => panel.cursor = (panel.cursor + 1).min(last),
            "prev" => panel.cursor = panel.cursor.saturating_sub(1),
            "first" => panel.cursor = 0,
            "last" => panel.cursor = last,
            "half_down" => panel.cursor = (panel.cursor + half).min(last),
            "half_up" => panel.cursor = panel.cursor.saturating_sub(half),
            other => return Err(format!("unknown panel action {other:?}")),
        }

        // Scroll the panel so the cursor line stays within the visible window.
        self.scroll_panel_into_view();
        Ok(())
    }
}
