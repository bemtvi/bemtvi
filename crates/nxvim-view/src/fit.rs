//! Pure text-fitting helpers shared by the clients' popup / gutter painting:
//! char-based truncation, padding, and scroll math that must agree cell-for-cell
//! between the renderer and its hit-tests (and between clients, so the popups
//! read the same everywhere). All char-count based — exact for the ASCII
//! identifiers and paths these rows usually hold. (The TUI's *text body* helpers
//! — `pad_to_width` / `expand_tabs` — stay client-side: the TUI measures display
//! width for CJK/emoji fidelity while the GUI's column math is deliberately
//! char-based, so those two must not be unified.)

/// First visible item index for a popup whose inner content area is `rows` tall
/// with `selected` highlighted: scroll the list to keep the selection in view,
/// else start at the top. The single source of truth shared by the pmenu/picker
/// renderers and their click geometry, so a click maps to the same row the
/// renderer drew.
pub fn pmenu_start(selected: Option<usize>, rows: usize) -> usize {
    match selected {
        Some(s) if rows > 0 && s >= rows => s + 1 - rows,
        _ => 0,
    }
}

/// One popup row padded to `width` cells: the `label` left-aligned, and the
/// `detail` (a type/source hint) right-aligned when it fits after a one-cell gap.
/// A too-long label is truncated.
pub fn pmenu_row(label: &str, detail: &str, width: usize) -> String {
    let label: String = label.chars().take(width).collect();
    let label_w = label.chars().count();
    let detail_w = detail.chars().count();
    if !detail.is_empty() && label_w + 1 + detail_w <= width {
        let pad = width - label_w - detail_w;
        format!("{label}{}{detail}", " ".repeat(pad))
    } else {
        format!("{label:<width$}")
    }
}

/// Shorten `s` to at most `width` chars, keeping its head and tail and dropping the
/// middle behind a single `…` when it won't fit — so a too-long preview path shows
/// both its root and its filename instead of just the start.
pub fn elide_middle(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return chars.iter().take(width).collect();
    }
    let keep = width - 1; // one column for the `…`
    let front = keep / 2; // favour the tail (filename) with the larger half
    let back = keep - front;
    let head: String = chars[..front].iter().collect();
    let tail: String = chars[chars.len() - back..].iter().collect();
    format!("{head}…{tail}")
}

/// Truncate a picker row `label` to `width` columns while keeping the file name (the
/// path tail) visible. When a path overflows the row, drop whole leading directory
/// components behind a single `…` so the file name survives — instead of the plain
/// head-cut the caller would otherwise apply, which truncates the *tail* and hides
/// the very thing you scan for. `spans` (matched-char char ranges into `label`) are
/// remapped onto the returned string so highlights still land on the right chars.
///
/// A row that already fits — or a non-path row (no `/`) — is returned unchanged, so
/// only file paths get the tail-priority treatment; the caller's head-cut still
/// applies to plain labels. When even the file name alone can't fit, the tail is
/// kept (the name is truncated only because it's impossible to show whole).
pub fn elide_keep_tail(
    label: &str,
    spans: &[(u16, u16)],
    width: usize,
) -> (String, Vec<(u16, u16)>) {
    let chars: Vec<char> = label.chars().collect();
    let n = chars.len();
    if n <= width || width == 0 || !label.contains('/') {
        return (label.to_string(), spans.to_vec());
    }
    // Reserve one column for the leading `…`; keep at most the last `width - 1` chars.
    let drop = n - (width - 1);
    // Prefer a clean cut just after a path separator — the smallest `/`-boundary at
    // or past `drop` keeps the most directory context that still fits. None ⇒ raw cut.
    let cut = (drop..n).find(|&i| chars[i - 1] == '/').unwrap_or(drop);
    let mut out = String::with_capacity(width);
    out.push('…');
    out.extend(&chars[cut..]);
    // Remap spans: original index `i` (≥ cut) renders at display index `i - cut + 1`
    // (the `…` occupies index 0). A span wholly inside the dropped prefix vanishes.
    let shift = cut as i64 - 1;
    let remapped = spans
        .iter()
        .filter_map(|&(s, e)| {
            let ns = (s as i64).max(cut as i64) - shift;
            let ne = (e as i64).min(n as i64) - shift;
            (ns < ne).then_some((ns as u16, ne as u16))
        })
        .collect();
    (out, remapped)
}

/// Build one `width`-cell gutter cell for a row whose buffer line is `num`
/// (`None` for a `~` filler): absolute numbers (`number`), distance-from-cursor
/// (`relativenumber`), or the hybrid — absolute on the cursor line, relative
/// elsewhere. Numbers are right-aligned with a trailing space, except the hybrid
/// cursor line whose absolute number is left-aligned — vim's layout.
pub fn gutter_cell(
    num: Option<usize>,
    current_line: usize,
    number: bool,
    relativenumber: bool,
    width: usize,
) -> String {
    let Some(n) = num else {
        return " ".repeat(width);
    };
    let is_current = n == current_line;
    if number && relativenumber && is_current {
        // Hybrid cursor line: absolute number, left-aligned.
        format!("{n:<width$}")
    } else {
        let value = if !relativenumber {
            n // number-only: absolute on every line
        } else if is_current {
            0 // relativenumber-only cursor line shows 0
        } else {
            n.abs_diff(current_line)
        };
        let field = width.saturating_sub(1); // reserve the trailing space
        format!("{value:>field$} ")
    }
}
