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

/// neovim's `DEFAULT_PRIO` for extmark highlights — above the treesitter
/// highlighter's baseline ([`TS_HL_PRIORITY`]), so a plugin / semantic-token
/// mark wins over the base syntax color by default.
pub const DEFAULT_PRIORITY: u32 = 4096;

/// Baseline priority the treesitter highlighter paints at (neovim's
/// `vim.highlight.priorities.treesitter`). Extmarks default above this.
pub const TS_HL_PRIORITY: u32 = 100;

/// A single extmark, identified within its buffer by `(namespace, id)`.
///
/// `start`/`end` are byte offsets into the buffer rope, kept current by
/// [`ExtmarkStore::shift`]. `end` is `None` for a *point* mark (no range); a
/// point mark with only `hl_group` contributes nothing visible in v1 (no
/// virtual text / signs yet) but still tracks its position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extmark {
    pub id: u64,
    pub start: usize,
    pub end: Option<usize>,
    pub hl_group: Option<String>,
    pub priority: u32,
}

/// Marks within one namespace, plus that namespace's monotonic id allocator.
/// Ids are never reused, matching neovim (a deleted id is gone for good).
#[derive(Debug, Default)]
struct NsMarks {
    marks: BTreeMap<u64, Extmark>,
    next_id: u64,
}

/// All extmarks of one buffer, partitioned by namespace id. A namespace id is an
/// opaque `u32` key here — the name↔id registry lives at the scripting layer
/// (`nvim_create_namespace`); core only ever sees ids.
#[derive(Debug, Default)]
pub struct ExtmarkStore {
    by_ns: HashMap<u32, NsMarks>,
}

impl ExtmarkStore {
    /// Set (create or replace) an extmark. With `id == None` a fresh id is
    /// allocated from the namespace's counter; with `Some(id)` the caller's id is
    /// used (replacing any existing mark) and the counter is advanced past it so
    /// a later auto-id can't collide. Returns the id used.
    pub fn set(
        &mut self,
        ns: u32,
        id: Option<u64>,
        start: usize,
        end: Option<usize>,
        hl_group: Option<String>,
        priority: u32,
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
    /// Gravity is fixed to neovim's defaults: `start` is **right-gravity** (text
    /// inserted exactly at the mark's start lands *before* it, so the start
    /// slides right), `end` is **left-gravity** (text inserted at the end lands
    /// *after* it, so the end stays). Net effect: typing at either boundary of a
    /// highlighted range leaves the highlighted text unchanged. An anchor swallowed
    /// by a deletion collapses to the deletion point rather than vanishing.
    pub fn shift(&mut self, start: usize, old_end: usize, new_end: usize) {
        if old_end == start && new_end == start {
            return; // no-op edit
        }
        for slot in self.by_ns.values_mut() {
            for m in slot.marks.values_mut() {
                m.start = shift_right_gravity(m.start, start, old_end, new_end);
                if let Some(e) = m.end {
                    let e = shift_left_gravity(e, start, old_end, new_end);
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
