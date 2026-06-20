//! The server's view, mirrored client-side for rendering, and the `redraw`
//! notification parsing that fills it in. Frontend-agnostic: styles are the
//! neutral [`Style`], so a TUI and a GUI share this model and each converts to
//! its own toolkit at paint time.

use std::time::Duration;

use rmpv::Value;

use crate::parse::{
    chrome_style, map_get, map_str, map_str_array, map_u16, map_u64, parse_bools, parse_border,
    parse_cursor_list, parse_diagnostics, parse_diagnostics_signs, parse_diagnostics_virt,
    parse_float_lines, parse_highlights, parse_inlay_hints, parse_multi_spans, parse_numbers,
    parse_padding, parse_pair, parse_pmenu_items, parse_spans, parse_status, parse_styles,
    parse_virt_lines, parse_virt_text, DiagSign, DiagSpan, DiagVirt, HlSpan, IncSearchSpans,
    InlayHint, PmenuItem, SearchSpans, StatusSegment, VirtChunk, VirtPlacement,
};
use crate::style::{Border, Style};

/// The scroll gesture mirrored from the server's redraw, ready to animate. The
/// band is **screen-row based**: `lines` and the parallel overlay arrays are the
/// over-scanned screen rows the slide reveals (one entry per row), and the slide
/// is expressed as screen-row offsets *into the band* (`from_row`/`to_row`), kept
/// as `f32` for interpolation. The client interpolates the offset and slices
/// `rows[off .. off + height]` per frame. A client that animates scrolling drives
/// this; one that doesn't can ignore it. Because the band is screen rows,
/// interleaved `virt_lines` slide correctly (they are just more rows).
#[derive(Clone)]
pub struct ScrollData {
    /// Screen-row offset of the viewport's top row into the band at slide start /
    /// end (`rows[0]` is the topmost line's first screen row).
    pub from_row: f32,
    pub to_row: f32,
    /// Screen-row offset of the cursor's row into the band at slide start / end.
    pub from_cursor_row: f32,
    pub to_cursor_row: f32,
    pub duration: Duration,
    pub lines: Vec<String>,
    pub selection: Vec<Option<(u16, u16)>>,
    /// Per band row, the secondary multi-cursors' selection spans (the primary's
    /// is in `selection`), so multi-cursor selections slide with the text.
    pub secondary_selection: SearchSpans,
    /// Orientation of the visual selection sliding with the band: `Some(true)` the
    /// anchor is at/above the cursor (selection extends downward), `Some(false)`
    /// upward, `None` when no visual selection is sliding. The client clips the
    /// highlight's moving edge to the interpolated cursor accordingly — rows past
    /// the cursor on the growing side aren't selected yet.
    pub sel_extends_down: Option<bool>,
    pub numbers: Vec<Option<usize>>,
    /// Per band row, `true` on a soft-wrap continuation row — the band sibling of
    /// [`WindowView::continuation`], so the gutter blanks the wrapped rows while the
    /// slide animates exactly as it does when settled.
    pub continuation: Vec<bool>,
    /// `hlsearch` match spans for the band (aligned with `lines`), so the search
    /// highlight slides with the text rather than vanishing until the slide
    /// settles. Empty inner vec for rows with no match.
    pub search: SearchSpans,
    /// The live `incsearch` preview match per band row, or `None` — carried for the
    /// same reason as [`search`](ScrollData::search).
    pub incsearch: Vec<Option<(u16, u16)>>,
    /// Syntax highlights for the band (aligned with `lines`), so the slide is
    /// colored frame by frame instead of flashing white until it settles. Style
    /// ids index `styles` below.
    pub highlights: Vec<Vec<HlSpan>>,
    /// Inline LSP inlay hints for the band (aligned with `lines`), so they slide
    /// with the text rather than vanishing until the slide settles. Like the
    /// per-window `inlay_hints`, each entry is `[col, text, style_id]`.
    pub inlay_hints: Vec<Vec<InlayHint>>,
    /// Extmark `virt_text` placements for the band (aligned with `lines`), so eol /
    /// inline / overlay / win_col / right_align text slides with the line instead of
    /// flashing out and back when the slide settles. Same shape as the per-window
    /// `virt_text`.
    pub virt_text: Vec<Vec<VirtPlacement>>,
    /// Extmark `virt_lines` content per band row (`Some(chunks)` for a virtual
    /// row, else `None`) — the interleaved whole virtual rows now ride the band, so
    /// they slide with the text instead of only appearing once the slide settles.
    pub virt_lines: Vec<Option<Vec<VirtChunk>>>,
    /// Inline diagnostic virtual text per band row, so it slides with the line.
    pub diagnostics_virt: Vec<Option<DiagVirt>>,
    /// Diagnostic underline spans per band row (aligned with `lines`), so the
    /// squiggles slide with the text instead of blanking out for the slide. Same
    /// shape as the per-window `diagnostics`.
    pub diagnostics: Vec<Vec<DiagSpan>>,
    /// Diagnostic sign-column glyph per band row (`Some` on a row with a sign), so
    /// the signs slide with the text. Same shape as the per-window
    /// `diagnostics_signs`.
    pub diagnostics_signs: Vec<Option<DiagSign>>,
    /// The style palette captured with this gesture. Snapshotted (not read live
    /// from [`View::styles`]) because a delayed highlight redraw arriving
    /// mid-slide replaces the live palette, which would leave the band's frozen
    /// style ids pointing at the wrong entries.
    pub styles: Vec<Style>,
}

/// A window's per-side blank margin (cells) mirrored from the redraw `padding`
/// array (`[top, right, bottom, left]`, CSS order). The renderer insets the
/// content box by it; all-zero (the default) renders flush as before.
#[derive(Default, Clone, Copy)]
pub struct Padding {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

impl Padding {
    /// Total cells consumed horizontally (left + right). Saturating: the operands
    /// come off the wire, so a malformed/oversized pair must not overflow-panic.
    pub fn horizontal(&self) -> u16 {
        self.left.saturating_add(self.right)
    }

    /// Total cells consumed vertically (top + bottom). Saturating for the same
    /// wire-safety reason as [`horizontal`](Self::horizontal).
    pub fn vertical(&self) -> u16 {
        self.top.saturating_add(self.bottom)
    }
}

/// One window's content mirrored from a redraw `windows[i]` sub-map: its screen
/// rect, focus flag, and all the per-window fields (text, cursor, selection,
/// search, syntax/diagnostics, gutter, and status-line data). The client paints
/// each of these at its `rect` with its own gutter, text body, and status line.
#[derive(Default)]
pub struct WindowView {
    /// The window's rect in **windows-area** cells, or `None` for the legacy flat
    /// redraw (a single window that fills the whole windows area — the synthetic
    /// paint fixtures). The renderer offsets a `Some` rect by the windows-area
    /// origin; a `None` rect takes the whole area.
    pub rect: Option<WinRect>,
    /// Which screen region (main area or a dock) this window belongs to. Its
    /// `rect` is relative to that region's origin; the renderer offsets by the
    /// region's absolute screen origin (derived from the [`View`] dock bands).
    pub region: WindowRegion,
    pub focused: bool,
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_screen_col: u16,
    /// Display width (screen cells) of the grapheme under the cursor. `1` for an
    /// ordinary char or end-of-line; wider for a tab, a wide CJK/emoji glyph, or a
    /// `^X` / `<xx>` control-char token. The renderer's block cursor envelops this
    /// many cells. Defaults to `1` from an older server that omits the key.
    pub cursor_width: u16,
    /// Secondary multi-cursor positions as `(row, screen_col)` within this
    /// window's text body — the terminal's one real cursor is the primary
    /// (`cursor_row`/`cursor_screen_col`); the renderer paints these as
    /// reverse-video block cells. Empty for an unfocused window or with no
    /// multi-cursors active (also empty from an older server that omits the key).
    pub secondary_cursors: Vec<(u16, u16)>,
    /// First visible screen column (horizontal scroll offset) under `nowrap`. The
    /// renderer drops this many leading screen cells from each text row and shifts
    /// the cursor and every span left by it; the number gutter is not offset.
    pub leftcol: u16,
    /// This window's status line as rendered segments (`text` + resolved
    /// `style`), projected by the server's `%`-format engine. The client paints
    /// them left-to-right; an empty vec (an older server) falls back to a bare
    /// reverse-video status row.
    pub status: Vec<StatusSegment>,
    /// Whether this window paints its own status row (per `'laststatus'`). False
    /// (modes 0/3, or 1 with one window) gives the freed bottom row to text. An
    /// older server omits the flag; defaults to `true` (the historical
    /// every-window-has-a-status look).
    pub status_visible: bool,
    pub cursor_line: usize,
    /// Per visible row, the half-open screen-column span `[start, end)` to paint
    /// as the visual selection, or `None`.
    pub selection: Vec<Option<(u16, u16)>>,
    /// Per visible row, the half-open screen-column spans of every **secondary**
    /// multi-cursor's visual selection (the primary's lives in `selection`).
    /// Painted with the same `Visual` style; empty rows carry empty vecs.
    pub secondary_selection: SearchSpans,
    /// Per visible row, the half-open screen-column spans of every search match
    /// (`hlsearch`). Empty inner vecs for rows with no match.
    pub search: SearchSpans,
    /// Per visible row, the single span the live `incsearch` preview rests on.
    pub incsearch: IncSearchSpans,
    /// Per visible row, the treesitter highlight spans `(start_col, end_col,
    /// group, style_id)` in screen columns. `style_id` indexes [`View::styles`]
    /// (the global palette) when resolved through a colorscheme; `None` leaves
    /// the client to fall back to its own per-group theme.
    pub highlights: Vec<Vec<HlSpan>>,
    /// Per visible row, the LSP diagnostic underline spans `(start_col, end_col,
    /// severity, style_id)` in screen columns.
    pub diagnostics: Vec<Vec<DiagSpan>>,
    /// Per visible row, the inline diagnostic virtual text painted after the
    /// line's end-of-text (`Some((text, severity, style_id))`), or `None`.
    pub diagnostics_virt: Vec<Option<DiagVirt>>,
    /// Per visible row, the gutter diagnostic sign painted in the reserved sign
    /// column (`Some((glyph, severity, style_id))`), or `None` for a blank cell.
    pub diagnostics_signs: Vec<Option<DiagSign>>,
    /// Width in cells of the sign column reserved left of the number gutter (vim's
    /// `signcolumn`). `0` keeps the old gutter layout; otherwise it is a multiple of
    /// 2 (each sign column is 2 cells). The server resolves the `signcolumn` policy
    /// against the signs present, so the client just reserves this many cells.
    pub sign_width: u16,
    /// Per visible row, the inline LSP inlay hints `(col, text, style_id)` in
    /// screen columns, sorted left to right. The renderer inserts each hint's text
    /// at its column, shifting the real glyphs (and the cursor) right. Empty inner
    /// vecs for rows with no hints (and for a buffer with inlay hints disabled).
    pub inlay_hints: Vec<Vec<InlayHint>>,
    /// Per visible row, the extmark virtual-text placements (`virt_text`): each is a
    /// position + screen column + hl-mode + chunk run. Empty inner vecs for rows
    /// with no virtual text. The renderer paints eol placements after end-of-text
    /// and inline ones spliced into the row (later positions: overlay / right_align
    /// / win_col).
    pub virt_text: Vec<Vec<VirtPlacement>>,
    /// Per visible row, the extmark `virt_lines` content (`virt_lines`): `Some(chunks)`
    /// when the row is a **virtual line** (a whole extra screen row interleaved above /
    /// below its buffer line), else `None`. A virtual row also has `numbers[i] == None`
    /// and an empty `lines` entry; this `Some` is what distinguishes it from a `~`
    /// filler, so the renderer paints the chunks (no gutter number, no cursor) there.
    pub virt_lines: Vec<Option<Vec<VirtChunk>>>,
    /// A scroll gesture for this window, when its viewport just moved.
    pub scroll: Option<ScrollData>,
    /// Per visible row, the 1-based buffer line number (`None` for `~` fillers).
    /// A soft-wrap continuation row keeps its line's number here (so it stays the
    /// row→line mapping for highlights / diagnostics, and stays distinct from a
    /// `~` filler whose number is `None`); the renderer blanks the *gutter* on it
    /// using the parallel [`continuation`](WindowView::continuation) flag instead.
    pub numbers: Vec<Option<usize>>,
    /// Per visible row, `true` on a soft-wrap continuation row (a buffer line's
    /// 2nd+ display row). The renderer shows the line number on the line's first
    /// row only — a continuation's gutter is blank, matching vim. Empty / all-false
    /// from an older server that omits the key (every wrapped row then shows its
    /// number, the prior behavior).
    pub continuation: Vec<bool>,
    pub number: bool,
    pub relativenumber: bool,
    /// `'cursorline'` — when set, the renderer paints the cursor's screen row
    /// (`cursor_row`) with the [`View::cursor_line`] background. `false` from an
    /// older server that omits the key.
    pub cursorline: bool,
    pub number_width: u16,
    /// This window's `'padding'` — a per-side blank margin (cells) the renderer
    /// leaves around the content box (gutter + text + status), inside any float
    /// border. All-zero (no margin) from a server that omits the key. The window's
    /// projected `rows`/`cursor` already assume this inset, so the renderer only has
    /// to shift the content origin in and shrink the box by it.
    pub padding: Padding,
    /// This window's buffer `tabstop`: how many cells to expand a `\t` to when
    /// painting, mirrored from the server so the text lines up with the server's
    /// `cursor_screen_col` (computed with the same value). Defaults to 8.
    pub tabstop: u16,
    /// Whether this window is a **float**: the renderer paints it in a second,
    /// on-top pass (over the tiled windows) rather than tiling it. Tiled windows
    /// and the legacy flat redraw are `false`.
    pub floating: bool,
    /// The float's border type, or `None` for a borderless float / tiled window.
    /// When set the renderer draws a bordered box around the window's rect and
    /// paints the content one cell inside it.
    pub border: Option<Border>,
    /// The float's title, drawn on the top border. `None` when untitled.
    pub title: Option<String>,
    /// Whether this window's buffer has no file path yet (a fresh `[No Name]`
    /// buffer), so a write needs a target. An explicit flag from the server, so a
    /// client need not match the displayed name; an older server omitting it
    /// defaults to `false`.
    pub unnamed: bool,
    /// When `Some`, this window's buffer is an image opened for preview
    /// (`'imagepreview'`): the client renders the picture at [`ImageData::path`]
    /// instead of the (empty) `lines`. `None` for an ordinary buffer (and from an
    /// older server that omits the key).
    pub image: Option<ImageData>,
}

/// An image-preview window's payload (mirrors `nxvim_core::view::ImageView`): the
/// filesystem path of the image to render. The client reads and decodes it,
/// caching the decoded result.
///
/// **Where the bytes live** depends on [`remote`](ImageData::remote): an *embedded*
/// (local-disk) session shares the filesystem, so the client opens `path` directly;
/// a *daemon* (`:connect`) session keeps the bytes on the remote host, so the client
/// must fetch them out-of-band over the editor RPC (`nxvim_image_read`) — `path` is a
/// reference into the daemon's filesystem the client can't open itself.
#[derive(Clone)]
pub struct ImageData {
    pub path: String,
    /// The file's on-disk version — size in bytes and mtime as Unix milliseconds —
    /// so the client re-decodes its cached picture when the file changes on disk
    /// (rather than showing a stale image). `0` from an older server that omits them.
    pub size: u64,
    pub mtime_ms: u64,
    /// Whether the image's bytes live on a **remote daemon** rather than the client's
    /// local disk. `true` for a daemon (`:connect`) session: the client fetches the
    /// bytes over the editor RPC (`nxvim_image_read [path]`) instead of opening `path`
    /// locally. `false` (the default, and from an older server that omits the key) for
    /// an embedded session whose client shares the filesystem.
    pub remote: bool,
}

/// Which screen region a window/separator belongs to (mirrors
/// `nxvim_core::view::WindowRegion`). Its `rect`/coords are relative to the
/// region's own origin; the renderer maps the region to an absolute screen origin
/// using the [`View`]'s dock band sizes. `Main` is the default and the only region
/// when no dock is open.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowRegion {
    #[default]
    Main,
    DockLeft,
    DockRight,
    DockTop,
    DockBottom,
}

impl WindowRegion {
    /// Decode the wire string (`"main"`/`"dock_left"`/…); unknown ⇒ `Main`.
    fn from_wire(s: &str) -> WindowRegion {
        match s {
            "dock_left" => WindowRegion::DockLeft,
            "dock_right" => WindowRegion::DockRight,
            "dock_top" => WindowRegion::DockTop,
            "dock_bottom" => WindowRegion::DockBottom,
            _ => WindowRegion::Main,
        }
    }
}

/// A window's rect in windows-area cells (mirrors `nxvim_core::ViewRect`).
#[derive(Default, Clone, Copy)]
pub struct WinRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// A split separator the client draws between windows: a vertical `│` or
/// horizontal `─` run of `length` cells anchored at `(x, y)` in windows-area
/// cells. Empty with a single window.
#[derive(Clone, Copy)]
pub struct Separator {
    pub vertical: bool,
    pub x: u16,
    pub y: u16,
    pub length: u16,
    /// The region this separator's `(x, y)` is relative to.
    pub region: WindowRegion,
}

/// The server's view, mirrored client-side for rendering: the **global** chrome
/// (mode, command line, message, panel, popup, style palette) plus the list of
/// [`WindowView`]s to paint and the split [`Separator`]s between them.
#[derive(Default)]
pub struct View {
    /// The windows to paint, in layout order. Always at least one.
    pub windows: Vec<WindowView>,
    /// The split separators between windows (empty with one window).
    pub separators: Vec<Separator>,
    /// The left dock's content width in cells, `0` when closed. The client reserves
    /// `width + 1` columns on the left (the `+1` a separator) and renders
    /// `WindowRegion::DockLeft` windows there. See [`WindowRegion`].
    pub dock_left: u16,
    /// The right dock's content width in cells, `0` when closed.
    pub dock_right: u16,
    /// The top dock's content height in rows, `0` when closed. Sits **above** the
    /// tabline.
    pub dock_top: u16,
    /// The bottom dock's content height in rows, `0` when closed (above the panel).
    pub dock_bottom: u16,
    /// The tab pages, in tabline order — empty when only one tab is open (no
    /// tabline is drawn). When non-empty, `current_tab` indexes the active cell.
    pub tabline: Vec<TabData>,
    /// A custom `'tabline'` rendered by the server's `%`-format engine into styled
    /// segments spanning the editor width. Empty when no `'tabline'` is set (the
    /// client then formats the [`TabData`] cells itself); when non-empty it is
    /// painted in place of those cells on the same reserved top row.
    pub tabline_segments: Vec<StatusSegment>,
    /// Index into `tabline` of the active tab (meaningful only when `tabline` is
    /// non-empty).
    pub current_tab: usize,
    /// Per-region tablines — each region (main + each open dock) carries its own
    /// independent tab pages. `tabline`/`current_tab` above mirror `main`; the dock
    /// entries are the per-dock tablines a client draws at the top of each dock's
    /// band. Empty `tabs` ⇒ that region draws no tabline. See [`RegionTablines`].
    pub region_tablines: RegionTablines,
    pub mode_label: String,
    pub command_mode: bool,
    /// True while `r` waits for its replacement character (a one-shot replace
    /// that stays in normal mode). Drives the replace cursor shape.
    pub pending_replace: bool,
    pub cmdline: String,
    /// The command-line prompt char (`:` ex, `/` / `?` search). Defaults to `:`.
    pub cmdline_prefix: char,
    /// The multi-char `vim.ui.input` prompt label, rendered ahead of the editable
    /// line in place of `cmdline_prefix`. Empty for `:`/`/`/`?`.
    pub cmdline_prompt: String,
    /// Command cursor position as a character offset into `cmdline`, for placing
    /// the terminal cursor mid-line after `<Left>`/`<Right>` edits.
    pub cmdline_cursor: usize,
    pub message: String,
    /// Whether `message` is an error — the client paints the message line with the
    /// theme's red `ErrorMsg` highlight rather than the default foreground.
    pub message_error: bool,
    /// The `'guifont'` value (`"Fira Code:h14"`, neovim/neovide syntax), relayed
    /// from the server. A GUI client parses it for the font family and `:h` size;
    /// empty means the client's own default. Frontend-agnostic — the TUI ignores it.
    pub guifont: String,
    /// `'timeout'` — whether an ambiguous mapped prefix resolves on idle (the
    /// client arms its `timeoutlen` flush timer) or waits forever for the next key
    /// (`notimeout` → the client never arms it, so a which-key popup stays up).
    /// Relayed each frame; an older server that omits it falls back to `true`
    /// (vim's default). The struct's own `Default` is `false` — a safe pre-redraw
    /// state (no flush armed until the first frame carries the real value).
    pub timeout: bool,
    /// `'timeoutlen'` — how long (ms) the client waits after the last key before
    /// firing the idle flush, when [`View::timeout`] is on. Relayed each frame; an
    /// omitted field falls back to `1000` (vim's default).
    pub timeoutlen: u64,
    /// The per-frame style palette the server resolved from the active
    /// colorscheme; per-window `highlights`/chrome ids index into it. Global.
    pub styles: Vec<Style>,
    /// Resolved editor-chrome styles (`None` when the theme leaves the group
    /// undefined — the client then keeps its built-in look for that region).
    pub normal: Option<Style>,
    pub line_nr: Option<Style>,
    pub cursor_line_nr: Option<Style>,
    /// The `CursorLine` background, painted behind the cursor's screen row when a
    /// window has `'cursorline'` set. `None` when the colorscheme leaves it
    /// undefined — the client then uses its own subtle fallback.
    pub cursor_line: Option<Style>,
    pub visual: Option<Style>,
    pub search_style: Option<Style>,
    pub incsearch_style: Option<Style>,
    pub status_line: Option<Style>,
    /// The `ErrorMsg` look — the red foreground the client paints an error message
    /// line with (`message_error`). `None` when the colorscheme leaves it undefined,
    /// so the client falls back to a plain red foreground.
    pub error_msg: Option<Style>,
    pub end_of_buffer: Option<Style>,
    /// Float chrome (`FloatBorder` / `NormalFloat` / `FloatTitle`). `None` when
    /// the colorscheme leaves the group undefined — the client then keeps its
    /// built-in look (the terminal-default border, a `Normal`-derived bg). These
    /// are global: every float shares one theme, not a per-window style.
    pub float_border: Option<Style>,
    pub normal_float: Option<Style>,
    pub float_title: Option<Style>,
    /// The single global status line (`laststatus=3`) as rendered segments,
    /// spanning the full editor width and showing the focused window's facts.
    /// Empty for modes 0/1/2 (status lines are per-window, or hidden); when
    /// non-empty the renderer docks it on one row just above the command line and
    /// no window paints its own status row. Global — one per editor.
    pub global_status: Vec<StatusSegment>,
    /// The insert-mode completion popup, or `None` when none is open. Drawn last,
    /// over the focused window's text area. Global.
    pub pmenu: Option<PmenuData>,
    /// The floating selectable-list menu (`nx.ui.select`; later the picker), or
    /// `None` when none is open. Drawn over the focused window's text area with
    /// input focus, like the popup. Global.
    pub menu: Option<MenuData>,
    /// The list-less **content float** (`nx.ui.float`; LSP hover / signature help),
    /// or `None` when none is open. A transient, non-grabbing overlay drawn over the
    /// focused window's text area — the sibling of [`menu`](Self::menu). Global.
    pub content_float: Option<ContentFloatData>,
    /// Labels for each **hidden** dock (toggle / auto-hide collapsed), in dock-side
    /// order. The client paints these as clickable `▸{label}` chips on the
    /// command-line row while it is idle (`message` empty and not `command_mode`),
    /// so a collapsed dock still shows it exists; a click re-shows it. Empty when no
    /// dock is hidden.
    pub hidden_docks: Vec<String>,
}

/// The insert-mode completion popup mirrored from the server's redraw: the ranked
/// items, the selected index (`None` until the user navigates), and the overlay's
/// anchor and content size in **text-area cells** (the client adds the gutter and
/// text-area origin, then draws a bordered box around the content).
#[derive(Clone)]
pub struct PmenuData {
    pub items: Vec<PmenuItem>,
    pub selected: Option<usize>,
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
    /// The selected item's documentation lines, drawn in a preview box beside the
    /// popup. Empty ⇒ no preview (nothing selected, or the item has no docs).
    pub doc: Vec<String>,
}

/// The floating selectable-list menu mirrored from the server's redraw: the
/// choice labels, the highlighted index, and the overlay's anchor and content
/// size in **text-area cells** (the client adds the gutter and text-area origin,
/// then draws a bordered box) — the same convention as [`PmenuData`], but the
/// rows are plain labels (no kind / detail). A picker may also carry a side
/// [`preview`](Self::preview) pane and a completion popup a [`docs`](Self::docs)
/// sidebar.
#[derive(Clone)]
pub struct MenuData {
    pub items: Vec<String>,
    pub selected: usize,
    /// Whether `selected` is an active selection to highlight. `false` for a
    /// freshly opened completion popup (noselect) — no row is highlighted until the
    /// user navigates. Absent in the map ⇒ `true` (the `select` / picker default).
    pub selected_active: bool,
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
    /// The picker prompt query — `Some` (even when empty) for a `nx.picker`,
    /// `None` for a promptless `nx.ui.select`. Presence tells the client to draw a
    /// prompt row, a separator, and the caret.
    pub query: Option<String>,
    /// The prompt caret's column — a count of chars before it within `query`. `0`
    /// for a promptless `nx.ui.select`.
    pub query_cursor: u16,
    /// Whether the prompt sits **below** the results list (telescope-style) rather
    /// than above it (the default). `false` for a promptless `nx.ui.select`.
    pub prompt_bottom: bool,
    /// Whether to draw the box's **top** border. `false` for the completion popup,
    /// which sits flush against the line below the cursor; `true` (the default,
    /// when the key is absent) for a `select` / picker.
    pub border_top: bool,
    /// Whether this is the **command-line** completion wildmenu
    /// (`nx.cmdline_complete`). When set, the client anchors the box to the
    /// command-line area (frame-bottom, no number gutter) rather than the focused
    /// window's text inner — `col` is then a column within the command line and the
    /// box floats just above it. `false` (key absent) for every other menu.
    pub cmdline: bool,
    /// Per visible row (parallel to `items`), the matched-character spans to
    /// highlight as half-open **char** ranges (empty for rows with no match).
    pub match_spans: Vec<Vec<(u16, u16)>>,
    /// The picker preview pane (Phase 3), present when the source declared a
    /// `preview` kind. `None` for a `select` / preview-less picker — then the box is
    /// the list alone, exactly as before.
    pub preview: Option<MenuPreview>,
    /// The **docs sidebar**, present when the highlighted row carries documentation:
    /// a `Cursor`-placed completion popup's selected `lsp` row (Phase 4-D), or the
    /// cmdline wildmenu's selected command synopsis + help (cmdline-completion Phase
    /// 3). A *separate* bordered float beside the box (right, flipping left for room),
    /// not a column within it like [`preview`](Self::preview). For a cmdline wildmenu
    /// the float's geometry is rebased onto the box (which is `cmd_area`-anchored, not
    /// window-relative); see the client render. `None` for a `select` / picker or a
    /// row with no docs — the box then stands alone.
    pub docs: Option<MenuDocs>,
}

/// The list-less content float mirrored from the redraw (`nx.ui.float`; LSP hover
/// / signature help): the content lines, the float's absolute geometry (text-area
/// content coordinates, same convention as [`MenuData::row`]/`col`), its border
/// style keyword, and an optional title drawn on the top border. Rendered as its
/// own bordered box, like [`MenuDocs`] but standalone. See [`View::content_float`].
#[derive(Clone)]
pub struct ContentFloatData {
    /// The content lines (already windowed to `height`), each a run of styled
    /// [`VirtChunk`]s (`(text, style_id)`, the `virt_lines` wire form): a plain
    /// caller is one un-styled chunk per line, while a styled caller (which-key)
    /// colours keys vs. descriptions. `style_id` indexes [`View::styles`].
    pub lines: Vec<Vec<VirtChunk>>,
    /// The float's content top-left, **text-area-relative**.
    pub row: u16,
    pub col: u16,
    /// The float's content width / height in cells (the client adds its border).
    pub width: u16,
    pub height: u16,
    /// The border style, or `None` for a borderless float (the `"none"` keyword).
    pub border: Option<Border>,
    /// An optional title drawn on the top border (`None` when untitled).
    pub title: Option<String>,
}

/// The docs sidebar mirrored from the redraw: the highlighted item's documentation
/// lines and the float's absolute geometry (text-area-relative content coordinates,
/// same convention as [`MenuData::row`]/`col`). Rendered as its own bordered box
/// beside the completion popup (Phase 4-D) or the cmdline wildmenu (cmdline-completion
/// Phase 3). See [`MenuData::docs`].
#[derive(Clone)]
pub struct MenuDocs {
    /// The documentation lines (a `detail` / synopsis heading, then the body) —
    /// already windowed to `height`. Plain text, like a hover.
    pub lines: Vec<String>,
    /// The float's content top-left, **text-area-relative** (the server placed it
    /// beside the box and clamped it to the viewport).
    pub row: u16,
    pub col: u16,
    /// The float's content width / height in cells (the client adds its border).
    pub width: u16,
    pub height: u16,
}

/// The picker preview pane mirrored from the redraw: the windowed file content for
/// the selected row, the column width the pane occupies (so the list keeps `width −
/// preview.width − 1`), a title, and the match location to highlight. A column on
/// the right of an editor-placement picker box. See [`MenuData::preview`].
#[derive(Clone)]
pub struct MenuPreview {
    /// The windowed file lines (already sliced to the pane height). A single line
    /// like `"<path>: <err>"` / `"No preview"` is a visible placeholder.
    pub lines: Vec<String>,
    /// 1-based file line of `lines[0]` — context for an optional line-number gutter.
    pub first_line: u32,
    /// The previewed file's path, drawn as the pane's title.
    pub title: String,
    /// The pane's column width (cells). The list column is `MenuData::width −
    /// width − 1` (the `1` is the vertical separator).
    pub width: u16,
    /// The match position to range-highlight, as `(row, col)` **relative to
    /// `lines[0]`**; `None` for a file-kind preview (no location) or a placeholder.
    pub loc: Option<(u16, u16)>,
    /// Per-line native tree-sitter highlight spans (Phase 3b) — same shape as a
    /// window's text highlights (`[start_col, end_col, group, style_id]`), so a
    /// client reuses its span renderer. Empty inner vecs for unhighlighted lines
    /// (and for a preview the server couldn't highlight, e.g. an unknown filetype).
    pub highlights: Vec<Vec<HlSpan>>,
}

/// One tabline cell mirrored from the server's redraw: the buffer label, its
/// modified flag, and the tab's window count. The client formats the rendered
/// text (vim's default `{count}{+} {label}`).
#[derive(Clone)]
pub struct TabData {
    pub label: String,
    pub modified: bool,
    pub window_count: usize,
}

/// One region's tabline mirrored from the redraw: its tab cells plus the active
/// cell index. Empty `tabs` ⇒ that region draws no tabline (its `showtabline`
/// gate hid it, or — for a dock — it is closed).
#[derive(Clone, Default)]
pub struct RegionTabline {
    pub tabs: Vec<TabData>,
    pub current: usize,
    /// A fixed dock title shown at the start of the strip (the `nx.dock` `title`
    /// option). Empty for the main region and untitled docks.
    pub title: String,
}

/// Every region's independent tabline (see [`RegionTabline`]): the main editor
/// area plus the four docks. A client draws each region's tabline at the top of
/// that region's band.
#[derive(Clone, Default)]
pub struct RegionTablines {
    pub main: RegionTabline,
    pub left: RegionTabline,
    pub right: RegionTabline,
    pub top: RegionTabline,
    pub bottom: RegionTabline,
}

impl View {
    pub fn update(&mut self, params: &[Value]) {
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        self.mode_label = map_str(map, "mode_label");
        self.command_mode = map_get(map, "command_mode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.pending_replace = map_get(map, "pending_replace")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.cmdline = map_str(map, "cmdline");
        self.cmdline_prefix = map_get(map, "cmdline_prefix")
            .and_then(Value::as_str)
            .and_then(|s| s.chars().next())
            .unwrap_or(':');
        self.cmdline_prompt = map_str(map, "cmdline_prompt");
        self.cmdline_cursor = map_u64(map, "cmdline_cursor") as usize;
        self.message = map_str(map, "message");
        self.message_error = map_get(map, "message_error").and_then(Value::as_bool) == Some(true);
        self.guifont = map_str(map, "guifont");
        // The mapping-timeout config drives the client's idle-flush timer (below).
        // An older server omits these — fall back to vim's defaults (timeout on,
        // 1000ms) so the flush still fires; `notimeout` (false) disarms it entirely.
        self.timeout = map_get(map, "timeout")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.timeoutlen = map_get(map, "timeoutlen")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        // The style palette must land before windows (their scroll bands snapshot
        // it) and chrome (which indexes into it).
        self.styles = parse_styles(map_get(map, "styles"));
        let chrome = |key| chrome_style(map_get(map, "chrome"), key, &self.styles);
        self.normal = chrome("normal");
        self.line_nr = chrome("line_nr");
        self.cursor_line_nr = chrome("cursor_line_nr");
        self.cursor_line = chrome("cursorline");
        self.visual = chrome("visual");
        self.search_style = chrome("search");
        self.incsearch_style = chrome("incsearch");
        self.status_line = chrome("status_line");
        self.error_msg = chrome("error_msg");
        self.end_of_buffer = chrome("end_of_buffer");
        self.float_border = chrome("float_border");
        self.normal_float = chrome("normal_float");
        self.float_title = chrome("float_title");
        // The window list (the multi-window form), or a single window built from
        // the legacy flat top-level fields (the synthetic paint fixtures).
        self.windows = match map_get(map, "windows") {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|w| match w {
                    Value::Map(wm) => Some(parse_window(wm, &self.styles)),
                    _ => None,
                })
                .collect(),
            _ => vec![parse_window(map, &self.styles)],
        };
        self.separators = parse_separators(map_get(map, "separators"));
        // The permanent dock band sizes (0 = closed); absent on an older server.
        self.dock_left = map_u16(map, "dock_left");
        self.dock_right = map_u16(map, "dock_right");
        self.dock_top = map_u16(map, "dock_top");
        self.dock_bottom = map_u16(map, "dock_bottom");
        // The global status line (`laststatus=3`); empty/absent for per-window modes.
        self.global_status = parse_status(map_get(map, "global_status"), &self.styles);
        self.tabline = parse_tabline(map_get(map, "tabline"));
        self.tabline_segments = parse_status(map_get(map, "tabline_segments"), &self.styles);
        self.current_tab = map_u64(map, "current_tab") as usize;
        self.region_tablines = parse_region_tablines(map_get(map, "region_tablines"));
        self.pmenu = match map_get(map, "pmenu") {
            Some(Value::Map(p)) => Some(PmenuData {
                items: parse_pmenu_items(map_get(p, "items")),
                selected: map_get(p, "selected")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
                row: map_u16(p, "row"),
                col: map_u16(p, "col"),
                width: map_u16(p, "width"),
                height: map_u16(p, "height"),
                doc: map_str_array(p, "doc"),
            }),
            _ => None,
        };
        self.menu = match map_get(map, "menu") {
            Some(Value::Map(m)) => Some(MenuData {
                items: map_str_array(m, "items"),
                selected: map_u64(m, "selected") as usize,
                selected_active: map_get(m, "selected_active").and_then(Value::as_bool)
                    != Some(false),
                row: map_u16(m, "row"),
                col: map_u16(m, "col"),
                width: map_u16(m, "width"),
                height: map_u16(m, "height"),
                query: map_get(m, "query")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                query_cursor: map_u16(m, "query_cursor"),
                prompt_bottom: map_get(m, "prompt_pos").and_then(Value::as_str) == Some("bottom"),
                border_top: map_get(m, "border_top").and_then(Value::as_bool) != Some(false),
                cmdline: map_get(m, "cmdline").and_then(Value::as_bool) == Some(true),
                match_spans: parse_multi_spans(map_get(m, "match_spans")),
                preview: match map_get(m, "preview") {
                    Some(Value::Map(p)) => Some(MenuPreview {
                        lines: map_str_array(p, "lines"),
                        first_line: map_u64(p, "first_line") as u32,
                        title: map_str(p, "title"),
                        width: map_u16(p, "width"),
                        loc: parse_pair(map_get(p, "loc")),
                        highlights: parse_highlights(map_get(p, "highlights")),
                    }),
                    _ => None,
                },
                docs: match map_get(m, "docs") {
                    Some(Value::Map(d)) => Some(MenuDocs {
                        lines: map_str_array(d, "lines"),
                        row: map_u16(d, "row"),
                        col: map_u16(d, "col"),
                        width: map_u16(d, "width"),
                        height: map_u16(d, "height"),
                    }),
                    _ => None,
                },
            }),
            _ => None,
        };
        self.content_float = match map_get(map, "float") {
            Some(Value::Map(f)) => Some(ContentFloatData {
                lines: parse_float_lines(map_get(f, "lines")),
                row: map_u16(f, "row"),
                col: map_u16(f, "col"),
                width: map_u16(f, "width"),
                height: map_u16(f, "height"),
                border: parse_border(map_get(f, "border")),
                title: map_get(f, "title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            }),
            _ => None,
        };
        // Collapsed-dock chips (toggle / auto-hide); absent on an older server.
        self.hidden_docks = map_str_array(map, "hidden_docks");
    }

    /// The focused window — where the terminal cursor is drawn and the reference
    /// point for the completion popup — or `None` before the first redraw (a
    /// default `View` has no windows). A real redraw always carries at least one
    /// window, with one flagged focused; falls back to the first if none is.
    pub fn focused(&self) -> Option<&WindowView> {
        self.windows
            .iter()
            .find(|w| w.focused)
            .or_else(|| self.windows.first())
    }

    /// Whether the editor is in insert mode, mirrored from the server's
    /// `mode_label`. Drives the thin-bar "edit cursor" shape.
    pub fn is_insert(&self) -> bool {
        self.mode_label == "INSERT"
    }

    /// Whether the editor is replacing: either `R` replace mode (`mode_label`)
    /// or `r` waiting for its one replacement char. Both drive the underline
    /// cursor shape, matching vim's replace/operator-pending feedback.
    pub fn is_replace(&self) -> bool {
        self.mode_label == "REPLACE" || self.pending_replace
    }

    /// Whether the editor is in multi-cursor *placement* mode, mirrored from the
    /// server's `mode_label`. The renderer recolors the active (primary) cursor
    /// while it's here, signaling that motions drop cursors rather than edit.
    pub fn is_multicursor(&self) -> bool {
        self.mode_label == "MULTICURSOR"
    }

    /// Build a view from a `redraw` notification's params — the client's own
    /// parsing path — so tests and tools can paint a known view.
    pub fn from_redraw(params: &[Value]) -> Self {
        let mut view = View::default();
        view.update(params);
        view
    }
}

/// Parse one window from a map slice — either a `windows[i]` sub-map (with a
/// `rect`) or the legacy flat top-level redraw (no `rect`, so the renderer gives
/// the single window the whole windows area). `styles` is the global palette the
/// window's scroll band snapshots.
fn parse_window(m: &[(Value, Value)], styles: &[Style]) -> WindowView {
    let rect = match map_get(m, "rect") {
        Some(Value::Map(r)) => Some(WinRect {
            x: map_u16(r, "x"),
            y: map_u16(r, "y"),
            width: map_u16(r, "width"),
            height: map_u16(r, "height"),
        }),
        _ => None,
    };
    let region =
        WindowRegion::from_wire(map_get(m, "region").and_then(Value::as_str).unwrap_or(""));
    let scroll = match map_get(m, "scroll") {
        Some(Value::Map(s)) => Some(ScrollData {
            from_row: map_u64(s, "from_row") as f32,
            to_row: map_u64(s, "to_row") as f32,
            from_cursor_row: map_u64(s, "from_cursor_row") as f32,
            to_cursor_row: map_u64(s, "to_cursor_row") as f32,
            duration: Duration::from_millis(map_u64(s, "duration_ms")),
            lines: map_str_array(s, "lines"),
            selection: parse_spans(map_get(s, "selection")),
            secondary_selection: parse_multi_spans(map_get(s, "secondary_selection")),
            sel_extends_down: map_get(s, "sel_extends_down").and_then(Value::as_bool),
            search: parse_multi_spans(map_get(s, "search")),
            incsearch: parse_spans(map_get(s, "incsearch")),
            numbers: parse_numbers(map_get(s, "numbers")),
            continuation: parse_bools(map_get(s, "continuation")),
            highlights: parse_highlights(map_get(s, "highlights")),
            inlay_hints: parse_inlay_hints(map_get(s, "inlay_hints")),
            virt_text: parse_virt_text(map_get(s, "virt_text")),
            virt_lines: parse_virt_lines(map_get(s, "virt_lines")),
            diagnostics: parse_diagnostics(map_get(s, "diagnostics")),
            diagnostics_signs: parse_diagnostics_signs(map_get(s, "diagnostics_signs")),
            diagnostics_virt: parse_diagnostics_virt(map_get(s, "diagnostics_virt")),
            // The band's ids index this redraw's palette — snapshot it now, since
            // a later redraw will replace the live `styles`.
            styles: styles.to_vec(),
        }),
        _ => None,
    };
    WindowView {
        rect,
        region,
        // A flat redraw has no `focused` flag; its sole window is always focused.
        focused: map_get(m, "focused")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        lines: map_str_array(m, "lines"),
        cursor_row: map_u16(m, "cursor_row"),
        cursor_screen_col: map_u16(m, "cursor_screen_col"),
        // Default 1 so an older server (no key) gives an ordinary one-cell cursor.
        cursor_width: map_get(m, "cursor_width")
            .and_then(Value::as_u64)
            .map(|n| n as u16)
            .unwrap_or(1)
            .max(1),
        secondary_cursors: parse_cursor_list(map_get(m, "cursors")),
        leftcol: map_u16(m, "leftcol"),
        status: parse_status(map_get(m, "status"), styles),
        // Default true so an older server (no flag) keeps the per-window status.
        status_visible: map_get(m, "status_visible")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        cursor_line: map_u64(m, "cursor_line") as usize,
        selection: parse_spans(map_get(m, "selection")),
        secondary_selection: parse_multi_spans(map_get(m, "secondary_selection")),
        search: parse_multi_spans(map_get(m, "search")),
        incsearch: parse_spans(map_get(m, "incsearch")),
        highlights: parse_highlights(map_get(m, "highlights")),
        diagnostics: parse_diagnostics(map_get(m, "diagnostics")),
        diagnostics_virt: parse_diagnostics_virt(map_get(m, "diagnostics_virt")),
        virt_text: parse_virt_text(map_get(m, "virt_text")),
        virt_lines: parse_virt_lines(map_get(m, "virt_lines")),
        diagnostics_signs: parse_diagnostics_signs(map_get(m, "diagnostics_signs")),
        sign_width: map_u16(m, "sign_width"),
        inlay_hints: parse_inlay_hints(map_get(m, "inlay_hints")),
        scroll,
        numbers: parse_numbers(map_get(m, "numbers")),
        continuation: parse_bools(map_get(m, "continuation")),
        number: map_get(m, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        relativenumber: map_get(m, "relativenumber")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cursorline: map_get(m, "cursorline")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        number_width: map_u16(m, "number_width"),
        // `'padding'` as `[top, right, bottom, left]` cells (CSS order); absent ⇒
        // no margin (an older server, or the default).
        padding: parse_padding(map_get(m, "padding")),
        // The server always sends a tabstop ≥ 1; treat a missing/0 value as the
        // historical default of 8 so an older server still renders sanely.
        tabstop: match map_u16(m, "tabstop") {
            0 => 8,
            ts => ts,
        },
        floating: map_get(m, "floating")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        border: parse_border(map_get(m, "border")),
        title: {
            let t = map_str(m, "title");
            (!t.is_empty()).then_some(t)
        },
        unnamed: map_get(m, "unnamed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image: match map_get(m, "image") {
            Some(Value::Map(im)) => Some(ImageData {
                path: map_str(im, "path"),
                size: map_u64(im, "size"),
                mtime_ms: map_u64(im, "mtime_ms"),
                remote: map_get(im, "remote")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            _ => None,
        },
    }
}

/// Parse the `tabline` array: each entry a `{ label, modified, window_count }`
/// map. Empty (so no tabline drawn) when only one tab is open.
fn parse_tabline(value: Option<&Value>) -> Vec<TabData> {
    let Some(Value::Array(arr)) = value else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| match v {
            Value::Map(t) => Some(TabData {
                label: map_str(t, "label"),
                modified: map_get(t, "modified")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                window_count: map_u64(t, "window_count") as usize,
            }),
            _ => None,
        })
        .collect()
}

/// Parse the `region_tablines` map (`{ main, left, right, top, bottom }`, each a
/// `{ tabs, current, title }`). Absent on an older server ⇒ all-empty (no
/// per-region tablines drawn).
fn parse_region_tablines(value: Option<&Value>) -> RegionTablines {
    let Some(Value::Map(m)) = value else {
        return RegionTablines::default();
    };
    let region = |key: &str| -> RegionTabline {
        match map_get(m, key) {
            Some(Value::Map(r)) => RegionTabline {
                tabs: parse_tabline(map_get(r, "tabs")),
                current: map_u64(r, "current") as usize,
                title: map_str(r, "title"),
            },
            _ => RegionTabline::default(),
        }
    };
    RegionTablines {
        main: region("main"),
        left: region("left"),
        right: region("right"),
        top: region("top"),
        bottom: region("bottom"),
    }
}

/// Parse the `separators` array: each entry a `{ vertical, x, y, length }` map.
fn parse_separators(value: Option<&Value>) -> Vec<Separator> {
    let Some(Value::Array(arr)) = value else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| match v {
            Value::Map(s) => Some(Separator {
                vertical: map_get(s, "vertical")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                x: map_u16(s, "x"),
                y: map_u16(s, "y"),
                length: map_u16(s, "length"),
                region: WindowRegion::from_wire(
                    map_get(s, "region").and_then(Value::as_str).unwrap_or(""),
                ),
            }),
            _ => None,
        })
        .collect()
}
