//! Decoding of the server's `redraw` notification map into the client model:
//! small msgpack-map accessors and the per-region span/style parsers. Everything
//! here is frontend-agnostic — styles decode to the neutral [`Style`].

use rmpv::Value;

use crate::style::Style;

/// One treesitter highlight span in screen columns: `(start, end, group,
/// style_id)`. `style_id` indexes the frame's [`style palette`](crate::View::styles)
/// when the server resolved the capture through a loaded colorscheme; `None`
/// leaves the client to fall back to its own per-group theme.
pub type HlSpan = (u16, u16, String, Option<usize>);

/// One diagnostic underline span in screen columns: `(start, end, severity,
/// style_id)`. `severity` is `1`=error … `4`=hint; `style_id` indexes the
/// frame's style palette when the server resolved the `DiagnosticUnderline*`
/// group through a loaded colorscheme, `None` to fall back to a built-in
/// severity-colored undercurl.
pub type DiagSpan = (u16, u16, u8, Option<usize>);

/// One row's inline diagnostic virtual text: `(text, severity, style_id)`. The
/// `text` (already prefixed by the server) is painted after the line's
/// end-of-text, colored by the resolved `DiagnosticVirtualText*` `style_id`
/// (frame-palette index) or a built-in severity foreground when `None`.
pub type DiagVirt = (String, u8, Option<usize>);

/// One row's gutter diagnostic sign: `(glyph, severity, style_id)`. The `glyph`
/// (the server's per-severity sign text) is painted in the reserved sign column,
/// colored by the resolved `DiagnosticSign*` `style_id` (frame-palette index) or a
/// built-in severity foreground when `None`.
pub type DiagSign = (String, u8, Option<usize>);

/// One inline LSP inlay hint: `(col, text, style_id)`. `col` is the screen column
/// (the server resolved the byte anchor through the same tab/wide-char `virtcol`
/// the highlights use) the hint's `text` is inserted at — shifting the real glyphs
/// (and the cursor) right — colored by the resolved `LspInlayHint` `style_id`
/// (frame-palette index) or a built-in dim foreground when `None`.
pub type InlayHint = (u16, String, Option<usize>);

/// One chunk of extmark virtual text: `(text, style_id)`. `style_id` indexes the
/// frame palette when the server resolved the chunk's `hl_group` through a loaded
/// colorscheme; `None` paints in the window's normal foreground.
pub type VirtChunk = (String, Option<usize>);

/// One extmark virtual-text placement on a row. `pos` is where it sits — `0`=eol,
/// `1`=inline, `2`=overlay, `3`=right_align, `4`=win_col; `col` is the screen
/// column it anchors at (used by inline / overlay / win_col, `0` for eol /
/// right_align); `hl_mode` is `0`=replace, `1`=combine, `2`=blend. The wire shape
/// is fixed across all positions so adding a position needs no re-parse, only new
/// render handling.
#[derive(Clone, Debug, PartialEq)]
pub struct VirtPlacement {
    pub pos: u8,
    pub col: u16,
    pub hl_mode: u8,
    pub chunks: Vec<VirtChunk>,
}

/// Per visible row, the screen-column spans of every search match (`hlsearch`).
pub type SearchSpans = Vec<Vec<(u16, u16)>>;
/// Per visible row, the single span the live `incsearch` preview rests on.
pub type IncSearchSpans = Vec<Option<(u16, u16)>>;

/// One rendered status-line segment: its literal text and the resolved style.
/// `style` is `None` for a segment with no highlight group (or one the
/// colorscheme left undefined) — the client then paints it in the base
/// `StatusLine` look.
pub type StatusSegment = (String, Option<Style>);

/// One completion-popup row: `(label, kind, detail)`. `kind` is the
/// `CompletionItemKind` as a small int (`0` = unspecified) the client could map
/// to an icon; `detail` is `""` when the server provided none.
pub type PmenuItem = (String, u8, String);

/// Parse the `pmenu` redraw key's `items` array into `(label, kind, detail)`
/// rows. Malformed entries are dropped, yielding an empty list.
pub(crate) fn parse_pmenu_items(value: Option<&Value>) -> Vec<PmenuItem> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let t = item.as_array()?;
                    if t.len() != 3 {
                        return None;
                    }
                    Some((
                        t[0].as_str()?.to_string(),
                        t[1].as_u64()? as u8,
                        t[2].as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a window's `status` array into rendered segments: each entry a
/// `{ text, style }` map, where `style` is an index into the frame's `styles`
/// palette (`Nil` for the base `StatusLine` look). An absent/empty array yields
/// no segments, and the client falls back to the bare reverse-video status row
/// (an older server that doesn't project styled segments).
pub(crate) fn parse_status(value: Option<&Value>, styles: &[Style]) -> Vec<StatusSegment> {
    value
        .and_then(Value::as_array)
        .map(|segs| {
            segs.iter()
                .filter_map(|seg| {
                    let Value::Map(m) = seg else {
                        return None;
                    };
                    let style = map_get(m, "style")
                        .and_then(Value::as_u64)
                        .and_then(|id| styles.get(id as usize).copied());
                    Some((map_str(m, "text"), style))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

pub(crate) fn map_u64(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
}

/// Read a map field as a `u16`, truncating the wire `u64` (screen coordinates
/// and widths never approach `u16::MAX`, so the wrap is unreachable in practice).
pub(crate) fn map_u16(map: &[(Value, Value)], key: &str) -> u16 {
    map_u64(map, key) as u16
}

pub(crate) fn map_str(map: &[(Value, Value)], key: &str) -> String {
    map_get(map, key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Read an array-of-strings field (`lines`) into a `Vec<String>`.
pub(crate) fn map_str_array(map: &[(Value, Value)], key: &str) -> Vec<String> {
    map_get(map, key)
        .and_then(Value::as_array)
        .map(|a| {
            // Pre-size to the row count: `filter_map`'s 0 lower-bound size hint
            // otherwise reallocates as the (rarely-filtered) `lines` array grows.
            let mut out = Vec::with_capacity(a.len());
            out.extend(a.iter().filter_map(|v| v.as_str().map(String::from)));
            out
        })
        .unwrap_or_default()
}

/// Decode a single `[a, b]` wire value into a `(u16, u16)` pair, truncating each
/// `u64` (with a `0` fallback for a missing/non-int element). `None` when the
/// value isn't a 2-element array. The shared primitive behind every `[start,
/// end]` / `[row, col]` decode (`parse_spans`, `parse_multi_spans`, `parse_pair`,
/// `parse_cursor_list`).
pub(crate) fn pair_u16(value: &Value) -> Option<(u16, u16)> {
    match value.as_array() {
        Some(pair) if pair.len() == 2 => Some((
            pair[0].as_u64().unwrap_or(0) as u16,
            pair[1].as_u64().unwrap_or(0) as u16,
        )),
        _ => None,
    }
}

/// Parse a per-row array of `[start, end]` selection-span pairs (`Nil` rows
/// become `None`).
pub(crate) fn parse_spans(value: Option<&Value>) -> Vec<Option<(u16, u16)>> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().map(pair_u16).collect())
        .unwrap_or_default()
}

/// Parse a `[a, b]` pair (e.g. the picker preview's `loc` row/col) into
/// `Some((a, b))`, or `None` when the value is `Nil` / absent / malformed.
pub(crate) fn parse_pair(value: Option<&Value>) -> Option<(u16, u16)> {
    pair_u16(value?)
}

/// Parse an `[a, b, c]` triple (the picker row's `layouts` entry — head length
/// plus the match's char range) into `Some((a, b, c))`, or `None` when the value is
/// `Nil` / absent / malformed.
pub(crate) fn parse_triple(value: &Value) -> Option<(u16, u16, u16, u16)> {
    let a = value.as_array()?;
    // 3 elements is the pre-`tag` shape (an older server): no pinned tag.
    if a.len() != 3 && a.len() != 4 {
        return None;
    }
    let cell = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0) as u16;
    Some((cell(0), cell(1), cell(2), cell(3)))
}

/// Parse the redraw `padding` field — a `[top, right, bottom, left]` array of
/// cell counts (CSS order) — into a [`Padding`](crate::view::Padding). `Nil` /
/// absent / malformed (not a 4-element array) ⇒ no margin, so an older server or a
/// default window renders flush.
pub(crate) fn parse_padding(value: Option<&Value>) -> crate::view::Padding {
    let parse = || -> Option<crate::view::Padding> {
        let a = value?.as_array()?;
        if a.len() != 4 {
            return None;
        }
        let cell = |i: usize| a[i].as_u64().unwrap_or(0) as u16;
        Some(crate::view::Padding {
            top: cell(0),
            right: cell(1),
            bottom: cell(2),
            left: cell(3),
        })
    };
    parse().unwrap_or_default()
}

/// Parse the per-row search-match payload: an array with one entry per visible
/// row, each an array of `[start, end]` screen-column pairs (empty for rows with
/// no match).
pub(crate) fn parse_multi_spans(value: Option<&Value>) -> SearchSpans {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| spans.iter().filter_map(pair_u16).collect())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the per-row `highlights` payload: an array (one entry per visible row)
/// of `[start_col, end_col, group, style_id]` spans in screen columns. The
/// trailing `style_id` (an index into the frame's `styles` palette) is `Nil`
/// when the server couldn't resolve the span through a colorscheme, in which
/// case the client falls back to its own per-group theme.
pub(crate) fn parse_highlights(value: Option<&Value>) -> Vec<Vec<HlSpan>> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|span| {
                                    let t = span.as_array()?;
                                    if t.len() != 4 {
                                        return None;
                                    }
                                    Some((
                                        t[0].as_u64()? as u16,
                                        t[1].as_u64()? as u16,
                                        t[2].as_str()?.to_string(),
                                        t[3].as_u64().map(|id| id as usize),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decode a per-row `[text, severity, style_id]` payload (`Nil` → `None`,
/// malformed → `None`) — the shared shape behind both `diagnostics_virt` (inline
/// text) and `diagnostics_signs` (gutter glyphs), whose `DiagVirt` / `DiagSign`
/// aliases are the same `(String, u8, Option<usize>)` tuple.
fn parse_text_sev_style(value: Option<&Value>) -> Vec<Option<(String, u8, Option<usize>)>> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let t = row.as_array()?;
                    if t.len() != 3 {
                        return None;
                    }
                    Some((
                        t[0].as_str()?.to_string(),
                        t[1].as_u64()? as u8,
                        t[2].as_u64().map(|id| id as usize),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `diagnostics_virt` redraw key into per-row optional inline
/// decorations: each row is `Nil` (no virt text) or `[text, severity, style_id]`.
/// Malformed entries decode to `None`, leaving that row undecorated.
pub(crate) fn parse_diagnostics_virt(value: Option<&Value>) -> Vec<Option<DiagVirt>> {
    parse_text_sev_style(value)
}

/// Parse the `diagnostics_signs` redraw key into per-row optional gutter signs:
/// each row is `Nil` (no sign) or `[glyph, severity, style_id]`. Malformed entries
/// decode to `None`, leaving that row's sign cell blank. Same shape as
/// [`parse_diagnostics_virt`].
pub(crate) fn parse_diagnostics_signs(value: Option<&Value>) -> Vec<Option<DiagSign>> {
    parse_text_sev_style(value)
}

/// Decode one chunk run `[[text, style_id], …]` into `Vec<VirtChunk>`: each `[text,
/// style_id]` pair becomes `(String, Option<usize>)` (a `Nil` id paints in the
/// normal foreground). A non-array value yields an empty run; malformed chunks are
/// dropped. The shared inner decode for `virt_text` chunk runs, `virt_lines`, and
/// the content float's `lines`.
pub(crate) fn parse_chunks(value: &Value) -> Vec<VirtChunk> {
    value
        .as_array()
        .map(|chunks| {
            chunks
                .iter()
                .filter_map(|c| {
                    let c = c.as_array()?;
                    if c.len() != 2 {
                        return None;
                    }
                    Some((
                        c[0].as_str()?.to_string(),
                        c[1].as_u64().map(|id| id as usize),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `virt_text` redraw key into per-row extmark virtual-text placements:
/// each row is an array of `[pos, col, hl_mode, [[text, style_id], …]]` (empty for
/// rows with none). Malformed placements / chunks are dropped. Same per-row-list
/// shape as [`parse_inlay_hints`], but each entry carries a position + chunk run.
pub(crate) fn parse_virt_text(value: Option<&Value>) -> Vec<Vec<VirtPlacement>> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|ps| {
                            ps.iter()
                                .filter_map(|p| {
                                    let t = p.as_array()?;
                                    if t.len() != 4 {
                                        return None;
                                    }
                                    Some(VirtPlacement {
                                        pos: t[0].as_u64()? as u8,
                                        col: t[1].as_u64()? as u16,
                                        hl_mode: t[2].as_u64()? as u8,
                                        chunks: parse_chunks(&t[3]),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the `virt_lines` redraw key into per-row optional virtual-line content:
/// each row is `Nil` (a real text row or a `~` filler) or `[[text, style_id], …]`
/// — the chunk run for a **virtual line** (a whole extra screen row the server
/// interleaved above / below its buffer line). `Some(chunks)` is what tells the
/// client a `None`-number row is a virtual line to paint (in its chunk styles) and
/// not a `~` filler. Malformed chunks are dropped.
pub(crate) fn parse_virt_lines(value: Option<&Value>) -> Vec<Option<Vec<VirtChunk>>> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                // A non-array row stays `None` (a real text row or `~` filler);
                // an array row is a virtual line whose chunks we decode.
                .map(|row| row.as_array().map(|_| parse_chunks(row)))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the content float's `lines` key: an array of chunk runs `[[text,
/// style_id], …]` (the same per-row form as [`parse_virt_lines`], but every row is
/// real content — no `Nil` filler rows). Each line is a styled run; a plain caller
/// is one chunk with a `Nil` style id. Malformed chunks are dropped.
pub(crate) fn parse_float_lines(value: Option<&Value>) -> Vec<Vec<VirtChunk>> {
    value
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(parse_chunks).collect())
        .unwrap_or_default()
}

/// Parse the `inlay_hints` redraw key into per-row inline hints: each row is an
/// array of `[col, text, style_id]` (empty for rows with none). Malformed entries
/// are dropped. Same per-row-list shape as [`parse_diagnostics`], but each entry
/// carries a column + text rather than a span.
pub(crate) fn parse_inlay_hints(value: Option<&Value>) -> Vec<Vec<InlayHint>> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|hints| {
                            hints
                                .iter()
                                .filter_map(|hint| {
                                    let t = hint.as_array()?;
                                    if t.len() != 3 {
                                        return None;
                                    }
                                    Some((
                                        t[0].as_u64()? as u16,
                                        t[1].as_str()?.to_string(),
                                        t[2].as_u64().map(|id| id as usize),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the per-row `diagnostics` payload: an array (one entry per visible
/// row) of `[start_col, end_col, severity, style_id]` underline spans in screen
/// columns. The trailing `style_id` indexes the frame's `styles` palette when
/// the server resolved the `DiagnosticUnderline*` group through a colorscheme;
/// `Nil` falls back to a built-in severity color in the client.
pub(crate) fn parse_diagnostics(value: Option<&Value>) -> Vec<Vec<DiagSpan>> {
    value
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|span| {
                                    let t = span.as_array()?;
                                    if t.len() != 4 {
                                        return None;
                                    }
                                    Some((
                                        t[0].as_u64()? as u16,
                                        t[1].as_u64()? as u16,
                                        t[2].as_u64()? as u8,
                                        t[3].as_u64().map(|id| id as usize),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the per-frame `styles` palette: an array of `{ fg, bg, sp, <attrs> }`
/// maps (colors as `0xRRGGBB` integers, attributes as `true` flags), each
/// converted to a neutral [`Style`]. Highlight spans and chrome regions index
/// into the returned vec.
pub(crate) fn parse_styles(value: Option<&Value>) -> Vec<Style> {
    value
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(style_from_value).collect())
        .unwrap_or_default()
}

/// Build a neutral [`Style`] from one `styles`-palette entry. `fg`/`bg`/`sp`
/// become truecolor; each present boolean sets its attribute flag. Absent fields
/// are left unset so the style patches cleanly onto whatever it is painted over.
fn style_from_value(value: &Value) -> Style {
    let Value::Map(map) = value else {
        return Style::default();
    };
    let flag = |key| map_get(map, key).and_then(Value::as_bool).unwrap_or(false);
    Style {
        fg: rgb_color(map, "fg"),
        bg: rgb_color(map, "bg"),
        sp: rgb_color(map, "sp"),
        bold: flag("bold"),
        italic: flag("italic"),
        underline: flag("underline"),
        undercurl: flag("undercurl"),
        strikethrough: flag("strikethrough"),
        reverse: flag("reverse"),
    }
}

/// Read a `0xRRGGBB` color integer at `key`. `None` when the key is absent. The
/// value is stored as-is; clients unpack the low three bytes (the wire never sets
/// the top byte).
fn rgb_color(map: &[(Value, Value)], key: &str) -> Option<u32> {
    map_get(map, key).and_then(Value::as_u64).map(|c| c as u32)
}

/// Resolve one chrome region (`normal`, `visual`, …) to its style by looking up
/// its id in the `chrome` map and indexing the parsed `styles` palette. `None`
/// when the theme left the group undefined (so the client keeps its built-in
/// look for that region).
pub(crate) fn chrome_style(chrome: Option<&Value>, key: &str, styles: &[Style]) -> Option<Style> {
    let Some(Value::Map(map)) = chrome else {
        return None;
    };
    let id = map_get(map, key).and_then(Value::as_u64)? as usize;
    styles.get(id).copied()
}

/// Parse the per-window `line_bg` layer — the line-background rows (neovim's
/// `line_hl_group`, `hl_eol` semantics): each entry is `[row, style_id]`, resolved
/// against this frame's `styles` palette to `(row, Style)`. The renderer paints each
/// row's background across the full text-area width *before* the text, the way
/// `'cursorline'` does, so syntax spans compose on top. A malformed entry, or one
/// whose `style_id` the palette doesn't hold, is dropped; an absent / empty array
/// (an older server, or no line backgrounds) yields an empty vec.
pub(crate) fn parse_line_bg(value: Option<&Value>, styles: &[Style]) -> Vec<(u16, Style)> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let e = v.as_array()?;
                    let row = e.first()?.as_u64()? as u16;
                    let id = e.get(1)?.as_u64()? as usize;
                    Some((row, *styles.get(id)?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a per-row array of 1-based line numbers (`Nil` rows become `None`).
pub(crate) fn parse_numbers(value: Option<&Value>) -> Vec<Option<usize>> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_u64().map(|n| n as usize)).collect())
        .unwrap_or_default()
}

/// Parse a per-row boolean flag array (e.g. the soft-wrap `continuation` signal).
/// A missing array (older server) or a non-bool entry yields `false`, so the
/// renderer falls back to the prior behavior (every wrapped row shows its number).
pub(crate) fn parse_bools(value: Option<&Value>) -> Vec<bool> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_bool().unwrap_or(false)).collect())
        .unwrap_or_default()
}

/// Parse the per-window `cursors` array — secondary multi-cursor positions, each
/// a `[row, screen_col]` pair — into `(row, col)` tuples. Empty (no extra
/// cursors) or absent (an older server) both yield an empty vec.
pub(crate) fn parse_cursor_list(value: Option<&Value>) -> Vec<(u16, u16)> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(pair_u16).collect())
        .unwrap_or_default()
}

/// Parse a float's wire border name (matching `nvim_win_get_config`) into the
/// neutral [`Border`](crate::style::Border). `"none"`, a missing value, or an
/// unknown name yields `None` (no border).
pub(crate) fn parse_border(value: Option<&Value>) -> Option<crate::style::Border> {
    use crate::style::Border;
    match value.and_then(Value::as_str) {
        Some("single") => Some(Border::Single),
        Some("rounded") => Some(Border::Rounded),
        Some("double") => Some(Border::Double),
        Some("solid") => Some(Border::Solid),
        _ => None,
    }
}
