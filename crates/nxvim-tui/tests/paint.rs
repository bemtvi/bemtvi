//! Tier 1: render a known `View` into a cell grid via ratatui's test backend
//! and assert on exactly what a user would see. Synthetic views are the right
//! input here — this pins the *client's painting contract*, not server logic.

use nxvim_tui::{paint, View};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
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
