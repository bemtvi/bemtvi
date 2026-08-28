//! The unified floating selectable-list widget — `btv.ui.select` (a promptless
//! choice menu) and `btv.picker` (a fuzzy finder with a prompt) both render through
//! it (see `docs/specs/2026-06-14-btv-ui-float-widget.md`). It grabs every keystroke
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

use crate::sandbox::{SandboxError, RERANK_LIMIT};
use std::ops::Range;

use super::*;
use crate::input::{Key, KeyCode};
use crate::mode::KeyContext;
use crate::view::MenuView;

/// How a confirmed picker pick opens — the gesture the user confirmed with. The
/// server reads it off [`Editor::picker_confirm_mode`] and forwards the matching
/// string to `btv._picker_result` → the source's `confirm(item, mode)`. Only the
/// picker uses it (`btv.ui.select` ignores it). `<C-t>`/`<C-x>`/`<C-v>` map to
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
    /// The mode string handed to Lua (`btv._picker_result` / `confirm(item, mode)`).
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
/// reopened picker (`btv.picker.resume`) shows at most this many rows — a window
/// around the cursor of the picker as it closed. A live-grep result set has no
/// stable order across runs, so resume can't re-run the source; it replays this
/// frozen window instead. Bounding it keeps a closed 100k-row picker from pinning
/// its whole list in memory until the next picker opens.
pub(crate) const RESUME_WINDOW: usize = 1000;

/// The picker's "working" spinner frames, cycled by [`Editor::picker_spin`] while a
/// source run is in flight and rendered at the head of the prompt-row
/// [`status`](Menu::status_text). Braille dots: one cell wide in every client, and no
/// glyph outside the base BMP (a nerd-font icon would render as a box for anyone
/// without one patched in).
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Where the float (menu or content float) anchors on the shared placement layer.
/// `Cursor` anchors it under the cursor (the `btv.ui.select` / completion shape);
/// `Editor` centers it over the editor (the picker shape); `Bottom` pins it to the
/// editor's bottom-right corner (the which-key content-float shape — menus never
/// request it); `Cmdline` floats it directly **above the command line**, anchored
/// under the token being completed (the `btv.cmdline_complete` shape).
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
    /// Command-line completion (`btv.cmdline_complete`) — the wildmenu sibling of
    /// `Complete`. Like `Complete` it does **not** grab input (the command line
    /// keeps focus, keys keep editing the line) and its rows carry their own
    /// [`MenuItem::insert`] text; unlike it, the accept replaces a token of the
    /// command line, not buffer text, and it floats above the command line
    /// ([`MenuPlacement::Cmdline`]).
    Cmdline,
}

/// Where a picker's prompt sits relative to its results list — above it (`Top`,
/// the default) or below it (`Bottom`, the telescope-style "input at the bottom"
/// layout). Only meaningful for a picker (a promptless `btv.ui.select` has no
/// prompt); the client lays the box out accordingly and draws the separator
/// between the prompt and the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PromptPos {
    #[default]
    Top,
    Bottom,
}

/// One axis of a window's size — the shared geometry primitive used by every
/// surface (picker / select menus, floats, `btv.view`, the panel). A fixed size,
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

/// One command-line / prompt completion candidate handed to
/// [`Editor::open_cmdline_menu`]: `(label, insert, doc, replace)`. `replace` is an
/// optional explicit `(start_byte, end_byte)` span overriding the trailing-token range
/// for that row (the prompt path's adapter-specified DAP range); the ex catalog passes
/// `None`.
pub type CmdlineCandidate = (String, String, Option<String>, Option<(usize, usize)>);

/// A picker row's **two-column** layout hint, declared by the source
/// (`ctx.push { head = …, focus = … }`) for rows shaped as a location column plus
/// a content body — live_grep's `src/foo.rs:12:5: <the matched line>`. Both are
/// **char** counts into the row's `label`, and both are structural facts only the
/// source knows: a client must never re-derive them by parsing the label.
///
/// Clients fit such a row as two columns (`bemtvi_view::fit_row`): the head keeps a
/// minimum share of the row (so the file name never gets squeezed off by a long
/// line), the body is windowed around the match so the hit itself stays on screen,
/// and the match range is highlighted like a fuzzy hit — a *dynamic* source
/// (live_grep) bypasses the fuzzy matcher, so its own match is the only one there
/// is. A row without a layout truncates as before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowLayout {
    /// Char length of the leading location column (`"src/foo.rs:12:5: "`).
    pub head: u32,
    /// The source's match within the label, as a half-open **char** range. Empty
    /// (`match_end == match_start`) when the source knows only where the interesting
    /// content begins — the body still windows around it, nothing is highlighted.
    pub match_start: u32,
    pub match_end: u32,
    /// Char length of a **pinned tag** at the head's start (`"E "` on a diagnostics
    /// row): the part that *classifies* the row, which survives the elision the rest
    /// of the head takes when the column is too narrow for it. Without it the head
    /// elides tail-first and the classification — the very thing the row is scanned
    /// by — would be the first thing to go. `0` for a head that is pure location
    /// (live_grep's `path:line:col: `), where the tail is what matters.
    pub tag: u32,
}

/// One candidate row: its display `label` and the **opaque source key** that
/// identifies it back to the engine. For `btv.ui.select` the key is the choice's
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
    /// A short **kind** label shown right-aligned on the row (`Complete` menus only):
    /// the candidate's category — `"Snippet"` for a snippet row, an LSP
    /// `CompletionItemKind` name (`"Function"`, `"Variable"`, `"Field"`, …) for an
    /// `lsp` row, or whatever a plugin source declares (`push { kind = … }`). `None`
    /// for a `buffer` word (no kind) and for every `select` / picker / cmdline row —
    /// those render no kind column.
    pub kind: Option<String>,
    pub preview: Option<PreviewTarget>,
    /// The text a completion row inserts when accepted (`Complete` menus only).
    /// `None` ⇒ use `label`. `Select` / picker rows leave this `None` — they
    /// round-trip the opaque `key` to Lua, which applies the choice itself.
    pub insert: Option<String>,
    /// Source **bias** for the merged completion view (`Complete` menus only): a small
    /// number ADDED to the row's fuzzy score to break near-ties in the source's favour
    /// (`lsp` 8 > snippets 5 > buffer 0). The order is fuzzy-first — a clearly better
    /// buffer word still outranks a mediocre `lsp` match — so this only tips *equally-
    /// good* candidates. `0` for `select` / picker rows (single source, no merge).
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
    /// (`btv.complete.source` → `push { text, insert, doc }`). The server renders it
    /// beside the popup for the selected row directly, no source cache — unlike the
    /// `lsp` source, whose docs the server holds itself (`source_accept` rows leave
    /// this `None`). `None` for `buffer` / `select` / picker rows.
    pub doc: Option<String>,
    /// A **lazy-docs resolve handle** for a plugin async row (`Complete` menus only):
    /// the opaque id the Lua source side maps back to `(source.resolve, item)`. Set
    /// when a row has a `resolve` callback but no inline `doc`; the server asks Lua to
    /// resolve it (`btv._complete_resolve`) once the row is selected and caches the
    /// reply for the sidebar. `None` for an inline-doc / `buffer` / `lsp` / `select`
    /// row — there's nothing to resolve. Phase 4-E.
    pub resolve: Option<u64>,
    /// An explicit `(start_byte, end_byte)` replace range into the command line for a
    /// **cmdline/prompt** row, overriding the default `[anchor .. cursor)`. Set when an
    /// `btv.ui.input` completion source supplies an adapter-specified range (the DAP
    /// `CompletionItem.start`/`length`): accepting / previewing the row replaces exactly
    /// this span rather than the trailing-identifier token. `None` ⇒ the default token
    /// range. The bytes index the line as it was at menu-build time, so preview / accept
    /// restore that line first (see [`Editor::cmdline_complete_preview`]).
    pub replace: Option<(usize, usize)>,
    /// The row's two-column layout ([`RowLayout`]) when the source declared one
    /// (`ctx.push { head = …, focus = … }` — live_grep's `path:line:col:` head plus
    /// the matched line). `None` for every plain single-column row, which truncates
    /// path-tail-first as before.
    pub layout: Option<RowLayout>,
    /// The **highlight group** the source painted this row with (`ctx.push { hl = … }`)
    /// — `"DiagnosticError"` on an error row of the diagnostics picker, a git-status
    /// group on a status row. The server resolves the name against the live
    /// colorscheme and ships the resolved style per row; a group the scheme leaves
    /// undefined simply doesn't paint (the row keeps the list's own look), never an
    /// error.
    ///
    /// It colors the row's **head column** when the row declares a [`RowLayout`] — the
    /// location/tag column is the part that classifies the row, and leaving the body
    /// alone keeps the fuzzy-match highlight readable — and the whole label when it
    /// doesn't. `None` for every plain row (`select` / completion / cmdline included).
    pub hl: Option<String>,
    /// The **source's own stated order** for this row (`Complete` menus only): the
    /// opaque string the merged view compares two *equally-good* matches by, before
    /// falling back to the order they streamed in. The `lsp` source fills it from the
    /// item's `sortText` (the protocol's field for exactly this — "how relevant here",
    /// as opposed to the matcher's "how good a match"), falling back to the label the
    /// way the spec says a missing `sortText` does.
    ///
    /// Load-bearing because equal fuzzy scores are the *common* case, not a corner: a
    /// two-character prefix matches a dozen candidates at the same positions, so with
    /// nothing under the score the popup falls back to the order the items happened to
    /// arrive in — a server's internal array order, or across servers whichever replied
    /// first. That is what buries a call's parameters under unrelated globals.
    ///
    /// `None` for a `buffer` word and for every `select` / picker / cmdline row (no
    /// source order to state); a `None` row leads a tied `Some` one, which is the right
    /// way round — the tiers differ by their `priority` bias, so a tie across them means
    /// the un-stated row out-matched the stated one by exactly that bias.
    pub sort_key: Option<String>,
}

impl MenuItem {
    /// A bare row: `label` + `key`, every optional/completion-only field at its
    /// neutral default (`None` / `0` / `false`). The `select` / picker shape; other
    /// builders override the fields they care about via struct-update
    /// (`MenuItem { insert: …, ..MenuItem::new(label, key) }`).
    pub fn new(label: String, key: usize) -> Self {
        MenuItem {
            label,
            key,
            kind: None,
            preview: None,
            insert: None,
            priority: 0,
            source_accept: false,
            doc: None,
            resolve: None,
            replace: None,
            layout: None,
            hl: None,
            sort_key: None,
        }
    }
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

/// One of a picker's editable lines. The query is always present; `Include` and
/// `Exclude` are the glob-pattern filter boxes, reachable only on a source that
/// opted into filtering ([`PromptSet::filterable`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PromptField {
    #[default]
    Query,
    Include,
    Exclude,
}

/// The picker's input-grab query field — a single editable line, modeled on the
/// command line ([`Editor::cmdline`]). `col` is the byte offset of the text
/// cursor within `text`.
#[derive(Clone, Default)]
pub(crate) struct Prompt {
    pub text: String,
    pub col: usize,
}

impl Prompt {
    /// A field seeded with `text`, caret at its end — the shape every pre-filled
    /// line (`open{ query = … }`, a seeded filter box) opens in.
    fn seeded(text: &str) -> Self {
        Prompt {
            col: text.len(),
            text: text.to_string(),
        }
    }

    /// Insert `c` at the text cursor and step past it.
    fn insert(&mut self, c: char) {
        self.text.insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Delete the char before the text cursor (`<BS>`); a no-op at the start.
    /// Returns whether anything changed (so the caller re-queries only on an edit).
    fn backspace(&mut self) -> bool {
        if let Some(prev) = self.prev_boundary() {
            self.text.remove(prev);
            self.col = prev;
            true
        } else {
            false
        }
    }

    /// Delete the char under the text cursor (`<Del>`); a no-op at the end.
    fn delete(&mut self) -> bool {
        if self.col < self.text.len() {
            self.text.remove(self.col);
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
        if let Some(c) = self.text[self.col..].chars().next() {
            self.col += c.len_utf8();
        }
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.col]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    /// The text cursor as a count of characters before it — the caret's column
    /// within the single-line query, which the client draws the prompt caret at
    /// (the prompt is char-indexed like the menu's match spans).
    fn cursor_chars(&self) -> usize {
        self.text[..self.col].chars().count()
    }
}

/// A request to (re-)run a picker's source: the generation the results must be
/// stamped with, plus every line that shapes the search. Queued on
/// [`Editor::picker_query_changes`] for the server to forward to `btv._picker_run`.
///
/// `include` / `exclude` ride along with the query because a source needs all three
/// to reproduce a search — they are the raw comma-separated lines, split into
/// patterns by [`crate::glob::split_patterns`] on the Lua side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerRun {
    pub gen: u64,
    pub query: String,
    pub include: String,
    pub exclude: String,
}

/// What a picker's filter boxes open holding — the seed `btv.picker.open` resolves
/// from the source spec, `btv.picker.setup` defaults, the persisted value and its own
/// options, handed to the core as already-composed text.
///
/// `include` / `exclude` are the raw comma-separated lines the user would have typed
/// (`crate::glob::split_patterns` is what turns them into patterns), so a seeded box
/// is editable exactly like a hand-typed one — a seed, never a lock.
#[derive(Clone, Debug, Default)]
pub struct FilterSeed {
    pub include: String,
    pub exclude: String,
    /// Open with the rows already revealed rather than collapsed to the badge.
    pub expanded: bool,
    /// Whether the source declared `filter = true`. Everything else here is inert
    /// without it.
    pub filterable: bool,
    /// The previously-used lines for each box, **most recent first** — what
    /// `history_prev` / `history_next` cycle through. Persisted across sessions in
    /// the Lua `btv.shada.plugin` store and handed over whole at open: the lists are
    /// capped at a couple of dozen short strings, so carrying them costs nothing and
    /// cycling stays synchronous instead of a round-trip per keypress.
    pub include_history: Vec<String>,
    pub exclude_history: Vec<String>,
}

/// A picker's editable lines: the fuzzy/search `query` plus the two glob-pattern
/// filter boxes (`include` / `exclude`, VSCode's "files to include / exclude").
/// `None` on `Menu` means a promptless `btv.ui.select`.
///
/// The filter boxes are **collapsed** by default — the picker looks exactly as it did
/// before they existed — and `toggle_filters` (`<C-g>`) reveals them. Only the
/// [`focus`](Self::focus)ed field takes typed text, so one `Prompt` editor serves all
/// three. A source that did not opt in (`btv.picker.source{ filter = true }`) has
/// `filterable` clear and can never leave `Query`.
#[derive(Clone, Default)]
pub(crate) struct PromptSet {
    pub query: Prompt,
    pub include: Prompt,
    pub exclude: Prompt,
    /// Which line typed text edits.
    pub focus: PromptField,
    /// Whether the include/exclude rows are drawn. Collapsed still *applies* the
    /// patterns — they surface as the badge rather than as two rows.
    pub expanded: bool,
    /// Whether this picker's source declared `filter = true`. Clear ⇒ the filter
    /// actions fail loud rather than presenting boxes that would filter nothing.
    pub filterable: bool,
    /// Each filter box's recallable past lines and where in them it is browsing.
    pub include_hist: FilterHistory,
    pub exclude_hist: FilterHistory,
}

/// One filter box's line history and its browse position — the cmdline-history model
/// (`<C-Up>` walks back into older lines, `<C-Down>` forward to the one you were
/// typing).
///
/// `idx` is `None` while you are editing your own line, and `Some(i)` while browsing
/// `entries[i]`; `draft` holds the line browsing started from, so walking all the way
/// forward restores what you had rather than stranding you on an old pattern.
#[derive(Clone, Default)]
pub(crate) struct FilterHistory {
    /// Past lines, most recent first.
    pub entries: Vec<String>,
    pub idx: Option<usize>,
    pub draft: String,
}

impl FilterHistory {
    /// Step to an older entry, returning the line to show. The first step from your
    /// own text stashes it as the draft and **skips an entry identical to it** — the
    /// box usually opens pre-filled with the most recent line, and a first press that
    /// visibly did nothing would read as a broken key.
    fn older(&mut self, current: &str) -> Option<String> {
        let next = match self.idx {
            None => {
                self.draft = current.to_string();
                self.entries.iter().position(|e| e != current)?
            }
            Some(i) => (i + 1).min(self.entries.len().saturating_sub(1)),
        };
        self.idx = Some(next);
        self.entries.get(next).cloned()
    }

    /// Step to a newer entry, returning the line to show — or the draft once past the
    /// most recent one. `None` when not browsing (nothing newer to go to).
    fn newer(&mut self) -> Option<String> {
        match self.idx? {
            0 => {
                self.idx = None;
                Some(std::mem::take(&mut self.draft))
            }
            i => {
                self.idx = Some(i - 1);
                self.entries.get(i - 1).cloned()
            }
        }
    }

    /// Leave browsing without changing the text — called when the box is edited, so
    /// the next `history_prev` starts from what is now on the line rather than
    /// resuming a walk the edit invalidated.
    fn stop_browsing(&mut self) {
        self.idx = None;
    }
}

impl PromptSet {
    fn field(&self, which: PromptField) -> &Prompt {
        match which {
            PromptField::Query => &self.query,
            PromptField::Include => &self.include,
            PromptField::Exclude => &self.exclude,
        }
    }

    /// The line typed text edits and the caret is drawn on.
    fn focused(&self) -> &Prompt {
        self.field(self.focus)
    }

    fn focused_mut(&mut self) -> &mut Prompt {
        match self.focus {
            PromptField::Query => &mut self.query,
            PromptField::Include => &mut self.include,
            PromptField::Exclude => &mut self.exclude,
        }
    }

    /// The focused box's history, or `None` when the query has focus (which has no
    /// filter history — it is not a glob line).
    fn focused_history(&mut self) -> Option<&mut FilterHistory> {
        match self.focus {
            PromptField::Query => None,
            PromptField::Include => Some(&mut self.include_hist),
            PromptField::Exclude => Some(&mut self.exclude_hist),
        }
    }

    /// Replace the focused box's text (a history recall), caret at its end.
    fn set_focused(&mut self, text: String) {
        let p = self.focused_mut();
        p.col = text.len();
        p.text = text;
    }

    /// Whether any pattern is in force — what makes the collapsed badge appear, so
    /// a filtered picker never looks like an unfiltered one.
    fn filtering(&self) -> bool {
        !crate::glob::split_patterns(&self.include.text).is_empty()
            || !crate::glob::split_patterns(&self.exclude.text).is_empty()
    }

    /// The collapsed-state badge — `[+2 -1]` for two include and one exclude
    /// pattern, `None` when nothing is filtering or the rows are already shown (the
    /// rows *are* the indicator then). Composed here rather than in each client so
    /// the TUI, GUI and web can't disagree on what a pattern is; the count comes
    /// from the same [`crate::glob::split_patterns`] the filter itself compiles.
    fn badge(&self) -> Option<String> {
        if self.expanded || !self.filtering() {
            return None;
        }
        let inc = crate::glob::split_patterns(&self.include.text).len();
        let exc = crate::glob::split_patterns(&self.exclude.text).len();
        let mut s = String::from("[");
        if inc > 0 {
            s.push_str(&format!("+{inc}"));
        }
        if exc > 0 {
            if inc > 0 {
                s.push(' ');
            }
            s.push_str(&format!("-{exc}"));
        }
        s.push(']');
        Some(s)
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
    /// Set whenever the filtered view changed and a picker re-ranker has not yet
    /// been applied to it. The scorer is *not* run where the view is rebuilt: a
    /// streamed picker rebuilds per batch, and re-ranking per batch would undo
    /// [`Menu::extend_view`]'s O(batch) property. It is settled once per repaint
    /// instead (`Editor::settle_picker_rank`), the same
    /// project-once-per-frame rule the rest of the editor follows.
    rank_dirty: bool,
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
    /// Whether the highlighted row is one the user **chose** (navigated to), as
    /// opposed to a *preselection* — the top row a manual completion trigger
    /// highlights up front so a confirm needs no navigation step. Both look the same
    /// on screen (`selected_active`), but they mean different things to a re-sort:
    /// a chosen row is an identity to follow, a preselection is just "the top row"
    /// and must stay there. Only [`sort_complete_view`](Menu::sort_complete_view)
    /// reads it; every other menu kind sets it alongside `selected_active` and never
    /// re-sorts under the caret.
    selection_chosen: bool,
    placement: MenuPlacement,
    /// The input-grab query field — `Some` for a picker, `None` for `select`.
    prompt: Option<PromptSet>,
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
    /// for a promptless `btv.ui.select`.
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
    /// `send_to_list` sends these when non-empty, else the whole filtered view.
    marked: Vec<usize>,
    /// An optional title rendered on the picker box's top border
    /// (`btv.picker.open(name, { title = … })`). `None` ⇒ no title. Only a picker
    /// sets it; the wildmenu / completion / select menus leave it `None`.
    title: Option<String>,
    /// Whether `<Tab>` multi-selects (marks) rows (`btv.picker.open{ multiselect }`,
    /// default `true`). `false` makes `toggle_select` a no-op — a single-choice
    /// picker (e.g. the cmdline file completer) where marking makes no sense.
    multiselect: bool,
    /// Whether this picker is captured for `btv.picker.resume()` when it closes
    /// (`btv.picker.source{ resumable = … }`, default `true`). A transient internal
    /// picker — the cmdline file completer — sets `false` so it never overwrites the
    /// resume snapshot of the last user-facing picker. Always `false` for a
    /// `select` / completion / cmdline menu (only a picker resumes).
    resumable: bool,
    /// Whether a source run is **in flight** for the live generation — set the moment
    /// a run is queued ([`Editor::open_picker`]'s generation-0 kick, or a prompt edit
    /// that re-runs the source) and cleared when that generation's `done()` lands
    /// ([`Editor::menu_finish`]). It spans the Lua-side debounce as well as the search
    /// itself, which is the point: the interval it covers is exactly "the results on
    /// screen are not the answer to what is typed yet", and that is what the prompt's
    /// [`status`](Menu::status_text) readout tells the user. Always `false` for a
    /// promptless `select` / completion / cmdline menu, which have no source to run.
    running: bool,
    /// The spinner animation frame, advanced by [`Editor::picker_spin`] while
    /// `running` (the server wakes on a timer to bump it and repaint). Only ever read
    /// through [`Menu::status_text`], modulo the frame count, so it wraps freely.
    spin: u8,
}

impl Menu {
    /// An empty menu of the given `kind` / `placement` with every other field at its
    /// neutral default: no items, passthrough view, cursor at row 0, noselect, no
    /// prompt/prefix, generation 0, default box geometry. Each open-path builds on
    /// this via struct-update (`Menu { all_items, …, ..Menu::new(kind, placement) }`),
    /// overriding only what that menu shape actually diverges on.
    fn new(kind: MenuKind, placement: MenuPlacement) -> Self {
        Menu {
            kind,
            rank_dirty: false,
            anchor: 0,
            anchor_width: 0,
            all_items: Vec::new(),
            filtered: None,
            match_spans: Vec::new(),
            cursor: 0,
            selected_active: false,
            selection_chosen: false,
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
            running: false,
            spin: 0,
        }
    }

    /// The picker's progress readout, rendered right-aligned on the prompt row: the
    /// result count, led by a spinner frame while a source run is in flight.
    ///
    /// This is the picker's only "it is working" signal, and it exists because a
    /// search over a large tree spends most of its time in a state that looks
    /// identical to a broken one — the rows on screen belong to the *previous* query
    /// (deliberately: they stay confirmable until the new run's first result lands),
    /// so with no readout the box simply looks frozen. Composed here rather than in
    /// each client for the same reason the filter [`badge`](PromptSet::badge) is: one
    /// format, three clients, no counting done thrice.
    ///
    /// `matched/total` whenever the local matcher has narrowed the candidates (a
    /// static source with a query typed); a bare count when every candidate is shown —
    /// which is always the case for a **dynamic** source, since it filters itself and
    /// its rows ride through in passthrough. `None` for a promptless `btv.ui.select` /
    /// completion / cmdline menu: no prompt row to hang it on, and no source behind it.
    fn status_text(&self) -> Option<String> {
        self.prompt.as_ref()?;
        let matched = self.view_len();
        let total = self.all_items.len();
        let mut out = String::new();
        if self.running {
            out.push(SPINNER[self.spin as usize % SPINNER.len()]);
            out.push(' ');
        }
        if matched == total {
            out.push_str(&matched.to_string());
        } else {
            out.push_str(&matched.to_string());
            out.push('/');
            out.push_str(&total.to_string());
        }
        Some(out)
    }

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
            Some(p) => p.query.text.as_str(),
            None => self.complete_prefix.as_str(),
        }
    }

    /// This menu's current run request — the generation plus every line the source
    /// needs to reproduce the search. A promptless `select` never reaches here, so the
    /// absent-prompt case reads as empty text rather than failing.
    fn picker_run(&self) -> PickerRun {
        let p = self.prompt.as_ref();
        PickerRun {
            gen: self.generation,
            query: p.map_or(String::new(), |p| p.query.text.clone()),
            include: p.map_or(String::new(), |p| p.include.text.clone()),
            exclude: p.map_or(String::new(), |p| p.exclude.text.clone()),
        }
    }

    /// Drop the displayed results and adopt `gen` as the generation on show — the
    /// atomic swap when a newer run's first batch lands (or when it completes empty).
    ///
    /// The view mode is re-derived rather than forced to passthrough: a **static**
    /// source with a live query must resume in *filtered* mode, exactly as
    /// [`Editor::open_picker`] seeds it, so the re-run's items are ranked against the
    /// query as they stream (`extend_view` is a no-op in passthrough, which would show
    /// them unranked). Before include/exclude filters this could only ever fire for a
    /// dynamic source — which self-filters and so wants passthrough — and forcing
    /// `None` was equivalent; a filter edit re-runs a static source too.
    fn reset_items(&mut self, gen: u64) {
        let refilter = !self.dynamic && !self.match_query().is_empty();
        self.all_items.clear();
        self.filtered = refilter.then(Vec::new);
        self.match_spans.clear();
        self.items_gen = gen;
        self.cursor = 0;
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
        self.rank_dirty = true;
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
        self.rank_dirty = true;
        self.clamp_cursor();
    }

    fn clamp_cursor(&mut self) {
        let len = self.view_len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
    }

    /// Re-order the merged completion view by a **blended** key: each row's raw fuzzy
    /// score against the prefix plus its source's small `priority` bias, descending. So
    /// the list is fuzzy-first — a clearly better buffer word beats a mediocre `lsp`
    /// match — but among *equally-good* matches the higher-priority source edges ahead
    /// (`lsp` bias 8 > snippets 5 > buffer 0). This replaces the old strict priority
    /// tiers, which pinned every buffer word below every snippet regardless of quality.
    ///
    /// Rows that tie on the blended key then order by their source's own stated order
    /// ([`MenuItem::sort_key`] — the `lsp` source's `sortText`), and only then by the
    /// order they streamed in. Ties are the *common* case, not a corner: a short prefix
    /// matches many candidates at the same positions, so without the second key the
    /// popup's order is the order the items happened to arrive in — a server's internal
    /// array order, and across servers whichever replied first. The matcher still decides
    /// *whether* two rows tie; the source only breaks the tie, so a stated order can
    /// never pull a poor match above a good one.
    ///
    /// Re-scores via [`crate::fuzzy::rank_scored`] over the (small) filtered label set —
    /// the streamed spans are kept; only the order changes. A stable sort over the
    /// parallel `filtered` / `match_spans`. Called by [`Editor::menu_push`] for
    /// [`MenuKind::Complete`] menus only (a single-source picker keeps pure fuzzy order).
    /// The `all_items` index the caret is standing on, when a row is *actively*
    /// chosen — the identity a reorder has to keep. `self.cursor` is a position in
    /// the *view*, so leaving it alone slides whatever the reorder moves into that
    /// slot under the caret: the popup would accept a candidate the user never
    /// chose.
    ///
    /// `None` when there is no identity to preserve and the caret belongs at the
    /// top: a noselect popup (nothing highlighted yet), and equally a
    /// *preselection* — the top row a manual trigger highlights before the async
    /// sources have answered. A preselection means "the top row", so following it
    /// down as the snippet/LSP rows sort above would open the popup with its caret
    /// parked mid-list, on a candidate nobody chose.
    fn chosen_identity(&self) -> Option<usize> {
        (self.selected_active && self.selection_chosen && self.cursor < self.view_len())
            .then(|| self.item_at(self.cursor))
    }

    /// Park the caret back on `identity` after a reorder — or on the new top row
    /// when there was none. A row that vanished mid-reorder is not reachable (a
    /// reorder is a permutation, never a filter), but fall back to the cursor as it
    /// was rather than assuming it.
    fn follow_identity(&mut self, identity: Option<usize>) {
        match identity {
            Some(item) => {
                if let Some(at) = self
                    .filtered
                    .as_ref()
                    .and_then(|f| f.iter().position(|&i| i == item))
                {
                    self.cursor = at;
                }
            }
            None => self.cursor = 0,
        }
    }

    /// Reorder the first `keys.len()` rows of the view by **descending** key,
    /// keeping the parallel `filtered` / `match_spans` in lockstep. Stable, so rows
    /// the keys tie keep their existing order rather than shuffling frame to frame.
    ///
    /// Shared by both re-rankers ([`Editor::settle_picker_rank`] and
    /// [`Editor::settle_complete_rank`]); neither touches the tail beyond
    /// `keys.len()`, which keeps native order.
    fn reorder_head_by_keys(&mut self, keys: &[f64]) {
        let n = keys.len();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| {
            keys[b]
                .partial_cmp(&keys[a])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let filtered = self.filtered.as_mut().expect("caller checked");
        let head: Vec<usize> = order.iter().map(|&p| filtered[p]).collect();
        filtered[..n].copy_from_slice(&head);
        let spans: Vec<_> = order
            .iter()
            .map(|&p| std::mem::take(&mut self.match_spans[p]))
            .collect();
        for (slot, v) in self.match_spans[..n].iter_mut().zip(spans) {
            *slot = v;
        }
    }

    fn sort_complete_view(&mut self) {
        if self.filtered.is_none() {
            return;
        }
        // The identity the caret has to keep across the reorder — see
        // [`Menu::chosen_identity`] for why the view position alone will not do.
        let selected = self.chosen_identity();
        let filtered = self.filtered.take().unwrap();
        let spans = std::mem::take(&mut self.match_spans);
        // The blend needs each row's fuzzy score against the live prefix; re-rank the
        // filtered labels to recover it (they all match, so every position gets a score).
        let query = self.match_query().to_string();
        let labels: Vec<&str> = filtered
            .iter()
            .map(|&i| self.all_items[i].label.as_str())
            .collect();
        let mut score_of = vec![0i32; filtered.len()];
        for (pos, sc, _) in crate::fuzzy::rank_scored(&query, &labels) {
            score_of[pos] = sc as i32;
        }
        // Sort a permutation of the view rather than the rows themselves, so the
        // comparator can read `all_items` in place (no cloned keys) and the parallel
        // `filtered` / `match_spans` are rebuilt from one order.
        let mut order: Vec<usize> = (0..filtered.len()).collect();
        // Descending blended key, then the source's stated order; stable, so rows that
        // tie on both keep their streamed order.
        order.sort_by(|&a, &b| {
            let (ia, ib) = (&self.all_items[filtered[a]], &self.all_items[filtered[b]]);
            (score_of[b] + ib.priority)
                .cmp(&(score_of[a] + ia.priority))
                .then_with(|| ia.sort_key.cmp(&ib.sort_key))
        });
        let mut spans: Vec<Option<Vec<Range<usize>>>> = spans.into_iter().map(Some).collect();
        self.match_spans = order
            .iter()
            .map(|&pos| spans[pos].take().unwrap_or_default())
            .collect();
        let reordered: Vec<usize> = order.into_iter().map(|pos| filtered[pos]).collect();
        self.filtered = Some(reordered);
        // Follow the chosen row to wherever it landed (a preselection / noselect
        // caret re-parks on the new top row).
        self.follow_identity(selected);
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
            self.selection_chosen = true;
            self.cursor = 0;
        } else {
            self.selection_chosen = true;
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
            self.selection_chosen = true;
            self.cursor = len - 1;
        } else {
            self.selection_chosen = true;
            self.cursor = (self.cursor + len - 1) % len;
        }
    }
}

impl Editor {
    /// Apply the picker re-ranker to the current view, at most once per repaint.
    ///
    /// Deliberately *not* called where the view is rebuilt. A streamed picker
    /// rebuilds once per arriving batch, so re-ranking there would turn
    /// `extend_view`'s O(batch) into O(view) per batch — the shape the
    /// never-freeze rule exists to prevent. The server calls this from `redraw`,
    /// just before projecting the menu, so cost is bounded per *frame* however
    /// many batches landed in between.
    ///
    /// Only the top [`RERANK_LIMIT`] survivors are scored; the tail keeps native
    /// order. There is nothing to do without an active query — an unfiltered
    /// picker is passthrough, and re-ranking it would mean scoring `all_items`,
    /// which is exactly what this feature must never do.
    pub fn settle_picker_rank(&mut self) {
        let Some(handle) = self.picker_scorer else {
            return;
        };
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        if menu.kind != MenuKind::Picker || !menu.rank_dirty || menu.filtered.is_none() {
            return;
        }
        let query = menu.match_query().to_string();

        let failure = self.with_sandbox(|ed, sb| {
            let menu = ed.menu.as_mut().expect("checked above");
            menu.rank_dirty = false;
            let filtered = menu.filtered.as_ref().expect("checked above");
            let n = filtered.len().min(RERANK_LIMIT);

            // The native score each row already earned, so an expression can nudge
            // the order instead of reinventing matching. One extra native pass over
            // at most RERANK_LIMIT labels — nanoseconds each, next to microseconds
            // per sandbox call.
            let labels: Vec<&str> = filtered[..n]
                .iter()
                .map(|&i| menu.all_items[i].label.as_str())
                .collect();
            let mut scores = vec![0i64; n];
            for (pos, sc, _) in crate::fuzzy::rank_scored(&query, &labels) {
                scores[pos] = sc as i64;
            }

            let mut keys = Vec::with_capacity(n);
            for (pos, label) in labels.iter().enumerate() {
                let got = match sb.as_mut() {
                    Some(engine) => engine.call_score(handle, label, &query, scores[pos]),
                    None => Err(SandboxError::Unavailable),
                };
                match got {
                    Ok(k) => keys.push(k),
                    Err(err) => return Some(err),
                }
            }

            let menu = ed.menu.as_mut().expect("checked above");
            menu.reorder_head_by_keys(&keys);
            None
        });

        // Loud once, then off: this runs every repaint, so leaving a broken scorer
        // installed would echo the same error on every keystroke.
        if let Some(err) = failure {
            self.echo(format!("btv.picker.scorer: {err} — scorer disabled"));
            if let Some(h) = self.picker_scorer.take() {
                self.sandbox_release(h);
            }
        }
    }

    /// Apply `btv.complete.scorer` to the completion popup's rows.
    ///
    /// The picker's sibling ([`Editor::settle_picker_rank`]), and bounded the same
    /// way: once per *frame* rather than once per arriving batch (a streaming source
    /// rebuilds the view per batch, and re-ranking there would turn `extend_view`'s
    /// O(batch) into O(view)-per-batch), and over at most [`RERANK_LIMIT`] rows.
    ///
    /// Two differences from the picker. The `score` handed in is the **blended**
    /// native key — the fuzzy score *plus* the row's source `priority` bias, which
    /// is what [`Menu::sort_complete_view`] sorts on — so nudging it composes with
    /// the source order instead of fighting it. And the caret follows the row it was
    /// standing on ([`Menu::chosen_identity`]): a popup that reorders under the
    /// caret would accept a candidate nobody chose.
    pub fn settle_complete_rank(&mut self) {
        let Some(handle) = self.complete_scorer else {
            return;
        };
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        if menu.kind != MenuKind::Complete || !menu.rank_dirty || menu.filtered.is_none() {
            return;
        }
        let query = menu.match_query().to_string();

        let failure = self.with_sandbox(|ed, sb| {
            let menu = ed.menu.as_mut().expect("checked above");
            menu.rank_dirty = false;
            let filtered = menu.filtered.as_ref().expect("checked above");
            let n = filtered.len().min(RERANK_LIMIT);

            // The blended key each row already earned. One extra native pass over at
            // most RERANK_LIMIT labels — nanoseconds each, next to microseconds per
            // sandbox call.
            let labels: Vec<&str> = filtered[..n]
                .iter()
                .map(|&i| menu.all_items[i].label.as_str())
                .collect();
            let mut scores = vec![0i64; n];
            for (pos, sc, _) in crate::fuzzy::rank_scored(&query, &labels) {
                scores[pos] = sc as i64;
            }
            for (pos, &i) in filtered[..n].iter().enumerate() {
                scores[pos] += menu.all_items[i].priority as i64;
            }
            let kinds: Vec<&str> = filtered[..n]
                .iter()
                .map(|&i| menu.all_items[i].kind.as_deref().unwrap_or(""))
                .collect();

            let mut keys = Vec::with_capacity(n);
            for (pos, label) in labels.iter().enumerate() {
                let got = match sb.as_mut() {
                    Some(engine) => {
                        engine.call_complete_score(handle, label, &query, scores[pos], kinds[pos])
                    }
                    None => Err(SandboxError::Unavailable),
                };
                match got {
                    Ok(k) => keys.push(k),
                    Err(err) => return Some(err),
                }
            }

            let menu = ed.menu.as_mut().expect("checked above");
            let chosen = menu.chosen_identity();
            menu.reorder_head_by_keys(&keys);
            menu.follow_identity(chosen);
            None
        });

        // Loud once, then off — as for the picker: a scorer that runs every repaint
        // must not echo the same error on every keystroke.
        if let Some(err) = failure {
            self.echo(format!("btv.complete.scorer: {err} — scorer disabled"));
            if let Some(h) = self.complete_scorer.take() {
                self.sandbox_release(h);
            }
        }
    }

    /// Open a promptless floating choice list (`btv.ui.select`): `items` are the
    /// display labels, `cursor` the initially-highlighted row (clamped). Grabs
    /// input until the user confirms (`<CR>`) or cancels (`<Esc>` / `q`); the
    /// outcome lands in [`Editor::menu_results`]. The list must be non-empty.
    pub fn open_menu(&mut self, items: Vec<String>, placement: MenuPlacement, cursor: usize) {
        let all_items: Vec<MenuItem> = items
            .into_iter()
            .enumerate()
            .map(|(key, label)| MenuItem::new(label, key))
            .collect();
        let last = all_items.len().saturating_sub(1);
        // Opens noselect (`selected_active` defaults `false`), like the completion
        // popup / wildmenu: nothing is highlighted until the user navigates, so
        // `<CR>` on a just-opened menu does nothing rather than confirming a row no
        // one picked. The first navigation activates the highlight
        // (`apply_select_action`).
        let mut menu = Menu {
            all_items,
            cursor: cursor.min(last),
            ..Menu::new(MenuKind::Select, placement)
        };
        menu.refilter();
        self.menu = Some(menu);
    }

    /// Open a fuzzy picker (`btv.picker`): a centered float with a prompt that grabs
    /// input. The source streams candidates in via [`Editor::menu_push`].
    /// `dynamic` selects forward-the-query (live grep) over local fuzzy matching.
    /// `width` / `height` fix the box size ([`Extent`], `None` ⇒ the picker
    /// default) — never content-derived. `align` / `margin` place the box within
    /// the editor area (`None` align ⇒ centered). `query` pre-fills the prompt
    /// (`btv.picker.open(name, { query = … })`) with the caret at its end, so the
    /// list opens already filtered against it; empty ⇒ the historical empty-prompt
    /// open. The server invokes the source's initial run after opening with this
    /// `query` (generation `0`). `filters` seeds the include/exclude boxes and says
    /// whether this source has them at all ([`FilterSeed`]).
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
        filters: FilterSeed,
    ) {
        let prompt = PromptSet {
            query: Prompt::seeded(query),
            include: Prompt::seeded(&filters.include),
            exclude: Prompt::seeded(&filters.exclude),
            focus: PromptField::Query,
            // Only a filterable picker can show the rows; `expanded` on a source that
            // never opted in would draw two boxes that filter nothing.
            expanded: filters.filterable && filters.expanded,
            filterable: filters.filterable,
            include_hist: FilterHistory {
                entries: filters.include_history,
                ..FilterHistory::default()
            },
            exclude_hist: FilterHistory {
                entries: filters.exclude_history,
                ..FilterHistory::default()
            },
        };
        // A non-empty seed on a STATIC source opens in *filtered* mode (an empty
        // ranked view), so the items the source streams in are matched against the
        // seed as they arrive (`extend_view` is a no-op in passthrough). A DYNAMIC
        // source bypasses the matcher — it filters itself from `ctx.query` (which is
        // seeded too) — so it must stay in passthrough or its own rows would be
        // re-ranked away. An empty seed always stays in passthrough.
        let filtered = (!query.is_empty() && !dynamic).then(Vec::new);
        self.menu = Some(Menu {
            filtered,
            // A picker always has a highlighted row, unlike the noselect default.
            selected_active: true,
            prompt: Some(prompt),
            prompt_pos,
            dynamic,
            preview,
            width,
            height,
            align,
            margin,
            title,
            multiselect,
            resumable,
            // The server kicks the source's generation-0 run immediately after this
            // open, so the picker is already working by the time the first frame is
            // painted — say so on that frame rather than a tick later.
            running: true,
            ..Menu::new(MenuKind::Picker, placement)
        });
    }

    /// Open / rebuild the **command-line completion** popup (`btv.cmdline_complete`):
    /// the catalog `candidates` (each `(label, insert, doc)`) are fuzzy-ranked against
    /// `prefix` (the command-name token typed so far) and become a
    /// [`MenuKind::Cmdline`] menu floating above the command line, anchored under the
    /// token. `anchor` is the byte offset of the token in [`Editor::cmdline`] (accept
    /// replaces `[anchor .. cmdline_col)` — Phase 2); `anchor_width` is the display
    /// width of the line before it (the float's column after the `:` prompt). Nothing
    /// matching closes any open popup (the wildmenu just disappears). Opens noselect —
    /// no row highlighted until the user navigates (Phase 2). The server calls this
    /// after resolving an [`Editor::cmdline_complete_request`] against the Lua source.
    ///
    /// Each candidate is `(label, insert, doc, replace)`; `replace` is an optional
    /// explicit `(start_byte, end_byte)` span overriding `[anchor .. cursor)` for that
    /// row (the prompt path's adapter-specified range — see [`MenuItem::replace`]). The
    /// ex catalog passes `None`.
    pub fn open_cmdline_menu(
        &mut self,
        anchor: usize,
        anchor_width: usize,
        prefix: &str,
        candidates: Vec<CmdlineCandidate>,
        docs: bool,
    ) {
        let labels: Vec<&str> = candidates.iter().map(|(l, ..)| l.as_str()).collect();
        let ranked = crate::fuzzy::rank(prefix, &labels);
        if ranked.is_empty() {
            self.close_cmdline_menu();
            return;
        }
        let mut all_items = Vec::with_capacity(ranked.len());
        let mut filtered = Vec::with_capacity(ranked.len());
        let mut match_spans = Vec::with_capacity(ranked.len());
        for (key, (idx, spans)) in ranked.into_iter().enumerate() {
            let (label, insert, doc, replace) = candidates[idx].clone();
            all_items.push(MenuItem {
                insert: Some(insert),
                doc,
                replace,
                ..MenuItem::new(label, key)
            });
            filtered.push(key);
            match_spans.push(spans);
        }
        // Noselect (the `selected_active` default): nothing highlighted until the
        // user navigates (Phase 2), so `<CR>` keeps executing the typed line until
        // a row is chosen.
        self.menu = Some(Menu {
            anchor,
            anchor_width,
            all_items,
            filtered: Some(filtered),
            match_spans,
            complete_prefix: prefix.to_string(),
            docs,
            ..Menu::new(MenuKind::Cmdline, MenuPlacement::Cmdline)
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
        self.cmdline_complete_navigate(true);
    }

    pub(crate) fn cmdline_complete_prev(&mut self) {
        self.cmdline_complete_navigate(false);
    }

    /// Step the wildmenu selection one row (`down` = forward) and re-preview it in
    /// the command line — the shared body of `cmdline_complete_next`/`_prev`.
    fn cmdline_complete_navigate(&mut self, down: bool) {
        if let Some(m) = self.cmdline_menu_mut() {
            if down {
                m.select_next();
            } else {
                m.select_prev();
            }
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

    /// Whether the open menu is the command-line wildmenu (`btv.cmdline_complete`),
    /// the one the mouse drives in command mode.
    pub fn cmdline_complete_active(&self) -> bool {
        self.menu_kind() == Some(MenuKind::Cmdline)
    }

    /// The actively-highlighted command-line completion's `(start, end, insert_text)`
    /// **without** closing the menu — the peek twin of
    /// [`cmdline_complete_take_accept`](Self::cmdline_complete_take_accept), used to
    /// preview the selection in the command line as the user cycles. `None` when no
    /// cmdline menu is open or nothing is selected yet (the noselect popup). The replace
    /// span is the row's explicit `replace` range (the adapter-specified prompt range)
    /// when set, else the default `[anchor .. cursor)`.
    pub(crate) fn cmdline_complete_selected(&self) -> Option<(usize, usize, String)> {
        let m = self.menu.as_ref().filter(|m| m.kind == MenuKind::Cmdline)?;
        if !m.selected_active {
            return None;
        }
        let row = m.all_items.get(m.item_at(m.cursor))?;
        let (start, end) = row.replace.unwrap_or((m.anchor, self.cmdline_col));
        Some((
            start,
            end,
            row.insert.clone().unwrap_or_else(|| row.label.clone()),
        ))
    }

    /// The actively-selected command-line completion's `(start, end, insert_text)`,
    /// closing the menu. `None` when no cmdline menu is open **or nothing is selected
    /// yet** (the popup is noselect until the user navigates) — the caller then runs
    /// the typed line unchanged. The caller rewrites `[start .. end)` with the insert
    /// text ([`Editor::cmdline_complete_accept`]); the span is the row's explicit
    /// `replace` range when set, else `[anchor .. cursor)`.
    pub(crate) fn cmdline_complete_take_accept(&mut self) -> Option<(usize, usize, String)> {
        let cursor = self.cmdline_col;
        let m = self.cmdline_menu_mut()?;
        if !m.selected_active {
            return None;
        }
        let row = m.all_items.get(m.item_at(m.cursor))?;
        let (start, end) = row.replace.unwrap_or((m.anchor, cursor));
        let acc = (
            start,
            end,
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

    /// Whether the open picker has a source run **in flight** — the queued-to-`done()`
    /// window described on [`Menu::running`]. The server reads it to keep the spinner
    /// animating: while this holds it re-arms a short timer that calls
    /// [`picker_spin`](Self::picker_spin) and repaints. `false` when no menu is open, or
    /// when the open one is a promptless `select` / completion popup.
    pub fn picker_running(&self) -> bool {
        self.menu.as_ref().is_some_and(|m| m.running)
    }

    /// Advance the picker's spinner one frame (the server's animation wake). A no-op
    /// unless a run is in flight — the wake that arrives just after a search finished
    /// paints the settled readout instead, and the frame it paints re-arms nothing
    /// (`picker_running` is false by then), so the clock stops on its own.
    pub fn picker_spin(&mut self) {
        if let Some(m) = self.menu.as_mut().filter(|m| m.running) {
            m.spin = m.spin.wrapping_add(1);
        }
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
            // First result of a newer run — swap the old results out now.
            menu.reset_items(gen);
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

    /// Raise an already-pushed completion row's `priority` and re-order the view — for
    /// a row whose source learns, *after* pushing it, that a better-ranked contributor
    /// also offers it.
    ///
    /// The LSP source is the caller: every capable server is asked at once and their
    /// replies stream in, so a row is pushed at the rank of whichever server happened
    /// to answer first. When a higher-ranked server later makes the same offer, the row
    /// is merged into (one row, both servers' docs) and must rank where the *best* of
    /// its contributors would put it — otherwise the popup's order depends on which
    /// server was quicker, which is exactly what a stated priority is meant to remove.
    ///
    /// Raise-only, so this can never demote a row under a late straggler; a no-op on a
    /// closed menu, a stale generation, a non-completion menu, or a key no row carries.
    /// `key` is matched against **delegated-accept** rows only, the ones a source owns.
    pub fn menu_reprioritize(&mut self, gen: u64, key: usize, priority: i32) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };
        if menu.kind != MenuKind::Complete || menu.items_gen != gen {
            return;
        }
        let Some(item) = menu
            .all_items
            .iter_mut()
            .find(|i| i.key == key && i.source_accept)
        else {
            return;
        };
        if item.priority >= priority {
            return;
        }
        item.priority = priority;
        menu.sort_complete_view();
        menu.clamp_cursor();
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
            menu.reset_items(gen);
        }
        // The live generation's search is over: stop the spinner. A *stale* run's
        // `done()` (the user has typed on) must not — its successor is still working.
        if gen == menu.generation {
            menu.running = false;
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
                // A buffer word completes to plain text — the LSP `Text` kind. It
                // inserts natively (no source edit) and carries no docs; async
                // source rows (with a `doc` or a `resolve` handle) are appended
                // later via `menu_push`.
                kind: Some("Text".to_string()),
                priority,
                ..MenuItem::new(label, key)
            });
            filtered.push(key);
            match_spans.push(spans);
        }
        self.menu = Some(Menu {
            anchor,
            anchor_width: crate::unicode::display_width(prefix),
            all_items,
            // `Some` (an active query = the prefix) even when empty, so a later
            // `menu_push` matches the streamed async batch against the prefix via
            // `extend_view` rather than dropping to passthrough.
            filtered: Some(filtered),
            match_spans,
            // Noselect by default: nothing is highlighted until the user navigates,
            // so an auto-opened popup never hijacks `<CR>` (it stays a newline). A
            // manual trigger passes `preselect = true` to highlight the first row.
            selected_active: preselect,
            complete_prefix: prefix.to_string(),
            // The docs sidebar follows the engine config; the server fills it from
            // its LSP item cache for the selected row (a `buffer` row has no docs).
            docs: self.complete_config.docs,
            generation: gen,
            items_gen: gen,
            ..Menu::new(MenuKind::Complete, MenuPlacement::Cursor)
        });
    }

    /// Open a non-grabbing choice dropdown over the byte range `(sr,sc)..(er,ec)` — the
    /// Lua-facing entry (`btv.complete.choice`) behind a plugin snippet engine's choice
    /// tabstops. Enters Insert, parks the caret at the range end, and opens the
    /// [`open_snippet_choice_menu`](Self::open_snippet_choice_menu) popup anchored at the
    /// start, so accepting a row replaces exactly `[start..end)` with the pick (the
    /// popup is non-grabbing, so a plugin's `on_bytes` then syncs any mirrors natively).
    /// Preselects the alternative already sitting in the range. Rows/cols are 0-based.
    pub fn open_choice_menu(
        &mut self,
        sr: usize,
        sc: usize,
        er: usize,
        ec: usize,
        choices: Vec<String>,
    ) {
        if choices.is_empty() {
            return;
        }
        let start = self.buffer().byte_at(sr, sc);
        let end = self.buffer().byte_at(er, ec).max(start);
        self.mode = Mode::Insert;
        self.set_cursor_char_insert(end);
        let current = self.buffer().text.slice(start..end).to_string();
        let active = choices.iter().position(|c| *c == current).unwrap_or(0);
        self.open_snippet_choice_menu(start, &choices, active);
        self.ensure_visible();
    }

    /// Open a **snippet-choice** dropdown: a [`MenuKind::Complete`] popup listing a
    /// choice tabstop's alternatives (`${1|a,b,c|}`), anchored at the tabstop start
    /// `anchor` so accepting a row replaces the whole current value `[anchor..cursor)`.
    /// Preselects `active` (the alternative already in the buffer) so `<C-y>`/`<CR>`
    /// accepts at once and `<C-n>`/`<C-p>` steps from it. Unlike the engine popup this
    /// bypasses prefix fuzzy-ranking — every alternative is shown regardless of the
    /// value already sitting in the tabstop — and carries no docs sidebar.
    pub(crate) fn open_snippet_choice_menu(
        &mut self,
        anchor: usize,
        choices: &[String],
        active: usize,
    ) {
        self.complete_gen += 1;
        let gen = self.complete_gen;
        let value = self
            .buffer()
            .text
            .slice(anchor..self.cursor_char())
            .to_string();
        let all_items: Vec<MenuItem> = choices
            .iter()
            .enumerate()
            .map(|(key, c)| MenuItem {
                insert: Some(c.clone()),
                ..MenuItem::new(c.clone(), key)
            })
            .collect();
        let n = all_items.len();
        self.menu = Some(Menu {
            anchor,
            anchor_width: crate::unicode::display_width(&value),
            all_items,
            filtered: Some((0..n).collect()),
            match_spans: vec![Vec::new(); n],
            cursor: active.min(n.saturating_sub(1)),
            // Preselected: a choice dropdown is an explicit pick, so `<C-y>`/`<CR>`
            // accepts the highlighted alternative straight away.
            selected_active: true,
            generation: gen,
            items_gen: gen,
            ..Menu::new(MenuKind::Complete, MenuPlacement::Cursor)
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
        self.complete_select_navigate(true);
    }

    pub(crate) fn complete_select_prev(&mut self) {
        self.complete_select_navigate(false);
    }

    /// Step the completion selection one row (`down` = forward) and reset the docs
    /// scroll to the new row's top — the shared body of
    /// `complete_select_next`/`_prev`. A no-op unless a completion menu is open.
    fn complete_select_navigate(&mut self, down: bool) {
        if let Some(m) = self.completion_menu_mut() {
            if down {
                m.select_next();
            } else {
                m.select_prev();
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
                m.selection_chosen = true;
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
        let confirm_first = self.complete_config.confirm_first;
        let m = self.completion_menu_mut()?;
        // Which view row to accept: the active selection when the user has navigated to
        // one; otherwise — only when `confirm_first` is set — the first row (Enter-to-
        // accept). A noselect menu with `confirm_first` off accepts nothing, so the
        // caller lets the confirm key fall through (e.g. `<CR>` inserts a newline).
        let view_idx = if m.selected_active {
            m.cursor
        } else if confirm_first && m.view_len() > 0 {
            0
        } else {
            return None;
        };
        let row = m.all_items.get(m.item_at(view_idx))?;
        let acc = CompleteAcceptance {
            anchor: m.anchor,
            insert: row.insert.clone().unwrap_or_else(|| row.label.clone()),
            key: row.key,
            source_accept: row.source_accept,
        };
        self.menu = None;
        self.end_complete_manual_session();
        Some(acc)
    }

    /// Close the popup **only if it is a completion menu** — leaves an open
    /// `select` / picker untouched. A no-op when nothing (or a non-completion
    /// menu) is open.
    pub(crate) fn close_completion(&mut self) {
        if self.menu_kind() == Some(MenuKind::Complete) {
            self.menu = None;
            self.end_complete_manual_session();
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
    /// takes the context. The explorer / `btv.view` / quickfix buffers, and the
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
        let field = {
            let Some(menu) = self.menu.as_mut() else {
                return;
            };
            let p = menu.prompt.as_mut().unwrap();
            p.focused_mut().insert(c);
            // Typing ends a history walk: the line is yours again, so the next recall
            // starts from it rather than resuming a walk this edit invalidated.
            if let Some(h) = p.focused_history() {
                h.stop_browsing();
            }
            p.focus
        };
        self.on_prompt_changed(field);
    }

    /// Snapshot a closing **resumable** picker for `btv.picker.resume()` (`<leader>fr`)
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
        // A resume kicks NO run (the frozen window *is* the content), so the snapshot
        // must not carry the in-flight flag of a picker closed mid-search — the reopened
        // box would spin forever over rows nothing is looking for.
        snap.running = false;
        self.picker_snapshot = Some(snap);
        keys
    }

    /// Reopen the last resumable picker from its snapshot ([`snapshot_picker_for_resume`]
    /// (Self::snapshot_picker_for_resume)) — `btv.picker.resume()`. Restores the frozen
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
        // so snapshot it first for `btv.picker.resume()` — the live menu is gone after
        // `close_menu`. The window's keys ride [`Editor::picker_resume_keys`] to the
        // server, which tells Lua which item tables to keep for `confirm`.
        if matches!(
            action,
            "confirm"
                | "confirm_tab"
                | "confirm_split"
                | "confirm_vsplit"
                | "cancel"
                | "send_to_list"
        ) {
            self.picker_resume_keys = self.snapshot_picker_for_resume();
            // The filter lines as they stand at close, for Lua to fold into the
            // persisted history. Captured HERE, not from the last source run: a
            // dynamic source's re-run is debounced, so closing within the debounce
            // would otherwise record the line as it was a keystroke or two ago.
            self.picker_closed_filters = self
                .menu
                .as_ref()
                .and_then(|m| m.prompt.as_ref())
                .filter(|p| p.filterable)
                .map(|p| (p.include.text.clone(), p.exclude.text.clone()));
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
            // — the server hands the keys (and the live query) to Lua to build a named
            // list. The bemtvi port of telescope's send(-selected)-to-list.
            "send_to_list" => {
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
                // The live query names the per-search list (`<picker>:<query>`).
                let query = self
                    .menu
                    .as_ref()
                    .map(|m| m.match_query().to_string())
                    .unwrap_or_default();
                self.picker_sends.push((keys, query));
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

        // Navigation / preview / prompt-edit mutate the open menu; the prompt-edit ones
        // report **which field** changed so we can react after the borrow ends — a
        // query edit re-ranks locally, a filter edit re-runs the source.
        let mut unfilterable = false;
        let mut no_history = false;
        let query_changed = {
            let Some(menu) = self.menu.as_mut() else {
                return Ok(());
            };
            let last = menu.view_len().saturating_sub(1);
            let mut query_changed: Option<PromptField> = None;
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
                // A deletion is an edit like typing: it ends any history walk, so the
                // next recall starts from what is now on the line.
                "backspace" => {
                    let p = menu.prompt.as_mut().unwrap();
                    let field = p.focus;
                    query_changed = p.focused_mut().backspace().then_some(field);
                    if query_changed.is_some() {
                        if let Some(h) = p.focused_history() {
                            h.stop_browsing();
                        }
                    }
                }
                "delete" => {
                    let p = menu.prompt.as_mut().unwrap();
                    let field = p.focus;
                    query_changed = p.focused_mut().delete().then_some(field);
                    if query_changed.is_some() {
                        if let Some(h) = p.focused_history() {
                            h.stop_browsing();
                        }
                    }
                }
                "left" => menu.prompt.as_mut().unwrap().focused_mut().cursor_left(),
                "right" => menu.prompt.as_mut().unwrap().focused_mut().cursor_right(),
                "to_start" => menu.prompt.as_mut().unwrap().focused_mut().col = 0,
                "to_end" => {
                    let p = menu.prompt.as_mut().unwrap().focused_mut();
                    p.col = p.text.len();
                }
                // Reveal / hide the include-exclude rows, and step through the three
                // fields. On a source that never declared `filter = true` these are a
                // no-op with a word about why — the same shape as a preview gesture on
                // a preview-less picker, since the action is implemented and it is the
                // *source* that has nothing to filter.
                "toggle_filters" => {
                    let p = menu.prompt.as_mut().unwrap();
                    if p.filterable {
                        p.expanded = !p.expanded;
                        // Collapsing must not strand the caret on a row that is no
                        // longer drawn — typed text would vanish into an invisible box.
                        p.focus = if p.expanded {
                            PromptField::Include
                        } else {
                            PromptField::Query
                        };
                    } else {
                        unfilterable = true;
                    }
                }
                "next_field" => {
                    let p = menu.prompt.as_mut().unwrap();
                    if p.filterable {
                        // Cycling reveals the rows, so `next_field` alone is enough to
                        // reach the boxes without knowing about `toggle_filters`.
                        p.expanded = true;
                        p.focus = match p.focus {
                            PromptField::Query => PromptField::Include,
                            PromptField::Include => PromptField::Exclude,
                            PromptField::Exclude => PromptField::Query,
                        };
                    } else {
                        unfilterable = true;
                    }
                }
                // Recall a past line for the focused box. Only the filter boxes have
                // history — the query is not a glob line — so on the prompt this says
                // so rather than silently doing nothing.
                "history_prev" | "history_next" => {
                    let p = menu.prompt.as_mut().unwrap();
                    let current = p.focused().text.clone();
                    let older = action == "history_prev";
                    match p.focused_history() {
                        None => no_history = true,
                        Some(h) => {
                            let recalled = if older { h.older(&current) } else { h.newer() };
                            if let Some(text) = recalled {
                                p.set_focused(text);
                                query_changed = Some(p.focus);
                            }
                        }
                    }
                }
                other => return Err(format!("unknown picker action {other:?}")),
            }
            query_changed
        };

        if unfilterable {
            self.echo("this picker has no include/exclude filters".to_string());
        }
        if no_history {
            self.echo("history is per filter box — <C-g> to one first".to_string());
        }
        if let Some(field) = query_changed {
            self.on_prompt_changed(field);
        }
        Ok(())
    }

    // ── Mouse helpers for the input-grabbing menus (picker / select) ────────────
    // The core hit-tests a click/wheel on the open box back to a row (see
    // `mouse.rs`); these are the mouse equivalents of the navigation / confirm /
    // cancel keymap actions, dispatched by menu kind so the right `apply_*_action`
    // runs. They are deliberately thin — the behavior lives in the action handlers.

    /// Whether an **input-grabbing** menu (a picker or a promptless `btv.ui.select`)
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

    /// Route a string action to whichever list menu is open (picker or select),
    /// ignoring its result; a no-op when no list menu is open. The shared dispatch
    /// behind `menu_step`/`menu_confirm`/`menu_cancel` (the mouse/wheel entry points).
    fn dispatch_menu_action(&mut self, action: &str) {
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

    /// Move the highlight one row, non-wrapping (a wheel notch over the list) —
    /// routed to the open menu's own `next`/`prev` action by kind.
    pub(crate) fn menu_step(&mut self, down: bool) {
        self.dispatch_menu_action(if down { "next" } else { "prev" });
    }

    /// Confirm the highlighted row of an open picker / select (a click on the
    /// already-highlighted row), routed by kind — pushes the chosen key and closes.
    pub(crate) fn menu_confirm(&mut self) {
        self.dispatch_menu_action("confirm");
    }

    /// Cancel an open picker / select (a click off the box), routed by kind —
    /// pushes the cancel result (`None`) and closes, like `<Esc>` on the widget.
    pub(crate) fn menu_cancel(&mut self) {
        self.dispatch_menu_action("cancel");
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

    /// React to an edit of one of the picker's prompt lines.
    ///
    /// Which line was edited decides the work, because the two kinds of text mean
    /// different things to a source:
    ///
    /// * **The query** is what it has always been — a *dynamic* source bumps the
    ///   generation and emits a run onto [`Editor::picker_query_changes`] for the
    ///   server to forward; a *static* source just re-ranks the candidates it already
    ///   holds ([`Menu::refilter`]), no re-run.
    /// * **A filter pattern** (include / exclude) changes which paths *exist* for this
    ///   search, which no amount of local re-ranking can produce. It re-runs the
    ///   source — static sources included — so `rg` is re-spawned with the new `-g`
    ///   arguments and the walk legs re-enumerate.
    ///
    /// Either way the edit **keeps the current results displayed**: they are swapped
    /// out only when the new run's first result (or its completion) arrives
    /// ([`Editor::menu_push`] / [`Editor::menu_finish`]), so the list never flashes
    /// empty while a debounced search runs.
    fn on_prompt_changed(&mut self, field: PromptField) {
        let signal = {
            let Some(menu) = self.menu.as_mut() else {
                return;
            };
            let rerun = menu.dynamic || field != PromptField::Query;
            if rerun {
                menu.generation += 1;
                menu.running = true;
                Some(menu.picker_run())
            } else {
                menu.refilter();
                None
            }
        };
        if let Some(sig) = signal {
            self.picker_query_changes.push(sig);
        }
    }

    /// Drop the one-shot preview-scroll gesture after a frame has consumed it (called
    /// from [`Editor::view`], alongside `pending_scroll`).
    pub(crate) fn clear_preview_scroll(&mut self) {
        if let Some(menu) = self.menu.as_mut() {
            menu.preview_scroll = None;
        }
    }

    /// Project the open menu's **metadata** into [`MenuView`] — the highlighted row,
    /// the total visible count, the optional query line, placement, and size. The
    /// rows themselves are fetched windowed via [`Editor::menu_rows`] so a 100k-item
    /// picker never clones its whole list into a frame. `None` when closed.
    pub(crate) fn menu_view(&self) -> Option<MenuView> {
        self.menu.as_ref().map(|m| {
            let sel = m.selected_item();
            MenuView {
                selected: m.cursor,
                total: m.view_len(),
                placement: m.placement,
                query: m.prompt.as_ref().map(|p| p.query.text.clone()),
                // The caret belongs to whichever line has focus; `filters.focus` tells
                // the client which row to draw it on.
                query_cursor: m.prompt.as_ref().map_or(0, |p| p.focused().cursor_chars()),
                filters: m.prompt.as_ref().filter(|p| p.filterable).map(|p| {
                    crate::view::FilterView {
                        include: p.include.text.clone(),
                        exclude: p.exclude.text.clone(),
                        focus: p.focus,
                        expanded: p.expanded,
                        badge: p.badge(),
                    }
                }),
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
                status: m.status_text(),
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

    /// The actively-highlighted completion row's **inline** docs (`push { doc = … }`
    /// from a plugin async source) — the docs float renders this markdown directly,
    /// unlike an `lsp` row (server LSP cache) or a `resolve` row (server-fetched).
    /// `None` unless a completion menu is open with an active selection whose row
    /// carries an inline `doc`.
    pub fn complete_selected_doc(&self) -> Option<String> {
        let m = self.menu.as_ref()?;
        if m.kind != MenuKind::Complete {
            return None;
        }
        m.selected_item().and_then(|i| i.doc.clone())
    }

    /// The actively-highlighted **cmdline wildmenu** row's inline docs (the catalog
    /// candidate's `doc` — synopsis + help), which the cmdline docs float renders as
    /// plain text. `None` unless a [`MenuKind::Cmdline`] menu is open with an active
    /// selection whose row carries a `doc` (the popup is noselect until navigated).
    pub fn cmdline_selected_doc(&self) -> Option<String> {
        let m = self.menu.as_ref()?;
        if m.kind != MenuKind::Cmdline {
            return None;
        }
        m.selected_item().and_then(|i| i.doc.clone())
    }

    /// The actively-highlighted completion row's **lazy-docs resolve handle** — the
    /// id the server passes to `btv._complete_resolve` to fetch a plugin row's docs
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
    /// row's label and its matched-character spans (empty in passthrough).
    /// **Borrows** the requested window from the live menu — O(count) pointers,
    /// independent of the list size, and no per-frame clone of the whole list (the
    /// cursor-placement arm projects the entire view; the server consumes the
    /// borrow into RPC values before its next `&mut self` call). Empty when closed
    /// or out of range.
    pub fn menu_rows(&self, start: usize, count: usize) -> Vec<(&str, &[Range<usize>])> {
        let Some(m) = &self.menu else {
            return Vec::new();
        };
        let end = start.saturating_add(count).min(m.view_len());
        (start..end)
            .map(|i| {
                let item = &m.all_items[m.item_at(i)];
                let spans: &[Range<usize>] = if m.filtered.is_some() {
                    &m.match_spans[i]
                } else {
                    &[]
                };
                (item.label.as_str(), spans)
            })
            .collect()
    }

    /// The per-row **kind** label for the visible window `[start, start + count)`,
    /// parallel to [`Editor::menu_rows`] — the short category the client right-aligns
    /// on each completion row (`"Snippet"`, `"Function"`, …). `None` for a row whose
    /// source declares no kind (`buffer` words, `select` / picker rows); empty when no
    /// menu is open. Borrows from the live menu like [`Editor::menu_rows`].
    pub fn menu_kinds_window(&self, start: usize, count: usize) -> Vec<Option<&str>> {
        let Some(m) = &self.menu else {
            return Vec::new();
        };
        let end = start.saturating_add(count).min(m.view_len());
        (start..end)
            .map(|i| m.all_items[m.item_at(i)].kind.as_deref())
            .collect()
    }

    /// The per-row two-column **layout** hint for the visible window
    /// `[start, start + count)`, parallel to [`Editor::menu_rows`] — the head/focus
    /// char offsets a client fits a `path:line:col: <line>` row by. `None` for a row
    /// whose source declared none (every non-grep-shaped row); empty when no menu is
    /// open.
    pub fn menu_layout_window(&self, start: usize, count: usize) -> Vec<Option<RowLayout>> {
        let Some(m) = self.menu.as_ref() else {
            return Vec::new();
        };
        let end = start.saturating_add(count).min(m.view_len());
        (start..end)
            .map(|i| m.all_items[m.item_at(i)].layout)
            .collect()
    }

    /// The per-row **highlight group** for the visible window `[start, start + count)`,
    /// parallel to [`Editor::menu_rows`] — the group name the source painted the row
    /// with (`ctx.push { hl = … }`), which the server resolves against the live
    /// colorscheme. `None` for a row that declared none (every non-classified row);
    /// empty when no menu is open. Borrows from the live menu like
    /// [`Editor::menu_rows`].
    pub fn menu_hl_window(&self, start: usize, count: usize) -> Vec<Option<&str>> {
        let Some(m) = self.menu.as_ref() else {
            return Vec::new();
        };
        let end = start.saturating_add(count).min(m.view_len());
        (start..end)
            .map(|i| m.all_items[m.item_at(i)].hl.as_deref())
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
    pub fn menu_geom<'a>(&'a self, m: &MenuView, metrics: MenuMetrics) -> MenuGeom<'a> {
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
        // list — and, with the filter boxes revealed, the include/exclude rows above
        // that separator; `btv.ui.select` carries none of it. All of them count toward
        // the box height (`chrome`), their text toward the width.
        let prompt_rows = usize::from(m.query.is_some());
        let chrome = prompt_rows + m.filter_rows() + prompt_rows;
        let query_w = m.query.as_ref().map_or(0, |q| q.chars().count() + 1);

        // The box rect, the scroll offset of the first visible row, the windowed
        // rows themselves, and the highlighted row rebased into that window — only
        // the visible slice is materialized, so a 100k-item picker costs the same
        // per frame as a 10-item one.
        let (row, col, width, height, start, rows, selected, kind_col) = match m.placement {
            MenuPlacement::Cursor => {
                // `select` is small — project the whole list (no scrolling subtlety)
                // and let the client place the cursor; keeps the four-tier flip exact.
                let rows = self.menu_rows(0, m.total);
                let count = (rows.len() + prompt_rows).min(MAX_H);
                // Reserve a single ALIGNED kind column: the box holds the widest label,
                // a 1-col gap, then the widest kind, so every `Snippet`/`Function`/`Text`
                // label lines up in one column (not each floating flush-right on its own
                // row). A kind-less menu (`select`) adds nothing — its kinds are all `None`.
                let kinds = self.menu_kinds_window(0, m.total);
                let max_label = rows
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(0);
                let max_kind = kinds
                    .iter()
                    .flatten()
                    .map(|k| k.chars().count())
                    .max()
                    .unwrap_or(0);
                let rows_w = if max_kind > 0 {
                    max_label + 1 + max_kind
                } else {
                    max_label
                };
                // ...then cap the rows at `'pummaxwidth'` (`0` = no maximum). The box is
                // sized to its WIDEST row, so uncapped a single outlier — a generated
                // identifier, a word scanned out of a minified line — stretches the popup
                // across the whole window and every other row is read against a box built
                // for the one it isn't. Capped, that row elides with a trailing `…` (the
                // client's `elide_keep_tail`) and the kind column slides in beside it.
                //
                // The **prompt** is exempt: `query_w` is applied after the cap, since a
                // `btv.ui.select` title is one line the user is meant to read whole, not a
                // row the popup is being sized *by*.
                let content_w = match self.options.pummaxwidth {
                    0 => rows_w,
                    cap => rows_w.min(cap),
                }
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
                // The client draws a one-cell border on each side of this content, and
                // the top-borderless completion popup additionally shifts one cell LEFT
                // so its left border doesn't sit on the word (`left_shift` in the
                // renderers) — recovering a column, unless the anchor is already at the
                // text area's left edge and the shift saturates. Reserve that chrome so
                // the whole bordered box fits: without it the content claimed the full
                // remaining width, the client clamped the box to the window edge, and the
                // rightmost column — the tail of the aligned kind label — fell off screen.
                let left_shift = usize::from(m.completion).min(anchor_col);
                let max_w = (text_width + left_shift)
                    .saturating_sub(anchor_col + 2)
                    .max(1);
                let width = content_w.min(max_w);
                // Where every kind label starts — just past the widest label, so they
                // align into one column. Clamped so the kinds still fit if the box was
                // capped at the screen edge (labels then truncate into the reserved gap);
                // `None` for a kind-less popup or a box too narrow to hold any kind.
                let kind_col = (max_kind > 0 && width > max_kind)
                    .then(|| (max_label + 1).min(width - max_kind));
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
                (
                    row, anchor_col, width, height, start, rows, m.selected, kind_col,
                )
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
                (
                    row,
                    col,
                    width,
                    height,
                    start,
                    rows,
                    m.selected - start,
                    None,
                )
            }
            MenuPlacement::Cmdline => {
                // The `btv.cmdline_complete` wildmenu: a bordered list floating just
                // above the command line (the bottom of the text area). The whole
                // (small) list is projected — like `select`, no scroll subtlety.
                // `anchor_offset` is the display width of the line before the token
                // (0 for the leading command name); the prompt the client paints ahead
                // of the line (`:` for ex, a `btv.ui.input` label like `dap> `) shifts
                // the token that many cells right, so the box left-aligns under it.
                let rows = self.menu_rows(0, m.total);
                let count = rows.len().min(MAX_H);
                let content_w = rows
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(1)
                    .max(1);
                // `col` is the BOX's left edge (its border), not the token's column:
                // every client draws the border at `col` and the first list character at
                // `col + 1`. So sit one cell before the token — then the candidates line
                // up *under* the text they complete instead of one cell right of it,
                // which is the same alignment the cursor-anchored completion popup gets
                // from its client-side one-cell shift. A token at column 0 (a promptless
                // line) has nowhere to go and keeps the edge, exactly as that popup does.
                let anchor = m.anchor_offset + self.cmdline_prompt_width();
                let col = anchor.min(text_width.saturating_sub(1)).saturating_sub(1);
                let max_w = text_width.saturating_sub(col).max(1);
                let width = content_w.min(max_w);
                // A full bordered box (2 rows of chrome) sitting on the last text rows,
                // so its bottom border abuts the command line below.
                const VCHROME: usize = 2;
                let height = count.min(text_height.saturating_sub(VCHROME).max(1));
                let row = text_height.saturating_sub(height + VCHROME);
                let start = menu_start(m.selected_active.then_some(m.selected), height);
                (row, col, width, height, start, rows, m.selected, None)
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
            kind_col,
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
/// the focused window's text-area size (its width minus every left gutter, and its
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
pub struct MenuGeom<'a> {
    pub row: usize,
    pub col: usize,
    pub width: usize,
    pub height: usize,
    /// Highlighted row, window-relative (add `start` for the absolute view index).
    pub selected: usize,
    /// Scroll offset of the first visible row (`0` for the whole-list placements).
    pub start: usize,
    /// The visible rows `[start, start + rows.len())`: label + matched-char spans,
    /// borrowed from the editor's live menu — the projection and the mouse hit-test
    /// read them without cloning the list. The server must consume the borrow (into
    /// RPC values) before its next `&mut self` call.
    pub rows: Vec<(&'a str, &'a [Range<usize>])>,
    /// The content column where the aligned **kind** labels start (just past the widest
    /// label), for a completion popup that carries kinds. `None` for a kind-less popup,
    /// a `select` / picker / cmdline menu, or a box too narrow to hold a kind column.
    pub kind_col: Option<usize>,
}
