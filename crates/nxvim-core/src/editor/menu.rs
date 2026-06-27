//! The unified floating selectable-list widget — `nx.ui.select` (a promptless
//! choice menu) and `nx.picker` (a fuzzy finder with a prompt) both render through
//! it (see `docs/specs/2026-06-14-nx-ui-float-widget.md`). It grabs every keystroke
//! while open, floating over the text — anchored under the cursor or centered over
//! the editor — and resolves to a single choice.
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

use std::ops::Range;

use super::*;
use crate::input::{Key, KeyCode};
use crate::mode::KeyContext;
use crate::view::MenuView;

/// How a confirmed picker pick opens — the gesture the user confirmed with. The
/// server reads it off [`Editor::picker_confirm_mode`] and forwards the matching
/// string to `nx._picker_result` → the source's `confirm(item, mode)`. Only the
/// picker uses it (`nx.ui.select` ignores it). `<C-t>`/`<C-x>`/`<C-v>` map to
/// `Tab`/`Split`/`Vsplit`; plain `<CR>` is `Current` (honoring `'switchbuf'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerOpenMode {
    /// `<CR>` — open in the focused window, honoring `'switchbuf'`.
    #[default]
    Current,
    /// `<C-t>` — open in a new tab.
    Tab,
    /// `<C-x>` — open in a horizontal split.
    Split,
    /// `<C-v>` — open in a vertical split.
    Vsplit,
}

impl PickerOpenMode {
    /// The mode string handed to Lua (`nx._picker_result` / `confirm(item, mode)`).
    pub fn as_str(self) -> &'static str {
        match self {
            PickerOpenMode::Current => "current",
            PickerOpenMode::Tab => "tab",
            PickerOpenMode::Split => "split",
            PickerOpenMode::Vsplit => "vsplit",
        }
    }
}

/// The bound on the resume snapshot ([`Editor::snapshot_picker_for_resume`]): a
/// reopened picker (`nx.picker.resume`) shows at most this many rows — a window
/// around the cursor of the picker as it closed. A live-grep result set has no
/// stable order across runs, so resume can't re-run the source; it replays this
/// frozen window instead. Bounding it keeps a closed 100k-row picker from pinning
/// its whole list in memory until the next picker opens.
pub(crate) const RESUME_WINDOW: usize = 1000;

/// Where the float (menu or content float) anchors on the shared placement layer.
/// `Cursor` anchors it under the cursor (the `nx.ui.select` / completion shape);
/// `Editor` centers it over the editor (the picker shape); `Bottom` pins it to the
/// editor's bottom-right corner (the which-key content-float shape — menus never
/// request it); `Cmdline` floats it directly **above the command line**, anchored
/// under the token being completed (the `nx.cmdline_complete` shape).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuPlacement {
    Cursor,
    Editor,
    Bottom,
    Cmdline,
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
    /// Command-line completion (`nx.cmdline_complete`) — the wildmenu sibling of
    /// `Complete`. Like `Complete` it does **not** grab input (the command line
    /// keeps focus, keys keep editing the line) and its rows carry their own
    /// [`MenuItem::insert`] text; unlike it, the accept replaces a token of the
    /// command line, not buffer text, and it floats above the command line
    /// ([`MenuPlacement::Cmdline`]).
    Cmdline,
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

/// One axis of a window's size — the shared geometry primitive used by every
/// surface (picker / select menus, floats, `nx.view`, the panel). A fixed size,
/// never content-derived (a content-hugging picker looks ragged). Resolved against
/// the relevant reference dimension (the editor viewport, almost always) at
/// projection / layout time, so a fractional size **reflows on resize**. `Cells`
/// is an absolute column/row count; `Frac` is a fraction `(0, 1]` of the reference
/// dimension — the CSS `vw`/`vh` analogue (`"80vw"` → `0.8`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Extent {
    Cells(u16),
    Frac(f32),
}

impl Extent {
    /// Resolve to a concrete cell count against a `reference` dimension (the
    /// viewport columns or rows). A fraction rounds to the nearest cell, floored
    /// at 1.
    pub fn resolve(self, reference: usize) -> usize {
        match self {
            Extent::Cells(c) => c as usize,
            Extent::Frac(f) => ((reference as f32) * f).round().max(1.0) as usize,
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
    /// One horizontal step left / right (a `<S-ScrollWheel>` or horizontal wheel over
    /// the pane). The server owns the column magnitude, like the vertical gestures.
    Left,
    Right,
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
    /// falls back to the picker default. Never content-derived. See [`Extent`].
    width: Option<Extent>,
    height: Option<Extent>,
    /// Where an `Editor`-placement picker aligns within the editor area, inset by
    /// `margin`. `None` ⇒ centered (the historical placement). See [`Align`].
    align: Option<Align>,
    margin: Margin,
    /// The generation the currently-displayed `all_items` belong to. A dynamic
    /// query edit bumps `generation` but does **not** clear the list — the old
    /// results stay until the new search's first result (or completion) arrives;
    /// [`Editor::menu_push`] / [`Editor::menu_finish`] swap atomically when
    /// `items_gen` falls behind, so the list never flashes empty while typing.
    items_gen: u64,
    /// Multi-selection: the source keys the user has **marked** (`<Tab>`), in mark
    /// order. Keyed by source key (not view index) so a mark survives query edits and
    /// re-ranking. Empty for `select` / completion (only a picker marks). The picker's
    /// `send_to_loclist` sends these when non-empty, else the whole filtered view.
    marked: Vec<usize>,
    /// An optional title rendered on the picker box's top border
    /// (`nx.picker.open(name, { title = … })`). `None` ⇒ no title. Only a picker
    /// sets it; the wildmenu / completion / select menus leave it `None`.
    title: Option<String>,
    /// Whether `<Tab>` multi-selects (marks) rows (`nx.picker.open{ multiselect }`,
    /// default `true`). `false` makes `toggle_select` a no-op — a single-choice
    /// picker (e.g. the cmdline file completer) where marking makes no sense.
    multiselect: bool,
    /// Whether this picker is captured for `nx.picker.resume()` when it closes
    /// (`nx.picker.source{ resumable = … }`, default `true`). A transient internal
    /// picker — the cmdline file completer — sets `false` so it never overwrites the
    /// resume snapshot of the last user-facing picker. Always `false` for a
    /// `select` / completion / cmdline menu (only a picker resumes).
    resumable: bool,
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

    /// Advance the selection one row, wrapping. A noselect popup (the completion /
    /// cmdline wildmenu just opened) activates the selection on the first call and
    /// highlights row 0; thereafter it cycles forward. A no-op on an empty view.
    /// Shared by the insert-completion popup and the command-line wildmenu.
    fn select_next(&mut self) {
        let len = self.view_len();
        if len == 0 {
        } else if !self.selected_active {
            self.selected_active = true;
            self.cursor = 0;
        } else {
            self.cursor = (self.cursor + 1) % len;
        }
    }

    /// Retreat the selection one row, wrapping. A noselect popup activates the
    /// selection on the first call and highlights the **last** row; thereafter it
    /// cycles backward. A no-op on an empty view.
    fn select_prev(&mut self) {
        let len = self.view_len();
        if len == 0 {
        } else if !self.selected_active {
            self.selected_active = true;
            self.cursor = len - 1;
        } else {
            self.cursor = (self.cursor + len - 1) % len;
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
            // Open noselect, like the completion popup / wildmenu: nothing is
            // highlighted until the user navigates, so `<CR>` on a just-opened menu
            // does nothing rather than confirming a row no one picked. The first
            // navigation activates the highlight (`apply_select_action`).
            selected_active: false,
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
            marked: Vec::new(),
            width: None,
            height: None,
            align: None,
            margin: Margin::default(),
            title: None,
            multiselect: false,
            resumable: false,
        };
        menu.refilter();
        self.menu = Some(menu);
    }

    /// Open a fuzzy picker (`nx.picker`): a centered float with a prompt that grabs
    /// input. The source streams candidates in via [`Editor::menu_push`].
    /// `dynamic` selects forward-the-query (live grep) over local fuzzy matching.
    /// `width` / `height` fix the box size ([`Extent`], `None` ⇒ the picker
    /// default) — never content-derived. `align` / `margin` place the box within
    /// the editor area (`None` align ⇒ centered). `query` pre-fills the prompt
    /// (`nx.picker.open(name, { query = … })`) with the caret at its end, so the
    /// list opens already filtered against it; empty ⇒ the historical empty-prompt
    /// open. The server invokes the source's initial run after opening with this
    /// `query` (generation `0`).
    #[allow(clippy::too_many_arguments)]
    pub fn open_picker(
        &mut self,
        placement: MenuPlacement,
        dynamic: bool,
        preview: bool,
        width: Option<Extent>,
        height: Option<Extent>,
        align: Option<Align>,
        margin: Margin,
        prompt_pos: PromptPos,
        query: &str,
        title: Option<String>,
        multiselect: bool,
        resumable: bool,
    ) {
        let prompt = Prompt {
            col: query.len(),
            query: query.to_string(),
        };
        // A non-empty seed on a STATIC source opens in *filtered* mode (an empty
        // ranked view), so the items the source streams in are matched against the
        // seed as they arrive (`extend_view` is a no-op in passthrough). A DYNAMIC
        // source bypasses the matcher — it filters itself from `ctx.query` (which is
        // seeded too) — so it must stay in passthrough or its own rows would be
        // re-ranked away. An empty seed always stays in passthrough.
        let filtered = (!query.is_empty() && !dynamic).then(Vec::new);
        self.menu = Some(Menu {
            kind: MenuKind::Picker,
            anchor: 0,
            anchor_width: 0,
            all_items: Vec::new(),
            filtered,
            match_spans: Vec::new(),
            cursor: 0,
            selected_active: true,
            placement,
            prompt: Some(prompt),
            complete_prefix: String::new(),
            prompt_pos,
            dynamic,
            preview,
            preview_scroll: None,
            docs: false,
            generation: 0,
            items_gen: 0,
            marked: Vec::new(),
            width,
            height,
            align,
            margin,
            title,
            multiselect,
            resumable,
        });
    }

    /// Open / rebuild the **command-line completion** popup (`nx.cmdline_complete`):
    /// the catalog `candidates` (each `(label, insert, doc)`) are fuzzy-ranked against
    /// `prefix` (the command-name token typed so far) and become a
    /// [`MenuKind::Cmdline`] menu floating above the command line, anchored under the
    /// token. `anchor` is the byte offset of the token in [`Editor::cmdline`] (accept
    /// replaces `[anchor .. cmdline_col)` — Phase 2); `anchor_width` is the display
    /// width of the line before it (the float's column after the `:` prompt). Nothing
    /// matching closes any open popup (the wildmenu just disappears). Opens noselect —
    /// no row highlighted until the user navigates (Phase 2). The server calls this
    /// after resolving an [`Editor::cmdline_complete_request`] against the Lua source.
    pub fn open_cmdline_menu(
        &mut self,
        anchor: usize,
        anchor_width: usize,
        prefix: &str,
        candidates: Vec<(String, String, Option<String>)>,
        docs: bool,
    ) {
        let labels: Vec<&str> = candidates.iter().map(|(l, _, _)| l.as_str()).collect();
        let ranked = crate::fuzzy::rank(prefix, &labels);
        if ranked.is_empty() {
            self.close_cmdline_menu();
            return;
        }
        let mut all_items = Vec::with_capacity(ranked.len());
        let mut filtered = Vec::with_capacity(ranked.len());
        let mut match_spans = Vec::with_capacity(ranked.len());
        for (key, (idx, spans)) in ranked.into_iter().enumerate() {
            let (label, insert, doc) = candidates[idx].clone();
            all_items.push(MenuItem {
                label,
                key,
                preview: None,
                insert: Some(insert),
                priority: 0,
                source_accept: false,
                doc,
                resolve: None,
            });
            filtered.push(key);
            match_spans.push(spans);
        }
        self.menu = Some(Menu {
            kind: MenuKind::Cmdline,
            anchor,
            anchor_width,
            all_items,
            filtered: Some(filtered),
            match_spans,
            cursor: 0,
            // Noselect: nothing highlighted until the user navigates (Phase 2), so
            // `<CR>` keeps executing the typed line until a row is chosen.
            selected_active: false,
            placement: MenuPlacement::Cmdline,
            prompt: None,
            complete_prefix: prefix.to_string(),
            prompt_pos: PromptPos::default(),
            dynamic: false,
            preview: false,
            preview_scroll: None,
            docs,
            generation: 0,
            items_gen: 0,
            marked: Vec::new(),
            width: None,
            height: None,
            align: None,
            margin: Margin::default(),
            title: None,
            multiselect: false,
            resumable: false,
        });
    }

    /// Close the popup **only if it is a command-line completion menu** — leaves an
    /// open `select` / picker / insert-completion menu untouched. A no-op when
    /// nothing (or a non-cmdline menu) is open.
    pub(crate) fn close_cmdline_menu(&mut self) {
        if self.menu_kind() == Some(MenuKind::Cmdline) {
            self.menu = None;
            // The revert snapshot only outlives the menu via an explicit `<Esc>`
            // revert (which takes it before closing); any other close — accept,
            // execute, no-match — drops it.
            self.cmdline_complete_saved = None;
        }
    }

    /// `&mut` to the open menu iff it is a command-line completion menu.
    fn cmdline_menu_mut(&mut self) -> Option<&mut Menu> {
        self.menu.as_mut().filter(|m| m.kind == MenuKind::Cmdline)
    }

    /// Move the command-line wildmenu selection forward (`<Tab>` / `<C-n>` / `<Down>`
    /// while the popup is open). Like the insert-completion popup it opens **noselect**
    /// — the first `next` highlights row 0, the first `prev` the last row — and only
    /// then does `<CR>` accept (until then it runs the typed line unchanged). A no-op
    /// unless a cmdline menu is open.
    pub(crate) fn cmdline_complete_next(&mut self) {
        if let Some(m) = self.cmdline_menu_mut() {
            m.select_next();
        }
        self.cmdline_complete_preview();
    }

    pub(crate) fn cmdline_complete_prev(&mut self) {
        if let Some(m) = self.cmdline_menu_mut() {
            m.select_prev();
        }
        self.cmdline_complete_preview();
    }

    /// Highlight wildmenu row `idx` directly (a mouse click on it) and preview it in
    /// the command line — the mouse twin of
    /// [`cmdline_complete_next`](Self::cmdline_complete_next). A no-op unless a cmdline
    /// menu is open.
    pub(crate) fn cmdline_complete_select_index(&mut self, idx: usize) {
        if let Some(m) = self.cmdline_menu_mut() {
            let len = m.view_len();
            if len > 0 {
                m.selected_active = true;
                m.cursor = idx.min(len - 1);
            }
        }
        self.cmdline_complete_preview();
    }

    /// Whether the open menu is the command-line wildmenu (`nx.cmdline_complete`),
    /// the one the mouse drives in command mode.
    pub(crate) fn cmdline_complete_active(&self) -> bool {
        self.menu_kind() == Some(MenuKind::Cmdline)
    }

    /// The actively-highlighted command-line completion's `(anchor, insert_text)`
    /// **without** closing the menu — the peek twin of
    /// [`cmdline_complete_take_accept`](Self::cmdline_complete_take_accept), used to
    /// preview the selection in the command line as the user cycles. `None` when no
    /// cmdline menu is open or nothing is selected yet (the noselect popup).
    pub(crate) fn cmdline_complete_selected(&self) -> Option<(usize, String)> {
        let m = self.menu.as_ref().filter(|m| m.kind == MenuKind::Cmdline)?;
        if !m.selected_active {
            return None;
        }
        let row = m.all_items.get(m.item_at(m.cursor))?;
        Some((
            m.anchor,
            row.insert.clone().unwrap_or_else(|| row.label.clone()),
        ))
    }

    /// The actively-selected command-line completion's `(anchor, insert_text)`,
    /// closing the menu. `None` when no cmdline menu is open **or nothing is selected
    /// yet** (the popup is noselect until the user navigates) — the caller then runs
    /// the typed line unchanged. The caller rewrites `[anchor .. cmdline_col)` with the
    /// insert text ([`Editor::cmdline_complete_accept`]).
    pub(crate) fn cmdline_complete_take_accept(&mut self) -> Option<(usize, String)> {
        let m = self.cmdline_menu_mut()?;
        if !m.selected_active {
            return None;
        }
        let row = m.all_items.get(m.item_at(m.cursor))?;
        let acc = (
            m.anchor,
            row.insert.clone().unwrap_or_else(|| row.label.clone()),
        );
        self.menu = None;
        Some(acc)
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

    /// The open **command-line** wildmenu's anchor — the byte offset in
    /// [`Editor::cmdline`] where the token it is completing starts. `None` when no
    /// cmdline menu is open. Used by [`cmdline_complete_refresh`](Self::cmdline_complete_refresh)
    /// to tell a same-token narrowing edit from one that moved past the token (a new
    /// token, which must not auto-open a fresh completion).
    pub(crate) fn cmdline_menu_anchor(&self) -> Option<usize> {
        self.menu
            .as_ref()
            .filter(|m| m.kind == MenuKind::Cmdline)
            .map(|m| m.anchor)
    }

    /// Whether the open menu **grabs all input** (`Select` / `Picker`) — the
    /// signal [`key_context`](Self::key_context) reads to route every key through
    /// the menu's own keymap bucket. A `Complete` menu does not: it floats over the
    /// text while typing flows on to the document, so the dispatch lets the key
    /// through to `handle_insert`.
    pub(crate) fn menu_grabs_input(&self) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|m| !matches!(m.kind, MenuKind::Complete | MenuKind::Cmdline))
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
            marked: Vec::new(),
            width: None,
            height: None,
            align: None,
            margin: Margin::default(),
            title: None,
            multiselect: false,
            resumable: false,
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
            m.select_next();
            // A new row's docs start at the top.
            self.complete_docs_scroll = 0;
            self.complete_docs_hscroll = 0;
        }
    }

    pub(crate) fn complete_select_prev(&mut self) {
        if let Some(m) = self.completion_menu_mut() {
            m.select_prev();
            self.complete_docs_scroll = 0;
            self.complete_docs_hscroll = 0;
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
                // A new row's docs start at the top (the mouse hover/click + wheel-list
                // path lands here too, so scrolling the LIST resets the docs sidebar).
                self.complete_docs_scroll = 0;
                self.complete_docs_hscroll = 0;
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

    /// Which key context owns input — the grabbing widget that routes keys through
    /// its own keymap bucket, otherwise [`KeyContext::Editing`] (the buffer, or a
    /// non-grabbing completion menu whose typing flows on to the document). A
    /// grabbing menu ([`Picker`](KeyContext::Picker) / [`Select`](KeyContext::Select))
    /// takes the context. The explorer / `nx.view` / quickfix buffers, and the
    /// read-only scratch / loclist listings, are *not* widgets — they are ordinary
    /// `nomodifiable` buffers whose special keys are buffer-local maps, so they stay
    /// in [`KeyContext::Editing`].
    pub fn key_context(&self) -> KeyContext {
        match self.menu_kind() {
            Some(MenuKind::Picker) => return KeyContext::Picker,
            Some(MenuKind::Select) => return KeyContext::Select,
            _ => {}
        }
        KeyContext::Editing
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
                // Only an *active* selection confirms. A noselect menu (nothing
                // navigated to yet) resolves nothing and stays open — `<CR>` is inert
                // until the user picks a row, like the completion popup / wildmenu.
                let chosen = self.menu.as_ref().and_then(|m| {
                    (m.selected_active && m.cursor < m.view_len())
                        .then(|| m.all_items[m.item_at(m.cursor)].key)
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
        // The first navigation on a noselect menu activates the highlight at the
        // current row rather than moving past it; thereafter `next`/`prev` step it
        // (clamped — the select list doesn't wrap). `first`/`last` always jump. An
        // unknown action fails loud *before* touching the selection state.
        let was_active = menu.selected_active;
        match action {
            "next" if was_active => menu.cursor = (menu.cursor + 1).min(last),
            "prev" if was_active => menu.cursor = menu.cursor.saturating_sub(1),
            "next" | "prev" => {}
            "first" => menu.cursor = 0,
            "last" => menu.cursor = last,
            other => return Err(format!("unknown select action {other:?}")),
        }
        menu.selected_active = true;
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

    /// Snapshot a closing **resumable** picker for `nx.picker.resume()` (`<leader>fr`)
    /// — a frozen [`RESUME_WINDOW`]-row window of the *display* view around the cursor.
    /// A live-grep result set has no stable order across runs, so resume can't re-run
    /// the source; it replays this exact window instead. The snapshot is a passthrough
    /// `Menu` (no query re-filter — the rows already *are* the result), with the
    /// cursor rebased into the window and only the marks that fall inside it kept. The
    /// **keys** of the window are returned so the server can tell Lua which item tables
    /// to retain for `confirm` (bounding Lua's memory to the window too). A non-resumable
    /// or promptless menu records nothing and returns an empty key list.
    ///
    /// Called *before* `close_menu`, which then tears the live menu down.
    pub fn snapshot_picker_for_resume(&mut self) -> Vec<usize> {
        let Some(menu) = self.menu.as_ref() else {
            return Vec::new();
        };
        if !menu.resumable || menu.prompt.is_none() {
            return Vec::new();
        }
        let len = menu.view_len();
        // The window [lo, hi): RESUME_WINDOW rows centered on the cursor, clamped to
        // the ends (a list shorter than the window keeps all of it). `len - WINDOW`
        // saturates to 0 when short, so `lo` falls to 0 and `hi` to `len`.
        let half = RESUME_WINDOW / 2;
        let lo = menu
            .cursor
            .saturating_sub(half)
            .min(len.saturating_sub(RESUME_WINDOW));
        let hi = (lo + RESUME_WINDOW).min(len);
        let window: Vec<MenuItem> = (lo..hi)
            .map(|i| menu.all_items[menu.item_at(i)].clone())
            .collect();
        let keys: Vec<usize> = window.iter().map(|it| it.key).collect();
        let key_set: std::collections::HashSet<usize> = keys.iter().copied().collect();
        // Marks that survive the window (the rest are far from the cursor — dropped).
        let marked: Vec<usize> = menu
            .marked
            .iter()
            .copied()
            .filter(|k| key_set.contains(k))
            .collect();
        let mut snap = menu.clone();
        snap.all_items = window;
        // Passthrough: the window IS the displayed result, shown verbatim (the query
        // stays in the prompt but is not re-applied — it already produced these rows).
        snap.filtered = None;
        snap.match_spans.clear();
        snap.cursor = menu.cursor - lo;
        snap.selected_active = true;
        snap.marked = marked;
        snap.preview_scroll = None;
        self.picker_snapshot = Some(snap);
        keys
    }

    /// Reopen the last resumable picker from its snapshot ([`snapshot_picker_for_resume`]
    /// (Self::snapshot_picker_for_resume)) — `nx.picker.resume()`. Restores the frozen
    /// window verbatim (rows, cursor, marks, query). Returns `false` (a no-op) when no
    /// snapshot exists. The source is re-armed Lua-side, so editing the query re-runs it
    /// (live grep) or re-ranks the window (a static source); an untouched resume just
    /// shows the frozen rows.
    pub fn restore_picker_snapshot(&mut self) -> bool {
        let Some(snap) = self.picker_snapshot.clone() else {
            return false;
        };
        self.menu = Some(snap);
        true
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
        // result + close), so handle them first. Every one of them closes the picker,
        // so snapshot it first for `nx.picker.resume()` — the live menu is gone after
        // `close_menu`. The window's keys ride [`Editor::picker_resume_keys`] to the
        // server, which tells Lua which item tables to keep for `confirm`.
        if matches!(
            action,
            "confirm"
                | "confirm_tab"
                | "confirm_split"
                | "confirm_vsplit"
                | "cancel"
                | "send_to_loclist"
        ) {
            self.picker_resume_keys = self.snapshot_picker_for_resume();
        }
        match action {
            "confirm" | "confirm_tab" | "confirm_split" | "confirm_vsplit" => {
                let chosen = self.menu.as_ref().and_then(|m| {
                    (m.cursor < m.view_len()).then(|| m.all_items[m.item_at(m.cursor)].key)
                });
                // A picker with no matches under the current query confirms nothing.
                if let Some(key) = chosen {
                    // The confirm gesture's open mode (`<C-t>`/`<C-x>`/`<C-v>` ⇒ a new
                    // tab / split / vsplit); the server reads it when it routes the
                    // result to Lua. Plain `confirm` keeps the default (current window).
                    self.picker_confirm_mode = match action {
                        "confirm_tab" => menu::PickerOpenMode::Tab,
                        "confirm_split" => menu::PickerOpenMode::Split,
                        "confirm_vsplit" => menu::PickerOpenMode::Vsplit,
                        _ => menu::PickerOpenMode::Current,
                    };
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
            // "Send these results to a list": the **marked** keys (multi-select) when
            // any are marked, else the whole filtered view in display order. Then close
            // — the server hands the keys to Lua to build a location list. The nxvim
            // port of telescope's send(-selected)-to-loclist.
            "send_to_loclist" => {
                let keys = self
                    .menu
                    .as_ref()
                    .map(|m| {
                        if m.marked.is_empty() {
                            (0..m.view_len())
                                .map(|i| m.all_items[m.item_at(i)].key)
                                .collect()
                        } else {
                            m.marked.clone()
                        }
                    })
                    .unwrap_or_default();
                self.picker_sends.push(keys);
                self.close_menu();
                return Ok(());
            }
            // Multi-select: toggle the current row's mark and advance to the next row
            // (telescope's `<Tab>`). Marks are keyed by source key, so they survive
            // query edits / re-ranking. `clear_select` drops all marks.
            "toggle_select" => {
                if let Some(m) = self.menu.as_mut() {
                    // A single-choice picker (`multiselect = false`, e.g. the cmdline
                    // file completer) ignores the mark gesture entirely.
                    if m.multiselect && m.cursor < m.view_len() {
                        let key = m.all_items[m.item_at(m.cursor)].key;
                        if let Some(pos) = m.marked.iter().position(|&k| k == key) {
                            m.marked.remove(pos);
                        } else {
                            m.marked.push(key);
                        }
                        let last = m.view_len().saturating_sub(1);
                        m.cursor = (m.cursor + 1).min(last);
                        m.selected_active = true;
                    }
                }
                return Ok(());
            }
            "clear_select" => {
                if let Some(m) = self.menu.as_mut() {
                    m.marked.clear();
                }
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
                "preview_left" if menu.preview => menu.preview_scroll = Some(PreviewScroll::Left),
                "preview_right" if menu.preview => menu.preview_scroll = Some(PreviewScroll::Right),
                // A preview gesture with no preview pane is a no-op, not an error.
                "preview_half_down" | "preview_half_up" | "preview_page_down"
                | "preview_page_up" | "preview_left" | "preview_right" => {}
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

    // ── Mouse helpers for the input-grabbing menus (picker / select) ────────────
    // The core hit-tests a click/wheel on the open box back to a row (see
    // `mouse.rs`); these are the mouse equivalents of the navigation / confirm /
    // cancel keymap actions, dispatched by menu kind so the right `apply_*_action`
    // runs. They are deliberately thin — the behavior lives in the action handlers.

    /// Whether an **input-grabbing** menu (a picker or a promptless `nx.ui.select`)
    /// is open — the menus the mouse drives modally (a click off the box cancels the
    /// widget; the wheel scrolls the list). The non-grabbing completion popup is
    /// **not** included (it has its own [`Self::completion_active`] path).
    pub(crate) fn picker_or_select_active(&self) -> bool {
        matches!(self.menu_kind(), Some(MenuKind::Picker | MenuKind::Select))
    }

    /// Highlight row `idx` (clamped) of an open picker / select — the mouse
    /// equivalent of navigating to it. A no-op without an open menu.
    pub(crate) fn menu_cursor_to(&mut self, idx: usize) {
        if let Some(menu) = self.menu.as_mut() {
            let last = menu.view_len().saturating_sub(1);
            menu.cursor = idx.min(last);
            menu.selected_active = true;
        }
    }

    /// Move the highlight one row, non-wrapping (a wheel notch over the list) —
    /// routed to the open menu's own `next`/`prev` action by kind.
    pub(crate) fn menu_step(&mut self, down: bool) {
        let action = if down { "next" } else { "prev" };
        match self.menu_kind() {
            Some(MenuKind::Picker) => {
                let _ = self.apply_picker_action(action);
            }
            Some(MenuKind::Select) => {
                let _ = self.apply_select_action(action);
            }
            _ => {}
        }
    }

    /// Confirm the highlighted row of an open picker / select (a click on the
    /// already-highlighted row), routed by kind — pushes the chosen key and closes.
    pub(crate) fn menu_confirm(&mut self) {
        match self.menu_kind() {
            Some(MenuKind::Picker) => {
                let _ = self.apply_picker_action("confirm");
            }
            Some(MenuKind::Select) => {
                let _ = self.apply_select_action("confirm");
            }
            _ => {}
        }
    }

    /// Cancel an open picker / select (a click off the box), routed by kind —
    /// pushes the cancel result (`None`) and closes, like `<Esc>` on the widget.
    pub(crate) fn menu_cancel(&mut self) {
        match self.menu_kind() {
            Some(MenuKind::Picker) => {
                let _ = self.apply_picker_action("cancel");
            }
            Some(MenuKind::Select) => {
                let _ = self.apply_select_action("cancel");
            }
            _ => {}
        }
    }

    /// Scroll an open picker's preview pane (a wheel notch over it) by the coarsest
    /// available gesture — a half page per notch (the preview-scroll model has no
    /// finer step). A no-op without a preview pane.
    pub(crate) fn menu_preview_scroll(&mut self, down: bool) {
        let action = if down {
            "preview_half_down"
        } else {
            "preview_half_up"
        };
        let _ = self.apply_picker_action(action);
    }

    /// Scroll an open picker's preview pane horizontally one step (a `<S-ScrollWheel>`
    /// or horizontal wheel over it). The server owns the column magnitude and clamps to
    /// the widest visible line. A no-op without a preview pane.
    pub(crate) fn menu_preview_scroll_h(&mut self, right: bool) {
        let action = if right {
            "preview_right"
        } else {
            "preview_left"
        };
        let _ = self.apply_picker_action(action);
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
                align: m.align,
                margin: m.margin,
                anchor_offset: m.anchor_width,
                completion: m.kind == MenuKind::Complete,
                selected_active: m.selected_active,
                docs: m.docs,
                selected_key: sel.map(|i| i.key),
                selected_source_accept: sel.is_some_and(|i| i.source_accept),
                selected_doc: sel.and_then(|i| i.doc.clone()),
                selected_resolve: sel.and_then(|i| i.resolve),
                title: m.title.clone(),
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

    /// Whether each row in the visible window `[start, start + count)` is **marked**
    /// (multi-select), parallel to [`Editor::menu_rows`] — so the client can flag the
    /// marked rows. All `false` when nothing is marked (the common case); empty when
    /// no menu is open.
    pub fn menu_marked_window(&self, start: usize, count: usize) -> Vec<bool> {
        let Some(m) = self.menu.as_ref() else {
            return Vec::new();
        };
        let end = start.saturating_add(count).min(m.view_len());
        if m.marked.is_empty() {
            return vec![false; end.saturating_sub(start)];
        }
        (start..end)
            .map(|i| m.marked.contains(&m.all_items[m.item_at(i)].key))
            .collect()
    }

    /// Resolve an open menu's on-screen box against the cursor-screen `metrics` —
    /// the placement math (the cursor popup's four-tier above/below flip, the
    /// centered-or-aligned editor box, the command-line wildmenu), the scroll
    /// window, and the visible rows. The single geometry shared by the server's
    /// [`MenuView`] projection (`redraw.rs::project_menu`) and the mouse hit-test,
    /// so a click lands on the row the user sees. The server fills the content
    /// (labels' styling, the preview pane, the docs sidebar) around the returned
    /// box; the box, `start`, and rows come from here.
    pub fn menu_geom(&self, m: &MenuView, metrics: MenuMetrics) -> MenuGeom {
        const MAX_H: usize = 10;
        let MenuMetrics {
            cursor_row,
            cursor_screen_col,
            leftcol,
            text_width,
            text_height,
            editor_w,
            editor_h,
        } = metrics;
        // A picker carries a prompt line plus a separator row between it and the
        // list; `nx.ui.select` carries neither. Both count toward the box height
        // (`chrome`), the prompt's text toward the width.
        let prompt_rows = usize::from(m.query.is_some());
        let chrome = prompt_rows * 2;
        let query_w = m.query.as_ref().map_or(0, |q| q.chars().count() + 1);

        // The box rect, the scroll offset of the first visible row, the windowed
        // rows themselves, and the highlighted row rebased into that window — only
        // the visible slice is materialized, so a 100k-item picker costs the same
        // per frame as a 10-item one.
        let (row, col, width, height, start, rows, selected) = match m.placement {
            MenuPlacement::Cursor => {
                // `select` is small — project the whole list (no scrolling subtlety)
                // and let the client place the cursor; keeps the four-tier flip exact.
                let rows = self.menu_rows(0, m.total);
                let count = (rows.len() + prompt_rows).min(MAX_H);
                let content_w = rows
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(1)
                    .max(query_w)
                    .max(1);
                // Anchor under the start of the word being completed (the caret
                // minus the typed prefix's display width), not under the caret —
                // so the list lines up with the text it will replace. `anchor_offset`
                // is `0` for a `select`, leaving it cursor-anchored as before. This
                // is the logical content anchor (the word start); each client offsets
                // the box left by its own left-border width so the *content* lands
                // here (a full cell in the TUI / GUI, ~nothing for the web's 1px rule).
                let anchor_col = cursor_screen_col
                    .saturating_sub(leftcol)
                    .saturating_sub(m.anchor_offset);
                let max_w = text_width.saturating_sub(anchor_col).max(1);
                let width = content_w.min(max_w);
                // The vertical border chrome: 2 (top + bottom) normally, 1 for the
                // top-borderless completion popup. Drives both the fit test and the
                // above-placement origin.
                let vchrome = if m.completion { 1 } else { 2 };
                // Below if the bordered box fits, else above, else clamp to whichever
                // side has more room (the popup's four-tier fallback).
                let below = text_height.saturating_sub(cursor_row.saturating_add(1));
                let above = cursor_row;
                let (row, height) = if count + vchrome <= below {
                    (cursor_row.saturating_add(1), count)
                } else if count + vchrome <= above {
                    (cursor_row - (count + vchrome), count)
                } else if below >= above {
                    (
                        cursor_row.saturating_add(1),
                        below.saturating_sub(vchrome).clamp(1, count),
                    )
                } else {
                    let h = above.saturating_sub(vchrome).clamp(1, count);
                    (cursor_row.saturating_sub(h + vchrome), h)
                };
                // The whole list is sent; the visible window scrolls to keep the
                // selection in view (the same offset the client computes), so the
                // mouse hit-test maps a click to the right absolute row.
                let start = menu_start(
                    m.selected_active.then_some(m.selected),
                    height.saturating_sub(chrome),
                );
                (row, anchor_col, width, height, start, rows, m.selected)
            }
            // `Bottom` is a content-float-only placement; a menu never requests it, so
            // it falls in with the centered `Editor` box here.
            MenuPlacement::Editor | MenuPlacement::Bottom => {
                // A picker is an EDITOR-LEVEL overlay: sized and centered against the
                // WHOLE editor's windows area (`editor_w`/`editor_h`), not the focused
                // window — a split must not squeeze it into the active pane. The
                // resulting box is editor-absolute (windows-area cells); the client
                // anchors it to the windows-area origin (the `editor_relative` flag the
                // server emits), the same convention the which-key content float uses.
                //
                // A picker is a FIXED box — never content-hugging (that looks ragged).
                // Resolve the configured extent against the viewport, default ~80% × 60%.
                const DEFAULT_W: f32 = 0.8;
                const DEFAULT_H: f32 = 0.6;
                let max_w = editor_w.saturating_sub(2).max(1);
                let max_h = editor_h.saturating_sub(2).max(1);
                let width = m
                    .width
                    .map_or((editor_w as f32 * DEFAULT_W).round() as usize, |e| {
                        e.resolve(editor_w)
                    })
                    .clamp(1, max_w);
                // The natural floor is `chrome + 1` (the prompt/separator rows plus
                // one list row), but in a region too short to fit even that
                // (`max_h < chrome + 1`, e.g. the picker focused in a 2-row dock) the
                // floor would exceed the ceiling and `clamp` would panic on `min > max`.
                // Cap the floor at `max_h` so the box just shrinks to the region; the
                // `list_rows` `.max(1)` below still guarantees a usable row.
                let min_h = (chrome + 1).min(max_h);
                let height = m
                    .height
                    .map_or((editor_h as f32 * DEFAULT_H).round() as usize, |e| {
                        e.resolve(editor_h)
                    })
                    .clamp(min_h, max_h);
                // Align the box within the whole editor, inset by the margin. The
                // default (`align == None`) is `Center` — the historical centered
                // picker placement. The `+2` accounts for the box's own border, so
                // the *outer* box (border included) is what gets aligned.
                let align = m.align.unwrap_or(Align::Center);
                let (col, row) = place_aligned(
                    (0, 0, editor_w, editor_h),
                    width.saturating_add(2),
                    height.saturating_add(2),
                    align,
                    m.margin,
                );
                // Scroll the window so the selected row stays visible, clamped to the end,
                // and send `selected` rebased into that window (the client renders the
                // window directly). Only `list_rows` rows are cloned, never all `total`.
                // `chrome` reserves the prompt + separator rows.
                let list_rows = height.saturating_sub(chrome).max(1);
                let mut start = if m.selected >= list_rows {
                    m.selected + 1 - list_rows
                } else {
                    0
                };
                start = start.min(m.total.saturating_sub(list_rows));
                let rows = self.menu_rows(start, list_rows);
                (row, col, width, height, start, rows, m.selected - start)
            }
            MenuPlacement::Cmdline => {
                // The `nx.cmdline_complete` wildmenu: a bordered list floating just
                // above the command line (the bottom of the text area). The whole
                // (small) list is projected — like `select`, no scroll subtlety.
                // `anchor_offset` is the display width of the line before the token
                // (0 for the leading command name), so the box left-aligns under it.
                let rows = self.menu_rows(0, m.total);
                let count = rows.len().min(MAX_H);
                let content_w = rows
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let col = m.anchor_offset.min(text_width.saturating_sub(1));
                let max_w = text_width.saturating_sub(col).max(1);
                let width = content_w.min(max_w);
                // A full bordered box (2 rows of chrome) sitting on the last text rows,
                // so its bottom border abuts the command line below.
                const VCHROME: usize = 2;
                let height = count.min(text_height.saturating_sub(VCHROME).max(1));
                let row = text_height.saturating_sub(height + VCHROME);
                let start = menu_start(m.selected_active.then_some(m.selected), height);
                (row, col, width, height, start, rows, m.selected)
            }
        };
        MenuGeom {
            row,
            col,
            width,
            height,
            selected,
            start,
            rows,
        }
    }
}

/// The scroll offset of the first visible list row so the `selected` row stays in
/// view within a `list_rows`-tall window: `0` until the selection passes the last
/// visible row, then enough to pull it to the bottom. The same windowing every
/// client renders (`pmenu_start`), shared so the mouse hit-test maps a click to the
/// row the user sees. `None` selection (a noselect popup) scrolls from the top.
fn menu_start(selected: Option<usize>, list_rows: usize) -> usize {
    match selected {
        Some(s) if s >= list_rows => s + 1 - list_rows,
        _ => 0,
    }
}

/// The cursor-screen metrics a menu's box is placed against — read from the
/// focused window's projection ([`WindowView`](crate::view::WindowView)) at redraw,
/// or recomputed in core for mouse hit-testing. `text_width` / `text_height` are
/// the focused window's text-area size (its width minus the number gutter, and its
/// visible row count); the cursor fields are window-relative (the cursor popup
/// anchors under the caret). `editor_w` / `editor_h` are the WHOLE editor's
/// windows-area size — an `Editor` / `Bottom` placement (the picker) sizes and
/// centers against these, not the focused window, so it overlays the entire editor
/// rather than the active split. `Cmdline` placement ignores the cursor + editor
/// fields (command-line-anchored), but every caller supplies them.
#[derive(Debug, Clone, Copy)]
pub struct MenuMetrics {
    pub cursor_row: usize,
    pub cursor_screen_col: usize,
    pub leftcol: usize,
    pub text_width: usize,
    pub text_height: usize,
    pub editor_w: usize,
    pub editor_h: usize,
}

/// An open menu's resolved on-screen box. The coordinate base depends on the
/// placement: the focused window's text-area cells for `Cursor`, the command-line
/// area for [`MenuPlacement::Cmdline`], and the **whole editor's windows-area
/// cells** (editor-absolute) for the `Editor` / `Bottom` picker overlay. The
/// geometry both the server projection and the mouse hit-test consume. `rows` is the
/// visible slice
/// `[start, start + rows.len())`; `selected` is the highlighted row rebased into
/// that window (add `start` for the absolute view index).
#[derive(Debug, Clone)]
pub struct MenuGeom {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
    /// Highlighted row, window-relative (add `start` for the absolute view index).
    pub selected: usize,
    /// Scroll offset of the first visible row (`0` for the whole-list placements).
    pub start: usize,
    /// The visible rows `[start, start + rows.len())`: label + matched-char spans.
    pub rows: Vec<(String, Vec<Range<usize>>)>,
}
