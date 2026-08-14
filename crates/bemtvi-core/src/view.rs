//! The renderable view of the editor: semantic regions, not a baked grid.
//!
//! The core no longer lays out a flat screen (status/command lines are not
//! painted into text rows). Instead it produces a [`View`] describing *what* to
//! show in each region, and the client arranges those regions with its own
//! widgets. This keeps layout and styling a UI concern while the core stays the
//! single source of truth for content, scrolling, and cursor placement.
//!
//! Columns are byte offsets (ropey's native metric and vim's column model);
//! `cursor_screen_col` additionally carries the cursor's screen-cell column,
//! accounting for wide characters and tabs.

use crate::buffer::Buffer;
use crate::editor::{
    signcol_cells, BorderStyle, BufferId, Cursor, Editor, Fold, MenuPlacement, TabLabel, WindowId,
    WindowLayout,
};
use crate::extmark::VirtChunk;
use crate::mode::Mode;
use crate::statusline::StatuslineCtx;
use crate::unicode;

/// A scroll gesture for the client to animate. Self-contained and **screen-row
/// based**: it carries an over-scanned band of [`RenderRow`]s — every screen row
/// the slide reveals, each with its own text and overlays — and the slide is
/// expressed as screen-row offsets *into that band*. The client interpolates
/// `from_row`→`to_row` against its local clock and slices `rows[off .. off +
/// height]` per frame; the main `View` fields stay the *destination* viewport for
/// clients that don't animate.
///
/// Because the band is screen rows (not buffer lines), interleaved `virt_lines`
/// and (later) wrapped continuation rows slide correctly — they are simply more
/// rows in the band, and the offset advances one screen row at a time regardless
/// of how many screen rows a buffer line expands into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnim {
    /// Screen-row offset into `rows` of the viewport's top row at slide start /
    /// end. `rows[0]` is the topmost viewport line's first screen row, so these
    /// are `0` and the slide's screen-row distance (in either order).
    pub from_row: usize,
    pub to_row: usize,
    /// Screen-row offset into `rows` of the cursor's row at slide start / end, so
    /// the cursor (and the relative-number gutter) tracks the moving text.
    pub from_cursor_row: usize,
    pub to_cursor_row: usize,
    pub duration_ms: u64,
    /// The over-scanned screen-row layout the slide reveals — the same
    /// [`RenderRow`] the settled frame uses, just taller. Carries each row's text,
    /// `virt_lines` content, and every overlay (selection / secondary selection /
    /// search / incsearch), so the band is projected exactly like a window.
    pub rows: Vec<RenderRow>,
    /// Orientation of the visual selection sliding with the band, used by the
    /// client to clip the highlight's moving edge to the interpolated cursor:
    /// `Some(true)` when the anchor is at/above the cursor (selection extends
    /// downward, so rows below the cursor aren't selected yet), `Some(false)`
    /// when it extends upward, `None` when no visual selection is sliding. The
    /// band's `selection` already covers the **maximal** extent the slide touches
    /// (anchor → the scroll endpoint furthest from the anchor), so the client can
    /// grow *and* shrink the highlight to the interpolated cursor.
    pub sel_extends_down: Option<bool>,
}

/// What a single screen row of a window's text body *is*. A buffer line expands
/// into one or more screen rows (its interleaved `virt_lines`, then its text
/// row); rows past the end of the buffer are `~` fillers. Tagging the kind keeps
/// the scroll path from having to know about any one feature — a new row variety
/// (folds, diff filler, wrapped continuation) is just another arm here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A display row of a real buffer line. `line` is the 0-based buffer line;
    /// `start_col` is the screen column where this row's text begins within the
    /// line — `0` for the first/only row, and `> 0` for a soft-wrap continuation
    /// row (`'wrap'`). A line that fits (or `nowrap`) has a single row at
    /// `start_col 0`. Overlays on this row are clipped to its `[start_col, …)`
    /// segment and rebased to row-local screen columns.
    Line { line: usize, start_col: usize },
    /// A virtual line (extmark `virt_lines`) interleaved above/below its anchor
    /// buffer line; its chunk run lives in [`RenderRow::virt_line`].
    VirtLine,
    /// A **closed fold**: one placeholder row standing in for the whole folded
    /// range. `line` is the fold's first (0-based) buffer line — the row shows its
    /// number and the fold's placeholder text ([`RenderRow::text`]) — and `count`
    /// is how many buffer lines the fold collapses. The lines after `line` in the
    /// range are simply absent from the row list.
    Fold { line: usize, count: usize },
    /// Filler past the end of the buffer — vim's `~`.
    Filler,
}

/// One screen row of a window's text body: the single projection primitive both
/// the settled frame and the scroll band are built from. The row carries its own
/// text **and every overlay the client paints on it** (selection, search,
/// incsearch, virtual-line content), so projecting a window — settled or
/// mid-slide — is just "lay out these rows", with no per-feature special-casing
/// in the scroll path. Per-row column spans are screen columns, matching
/// [`WindowView`]'s arrays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRow {
    /// What this row is (real line / virtual line / filler).
    pub kind: RowKind,
    /// Text to paint: the buffer line for [`RowKind::Line`], `""` for a virtual
    /// line, `"~"` for a filler.
    pub text: String,
    /// The extmark `virt_lines` chunk run when [`kind`](RenderRow::kind) is
    /// [`RowKind::VirtLine`], else `None`. `virt_line.is_some()` is exactly what
    /// distinguishes a virtual row from a `~` filler (both carry no number).
    pub virt_line: Option<Vec<VirtChunk>>,
    /// The primary visual selection's half-open screen-column span on this row,
    /// or `None`. `end` may exceed the text width to mark a selected newline or to
    /// fill a linewise selection to the window edge.
    pub selection: Option<(usize, usize)>,
    /// Every **secondary** multi-cursor's selection span on this row (the
    /// primary's is in [`selection`](RenderRow::selection)); empty off a secondary
    /// selection or outside visual mode.
    pub secondary_selection: Vec<(usize, usize)>,
    /// Every `hlsearch`/`Search` match's span on this row; empty when none.
    pub search: Vec<(usize, usize)>,
    /// The live `incsearch` preview match on this row, or `None`.
    pub incsearch: Option<(usize, usize)>,
    /// Exclusive end column of this row's soft-wrap segment in **full-line**
    /// screen-column space (`usize::MAX` for an unwrapped row or a wrapped line's
    /// last segment). With `start_col` (in [`kind`](RenderRow::kind)) this bounds the
    /// segment a per-row server overlay projection (treesitter / diagnostics / inlay /
    /// extmark virt_text) clips its full-line spans to before rebasing them to
    /// row-local columns.
    pub seg_end_col: usize,
    /// The `'breakindent'`/`'showbreak'` prefix width baked onto this row's text
    /// (`0` on a first/only row and under `nowrap`). A projection adds it when
    /// rebasing a clipped span, so overlays line up past the prefix.
    pub indent: usize,
}

impl RenderRow {
    /// A structural row with no overlay state: `selection`/`secondary_selection`/
    /// `search`/`incsearch` all start empty (the overlay passes fill them in later).
    /// The shared constructor for every row [`row_skeleton`] emits — virtual lines,
    /// `~` fillers, closed folds, and the wrapped / unwrapped text segments.
    fn structural(
        kind: RowKind,
        text: String,
        virt_line: Option<Vec<VirtChunk>>,
        seg_end_col: usize,
        indent: usize,
    ) -> RenderRow {
        RenderRow {
            kind,
            text,
            virt_line,
            selection: None,
            secondary_selection: Vec::new(),
            search: Vec::new(),
            incsearch: None,
            seg_end_col,
            indent,
        }
    }

    /// 1-based buffer line number for the number column — `Some` for a real
    /// [`RowKind::Line`] row (virtual / filler rows show no number). A soft-wrap
    /// continuation row repeats its line's number (a v1 cosmetic: vim blanks the
    /// gutter on continuations, but repeating it keeps the wire identical — a
    /// continuation is an ordinary numbered text row to every client).
    pub fn number(&self) -> Option<usize> {
        match self.kind {
            // A closed fold shows its first line's number, like vim.
            RowKind::Line { line, .. } | RowKind::Fold { line, .. } => Some(line + 1),
            RowKind::VirtLine | RowKind::Filler => None,
        }
    }

    /// 0-based buffer line index when this row renders a real buffer line — or a
    /// closed fold's first line, so the cursor (parked on the fold start) maps to
    /// this row.
    pub fn line(&self) -> Option<usize> {
        match self.kind {
            RowKind::Line { line, .. } | RowKind::Fold { line, .. } => Some(line),
            RowKind::VirtLine | RowKind::Filler => None,
        }
    }

    /// The full-line screen column this row's text begins at — its wrap segment's
    /// `start_col` (`0` for a first/only row, a virtual line, a filler, or a fold).
    pub fn start_col(&self) -> usize {
        match self.kind {
            RowKind::Line { start_col, .. } => start_col,
            RowKind::VirtLine | RowKind::Filler | RowKind::Fold { .. } => 0,
        }
    }

    /// Whether this row is the **last** display row of its buffer line (the only row
    /// under `nowrap`/no-wrap-needed). Its segment runs to end-of-line, so end-of-line
    /// overlays (eol virt_text, the diagnostic message) belong here.
    pub fn is_last_segment(&self) -> bool {
        self.seg_end_col == usize::MAX
    }

    /// Whether this is a soft-wrap *continuation* row — a second-or-later display
    /// row of a buffer line (`start_col > 0`). The number column carries the line's
    /// number on every row of the line (so [`number`](RenderRow::number) keeps the
    /// row→line mapping intact for highlights / diagnostics), but the client blanks
    /// the gutter on continuations, matching vim: the number shows on the line's
    /// first row only.
    pub fn is_continuation(&self) -> bool {
        matches!(self.kind, RowKind::Line { start_col, .. } if start_col > 0)
    }
}

/// The renderable form of the list-less **content float** (`btv.ui.float`; the LSP
/// hover / signature-help surface) — the sibling of [`MenuView`] with no list and
/// no selection, just content lines, an optional title, a border, and where it
/// floats. The server projects the on-screen geometry (`project_content_float`)
/// from this plus the cursor / editor size. `None` in [`View::content_float`] when
/// none is open.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentFloatView {
    /// The content lines to render, in order (non-empty — an empty float never
    /// opens), each a run of styled [`VirtChunk`](crate::extmark::VirtChunk)s (a
    /// plain caller is one unstyled chunk per line). The server windows them to the
    /// available height and resolves each chunk's `hl_group` to a wire style id.
    pub lines: Vec<Vec<crate::extmark::VirtChunk>>,
    /// An optional title drawn on the top border (`None` when untitled).
    pub title: Option<String>,
    /// The border style the client draws around the content.
    pub border: crate::editor::BorderStyle,
    /// Whether the float anchors at the cursor (hover / signature help) or centers
    /// over the editor (an `btv.ui.float` caller that asked for `relative="editor"`).
    pub placement: crate::editor::MenuPlacement,
}

/// The renderable form of the floating selectable-list [`Menu`](crate::editor):
/// the visible (filtered) labels, the highlighted row, the optional picker prompt,
/// per-row match highlighting, and where it floats. The server projects the
/// on-screen geometry (anchor + size) from this plus the focused window, the same
/// way it places the completion popup. `None` in [`View::menu`] when no menu is
/// open.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuView {
    /// The highlighted row, as a 0-based index into the **whole** view (`0` when the
    /// view is empty). The server windows the rows it sends and rebases this.
    pub selected: usize,
    /// Total number of rows in the view (after fuzzy filtering) — may be 100k+. The
    /// server uses it for geometry and the scroll window; the rows themselves are
    /// fetched windowed via [`Editor::menu_rows`](crate::Editor::menu_rows).
    pub total: usize,
    /// Whether the menu floats under the cursor or centered over the editor.
    pub placement: MenuPlacement,
    /// The picker prompt's query text — `Some` for a `btv.picker`, `None` for a
    /// promptless `btv.ui.select`. Presence tells the client to draw a prompt line.
    pub query: Option<String>,
    /// The picker prompt's text-cursor position, as a count of chars before it — the
    /// caret column within the **focused** line ([`FilterView::focus`] says which one
    /// that is; without filters it is always the query). `0` for a promptless
    /// `btv.ui.select`.
    pub query_cursor: usize,
    /// The include/exclude glob filter boxes, when this picker's source declared
    /// `filter = true`. `None` for a `select` / completion / cmdline menu and for a
    /// picker whose source has nothing to filter (`keymaps`, `marks`, …) — the client
    /// then draws exactly the single prompt line it always has.
    pub filters: Option<FilterView>,
    /// Where the picker prompt sits relative to the list
    /// ([`PromptPos`](crate::editor::PromptPos)). `Top` (the default) for a
    /// promptless `btv.ui.select`; only meaningful when `query` is `Some`.
    pub prompt_pos: crate::editor::PromptPos,
    /// Whether this picker carries a preview pane at all (the source declared a
    /// `preview` kind). The server reserves the preview column whenever this is set —
    /// even on a row with no target, which shows a "no preview" placeholder rather
    /// than collapsing the layout. `false` for a `select` / preview-less picker.
    pub has_preview: bool,
    /// The **selected** row's preview target ([`PreviewTarget`](crate::editor::PreviewTarget)),
    /// when this picker carries a preview pane and the row supplied one. `None` for a
    /// `select` / preview-less picker, or a row with no path. The server reads +
    /// renders the target into the pane; this only names *what* to show.
    pub preview: Option<crate::editor::PreviewTarget>,
    /// A one-shot preview-scroll gesture ([`PreviewScroll`](crate::editor::PreviewScroll))
    /// from the keystroke that produced this view — `<C-d>`/`<C-u>`/`<C-f>`/`<C-b>` while
    /// a preview picker is open. The server folds it into its persistent preview scroll
    /// offset (resolving the line delta from the live pane height, clamping to the file);
    /// `None` on every frame that doesn't carry the gesture. Always `None` for a
    /// preview-less picker / `btv.ui.select`.
    pub preview_scroll: Option<crate::editor::PreviewScroll>,
    /// The picker box's fixed width / height ([`Extent`](crate::editor::Extent),
    /// `None` ⇒ the picker default), resolved against the viewport by the server at
    /// projection time. Both `None` for a content-anchored `btv.ui.select`.
    pub width: Option<crate::editor::Extent>,
    pub height: Option<crate::editor::Extent>,
    /// Where the box aligns within the editor area ([`Align`](crate::editor::Align)),
    /// inset by [`margin`](Self::margin). `None` ⇒ centered (the historical picker
    /// placement). Only honored for `Editor`-placement menus.
    pub align: Option<crate::editor::Align>,
    /// Edge inset (cells) for an aligned picker box; ignored when `align` is `None`.
    pub margin: crate::editor::Margin,
    /// Screen columns to shift a `Cursor`-placed completion popup **left** of the
    /// cursor, so the list anchors under the start of the word being completed
    /// rather than under the caret — the display width of the typed prefix. `0`
    /// for a `select` / picker (anchored at the cursor / centered).
    pub anchor_offset: usize,
    /// Whether this is the insert-mode completion popup (vs a `select` / picker).
    /// The server projects it to sit *flush* with the line below the cursor — no
    /// top border, and shifted one cell left so the left border doesn't push the
    /// list off the word it completes. `false` for `select` / picker.
    pub completion: bool,
    /// Whether [`selected`](Self::selected) is an **active** selection the client
    /// highlights. Always `true` for a `select` / picker. `false` for a freshly
    /// opened completion popup (noselect — no row highlighted until the user
    /// navigates), so an auto-open never makes `<CR>` accept a row.
    pub selected_active: bool,
    /// Whether this completion popup carries a **docs sidebar** (the widget-spec
    /// `preview = "markdown"` kind, Phase 4-D) — a float beside the list rendering
    /// the selected item's documentation. `false` for a `select` / picker (they use
    /// the file [`preview`](Self::preview) pane) and for a completion config with
    /// docs disabled. The server fills the content from its LSP item cache keyed by
    /// [`selected_key`](Self::selected_key) / [`selected_source_accept`](Self::selected_source_accept).
    pub docs: bool,
    /// The actively-selected row's source `key` (its index into the source's item
    /// list — for the `lsp` source, the position in the server's cached LSP items).
    /// `None` when nothing is actively selected (noselect) or the view is empty, so
    /// the docs sidebar stays hidden until a row is chosen. Only meaningful when
    /// [`docs`](Self::docs) is set.
    pub selected_key: Option<usize>,
    /// Whether the actively-selected row's accept is delegated to its source
    /// (`true` for an `lsp` row, `false` for a native `buffer` row) — the server
    /// shows docs only for delegated (`lsp`) rows, the ones whose cache it holds.
    /// `false` when nothing is selected. Only meaningful when [`docs`](Self::docs).
    pub selected_source_accept: bool,
    /// The actively-selected row's **inline** docs, when a plugin async source
    /// attached one to the candidate (`push { doc = … }`). The server renders this
    /// directly beside the popup — unlike an `lsp` row, whose docs it fetches from
    /// its own item cache. `None` for a `buffer` / `lsp` / noselect row. Only
    /// meaningful when [`docs`](Self::docs) is set.
    pub selected_doc: Option<String>,
    /// The actively-selected row's **lazy-docs resolve handle** (a plugin async row
    /// with a `resolve` callback but no inline [`selected_doc`](Self::selected_doc)):
    /// the server asks Lua to resolve it (`btv._complete_resolve`) off the input path
    /// and caches the docs for the sidebar. `None` for an inline-doc / `buffer` /
    /// `lsp` / noselect row. Only meaningful when [`docs`](Self::docs). Phase 4-E.
    pub selected_resolve: Option<u64>,
    /// The picker box's optional title (`btv.picker.open(name, { title = … })`),
    /// rendered on the top border. `None` for the wildmenu / completion / select.
    pub title: Option<String>,
}

/// A picker's include / exclude glob filter boxes — VSCode's "files to include" and
/// "files to exclude", projected for the client to paint.
///
/// The two lines are the raw comma-separated text the user typed; the client renders
/// them verbatim and never has to know what a pattern is. When
/// [`expanded`](Self::expanded) they are drawn as two rows between the prompt and the
/// list; when collapsed the picker keeps its original single-line shape and
/// [`badge`](Self::badge) — already composed — is the only trace, so an active filter
/// is never invisible.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterView {
    /// The "files to include" line. Empty ⇒ every path is a candidate.
    pub include: String,
    /// The "files to exclude" line. Empty ⇒ nothing is filtered out.
    pub exclude: String,
    /// Which of the three lines has the caret ([`query_cursor`](MenuView::query_cursor)
    /// is its column), so the client draws it on the right row.
    pub focus: crate::editor::PromptField,
    /// Whether the two rows are drawn. They still filter when collapsed.
    pub expanded: bool,
    /// The collapsed-state indicator (`[+2 -1]` — two include, one exclude patterns),
    /// composed core-side so the clients cannot disagree on the count. `None` when
    /// expanded (the rows say it) or when nothing is filtering.
    pub badge: Option<String>,
}

impl MenuView {
    /// How many rows the include/exclude boxes occupy — `2` when revealed, `0`
    /// otherwise. **The single definition of that budget**: the core's box geometry,
    /// the mouse hit-test and all three clients derive their layout from this, and any
    /// one of them counting rows for itself would drift the list (and every click on
    /// it) off by the difference.
    pub fn filter_rows(&self) -> usize {
        match &self.filters {
            Some(f) if f.expanded => 2,
            _ => 0,
        }
    }
}

/// A rectangle in screen cells, relative to the **windows area** (the region the
/// window tree lays out into — the frame minus the global bottom panel and
/// command line). The client offsets it by that area's origin when painting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Which screen region a window (or separator) belongs to: the main editor area,
/// one of the four permanent docks, or the whole screen. A window's
/// `rect`/separator coordinates are relative to its region's own origin (each
/// region lays out at `(0, 0)`); the client maps the region to its absolute screen
/// origin using the [`View`]'s dock band sizes. `Main` is the default and the only
/// region when no dock is open, so a dock-free session renders exactly as before.
///
/// `Screen` is the odd one out: it is not a layout band but the whole **windows
/// area** (the frame minus the command line), the space an `editor`-relative float
/// positions against. Such a float is owned by some layer's tree (whichever had
/// focus when it opened) but must be free to span the dock bands, so it is
/// projected in screen cells and the client offsets it by the windows-area origin
/// — never a region's. Only floats ever carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowRegion {
    #[default]
    Main,
    DockLeft,
    DockRight,
    DockTop,
    DockBottom,
    Screen,
}

/// A split border between sibling windows: a vertical `│` run (between the
/// columns of a vertical split) or a horizontal `─` run (between the rows of a
/// horizontal split), anchored at `(x, y)` in its [`region`](Separator::region)'s
/// cells and `length` cells long. The core computes these from the layout tree;
/// the client only paints them (offsetting by the region origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Separator {
    pub vertical: bool,
    pub x: usize,
    pub y: usize,
    pub length: usize,
    /// The region this separator's `(x, y)` is relative to.
    pub region: WindowRegion,
}

/// One window's renderable content: its screen `rect`, whether it holds focus,
/// and every field that is per-window (text, cursor, selection, search, gutter,
/// status-line data, and any scroll gesture). The client paints each window at
/// its `rect` with its own gutter, text body, and a status line on its bottom
/// row; the terminal cursor is drawn only in the `focused` window. With a single
/// window this is the whole text area — identical to the pre-windows view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    /// This window's stable id (the handle `nvim_list_wins` /
    /// `nvim_get_current_win` report). The server keys per-window projected state
    /// by it — the `btv.statusline` custom-segment cache, rendered per window.
    pub id: WindowId,
    /// Where this window sits within its [`region`](WindowView::region) (origin
    /// `(0, 0)` per region; the client offsets by the region's screen origin).
    pub rect: ViewRect,
    /// Which screen region (main area or a dock) this window belongs to.
    pub region: WindowRegion,
    /// The buffer this window shows. The server projects this buffer's syntax /
    /// diagnostics slice for the window (two windows onto the same buffer share
    /// one `SyntaxState`, each slicing its own `(top, height)`).
    pub buffer: BufferId,
    /// Whether this window holds focus (the terminal cursor is drawn here).
    pub focused: bool,
    /// The window's text body as a flat list of screen rows ([`RenderRow`]) — one
    /// per visible row (`rect.height - 1` rows, the last row being its status
    /// line). Each row carries its text plus every overlay painted on it
    /// (selection, search, virtual-line content); a buffer line expands into its
    /// interleaved `virt_lines` rows then its text row, and rows past the buffer
    /// are `~` fillers. This is the single source of truth the server projects the
    /// per-row wire arrays from, and the same shape the scroll band slides.
    pub rows: Vec<RenderRow>,
    /// Cursor row relative to the top of this window's text body.
    pub cursor_row: usize,
    /// First visible screen column (horizontal scroll offset) under `nowrap`. `0`
    /// unless a long line scrolled the viewport right. The client drops this many
    /// leading screen cells from each row and shifts the cursor and every span
    /// left by it; the number gutter is *not* offset.
    pub leftcol: usize,
    /// Cursor byte/column offset within its line (for the ruler and
    /// `nvim_win_get_cursor`).
    pub cursor_col: usize,
    /// Cursor's screen-cell column on its line (wide-char and tab aware), for
    /// placing the terminal cursor.
    pub cursor_screen_col: usize,
    /// Display width (screen cells) of the grapheme under the cursor — `1` for an
    /// ordinary char or at end-of-line, `2` for a wide CJK/emoji glyph or a `^X`
    /// caret token, `4` for a `<xx>` hex token, a tab's width for a tab. A block
    /// cursor envelops this many cells so a multi-cell token is fully covered.
    pub cursor_width: usize,
    /// Secondary (multi-)cursors visible in this window, each as `(row, screen
    /// col)` relative to the top of the text body — the primary cursor is carried
    /// separately by [`cursor_row`]/[`cursor_screen_col`]. Empty for an
    /// unfocused window or when no multi-cursors are active.
    ///
    /// [`cursor_row`]: WindowView::cursor_row
    /// [`cursor_screen_col`]: WindowView::cursor_screen_col
    pub secondary_cursors: Vec<(usize, usize)>,
    /// File name for this window's status line (`"[No Name]"` when unset).
    pub file_name: String,
    /// The buffer's **effective** treesitter filetype/language — the override
    /// (`:set filetype=…` / `btv.bo.filetype`) when set, otherwise the
    /// extension-derived language, otherwise empty. The server highlights
    /// in-process so it ignores this, but the serverless web build picks its
    /// front-end grammar from it (preferring it over the file extension), which
    /// is how `:set ft=…` highlights a buffer the extension table misses.
    pub filetype: String,
    /// The fenced code blocks of a rendered-markdown doc float (LSP hover / signature
    /// help), as row spans into this window's own lines plus the fence's language.
    /// Empty for every ordinary window.
    ///
    /// The float's markdown is rendered server-side into STRIPPED lines — the ```lang
    /// fences are consumed, and the buffer is left untyped so nothing repaints the
    /// stripped text — so neither the text nor `filetype` still says "these rows are
    /// python". Native clients ignore this (they paint the server's highlight spans);
    /// the serverless web build highlights front-end and needs the structure to colour
    /// a hover's signature, which is the part of a hover worth colouring.
    pub code_blocks: Vec<crate::markdown::MdCode>,
    /// Whether this window's buffer has no file path yet (a fresh `[No Name]`
    /// buffer), so a write needs a target. Sent to clients as an explicit flag so
    /// a GUI can route a bare `:w` to its save dialog without matching `file_name`.
    pub unnamed: bool,
    pub modified: bool,
    /// 1-based cursor line, for this window's status-line ruler.
    pub cursor_line: usize,
    /// Present only on a redraw caused by a scroll command that moved this
    /// window's viewport; carries the data a client needs to animate the slide.
    pub scroll: Option<ScrollAnim>,
    /// `:set number` — show the absolute line number.
    pub number: bool,
    /// `:set relativenumber` — show numbers relative to the cursor line.
    pub relativenumber: bool,
    /// `:set cursorline` — highlight the screen line the cursor sits on. The
    /// client paints the cursor row's background with the `CursorLine` group; the
    /// cursor's screen row is [`cursor_row`](WindowView::cursor_row).
    pub cursorline: bool,
    /// `'colorcolumn'` — the 1-based text columns to highlight with the
    /// `ColorColumn` group (a vertical ruler down the text body), resolved from the
    /// window option and sorted ascending. Empty (no ruler) by default. The client
    /// paints each column at screen cell `text_origin + (col - 1) - leftcol` when
    /// that lands within the text area, across every text row.
    pub colorcolumn: Vec<usize>,
    /// Width in cells of the number column (`0` when both options are off).
    pub number_width: usize,
    /// The window's **text-area** width in cells: the content box minus every
    /// left gutter the client carves off — the fold column, the sign column,
    /// then the number gutter (the same `width` the projection sizes its rows
    /// to). Consumers that must match what the client paints (the menu box
    /// placement, mouse hit-testing) use this, not `rect.width` minus the
    /// number gutter alone, which would overstate the text width whenever a
    /// fold or sign column is shown.
    pub text_width: usize,
    /// Width in cells of every left gutter the client carves off before the text
    /// body — the fold column, the sign column, then the number gutter (exactly
    /// the terms [`text_width`](WindowView::text_width) subtracts from the content
    /// box). The client's text-body origin is the content origin plus this many
    /// cells, so a consumer anchoring to the painted text inner (the menu box's
    /// x-position) adds this, not the number gutter alone, which would land the
    /// box (fold + sign) cells left of the caret whenever either column shows.
    pub left_gutters: usize,
    /// `'foldcolumn'` width in cells (`0` when off) — how many cells the client
    /// reserves for the fold-marker gutter, to the left of the sign / number
    /// columns. The per-row markers are in [`foldcolumn`](WindowView::foldcolumn).
    pub foldcolumn_width: usize,
    /// Per visible row, the fold-marker string to paint in the fold gutter — each
    /// exactly [`foldcolumn_width`](WindowView::foldcolumn_width) cells wide (`-`/`│`
    /// for open folds, `+` for a closed one, spaces elsewhere). Empty when
    /// `foldcolumn` is `0`.
    pub foldcolumn: Vec<String>,
    /// This window's `'signcolumn'` policy. Carried so the server (which owns the
    /// diagnostics that fill the column) can resolve it to a rendered sign width;
    /// core itself only consults its [`floor`](crate::SignColumn::floor_cells) for
    /// text-width math.
    pub signcolumn: crate::options::SignColumn,
    /// This window's `'padding'` — the per-side blank margin (cells) around its
    /// content box. The server projects it on the wire; each client insets the
    /// window's gutter/text/status/cursor by it (the same way it re-derives the
    /// float-border inset), so the content reads with breathing room from the rect
    /// edges. All-zero by default (no margin); the projection's `width`/`height`
    /// already account for it.
    pub padding: crate::options::Padding,
    /// This window's buffer `tabstop`: the width the client must expand a `\t`
    /// to, so its tab rendering matches the server's [`cursor_screen_col`] (which
    /// is computed with this same value). A client that hard-codes a different
    /// width would misplace the cursor over tabbed lines.
    ///
    /// [`cursor_screen_col`]: WindowView::cursor_screen_col
    pub tabstop: usize,
    /// Whether this window is a **float**: drawn on top of the tiled windows at
    /// its absolute `rect`. The client paints floats in a second, on-top pass (in
    /// list order, which is z-order); a tiled window is `false`.
    pub floating: bool,
    /// The float's border style (`None` for a tiled window or a borderless
    /// float). When set, the client draws the border around `rect` and the inner
    /// content (this view's `lines`/gutter/status) sits one cell inside it — the
    /// projection already sized `lines` to that inset area.
    pub border: BorderStyle,
    /// The float's title, drawn on its top border. `None` when untitled.
    pub title: Option<String>,
    /// Pre-computed facts the `'statusline'` `%`-format engine reads to expand its
    /// built-in fields (`%f`, `%l`, `%y`, …). Built here in core — where the
    /// buffer and viewport live — so the server only has to run the (Lua-aware)
    /// engine over it. See [`crate::statusline`].
    pub status_ctx: StatuslineCtx,
    /// Whether this window paints its own status row, per `'laststatus'`
    /// ([`Editor::window_statusline_visible`]): false hides it (modes `0`/`3`, or
    /// `1` with a single window) and the freed bottom row becomes text. When
    /// false the window's `lines` already fill the extra row, so the client must
    /// not carve a status row off this window's rect.
    ///
    /// [`Editor::window_statusline_visible`]: crate::Editor::window_statusline_visible
    pub status_visible: bool,
    /// When `Some`, this window's buffer is an image opened for preview
    /// (`'imagepreview'`): the client renders the picture instead of the (empty)
    /// text body. `None` for an ordinary text / terminal / directory buffer. See
    /// [`ImageView`].
    pub image: Option<ImageView>,
    /// This window's effective `'winhighlight'` — the per-window highlight-group
    /// remap, resolved from the window-local option falling back to the dock's (see
    /// [`Editor::effective_winhighlight`]). Empty for almost every window. The
    /// server applies it while resolving this window's highlights, so a group named
    /// on the left renders with the group on its right *here only*.
    ///
    /// [`Editor::effective_winhighlight`]: crate::Editor::effective_winhighlight
    pub winhl: crate::WinHl,
}

/// An image-preview window's payload ([`WindowView::image`]): just the filesystem
/// path of the image to render — a *reference*, never the bytes (never-freeze). The
/// client reads and decodes it; in an embedded session it shares the filesystem and
/// opens `path` directly, while in a daemon (`:connect`) session the file lives on
/// the remote host, so the client fetches the bytes out-of-band over the editor RPC
/// (`bemtvi_image_read`). The server stamps that `remote` bit onto the redraw marker,
/// not here — core knows nothing of the daemon. Kept as a struct (not a bare
/// `String`) so the cache key can grow `size`/`mtime` fields without re-threading the
/// wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageView {
    /// The image file's path (the buffer's path).
    pub path: String,
    /// The file's on-disk size in bytes, and its mtime as Unix milliseconds (`0`
    /// when unknown). Together they version the file so the client re-decodes its
    /// cached picture when the file changes on disk (e.g. an external edit the
    /// always-on watch reloaded) rather than showing the stale image.
    pub size: u64,
    pub mtime_ms: u64,
}

/// One tab page's cell in the tabline. `label` is the tab's focused window's
/// buffer name (`[No Name]` when unset), `modified` flags unsaved changes, and
/// `window_count` is how many windows the tab holds. The client formats the cell
/// (vim's default `{count}{+} {label}`) and highlights the `current` one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub label: String,
    pub modified: bool,
    pub window_count: usize,
}

/// One region's tabline: its tab cells in tabline order plus the active cell
/// index. Empty `tabs` ⇒ that region draws no tabline (its `showtabline` gate hid
/// it, or — for a dock — it is closed). `current` is meaningful only when `tabs`
/// is non-empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionTabline {
    pub tabs: Vec<TabView>,
    pub current: usize,
    /// A fixed dock title shown at the start of the strip (the `btv.dock` `title`
    /// option), independent of the tab cells. Empty for the main region and for a
    /// dock with no title.
    pub title: String,
}

/// Every region's independent tabline (see [`RegionTabline`]): the main editor
/// area plus the four docks, indexed by `DockSide::idx` (`[left, right, top,
/// bottom]`). Each region carries its own tab pages, so the client draws a tabline
/// at the top of each region's band rather than one global bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionTablines {
    pub main: RegionTabline,
    pub docks: [RegionTabline; 4],
}

/// A snapshot of everything a client needs to draw a frame: the **global** chrome
/// (mode label, command line, message, panel) plus the list of [`WindowView`]s to
/// paint. With one window the list has a single entry spanning the whole text
/// area, so the rendered frame matches the pre-windows view exactly.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// The windows to paint, in layout order. Always at least one.
    pub windows: Vec<WindowView>,
    /// The tab pages, in tabline order — **empty** when only one tab is open (the
    /// client then draws no tabline). When non-empty, `current_tab` indexes the
    /// active cell.
    pub tabline: Vec<TabView>,
    /// Index into `tabline` of the active tab. Meaningful only when `tabline` is
    /// non-empty.
    pub current_tab: usize,
    /// Per-region tablines — each region (main + each open dock) has its own
    /// independent tab pages. `region_tablines.main` carries the same cells as the
    /// legacy `tabline`/`current_tab` above (kept until clients migrate); the dock
    /// entries are the new per-dock tablines. See [`RegionTablines`].
    pub region_tablines: RegionTablines,
    /// The split borders between windows, each in its [`Separator::region`]'s
    /// cells. Empty with a single window and no dock.
    pub separators: Vec<Separator>,
    /// The left dock's content width in cells, `0` when closed. A client reserves
    /// `width + 1` columns on the left (the `+1` a separator) and renders
    /// `WindowRegion::DockLeft` windows there. See [`WindowRegion`].
    pub dock_left: usize,
    /// The right dock's content width in cells, `0` when closed.
    pub dock_right: usize,
    /// The top dock's content height in rows, `0` when closed. The top dock sits
    /// **above** the tabline.
    pub dock_top: usize,
    /// The bottom dock's content height in rows, `0` when closed (sits above the
    /// read-only panel).
    pub dock_bottom: usize,
    /// Uppercase mode name for the status line, e.g. `"NORMAL"`.
    pub mode_label: String,
    /// True while in command-line mode; the cursor then belongs to the command
    /// region, which the client owns.
    pub command_mode: bool,
    /// True while `r` waits for its replacement character (a one-shot replace
    /// that stays in normal mode). Clients show the replace cursor shape while it
    /// holds, mirroring vim's operator-pending feedback.
    pub pending_replace: bool,
    /// Command-line contents (text after the leading prompt char).
    pub cmdline: String,
    /// The command-line prompt character: `:` for an ex command, `/` / `?` for a
    /// forward / backward search. Only meaningful while `command_mode`.
    pub cmdline_prefix: char,
    /// The multi-char prompt label for a `vim.ui.input` prompt, shown ahead of
    /// the editable line in place of `cmdline_prefix`. Empty for `:`/`/`/`?`.
    pub cmdline_prompt: String,
    /// Cursor position within `cmdline` as a character count from its start, so
    /// the client can place the command cursor mid-line after `<Left>`/`<Right>`
    /// edits. Only meaningful while `command_mode`.
    pub cmdline_cursor: usize,
    /// Transient status message (shown on the command line when not typing one).
    pub message: String,
    /// Whether [`message`](View::message) is an error — the client paints it with
    /// the red `ErrorMsg` highlight. Mirrors [`Editor::message_error`].
    pub message_error: bool,
    /// The floating selectable-list widget (`btv.ui.select`; later the picker),
    /// or `None` when none is open. When present it has input focus and floats
    /// over the focused window's text area; the server projects its geometry.
    pub menu: Option<MenuView>,
    /// The list-less content float (`btv.ui.float`; LSP hover / signature help), or
    /// `None` when none is open. A transient, non-grabbing overlay floating over
    /// the text — the sibling of [`menu`](View::menu).
    pub content_float: Option<ContentFloatView>,
    /// The single **global** status line's `%`-format context, present only at
    /// `'laststatus'`=3. It carries the *focused* window's facts (vim shows the
    /// current window's status line in the global bar); the server runs the engine
    /// over it across the full editor width and the client paints it docked one
    /// row above the command line. `None` for modes `0/1/2`, where status lines
    /// are per-window (or hidden) instead.
    pub global_statusline: Option<StatuslineCtx>,
    /// Labels for each **hidden** dock (toggle / auto-hide collapsed), in
    /// `DockSide::ALL` order — the dock's `btv.dock` title, or its side keyword when
    /// untitled. Empty when no dock is hidden. The client paints these as clickable
    /// `▸{label}` chips on the command-line row while it is idle (message empty, not
    /// command mode), so a collapsed dock still advertises that it exists; clicking
    /// a chip re-shows that dock.
    pub hidden_docks: Vec<String>,
}

impl View {
    pub(crate) fn from_editor(ed: &Editor) -> View {
        let bands = ed.dock_bands();
        let windows: Vec<WindowView> = ed
            .window_layouts()
            .iter()
            .map(|w| window_view(ed, w))
            .collect();
        // At `laststatus=3` the single global status line shows the *focused*
        // window's facts; capture that window's `%`-context for the server to run
        // the engine over (falling back to the first window if none is flagged).
        let global_statusline = ed.global_statusline_visible().then(|| {
            windows
                .iter()
                .find(|w| w.focused)
                .unwrap_or(&windows[0])
                .status_ctx
                .clone()
        });
        View {
            windows,
            tabline: ed.tab_labels().into_iter().map(tab_label_to_view).collect(),
            current_tab: ed.current_tab_index(),
            region_tablines: ed.region_tablines(),
            separators: ed.all_separators(),
            mode_label: ed.mode_label().to_string(),
            command_mode: ed.mode == Mode::Command,
            pending_replace: ed.pending_replace(),
            cmdline: ed.cmdline.clone(),
            cmdline_prefix: ed.cmdline_prefix(),
            cmdline_prompt: ed.cmdline_prompt().to_string(),
            cmdline_cursor: ed.cmdline_cursor(),
            // Two placeholders ride the idle message line where vim shows
            // `-- INSERT --`, each only while no real message is up. In terminal-job
            // mode every key goes to the child, so surface the way out; and a macro
            // recording announces itself exactly as vim does (`recording @a`), which
            // is why it needs no field of its own on the wire.
            message: match (ed.message.is_empty(), ed.mode, ed.recording_register()) {
                (true, Mode::Terminal, _) => {
                    "-- TERMINAL --  (<C-\\><C-n> or 3×<Esc> to exit)".to_string()
                }
                (true, _, Some(reg)) => format!("recording @{reg}"),
                _ => ed.message.clone(),
            },
            // The terminal placeholder isn't an error; the flag only rides a real
            // message (`message_error` is stale-but-unseen when `message` is empty).
            message_error: !ed.message.is_empty() && ed.message_error,
            menu: ed.menu_view(),
            content_float: ed.content_float_view(),
            global_statusline,
            hidden_docks: ed
                .hidden_dock_chips()
                .into_iter()
                .map(|(_, label)| label)
                .collect(),
            dock_left: bands.left,
            dock_right: bands.right,
            dock_top: bands.top,
            dock_bottom: bands.bottom,
        }
    }

    /// The focused window — the one the terminal cursor is drawn in, and the
    /// reference point for global overlays (the completion popup, the
    /// under-cursor diagnostic). There is always at least one window; falls back
    /// to the first if somehow none is flagged.
    pub fn focused(&self) -> &WindowView {
        self.windows
            .iter()
            .find(|w| w.focused)
            .unwrap_or(&self.windows[0])
    }
}

/// Project a core [`TabLabel`] into the wire-facing [`TabView`].
pub(crate) fn tab_label_to_view(label: TabLabel) -> TabView {
    TabView {
        label: label.name,
        modified: label.modified,
        window_count: label.window_count,
    }
}

/// Project one window into a [`WindowView`], slicing *its* buffer at *its* view
/// position. The focused window renders the editor's live `cursor`/`top` and owns
/// the transient overlays — the visual selection, the live `incsearch` preview,
/// and the scroll-animation band; the rest render their stashed positions with
/// only the persistent `hlsearch` highlight.
fn window_view(ed: &Editor, w: &WindowLayout) -> WindowView {
    let buf = ed
        .buffer_of(w.buffer)
        .expect("a live window's buffer is always open");
    let line_count = buf.line_count();
    let number_width = ed.number_width_for(&w.options, line_count);
    // A bordered float spends one cell on each side on its border, so its content
    // (gutter + text + status) lives in the rect inset by one cell. Tiled windows
    // and borderless floats use the whole rect. The client draws the border on the
    // outer `rect`, then paints this content into the inset area, so the two agree.
    let inset = if w.floating && w.border != BorderStyle::None {
        1
    } else {
        0
    };
    // `'padding'` insets the whole content box (gutter + text + status) by a
    // per-side blank margin, inside any float border. Clients re-derive the same
    // inset from the projected `padding`, so the two agree (mirroring how the float
    // border inset is handled). Clamped so at least one text row/col survives.
    let pad = w.options.padding;
    let content_height = w
        .rect
        .height
        .saturating_sub(2 * inset)
        .saturating_sub(pad.vertical());
    let content_width = w
        .rect
        .width
        .saturating_sub(2 * inset)
        .saturating_sub(pad.horizontal());
    // Whether this window draws its own status row (per `'laststatus'`, with any
    // per-dock override for the window's region); when it does not, the freed row
    // becomes text, so the window shows one more line.
    let status_visible = ed.window_statusline_visible(w.region, w.floating);
    // The content's own rows minus its status line (when shown); selections fill
    // to the text width (the area past the number gutter).
    let height = content_height
        .saturating_sub(usize::from(status_visible))
        .max(1);
    // The text area is the content box minus *every* gutter the client carves off its
    // left: the fold column, then the sign column, then the number gutter (the order
    // the clients split them in). Counting only the number gutter here would wrap
    // segments too wide by the other two, and the client would clip each row's tail
    // off the right edge — text silently lost on `wrap`, and a `nowrap` line that
    // looks like it fits.
    let width = content_width
        .saturating_sub(w.options.foldcolumn)
        .saturating_sub(signcol_cells(w.sign_width, &w.options))
        .saturating_sub(number_width);
    // The left-gutter total the client carves off before the text body (fold +
    // sign + number, the same terms `width` subtracts). A consumer matching the
    // painted text inner — the menu box's x-anchor — must offset the content
    // origin by all three, not just the number gutter.
    let left_gutters = content_width.saturating_sub(width);
    let top = w.top;
    // A stashed cursor may sit past a buffer that shrank while this window was
    // inactive; clamp it for the rendered ruler / cursor row.
    let cur_line = w.cursor.line.min(line_count.saturating_sub(1));

    // The single screen-row layout both the settled frame and the scroll band are
    // built from (`render_rows`): each visible buffer line expands into its
    // interleaved `virt_lines` rows then its text row, padded with `~` fillers past
    // end-of-buffer, and every per-row overlay (selection, secondary selection,
    // search, incsearch) rides on the row it belongs to. With no `virt_lines` this
    // is one row per buffer line. There is no separate per-array scatter step — the
    // overlays are written straight onto the rows they fall on.
    let wrap = w.options.wrap;
    // `'breakindent'` / `'showbreak'` / `'breakindentopt'` only take effect with
    // `wrap`; all default off. Bundled so the wrap helpers thread one value.
    let wp = unicode::WrapPrefix {
        breakindent: w.options.breakindent,
        showbreak: w.options.showbreak.as_str(),
        sbr: w.options.breakindent_sbr(),
    };
    let tabstop = buf.options.effective_tabstop();
    // The end-of-buffer filler char (`'fillchars'`' `eob`; vim's `~` by default).
    let eob = w.options.fillchars_eob();
    // Closed folds collapse on screen only while `'foldenable'` is on; otherwise
    // every line shows. The scroll band (below) is rendered fold-unaware for now —
    // its geometry helpers don't yet skip folds — so only the settled frame folds.
    let collapsed = if w.options.foldenable {
        w.folds.collapsed_regions(line_count)
    } else {
        Vec::new()
    };
    let rows = render_rows(
        ed, buf, top, height, line_count, w.focused, width, wrap, tabstop, wp, eob, &collapsed,
        w.cursor, ed.cursor,
    );

    let scroll = if w.focused {
        ed.pending_scroll().map(|ps| {
            // The band is anchored at the slide's topmost viewport line and spans, in
            // **screen rows**, from there down past the lower viewport plus a window
            // height — so whichever endpoint the slide rests at, `height` real rows are
            // under the offset. `virt_lines` no longer force an instant snap: they are
            // just more rows in the band, counted by `screen_rows_between`.
            let base_line = ps.from_top.min(ps.to_top);
            let max_top = ps.from_top.max(ps.to_top);
            let lead = screen_rows_between(
                buf, base_line, max_top, line_count, wrap, width, tabstop, wp,
            );
            let band_height = lead + height;
            // How the selection rides the band, by selection kind. `sel_head` feeds
            // the extent into `render_rows`; `sel_extends_down` tells the client which
            // side of the interpolated cursor to clip so it slides instead of snapping.
            let (sel_head, sel_extends_down) = if ed.mode == Mode::HelixNormal {
                // A Helix-normal selection is *collapsed* — a 1-wide block that moves
                // with the cursor (both ends follow it), so there is no fixed anchor to
                // sweep from. Rendered at the destination it would jump ahead of the
                // sliding cursor. Clip it "past" the interpolated cursor (down when the
                // scroll goes down, up otherwise) so the block stays hidden through the
                // slide — the terminal cursor animates in its place — and reappears only
                // on the settled frame, where the cursor has arrived.
                (ed.cursor, Some(ps.to_cursor >= ps.from_cursor))
            } else if ed.mode.is_visual() || ed.mode == Mode::HelixSelect {
                // An extending selection (Visual, or Helix select mode): the anchor is
                // fixed and the head sweeps. Carry the selection at the *maximal* extent
                // the slide touches — anchor → whichever scroll endpoint is furthest —
                // and let the client reveal/hide the moving edge per the interpolated
                // cursor (`sel_extends_down`). Projecting the destination cursor's extent
                // would carry the *small* end while shrinking, so the rows the cursor
                // sweeps back across would flash instead of sliding.
                let anchor_line = ed.visual_anchor().line;
                let far_line =
                    if anchor_line.abs_diff(ps.from_cursor) >= anchor_line.abs_diff(ps.to_cursor) {
                        ps.from_cursor
                    } else {
                        ps.to_cursor
                    };
                let mut head = ed.cursor;
                head.line = far_line;
                (head, Some(anchor_line <= far_line))
            } else {
                (ed.cursor, None)
            };
            // The band is projected exactly like a window — one `render_rows` over the
            // taller screen-row range — so it carries every overlay the settled frame
            // does (selection, secondary selections, search, incsearch, and the
            // interleaved `virt_lines` rows), keyed on the same rows.
            let band_rows = render_rows(
                ed,
                buf,
                base_line,
                band_height,
                line_count,
                true,
                width,
                wrap,
                tabstop,
                wp,
                eob,
                // The scroll band ignores folds (its geometry helpers don't skip
                // them yet); fold-aware scrolling lands in a later phase.
                &[],
                w.cursor,
                sel_head,
            );
            ScrollAnim {
                from_row: screen_rows_between(
                    buf,
                    base_line,
                    ps.from_top,
                    line_count,
                    wrap,
                    width,
                    tabstop,
                    wp,
                ),
                to_row: screen_rows_between(
                    buf, base_line, ps.to_top, line_count, wrap, width, tabstop, wp,
                ),
                from_cursor_row: cursor_band_row(
                    buf,
                    base_line,
                    ps.from_cursor,
                    line_count,
                    wrap,
                    width,
                    tabstop,
                    wp,
                ),
                to_cursor_row: cursor_band_row(
                    buf,
                    base_line,
                    ps.to_cursor,
                    line_count,
                    wrap,
                    width,
                    tabstop,
                    wp,
                ),
                duration_ms: ps.duration_ms,
                rows: band_rows,
                sel_extends_down,
            }
        })
    } else {
        None
    };

    let file_name = if buf.is_terminal() {
        // A terminal buffer shows the child's window title (OSC) as its name, seeded
        // from the command until the child sets one.
        buf.terminal_title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "terminal".to_string())
    } else if let Some(name) = buf.view_name.as_ref().filter(|n| !n.is_empty()) {
        // A plugin view (`btv.view`) has no file path; it shows its `create` name.
        name.clone()
    } else {
        buf.path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[No Name]".to_string())
    };

    // Effective filetype: the treesitter override (`:set ft=…`) if any, else the
    // extension-derived language. Drives `%y` and the web build's grammar choice.
    let filetype = ed.ts_language_for(w.buffer).unwrap_or_default();

    // The cursor's display row offset within its buffer line (its soft-wrap segment
    // index) and the screen column that segment starts at — both `0` under `nowrap`.
    // The cursor's screen column is then *row-local* (measured from its segment), so
    // it lands on the right wrapped row at the right column.
    // `cursor_seg_col` is the cursor's wrap segment start (subtracted to make the
    // column row-local); `cursor_prefix` is that row's `'breakindent'`/`'showbreak'`
    // prefix width (added back, since the prefix is baked onto the row text), so the
    // cursor lands past the indent on a continuation row. Both `0` on the first row.
    // A cursor parked on a closed fold sits on the single collapsed row, so it has
    // no wrap segments — skip the wrap math (which would otherwise place it on a
    // continuation row that the fold replaced).
    let cursor_folded = collapsed.iter().any(|f| f.contains(cur_line));
    let (cursor_extra_rows, cursor_seg_col, cursor_prefix) = if wrap && width > 0 && !cursor_folded
    {
        // `line_cow` borrows the rope chunk (no copy) when the line is contiguous —
        // the per-frame projection must not copy a huge wrapped line per window.
        let line = buf.line_cow(cur_line);
        let indent = unicode::cont_indent(&line, tabstop, width, wp);
        let segs = unicode::wrap_segments_indented(&line, tabstop, width, indent);
        let idx = segs
            .iter()
            .rposition(|s| w.cursor.col >= s.start_byte)
            .unwrap_or(0);
        // The prefix sits on continuation rows only (segment index > 0).
        let prefix = if idx > 0 { indent } else { 0 };
        (idx, segs[idx].start_col, prefix)
    } else {
        (0, 0, 0)
    };
    let (cursor_screen_col, cursor_width) = {
        let line = buf.line_cow(cur_line);
        let tab = buf.options.effective_tabstop();
        (
            unicode::virtcol(&line, w.cursor.col, tab).saturating_sub(cursor_seg_col)
                + cursor_prefix,
            unicode::cursor_cell_width(&line, w.cursor.col, tab),
        )
    };

    // Secondary multi-cursors visible in the focused window, as (row, screen
    // col). Off-screen cursors are dropped; the primary is carried separately.
    let secondary_cursors = if w.focused {
        ed.secondary_cursor_bytes()
            .into_iter()
            .filter_map(|byte| {
                let line = buf.byte_to_line(byte);
                // The cursor's *screen* row in the interleaved layout (skips it when
                // off-screen) — not `line - top`, which ignores virtual rows.
                let row = screen_row_of(&rows, line)?;
                let col = byte - buf.line_start(line);
                let s = buf.line_cow(line);
                let screen_col = unicode::virtcol(&s, col, buf.options.effective_tabstop());
                Some((row, screen_col))
            })
            .collect()
    } else {
        Vec::new()
    };

    // An image-preview buffer carries no text to paint; hand the client the path
    // so it renders the picture over this window's body instead of `lines`.
    let image = buf.is_image().then(|| {
        let path = buf
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        match buf.disk_stat() {
            // Local preview: a real on-disk version, so the client re-decodes when the
            // file's (size, mtime) moves (the watch/reload re-stats it).
            Some(disk) => ImageView {
                path,
                size: disk.size,
                mtime_ms: disk
                    .mtime
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as u64),
            },
            // Off-tick preview (daemon / WASM): no synchronous stat, so use the
            // reopen/reload generation as the version — it bumps on `:e` and on a
            // watch-driven reload, so the client (which fetches the bytes out of band)
            // re-fetches/re-decodes then, and not on every redraw in between.
            None => ImageView {
                path,
                size: 0,
                mtime_ms: buf.image_gen,
            },
        }
    });

    let status_ctx = window_status_ctx(StatusCtxInputs {
        buf,
        w,
        file_name: &file_name,
        filetype: &filetype,
        buftype: ed.buffer_buftype(w.buffer),
        cur_line,
        line_count,
        cursor_screen_col,
        top,
        text_height: height,
        recording: ed.recording_register(),
    });

    // The fold-marker gutter: one string per visible row, each `foldcolumn` cells
    // wide. Computed from the window's full fold set (open + closed) so it shows
    // structure even while a fold is open; blank for virtual / `~` filler rows.
    let foldcolumn_width = w.options.foldcolumn;
    let foldcolumn: Vec<String> = if foldcolumn_width > 0 {
        rows.iter()
            .map(|r| match r.line() {
                Some(line) => w
                    .folds
                    .column_marker(line, foldcolumn_width, w.options.foldenable),
                None => " ".repeat(foldcolumn_width),
            })
            .collect()
    } else {
        Vec::new()
    };

    WindowView {
        id: w.id,
        rect: ViewRect {
            x: w.rect.x,
            y: w.rect.y,
            width: w.rect.width,
            height: w.rect.height,
        },
        region: w.region,
        buffer: w.buffer,
        focused: w.focused,
        // The cursor's *screen* row: where its buffer line lands in the interleaved
        // layout (past any `virt_lines` above it). A stashed cursor of an unfocused
        // window can sit outside the visible band (the buffer shrank, or it never
        // scrolled to it) — clamp it to the nearest edge for the rendered ruler.
        // The cursor's line lands at its first display row; `cursor_extra_rows` then
        // steps down to the wrapped row the cursor's column is on (0 under nowrap).
        cursor_row: screen_row_of(&rows, cur_line)
            .map(|r| r + cursor_extra_rows)
            .unwrap_or_else(|| {
                if cur_line < top {
                    0
                } else {
                    height.saturating_sub(1)
                }
            }),
        leftcol: w.leftcol,
        cursor_col: w.cursor.col,
        cursor_screen_col,
        cursor_width,
        secondary_cursors,
        file_name,
        filetype,
        code_blocks: ed.doc_float_code_blocks(w.buffer).to_vec(),
        unnamed: buf.path.is_none(),
        modified: buf.modified,
        cursor_line: cur_line + 1,
        rows,
        scroll,
        number: w.options.number,
        relativenumber: w.options.relativenumber,
        cursorline: w.options.cursorline,
        colorcolumn: w.options.colorcolumns(),
        number_width,
        text_width: width,
        left_gutters,
        foldcolumn_width,
        foldcolumn,
        signcolumn: w.options.signcolumn,
        padding: pad,
        tabstop: buf.options.effective_tabstop(),
        floating: w.floating,
        border: w.border,
        title: w.title.clone(),
        status_ctx,
        status_visible,
        image,
        winhl: ed.effective_winhighlight(w.region, &w.options),
    }
}

/// The window facts [`window_status_ctx`] reads — grouped into a struct so the
/// builder doesn't take a long positional argument list (and `clippy` stays
/// happy).
struct StatusCtxInputs<'a> {
    buf: &'a Buffer,
    w: &'a WindowLayout,
    file_name: &'a str,
    /// The buffer's effective treesitter filetype (override or extension), for
    /// `%y` — so `:set ft=…` shows in the status line, not just the extension.
    filetype: &'a str,
    /// The buffer's kind ([`Editor::buffer_buftype`]), for `%{&buftype}` and to gate
    /// the file-only `[noeol]` marker off scratch surfaces.
    buftype: &'a str,
    /// Clamped cursor line (0-based) — the rendered line, matching the ruler.
    cur_line: usize,
    line_count: usize,
    /// Cursor screen-cell column (0-based, tab/wide aware) for `%v`.
    cursor_screen_col: usize,
    /// 0-based first visible buffer line, and visible text rows, for `%P`.
    top: usize,
    text_height: usize,
    /// The macro register being recorded into, for the `macro` segment.
    recording: Option<char>,
}

/// Build the [`StatuslineCtx`] the `%`-format engine expands its built-in fields
/// from. The facts come straight off the buffer and the window's viewport; the
/// flags bemtvi does not model yet (`'modifiable'`, `'readonly'`, help buffers)
/// take their always-true / always-false defaults, so `%m`/`%r`/`%h` render
/// faithfully for what bemtvi supports rather than faking a state it lacks.
fn window_status_ctx(inp: StatusCtxInputs) -> StatuslineCtx {
    let path = inp.buf.path.as_deref();
    let file_tail = path
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "[No Name]".to_string());
    StatuslineCtx {
        // `%f`/`%F`: bemtvi keeps the path as opened; v1 uses it verbatim for both
        // (no `canonicalize`, which would be I/O — forbidden in core).
        file_rel: inp.file_name.to_string(),
        file_full: inp.file_name.to_string(),
        file_tail,
        modified: inp.buf.modified,
        modifiable: true,
        readonly: false,
        help: false,
        filetype: inp.filetype.to_string(),
        fileencoding: inp.buf.options.fileencoding.to_string(),
        bomb: inp.buf.options.bomb,
        endofline: inp.buf.options.endofline,
        fixendofline: inp.buf.options.fixendofline,
        unterminated_file: inp.buf.is_unterminated_document(),
        buftype: inp.buftype.to_string(),
        bufnr: inp.w.buffer.0 as usize,
        line: inp.cur_line + 1,
        line_count: inp.line_count,
        col: inp.w.cursor.col + 1,
        virtcol: inp.cursor_screen_col + 1,
        top_line: inp.top + 1,
        text_height: inp.text_height,
        // Diagnostics are LSP-sourced and live in the server; core defaults to
        // zero and the server fills the counts in on the segment-render path.
        diag_counts: [0; 4],
        recording: inp.recording,
    }
}

/// The number of display (text) rows buffer `line` occupies: `1` under `nowrap`
/// (or for a line that fits), else its soft-wrap segment count. The free-function
/// mirror of [`Editor::line_text_rows`](crate::Editor), for the band geometry.
fn line_text_rows_of(
    buf: &Buffer,
    line: usize,
    wrap: bool,
    width: usize,
    tabstop: usize,
    wp: unicode::WrapPrefix,
) -> usize {
    if !wrap || width == 0 {
        return 1;
    }
    let text = buf.line_cow(line);
    let indent = unicode::cont_indent(&text, tabstop, width, wp);
    unicode::wrap_segments_indented(&text, tabstop, width, indent).len()
}

/// The number of **screen rows** the buffer lines `[base, target)` occupy: each
/// line's text rows (its soft-wrap segment count) plus its `virt_lines`
/// above/below (and past end-of-buffer each `~` filler is one row). This maps a
/// buffer-line viewport top to its screen-row offset within a band anchored at
/// `base` — the bridge from the buffer-line scroll *gesture* to the screen-row
/// band. (`target >= base`.)
#[allow(clippy::too_many_arguments)]
fn screen_rows_between(
    buf: &Buffer,
    base: usize,
    target: usize,
    line_count: usize,
    wrap: bool,
    width: usize,
    tabstop: usize,
    wp: unicode::WrapPrefix,
) -> usize {
    let virt = buf.virt_lines_by_line();
    (base..target)
        .map(|line| {
            if line < line_count {
                line_text_rows_of(buf, line, wrap, width, tabstop, wp)
                    + virt.get(&line).map_or(0, |r| r.above.len() + r.below.len())
            } else {
                1
            }
        })
        .sum()
}

/// The cursor line's screen-row offset within a band anchored at `base`: the
/// screen rows of `[base, cursor_line)` plus the cursor line's own `virt_lines`
/// *above*. Mirrors [`Editor::cursor_screen_row`](crate::Editor) for the band at
/// buffer-line granularity (the gesture carries lines, not the cursor's wrap
/// segment, so a wrapped cursor lands on its line's first display row).
#[allow(clippy::too_many_arguments)]
fn cursor_band_row(
    buf: &Buffer,
    base: usize,
    cursor_line: usize,
    line_count: usize,
    wrap: bool,
    width: usize,
    tabstop: usize,
    wp: unicode::WrapPrefix,
) -> usize {
    let above = buf
        .virt_lines_by_line()
        .get(&cursor_line)
        .map_or(0, |r| r.above.len());
    screen_rows_between(buf, base, cursor_line, line_count, wrap, width, tabstop, wp) + above
}

/// The placeholder text a closed fold shows on its single collapsed row — vim's
/// default `'foldtext'` shape: a dash run, the folded-line count, and the fold's
/// first line (leading/trailing whitespace trimmed). Customizable `foldtext` is a
/// later phase; this is the built-in default.
fn fold_text(buf: &Buffer, fold: &Fold) -> String {
    let first = buf.line_cow(fold.start);
    format!("+--{:>3} lines: {}", fold.line_count(), first.trim())
}

/// Lay out `height` screen rows starting at buffer line `base`, **expanding** each
/// buffer line that carries extmark `virt_lines` into extra rows: its
/// `virt_lines_above` rows, then its text row, then its `virt_lines_below` rows.
/// With no `virt_lines` this is one screen row per buffer line, `~`-padded past
/// end-of-buffer. The returned rows carry only their *structure* (kind / text /
/// virtual-line content); the focused-window overlays are layered on by
/// [`render_rows`].
///
/// A virtual row is [`RowKind::VirtLine`] with an empty `text` and its chunk run
/// in `virt_line`; a `~` filler is [`RowKind::Filler`]. Both report `number() ==
/// None`, but `virt_line.is_some()` distinguishes them — exactly the bit a client
/// uses to paint chunks rather than `~`.
///
/// When `wrap` is on (and `width > 0`), a buffer line wider than `width` cells is
/// laid out across several [`RowKind::Line`] rows — one per [`unicode::wrap_segments`]
/// segment, each carrying its byte slice and `start_col`. `tabstop` drives the
/// cell-accurate break.
#[allow(clippy::too_many_arguments)]
fn row_skeleton(
    buf: &Buffer,
    base: usize,
    height: usize,
    line_count: usize,
    wrap: bool,
    width: usize,
    tabstop: usize,
    wp: unicode::WrapPrefix,
    eob: char,
    folds: &[Fold],
) -> Vec<RenderRow> {
    let virt_by_line = buf.virt_lines_by_line();
    let mut rows = Vec::with_capacity(height);
    let push_virt = |rows: &mut Vec<RenderRow>, chunks: &[VirtChunk]| {
        rows.push(RenderRow::structural(
            RowKind::VirtLine,
            String::new(),
            Some(chunks.to_vec()),
            usize::MAX,
            0,
        ));
    };
    let mut buf_line = base;
    while rows.len() < height {
        if buf_line >= line_count {
            // `~` filler past the end of the buffer (no number, no virtual content).
            // The marker char is `'fillchars'`' `eob` key (vim's `~` by default;
            // `eob:\ ` blanks it).
            rows.push(RenderRow::structural(
                RowKind::Filler,
                eob.to_string(),
                None,
                usize::MAX,
                0,
            ));
            continue;
        }
        // A closed fold covering this line collapses its whole range into one
        // placeholder row (the fold's first-line number + fold text); the lines
        // after the start are skipped. Checked before virtual lines / wrapping so
        // a closed fold shows just the fold text, as vim does.
        if let Some(f) = folds.iter().find(|f| f.contains(buf_line)) {
            rows.push(RenderRow::structural(
                RowKind::Fold {
                    line: f.start,
                    count: f.line_count(),
                },
                fold_text(buf, f),
                None,
                usize::MAX,
                0,
            ));
            buf_line = f.end + 1;
            continue;
        }
        let virt = virt_by_line.get(&buf_line);
        if let Some(v) = virt {
            for vl in &v.above {
                if rows.len() >= height {
                    break;
                }
                push_virt(&mut rows, vl);
            }
        }
        // The buffer line's display rows: a single row at `nowrap`, else one per
        // soft-wrap segment. Each carries its byte slice and the screen column it
        // begins at (`start_col`), which `render_rows` uses to clip overlays.
        let text = buf.line_cow(buf_line);
        if wrap {
            // The `'breakindent'` / `'showbreak'` prefix on this line's continuation
            // rows; segments wrap into `width - prefix` cells (the first row keeps the
            // full width). The prefix is baked onto the continuation row text here, so
            // the client paints it as leading text — `render_rows` shifts this line's
            // overlays right by the prefix width to keep them aligned.
            let (prefix, indent) = unicode::break_prefix(&text, tabstop, width, wp);
            let segs = unicode::wrap_segments_indented(&text, tabstop, width, indent);
            for (i, seg) in segs.iter().enumerate() {
                if rows.len() >= height {
                    break;
                }
                let body = &text[seg.start_byte..seg.end_byte];
                let row_text = if seg.start_col == 0 {
                    body.to_string()
                } else {
                    format!("{prefix}{body}")
                };
                // The segment's end column (where the next segment begins) bounds the
                // overlay clip; the last segment runs to end-of-line (`MAX`). The baked
                // prefix is the rebase offset on continuation rows only.
                rows.push(RenderRow::structural(
                    RowKind::Line {
                        line: buf_line,
                        start_col: seg.start_col,
                    },
                    row_text,
                    None,
                    segs.get(i + 1).map_or(usize::MAX, |s| s.start_col),
                    if seg.start_col == 0 { 0 } else { indent },
                ));
            }
        } else if rows.len() < height {
            rows.push(RenderRow::structural(
                RowKind::Line {
                    line: buf_line,
                    start_col: 0,
                },
                // The row owns its text — only the wrap path above could borrow.
                text.into_owned(),
                None,
                usize::MAX,
                0,
            ));
        }
        if rows.len() >= height {
            break;
        }
        if let Some(v) = virt {
            for vl in &v.below {
                if rows.len() >= height {
                    break;
                }
                push_virt(&mut rows, vl);
            }
        }
        buf_line += 1;
    }
    rows.truncate(height);
    rows
}

/// Build the full [`RenderRow`] layout for a window's text body: the structural
/// [`row_skeleton`] with the focused window's per-row overlays (primary +
/// secondary visual selections, `hlsearch`, the live `incsearch` preview) written
/// straight onto the rows they fall on. An unfocused window carries no overlays.
///
/// The overlay arrays are computed one-per-buffer-line over `[base, base+height)`
/// (a safe over-provision — at most `height` buffer lines are visible) and indexed
/// by each [`RowKind::Line`] row's buffer line, which is how a virtual / `~` row
/// ends up with the defaults. This is the single place the settled frame and the
/// scroll band both build their rows from.
///
/// `cursor` drives the `incsearch` window; `sel_head` is the visual selection's
/// head — the live cursor for the settled frame, or the slide's furthest extent
/// for the scroll band (so it carries the maximal selection it touches).
#[allow(clippy::too_many_arguments)]
fn render_rows(
    ed: &Editor,
    buf: &Buffer,
    base: usize,
    height: usize,
    line_count: usize,
    focused: bool,
    width: usize,
    wrap: bool,
    tabstop: usize,
    wp: unicode::WrapPrefix,
    eob: char,
    folds: &[Fold],
    cursor: Cursor,
    sel_head: Cursor,
) -> Vec<RenderRow> {
    let mut rows = row_skeleton(
        buf, base, height, line_count, wrap, width, tabstop, wp, eob, folds,
    );
    if !focused {
        return rows;
    }
    let selection = selection_spans_with_head(ed, buf, width, line_count, base, height, sel_head);
    let secondary = secondary_selection_spans(ed, buf, width, line_count, base, height);
    let (search, incsearch) = ed.search_highlights_in(buf, cursor, focused, base, height);
    // Clip a full-line screen-column span to the row's wrap segment `[start_col,
    // seg_end_col)` and rebase to row-local columns (adding the baked-prefix
    // `indent`); `None` if it misses the segment. This is the same clip the server's
    // per-row overlay projections use (see `RowSeg::clip`), so selection/search and
    // treesitter/diagnostics line up on a wrapped row. Under `nowrap` /
    // first-or-only row (`start_col == 0`, `seg_end_col == MAX`, `indent == 0`) it is
    // the identity — spans pass through unchanged.
    let clip = |span: (usize, usize), start_col: usize, end_col: usize, indent: usize| {
        let lo = span.0.max(start_col);
        let hi = span.1.min(end_col);
        (lo < hi).then(|| (lo - start_col + indent, hi - start_col + indent))
    };
    for row in &mut rows {
        let Some(line) = row.line() else {
            continue;
        };
        let (start_col, end_col, indent) = (row.start_col(), row.seg_end_col, row.indent);
        let k = line - base;
        row.selection = selection
            .get(k)
            .copied()
            .flatten()
            .and_then(|s| clip(s, start_col, end_col, indent));
        if let Some(spans) = secondary.get(k) {
            row.secondary_selection = spans
                .iter()
                .filter_map(|s| clip(*s, start_col, end_col, indent))
                .collect();
        }
        if let Some(spans) = search.get(k) {
            row.search = spans
                .iter()
                .filter_map(|s| clip(*s, start_col, end_col, indent))
                .collect();
        }
        row.incsearch = incsearch
            .get(k)
            .copied()
            .flatten()
            .and_then(|s| clip(s, start_col, end_col, indent));
    }
    rows
}

/// Screen row (index into a [`RenderRow`] layout) showing buffer line `line`
/// (0-based), or `None` when it isn't on screen.
fn screen_row_of(rows: &[RenderRow], line: usize) -> Option<usize> {
    rows.iter().position(|r| r.line() == Some(line))
}

/// Compute, for each of the `count` rows starting at buffer line `base`, the
/// half-open screen-column span to highlight as the visual selection (or
/// `None`) for an explicit selection `head`. Returns all-`None` outside visual
/// modes. Called for the focused window only, so the editor's live `mode` /
/// anchor describe `buf`. The settled frame passes the live cursor as `head`; the
/// scroll band passes the slide's furthest extent (so it carries the maximal
/// selection it touches) and lets the client clip it back to
/// the interpolated cursor.
fn selection_spans_with_head(
    ed: &Editor,
    buf: &Buffer,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
    head: Cursor,
) -> Vec<Option<(usize, usize)>> {
    let mut spans = vec![None; count];
    let Some(visual_mode) = ed.rendered_visual_mode() else {
        return spans;
    };

    let (start, end) = order_selection(ed.visual_anchor(), head);
    let linewise = visual_mode == Mode::VisualLine;
    let ts = ed.tabstop();
    for (row, span) in spans.iter_mut().enumerate() {
        let buf_line = base + row;
        if buf_line >= line_count {
            continue;
        }
        *span = selection_row_span(buf, width, start, end, linewise, ts, buf_line);
    }

    spans
}

/// Like [`selection_spans`] but for the **secondary** multi-cursors: per row, the
/// spans of every secondary cursor's selection (its anchor→head). Empty outside a
/// visual mode or with no secondary cursors. The primary's selection is projected
/// separately into [`WindowView::selection`].
fn secondary_selection_spans(
    ed: &Editor,
    buf: &Buffer,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
) -> Vec<Vec<(usize, usize)>> {
    let mut rows = vec![Vec::new(); count];
    // Show secondaries whenever the primary selection shows — including while a
    // search command line is open over a Helix / visual selection — so the whole
    // multi-selection stays visible together (issue: a Helix `/` must not hide it).
    let Some(visual_mode) = ed
        .rendered_visual_mode()
        .filter(|_| ed.has_secondary_cursors())
    else {
        return rows;
    };
    let linewise = visual_mode == Mode::VisualLine;
    let ts = ed.tabstop();
    for (anchor, head) in ed.secondary_selections() {
        let (start, end) = order_selection(anchor, head);
        for (row, list) in rows.iter_mut().enumerate() {
            let buf_line = base + row;
            if buf_line >= line_count {
                continue;
            }
            if let Some(span) = selection_row_span(buf, width, start, end, linewise, ts, buf_line) {
                list.push(span);
            }
        }
    }
    rows
}

/// Order a selection's two ends (`anchor`, `cursor`) by buffer position into
/// `(start, end)`.
fn order_selection(a: Cursor, c: Cursor) -> (Cursor, Cursor) {
    if (a.line, a.col) <= (c.line, c.col) {
        (a, c)
    } else {
        (c, a)
    }
}

/// The half-open screen-column span to highlight on buffer line `buf_line` for a
/// single selection running from ordered `start` to `end`, or `None` if the line
/// lies outside it. Shared by the primary selection and every secondary cursor's.
fn selection_row_span(
    buf: &Buffer,
    width: usize,
    start: Cursor,
    end: Cursor,
    linewise: bool,
    ts: usize,
    buf_line: usize,
) -> Option<(usize, usize)> {
    if buf_line < start.line || buf_line > end.line {
        return None;
    }
    if linewise {
        // Whole line, filled to the viewport edge — as vim paints it.
        return Some((0, width));
    }
    let text = buf.line_cow(buf_line);
    let mut vc = unicode::LineVirtcol::new(&text, ts);
    // Charwise: clip the inclusive [start, end] region to this row.
    let lo = if buf_line == start.line { start.col } else { 0 };
    let start_col = vc.at(lo);
    let end_col = if buf_line == end.line {
        // Include the grapheme under the trailing cursor.
        let hi = unicode::next_grapheme(&text, end.col.min(text.len()));
        vc.at(hi)
    } else {
        // The selection continues onto the next line: highlight the text and one
        // extra cell standing in for the selected newline.
        vc.at(text.len()) + 1
    };
    Some((start_col, end_col))
}
