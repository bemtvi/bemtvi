//! The shared selection model — one range-set both editing grammars read.
//!
//! bemtvi's vim grammar is verb→noun on a *point* cursor ([`Editor::cursor`]), with
//! a selection alive only transiently in visual mode ([`Editor::visual_anchor`]).
//! Helix's grammar is noun→verb on an always-present *range* set. To let both
//! consume the same source of truth without churning the thousands of vim read
//! sites, the live cursor state is projected into (and written back from) one
//! unified view: [`Editor::selections`] / [`Editor::set_selections`]. The primary
//! stays cached in `cursor`/`visual_anchor`; the secondaries stay persisted as
//! [`CURSOR_NS`](crate::extmark::CURSOR_NS)/[`ANCHOR_NS`](crate::extmark::ANCHOR_NS)
//! extmarks (so they auto-shift through the buffer's edit choke point). This
//! module only defines the shared *vocabulary*; the projection lives with the
//! extmark machinery it drives, in [`crate::editor::multicursor`].
//!
//! **Range convention (Helix).** `head` is the moving end (where a motion lands,
//! and where the block/point cursor is drawn); `anchor` is the fixed end a motion
//! in *extend* mode leaves put. Both ends are [`Cursor`] `(line, col)`. A bare
//! cursor is a *point* range (`anchor == head`) — Helix's width-1 minimum
//! selection. Ordering the two ends (`from`/`to`) is left to the operator sites
//! that already do it for visual mode; a `Range` itself keeps the ends
//! directional so *which* end moved is never lost.

use super::Cursor;

/// One selection range: `anchor..head`. See the module docs for the convention —
/// `head` is the moving/drawn end, `anchor` the fixed one, both inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    pub(crate) anchor: Cursor,
    pub(crate) head: Cursor,
}

impl Range {
    /// Swap the two ends — the same span, but the end that was moving is now
    /// fixed and vice versa (visual `o`/`O`, Helix `Alt-;`).
    pub(crate) fn flipped(self) -> Self {
        Self {
            anchor: self.head,
            head: self.anchor,
        }
    }
}

/// The full selection set: every [`Range`] plus which one is *primary* (the range
/// whose head is the visible cursor and whose register the unnamed yank/paste
/// tracks). Always non-empty — the primary is present even when it's a lone point.
#[derive(Debug, Clone)]
pub(crate) struct Selections {
    pub(crate) ranges: Vec<Range>,
    pub(crate) primary: usize,
}

impl Selections {
    /// The primary range. Never panics: [`Editor::selections`] always includes it.
    pub(crate) fn primary(&self) -> Range {
        self.ranges[self.primary]
    }
}
