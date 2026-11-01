//! Extmarks: buffer-anchored marks that carry a highlight group and shift with
//! edits. The foundational "highlight-layering primitive" plugins (and, later,
//! LSP semantic tokens) build on — projected into the redraw highlight payload
//! alongside treesitter and diagnostics. See
//! `docs/specs/2026-06-07-extmark-decoration-layer-design.md`.
//!
//! Anchoring is **byte-offset based**, like the rest of bemtvi's text model: a
//! mark stores a single `start` byte (and optional `end` byte for a range),
//! and screen columns are derived at projection time. The 2-D `(row, col)`
//! neovim exposes is reconstructed from the byte offset against the live rope.
//!
//! The store lives on the [`Buffer`](crate::buffer::Buffer) so the single edit
//! choke point ([`Buffer::record`](crate::buffer::Buffer)) can shift every
//! mark's anchors for free, across every edit path.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

/// Reserved namespace for the editor's secondary (multi-)cursors. They are
/// stored as point extmarks so the buffer's single edit choke point
/// ([`ExtmarkStore::shift`]) keeps every extra cursor's byte anchor correct
/// across edits for free. The id sits at the top of the `u32` range, far above
/// any namespace `nvim_create_namespace` hands out, so it never collides with a
/// plugin's. Carrying no `hl_group`/`end`, these marks render nothing and are
/// filtered out of the user-facing extmark mirror.
pub const CURSOR_NS: u32 = u32::MAX;

/// Reserved namespace for the per-cursor *visual anchors* of multi-cursor visual
/// mode. While a visual mode is active each secondary cursor's selection runs
/// from its [`CURSOR_NS`] head mark to a paired anchor mark here — **same id in
/// both namespaces**, so the two are looked up together. Like [`CURSOR_NS`] these
/// are point marks carrying no `hl_group`, so they render nothing and are kept
/// out of the user-facing extmark mirror. They exist only between entering visual
/// mode and the operator/`<Esc>` that ends it. Sits just below [`CURSOR_NS`], far
/// above any plugin namespace.
pub const ANCHOR_NS: u32 = u32::MAX - 1;

/// Reserved namespace for the active snippet session's tabstops. Each tabstop
/// occurrence (primary or mirror) is a **range** extmark here, so the buffer's
/// single edit choke point ([`ExtmarkStore::shift`]) keeps every tabstop's byte
/// range correct as the user types into the snippet. Unlike [`CURSOR_NS`] these
/// carry an `hl_group` (`SnippetTabstop` / `SnippetTabstopActive`) so clients can
/// paint the placeholders, and they are surfaced through the dedicated snippet
/// span projection rather than the user-facing extmark mirror. They exist only
/// for the lifetime of a [`crate::editor::snippet::SnippetSession`]. Sits just
/// below [`ANCHOR_NS`], far above any plugin namespace.
pub const SNIPPET_NS: u32 = u32::MAX - 2;

/// Reserved namespace for the highlights a built-in **listing panel** paints on
/// its own lines — e.g. `:messages` flagging each error line `ErrorMsg`. These
/// are range extmarks carrying an `hl_group`, set on the (reused) panel buffer
/// right after its content is loaded and cleared on the next reload, so they
/// never collide with a plugin namespace. Sits just below [`SNIPPET_NS`].
pub const LISTING_HL_NS: u32 = u32::MAX - 3;

/// Reserved namespace for the markdown doc-float's rendered styling — the inline
/// `@markup.*` highlight ranges, fenced-code syntax spans, and thematic-break
/// [`line_fill`](VirtDecor::line_fill)s that [`Editor::open_markdown_float`] paints
/// over the (reused) hover/doc scratch buffer, cleared and repainted on each new
/// reply. Like the listing highlights it is a reserved range/decor namespace, set
/// right after the content loads. Sits just below [`LISTING_HL_NS`].
pub const DOC_MD_NS: u32 = u32::MAX - 4;

/// Reserved namespace for the live `:s///` **substitute preview** (the diff
/// overlay shown while a `:[range]s/pat/rep` command line is being typed). Each
/// match gets a range extmark over the matched bytes carrying the
/// `BtvSubstituteDelete` group (the "removed" side) plus — when the replacement is
/// non-empty and single-line — an [`Inline`](VirtTextPos::Inline) `virt_text`
/// extmark at the match end holding the replacement text in `BtvSubstituteAdd`
/// (the "added" side). Populated / cleared by
/// [`Editor::refresh_subst_preview`](crate::editor::Editor::refresh_subst_preview)
/// as the command line changes and torn down when it closes, so — like the other
/// reserved decoration namespaces — it never persists, never enters an undo
/// snapshot, and is kept out of the user-facing extmark mirror. Sits just below
/// [`DOC_MD_NS`].
pub const SUBST_PREVIEW_NS: u32 = u32::MAX - 5;

/// Reserved namespace for the **signature-help** float's active-parameter marker:
/// one [`Overlay`](VirtTextPos::Overlay) `virt_text` extmark per marked row,
/// drawn over the indent of the parameter the cursor sits in (see
/// [`Editor::open_signature_float`](crate::editor::Editor::open_signature_float)).
/// It rides an overlay rather than living in the buffer text so the popup's text
/// stays *valid code* for the tree-sitter pass that colors it — a `▸` spliced into
/// the line would parse as an error node and take that line's highlighting with
/// it. Cleared and repainted with each reply, like the other reserved decoration
/// namespaces. Sits just below [`SUBST_PREVIEW_NS`].
pub const SIGNATURE_NS: u32 = u32::MAX - 6;

/// Reserved namespace anchoring the **diagnostics** currently on screen. One range
/// extmark per diagnostic in the buffer's merged (LSP + client-set) set, carrying no
/// `hl_group` and no decor — it renders nothing itself; the diagnostic projection
/// reads its span and paints the squiggle, sign and inline message from that.
///
/// The point is edit tracking. A diagnostic arrives as an absolute LSP
/// `(line, character)` range, which goes stale the instant you type: a server only
/// re-publishes after a `didChange` round trip, and bemtvi deliberately holds those
/// updates while you type (`vim.diagnostic.config({ update_in_insert = … })`), so
/// between updates the ranges must follow the text themselves. Anchored here they do
/// — the buffer's single edit choke point ([`ExtmarkStore::shift`]) moves them for
/// free, exactly as it does for marks and (in neovim, where diagnostics are extmarks
/// too) for the same reason. Re-placed whenever the applied set changes; like the
/// other reserved namespaces it is kept out of the user-facing extmark mirror. Sits
/// just below [`SIGNATURE_NS`].
pub const DIAGNOSTIC_NS: u32 = u32::MAX - 7;

/// Reserved namespace for the **frame-time paint** a `btv.decor.expr` block asks
/// for. One range extmark per span it returned, carrying an `hl_group`, so the
/// paint rides the ordinary extmark highlight projection to every client with no
/// client change — the same ephemeral-mark route the `:s` diff preview takes.
///
/// Rewritten from scratch for a window whose `(buffer, top, bot, changedtick)`
/// moved, and cleared when the expression is uninstalled. Between those it is
/// carried by the buffer's edit choke point ([`ExtmarkStore::shift`]) like any
/// other mark, so a paint stays on its text while you type on the line. Like the
/// other reserved namespaces it is kept out of the user-facing extmark mirror.
/// Sits just below [`DIAGNOSTIC_NS`].
pub const PAINT_NS: u32 = u32::MAX - 8;

/// neovim's `DEFAULT_PRIO` for extmark highlights — above the treesitter
/// highlighter's baseline ([`TS_HL_PRIORITY`]), so a plugin / semantic-token
/// mark wins over the base syntax color by default.
pub const DEFAULT_PRIORITY: u32 = 4096;

/// Baseline priority the treesitter highlighter paints at (neovim's
/// `vim.highlight.priorities.treesitter`). Extmarks default above this.
pub const TS_HL_PRIORITY: u32 = 100;

/// Priority LSP semantic tokens paint at (neovim's
/// `vim.highlight.priorities.semantic_tokens`): just *above* the treesitter floor
/// ([`TS_HL_PRIORITY`]) — the server's authoritative classification refines the
/// syntactic guess — and *below* [`DEFAULT_PRIORITY`], so a user/plugin extmark
/// still wins over both.
pub const SEMANTIC_HL_PRIORITY: u32 = 125;

/// Priority the `SpecialKey` overlay on an unprintable control char's `^X` /
/// `<xx>` substitution paints at — above everything, since the highlighted cells
/// aren't real buffer content (the char isn't there to be syntax-colored), so no
/// treesitter span, semantic token, or user extmark should bleed onto them.
pub const SPECIAL_KEY_PRIORITY: u32 = u32::MAX;

/// One run of virtual text: a string with an optional highlight group. A
/// `virt_text` / `virt_lines` payload is a list of these (neovim's
/// `{ {text, hl_group}, … }` chunk form). `hl_group == None` paints in the
/// window's normal colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtChunk {
    pub text: String,
    pub hl_group: Option<String>,
}

/// The `virt_lines` (whole extra screen rows) anchored on one buffer line, split
/// by where they draw: `above` the line or `below` it. Each inner `Vec<VirtChunk>`
/// is one virtual line's chunk run. Returned per line by
/// [`Buffer::virt_lines_by_line`](crate::Buffer::virt_lines_by_line).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VirtLineRows {
    pub above: Vec<Vec<VirtChunk>>,
    pub below: Vec<Vec<VirtChunk>>,
}

/// Where a mark's `virt_text` is drawn, relative to the buffer line it anchors
/// to (neovim's `virt_text_pos`, plus the fixed-column `virt_text_win_col`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VirtTextPos {
    /// After the line's last character (the default; the diagnostics path's shape).
    #[default]
    Eol,
    /// Spliced into the line at the mark's column, pushing real text right.
    Inline,
    /// Drawn over the cells starting at the mark's column (replacing them).
    Overlay,
    /// Right-aligned to the window's right edge.
    RightAlign,
    /// Pinned to a fixed window column (0-based), independent of the mark column.
    WinCol(u16),
}

/// How a `virt_text` chunk's highlight combines with whatever it draws over
/// (neovim's `hl_mode`). Only meaningful for `Overlay` / `Inline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HlMode {
    /// The chunk's own `hl_group` fully replaces the underlying highlight.
    #[default]
    Replace,
    /// Combine the chunk highlight with the underlying highlight.
    Combine,
    /// Blend the chunk's background with the underlying text.
    Blend,
}

/// The virtual-text payload of an extmark: `virt_text` (inline / eol / overlay /
/// right-aligned text on the anchored line) and `virt_lines` (whole extra screen
/// rows above or below the line). Boxed onto [`Extmark`] so the common hl-only
/// mark stays small. `None` on an [`Extmark`] means a plain position / highlight
/// mark with no virtual content.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VirtDecor {
    pub virt_text: Vec<VirtChunk>,
    pub virt_text_pos: VirtTextPos,
    /// Hide the `virt_text` when the line is covered (e.g. by a visual block);
    /// honored by the renderer. Not for an `Inline` mark, whose cells are part of the
    /// line's columns.
    pub virt_text_hide: bool,
    /// Paint the `virt_text` with only the **foreground** of its highlight group —
    /// the background (and `reverse`) are dropped, so the surface underneath shows
    /// through. For an overlay glyph that is *chrome* rather than a highlight **of**
    /// text: the signature float's active-parameter caret is drawn in
    /// `LspSignatureActiveParameter`, a group a theme is free to express as a
    /// background band (catppuccin defines nothing but `bg` + `bold`) because
    /// neovim paints it over the parameter's own text. Over a lone `▸` sitting in a
    /// popup's indent that same band renders as a coloured box in the middle of the
    /// float, so the caret takes the group's color and leaves the float's own
    /// background alone. `hl_mode` can't express this: it merges the chunk's *set*
    /// attributes over the cell beneath, and `bg` is exactly what is set.
    pub virt_text_fg_only: bool,
    pub hl_mode: HlMode,
    /// Each inner `Vec` is one virtual line's chunk run; the outer order is the
    /// top-to-bottom draw order.
    pub virt_lines: Vec<Vec<VirtChunk>>,
    /// Draw `virt_lines` above the anchored line rather than below it.
    pub virt_lines_above: bool,
    /// A gutter sign glyph (1–2 cells, neovim's `sign_text`) drawn in the sign
    /// column on the anchored line's first display row. `None` ⇒ no sign.
    pub sign_text: Option<String>,
    /// The highlight group for [`sign_text`](Self::sign_text) (neovim's
    /// `sign_hl_group`); `None` paints the glyph in normal colors.
    pub sign_hl_group: Option<String>,
    /// An `btv`-native row fill: the [`text`](VirtChunk::text) is repeated from the end
    /// of the anchored line's own text to the right edge of the window's text area, in
    /// [`hl_group`](VirtChunk::hl_group). On a blank line that is the whole row (a `-`
    /// rule on an alignment / filler row); on a line that carries text the text stays
    /// and the fill runs out from it, which is how a labelled rule
    /// (`─ pyright ──────`, the doc float's section header) is drawn. Rendered as an
    /// overlay so a client needs no new wire field. `None` ⇒ no fill.
    pub line_fill: Option<VirtChunk>,
    /// neovim's `line_hl_group` — a highlight group that backs the **whole line**
    /// (full width, `hl_eol` semantics), not a char range. A mark carrying it means
    /// "tint this line's background with that group". Unlike an `hl_group` range span
    /// (which merges into the winner-takes-cell resolution and so loses every cell a
    /// syntax span covers, and never reaches past the text) this projects as a
    /// separate per-window `line_bg` layer the clients paint *under* the text — the
    /// [`cursorline`](crate::view::WindowView::cursorline) model — so syntax colouring
    /// composes on top and the fill spans the full width. Used by the rendered
    /// markdown doc floats to back fenced code blocks. `None` ⇒ no line background.
    pub line_hl_group: Option<String>,
    /// `btv`-native: the anchored line's text in the **gutter text** column, the
    /// column between the line numbers and the text (a diff's old and new line
    /// numbers, a blame hash). Shown on the line's first display row only. The column
    /// is as wide as the widest `gutter_text` in the buffer
    /// ([`ExtmarkStore::gutter_text_width`]), so a shorter one is padded on the right.
    /// A chunk with no group paints as `LineNr` (`CursorLineNr` on the cursor line).
    /// Empty ⇒ the mark puts nothing there.
    pub gutter_text: Vec<VirtChunk>,
}

impl VirtDecor {
    /// The display width of [`gutter_text`](Self::gutter_text), in cells, summed per
    /// grapheme as the clients measure what they paint.
    pub fn gutter_text_width(&self) -> usize {
        use unicode_segmentation::UnicodeSegmentation;
        self.gutter_text
            .iter()
            .flat_map(|c| c.text.graphemes(true))
            .map(crate::unicode::display_width)
            .sum()
    }
}

/// A single extmark, identified within its buffer by `(namespace, id)`.
///
/// `start`/`end` are byte offsets into the buffer rope, kept current by
/// [`ExtmarkStore::shift`]. `end` is `None` for a *point* mark (no range). A
/// point mark with neither `hl_group` nor [`decor`](Self::decor) still tracks
/// its position (for `get_extmarks`); `decor` carries any virtual text/lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extmark {
    pub id: u64,
    pub start: usize,
    pub end: Option<usize>,
    pub hl_group: Option<String>,
    pub priority: u32,
    pub decor: Option<Box<VirtDecor>>,
    /// Which way [`start`](Self::start) is dragged when text is inserted *at* it
    /// (neovim's `right_gravity`, default `true`). `true` ⇒ the anchor slides right
    /// with the inserted text (the insert lands to its *left*, outside a range);
    /// `false` ⇒ it stays put (the insert lands to its *right*, growing a range from
    /// the left edge). See [`ExtmarkStore::shift`].
    pub right_gravity: bool,
    /// Which way [`end`](Self::end) is dragged when text is inserted *at* it
    /// (neovim's `end_right_gravity`, default `false`). `false` ⇒ the end stays put
    /// (the insert lands to its *right*, outside a range); `true` ⇒ it slides right,
    /// growing a range from the right edge. The two flags together let an *active*
    /// snippet tabstop (`right_gravity = false`, `end_right_gravity = true`) grow
    /// from an empty range as the user types into it — the case fixed gravity can't
    /// express.
    pub end_right_gravity: bool,
    /// Which placement this is: a store-wide counter value assigned by every
    /// [`set`](ExtmarkStore::set), never by [`shift`](ExtmarkStore::shift). An id
    /// re-used for a new placement (a mark deleted and placed again, or moved by
    /// setting its id) gets a new serial, so undo can tell "the mark it snapshotted,
    /// since moved by edits" from "a different placement under the same id". See
    /// [`ExtmarkStore::adopt_positions_from`].
    serial: u64,
}

/// Marks within one namespace, plus that namespace's monotonic id allocator and
/// a structural-mutation counter.
///
/// Ids are never reused, matching neovim (a deleted id is gone for good).
///
/// `marks` is keyed by *id*, so [`ExtmarkStore::shift`]'s per-edit walk covers
/// every mark in the namespace — O(E), the honest bound (an anchor's new
/// position depends only on its own anchors and the edit, so a position index
/// cannot reduce the walk below one visit per mark, and per-mark index
/// maintenance measured 59x slower in round 3). The `gen` counter, bumped by
/// every structural mutation and untouched by shifts, lets a caller prove the
/// marks are the ones *it* placed (see the diagnostics anchor-trust guard in
/// `bemtvi-server`). Undo keeps the live store (see
/// [`ExtmarkStore::adopt_positions_from`]), so `gen` survives it too.
#[derive(Debug, Default, Clone)]
struct NsMarks {
    marks: BTreeMap<u64, Extmark>,
    /// Structural-mutation counter: bumped by `set` / `del` / `clear` /
    /// `clear_all` / `move_namespace_into`, never by [`ExtmarkStore::shift`].
    gen: u64,
    next_id: u64,
}

/// All extmarks of one buffer, partitioned by namespace id. A namespace id is an
/// opaque `u32` key here — the name↔id registry lives at the scripting layer
/// (`nvim_create_namespace`); core only ever sees ids.
///
/// `Clone` so undo/redo can snapshot and restore it: the marks at each history
/// point ride with that point, surviving the wholesale-rope-replace `mark_resync`
/// (which otherwise clears them, for a destructive reload).
#[derive(Debug, Default, Clone)]
pub struct ExtmarkStore {
    by_ns: HashMap<u32, NsMarks>,
    /// Bumped by every **structural** change — a mark set, deleted, or cleared, and a
    /// namespace moved in or out — but deliberately NOT by [`shift`](Self::shift),
    /// which moves byte anchors and touches nothing else.
    ///
    /// That asymmetry is what lets the Rust→Lua mirror stop re-serializing every
    /// mark's decorations on every keystroke: `hl_group`, `priority`, the sign /
    /// line-fill / line-hl payloads and the gravity flags are fixed for a mark's
    /// lifetime, so they only need re-pushing when this counter moves. An edit
    /// refreshes positions alone. See
    /// `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
    generation: u64,
    /// How many marks carry `virt_lines`. Maintained on every structural mutation so
    /// [`Buffer::virt_lines_by_line`](crate::Buffer::virt_lines_by_line) — called from
    /// the view, cursor and mouse walks, several times per frame — can answer "none"
    /// in O(1) instead of filtering every mark. [`shift`](Self::shift) never changes
    /// it: it moves anchors, not payloads.
    virt_lines_marks: usize,
    /// How many marks carry a `gutter_text` of each display width (only widths
    /// above zero are keyed). The gutter text column is as wide as the widest of
    /// them, which the view, cursor and mouse arithmetic all read every frame, so it
    /// is kept here on every structural mutation rather than found by scanning the
    /// marks: the largest key is the width. [`shift`](Self::shift) never changes
    /// it, for the same reason as [`virt_lines_marks`](Self::virt_lines_marks).
    gutter_widths: BTreeMap<usize, usize>,
    /// The next [`Extmark::serial`] to hand out. Store-wide and monotonic, carried by
    /// every clone, so a snapshot's serials never collide with later placements.
    next_serial: u64,
    /// Every mark's position, sorted, for the per-frame range queries
    /// ([`starting_in`](Self::starting_in), [`reaching`](Self::reaching)) — so a frame
    /// visits the marks on screen rather than every mark in the buffer. Built on the
    /// first query after a structural change, kept sorted by [`shift`](Self::shift),
    /// and not carried by a clone. See [`PosIndex`].
    index: PosIndex,
}

/// Whether `m` carries `virt_lines` rows, the one payload
/// [`ExtmarkStore::virt_lines_marks`] counts.
fn has_virt_lines(m: &Extmark) -> bool {
    m.decor.as_deref().is_some_and(|d| !d.virt_lines.is_empty())
}

/// The display width of `m`'s `gutter_text`, `0` for a mark with none — the key
/// [`ExtmarkStore::gutter_widths`] counts it under.
fn gutter_width(m: &Extmark) -> usize {
    m.decor.as_deref().map_or(0, VirtDecor::gutter_text_width)
}

/// Count `width` in or out of a [`gutter_widths`](ExtmarkStore::gutter_widths) map.
fn count_gutter(widths: &mut BTreeMap<usize, usize>, width: usize, add: bool) {
    if width == 0 {
        return;
    }
    if add {
        *widths.entry(width).or_default() += 1;
    } else if let Some(n) = widths.get_mut(&width) {
        *n -= 1;
        if *n == 0 {
            widths.remove(&width);
        }
    }
}

impl ExtmarkStore {
    /// Record a structural change: bump the [`generation`](Self::generation) and drop
    /// the position index, which no longer lists the marks there are.
    fn touch_structure(&mut self) {
        self.generation += 1;
        self.index = PosIndex::default();
    }

    /// Every mark whose `start` lies in `[lo, hi)`, with its namespace, in
    /// `(start, namespace, id)` order. O(log E + k) for k results, from the position
    /// index — what a per-frame layer anchored on the visible lines asks for.
    pub fn starting_in(&self, lo: usize, hi: usize) -> impl Iterator<Item = (u32, &Extmark)> + '_ {
        let ix = self.index();
        let from = ix.entries.partition_point(|e| e.start < lo);
        let to = ix.entries.partition_point(|e| e.start < hi).max(from);
        ix.entries[from..to].iter().map(|e| (e.ns, self.mark_of(e)))
    }

    /// Every mark that reaches into `[lo, hi)`: `start < hi` and its `end` (its
    /// `start`, for a point mark) at or past `lo`, with its namespace, in
    /// `(start, namespace, id)` order. This is the query for ranges that start above
    /// the viewport and end inside it, which [`starting_in`](Self::starting_in) misses.
    /// A max-end tree over the index makes it O((k + 1) log E) for k results: one long
    /// mark doesn't make every query walk the marks before it.
    ///
    /// Inclusive at `lo` so a zero-width range sitting exactly on it is found; a
    /// caller that wants `end > lo` filters the one extra case.
    pub fn reaching(&self, lo: usize, hi: usize) -> Vec<(u32, &Extmark)> {
        let ix = self.index();
        let upto = ix.entries.partition_point(|e| e.start < hi);
        let mut hits = Vec::new();
        ix.reaching(upto, lo, &mut hits);
        hits.into_iter()
            .map(|i| {
                let e = &ix.entries[i];
                (e.ns, self.mark_of(e))
            })
            .collect()
    }

    /// The position index, built now if a structural change dropped it.
    fn index(&self) -> &Positions {
        self.index.0.get_or_init(|| Positions::build(&self.by_ns))
    }

    /// The mark an index entry names. The index lists exactly the marks there are
    /// (every structural change drops it), so the lookup can't miss.
    fn mark_of(&self, e: &PosEntry) -> &Extmark {
        self.get(e.ns, e.id)
            .expect("the position index lists only marks the store holds")
    }

    /// Set (create or replace) an extmark. With `id == None` a fresh id is
    /// allocated from the namespace's counter; with `Some(id)` the caller's id is
    /// used (replacing any existing mark) and the counter is advanced past it so
    /// a later auto-id can't collide. Returns the id used.
    #[allow(clippy::too_many_arguments)] // positional mark setter; fields mirror neovim's
    pub fn set(
        &mut self,
        ns: u32,
        id: Option<u64>,
        start: usize,
        end: Option<usize>,
        hl_group: Option<String>,
        priority: u32,
        decor: Option<Box<VirtDecor>>,
    ) -> u64 {
        // Default gravity (neovim's): start right-gravity, end left-gravity — a range
        // that does *not* grow when text is typed at either boundary.
        self.set_with_gravity(ns, id, start, end, hl_group, priority, decor, true, false)
    }

    /// Like [`set`](Self::set) but with explicit anchor gravity (see
    /// [`Extmark::right_gravity`] / [`Extmark::end_right_gravity`]). The `btv.buf.set_extmark`
    /// bridge routes through here so a plugin snippet engine can place a *growing*
    /// tabstop; the internal callers use [`set`](Self::set)'s default gravity.
    #[allow(clippy::too_many_arguments)] // positional mark setter; fields mirror neovim's
    pub fn set_with_gravity(
        &mut self,
        ns: u32,
        id: Option<u64>,
        start: usize,
        end: Option<usize>,
        hl_group: Option<String>,
        priority: u32,
        decor: Option<Box<VirtDecor>>,
        right_gravity: bool,
        end_right_gravity: bool,
    ) -> u64 {
        self.touch_structure();
        let slot = self.by_ns.entry(ns).or_default();
        let id = match id {
            Some(id) => {
                // A caller-supplied id must not roll the counter backwards, but
                // `id + 1` would wrap (and panic in debug) on `u64::MAX` — a
                // malicious/errored plugin id must not take the store down.
                slot.next_id = slot.next_id.max(id.saturating_add(1));
                id
            }
            None => {
                // Saturate, never wrap: a caller-supplied `u64::MAX` id parks the
                // counter there (above), and `+= 1` from `u64::MAX` would panic in
                // debug and wrap to 0 in release — silently re-issuing id 0 over an
                // existing mark. At the parked value every auto-id returns
                // `u64::MAX`, replacing only that one mark; no other id is touched.
                let id = slot.next_id;
                slot.next_id = slot.next_id.saturating_add(1);
                id
            }
        };
        self.next_serial += 1;
        let mark = Extmark {
            id,
            start,
            end,
            hl_group,
            priority,
            decor,
            right_gravity,
            end_right_gravity,
            serial: self.next_serial,
        };
        let gained = has_virt_lines(&mark);
        count_gutter(&mut self.gutter_widths, gutter_width(&mark), true);
        let replaced = slot.marks.insert(id, mark);
        // `insert` hands back whatever occupied this id, which is the only way to
        // know whether the count should also drop for a mark just overwritten.
        let lost = replaced.as_ref().is_some_and(has_virt_lines);
        if let Some(old) = &replaced {
            count_gutter(&mut self.gutter_widths, gutter_width(old), false);
        }
        slot.gen += 1;
        self.virt_lines_marks = self.virt_lines_marks + usize::from(gained) - usize::from(lost);
        id
    }

    /// Look up one mark by `(namespace, id)`.
    pub fn get(&self, ns: u32, id: u64) -> Option<&Extmark> {
        self.by_ns.get(&ns)?.marks.get(&id)
    }

    /// Delete one mark; returns whether it existed.
    pub fn del(&mut self, ns: u32, id: u64) -> bool {
        self.touch_structure();
        let removed = match self.by_ns.get_mut(&ns) {
            Some(slot) => match slot.marks.remove(&id) {
                Some(m) => {
                    slot.gen += 1;
                    Some(m)
                }
                None => None,
            },
            None => None,
        };
        if removed.as_ref().is_some_and(has_virt_lines) {
            self.virt_lines_marks -= 1;
        }
        if let Some(m) = &removed {
            count_gutter(&mut self.gutter_widths, gutter_width(m), false);
        }
        removed.is_some()
    }

    /// Remove every mark of namespace `ns` whose `start` falls in the byte range
    /// `range` (`None` ⇒ the whole buffer). Mirrors
    /// `nvim_buf_clear_namespace`, whose line range the caller converts to bytes.
    pub fn clear(&mut self, ns: u32, range: Option<std::ops::Range<usize>>) {
        self.touch_structure();
        let Some(slot) = self.by_ns.get_mut(&ns) else {
            return;
        };
        match range {
            None => {
                let dropped = slot.marks.values().filter(|m| has_virt_lines(m)).count();
                for m in slot.marks.values() {
                    count_gutter(&mut self.gutter_widths, gutter_width(m), false);
                }
                slot.marks.clear();
                slot.gen += 1;
                self.virt_lines_marks -= dropped;
            }
            Some(r) => {
                // Counted inside the retain rather than by scanning twice around it:
                // `retain` already visits every mark, so the bookkeeping is free.
                let mut dropped = 0usize;
                let widths = &mut self.gutter_widths;
                slot.marks.retain(|_, m| {
                    if r.contains(&m.start) {
                        if has_virt_lines(m) {
                            dropped += 1;
                        }
                        count_gutter(widths, gutter_width(m), false);
                        false
                    } else {
                        true
                    }
                });
                slot.gen += 1;
                self.virt_lines_marks -= dropped;
            }
        }
    }

    /// Drop every mark in every namespace — used when the whole rope is replaced
    /// (undo/redo, file reload), where byte anchors are meaningless against the
    /// new text and there is nothing to rebuild them from.
    pub fn clear_all(&mut self) {
        self.touch_structure();
        self.by_ns.clear();
        self.virt_lines_marks = 0;
        self.gutter_widths.clear();
    }

    /// Move namespace `ns`'s marks out of `self` and into `dst`, replacing whatever
    /// `dst` held for `ns` (dropping `dst`'s slot when `self` has none). Used to carry
    /// **ephemeral** marks (viewport decoration-provider publishes) across an undo
    /// restore that swaps the whole store: the live marks are moved into the snapshot
    /// store about to be installed, so undo never wipes them. See
    /// [`crate::editor::Editor::restore_snapshot`].
    pub fn move_namespace_into(&mut self, ns: u32, dst: &mut ExtmarkStore) {
        self.touch_structure();
        dst.touch_structure();
        // Whatever `dst` held for `ns` is discarded either way, so its count sheds
        // that slot before taking on ours.
        let displaced = dst.by_ns.get(&ns).map_or(0, |s| {
            s.marks.values().filter(|m| has_virt_lines(m)).count()
        });
        dst.virt_lines_marks -= displaced;
        if let Some(s) = dst.by_ns.get(&ns) {
            for m in s.marks.values() {
                count_gutter(&mut dst.gutter_widths, gutter_width(m), false);
            }
        }
        match self.by_ns.remove(&ns) {
            Some(slot) => {
                let moved = slot.marks.values().filter(|m| has_virt_lines(m)).count();
                self.virt_lines_marks -= moved;
                dst.virt_lines_marks += moved;
                for m in slot.marks.values() {
                    let w = gutter_width(m);
                    count_gutter(&mut self.gutter_widths, w, false);
                    count_gutter(&mut dst.gutter_widths, w, true);
                }
                dst.by_ns.insert(ns, slot);
            }
            None => {
                dst.by_ns.remove(&ns);
            }
        }
    }

    /// Take an undo/redo history point's mark **positions** without changing which
    /// marks exist: every mark `self` shares with `snap` (the same placement: same
    /// namespace, id and [`serial`](Extmark::serial)) moves back to where `snap` had
    /// it, and every other mark of `self` keeps its place, clamped into the restored
    /// text (`text_len` bytes). Namespaces in `skip` are left alone.
    ///
    /// This is neovim's model: undo restores the text and where marks sit in it, but
    /// the set of marks is the plugins' to manage. A mark deleted through the API
    /// after a history point stays deleted when undo returns to it, and one added
    /// after it survives. Swapping in the whole snapshotted store did neither: it
    /// resurrected a cleared diagnostic or a dismissed intro screen, and wiped a mark
    /// a plugin had just placed. Text edits never delete a mark (they only
    /// [`shift`](Self::shift) it), so membership changes here only through the API.
    /// A mark placed again under the same id after the history point (diagnostics
    /// re-anchoring a new set, a plugin moving its mark) is a new placement: it keeps
    /// its live position, as neovim's undo would, which only reverses text edits.
    pub fn adopt_positions_from(&mut self, snap: &ExtmarkStore, text_len: usize, skip: &[u32]) {
        for (ns, slot) in &mut self.by_ns {
            if skip.contains(ns) {
                continue;
            }
            let theirs = snap.by_ns.get(ns);
            for (id, m) in &mut slot.marks {
                match theirs
                    .and_then(|s| s.marks.get(id))
                    .filter(|then| then.serial == m.serial)
                {
                    Some(then) => {
                        m.start = then.start;
                        m.end = then.end;
                    }
                    None => {
                        m.start = m.start.min(text_len);
                        m.end = m.end.map(|e| e.min(text_len).max(m.start));
                    }
                }
            }
        }
        // Positions moved wholesale: have the Lua mirror re-read the store, and the
        // position index rebuild.
        self.touch_structure();
    }

    /// Every mark across all namespaces (order across namespaces is unspecified;
    /// id-ascending within each). For redraw projection, which priority-sorts.
    pub fn iter_all(&self) -> impl Iterator<Item = &Extmark> {
        self.by_ns.values().flat_map(|s| s.marks.values())
    }

    /// How many marks namespace `ns` holds, in O(1). Lets a caller that addresses
    /// its marks by id check the set is actually *there* before trusting the
    /// addressing — an undo snapshot can restore a store from before the marks were
    /// placed, leaving a bookkeeping count that no longer describes it.
    pub fn ns_len(&self, ns: u32) -> usize {
        self.by_ns.get(&ns).map_or(0, |s| s.marks.len())
    }

    /// The namespace's structural-mutation counter — what the marks' placement
    /// recorded after its last `set`. The diagnostics anchor-trust guard compares
    /// it to the recorded value to reject a store an undo resurrected from before
    /// the placement (its older `gen` fails the equality) while edits that only
    /// *shift* marks pass (a shift never bumps `gen`).
    pub fn ns_generation(&self, ns: u32) -> u64 {
        self.by_ns.get(&ns).map_or(0, |s| s.gen)
    }

    /// Every mark of namespace `ns`, **in ascending id order** (the store keys each
    /// namespace by id in a `BTreeMap`, so this is the map's natural order and costs
    /// no sort). Empty for a namespace holding nothing.
    ///
    /// The ordering is load-bearing for callers that give their marks meaningful ids:
    /// the diagnostic anchors are addressed by their position in the merged
    /// diagnostic list, so walking them here yields them in merged order, which is
    /// what lets the render surfaces select the visible ones without touching the
    /// diagnostics that are off screen.
    pub fn iter_ns(&self, ns: u32) -> impl Iterator<Item = &Extmark> + '_ {
        self.by_ns
            .get(&ns)
            .into_iter()
            .flat_map(|s| s.marks.values())
    }

    /// Every mark paired with its namespace id — for the Rust→Lua mirror that
    /// `nvim_buf_get_extmarks` reads.
    pub fn iter_with_ns(&self) -> impl Iterator<Item = (u32, &Extmark)> {
        self.by_ns
            .iter()
            .flat_map(|(ns, s)| s.marks.values().map(move |m| (*ns, m)))
    }

    /// Whether any namespace holds a mark (empty namespace slots left by `del` /
    /// `clear` don't count). Lets the redraw / mirror skip buffers with none.
    /// Whether any mark carries `virt_lines` — the O(1) gate on the virtual-line
    /// walks. See [`virt_lines_marks`](Self::virt_lines_marks).
    pub fn has_virt_lines(&self) -> bool {
        self.virt_lines_marks > 0
    }

    /// The width in cells of the gutter text column: the widest `gutter_text` any
    /// mark carries, `0` when none does. Read from the per-width counts, never by
    /// walking the marks (see [`gutter_widths`](Self::gutter_widths)).
    pub fn gutter_text_width(&self) -> usize {
        self.gutter_widths.keys().next_back().copied().unwrap_or(0)
    }

    /// The structural [`generation`](Self::generation) — see that field for why a
    /// consumer gates a full re-serialize on it rather than on any change.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_empty(&self) -> bool {
        self.by_ns.values().all(|s| s.marks.is_empty())
    }

    /// Shift all anchors for an edit that replaced `[start, old_end)` with new
    /// content ending at `new_end`. Called from the buffer's single edit choke
    /// point, so every edit path keeps marks correct.
    ///
    /// Gravity is **per mark** (neovim's defaults — `start` right-gravity, `end`
    /// left-gravity — unless the mark opted out via
    /// [`set_with_gravity`](Self::set_with_gravity)): a right-gravity anchor slides
    /// with text inserted exactly at it, a left-gravity one stays put. With the
    /// defaults, typing at either boundary of a highlighted range leaves the
    /// highlighted text unchanged; with `start` left-gravity + `end` right-gravity a
    /// range instead *grows* to swallow text typed at either edge (a live snippet
    /// tabstop). An anchor swallowed by a deletion collapses to the deletion point
    /// rather than vanishing.
    pub fn shift(&mut self, start: usize, old_end: usize, new_end: usize) {
        if old_end == start && new_end == start {
            return; // no-op edit
        }
        // A mark's new anchors depend only on its own anchors and the edit (each
        // gravity fn is applied per anchor), so the walk is a flat pass over every
        // mark — O(E) per namespace, which the never-freeze rule measures and the
        // round-3 benchmark confirmed is the fastest shape: a position index
        // cannot skip the walk (an anchor strictly below the edit is unchanged,
        // but discovering that costs a visit), and per-mark index maintenance
        // measured 59x slower. (The read-side [`PosIndex`] is kept current below
        // with one more flat pass and a local re-sort, not per-mark re-keying.) `gen` is deliberately NOT bumped: a shift is not a
        // structural mutation, and the diagnostics anchor-trust guard relies on
        // `gen` surviving edits.
        for slot in self.by_ns.values_mut() {
            for m in slot.marks.values_mut() {
                m.start = if m.right_gravity {
                    shift_right_gravity(m.start, start, old_end, new_end)
                } else {
                    shift_left_gravity(m.start, start, old_end, new_end)
                };
                if let Some(e) = m.end {
                    let e = if m.end_right_gravity {
                        shift_right_gravity(e, start, old_end, new_end)
                    } else {
                        shift_left_gravity(e, start, old_end, new_end)
                    };
                    // Keep the range non-inverted if a deletion crossed it.
                    m.end = Some(e.max(m.start));
                }
            }
        }
        // The same move for the position index, when one is built, so the next frame
        // doesn't pay a rebuild for an edit.
        if let Some(ix) = self.index.0.get_mut() {
            ix.shift(start, old_end, new_end);
        }
    }
}

/// The lazily-built [`Positions`] of an [`ExtmarkStore`]. Its `Clone` gives an
/// empty one: undo snapshots clone the store, and a snapshot is rarely queried, so
/// it builds its own index only if it ever is rather than carrying a copy.
#[derive(Debug, Default)]
struct PosIndex(OnceLock<Positions>);

impl Clone for PosIndex {
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// One mark in the position index: its anchors (an `end` equal to `start` for a
/// point mark) and gravity, copied so [`Positions::shift`] can move them without a
/// lookup, and its `(ns, id)` key.
#[derive(Debug, Clone, Copy)]
struct PosEntry {
    start: usize,
    end: usize,
    has_end: bool,
    right_gravity: bool,
    end_right_gravity: bool,
    ns: u32,
    id: u64,
}

impl PosEntry {
    fn key(&self) -> (usize, u32, u64) {
        (self.start, self.ns, self.id)
    }
}

/// Every mark's position, sorted by `(start, ns, id)`, with a max-end segment tree
/// over the sorted entries for [`ExtmarkStore::reaching`].
///
/// The entries duplicate the positions the marks hold. Both move in the same
/// [`ExtmarkStore::shift`], with the same gravity rules, and every structural change
/// drops the index, so the two can't drift.
#[derive(Debug)]
struct Positions {
    entries: Vec<PosEntry>,
    /// `tree[node]` is the largest `end` under `node`, the root at 1 and the leaves
    /// from `leaves`, built on the first `reaching` query after an edit.
    tree: OnceLock<(usize, Vec<usize>)>,
}

impl Positions {
    fn build(by_ns: &HashMap<u32, NsMarks>) -> Self {
        let mut entries: Vec<PosEntry> = by_ns
            .iter()
            .flat_map(|(ns, slot)| {
                slot.marks.values().map(move |m| PosEntry {
                    start: m.start,
                    end: m.end.unwrap_or(m.start),
                    has_end: m.end.is_some(),
                    right_gravity: m.right_gravity,
                    end_right_gravity: m.end_right_gravity,
                    ns: *ns,
                    id: m.id,
                })
            })
            .collect();
        entries.sort_unstable_by_key(PosEntry::key);
        Self {
            entries,
            tree: OnceLock::new(),
        }
    }

    /// Move every entry for an edit replacing `[start, old_end)` with text ending at
    /// `new_end`, exactly as [`ExtmarkStore::shift`] moves the marks, and keep the
    /// entries sorted.
    ///
    /// Entries before `start` keep their place, and those at or past `old_end` all move
    /// by the same delta, so their order holds. Only an entry inside the replaced
    /// region (or at `start` itself, for an insertion) can change places, and each of
    /// those lands on `start` or `new_end` — a contiguous run of the vector, with no
    /// entry before it above `start` and none after it below `new_end`. Re-sorting that
    /// run restores the order: typing on a line re-sorts the marks on the line, not the
    /// buffer.
    fn shift(&mut self, start: usize, old_end: usize, new_end: usize) {
        for e in &mut self.entries {
            e.start = if e.right_gravity {
                shift_right_gravity(e.start, start, old_end, new_end)
            } else {
                shift_left_gravity(e.start, start, old_end, new_end)
            };
            e.end = if e.has_end {
                let end = if e.end_right_gravity {
                    shift_right_gravity(e.end, start, old_end, new_end)
                } else {
                    shift_left_gravity(e.end, start, old_end, new_end)
                };
                end.max(e.start)
            } else {
                e.start
            };
        }
        let from = self.entries.partition_point(|e| e.start < start);
        let to = self.entries.partition_point(|e| e.start <= new_end);
        if to > from {
            self.entries[from..to].sort_unstable_by_key(PosEntry::key);
        }
        debug_assert!(
            self.entries.windows(2).all(|w| w[0].key() <= w[1].key()),
            "the position index stays sorted across an edit"
        );
        self.tree = OnceLock::new();
    }

    /// The max-end tree, built now if an edit dropped it: `(leaves, tree)`.
    fn tree(&self) -> &(usize, Vec<usize>) {
        self.tree.get_or_init(|| {
            let leaves = self.entries.len().next_power_of_two().max(1);
            let mut tree = vec![0usize; 2 * leaves];
            for (i, e) in self.entries.iter().enumerate() {
                tree[leaves + i] = e.end;
            }
            for node in (1..leaves).rev() {
                tree[node] = tree[2 * node].max(tree[2 * node + 1]);
            }
            (leaves, tree)
        })
    }

    /// Push, in ascending order, the index of every entry among the first `upto`
    /// whose `end` is at or past `lo`. Descends only into subtrees whose largest
    /// `end` reaches `lo`, so it costs O((k + 1) log E).
    fn reaching(&self, upto: usize, lo: usize, out: &mut Vec<usize>) {
        if upto == 0 {
            return;
        }
        let (leaves, tree) = self.tree();
        // An explicit stack of `(node, first leaf, leaf count)`, right child pushed
        // first so the left one pops first and the results come out ascending.
        let mut stack = vec![(1usize, 0usize, *leaves)];
        while let Some((node, first, count)) = stack.pop() {
            if first >= upto || tree[node] < lo {
                continue;
            }
            if count == 1 {
                out.push(first);
                continue;
            }
            let half = count / 2;
            stack.push((2 * node + 1, first + half, half));
            stack.push((2 * node, first, half));
        }
    }
}

/// Right-gravity anchor shift, mirroring neovim's `extmark_adjust`: an anchor
/// inside the replaced region `[start, old_end)` — including one exactly at
/// `start`, the region's left edge — lands at `new_end` (the new text is
/// "typed at" the anchor, so right-gravity pushes it past). For a pure
/// deletion (`new_end == start`) that is the collapse-to-deletion-point case.
fn shift_right_gravity(p: usize, start: usize, old_end: usize, new_end: usize) -> usize {
    if p < start {
        p
    } else if p < old_end {
        new_end // inside replaced region → its new end (== start for a pure deletion)
    } else {
        // p >= old_end (includes p == start for a pure insertion)
        (p as isize + (new_end as isize - old_end as isize)) as usize
    }
}

/// Left-gravity anchor shift, mirroring neovim's `extmark_adjust`: an anchor
/// exactly at the edit's `start` (the insertion point) stays put; one strictly
/// inside the replaced region collapses to `start`. An anchor at exactly
/// `old_end` — the right edge, the start of the *kept* text — shifts by the
/// delta instead of collapsing (left-gravity: new text is inserted before it,
/// so it stays at the kept text's new left edge).
fn shift_left_gravity(p: usize, start: usize, old_end: usize, new_end: usize) -> usize {
    if p <= start {
        p
    } else if p < old_end {
        start // inside deleted region → collapse to the deletion point
    } else {
        (p as isize + (new_end as isize - old_end as isize)) as usize
    }
}
