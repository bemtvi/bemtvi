//! The floating selectable-list widget — `nx.ui.select` today, and the shared
//! surface completion and the fuzzy picker build on (see
//! `docs/specs/2026-06-14-nx-ui-float-widget.md`). Like the bottom [`Panel`] it
//! grabs every keystroke while open (navigation + confirm / cancel), but it
//! floats over the text — anchored under the cursor or centered over the
//! editor — and resolves to a single choice (the highlighted index the user
//! confirmed, or a cancel), drained by the server into the waiting
//! `nx.ui.select` callback. The core keeps only the logical state; the server
//! projects the float's screen geometry from [`Editor::menu_view`].
//!
//! [`Panel`]: super::Panel

use super::*;
use crate::input::{Key, KeyCode};
use crate::view::MenuView;

/// Where the menu floats. `Cursor` anchors it under the cursor (the
/// `nx.ui.select` / completion shape); `Editor` centers it over the editor (the
/// picker shape). Phase 1 (`nx.ui.select`) uses `Cursor`; `Editor` is modeled
/// now so the picker reuses the same widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuPlacement {
    Cursor,
    Editor,
}

/// An open menu: the choice labels, the highlighted index, where it floats, and
/// the `gg`-pending flag (the two-key motion, mirroring the [`Panel`](super::Panel)).
#[derive(Clone)]
pub(crate) struct Menu {
    pub items: Vec<String>,
    pub cursor: usize,
    pub placement: MenuPlacement,
    pub gpending: bool,
}

impl Editor {
    /// Open a floating selectable list of `items`, with `cursor` (clamped) the
    /// initially-highlighted row. Grabs input focus until the user confirms
    /// (`<CR>`) or cancels (`<Esc>` / `q`); the outcome lands in
    /// [`Editor::menu_results`] for the server to deliver. The list must be
    /// non-empty — the server resolves an empty `nx.ui.select` to a cancel
    /// without opening a menu.
    pub fn open_menu(&mut self, items: Vec<String>, placement: MenuPlacement, cursor: usize) {
        let last = items.len().saturating_sub(1);
        self.menu = Some(Menu {
            cursor: cursor.min(last),
            items,
            placement,
            gpending: false,
        });
    }

    /// Close the menu without recording a choice — the caller has already
    /// recorded the outcome (confirm / cancel), or is force-closing. Returns
    /// focus to the text window. A no-op when no menu is open.
    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Whether a menu is currently open (and grabbing input).
    pub fn menu_is_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Handle a keystroke while the menu has focus: vertical motions
    /// (`j` / `k` / `<C-n>` / `<C-p>` / arrows / `gg` / `G` / `Home` / `End`)
    /// move the highlight (clamped, no wrap); `<CR>` confirms the highlighted
    /// row; `<Esc>` / `q` cancels. Every other key is ignored — the buffer is
    /// untouched while the menu is open.
    pub(crate) fn handle_menu(&mut self, key: Key) {
        self.message.clear();

        // Cancel: record a cancel outcome and close.
        if key.code == KeyCode::Esc || matches!(key.as_char(), Some('q')) {
            self.menu_results.push(None);
            self.close_menu();
            return;
        }

        // Confirm: record the highlighted index, then close.
        if key.code == KeyCode::Enter {
            if let Some(idx) = self.menu.as_ref().map(|m| m.cursor) {
                self.menu_results.push(Some(idx));
            }
            self.close_menu();
            return;
        }

        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let last = menu.items.len().saturating_sub(1);

        // `gg` is two keys; the first `g` arms `gpending` (mirrors the panel).
        if menu.gpending {
            menu.gpending = false;
            if key.as_char() == Some('g') {
                menu.cursor = 0;
            }
        } else if key.as_char() == Some('g') {
            menu.gpending = true;
        } else {
            match (key.code, key.as_char()) {
                (KeyCode::Down, _) | (_, Some('j')) => menu.cursor = (menu.cursor + 1).min(last),
                (KeyCode::Char('n'), _) if key.ctrl => menu.cursor = (menu.cursor + 1).min(last),
                (KeyCode::Up, _) | (_, Some('k')) => menu.cursor = menu.cursor.saturating_sub(1),
                (KeyCode::Char('p'), _) if key.ctrl => menu.cursor = menu.cursor.saturating_sub(1),
                (_, Some('G')) => menu.cursor = last,
                (KeyCode::Home, _) => menu.cursor = 0,
                (KeyCode::End, _) => menu.cursor = last,
                _ => {}
            }
        }
    }

    /// Project the open menu into its renderable [`MenuView`] — the logical data
    /// only; the server computes the float's anchor and size. `None` when closed.
    pub(crate) fn menu_view(&self) -> Option<MenuView> {
        self.menu.as_ref().map(|m| MenuView {
            items: m.items.clone(),
            selected: m.cursor,
            placement: m.placement,
        })
    }
}
