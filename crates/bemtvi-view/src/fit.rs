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
    if width == 0 {
        // A 0-cell column shows nothing — an over-wide row (the whole label)
        // would overflow the picker column, and the highlight spans would point
        // at chars the renderer cuts. Mirrors `fit_row`'s own `width == 0`
        // guard on the two-column path.
        return (String::new(), Vec::new());
    }
    if n <= width {
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

/// How far a row's own highlight (`MenuData::row_hls` — the group its source painted
/// it with) reaches across the **fitted** label, in output columns.
///
/// A two-column row is painted over its HEAD only: the location/tag column is the part
/// that classifies the row (the severity + file of a diagnostic), and leaving the body
/// in the list's own color keeps the fuzzy-match highlight readable over it. A
/// single-column row has no such column, so its whole label paints. `head_col` and
/// `label_w` are the same values the caller passes [`fit_row`], so the extent lines up
/// with the columns it produced.
pub fn row_hl_extent(
    layout: Option<(usize, usize, usize, usize)>,
    head_col: usize,
    label_w: usize,
) -> usize {
    match layout {
        // `.max(1).min(..)` rather than `clamp`, which panics when a zero-width label
        // makes the lower bound exceed the upper.
        Some(_) => head_col.max(1).min(label_w),
        None => label_w,
    }
}

/// Fit one picker row `label` into `width` columns, honoring an optional
/// two-column `layout` — `(head, match_start, match_end, tag)`, all **char** offsets
/// into `label`: `head` is the length of the leading location column
/// (`src/foo.rs:12:5: `), `[match_start, match_end)` is the source's own match
/// within the body (the text `rg` hit), empty when the source knows only where the
/// interesting content starts, and `tag` is the pinned classification at the head's
/// start (`"E "`), which survives when the head has to elide.
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
    layout: Option<(usize, usize, usize, usize)>,
    head_col: usize,
) -> (String, Vec<(u16, u16)>) {
    let Some((head, match_start, match_end, tag)) = layout else {
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
        // The pinned tag leads, always: it classifies the row (a diagnostic's
        // severity), so a head too narrow to fit elides *around* it rather than
        // dropping it — one column is still reserved for the `…`.
        let tag = tag.min(head).min(hc.saturating_sub(1));
        if tag > 0 {
            take(&mut out, &mut used, 0, tag);
        }
        // Reserve one column for the `…`, then prefer a clean cut just past a path
        // separator — the same tail-priority rule `elide_keep_tail` applies.
        let drop = head - (hc - used - 1);
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
    // The declared range stays `usize` — it is a char offset into an unbounded label
    // (a hit 70k chars into a minified line), not a screen coordinate like `spans`.
    let declared = (match_end > match_start).then_some((match_start, match_end));
    let mut remapped = Vec::new();
    for (s, e) in spans
        .iter()
        .map(|&(s, e)| (s as usize, e as usize))
        .chain(declared)
    {
        for &(out_start, src_start, len) in &chunks {
            let ns = s.max(src_start);
            let ne = e.min(src_start + len);
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

/// A rectangle in screen **cells** — the toolkit-neutral spelling of the TUI's
/// ratatui `Rect` and the GUI's loose `(x, y, w, h)` quadruple, so popup geometry
/// the two clients must agree on can be expressed once here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl CellRect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    /// One past the last column the rect covers.
    pub fn right(self) -> u16 {
        self.x.saturating_add(self.w)
    }

    /// One past the last row the rect covers.
    pub fn bottom(self) -> u16 {
        self.y.saturating_add(self.h)
    }
}

/// The completion popup's documentation-preview box (border included), laid out
/// beside `popup` within `area`: to the popup's right when there's room, else to
/// its left (vim's `completeopt=popup` shape), top-aligned with it. `None` when
/// there are no docs, or no room on either side — a box that would overlap the
/// popup or hang off the area is not drawn at all.
///
/// The box is capped at 50×12 content cells so a long doc block can't swallow the
/// screen, then clamped to the room actually available: its width to the cells
/// beside the popup, its height to the rows below the popup's top. Height counts
/// **wrapped** lines (each doc line occupies `ceil(chars / content width)` rows),
/// so a long single-line doc gets the rows its wrapped form needs.
///
/// Lives here rather than in either client because both paint this same box: hand
/// -copied into the GUI it had already lost the room clamps (the box overran the
/// window edge, and drew on top of the popup when neither side had room).
pub fn doc_box(area: CellRect, popup: CellRect, doc: &[String]) -> Option<CellRect> {
    if doc.is_empty() {
        return None;
    }
    // Cap the preview so a long doc block doesn't swallow the screen.
    const MAX_W: u16 = 50;
    const MAX_H: u16 = 12;
    // Capped in `usize` before the cast: a pathologically long line would otherwise
    // wrap around `u16` and read as a *narrow* box.
    let natural_w = doc
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(1, MAX_W as usize) as u16;
    let want_box_w = natural_w.saturating_add(2);

    // Prefer the right of the popup; fall back to its left. Each side needs the
    // border plus at least one content cell (3 cells) to be worth drawing.
    let room_right = area.right().saturating_sub(popup.right());
    let room_left = popup.x.saturating_sub(area.x);
    let (x, box_w) = if room_right >= 3 {
        (popup.right(), want_box_w.min(room_right))
    } else if room_left >= 3 {
        let w = want_box_w.min(room_left);
        (popup.x.saturating_sub(w), w)
    } else {
        return None; // no room either side
    };
    // The documented guarantee: a box that would hang off the area is not drawn
    // at all. The side math above only sizes against the room on that side, so a
    // popup that itself hangs off the area (its rect comes off the wire) could
    // place the box outside it — e.g. a popup past the area's right edge puts
    // the left-side box between the popup and the area's right edge, and a popup
    // past the top edge top-aligns the box above the area. Check the placement
    // against the area itself (the height is clamped to the rows below `popup.y`
    // later, so the top edge is the only vertical hole).
    if x < area.x || x.saturating_add(box_w) > area.right() || popup.y < area.y {
        return None;
    }

    // Height from the wrapped line count, clamped to the cap and the room below.
    let content_w = box_w.saturating_sub(2).max(1);
    // Saturating rather than `sum()`: a huge doc block would overflow `u16` and
    // panic the client in a debug build, and the total is clamped to `MAX_H` anyway.
    let wrapped = doc.iter().fold(0u16, |acc, l| {
        let chars = (l.chars().count().min(u16::MAX as usize) as u16).max(1);
        acc.saturating_add(chars.div_ceil(content_w))
    });
    let content_h = wrapped.clamp(1, MAX_H);
    let box_h = content_h
        .saturating_add(2)
        .min(area.bottom().saturating_sub(popup.y));
    if box_w < 3 || box_h < 3 {
        return None;
    }
    Some(CellRect {
        x,
        y: popup.y,
        w: box_w,
        h: box_h,
    })
}

/// `line` hard-wrapped to `width` chars per row — the rows [`doc_box`] sized the
/// preview for, materialized for a client that has no wrapping text widget of its
/// own. An empty line yields one empty row (it still occupies a row), matching the
/// `max(1)` in the height math.
pub fn wrap_chars(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut row = String::new();
    let mut n = 0;
    for c in line.chars() {
        row.push(c);
        n += 1;
        if n == width {
            out.push(std::mem::take(&mut row));
            n = 0;
        }
    }
    if out.is_empty() || !row.is_empty() {
        out.push(row);
    }
    out
}
