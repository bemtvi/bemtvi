//! Unicode-aware column math over a single line of text.
//!
//! Cursor columns are stored as byte offsets (the rope's native metric and
//! vim's column model), but *movement* steps by grapheme cluster and *display*
//! accounts for wide characters and tabs. These pure helpers convert between
//! byte offset, grapheme boundary, and virtual (screen) column over a line
//! `&str`. ASCII is handled correctly either way (each ASCII char is its own
//! single-byte grapheme); [`floor_grapheme`] additionally takes an all-ASCII
//! fast path to skip grapheme segmentation on the cursor hot path.

use std::borrow::Cow;
use std::iter::Peekable;

use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};
use unicode_width::UnicodeWidthStr;

/// Default tab-stop width in cells — the fallback used where no buffer-local
/// `tabstop` is in scope (e.g. panel rendering, some LSP column math). Buffer
/// text uses [`crate::options::BufferOptions::effective_tabstop`] instead.
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

/// The number of display **rows** `lines` occupy when soft-wrapped to `width` columns:
/// each line takes `ceil(display_width / width)` rows (at least one, so a blank line
/// still costs a row), summed. This is the height a `wrap`ped float needs to show the
/// content without clipping — a single long line (a reflowed markdown paragraph) spans
/// several rows. With `wrap` off, or a zero width, it is just the line count (each line
/// truncates to one row).
pub fn wrapped_row_count(lines: &[String], width: usize, wrap: bool) -> usize {
    if !wrap || width == 0 {
        return lines.len();
    }
    lines
        .iter()
        .map(|l| display_width(l).max(1).div_ceil(width))
        .sum()
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

/// Display width of the grapheme the cursor sits on at byte offset `byte` — how
/// many screen cells a block cursor should cover. A tab, a wide (CJK / emoji)
/// grapheme, and the `^X` / `<xx>` control-char substitutions all report their
/// full cell span, so the cursor envelops the whole token rather than its first
/// cell. At or past end-of-line (no grapheme there) the width is `1` — the
/// cursor's own empty cell. Never returns `0`, so a zero-width combining grapheme
/// still gets a one-cell cursor.
pub fn cursor_cell_width(line: &str, byte: usize, tabstop: usize) -> usize {
    let start = floor_grapheme(line, byte);
    match line[start..].graphemes(true).next() {
        Some(g) => grapheme_width(g, virtcol(line, start, tabstop), tabstop).max(1),
        None => 1,
    }
}

/// One soft-wrap display segment of a line: the byte range it covers and the
/// screen column where it begins within the (continuous) line. The first segment
/// always starts at byte 0 / column 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapSeg {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_col: usize,
}

/// Split `line` into soft-wrap display segments for a `width`-cell text area: each
/// holds at most `width` screen cells, broken on grapheme boundaries with tab- and
/// wide-char-aware widths (vim wraps on display columns). With `width == 0` or a
/// line that fits in `width` cells, a single segment spans the whole line.
///
/// A grapheme wider than `width` (a wide char in a 1-cell window) still gets its
/// own segment rather than an empty one. (Tabs inside a *continuation* segment use
/// the original column grid for the break math; a client re-expanding the raw
/// segment text from screen column 0 may drift on tab-heavy wrapped lines — a
/// documented v1 limitation. Leading indentation, the common case, sits on the
/// first segment and is unaffected.)
pub fn wrap_segments(line: &str, tabstop: usize, width: usize) -> Vec<WrapSeg> {
    wrap_segments_indented(line, tabstop, width, 0)
}

/// A window's soft-wrap continuation-prefix config — `'breakindent'`,
/// `'showbreak'`, and the `'breakindentopt'` `sbr` flag — bundled so the wrap
/// helpers thread one `Copy` value. Borrows the showbreak string.
#[derive(Clone, Copy, Default)]
pub struct WrapPrefix<'a> {
    /// `'breakindent'`: indent continuation rows to the wrapped line's own indent.
    pub breakindent: bool,
    /// `'showbreak'`: the marker drawn at the start of each continuation row.
    pub showbreak: &'a str,
    /// `'breakindentopt'` contains `sbr`: draw `'showbreak'` *within* the breakindent
    /// (subtract its width from the indent) so the wrapped text still aligns under the
    /// line's indent. Default (`false`) is vim's additive prefix: breakindent then the
    /// marker, so the text sits one marker-width past the indent.
    pub sbr: bool,
}

/// The `'breakindent'`/`'showbreak'` continuation prefix for `line`: the string drawn
/// at the start of every soft-wrap continuation row, and its display width in cells.
/// Matches vim's draw order (`drawline.c`): by default the breakindent (the line's
/// own indent) is laid down first and `'showbreak'` follows right before the wrapped
/// text — so the text sits `showbreak`-width past the indent. With `'breakindentopt'`
/// `sbr` the marker is drawn first and the breakindent is reduced by its width, so the
/// text aligns exactly under the indent. The total width is clamped to `width - 1` so
/// a continuation row always keeps at least one text cell. Returns `("", 0)` when
/// neither option applies (the common case), so callers add no prefix.
pub fn break_prefix(line: &str, tabstop: usize, width: usize, wp: WrapPrefix) -> (String, usize) {
    if width == 0 || (!wp.breakindent && wp.showbreak.is_empty()) {
        return (String::new(), 0);
    }
    let sbr_w = display_width(wp.showbreak);
    let indent = if wp.breakindent {
        // Display width of the line's leading whitespace (the breakindent amount).
        let fnb = line.len() - line.trim_start_matches([' ', '\t']).len();
        virtcol(line, fnb, tabstop)
    } else {
        0
    };
    // vim's `bri`: the breakindent amount, reduced by the marker width under `sbr`.
    let bri = if wp.sbr {
        indent.saturating_sub(sbr_w)
    } else {
        indent
    };
    let total = (bri + sbr_w).min(width.saturating_sub(1));
    if total == 0 {
        return (String::new(), 0);
    }
    // The marker fitted to at most `total` cells (truncated if it alone overflows),
    // and the breakindent spaces filling the rest.
    let marker = truncate_to_width(wp.showbreak, tabstop, total);
    let marker_w = display_width(&marker);
    let spaces = " ".repeat(total - marker_w);
    // Draw order (see the doc / `drawline.c`): `sbr` → marker then indent; default →
    // indent then marker (the marker sits right before the wrapped text).
    let prefix = if wp.sbr {
        format!("{marker}{spaces}")
    } else {
        format!("{spaces}{marker}")
    };
    (prefix, total)
}

/// `s` truncated to at most `width` display cells, on a grapheme boundary.
fn truncate_to_width(s: &str, tabstop: usize, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = grapheme_width(g, w, tabstop);
        if w + gw > width {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out
}

/// The continuation-indent width [`wrap_segments_indented`] takes — the prefix cells
/// reserved on each continuation row (see [`break_prefix`]).
pub fn cont_indent(line: &str, tabstop: usize, width: usize, wp: WrapPrefix) -> usize {
    break_prefix(line, tabstop, width, wp).1
}

/// [`wrap_segments`] with a `'breakindent'` / `'showbreak'` **continuation indent**:
/// `cont_indent` leading cells of every continuation row are reserved for the
/// indent / marker prefix, so a continuation segment wraps into `width -
/// cont_indent` text cells rather than the full `width`. The first segment keeps the
/// full `width` (its row has no prefix). `cont_indent` is clamped so a continuation
/// row always keeps at least one text cell. `start_col` stays the segment's column
/// in the original line grid (the prefix is not part of it — the caller bakes the
/// prefix onto the row text and shifts overlays by `cont_indent` separately).
pub fn wrap_segments_indented(
    line: &str,
    tabstop: usize,
    width: usize,
    cont_indent: usize,
) -> Vec<WrapSeg> {
    if width == 0 {
        return vec![WrapSeg {
            start_byte: 0,
            end_byte: line.len(),
            start_col: 0,
        }];
    }
    // Keep ≥ 1 text cell per continuation row even with a wide prefix.
    let cont_indent = cont_indent.min(width.saturating_sub(1));
    let mut segs = Vec::new();
    let mut seg_start_byte = 0;
    let mut seg_start_col = 0;
    let mut col = 0;
    let mut byte = 0;
    for g in line.graphemes(true) {
        let w = grapheme_width(g, col, tabstop);
        // The current row's usable width: full for the first segment, reduced by the
        // continuation indent for every later one.
        let avail = if seg_start_byte == 0 {
            width
        } else {
            width - cont_indent
        };
        // Break *before* a grapheme that would overflow the current row, but never
        // emit an empty row (so an over-wide grapheme still occupies its own row).
        if col + w > seg_start_col + avail && byte > seg_start_byte {
            segs.push(WrapSeg {
                start_byte: seg_start_byte,
                end_byte: byte,
                start_col: seg_start_col,
            });
            seg_start_byte = byte;
            seg_start_col = col;
        }
        col += w;
        byte += g.len();
    }
    segs.push(WrapSeg {
        start_byte: seg_start_byte,
        end_byte: line.len(),
        start_col: seg_start_col,
    });
    segs
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
        // The 1-byte-1-cell shortcut holds only for printable ASCII: a tab expands,
        // and C0 controls / DEL are ASCII yet render as 2-cell `^X` tokens (see
        // [`grapheme_width`] / [`control_width`]). `is_ascii_control` covers both the
        // C0 range and DEL, so excluding it (plus the high bytes) keeps the fast path
        // correct on lines with embedded control chars.
        let simple = line.bytes().all(|b| b.is_ascii() && !b.is_ascii_control());
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
///
/// An unprintable control character is displayed vim-style — `^X` caret notation
/// (2 cells) or `<xx>` hex (4 cells), see [`control_width`] — rather than the
/// font's tofu box, so its width here is that of the substitution. `unicode-width`
/// reports control chars as zero-width, which is correct only if they're hidden;
/// nxvim shows them, so this is the authoritative width that cursor / span /
/// scroll column math all key off (it must match the text [`display_line`] emits).
fn grapheme_width(g: &str, col: usize, tabstop: usize) -> usize {
    if g == "\t" {
        return tabstop - (col % tabstop);
    }
    if let Some(w) = single_char(g).and_then(control_width) {
        return w;
    }
    UnicodeWidthStr::width(g)
}

/// The single `char` of `g` when it is exactly one, else `None`. Control
/// characters never combine, so an unprintable byte is always its own one-char
/// grapheme — this lets the width / display helpers classify it cheaply.
fn single_char(g: &str) -> Option<char> {
    let mut it = g.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Display width of an unprintable control char's vim-style substitution, or
/// `None` when `c` renders as itself (any printable char, or a tab — tabs expand
/// separately). C0 controls (`U+0000..=U+001F`) and DEL (`U+007F`) use caret
/// notation `^@`..`^_` / `^?` (2 cells); the C1 controls (`U+0080..=U+009F`,
/// where the latin1 fallback's undefined high bytes land) use `<xx>` hex (4
/// cells). Newlines never reach here — lines are stored EOL-stripped.
pub fn control_width(c: char) -> Option<usize> {
    match c {
        '\t' => None,
        '\u{00}'..='\u{1f}' | '\u{7f}' => Some(2),
        '\u{80}'..='\u{9f}' => Some(4),
        _ => None,
    }
}

/// The vim-style display text for an unprintable control char (the substitution
/// whose width [`control_width`] reports), or `None` when `c` renders as itself.
/// This is display-only: the buffer/rope keeps the original char, so the on-disk
/// bytes round-trip exactly regardless of how they're shown.
pub fn control_repr(c: char) -> Option<String> {
    match c {
        '\t' => None,
        '\u{00}'..='\u{1f}' => Some(format!("^{}", (b'@' + c as u8) as char)),
        '\u{7f}' => Some("^?".to_string()),
        '\u{80}'..='\u{9f}' => Some(format!("<{:02x}>", c as u32)),
        _ => None,
    }
}

/// Render `line` for display: every unprintable control char replaced by its
/// vim-style `^X` / `<xx>` text ([`control_repr`]), tabs and printables passing
/// through untouched. Returns the input borrowed when nothing needs substituting
/// (the overwhelmingly common case — no allocation). The substituted text's
/// per-char widths sum to what [`grapheme_width`] reports for the originals, so a
/// client that measures the returned string lands every cell where the server's
/// column math expects it.
pub fn display_line(line: &str) -> Cow<'_, str> {
    if !line.chars().any(|c| control_width(c).is_some()) {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len() + 8);
    for c in line.chars() {
        match control_repr(c) {
            Some(rep) => out.push_str(&rep),
            None => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Byte ranges `[start, end)` of each unprintable control char in `line` (the
/// chars [`display_line`] substitutes), in order. Used to overlay a `SpecialKey`
/// highlight on the `^X` / `<xx>` tokens. Empty for the common all-printable line.
pub fn unprintable_positions(line: &str) -> Vec<(usize, usize)> {
    line.char_indices()
        .filter(|&(_, c)| control_width(c).is_some())
        .map(|(i, c)| (i, i + c.len_utf8()))
        .collect()
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
