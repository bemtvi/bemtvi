//! Unicode-aware column math over a single line of text.
//!
//! Cursor columns are stored as byte offsets (the rope's native metric and
//! vim's column model), but *movement* steps by grapheme cluster and *display*
//! accounts for wide characters and tabs. These pure helpers convert between
//! byte offset, grapheme boundary, and virtual (screen) column over a line
//! `&str`. ASCII is handled correctly either way (each ASCII char is its own
//! single-byte grapheme); [`floor_grapheme`] additionally takes an all-ASCII
//! fast path to skip grapheme segmentation on the cursor hot path.

use std::iter::Peekable;

use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

/// Width of a tab stop in cells. A constant until the options system (`:set
/// tabstop`) exists.
pub const TABSTOP: usize = 8;

/// Byte offset of the grapheme boundary immediately after `byte` (clamped to
/// the end of `line`). Returns `line.len()` when there is no following grapheme.
pub fn next_grapheme(line: &str, byte: usize) -> usize {
    let byte = floor_grapheme(line, byte);
    line[byte..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(line.len(), |(i, _)| byte + i)
}

/// Byte offset of the grapheme boundary immediately before `byte` (clamped to 0).
pub fn prev_grapheme(line: &str, byte: usize) -> usize {
    let byte = floor_grapheme(line, byte);
    line[..byte]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

/// Snap `byte` down to the nearest grapheme-cluster boundary (a no-op for ASCII
/// or when already on a boundary). Returns `line.len()` for `byte >= line.len()`.
pub fn floor_grapheme(line: &str, byte: usize) -> usize {
    if byte >= line.len() {
        return line.len();
    }
    // Fast path for an all-ASCII line (the common case, and this is on the cursor
    // hot path): every byte is its own single-byte grapheme, so `byte` is already
    // a boundary. The `is_ascii` scan is far cheaper than grapheme segmentation.
    if line.is_ascii() {
        return byte;
    }
    let mut last = 0;
    for (i, _) in line.grapheme_indices(true) {
        if i > byte {
            break;
        }
        last = i;
    }
    last
}

/// Display width of `s` in screen cells — wide characters count as two (via
/// `unicode-width`), with no tab handling (callers use it for short tab-free
/// strings like tabline cells, where there are no tabs to expand).
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Virtual (screen-cell) column of byte offset `byte`: the cells occupied by
/// `line[..byte]`, with tabs expanding to the next multiple of `tabstop` and
/// wide characters counting as two (via `unicode-width`). If `byte` is not on a
/// grapheme boundary it is first snapped down to the nearest one.
pub fn virtcol(line: &str, byte: usize, tabstop: usize) -> usize {
    let byte = floor_grapheme(line, byte);
    let mut col = 0;
    for g in line[..byte].graphemes(true) {
        col += grapheme_width(g, col, tabstop);
    }
    col
}

/// A byte-offset → virtual-column mapper for a single line that answers a
/// **non-decreasing** sequence of queries by walking the line's graphemes at
/// most once across all of them — amortized O(1) per query, versus
/// [`virtcol`]'s O(byte) re-walk from column 0 on every call.
///
/// This is the projection hot path: per redraw, each visible row maps the
/// `(start, end)` byte offsets of every syntax span / search match / selection
/// to screen columns. Those offsets arrive sorted (spans are non-overlapping
/// and left-to-right), so a single forward walk serves them all instead of the
/// O(line_len × spans) re-scan a bare `virtcol` per offset would cost.
///
/// Correctness does not depend on the ordering: a query that moves *backwards*
/// transparently restarts the walk from the start of the line, so any order
/// yields the same result `virtcol` would — only the amortization is lost.
/// Pure-ASCII tab-free lines (the common case) skip the walk entirely, since
/// there `virtcol == byte`.
pub struct LineVirtcol<'a> {
    line: &'a str,
    tabstop: usize,
    /// Pure ASCII with no tab ⇒ every byte is one cell, so `virtcol == byte`.
    simple: bool,
    /// Grapheme walk state for the general path: the iterator's front sits at
    /// byte `walked_byte`, which is at virtual column `walked_col`.
    graphemes: Peekable<GraphemeIndices<'a>>,
    walked_byte: usize,
    walked_col: usize,
}

impl<'a> LineVirtcol<'a> {
    /// Build a mapper for `line`. The O(line_len) `simple` scan is paid once per
    /// line, replacing the per-offset re-walk.
    pub fn new(line: &'a str, tabstop: usize) -> Self {
        let simple = line.bytes().all(|b| b.is_ascii() && b != b'\t');
        LineVirtcol {
            line,
            tabstop,
            simple,
            graphemes: line.grapheme_indices(true).peekable(),
            walked_byte: 0,
            walked_col: 0,
        }
    }

    /// Virtual column of byte offset `byte` (snapped down to a grapheme boundary,
    /// exactly as [`virtcol`] does). Cheapest when called with non-decreasing
    /// `byte` values.
    pub fn at(&mut self, byte: usize) -> usize {
        if self.simple {
            return byte.min(self.line.len());
        }
        // A backward query can't be served by the forward walk — restart it.
        if byte < self.walked_byte {
            self.graphemes = self.line.grapheme_indices(true).peekable();
            self.walked_byte = 0;
            self.walked_col = 0;
        }
        // Consume every grapheme that ends at or before `byte` (one extending
        // past `byte` would be the grapheme `byte` floors into — excluded, matching
        // `virtcol`/`floor_grapheme`).
        while let Some(&(start, g)) = self.graphemes.peek() {
            if start + g.len() > byte {
                break;
            }
            self.walked_col += grapheme_width(g, self.walked_col, self.tabstop);
            self.walked_byte = start + g.len();
            self.graphemes.next();
        }
        self.walked_col
    }
}

/// Byte offset of the grapheme whose cell span covers virtual column `target`
/// (or `line.len()` when `target` is at or past the end). Used to land vertical
/// motion on the column nearest the remembered one.
pub fn byte_at_virtcol(line: &str, target: usize, tabstop: usize) -> usize {
    let mut col = 0;
    for (i, g) in line.grapheme_indices(true) {
        let w = grapheme_width(g, col, tabstop);
        if col + w > target || (w == 0 && col == target) {
            return i;
        }
        col += w;
    }
    line.len()
}

/// Cells occupied by a single grapheme starting at virtual column `col`.
fn grapheme_width(g: &str, col: usize, tabstop: usize) -> usize {
    if g == "\t" {
        tabstop - (col % tabstop)
    } else {
        UnicodeWidthStr::width(g)
    }
}

/// Number of UTF-16 code units in `line[..byte]` — i.e. the LSP
/// `Position.character` for the byte offset `byte`, under the protocol's default
/// UTF-16 position encoding. nxvim columns are byte offsets; this is the
/// conversion that keeps a position correct the moment a line holds a non-ASCII
/// character. ASCII is 1:1; a non-BMP scalar value (a surrogate pair) counts as
/// two units. `byte` is clamped to `line.len()`, and a `byte` landing inside a
/// multi-byte char counts the whole chars strictly before it (never panics).
///
/// UTF-8 is the identity on byte offsets, so the server applies this only when
/// the negotiated encoding is UTF-16 (Decision 4).
pub fn byte_to_utf16(line: &str, byte: usize) -> usize {
    let mut units = 0;
    for (i, ch) in line.char_indices() {
        if i >= byte {
            return units;
        }
        units += ch.len_utf16();
    }
    units
}

/// Inverse of [`byte_to_utf16`]: the byte offset `u16_units` UTF-16 code units
/// into `line` (clamped to `line.len()`). A `u16_units` that would land in the
/// middle of a surrogate pair snaps to the start of that character's next
/// boundary's char — i.e. it stops at the first char whose cumulative unit count
/// reaches the target, so the result is always a char boundary.
pub fn utf16_to_byte(line: &str, u16_units: usize) -> usize {
    let mut units = 0;
    for (i, ch) in line.char_indices() {
        if units >= u16_units {
            return i;
        }
        units += ch.len_utf16();
    }
    line.len()
}
