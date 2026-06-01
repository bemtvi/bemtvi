//! Tier 1: render a known `View` into a cell grid via ratatui's test backend
//! and assert on exactly what a user would see. Synthetic views are the right
//! input here — this pins the *client's painting contract*, not server logic.

use nxvim_tui::{paint, View};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use rmpv::Value;

/// Build a `redraw` params vec (a one-element array holding the view map),
/// matching what the server sends and `View::from_redraw` consumes.
fn redraw(fields: Vec<(&str, Value)>) -> Vec<Value> {
    let mut map: Vec<(Value, Value)> = vec![
        (Value::from("lines"), Value::Array(vec![])),
        (Value::from("cursor_row"), Value::from(0u64)),
        (Value::from("cursor_col"), Value::from(0u64)),
        (Value::from("cursor_screen_col"), Value::from(0u64)),
        (Value::from("mode_label"), Value::from("NORMAL")),
        (Value::from("command_mode"), Value::from(false)),
        (Value::from("cmdline"), Value::from("")),
        (Value::from("message"), Value::from("")),
        (Value::from("file_name"), Value::from("")),
        (Value::from("modified"), Value::from(false)),
        (Value::from("cursor_line"), Value::from(1u64)),
    ];
    for (k, v) in fields {
        if let Some(slot) = map.iter_mut().find(|(mk, _)| mk.as_str() == Some(k)) {
            slot.1 = v;
        } else {
            map.push((Value::from(k), v));
        }
    }
    vec![Value::Map(map)]
}

fn view(fields: Vec<(&str, Value)>) -> View {
    View::from_redraw(&redraw(fields))
}

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
        .collect()
}

fn reversed(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .map(|c| c.style().add_modifier.contains(Modifier::REVERSED))
        .unwrap_or(false)
}

fn lines(strs: &[&str]) -> Value {
    Value::Array(strs.iter().map(|s| Value::from(*s)).collect())
}

#[test]
fn text_is_painted_on_the_top_rows() {
    let v = view(vec![("lines", lines(&["hello"]))]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "hello");
}

#[test]
fn bottom_two_rows_are_status_and_command_chrome() {
    let v = view(vec![
        ("lines", lines(&["abc"])),
        ("file_name", Value::from("f.txt")),
    ]);
    let buf = paint(&v, 20, 5);
    assert!(
        row_text(&buf, 3).contains("NORMAL"),
        "status: {:?}",
        row_text(&buf, 3)
    );
    assert!(
        row_text(&buf, 3).contains("f.txt"),
        "status: {:?}",
        row_text(&buf, 3)
    );
    assert_eq!(row_text(&buf, 4).trim_end(), "");
}

#[test]
fn status_row_is_reversed() {
    let v = view(vec![("lines", lines(&["abc"]))]);
    let buf = paint(&v, 20, 5);
    assert!(reversed(&buf, 0, 3), "status row should be reverse-video");
}

#[test]
fn a_selection_span_highlights_exactly_its_cells() {
    let sel = Value::Array(vec![Value::Array(vec![
        Value::from(0u64),
        Value::from(3u64),
    ])]);
    let v = view(vec![("lines", lines(&["hello"])), ("selection", sel)]);
    let buf = paint(&v, 20, 5);
    assert!(reversed(&buf, 0, 0));
    assert!(reversed(&buf, 1, 0));
    assert!(reversed(&buf, 2, 0));
    assert!(
        !reversed(&buf, 3, 0),
        "cell past the span must not be highlighted"
    );
}

#[test]
fn wide_chars_occupy_two_cells_each() {
    let v = view(vec![("lines", lines(&["日本"]))]);
    let buf = paint(&v, 20, 5);
    assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "日");
    assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "本");
}

fn numbers(vals: &[Option<u64>]) -> Value {
    Value::Array(
        vals.iter()
            .map(|v| match v {
                Some(n) => Value::from(*n),
                None => Value::Nil,
            })
            .collect(),
    )
}

/// A view configured with the hybrid number column on (the server's default).
fn numbered(lines_: Value, nums: &[Option<u64>], cursor_line: u64) -> View {
    view(vec![
        ("lines", lines_),
        ("numbers", numbers(nums)),
        ("number", Value::from(true)),
        ("relativenumber", Value::from(true)),
        ("number_width", Value::from(4u64)),
        ("cursor_line", Value::from(cursor_line)),
    ])
}

#[test]
fn hybrid_gutter_shows_absolute_on_cursor_line_relative_elsewhere() {
    // Cursor on line 2 of three; gutter is 4 cells, then the text.
    let v = numbered(
        lines(&["one", "two", "three"]),
        &[Some(1), Some(2), Some(3)],
        2,
    );
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "  1 one"); // relative 1, right-aligned
    assert_eq!(row_text(&buf, 1).trim_end(), "2   two"); // absolute 2, left-aligned
    assert_eq!(row_text(&buf, 2).trim_end(), "  1 three"); // relative 1, right-aligned
}

#[test]
fn filler_rows_have_a_blank_gutter() {
    // One line of text; the rows below it are `~` fillers with no number.
    let v = numbered(lines(&["only", "~"]), &[Some(1), None], 1);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "1   only");
    // The `~` row's gutter is all blanks; the tilde sits in the text column.
    assert_eq!(row_text(&buf, 1).trim_end(), "    ~");
}

#[test]
fn gutter_disabled_paints_text_at_column_zero() {
    // No number options (the Tier-1 default): text is flush left, no gutter.
    let v = view(vec![("lines", lines(&["hello"]))]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "hello");
}

#[test]
fn command_mode_renders_the_colon_line() {
    let v = view(vec![
        ("command_mode", Value::from(true)),
        ("cmdline", Value::from("w")),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 4).trim_end(), ":w");
}

// ----- Phase 5: truecolor styles from the server's resolved payload ----------

/// One `styles`-palette entry: `0xRRGGBB` ints for the colors present, attribute
/// flags for the booleans present. Mirrors the server's `style_value` encoding.
fn style(fields: Vec<(&str, Value)>) -> Value {
    Value::Map(
        fields
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

/// A 24-bit color as the `0xRRGGBB` integer the server sends.
fn rgb(r: u8, g: u8, b: u8) -> Value {
    Value::from(((r as u64) << 16) | ((g as u64) << 8) | b as u64)
}

/// One row of highlight spans as `[start, end, group, style_id]` (style id `Nil`
/// to force the client's built-in fallback for that span).
fn hl_row(spans: &[(u64, u64, &str, Option<u64>)]) -> Value {
    Value::Array(
        spans
            .iter()
            .map(|(s, e, group, id)| {
                Value::Array(vec![
                    Value::from(*s),
                    Value::from(*e),
                    Value::from(*group),
                    id.map(Value::from).unwrap_or(Value::Nil),
                ])
            })
            .collect(),
    )
}

fn chrome(entries: Vec<(&str, u64)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(k, id)| (Value::from(k), Value::from(id)))
            .collect(),
    )
}

#[test]
fn a_resolved_style_paints_its_truecolor_foreground() {
    // Palette entry 0 is mauve; the span over cols 0..2 references it.
    let v = view(vec![
        ("lines", lines(&["fn x"])),
        (
            "styles",
            Value::Array(vec![style(vec![("fg", rgb(0xcb, 0xa6, 0xf7))])]),
        ),
        (
            "highlights",
            Value::Array(vec![hl_row(&[(0, 2, "keyword", Some(0))])]),
        ),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(
        buf.cell((0, 0)).unwrap().style().fg,
        Some(Color::Rgb(0xcb, 0xa6, 0xf7)),
        "the resolved keyword span paints its truecolor fg"
    );
    assert_eq!(
        buf.cell((2, 0)).unwrap().style().fg,
        Some(Color::Reset),
        "a cell past the span is left at the default fg"
    );
}

#[test]
fn the_normal_background_fills_the_text_area() {
    let v = view(vec![
        ("lines", lines(&["hi"])),
        (
            "styles",
            Value::Array(vec![style(vec![
                ("fg", rgb(0xcd, 0xd6, 0xf4)),
                ("bg", rgb(0x1e, 0x1e, 0x2e)),
            ])]),
        ),
        ("chrome", chrome(vec![("normal", 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    // An empty cell well past the text still carries the Normal background.
    assert_eq!(
        buf.cell((10, 0)).unwrap().style().bg,
        Some(Color::Rgb(0x1e, 0x1e, 0x2e)),
        "the editor background fills the whole text area"
    );
}

#[test]
fn the_visual_style_replaces_reverse_video_when_themed() {
    let sel = Value::Array(vec![Value::Array(vec![
        Value::from(0u64),
        Value::from(2u64),
    ])]);
    let v = view(vec![
        ("lines", lines(&["hi"])),
        ("selection", sel),
        (
            "styles",
            Value::Array(vec![style(vec![("bg", rgb(0x45, 0x47, 0x5a))])]),
        ),
        ("chrome", chrome(vec![("visual", 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    let cell = buf.cell((0, 0)).unwrap().style();
    assert_eq!(
        cell.bg,
        Some(Color::Rgb(0x45, 0x47, 0x5a)),
        "the selection takes Visual's background"
    );
    assert!(
        !cell.add_modifier.contains(Modifier::REVERSED),
        "a themed Visual replaces reverse-video rather than adding to it"
    );
}

#[test]
fn no_colorscheme_falls_back_to_the_builtin_theme() {
    // A span with a Nil style id and no palette: the client paints from its own
    // built-in `group_style` (keyword → magenta), and the selection reverts to
    // reverse-video — exactly today's default-startup behavior.
    let sel = Value::Array(vec![Value::Array(vec![
        Value::from(3u64),
        Value::from(5u64),
    ])]);
    let v = view(vec![
        ("lines", lines(&["fn x()"])),
        ("selection", sel),
        (
            "highlights",
            Value::Array(vec![hl_row(&[(0, 2, "keyword", None)])]),
        ),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(
        buf.cell((0, 0)).unwrap().style().fg,
        Some(Color::Magenta),
        "with no resolved style the client falls back to group_style"
    );
    assert_eq!(
        buf.cell((0, 0)).unwrap().style().bg,
        Some(Color::Reset),
        "no colorscheme means no editor background — terminal default shows through"
    );
    assert!(
        reversed(&buf, 3, 0),
        "with no Visual style the selection stays reverse-video"
    );
}
