//! The unified floating selectable-list widget — `nx.ui.select` (a promptless
//! choice menu) and `nx.picker` (a fuzzy finder with a prompt) both render through
//! it (see `docs/specs/2026-06-14-nx-ui-float-widget.md`). Like the bottom
//! [`Panel`] it grabs every keystroke while open, but it floats over the text —
//! anchored under the cursor or centered over the editor — and resolves to a
//! single choice.
//!
//! Two orthogonal capabilities make it one shape instead of three:
//!
//! - **prompt** (`Some` ⇒ picker, `None` ⇒ select). Both grab input through their
//!   own keymap bucket (`picker` / `select`) — every nameable key is a default map
//!   (`apply_picker_action` / `apply_select_action`), rebindable like any mode. With
//!   a prompt the one residual key is a printable char editing the query (it **never
//!   reaches the document**); a promptless list has no query, so an unmapped key is
//!   inert.
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
use crate::mode::KeyContext;
use crate::view::MenuView;

/// Where the menu floats. `Cursor` anchors it under the cursor (the
/// `nx.ui.select` / completion shape); `Editor` centers it over the editor (the
/// picker shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuPlacement {
    Cursor,
    Editor,
}

/// Which orchestration drives this `Menu`. The widget is one shape; the kind
/// decides only two things core cares about: whether the menu **grabs input**
/// (`Select` / `Picker` do — every keystroke navigates the list or edits the
/// prompt; `Complete` does **not** — the buffer is the query, so typing flows on
/// to the document and only the engine's control keys are intercepted while it is
/// open), and how `<CR>`-equivalent confirmation resolves (a `Complete` row
/// carries its own [`MenuItem::insert`] text and is applied natively, with no Lua
/// round-trip — the others push a key onto [`Editor::menu_results`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuKind {
    Select,
    Picker,
    Complete,
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
///
/// `preview` is the declarative target the **server** renders into the preview
/// pane (Phase 3) — `None` for a preview-less picker / `select`. Only the target
/// (path + optional location) crosses the bridge, never the file's contents: the
/// server reads and renders it natively, so no Lua runs as the selection moves.
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub key: usize,
    pub preview: Option<PreviewTarget>,
    /// The text a completion row inserts when accepted (`Complete` menus only).
    /// `None` ⇒ use `label`. `Select` / picker rows leave this `None` — they
    /// round-trip the opaque `key` to Lua, which applies the choice itself.
    pub insert: Option<String>,
    /// Source priority for the merged completion view (`Complete` menus only): the
    /// effective order is **priority descending, then fuzzy score** — so an `lsp`
    /// row (high priority) outranks a `buffer` word with the same match quality.
    /// `0` for `select` / picker rows (a single source, no merge).
    pub priority: i32,
    /// Whether accepting this row is **delegated to the source** rather than applied
    /// natively (`Complete` menus only). `false` ⇒ core replaces `[anchor..cursor)`
    /// with `insert` itself (the `buffer` source). `true` ⇒ core records the row's
    /// `key` on [`Editor::complete_accept_request`] and the server applies the edit
    /// (the `lsp` source's `textEdit` + `additionalTextEdits`), which core can't —
    /// it is LSP/encoding-agnostic.
    pub source_accept: bool,
    /// **Inline** documentation for the docs sidebar (`Complete` menus only): the
    /// markdown/plain `doc` a plugin async source attached to this candidate
    /// (`nx.complete.source` → `push { text, insert, doc }`). The server renders it
    /// beside the popup for the selected row directly, no source cache — unlike the
    /// `lsp` source, whose docs the server holds itself (`source_accept` rows leave
    /// this `None`). `None` for `buffer` / `select` / picker rows.
    pub doc: Option<String>,
    /// A **lazy-docs resolve handle** for a plugin async row (`Complete` menus only):
    /// the opaque id the Lua source side maps back to `(source.resolve, item)`. Set
    /// when a row has a `resolve` callback but no inline `doc`; the server asks Lua to
    /// resolve it (`nx._complete_resolve`) once the row is selected and caches the
    /// reply for the sidebar. `None` for an inline-doc / `buffer` / `lsp` / `select`
    /// row — there's nothing to resolve. Phase 4-E.
    pub resolve: Option<u64>,
}

/// The outcome of accepting a completion row, returned by
/// [`Editor::complete_take_accept`]. Either core applies it natively
/// (`source_accept = false`: replace `[anchor..cursor)` with `insert`) or the server
/// applies it from the row's `key` (`source_accept = true`: the `lsp` source's edit).
pub(crate) struct CompleteAcceptance {
    pub anchor: usize,
    pub insert: String,
    pub key: usize,
    pub source_accept: bool,
}

/// What the picker's preview pane should show for a candidate. The source's
/// declared `preview` kind decides the shape: `"file"` sets `loc = None` (render
/// the file's head); `"location"` sets `loc = Some((row, col))` (scroll to and
/// range-highlight the match). Both 0-based. The server resolves `path` against
/// its host FS and renders the content — core never reads the file.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewTarget {
    pub path: String,
    pub loc: Option<(usize, usize)>,
}

/// A one-shot request to scroll the picker's preview pane, emitted by core when the
/// user presses `<C-d>`/`<C-u>` (half page) or `<C-f>`/`<C-b>` (full page). Core
/// can't resolve the line delta — the pane height and the file length live on the
/// server — so it only names the *gesture*; the server maintains the actual scroll
/// offset (clamped to the file, reset when the selection moves to a new target).
/// Consumed once per keystroke, like the viewport [`ScrollAnim`](crate::view::ScrollAnim)
/// gesture: it lives only in the keystroke's frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewScroll {
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
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
    /// Which orchestration owns this menu — decides input-grab and confirm
    /// resolution. See [`MenuKind`].
    kind: MenuKind,
    /// Byte offset in the buffer where the completed prefix starts (`Complete`
    /// menus only; `0` and unused otherwise). Accepting a row replaces
    /// `[anchor .. cursor)` with the row's insert text.
    anchor: usize,
    /// Display width (screen columns) of the completed prefix — the distance the
    /// float is shifted **left** of the cursor so the list anchors under the word
    /// start, not the caret (`Complete` menus only; `0` otherwise). See
    /// [`MenuView::anchor_offset`].
    anchor_width: usize,
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
    /// Whether `cursor` is an **active** selection the client highlights. Always
    /// `true` for a `select` / picker (they always have a highlighted row). For the
    /// completion popup it starts `false` (noselect — nothing highlighted, `<CR>`
    /// makes a newline) and flips `true` on the first navigation.
    selected_active: bool,
    placement: MenuPlacement,
    /// The input-grab query field — `Some` for a picker, `None` for `select`.
    prompt: Option<Prompt>,
    /// The match query for a [`MenuKind::Complete`] menu: the word prefix left of
    /// the cursor. Completion has no input-grab [`Prompt`] (the buffer *is* the
    /// query), so the prefix is stored here and [`Menu::match_query`] reads it in
    /// place of `prompt.query` — so async candidates streaming in via
    /// [`Editor::menu_push`] rank against the prefix exactly as a static picker's do
    /// against its prompt. Empty (`""`) for `select` / picker, leaving their match
    /// behavior unchanged.
    complete_prefix: String,
    /// Where the picker prompt sits relative to the list ([`PromptPos`]). Only
    /// meaningful when `prompt` is `Some`; ignored for a promptless `select`.
    prompt_pos: PromptPos,
    /// A dynamic source forwards the query and bypasses the local matcher.
    dynamic: bool,
    /// Whether this picker carries a preview pane (the source declared a `preview`
    /// kind). When set, [`Editor::menu_view`] exposes the selected row's
    /// [`PreviewTarget`] and the server reserves a preview column. Always `false`
    /// for a promptless `nx.ui.select`.
    preview: bool,
    /// A pending preview-scroll gesture (`<C-d>`/`<C-u>`/`<C-f>`/`<C-b>`), set by
    /// [`Editor::handle_picker_key`] and exposed once via [`Editor::menu_view`]. The
    /// server resolves it against the live pane height and file length, then clears as
    /// [`Editor::view`] drops it after each projection — a one-shot, like the viewport
    /// scroll gesture. Only ever `Some` for a `preview` picker.
    preview_scroll: Option<PreviewScroll>,
    /// Whether this [`MenuKind::Complete`] menu carries a **docs sidebar** — a float
    /// beside the popup rendering the selected item's documentation (the widget-spec
    /// `preview = "markdown"` kind, Phase 4-D). Unlike a picker's file `preview`, the
    /// content is *not* a [`PreviewTarget`] path: the server renders it from its own
    /// LSP item cache keyed by the selected row's `(key, source_accept)` (exposed via
    /// [`Editor::menu_view`] / [`Editor::complete_selected`]), lazily fetching
    /// `completionItem/resolve` docs off the input path. Always `false` for a
    /// `select` / picker.
    docs: bool,
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

    /// The query the matcher ranks against: a picker's input-grab `prompt`, or — for
    /// a [`MenuKind::Complete`] menu, which has no prompt — the stored word prefix.
    /// Empty for a `select` (no prompt, no prefix), keeping it in passthrough.
    fn match_query(&self) -> &str {
        match &self.prompt {
            Some(p) => p.query.as_str(),
            None => self.complete_prefix.as_str(),
        }
    }

    /// Recompute the ranked view from scratch against the current query (a static
    /// query edit). An empty query drops to passthrough (`None`) — no per-item work.
    fn refilter(&mut self) {
        let query = self.match_query();
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
        let query = self.match_query().to_string();
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

    /// Re-order the ranked view by **source priority** (descending), preserving the
    /// fuzzy order within each priority — so a merged completion menu lists an `lsp`
    /// row (priority 100) above a `buffer` word (priority 10) of equal match quality.
    /// A stable sort over the parallel `filtered` / `match_spans`. Called by
    /// [`Editor::menu_push`] for [`MenuKind::Complete`] menus only (a single-source
    /// picker keeps pure fuzzy order). Cheap — completion lists are small.
    fn sort_complete_view(&mut self) {
        if self.filtered.is_none() {
            return;
        }
        let filtered = self.filtered.take().unwrap();
        let spans = std::mem::take(&mut self.match_spans);
        let mut rows: Vec<(usize, Vec<Range<usize>>)> = filtered.into_iter().zip(spans).collect();
        // Descending priority; stable, so equal-priority rows keep their fuzzy order.
        rows.sort_by(|a, b| {
            self.all_items[b.0]
                .priority
                .cmp(&self.all_items[a.0].priority)
        });
        let (filtered, spans): (Vec<usize>, Vec<Vec<Range<usize>>>) = rows.into_iter().unzip();
        self.filtered = Some(filtered);
        self.match_spans = spans;
    }

    /// The [`PreviewTarget`] for the highlighted row, when this picker carries a
    /// preview pane and that row declares one. `None` for a `select` / preview-less
    /// picker, an empty view, or a row whose source supplied no `path` (e.g. an
    /// unnamed buffer). The server reads + renders the target; core never does.
    fn selected_preview(&self) -> Option<&PreviewTarget> {
        if !self.preview || self.cursor >= self.view_len() {
            return None;
        }
        self.all_items[self.item_at(self.cursor)].preview.as_ref()
    }

    /// The highlighted item, when a row is **actively** selected and in range.
    /// `None` for noselect (a completion popup before the first navigation) or an
    /// empty view — so the docs sidebar stays hidden until a row is chosen. Drives
    /// both the [`MenuView`] docs-selection fields and [`Editor::complete_selected`].
    fn selected_item(&self) -> Option<&MenuItem> {
        if !self.selected_active || self.cursor >= self.view_len() {
            return None;
        }
        Some(&self.all_items[self.item_at(self.cursor)])
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
            .map(|(key, label)| MenuItem {
                label,
                key,
                preview: None,
                insert: None,
                priority: 0,
                source_accept: false,
                doc: None,
                resolve: None,
            })
            .collect();
        let last = all_items.len().saturating_sub(1);
        let mut menu = Menu {
            kind: MenuKind::Select,
            anchor_width: 0,
            anchor: 0,
            all_items,
            filtered: None,
            match_spans: Vec::new(),
            cursor: cursor.min(last),
            selected_active: true,
            placement,
            prompt: None,
            complete_prefix: String::new(),
            prompt_pos: PromptPos::default(),
            dynamic: false,
            preview: false,
            preview_scroll: None,
            docs: false,
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
        preview: bool,
        width: Option<MenuExtent>,
        height: Option<MenuExtent>,
        prompt_pos: PromptPos,
    ) {
        self.menu = Some(Menu {
            kind: MenuKind::Picker,
            anchor: 0,
            anchor_width: 0,
            all_items: Vec::new(),
            filtered: None,
            match_spans: Vec::new(),
            cursor: 0,
            selected_active: true,
            placement,
            prompt: Some(Prompt::default()),
            complete_prefix: String::new(),
            prompt_pos,
            dynamic,
            preview,
            preview_scroll: None,
            docs: false,
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
        // A completion menu merges multiple sources: re-order the (small) view so a
        // higher-priority source's rows lead, fuzzy order preserved within a source.
        if menu.kind == MenuKind::Complete {
            menu.sort_complete_view();
        }
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

    /// The open menu's [`MenuKind`], or `None` when no menu is open.
    pub(crate) fn menu_kind(&self) -> Option<MenuKind> {
        self.menu.as_ref().map(|m| m.kind)
    }

    /// Whether the open menu **grabs all input** (`Select` / `Picker`) — the
    /// signal the input dispatch uses to route every key to [`handle_menu`]. A
    /// `Complete` menu does not: it floats over the text while typing flows on to
    /// the document, so the dispatch lets the key through to `handle_insert`.
    ///
    /// [`handle_menu`]: Editor::handle_menu
    pub(crate) fn menu_grabs_input(&self) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|m| m.kind != MenuKind::Complete)
    }

    /// Open or refresh the completion popup at generation `gen`: the synchronous
    /// `buffer`-source `candidates` are fuzzy-ranked against `prefix` and become a
    /// [`MenuKind::Complete`] menu anchored at `anchor` (the buffer byte offset where
    /// the prefix begins). Each row's `insert` text is its label (the `buffer` source
    /// completes to the whole word).
    ///
    /// `keep_open` decides the no-match case: a buffer-only config (`false`) closes
    /// the popup when nothing matched (the 4-A behavior); a config with an **async**
    /// source (`true`) keeps an empty popup open so the streamed candidates have a
    /// menu to land in ([`Editor::menu_push`] appends them at `gen`). `gen` is stamped
    /// as both the live generation and the displayed `items_gen`, so a same-`gen`
    /// async push *appends* and a stale push (an earlier prefix) is dropped.
    ///
    /// `preselect` highlights the first row up front (an explicit manual trigger,
    /// vim-like); auto-typing passes `false` (noselect — nothing highlighted until the
    /// user navigates, so `<CR>` stays a newline).
    #[allow(clippy::too_many_arguments)] // one focused builder; bundling these
                                         // orthogonal completion knobs into a struct would only add indirection.
    pub(crate) fn set_complete_menu(
        &mut self,
        anchor: usize,
        prefix: &str,
        candidates: Vec<String>,
        preselect: bool,
        gen: u64,
        keep_open: bool,
        priority: i32,
    ) {
        let refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
        let ranked = crate::fuzzy::rank(prefix, &refs);
        if ranked.is_empty() && !keep_open {
            self.close_completion();
            return;
        }
        let mut all_items = Vec::with_capacity(ranked.len());
        let mut filtered = Vec::with_capacity(ranked.len());
        let mut match_spans = Vec::with_capacity(ranked.len());
        for (key, (idx, spans)) in ranked.into_iter().enumerate() {
            let label = candidates[idx].clone();
            all_items.push(MenuItem {
                insert: Some(label.clone()),
                label,
                key,
                preview: None,
                priority,
                // The `buffer` source inserts its word natively (no source edit).
                source_accept: false,
                // `buffer` words carry no docs; async source rows (with a `doc` or a
                // `resolve` handle) are appended later via `menu_push`.
                doc: None,
                resolve: None,
            });
            filtered.push(key);
            match_spans.push(spans);
        }
        self.menu = Some(Menu {
            kind: MenuKind::Complete,
            anchor,
            anchor_width: crate::unicode::display_width(prefix),
            all_items,
            // `Some` (an active query = the prefix) even when empty, so a later
            // `menu_push` matches the streamed async batch against the prefix via
            // `extend_view` rather than dropping to passthrough.
            filtered: Some(filtered),
            match_spans,
            cursor: 0,
            // Noselect by default: nothing is highlighted until the user navigates,
            // so an auto-opened popup never hijacks `<CR>` (it stays a newline). A
            // manual trigger passes `preselect = true` to highlight the first row.
            selected_active: preselect,
            placement: MenuPlacement::Cursor,
            prompt: None,
            complete_prefix: prefix.to_string(),
            prompt_pos: PromptPos::default(),
            dynamic: false,
            preview: false,
            preview_scroll: None,
            // The docs sidebar follows the engine config; the server fills it from
            // its LSP item cache for the selected row (a `buffer` row has no docs).
            docs: self.complete_config.docs,
            generation: gen,
            items_gen: gen,
            width: None,
            height: None,
        });
    }

    /// Whether the open menu's effective view has no rows. Used by
    /// [`Editor::complete_finish`] to close a confirmed-empty completion popup once
    /// an async source has streamed nothing. `false` when no menu is open.
    pub(crate) fn menu_view_is_empty(&self) -> bool {
        self.menu.as_ref().is_some_and(|m| m.view_len() == 0)
    }

    /// Move the completion selection (wrapping), `<C-n>` / `<C-p>`-style. The popup
    /// opens with **no** active selection (noselect, like nvim-cmp): the first
    /// `next` highlights row 0, the first `prev` the last row, and only then does
    /// `<CR>` accept (otherwise it's a plain newline). A no-op unless a completion
    /// menu is open.
    pub(crate) fn complete_select_next(&mut self) {
        if let Some(m) = self.completion_menu_mut() {
            let len = m.view_len();
            if len == 0 {
            } else if !m.selected_active {
                m.selected_active = true;
                m.cursor = 0;
            } else {
                m.cursor = (m.cursor + 1) % len;
            }
        }
    }

    pub(crate) fn complete_select_prev(&mut self) {
        if let Some(m) = self.completion_menu_mut() {
            let len = m.view_len();
            if len == 0 {
            } else if !m.selected_active {
                m.selected_active = true;
                m.cursor = len - 1;
            } else {
                m.cursor = (m.cursor + len - 1) % len;
            }
        }
    }

    /// Highlight completion row `idx` (clamped) and activate the selection — the
    /// mouse hover/click counterpart to the relative `<C-n>`/`<C-p>`. A no-op unless a
    /// completion menu is open.
    pub fn complete_select_index(&mut self, idx: usize) {
        if let Some(m) = self.completion_menu_mut() {
            let len = m.view_len();
            if len > 0 {
                m.selected_active = true;
                m.cursor = idx.min(len - 1);
            }
        }
    }

    /// The actively-selected completion's `(anchor, insert_text)`, closing the
    /// menu. `None` when no completion menu is open **or nothing is selected yet**
    /// (the popup just auto-opened) — the caller then leaves the key to its normal
    /// insert handling, so `<CR>` makes a newline rather than accepting a row the
    /// user never picked. The caller applies the edit (replacing `[anchor .. cursor)`).
    pub(crate) fn complete_take_accept(&mut self) -> Option<CompleteAcceptance> {
        let m = self.completion_menu_mut()?;
        if !m.selected_active {
            return None;
        }
        let row = m.all_items.get(m.item_at(m.cursor))?;
        let acc = CompleteAcceptance {
            anchor: m.anchor,
            insert: row.insert.clone().unwrap_or_else(|| row.label.clone()),
            key: row.key,
            source_accept: row.source_accept,
        };
        self.menu = None;
        Some(acc)
    }

    /// Close the popup **only if it is a completion menu** — leaves an open
    /// `select` / picker untouched. A no-op when nothing (or a non-completion
    /// menu) is open.
    pub(crate) fn close_completion(&mut self) {
        if self.menu_kind() == Some(MenuKind::Complete) {
            self.menu = None;
        }
    }

    /// `&mut` to the open menu iff it is a completion menu.
    fn completion_menu_mut(&mut self) -> Option<&mut Menu> {
        self.menu.as_mut().filter(|m| m.kind == MenuKind::Complete)
    }

    /// Which key context owns input — [`KeyContext::Picker`] while a prompted picker
    /// grabs input, [`KeyContext::Select`] while a promptless `nx.ui.select` list
    /// does (each routes keys through its own keymap bucket), otherwise
    /// [`KeyContext::Editing`] (the buffer, or a non-grabbing completion menu whose
    /// typing flows on to the document).
    pub fn key_context(&self) -> KeyContext {
        match self.menu_kind() {
            Some(MenuKind::Picker) => KeyContext::Picker,
            Some(MenuKind::Select) => KeyContext::Select,
            _ => KeyContext::Editing,
        }
    }

    /// Apply a named `select` action, dispatched by a `select`-bucket keymap (the
    /// default maps in `prelude/ui.lua`, or a user override). The rebindable
    /// operations of the promptless list: `next`/`prev` move the highlight,
    /// `first`/`last` jump to the ends, `confirm` resolves the highlighted row
    /// (pushing its source key onto [`Editor::menu_results`]) and `cancel` dismisses
    /// it. An unknown name fails loud per the no-silent-stub rule.
    pub fn apply_select_action(&mut self, action: &str) -> Result<(), String> {
        self.message.clear();
        match action {
            "confirm" => {
                let chosen = self.menu.as_ref().and_then(|m| {
                    (m.cursor < m.view_len()).then(|| m.all_items[m.item_at(m.cursor)].key)
                });
                if let Some(key) = chosen {
                    self.menu_results.push(Some(key));
                    self.close_menu();
                }
                return Ok(());
            }
            "cancel" => {
                self.menu_results.push(None);
                self.close_menu();
                return Ok(());
            }
            _ => {}
        }

        let Some(menu) = self.menu.as_mut() else {
            return Ok(());
        };
        let last = menu.view_len().saturating_sub(1);
        match action {
            "next" => menu.cursor = (menu.cursor + 1).min(last),
            "prev" => menu.cursor = menu.cursor.saturating_sub(1),
            "first" => menu.cursor = 0,
            "last" => menu.cursor = last,
            other => return Err(format!("unknown select action {other:?}")),
        }
        Ok(())
    }

    /// The picker's text fallthrough: an unmapped printable key inserts into the
    /// query. Every *nameable* picker key (navigation, confirm, cancel, preview
    /// scroll, the query-edit operations) is a `picker`-bucket default map that fires
    /// [`apply_picker_action`](Self::apply_picker_action) through the keymap engine,
    /// so the only key that reaches here is one no map claimed — by default that is a
    /// printable character (you cannot enumerate every char as a map), which edits
    /// the query. A non-printable unmapped key is inert.
    pub(crate) fn handle_picker_text(&mut self, key: Key) {
        let KeyCode::Char(c) = key.code else { return };
        if key.ctrl || key.alt {
            return;
        }
        self.message.clear();
        {
            let Some(menu) = self.menu.as_mut() else {
                return;
            };
            menu.prompt.as_mut().unwrap().insert(c);
        }
        self.on_query_changed();
    }

    /// Apply a named picker action, dispatched by a `picker`-bucket keymap (the
    /// default maps registered in `prelude/picker.lua`, or a user override). The name
    /// space is the picker's rebindable operations: `next`/`prev` move the selection,
    /// `confirm`/`cancel` resolve it, `preview_half_down`/`_half_up`/`_page_down`/
    /// `_page_up` scroll the preview pane (a no-op without one), and `backspace`/
    /// `delete`/`left`/`right`/`home`/`end` edit the query. An unknown name fails loud
    /// (returns `Err`) rather than silently no-op'ing, per the no-silent-stub rule.
    pub fn apply_picker_action(&mut self, action: &str) -> Result<(), String> {
        self.message.clear();
        // Confirm / cancel don't fit the shared `menu.as_mut()` nav block (they push a
        // result + close), so handle them first.
        match action {
            "confirm" => {
                let chosen = self.menu.as_ref().and_then(|m| {
                    (m.cursor < m.view_len()).then(|| m.all_items[m.item_at(m.cursor)].key)
                });
                // A picker with no matches under the current query confirms nothing.
                if let Some(key) = chosen {
                    self.menu_results.push(Some(key));
                    self.close_menu();
                }
                return Ok(());
            }
            "cancel" => {
                self.menu_results.push(None);
                self.close_menu();
                return Ok(());
            }
            _ => {}
        }

        // Navigation / preview / query-edit mutate the open menu; the query-edit ones
        // report whether the query changed so we can re-rank after the borrow ends.
        let query_changed = {
            let Some(menu) = self.menu.as_mut() else {
                return Ok(());
            };
            let last = menu.view_len().saturating_sub(1);
            let mut query_changed = false;
            match action {
                "next" => menu.cursor = (menu.cursor + 1).min(last),
                "prev" => menu.cursor = menu.cursor.saturating_sub(1),
                // Preview-pane scrolling, only when a preview is shown; core names the
                // gesture and the server resolves it against the pane height and file.
                "preview_half_down" if menu.preview => {
                    menu.preview_scroll = Some(PreviewScroll::HalfDown)
                }
                "preview_half_up" if menu.preview => {
                    menu.preview_scroll = Some(PreviewScroll::HalfUp)
                }
                "preview_page_down" if menu.preview => {
                    menu.preview_scroll = Some(PreviewScroll::PageDown)
                }
                "preview_page_up" if menu.preview => {
                    menu.preview_scroll = Some(PreviewScroll::PageUp)
                }
                // A preview gesture with no preview pane is a no-op, not an error.
                "preview_half_down" | "preview_half_up" | "preview_page_down"
                | "preview_page_up" => {}
                "backspace" => query_changed = menu.prompt.as_mut().unwrap().backspace(),
                "delete" => query_changed = menu.prompt.as_mut().unwrap().delete(),
                "left" => menu.prompt.as_mut().unwrap().cursor_left(),
                "right" => menu.prompt.as_mut().unwrap().cursor_right(),
                "to_start" => menu.prompt.as_mut().unwrap().col = 0,
                "to_end" => {
                    let p = menu.prompt.as_mut().unwrap();
                    p.col = p.query.len();
                }
                other => return Err(format!("unknown picker action {other:?}")),
            }
            query_changed
        };

        if query_changed {
            self.on_query_changed();
        }
        Ok(())
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

    /// Project the open menu's **metadata** into [`MenuView`] — the highlighted row,
    /// the total visible count, the optional query line, placement, and size. The
    /// rows themselves are fetched windowed via [`Editor::menu_rows`] so a 100k-item
    /// picker never clones its whole list into a frame. `None` when closed.
    /// Drop the one-shot preview-scroll gesture after a frame has consumed it (called
    /// from [`Editor::view`], alongside `pending_scroll`).
    pub(crate) fn clear_preview_scroll(&mut self) {
        if let Some(menu) = self.menu.as_mut() {
            menu.preview_scroll = None;
        }
    }

    pub(crate) fn menu_view(&self) -> Option<MenuView> {
        self.menu.as_ref().map(|m| {
            let sel = m.selected_item();
            MenuView {
                selected: m.cursor,
                total: m.view_len(),
                placement: m.placement,
                query: m.prompt.as_ref().map(|p| p.query.clone()),
                query_cursor: m.prompt.as_ref().map_or(0, |p| p.cursor_chars()),
                prompt_pos: m.prompt_pos,
                has_preview: m.preview,
                preview: m.selected_preview().cloned(),
                preview_scroll: m.preview_scroll,
                width: m.width,
                height: m.height,
                anchor_offset: m.anchor_width,
                completion: m.kind == MenuKind::Complete,
                selected_active: m.selected_active,
                docs: m.docs,
                selected_key: sel.map(|i| i.key),
                selected_source_accept: sel.is_some_and(|i| i.source_accept),
                selected_doc: sel.and_then(|i| i.doc.clone()),
                selected_resolve: sel.and_then(|i| i.resolve),
            }
        })
    }

    /// The actively-highlighted completion row's `(key, source_accept)` — what the
    /// server needs to fetch / `completionItem/resolve` the docs for the selected
    /// item, called off the input path after a navigation key. `None` unless a
    /// [`MenuKind::Complete`] menu is open with an **active** selection (a noselect
    /// popup shows no docs), so the server only resolves a row the user landed on.
    pub fn complete_selected(&self) -> Option<(usize, bool)> {
        let m = self.menu.as_ref()?;
        if m.kind != MenuKind::Complete {
            return None;
        }
        m.selected_item().map(|i| (i.key, i.source_accept))
    }

    /// The actively-highlighted completion row's **lazy-docs resolve handle** — the
    /// id the server passes to `nx._complete_resolve` to fetch a plugin row's docs
    /// off the input path (Phase 4-E). `None` unless a [`MenuKind::Complete`] menu is
    /// open with an active selection whose row carries a `resolve` handle (an
    /// inline-doc / `buffer` / `lsp` row yields `None`).
    pub fn complete_selected_resolve(&self) -> Option<u64> {
        let m = self.menu.as_ref()?;
        if m.kind != MenuKind::Complete {
            return None;
        }
        m.selected_item().and_then(|i| i.resolve)
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
