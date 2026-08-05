//! Extmarks: buffer-anchored marks that carry a highlight group and shift with
//! edits. The foundational "highlight-layering primitive" plugins (and, later,
//! LSP semantic tokens) build on — projected into the redraw highlight payload
//! alongside treesitter and diagnostics. See
//! `docs/specs/2026-06-07-extmark-decoration-layer-design.md`.
//!
//! Anchoring is **byte-offset based**, like the rest of nxvim's text model: a
//! mark stores a single `start` byte (and optional `end` byte for a range),
//! and screen columns are derived at projection time. The 2-D `(row, col)`
//! neovim exposes is reconstructed from the byte offset against the live rope.
//!
//! The store lives on the [`Buffer`](crate::buffer::Buffer) so the single edit
//! choke point ([`Buffer::record`](crate::buffer::Buffer)) can shift every
//! mark's anchors for free, across every edit path.

use std::collections::{BTreeMap, HashMap};

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
/// `NxSubstituteDelete` group (the "removed" side) plus — when the replacement is
/// non-empty and single-line — an [`Inline`](VirtTextPos::Inline) `virt_text`
/// extmark at the match end holding the replacement text in `NxSubstituteAdd`
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
    /// stored for parity, honored by the renderer.
    pub virt_text_hide: bool,
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
    /// An `nx`-native whole-line fill: the [`text`](VirtChunk::text) is repeated
    /// across the anchored line's text area (e.g. a `-` rule on a blank alignment /
    /// filler row), in [`hl_group`](VirtChunk::hl_group). Rendered as a full-width
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
}

/// Marks within one namespace, plus that namespace's monotonic id allocator.
/// Ids are never reused, matching neovim (a deleted id is gone for good).
#[derive(Debug, Default, Clone)]
struct NsMarks {
    marks: BTreeMap<u64, Extmark>,
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
}

impl ExtmarkStore {
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
    /// [`Extmark::right_gravity`] / [`Extmark::end_right_gravity`]). The `nx.buf.set_extmark`
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
        let slot = self.by_ns.entry(ns).or_default();
        let id = match id {
            Some(id) => {
                slot.next_id = slot.next_id.max(id + 1);
                id
            }
            None => {
                let id = slot.next_id;
                slot.next_id += 1;
                id
            }
        };
        slot.marks.insert(
            id,
            Extmark {
                id,
                start,
                end,
                hl_group,
                priority,
                decor,
                right_gravity,
                end_right_gravity,
            },
        );
        id
    }

    /// Look up one mark by `(namespace, id)`.
    pub fn get(&self, ns: u32, id: u64) -> Option<&Extmark> {
        self.by_ns.get(&ns)?.marks.get(&id)
    }

    /// Delete one mark; returns whether it existed.
    pub fn del(&mut self, ns: u32, id: u64) -> bool {
        match self.by_ns.get_mut(&ns) {
            Some(slot) => slot.marks.remove(&id).is_some(),
            None => false,
        }
    }

    /// Remove every mark of namespace `ns` whose `start` falls in the byte range
    /// `range` (`None` ⇒ the whole buffer). Mirrors
    /// `nvim_buf_clear_namespace`, whose line range the caller converts to bytes.
    pub fn clear(&mut self, ns: u32, range: Option<std::ops::Range<usize>>) {
        let Some(slot) = self.by_ns.get_mut(&ns) else {
            return;
        };
        match range {
            None => slot.marks.clear(),
            Some(r) => slot.marks.retain(|_, m| !r.contains(&m.start)),
        }
    }

    /// Drop every mark in every namespace — used when the whole rope is replaced
    /// (undo/redo, file reload), where byte anchors are meaningless against the
    /// new text and there is nothing to rebuild them from.
    pub fn clear_all(&mut self) {
        self.by_ns.clear();
    }

    /// Move namespace `ns`'s marks out of `self` and into `dst`, replacing whatever
    /// `dst` held for `ns` (dropping `dst`'s slot when `self` has none). Used to carry
    /// **ephemeral** marks (viewport decoration-provider publishes) across an undo
    /// restore that swaps the whole store: the live marks are moved into the snapshot
    /// store about to be installed, so undo never wipes them. See
    /// [`crate::editor::Editor::restore_snapshot`].
    pub fn move_namespace_into(&mut self, ns: u32, dst: &mut ExtmarkStore) {
        match self.by_ns.remove(&ns) {
            Some(slot) => {
                dst.by_ns.insert(ns, slot);
            }
            None => {
                dst.by_ns.remove(&ns);
            }
        }
    }

    /// Every mark across all namespaces (order across namespaces is unspecified;
    /// id-ascending within each). For redraw projection, which priority-sorts.
    pub fn iter_all(&self) -> impl Iterator<Item = &Extmark> {
        self.by_ns.values().flat_map(|s| s.marks.values())
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
    }
}

/// Right-gravity anchor shift: an anchor exactly at the edit's `start` slides
/// with inserted text (stays to its right).
fn shift_right_gravity(p: usize, start: usize, old_end: usize, new_end: usize) -> usize {
    if p < start {
        p
    } else if p < old_end {
        start // inside deleted region → collapse to the deletion point
    } else {
        // p >= old_end (includes p == start for a pure insertion)
        (p as isize + (new_end as isize - old_end as isize)) as usize
    }
}

/// Left-gravity anchor shift: an anchor exactly at the edit's `start` (the
/// insertion point) stays put.
fn shift_left_gravity(p: usize, start: usize, old_end: usize, new_end: usize) -> usize {
    if p <= start {
        p
    } else if p <= old_end {
        start // inside deleted region → collapse to the deletion point
    } else {
        (p as isize + (new_end as isize - old_end as isize)) as usize
    }
}
