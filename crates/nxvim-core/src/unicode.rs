//! Unicode-aware column math over a single line of text.
//!
//! Cursor columns are stored as byte offsets (the rope's native metric and
//! vim's column model), but *movement* steps by grapheme cluster and *display*
//! accounts for wide characters and tabs. These pure helpers convert between
//! byte offset, grapheme boundary, and virtual (screen) column over a line
//! `&str`. ASCII is handled correctly either way (each ASCII char is its own
//! single-byte grapheme); [`floor_grapheme`] additionally takes an all-ASCII
//! fast path to skip grapheme segmentation on the cursor hot path.

use unicode_segmentation::UnicodeSegmentation;
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
