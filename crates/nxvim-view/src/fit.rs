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
/// A row that already fits is returned unchanged. A non-path row (no `/`) keeps its
/// **head** — the identifier you scan for — and marks the cut with a trailing `…`, so
/// an over-long completion candidate reads as truncated instead of as a shorter word
/// run flush against the kind column. When even the file name alone can't fit, the
/// tail is kept (the name is truncated only because it's impossible to show whole).
pub fn elide_keep_tail(
    label: &str,
    spans: &[(u16, u16)],
    width: usize,
) -> (String, Vec<(u16, u16)>) {
    let chars: Vec<char> = label.chars().collect();
    let n = chars.len();
    if n <= width || width == 0 {
        return (label.to_string(), spans.to_vec());
    }
    if !label.contains('/') {
        // Head-priority: keep the first `width - 1` chars and spend the last column on
        // the `…`. Spans are clipped to what survives; one wholly past the cut vanishes.
        let keep = width - 1;
        let mut out: String = chars[..keep].iter().collect();
        out.push('…');
        let remapped = spans
            .iter()
            .filter_map(|&(s, e)| {
                let ne = e.min(keep as u16);
                (s < ne).then_some((s, ne))
            })
            .collect();
        return (out, remapped);
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

/// The shared **head column** width for a frame of two-column picker rows (a
/// `path:line:col: ` head followed by a content body — live_grep's shape): the
/// widest head in the visible window, capped at 40% of the list width so a long
/// path can never squeeze the body out, and never wider than it needs to be when
/// every head is short. Computed once per frame and passed to every [`fit_row`]
/// call so the bodies line up in one column instead of each row starting wherever
/// its own path happened to end.
pub fn row_head_col(max_head: usize, width: usize) -> usize {
    let cap = (width * 2 / 5).max(1); // the head keeps at least 40% of the row
    max_head.min(cap).max(1)
}

/// Fit one picker row `label` into `width` columns, honoring an optional
/// two-column `layout` — `(head, match_start, match_end)`, all **char** offsets
/// into `label`: `head` is the length of the leading location column
/// (`src/foo.rs:12:5: `) and `[match_start, match_end)` is the source's own match
/// within the body (the text `rg` hit), empty when the source knows only where the
/// interesting content starts.
///
/// Without a layout this is [`elide_keep_tail`] — a plain path row keeps its file
/// name, everything else head-cuts. With one, the row becomes two columns:
///
/// - the head is padded (or `…`-elided keeping its tail, so the file name and the
///   line number survive) to `head_col`, so bodies align down the list;
/// - the body fills the rest, **windowed around the match** with a little leading
///   context behind a `…` — a match 200 columns into a long line is visible
///   instead of scrolled off the right edge.
///
/// `spans` (matched-char char ranges into `label`) are remapped onto the returned
/// string, splitting across the two columns when a span straddles them. A non-empty
/// match range joins them, so a source that does its own matching (live_grep, whose
/// dynamic rows bypass the fuzzy matcher) highlights its hit exactly as `files`
/// highlights a fuzzy one.
pub fn fit_row(
    label: &str,
    spans: &[(u16, u16)],
    width: usize,
    layout: Option<(usize, usize, usize)>,
    head_col: usize,
) -> (String, Vec<(u16, u16)>) {
    let Some((head, match_start, match_end)) = layout else {
        return elide_keep_tail(label, spans, width);
    };
    if width == 0 {
        return (String::new(), Vec::new());
    }
    let chars: Vec<char> = label.chars().collect();
    let n = chars.len();
    let head = head.min(n);
    // Each chunk maps a run of source chars onto the output: `(out_start, src_start,
    // len)`. The `…` markers and the head padding are literal — they map to nothing,
    // so a span over dropped chars simply vanishes.
    let mut out = String::new();
    let mut used = 0usize;
    let mut chunks: Vec<(usize, usize, usize)> = Vec::new();
    let mut take = |out: &mut String, used: &mut usize, src: usize, len: usize| {
        chunks.push((*used, src, len));
        out.extend(&chars[src..src + len]);
        *used += len;
    };

    // ---- head column: the whole head when it fits, else its tail behind a `…`.
    let hc = head_col.clamp(1, width);
    if head <= hc {
        take(&mut out, &mut used, 0, head);
    } else {
        // Reserve one column for the `…`, then prefer a clean cut just past a path
        // separator — the same tail-priority rule `elide_keep_tail` applies.
        let drop = head - (hc - 1);
        let cut = (drop..head).find(|&i| chars[i - 1] == '/').unwrap_or(drop);
        out.push('…');
        used += 1;
        take(&mut out, &mut used, cut, head - cut);
    }
    // Pad the head out to its column so every row's body starts at the same cell. A
    // head-only row (no body) is left ragged — there is nothing to align it with, and
    // a row with no head at all (a source declaring only a match range) is never
    // indented into the column.
    if head > 0 && head < n && used < hc {
        out.push_str(&" ".repeat(hc - used));
        used = hc;
    }

    // ---- body column: the rest of the row, windowed so the match stays visible.
    let bw = width.saturating_sub(used);
    if bw > 0 && head < n {
        let blen = n - head;
        let f = match_start.clamp(head, n) - head;
        if blen <= bw {
            take(&mut out, &mut used, head, blen);
        } else {
            // Keep a fifth of the column as leading context before the match, then
            // slide back so the window still fills the column at the end of the line.
            let lead = (bw / 5).min(f);
            let start = (f - lead).min(blen - (bw - 1));
            if start == 0 {
                take(&mut out, &mut used, head, bw);
            } else {
                out.push('…');
                used += 1;
                take(&mut out, &mut used, head + start, bw - 1);
            }
        }
    }

    // Remap each span through the chunks; one straddling the head/body split lands as
    // two output ranges, and one wholly inside dropped text lands as none. The source's
    // own match joins the fuzzy spans so it highlights the same way.
    let declared = (match_end > match_start).then_some((match_start as u16, match_end as u16));
    let mut remapped = Vec::new();
    for &(s, e) in spans.iter().chain(declared.iter()) {
        for &(out_start, src_start, len) in &chunks {
            let ns = (s as usize).max(src_start);
            let ne = (e as usize).min(src_start + len);
            if ns < ne {
                remapped.push((
                    (ns - src_start + out_start) as u16,
                    (ne - src_start + out_start) as u16,
                ));
            }
        }
    }
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
