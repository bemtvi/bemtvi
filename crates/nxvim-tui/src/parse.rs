//! Decoding of the server's `redraw` notification map into the client's render
//! model: small msgpack-map accessors and the per-region span/style parsers.

use ratatui::style::{Color, Modifier, Style};
use rmpv::Value;

/// One treesitter highlight span in screen columns: `(start, end, group,
/// style_id)`. `style_id` indexes the frame's style palette when the server
/// resolved the capture through a loaded colorscheme; `None` falls back to the
/// client's built-in [`group_style`](crate::render::group_style).
pub(crate) type HlSpan = (u16, u16, String, Option<usize>);

/// Per visible row, the screen-column spans of every search match (`hlsearch`).
pub(crate) type SearchSpans = Vec<Vec<(u16, u16)>>;
/// Per visible row, the single span the live `incsearch` preview rests on.
pub(crate) type IncSearchSpans = Vec<Option<(u16, u16)>>;

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
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a per-row array of `[start, end]` selection-span pairs (`Nil` rows
/// become `None`).
pub(crate) fn parse_spans(value: Option<&Value>) -> Vec<Option<(u16, u16)>> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| match v.as_array() {
                    Some(pair) if pair.len() == 2 => Some((
                        pair[0].as_u64().unwrap_or(0) as u16,
                        pair[1].as_u64().unwrap_or(0) as u16,
                    )),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
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
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| match v.as_array() {
                                    Some(pair) if pair.len() == 2 => Some((
                                        pair[0].as_u64().unwrap_or(0) as u16,
                                        pair[1].as_u64().unwrap_or(0) as u16,
                                    )),
                                    _ => None,
                                })
                                .collect()
                        })
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
/// case the client falls back to [`group_style`](crate::render::group_style).
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

/// Parse the per-frame `styles` palette: an array of `{ fg, bg, sp, <attrs> }`
/// maps (colors as `0xRRGGBB` integers, attributes as `true` flags), each
/// converted to the ratatui [`Style`] the renderer paints. Highlight spans and
/// chrome regions index into the returned vec.
pub(crate) fn parse_styles(value: Option<&Value>) -> Vec<Style> {
    value
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(style_from_value).collect())
        .unwrap_or_default()
}

/// Build a ratatui [`Style`] from one `styles`-palette entry. `fg`/`bg` become
/// truecolor; `sp` sets the underline color; each present boolean adds its
/// modifier. Absent fields are left unset so the style patches cleanly onto
/// whatever it is painted over (e.g. the `Normal` background).
fn style_from_value(value: &Value) -> Style {
    let Value::Map(map) = value else {
        return Style::default();
    };
    let mut style = Style::default();
    if let Some(c) = rgb_color(map, "fg") {
        style = style.fg(c);
    }
    if let Some(c) = rgb_color(map, "bg") {
        style = style.bg(c);
    }
    if let Some(c) = rgb_color(map, "sp") {
        style = style.underline_color(c);
    }
    for (key, modifier) in [
        ("bold", Modifier::BOLD),
        ("italic", Modifier::ITALIC),
        ("underline", Modifier::UNDERLINED),
        // ratatui has no undercurl modifier, so it aliases to UNDERLINED here —
        // the underline-color (`sp`) still distinguishes it visually. The server
        // keeps `underline`/`undercurl` as distinct flags (nxvim-lua's `HlSet`).
        ("undercurl", Modifier::UNDERLINED),
        ("strikethrough", Modifier::CROSSED_OUT),
        ("reverse", Modifier::REVERSED),
    ] {
        if map_get(map, key).and_then(Value::as_bool).unwrap_or(false) {
            style = style.add_modifier(modifier);
        }
    }
    style
}

/// Read a `0xRRGGBB` color integer at `key` and unpack it into a truecolor
/// [`Color::Rgb`]. `None` when the key is absent.
fn rgb_color(map: &[(Value, Value)], key: &str) -> Option<Color> {
    let packed = map_get(map, key).and_then(Value::as_u64)?;
    let [_, r, g, b] = (packed as u32).to_be_bytes();
    Some(Color::Rgb(r, g, b))
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

/// Parse a per-row array of 1-based line numbers (`Nil` rows become `None`).
pub(crate) fn parse_numbers(value: Option<&Value>) -> Vec<Option<usize>> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_u64().map(|n| n as usize)).collect())
        .unwrap_or_default()
}
