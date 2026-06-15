//! The unified floating selectable-list widget — `nx.ui.select` (a promptless
//! choice menu) and `nx.picker` (a fuzzy finder with a prompt) both render through
//! it (see `docs/specs/2026-06-14-nx-ui-float-widget.md`). Like the bottom
//! [`Panel`] it grabs every keystroke while open, but it floats over the text —
//! anchored under the cursor or centered over the editor — and resolves to a
//! single choice.
//!
//! Two orthogonal capabilities make it one shape instead of three:
//!
//! - **prompt** (`Some` ⇒ picker, `None` ⇒ select). With a prompt, keystrokes edit
//!   a query field and **never reach the document**; navigation is `<C-n>`/`<C-p>`/
//!   arrows. Without one, the list is driven vim-style (`j`/`k`/`gg`/`G`).
//! - **dynamic** — a *static* source is fuzzy-matched **locally in Rust** as the
//!   query changes ([`crate::fuzzy`]); a *dynamic* source (live grep) bypasses the
//!   matcher and the widget forwards each query edit to the source under a
//!   **generation token** so a response for a query the user has typed past is
//!   dropped (the server gates the stale pushes).
//!
//! The core keeps only the logical state; the server projects the float's screen
//! geometry from [`Editor::menu_view`] and orchestrates the (async) source.
//!
//! [`Panel`]: super::Panel

use std::ops::Range;

use super::*;
use crate::input::{Key, KeyCode};
use crate::view::MenuView;

/// Where the menu floats. `Cursor` anchors it under the cursor (the
/// `nx.ui.select` / completion shape); `Editor` centers it over the editor (the
/// picker shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuPlacement {
    Cursor,
    Editor,
}

/// Where a picker's prompt sits relative to its results list — above it (`Top`,
/// the default) or below it (`Bottom`, the telescope-style "input at the bottom"
/// layout). Only meaningful for a picker (a promptless `nx.ui.select` has no
/// prompt); the client lays the box out accordingly and draws the separator
/// between the prompt and the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PromptPos {
    #[default]
    Top,
    Bottom,
}

/// A picker box dimension: a fixed size, never content-derived (a content-hugging
/// picker looks ragged). Resolved against the editor viewport at projection time.
/// `Cells` is an absolute column/row count; `Frac` is a fraction `(0, 1]` of the
/// relevant viewport dimension — the CSS `vw`/`vh` analogue (`"80vw"` → `0.8`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuExtent {
    Cells(u16),
    Frac(f32),
}

impl MenuExtent {
    /// Resolve to a concrete cell count against a `viewport` dimension (columns or
    /// rows). A fraction rounds to the nearest cell, floored at 1.
    pub fn resolve(self, viewport: usize) -> usize {
        match self {
            MenuExtent::Cells(c) => c as usize,
            MenuExtent::Frac(f) => ((viewport as f32) * f).round().max(1.0) as usize,
        }
    }
}

/// One candidate row: its display `label` and the **opaque source key** that
/// identifies it back to the engine. For `nx.ui.select` the key is the choice's
/// 0-based index; for a picker it is the 1-based index into the Lua wrapper's
/// per-generation item array, so the chosen item's arbitrary fields never cross
/// the bridge (only the key does — the wrapper resolves `confirm(items[key])`).
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub key: usize,
}

/// The picker's input-grab query field — a single editable line, modeled on the
/// command line ([`Editor::cmdline`]). `col` is the byte offset of the text
/// cursor within `query`.
#[derive(Clone, Default)]
pub(crate) struct Prompt {
    pub query: String,
    pub col: usize,
}

impl Prompt {
    /// Insert `c` at the text cursor and step past it.
    fn insert(&mut self, c: char) {
        self.query.insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Delete the char before the text cursor (`<BS>`); a no-op at the start.
    /// Returns whether anything changed (so the caller re-queries only on an edit).
    fn backspace(&mut self) -> bool {
        if let Some(prev) = self.prev_boundary() {
            self.query.remove(prev);
            self.col = prev;
            true
        } else {
            false
        }
    }

    /// Delete the char under the text cursor (`<Del>`); a no-op at the end.
    fn delete(&mut self) -> bool {
        if self.col < self.query.len() {
            self.query.remove(self.col);
            true
        } else {
            false
        }
    }

    fn cursor_left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.col = prev;
        }
    }

    fn cursor_right(&mut self) {
        if let Some(c) = self.query[self.col..].chars().next() {
            self.col += c.len_utf8();
        }
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.query[..self.col]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    /// The text cursor as a count of characters before it — the caret's column
    /// within the single-line query, which the client draws the prompt caret at
    /// (the prompt is char-indexed like the menu's match spans).
    fn cursor_chars(&self) -> usize {
        self.query[..self.col].chars().count()
    }
}

/// An open menu. `all_items` holds every candidate (fixed for `select`, growing as
/// a picker source streams); `filtered` indexes into it in ranked/visible order
/// and `cursor` highlights a row **within `filtered`**.
#[derive(Clone)]
pub(crate) struct Menu {
    /// Every candidate (label + source key). For a picker this grows via
    /// [`Editor::menu_push`] as the source streams items in — up to 100k+.
    all_items: Vec<MenuItem>,
    /// The visible order. `None` is **passthrough** — the view *is* `all_items` in
    /// stream order (a dynamic source, or a static one with an empty query); nothing
    /// is materialized, so streaming stays O(1) per item. `Some(idx)` is an active
    /// static query: ranked `all_items` indices, recomputed once per query edit and
    /// extended incrementally as new items stream (never re-ranked per batch).
    filtered: Option<Vec<usize>>,
    /// Matched-char spans (`char` ranges) parallel to `filtered` when it is `Some`;
    /// empty in passthrough.
    match_spans: Vec<Vec<Range<usize>>>,
    /// The highlighted row, as an index into the effective view (`filtered` when
    /// `Some`, else `all_items`).
    cursor: usize,
    placement: MenuPlacement,
    /// The `gg`-pending flag (two-key motion), select-mode only.
    gpending: bool,
    /// The input-grab query field — `Some` for a picker, `None` for `select`.
    prompt: Option<Prompt>,
    /// Where the picker prompt sits relative to the list ([`PromptPos`]). Only
    /// meaningful when `prompt` is `Some`; ignored for a promptless `select`.
    prompt_pos: PromptPos,
    /// A dynamic source forwards the query and bypasses the local matcher.
    dynamic: bool,
    /// Bumped on every query edit; the staleness token threaded to the server so a
    /// push from a superseded source run is dropped.
    generation: u64,
    /// The picker box's fixed width / height (`Editor` placement only); `None`
    /// falls back to the picker default. Never content-derived. See [`MenuExtent`].
    width: Option<MenuExtent>,
    height: Option<MenuExtent>,
    /// The generation the currently-displayed `all_items` belong to. A dynamic
    /// query edit bumps `generation` but does **not** clear the list — the old
    /// results stay until the new search's first result (or completion) arrives;
    /// [`Editor::menu_push`] / [`Editor::menu_finish`] swap atomically when
    /// `items_gen` falls behind, so the list never flashes empty while typing.
    items_gen: u64,
}

impl Menu {
    /// The number of rows in the effective view.
    fn view_len(&self) -> usize {
        self.filtered
            .as_ref()
            .map_or(self.all_items.len(), |f| f.len())
    }

    /// The `all_items` index displayed at view row `i` (caller ensures `i` is in
    /// range). O(1) — passthrough is the identity, a query maps through `filtered`.
    fn item_at(&self, i: usize) -> usize {
        self.filtered.as_ref().map_or(i, |f| f[i])
    }

    /// Recompute the ranked view from scratch against the current query (a static
    /// query edit). An empty query drops to passthrough (`None`) — no per-item work.
    fn refilter(&mut self) {
        let query = self.prompt.as_ref().map_or("", |p| p.query.as_str());
        if query.is_empty() {
            self.filtered = None;
            self.match_spans.clear();
        } else {
            let candidates: Vec<&str> = self.all_items.iter().map(|i| i.label.as_str()).collect();
            let ranked = crate::fuzzy::rank(query, &candidates);
            let mut idx = Vec::with_capacity(ranked.len());
            let mut spans = Vec::with_capacity(ranked.len());
            for (i, s) in ranked {
                idx.push(i);
                spans.push(s);
            }
            self.filtered = Some(idx);
            self.match_spans = spans;
        }
        self.clamp_cursor();
    }

    /// Fold the newly-appended `all_items[new_start..]` into the view **without**
    /// re-ranking everything — O(batch), the key to streaming 100k+ candidates.
    /// Passthrough needs nothing (the view *is* `all_items`); an active query matches
    /// only the new items and appends the matches.
    fn extend_view(&mut self, new_start: usize) {
        if self.filtered.is_none() {
            self.clamp_cursor();
            return;
        }
        let query = self
            .prompt
            .as_ref()
            .map_or(String::new(), |p| p.query.clone());
        let candidates: Vec<&str> = self.all_items[new_start..]
            .iter()
            .map(|i| i.label.as_str())
            .collect();
        let ranked = crate::fuzzy::rank(&query, &candidates);
        let filtered = self.filtered.as_mut().unwrap();
        for (i, spans) in ranked {
            filtered.push(new_start + i);
            self.match_spans.push(spans);
        }
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        let len = self.view_len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }
}

impl Editor {
    /// Open a promptless floating choice list (`nx.ui.select`): `items` are the
    /// display labels, `cursor` the initially-highlighted row (clamped). Grabs
    /// input until the user confirms (`<CR>`) or cancels (`<Esc>` / `q`); the
    /// outcome lands in [`Editor::menu_results`]. The list must be non-empty.
    pub fn open_menu(&mut self, items: Vec<String>, placement: MenuPlacement, cursor: usize) {
        let all_items: Vec<MenuItem> = items
            .into_iter()
            .enumerate()
            .map(|(key, label)| MenuItem { label, key })
            .collect();
        let last = all_items.len().saturating_sub(1);
        let mut menu = Menu {
            all_items,
            filtered: None,
            match_spans: Vec::new(),
            cursor: cursor.min(last),
            placement,
            gpending: false,
            prompt: None,
            prompt_pos: PromptPos::default(),
            dynamic: false,
            generation: 0,
            items_gen: 0,
            width: None,
            height: None,
        };
        menu.refilter();
        self.menu = Some(menu);
    }

    /// Open a fuzzy picker (`nx.picker`): a centered float with a prompt that grabs
    /// input. Starts empty — the source streams candidates in via
    /// [`Editor::menu_push`]. `dynamic` selects forward-the-query (live grep) over
    /// local fuzzy matching. `width` / `height` fix the box size ([`MenuExtent`],
    /// `None` ⇒ the picker default) — never content-derived. The server invokes the
    /// source's initial run after opening (query `""`, generation `0`).
    pub fn open_picker(
        &mut self,
        placement: MenuPlacement,
        dynamic: bool,
        width: Option<MenuExtent>,
        height: Option<MenuExtent>,
        prompt_pos: PromptPos,
    ) {
        self.menu = Some(Menu {
            all_items: Vec::new(),
            filtered: None,
            match_spans: Vec::new(),
            cursor: 0,
            placement,
            gpending: false,
            prompt: Some(Prompt::default()),
            prompt_pos,
            dynamic,
            generation: 0,
            items_gen: 0,
            width,
            height,
        });
    }

    /// The open menu's current query generation — the token the server stamps onto
    /// source runs and pushes, so a stale push is dropped. `0` when no menu (or a
    /// promptless `select`) is open.
    pub fn menu_generation(&self) -> u64 {
        self.menu.as_ref().map_or(0, |m| m.generation)
    }

    /// Feed streamed candidates of generation `gen` into the open picker. When
    /// `gen` is **newer** than the displayed items (`gen > items_gen`) this is the
    /// new query's first batch: the stale results are atomically *replaced* (not
    /// cleared on the keystroke), so the list never flashes empty while a debounced
    /// search is in flight. Same-generation batches append. A push into a closed
    /// menu is a silent no-op (a late push from a killed job).
    pub fn menu_push(&mut self, items: Vec<MenuItem>, gen: u64) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if gen > menu.items_gen {
            // First result of a newer query — swap the old results out now.
            menu.all_items.clear();
            menu.filtered = None;
            menu.match_spans.clear();
            menu.items_gen = gen;
            menu.cursor = 0;
        }
        let new_start = menu.all_items.len();
        menu.all_items.extend(items);
        // Incorporate just the new items (O(batch)) — passthrough needs nothing, an
        // active static query matches only the appended slice. Never re-ranks the
        // whole (100k+) list per streamed batch.
        menu.extend_view(new_start);
    }

    /// Mark generation `gen`'s search **complete** (the source called `done()`). If
    /// no result of `gen` ever arrived (`gen > items_gen`), the new query matched
    /// nothing: clear the now-confirmed-empty list. If results did arrive
    /// (`items_gen == gen`) this is a no-op — they stay. A no-op on a closed menu.
    pub fn menu_finish(&mut self, gen: u64) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if gen > menu.items_gen {
            menu.all_items.clear();
            menu.filtered = None;
            menu.match_spans.clear();
            menu.items_gen = gen;
            menu.cursor = 0;
        }
    }

    /// Close the menu without recording a choice — the caller has already recorded
    /// the outcome, or is force-closing. A no-op when no menu is open.
    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Whether a menu is currently open (and grabbing input).
    pub fn menu_is_open(&self) -> bool {
        self.menu.is_some()
    }

    /// Handle a keystroke while the menu has focus. `<CR>` confirms the highlighted
    /// row (pushing its source key onto [`Editor::menu_results`]); `<Esc>` / `q`
    /// (picker: `<Esc>` only) cancels. With a prompt, printable keys edit the query
    /// and never touch the document; navigation is `<C-n>`/`<C-p>`/arrows. Without
    /// one, the list is driven `j`/`k`/`gg`/`G`.
    pub(crate) fn handle_menu(&mut self, key: Key) {
        self.message.clear();

        // Confirm: resolve the highlighted filtered row to its source key.
        if key.code == KeyCode::Enter {
            let chosen = self.menu.as_ref().and_then(|m| {
                (m.cursor < m.view_len()).then(|| m.all_items[m.item_at(m.cursor)].key)
            });
            // A picker with no matches under the current query confirms nothing —
            // stay open. `select` always has a row, so this only no-ops the picker.
            if let Some(key) = chosen {
                self.menu_results.push(Some(key));
                self.close_menu();
            }
            return;
        }

        let has_prompt = self.menu.as_ref().is_some_and(|m| m.prompt.is_some());

        // Cancel: `<Esc>` everywhere; bare `q` only in promptless `select` (in a
        // picker `q` is a literal query character).
        if key.code == KeyCode::Esc || (!has_prompt && matches!(key.as_char(), Some('q'))) {
            self.menu_results.push(None);
            self.close_menu();
            return;
        }

        if has_prompt {
            self.handle_picker_key(key);
        } else {
            self.handle_select_key(key);
        }
    }

    /// Picker-mode keys: navigation drives the list, everything printable edits the
    /// query. A query edit re-ranks locally (static) or forwards to the source
    /// under a bumped generation (dynamic).
    fn handle_picker_key(&mut self, key: Key) {
        // Mutate the menu inside a block so its borrow ends before we may re-borrow
        // `self` to emit a query-changed signal.
        let query_changed = {
            let Some(menu) = self.menu.as_mut() else {
                return;
            };
            let last = menu.view_len().saturating_sub(1);
            let mut query_changed = false;

            match key.code {
                // List navigation (does not touch the query).
                KeyCode::Down => menu.cursor = (menu.cursor + 1).min(last),
                KeyCode::Up => menu.cursor = menu.cursor.saturating_sub(1),
                KeyCode::Char('n') if key.ctrl => menu.cursor = (menu.cursor + 1).min(last),
                KeyCode::Char('p') if key.ctrl => menu.cursor = menu.cursor.saturating_sub(1),
                // Query editing.
                KeyCode::Backspace => query_changed = menu.prompt.as_mut().unwrap().backspace(),
                KeyCode::Delete => query_changed = menu.prompt.as_mut().unwrap().delete(),
                KeyCode::Left => menu.prompt.as_mut().unwrap().cursor_left(),
                KeyCode::Right => menu.prompt.as_mut().unwrap().cursor_right(),
                KeyCode::Home => menu.prompt.as_mut().unwrap().col = 0,
                KeyCode::End => {
                    let p = menu.prompt.as_mut().unwrap();
                    p.col = p.query.len();
                }
                KeyCode::Char(c) if !key.ctrl && !key.alt => {
                    menu.prompt.as_mut().unwrap().insert(c);
                    query_changed = true;
                }
                _ => {}
            }
            query_changed
        };

        if query_changed {
            self.on_query_changed();
        }
    }

    /// React to a picker query edit: a dynamic source bumps the generation and
    /// emits the new `(generation, query)` onto [`Editor::picker_query_changes`]
    /// for the server to re-run the source; a static source just re-ranks locally.
    /// A dynamic edit **keeps the current results displayed** — they are swapped out
    /// only when the new search's first result (or its completion) arrives
    /// ([`Editor::menu_push`] / [`Editor::menu_finish`]), so the list never flashes
    /// empty while a debounced search runs.
    fn on_query_changed(&mut self) {
        let signal = {
            let Some(menu) = self.menu.as_mut() else {
                return;
            };
            if menu.dynamic {
                menu.generation += 1;
                let gen = menu.generation;
                let query = menu
                    .prompt
                    .as_ref()
                    .map_or(String::new(), |p| p.query.clone());
                Some((gen, query))
            } else {
                menu.refilter();
                None
            }
        };
        if let Some(sig) = signal {
            self.picker_query_changes.push(sig);
        }
    }

    /// Promptless `select`-mode keys: vim-style list navigation, no query.
    fn handle_select_key(&mut self, key: Key) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        let last = menu.view_len().saturating_sub(1);

        // `gg` is two keys; the first `g` arms `gpending`.
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

    /// Project the open menu's **metadata** into [`MenuView`] — the highlighted row,
    /// the total visible count, the optional query line, placement, and size. The
    /// rows themselves are fetched windowed via [`Editor::menu_rows`] so a 100k-item
    /// picker never clones its whole list into a frame. `None` when closed.
    pub(crate) fn menu_view(&self) -> Option<MenuView> {
        self.menu.as_ref().map(|m| MenuView {
            selected: m.cursor,
            total: m.view_len(),
            placement: m.placement,
            query: m.prompt.as_ref().map(|p| p.query.clone()),
            query_cursor: m.prompt.as_ref().map_or(0, |p| p.cursor_chars()),
            prompt_pos: m.prompt_pos,
            width: m.width,
            height: m.height,
        })
    }

    /// The visible window of rows `[start, start+count)` of the open menu: each
    /// row's label and its matched-character spans (empty in passthrough). Clones
    /// only the requested window — O(count), independent of the list size. Empty
    /// when closed or out of range.
    pub fn menu_rows(&self, start: usize, count: usize) -> Vec<(String, Vec<Range<usize>>)> {
        let Some(m) = self.menu.as_ref() else {
            return Vec::new();
        };
        let end = start.saturating_add(count).min(m.view_len());
        (start..end)
            .map(|i| {
                let label = m.all_items[m.item_at(i)].label.clone();
                let spans = if m.filtered.is_some() {
                    m.match_spans[i].clone()
                } else {
                    Vec::new()
                };
                (label, spans)
            })
            .collect()
    }
}
