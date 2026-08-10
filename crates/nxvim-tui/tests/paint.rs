//! Tier 1: render a known `View` into a cell grid via ratatui's test backend
//! and assert on exactly what a user would see. Synthetic views are the right
//! input here — this pins the *client's painting contract*, not server logic.

use crossterm::cursor::SetCursorStyle;
use nxvim_tui::{cursor_style, paint, paint_with_cursor, ScrollHarness};
use nxvim_view::View;
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

fn underlined(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .map(|c| c.style().add_modifier.contains(Modifier::UNDERLINED))
        .unwrap_or(false)
}

fn bg(buf: &Buffer, x: u16, y: u16) -> Option<Color> {
    buf.cell((x, y)).and_then(|c| c.style().bg)
}

fn fg(buf: &Buffer, x: u16, y: u16) -> Option<Color> {
    buf.cell((x, y)).and_then(|c| c.style().fg)
}

fn underline_color(buf: &Buffer, x: u16, y: u16) -> Option<Color> {
    buf.cell((x, y)).and_then(|c| c.style().underline_color)
}

fn lines(strs: &[&str]) -> Value {
    Value::Array(strs.iter().map(|s| Value::from(*s)).collect())
}

/// A window's `status` array: one `{ text, style }` segment per `(text, style)`,
/// where `style` is an index into the frame's `styles` palette (`None` ⇒ the wire
/// `Nil`, the base `StatusLine` look). Mirrors the server's `segment_value`.
fn status(segments: &[(&str, Option<u64>)]) -> Value {
    Value::Array(
        segments
            .iter()
            .map(|(text, style)| {
                Value::Map(vec![
                    (Value::from("text"), Value::from(*text)),
                    (Value::from("style"), style.map_or(Value::Nil, Value::from)),
                ])
            })
            .collect(),
    )
}

#[test]
fn cursor_shape_follows_the_mode() {
    // Insert shows the thin "edit cursor" bar, replace an underline, and every
    // other mode keeps the block — matching vim/neovim's default `guicursor`.
    let insert = view(vec![("mode_label", Value::from("INSERT"))]);
    assert_eq!(cursor_style(&insert), SetCursorStyle::SteadyBar);

    let replace = view(vec![("mode_label", Value::from("REPLACE"))]);
    assert_eq!(cursor_style(&replace), SetCursorStyle::SteadyUnderScore);

    // `r` waits for its replacement char in normal mode — still the replace cursor.
    let pending_r = view(vec![("pending_replace", Value::from(true))]);
    assert_eq!(cursor_style(&pending_r), SetCursorStyle::SteadyUnderScore);

    for label in ["NORMAL", "VISUAL", "V-LINE", "COMMAND"] {
        let v = view(vec![("mode_label", Value::from(label))]);
        assert_eq!(
            cursor_style(&v),
            SetCursorStyle::SteadyBlock,
            "{label} mode should keep the block cursor"
        );
    }
}

/// A window's `cursors` array: one `[row, screen_col]` pair per secondary cursor.
fn cursors(positions: &[(u64, u64)]) -> Value {
    Value::Array(
        positions
            .iter()
            .map(|&(r, c)| Value::Array(vec![Value::from(r), Value::from(c)]))
            .collect(),
    )
}

#[test]
fn secondary_cursors_paint_as_reverse_video_blocks() {
    // The terminal's one real cursor is the primary; extra multi-cursors are
    // painted as reverse-video block cells so they're visible.
    let v = view(vec![
        ("lines", lines(&["abc", "def"])),
        ("cursors", cursors(&[(1, 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    assert!(
        reversed(&buf, 0, 1),
        "the secondary cursor cell (row 1, col 0) should be reverse-video"
    );
    assert!(
        !reversed(&buf, 1, 1),
        "a neighboring non-cursor cell stays normal"
    );
}

#[test]
fn secondary_cursor_shape_follows_the_mode() {
    // The extra cursor mirrors the primary's mode-driven shape: a block (normal)
    // paints reverse-video; the insert bar shape — unpaintable in a cell — shows
    // as an underline, so a mode change propagates to every cursor.
    let normal = view(vec![
        ("lines", lines(&["abc", "def"])),
        ("cursors", cursors(&[(1, 0)])),
    ]);
    let buf = paint(&normal, 20, 5);
    assert!(reversed(&buf, 0, 1), "normal mode → reverse-video block");
    assert!(!underlined(&buf, 0, 1));

    let insert = view(vec![
        ("lines", lines(&["abc", "def"])),
        ("cursors", cursors(&[(1, 0)])),
        ("mode_label", Value::from("INSERT")),
    ]);
    let buf = paint(&insert, 20, 5);
    assert!(underlined(&buf, 0, 1), "insert mode → underline");
    assert!(!reversed(&buf, 0, 1), "no block in insert mode");
}

#[test]
fn secondary_cursor_underline_uses_a_distinct_accent_color() {
    // The primary insert cursor is a true bar; a secondary can only be a styled
    // cell, so it shows as an underline. To keep it from blending into the text's
    // own underlines, the underline is tinted with the multi-cursor accent.
    let insert = view(vec![
        ("lines", lines(&["abc", "def"])),
        ("cursors", cursors(&[(1, 0)])),
        ("mode_label", Value::from("INSERT")),
    ]);
    let buf = paint(&insert, 20, 5);
    assert_eq!(
        underline_color(&buf, 0, 1),
        Some(Color::Rgb(229, 192, 123)),
        "the secondary cursor's underline is tinted with the multi-cursor accent"
    );
}

#[test]
fn multicursor_mode_recolors_the_active_cursor() {
    // In MULTICURSOR placement mode the active (primary) cursor cell is recolored
    // with a distinct background, so it reads as "dropping cursors".
    let v = view(vec![
        ("lines", lines(&["abc", "def"])),
        ("mode_label", Value::from("MULTICURSOR")),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(
        bg(&buf, 0, 0),
        Some(Color::Rgb(229, 192, 123)),
        "the active cursor (0,0) is recolored in placement mode"
    );

    // In normal mode it is the plain terminal cursor — no recolored cell.
    let v = view(vec![("lines", lines(&["abc", "def"]))]);
    let buf = paint(&v, 20, 5);
    assert_ne!(bg(&buf, 0, 0), Some(Color::Rgb(229, 192, 123)));
}

#[test]
fn text_is_painted_on_the_top_rows() {
    let v = view(vec![("lines", lines(&["hello"]))]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "hello");
}

#[test]
fn tabs_expand_to_the_buffer_tabstop() {
    // Regression: a leading tab must render to the buffer's tabstop (mirrored
    // from the server), not a hard-coded width. With tabstop=4 the tab fills 4
    // cells, so the painted text matches the server's cursor_screen_col=4 and the
    // cursor sits on 'x' rather than in the middle of the tab.
    let v = view(vec![
        ("lines", lines(&["\tx"])),
        ("tabstop", Value::from(4u64)),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "    x");

    // A different tabstop changes the width: 2 cells.
    let v2 = view(vec![
        ("lines", lines(&["\tx"])),
        ("tabstop", Value::from(2u64)),
    ]);
    let buf2 = paint(&v2, 20, 5);
    assert_eq!(row_text(&buf2, 0).trim_end(), "  x");
}

#[test]
fn bottom_two_rows_are_status_and_command_chrome() {
    let v = view(vec![
        ("lines", lines(&["abc"])),
        ("status", status(&[(" NORMAL  f.txt", None)])),
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
fn status_segments_paint_their_own_styles_over_the_base() {
    // Two segments: a plain prefix in the base look, then `RED` carrying palette
    // entry 0 (a red foreground). The client paints each segment with its own
    // style, patched onto the reverse-video base — so the styled run keeps the
    // base's REVERSED while overriding the foreground.
    let v = view(vec![
        ("lines", lines(&["abc"])),
        (
            "styles",
            Value::Array(vec![style(vec![("fg", rgb(0xff, 0, 0))])]),
        ),
        ("status", status(&[("ab", None), ("RED", Some(0))])),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 3).trim_end(), "abRED");
    // The prefix has no foreground of its own (base reverse-video only)...
    assert!(reversed(&buf, 0, 3), "the prefix keeps the base look");
    // ...while the styled `RED` segment (cols 2..5) paints its red fg, still
    // reverse-video from the patched base.
    assert_eq!(
        buf.cell((2, 3)).unwrap().style().fg,
        Some(Color::Rgb(255, 0, 0))
    );
    assert!(
        reversed(&buf, 2, 3),
        "the styled segment keeps the base REVERSED"
    );
}

#[test]
fn status_segment_with_its_own_bg_is_not_inverted_by_the_reverse_base() {
    // A lualine-style themed segment carries BOTH fg and bg (here dark-on-blue,
    // like `lualine_a_normal`). With no `StatusLine` group loaded the base look is
    // reverse-video — but a segment supplying its own bg must paint those literal
    // colours, NOT inherit the base's REVERSED (which would swap fg/bg and render
    // blue text on a dark bar). Regression: themed statuslines looked inverted.
    let v = view(vec![
        ("lines", lines(&["abc"])),
        (
            "styles",
            Value::Array(vec![style(vec![
                ("fg", rgb(0x1c, 0x1c, 0x1c)),
                ("bg", rgb(0x5f, 0xaf, 0xff)),
            ])]),
        ),
        ("status", status(&[("NORMAL", Some(0))])),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(row_text(&buf, 3).trim_end(), "NORMAL");
    assert!(
        !reversed(&buf, 0, 3),
        "a segment with its own bg must not inherit the base REVERSED"
    );
    assert_eq!(
        buf.cell((0, 3)).unwrap().style().fg,
        Some(Color::Rgb(0x1c, 0x1c, 0x1c)),
        "the segment paints its literal foreground (dark)"
    );
    assert_eq!(
        buf.cell((0, 3)).unwrap().style().bg,
        Some(Color::Rgb(0x5f, 0xaf, 0xff)),
        "the segment paints its literal background (blue)"
    );
}

/// A `{ x, y, width, height }` rect sub-map.
fn rect(x: u64, y: u64, w: u64, h: u64) -> Value {
    Value::Map(vec![
        (Value::from("x"), Value::from(x)),
        (Value::from("y"), Value::from(y)),
        (Value::from("width"), Value::from(w)),
        (Value::from("height"), Value::from(h)),
    ])
}

/// One window sub-map for the `windows` redraw array. Its status line is the
/// single projected segment ` NORMAL  {file}`, matching the server's projection.
fn window(r: Value, focused: bool, file: &str, text: &[&str]) -> Value {
    let label = format!(" NORMAL  {file}");
    Value::Map(vec![
        (Value::from("rect"), r),
        (Value::from("focused"), Value::from(focused)),
        (Value::from("lines"), lines(text)),
        (Value::from("status"), status(&[(&label, None)])),
        (Value::from("cursor_line"), Value::from(1u64)),
    ])
}

#[test]
fn two_stacked_windows_each_paint_text_a_status_line_and_a_separator() {
    // A 20×10 grid: the windows area is rows 0..9 (the command line is row 9),
    // split into a top window (rows 0..4), a horizontal separator (row 4), and a
    // bottom window (rows 5..9). Each window paints its text on its top rows and
    // a status line on its bottom row; the bottom window holds focus.
    let windows = Value::Array(vec![
        window(rect(0, 0, 20, 4), false, "top.txt", &["top text"]),
        window(rect(0, 5, 20, 4), true, "bot.txt", &["bottom text"]),
    ]);
    let separators = Value::Array(vec![Value::Map(vec![
        (Value::from("vertical"), Value::from(false)),
        (Value::from("x"), Value::from(0u64)),
        (Value::from("y"), Value::from(4u64)),
        (Value::from("length"), Value::from(20u64)),
    ])]);
    let v = view(vec![("windows", windows), ("separators", separators)]);
    let buf = paint(&v, 20, 10);

    // Top window: text on row 0, its own status line on row 3.
    assert_eq!(row_text(&buf, 0).trim_end(), "top text");
    assert!(
        row_text(&buf, 3).contains("top.txt"),
        "top status: {:?}",
        row_text(&buf, 3)
    );
    // The horizontal separator between the windows.
    assert_eq!(row_text(&buf, 4), "─".repeat(20));
    // Bottom window: text on row 5, its own status line on row 8.
    assert_eq!(row_text(&buf, 5).trim_end(), "bottom text");
    assert!(
        row_text(&buf, 8).contains("bot.txt"),
        "bottom status: {:?}",
        row_text(&buf, 8)
    );
    // Both status rows are reverse-video; the command row (9) is blank.
    assert!(reversed(&buf, 0, 3), "top status reversed");
    assert!(reversed(&buf, 0, 8), "bottom status reversed");
    assert_eq!(row_text(&buf, 9).trim_end(), "");
}

#[test]
fn separator_recolors_when_the_colorscheme_switches() {
    // Repro of the reported bug: open under a dark theme, split (a separator
    // appears), switch to a light theme. The separator must repaint to the new
    // theme's WinSeparator, not keep the old dark colour. Uses the in-place
    // `View::update` the live TUI client feeds each redraw through.
    let windows = || {
        Value::Array(vec![
            window(rect(0, 0, 20, 4), false, "top.txt", &["top"]),
            window(rect(0, 5, 20, 4), true, "bot.txt", &["bot"]),
        ])
    };
    let separators = || {
        Value::Array(vec![Value::Map(vec![
            (Value::from("vertical"), Value::from(false)),
            (Value::from("x"), Value::from(0u64)),
            (Value::from("y"), Value::from(4u64)),
            (Value::from("length"), Value::from(20u64)),
        ])])
    };
    // Palette: 0 = dark WinSeparator fg + dark Normal bg, 1 = light WinSeparator fg
    // + light Normal bg. WinSeparator carries only a fg (like catppuccin), so the
    // separator cell's background must come from Normal — otherwise it keeps the
    // terminal's own bg and reads as a dark strip on the light theme.
    let styles = Value::Array(vec![
        Value::Map(vec![(Value::from("fg"), Value::from(0x11_11_1bu64))]), // 0: dark sep fg
        Value::Map(vec![(Value::from("fg"), Value::from(0xdc_e0_e8u64))]), // 1: light sep fg
        Value::Map(vec![(Value::from("bg"), Value::from(0x1e_1e_2eu64))]), // 2: dark Normal bg
        Value::Map(vec![(Value::from("bg"), Value::from(0xef_f1_f5u64))]), // 3: light Normal bg
    ]);
    let chrome = |sep: u64, normal: u64| {
        Value::Map(vec![
            (Value::from("win_separator"), Value::from(sep)),
            (Value::from("normal"), Value::from(normal)),
        ])
    };

    // Frame 1 — dark theme: the horizontal separator on row 4 is dark crust on the
    // dark Normal background.
    let mut v = view(vec![
        ("windows", windows()),
        ("separators", separators()),
        ("styles", styles.clone()),
        ("chrome", chrome(0, 2)),
    ]);
    let buf = paint(&v, 20, 10);
    assert_eq!(row_text(&buf, 4), "─".repeat(20), "the separator is drawn");
    assert_eq!(
        fg(&buf, 0, 4),
        Some(Color::Rgb(0x11, 0x11, 0x1b)),
        "dark sep fg"
    );
    assert_eq!(
        bg(&buf, 0, 4),
        Some(Color::Rgb(0x1e, 0x1e, 0x2e)),
        "the separator cell takes Normal's bg, not the terminal default"
    );

    // Frame 2 — switch to the light theme, fed through the same in-place update path
    // the live client uses. Both the glyph and the cell bg must repaint.
    v.update(&redraw(vec![
        ("windows", windows()),
        ("separators", separators()),
        ("styles", styles.clone()),
        ("chrome", chrome(1, 3)),
    ]));
    let buf2 = paint(&v, 20, 10);
    assert_eq!(
        fg(&buf2, 0, 4),
        Some(Color::Rgb(0xdc, 0xe0, 0xe8)),
        "after switching themes the separator glyph repaints light"
    );
    assert_eq!(
        bg(&buf2, 0, 4),
        Some(Color::Rgb(0xef, 0xf1, 0xf5)),
        "…and the separator cell background repaints to the light Normal (not a dark strip)"
    );
}

/// A window sub-map carrying a `region` (for dock layout tests). No status row.
fn region_window(r: Value, region: &str, focused: bool, text: &[&str]) -> Value {
    Value::Map(vec![
        (Value::from("rect"), r),
        (Value::from("region"), Value::from(region)),
        (Value::from("focused"), Value::from(focused)),
        (Value::from("lines"), lines(text)),
        (Value::from("status_visible"), Value::from(false)),
        (Value::from("cursor_line"), Value::from(1u64)),
    ])
}

#[test]
fn a_left_dock_paints_left_of_the_main_area_with_a_border() {
    // 30×6 grid (cmd row 5). A left dock of width 10 reserves columns 0..10 (the
    // dock content) plus a separator at column 10; the main area starts at col 11.
    let windows = Value::Array(vec![
        region_window(rect(0, 0, 10, 5), "dock_left", false, &["SIDEBAR"]),
        region_window(rect(0, 0, 19, 5), "main", true, &["MAIN-AREA"]),
    ]);
    let v = view(vec![
        ("windows", windows),
        ("dock_left", Value::from(10u64)),
    ]);
    let buf = paint(&v, 30, 6);

    // The dock's text sits at column 0; the main text starts past the band (col 11).
    assert!(
        row_text(&buf, 0).starts_with("SIDEBAR"),
        "{:?}",
        row_text(&buf, 0)
    );
    let main_at: String = row_text(&buf, 0).chars().skip(11).take(9).collect();
    assert_eq!(main_at, "MAIN-AREA", "main offset past the dock band");
    // The vertical dock border is painted at column 10 — heavy `┃`, distinct from
    // the light `│` between ordinary window splits.
    assert_eq!(
        buf.cell((10, 0)).unwrap().symbol(),
        "┃",
        "dock border column"
    );
}

/// First `(row, col)` where `needle` appears in the painted grid, scanning rows
/// top-to-bottom. `col` is a cell column — `row_text` joins cell symbols, so the
/// byte offset is mapped back to a char count (border glyphs are multibyte).
fn find_text(buf: &Buffer, needle: &str) -> Option<(u16, usize)> {
    (0..buf.area.height).find_map(|y| {
        let row = row_text(buf, y);
        row.find(needle).map(|b| (y, row[..b].chars().count()))
    })
}

#[test]
fn a_top_dock_paints_above_the_main_area() {
    // 20×8 grid (cmd row 7). A top dock of height 2 owns rows 0..2 with a border on
    // row 2; the main area starts on row 3.
    let windows = Value::Array(vec![
        region_window(rect(0, 0, 20, 2), "dock_top", false, &["TOPBAR"]),
        region_window(rect(0, 0, 20, 4), "main", true, &["MAIN"]),
    ]);
    let v = view(vec![("windows", windows), ("dock_top", Value::from(2u64))]);
    let buf = paint(&v, 20, 8);

    assert!(row_text(&buf, 0).starts_with("TOPBAR"), "top dock on row 0");
    assert_eq!(
        row_text(&buf, 2),
        "━".repeat(20),
        "top dock border on row 2 (heavy, distinct from split borders)"
    );
    assert!(
        row_text(&buf, 3).starts_with("MAIN"),
        "main area below the top dock"
    );
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
fn secondary_selection_spans_highlight_their_cells() {
    // Row 0 carries the primary selection (cols [0,3)); row 1 carries a secondary
    // multi-cursor's selection (cols [0,3)) in `secondary_selection` — both paint
    // as reverse-video, so a multi-cursor visual selection shows on every cursor.
    let primary = Value::Array(vec![
        Value::Array(vec![Value::from(0u64), Value::from(3u64)]),
        Value::Nil,
    ]);
    let secondary = Value::Array(vec![
        Value::Array(vec![]), // row 0: no secondary selection
        Value::Array(vec![Value::Array(vec![
            Value::from(0u64),
            Value::from(3u64),
        ])]),
    ]);
    let v = view(vec![
        ("lines", lines(&["hello", "hello"])),
        ("selection", primary),
        ("secondary_selection", secondary),
    ]);
    let buf = paint(&v, 20, 5);
    // Primary selection on row 0.
    assert!(reversed(&buf, 0, 0) && reversed(&buf, 2, 0));
    // Secondary selection on row 1.
    assert!(
        reversed(&buf, 0, 1) && reversed(&buf, 1, 1) && reversed(&buf, 2, 1),
        "the secondary cursor's selection cells are reverse-video"
    );
    assert!(
        !reversed(&buf, 3, 1),
        "the cell past the secondary span must not be highlighted"
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
fn cursor_lands_on_its_character_with_the_number_gutter_on() {
    // Repro for a reported cursor offset: with the number gutter on (the server
    // default), the cursor must be placed at gutter_width + cursor_screen_col — the
    // same cell the character is painted on — not shifted back over the gutter.
    let v = view(vec![
        ("lines", lines(&["hello", "world"])),
        ("numbers", numbers(&[Some(1), Some(2)])),
        ("number", Value::from(true)),
        ("relativenumber", Value::from(true)),
        ("number_width", Value::from(4u64)),
        ("cursor_line", Value::from(1u64)),
        ("cursor_row", Value::from(0u64)),
        ("cursor_screen_col", Value::from(2u64)),
    ]);
    let (buf, cursor) = paint_with_cursor(&v, 20, 5);
    // Text starts past the 4-cell gutter: "hello" at cols 4..9.
    assert_eq!(row_text(&buf, 0).trim_end(), "1   hello");
    // The character under the cursor (screen col 2 = 'l') is painted at col 4+2 = 6.
    assert_eq!(buf.cell((6, 0)).unwrap().symbol(), "l");
    // The cursor must sit ON that cell, not shifted left over the gutter.
    assert_eq!(
        cursor,
        Some((6, 0)),
        "cursor on its character, gutter accounted for"
    );
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

#[test]
fn command_cursor_lands_past_wide_chars() {
    // The server's `cmdline_cursor` is a *char* offset, but the terminal cursor
    // needs a *display column*. With CJK in the line the two diverge: "e 日本" is
    // 4 chars but 6 cells (each CJK glyph is 2 cells wide), so a caret placed at
    // prompt + char-offset lands mid-glyph, two cells short of the real end.
    let v = view(vec![
        ("command_mode", Value::from(true)),
        ("cmdline", Value::from("e 日本")),
        ("cmdline_cursor", Value::from(4u64)), // char offset: end of the line
    ]);
    let (buf, cursor) = paint_with_cursor(&v, 20, 5);
    // ':' col 0, 'e' 1, ' ' 2, 日 3–4, 本 5–6 (each wide glyph spans two cells).
    assert_eq!(buf.cell((3, 4)).unwrap().symbol(), "日");
    assert_eq!(buf.cell((5, 4)).unwrap().symbol(), "本");
    // The caret sits past the wide glyphs' display width, at col 7.
    assert_eq!(
        cursor,
        Some((7, 4)),
        "caret past the wide chars' display width, not at the char count"
    );
}

#[test]
fn command_cursor_accounts_for_a_wide_prompt() {
    // A multi-char `nx.ui.input` prompt with wide chars offsets the caret by its
    // display width (名(2)+前(2)+>(1)+space(1) = 6 cells), not its char count (4).
    let v = view(vec![
        ("command_mode", Value::from(true)),
        ("cmdline_prompt", Value::from("名前> ")),
        ("cmdline", Value::from("ab")),
        ("cmdline_cursor", Value::from(2u64)),
    ]);
    let (_buf, cursor) = paint_with_cursor(&v, 20, 5);
    assert_eq!(cursor, Some((8, 4)), "prompt width measured in cells");
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
fn a_windows_winhighlight_chrome_override_repaints_only_its_own_background() {
    // Two stacked windows over one palette: entry 0 = global Normal (#1e1e2e),
    // entry 1 = a sidebar NormalSB (#202030). The top window has no `winhighlight`,
    // so it uses the global `chrome.normal`; the bottom window carries a per-window
    // `chrome` override remapping `normal` to palette 1 (a dock with
    // `winhighlight = 'Normal:NormalSB'`). Each background paints independently.
    let top = match window(rect(0, 0, 20, 4), false, "main.txt", &["main"]) {
        Value::Map(m) => m,
        _ => unreachable!(),
    };
    let mut bottom = match window(rect(0, 5, 20, 4), true, "dock.txt", &["dock"]) {
        Value::Map(m) => m,
        _ => unreachable!(),
    };
    // Only the bottom window renames `normal` → palette entry 1.
    bottom.push((Value::from("chrome"), chrome(vec![("normal", 1)])));

    let v = view(vec![
        (
            "styles",
            Value::Array(vec![
                style(vec![("bg", rgb(0x1e, 0x1e, 0x2e))]),
                style(vec![("bg", rgb(0x20, 0x20, 0x30))]),
            ]),
        ),
        ("chrome", chrome(vec![("normal", 0)])),
        (
            "windows",
            Value::Array(vec![Value::Map(top), Value::Map(bottom)]),
        ),
    ]);
    let buf = paint(&v, 20, 10);

    // Top window (no override): global Normal background.
    assert_eq!(
        bg(&buf, 10, 0),
        Some(Color::Rgb(0x1e, 0x1e, 0x2e)),
        "the un-remapped window keeps the global Normal background"
    );
    // Bottom window (winhighlight Normal:NormalSB): the override background.
    assert_eq!(
        bg(&buf, 10, 5),
        Some(Color::Rgb(0x20, 0x20, 0x30)),
        "the winhighlight window repaints its own background with NormalSB"
    );
}

#[test]
fn the_padding_margin_shares_the_normal_background() {
    // A tiled window with a 2-cell `'padding'` margin and a themed `Normal`
    // background: the blank margin cells around the content must carry that same
    // background, not the terminal default — otherwise the margin reads as a hole
    // in the editor surface.
    let win = Value::Map(vec![
        (Value::from("rect"), rect(0, 0, 20, 8)),
        (Value::from("focused"), Value::from(true)),
        (Value::from("lines"), lines(&["hi"])),
        (Value::from("status_visible"), Value::from(false)),
        (Value::from("cursor_line"), Value::from(1u64)),
        // [top, right, bottom, left] — a uniform 2-cell margin.
        (
            Value::from("padding"),
            Value::Array(vec![
                Value::from(2u64),
                Value::from(2u64),
                Value::from(2u64),
                Value::from(2u64),
            ]),
        ),
    ]);
    let v = view(vec![
        ("windows", Value::Array(vec![win])),
        (
            "styles",
            Value::Array(vec![style(vec![("bg", rgb(0x1e, 0x1e, 0x2e))])]),
        ),
        ("chrome", chrome(vec![("normal", 0)])),
    ]);
    let buf = paint(&v, 20, 10);
    // The top-left corner cell sits in the blank margin (content starts at 2,2).
    assert_eq!(
        bg(&buf, 0, 0),
        Some(Color::Rgb(0x1e, 0x1e, 0x2e)),
        "the padding margin shares the Normal background, not the terminal default"
    );
}

#[test]
fn a_floats_body_matches_its_border_background() {
    // A bordered float whose theme gives `NormalFloat` a distinct background: the
    // border box already paints that bg, and the inner text body must match it
    // (not the editor's `Normal`), so the float reads as one solid panel rather
    // than a `Normal`-colored body inside a `NormalFloat` frame.
    let f = Value::Map(vec![
        (Value::from("rect"), rect(2, 1, 12, 5)),
        (Value::from("focused"), Value::from(true)),
        (Value::from("floating"), Value::from(true)),
        (Value::from("border"), Value::from("single")),
        (Value::from("lines"), lines(&["hi"])),
        (Value::from("status_visible"), Value::from(false)),
        (Value::from("cursor_line"), Value::from(1u64)),
    ]);
    let v = view(vec![
        ("windows", Value::Array(vec![f])),
        (
            "styles",
            Value::Array(vec![
                style(vec![("bg", rgb(0x1e, 0x1e, 0x2e))]), // 0 = Normal
                style(vec![("bg", rgb(0x30, 0x30, 0x46))]), // 1 = NormalFloat
            ]),
        ),
        ("chrome", chrome(vec![("normal", 0), ("normal_float", 1)])),
    ]);
    let buf = paint(&v, 20, 10);
    // The float's top-left border corner carries the NormalFloat background.
    assert_eq!(
        bg(&buf, 2, 1),
        Some(Color::Rgb(0x30, 0x30, 0x46)),
        "the border box is painted with NormalFloat"
    );
    // A cell inside the body must share that NormalFloat background.
    assert_eq!(
        bg(&buf, 5, 3),
        Some(Color::Rgb(0x30, 0x30, 0x46)),
        "the float body shares the border's NormalFloat background, not Normal"
    );
}

#[test]
fn cursorline_tints_the_cursor_row_with_the_themed_background() {
    // `'cursorline'` on, the cursor on the second screen row, and a `CursorLine`
    // chrome style (palette entry 0): the whole cursor row — including cells past
    // end-of-text — takes that background; other rows keep the plain background.
    let v = view(vec![
        ("lines", lines(&["alpha", "bravo"])),
        ("cursorline", Value::from(true)),
        ("cursor_row", Value::from(1u64)),
        (
            "styles",
            Value::Array(vec![style(vec![("bg", rgb(0x2a, 0x2a, 0x3a))])]),
        ),
        ("chrome", chrome(vec![("cursorline", 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(
        bg(&buf, 10, 1),
        Some(Color::Rgb(0x2a, 0x2a, 0x3a)),
        "the cursor row is tinted with CursorLine, past end-of-text too"
    );
    assert_ne!(
        bg(&buf, 10, 0),
        Some(Color::Rgb(0x2a, 0x2a, 0x3a)),
        "a non-cursor row is not tinted"
    );
}

#[test]
fn cursorline_off_leaves_the_cursor_row_untinted() {
    // Same payload but `'cursorline'` off: no row takes the CursorLine background.
    let v = view(vec![
        ("lines", lines(&["alpha", "bravo"])),
        ("cursorline", Value::from(false)),
        ("cursor_row", Value::from(1u64)),
        (
            "styles",
            Value::Array(vec![style(vec![("bg", rgb(0x2a, 0x2a, 0x3a))])]),
        ),
        ("chrome", chrome(vec![("cursorline", 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    assert_ne!(
        bg(&buf, 10, 1),
        Some(Color::Rgb(0x2a, 0x2a, 0x3a)),
        "with cursorline off the cursor row keeps the plain background"
    );
}

#[test]
fn cursorline_without_a_theme_falls_back_to_a_visible_tint() {
    // `'cursorline'` on but the colorscheme leaves `CursorLine` undefined (no
    // chrome entry): the client still tints the cursor row with its built-in
    // fallback (Indexed 236) so the line is visible out of the box.
    let v = view(vec![
        ("lines", lines(&["alpha", "bravo"])),
        ("cursorline", Value::from(true)),
        ("cursor_row", Value::from(1u64)),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(
        bg(&buf, 10, 1),
        Some(Color::Indexed(236)),
        "no CursorLine theme → the built-in fallback tint"
    );
}

#[test]
fn colorcolumn_tints_the_configured_columns_with_the_themed_background() {
    // `'colorcolumn'` rulers at text columns 3 and 6 (no gutter → screen cells 2 and
    // 5) and a `ColorColumn` chrome style (palette entry 0): those columns take the
    // tint down every text row — empty cells past end-of-text too — while neighbours
    // keep the plain background.
    let v = view(vec![
        ("lines", lines(&["alpha", "bravo"])),
        (
            "colorcolumn",
            Value::Array(vec![Value::from(3u64), Value::from(6u64)]),
        ),
        (
            "styles",
            Value::Array(vec![style(vec![("bg", rgb(0x33, 0x2a, 0x2a))])]),
        ),
        ("chrome", chrome(vec![("colorcolumn", 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    let tint = Some(Color::Rgb(0x33, 0x2a, 0x2a));
    for row in 0..3 {
        assert_eq!(
            bg(&buf, 2, row),
            tint,
            "column 3 (cell 2) tinted, row {row}"
        );
        assert_eq!(
            bg(&buf, 5, row),
            tint,
            "column 6 (cell 5) tinted, row {row}"
        );
        assert_ne!(
            bg(&buf, 3, row),
            tint,
            "the cell between the rulers is not tinted, row {row}"
        );
    }
}

#[test]
fn colorcolumn_without_a_theme_falls_back_to_a_visible_tint() {
    // `'colorcolumn'` set but the colorscheme leaves `ColorColumn` undefined (no
    // chrome entry): the ruler still paints with the built-in fallback (Indexed 236)
    // so it's visible out of the box.
    let v = view(vec![
        ("lines", lines(&["alpha"])),
        ("colorcolumn", Value::Array(vec![Value::from(2u64)])),
    ]);
    let buf = paint(&v, 20, 5);
    assert_eq!(
        bg(&buf, 1, 0),
        Some(Color::Indexed(236)),
        "no ColorColumn theme → the built-in fallback tint"
    );
}

#[test]
fn colorcolumn_offsets_by_the_gutter_and_horizontal_scroll() {
    // Number gutter width 4 and `leftcol` 1: column 6 lands at cell 4 + (6-1) - 1 = 8.
    // Column 1 is scrolled off the left (text column 0 < leftcol 1), so it paints
    // nothing — the ruler tracks the text under `nowrap` horizontal scroll.
    let v = view(vec![
        ("lines", lines(&["a really long line here"])),
        ("number", Value::from(true)),
        ("number_width", Value::from(4u64)),
        ("leftcol", Value::from(1u64)),
        (
            "colorcolumn",
            Value::Array(vec![Value::from(1u64), Value::from(6u64)]),
        ),
        (
            "styles",
            Value::Array(vec![style(vec![("bg", rgb(0x33, 0x2a, 0x2a))])]),
        ),
        ("chrome", chrome(vec![("colorcolumn", 0)])),
    ]);
    let buf = paint(&v, 20, 5);
    let tint = Some(Color::Rgb(0x33, 0x2a, 0x2a));
    assert_eq!(
        bg(&buf, 8, 0),
        tint,
        "column 6 lands at gutter(4) + (6-1) - leftcol(1) = 8"
    );
    assert_ne!(
        bg(&buf, 3, 0),
        tint,
        "the column scrolled off the left paints nothing"
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
fn a_search_match_shows_through_the_visual_selection() {
    // "hello" fully selected, with a search match on "el" [1,3). Where the two
    // overlap the cell must take the `Search` background, not the `Visual` one — the
    // selection paints under the match so `hlsearch` stays visible in the selection
    // (vim's behavior, and what the GUI already does). Palette: 0 = Visual bg,
    // 1 = Search bg.
    let visual_bg = Color::Rgb(0x45, 0x47, 0x5a);
    let search_bg = Color::Rgb(0x4d, 0x46, 0x36);
    let sel = Value::Array(vec![Value::Array(vec![
        Value::from(0u64),
        Value::from(5u64),
    ])]);
    let search = Value::Array(vec![Value::Array(vec![Value::Array(vec![
        Value::from(1u64),
        Value::from(3u64),
    ])])]);
    let v = view(vec![
        ("lines", lines(&["hello"])),
        ("selection", sel),
        ("search", search),
        (
            "styles",
            Value::Array(vec![
                style(vec![("bg", rgb(0x45, 0x47, 0x5a))]),
                style(vec![("bg", rgb(0x4d, 0x46, 0x36))]),
            ]),
        ),
        ("chrome", chrome(vec![("visual", 0), ("search", 1)])),
    ]);
    let buf = paint(&v, 20, 5);
    // Selected-but-unmatched cells wear Visual...
    assert_eq!(bg(&buf, 0, 0), Some(visual_bg), "col 0: selection only");
    assert_eq!(bg(&buf, 4, 0), Some(visual_bg), "col 4: selection only");
    // ...and the matched cells inside the selection wear Search, not Visual.
    assert_eq!(
        bg(&buf, 1, 0),
        Some(search_bg),
        "col 1: the search match shows through the selection"
    );
    assert_eq!(
        bg(&buf, 2, 0),
        Some(search_bg),
        "col 2: match over selection"
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

#[test]
fn a_zero_duration_scroll_gesture_does_not_arm_an_animation() {
    // R11: a scroll gesture with `duration_ms == 0` has no slide to play. Arming
    // a degenerate animation would later divide elapsed time by a zero duration
    // (NaN/inf progress → a one-frame glitch). `arm_animation` must skip it and
    // leave the static destination viewport the redraw already carries.
    let scroll = Value::Map(vec![
        (Value::from("from_row"), Value::from(0u64)),
        (Value::from("to_row"), Value::from(3u64)),
        (Value::from("from_cursor_row"), Value::from(0u64)),
        (Value::from("to_cursor_row"), Value::from(3u64)),
        (Value::from("duration_ms"), Value::from(0u64)),
        (
            Value::from("lines"),
            lines(&["l0", "l1", "l2", "l3", "l4", "l5"]),
        ),
    ]);
    // The main view carries the destination viewport (lines l3..).
    let params = redraw(vec![
        ("lines", lines(&["l3", "l4", "l5"])),
        ("scroll", scroll),
    ]);

    let mut client = ScrollHarness::new();
    client.on_redraw(&params);
    assert!(
        !client.animating(),
        "a zero-duration scroll must not arm an animation"
    );

    // The static destination is shown, and painting it can't produce NaN cells.
    let buf = client.paint(20, 5);
    assert_eq!(row_text(&buf, 0).trim_end(), "l3");
}

#[test]
fn the_visual_selection_grows_with_the_slide_instead_of_flashing_to_full_extent() {
    // A Ctrl-D in visual mode scrolls *and* extends the selection. The band carries
    // the selection over the maximal extent (anchor..far cursor), but the moving
    // edge must grow together with the slide: on frame 0 only the rows the cursor
    // has already reached are highlighted, not the full final extent. Anchor on
    // line 0 (so the selection extends down), cursor moving from line 1 to line 4.
    let band_sel = Value::Array(vec![
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l0 selected
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l1 selected
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l2 (ahead of cursor)
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l3 (ahead of cursor)
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l4 (ahead of cursor)
        Value::Nil,
        Value::Nil,
        Value::Nil,
    ]);
    let scroll = Value::Map(vec![
        (Value::from("from_row"), Value::from(0u64)),
        (Value::from("to_row"), Value::from(3u64)),
        (Value::from("from_cursor_row"), Value::from(1u64)),
        (Value::from("to_cursor_row"), Value::from(4u64)),
        (Value::from("duration_ms"), Value::from(10_000u64)), // long: paint at t≈0
        (Value::from("sel_extends_down"), Value::from(true)), // anchor above
        (
            Value::from("lines"),
            lines(&["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"]),
        ),
        (Value::from("selection"), band_sel),
    ]);
    let params = redraw(vec![
        ("lines", lines(&["l3", "l4", "l5"])),
        ("scroll", scroll),
    ]);

    let mut client = ScrollHarness::new();
    client.on_redraw(&params);
    assert!(client.animating(), "a non-zero scroll gesture must animate");

    // At t≈0 the interpolated cursor sits on line 1, so the selection reaches only
    // lines 0..=1 (screen rows 0 and 1). The destination-only rows 2..4 must stay
    // unhighlighted — they light up as the cursor sweeps past them, not before.
    let buf = client.paint(20, 10);
    assert!(
        reversed(&buf, 0, 0),
        "line 0 (at/above the cursor) is selected"
    );
    assert!(reversed(&buf, 0, 1), "line 1 (the cursor line) is selected");
    assert!(
        !reversed(&buf, 0, 2),
        "line 2 is past the interpolated cursor — not yet selected"
    );
    assert!(
        !reversed(&buf, 0, 3),
        "line 3 is past the interpolated cursor — not yet selected"
    );
}

#[test]
fn shrinking_the_visual_selection_tracks_the_cursor_instead_of_vanishing() {
    // Scrolling *back toward* the anchor (e.g. <C-u> after a <C-d>) shrinks the
    // selection. The band carries the maximal (source) extent and the orientation
    // flag — not the scroll direction — so the trailing rows the cursor sweeps back
    // across stay highlighted at t≈0 and peel off as the cursor reaches them. A
    // direction-derived clip got this backwards and hid them (a flash). Anchor on
    // line 0 (selection extends down); cursor recedes from line 5 to 1, viewport
    // top from 4 back to 0.
    let band_sel = Value::Array(vec![
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l0
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l1
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l2
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l3
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l4
        Value::Array(vec![Value::from(0u64), Value::from(2u64)]), // l5 (source edge)
        Value::Nil,
        Value::Nil,
    ]);
    let scroll = Value::Map(vec![
        (Value::from("from_row"), Value::from(4u64)),
        (Value::from("to_row"), Value::from(0u64)),
        (Value::from("from_cursor_row"), Value::from(5u64)),
        (Value::from("to_cursor_row"), Value::from(1u64)),
        (Value::from("duration_ms"), Value::from(10_000u64)), // long: paint at t≈0
        (Value::from("sel_extends_down"), Value::from(true)), // anchor above, still
        (
            Value::from("lines"),
            lines(&["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"]),
        ),
        (Value::from("selection"), band_sel),
    ]);
    let params = redraw(vec![
        ("lines", lines(&["l0", "l1", "l2"])),
        ("scroll", scroll),
    ]);

    let mut client = ScrollHarness::new();
    client.on_redraw(&params);
    assert!(client.animating(), "a non-zero scroll gesture must animate");

    // At t≈0 top is 4 (rows show lines l4, l5, l6, …) and the cursor is on line 5.
    // The selection still reaches line 5, so screen row 0 (l4) and row 1 (l5) are
    // highlighted — they peel off only as the cursor recedes past them. A
    // direction-derived clip would have hidden these (selection "vanishes" on <C-u>).
    let buf = client.paint(20, 10);
    assert!(
        reversed(&buf, 0, 0),
        "line 4 (still inside the selection) is highlighted"
    );
    assert!(
        reversed(&buf, 0, 1),
        "line 5 (the cursor line) is highlighted"
    );
    assert!(
        !reversed(&buf, 0, 2),
        "line 6 past the cursor is not selected"
    );
}

#[test]
fn search_matches_keep_highlighting_while_the_view_slides() {
    // Regression: a scroll over `hlsearch` matches must keep them lit on the moving
    // text, not blank them until the slide settles. The band carries per-row
    // `search` spans; the client paints them on the band rows (Search bg = yellow
    // with no colorscheme). Match at cols [0,2) on every band line.
    let band_search = Value::Array(
        (0..8)
            .map(|_| {
                Value::Array(vec![Value::Array(vec![
                    Value::from(0u64),
                    Value::from(2u64),
                ])])
            })
            .collect(),
    );
    let scroll = Value::Map(vec![
        (Value::from("from_row"), Value::from(0u64)),
        (Value::from("to_row"), Value::from(3u64)),
        (Value::from("from_cursor_row"), Value::from(0u64)),
        (Value::from("to_cursor_row"), Value::from(3u64)),
        (Value::from("duration_ms"), Value::from(10_000u64)), // long: paint at t≈0
        (
            Value::from("lines"),
            lines(&["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"]),
        ),
        (Value::from("search"), band_search),
    ]);
    let params = redraw(vec![
        ("lines", lines(&["l0", "l1", "l2"])),
        ("scroll", scroll),
    ]);

    let mut client = ScrollHarness::new();
    client.on_redraw(&params);
    assert!(client.animating(), "a non-zero scroll gesture must animate");

    // At t≈0 the band shows lines l0.. from the top; the matched cells (col 0) must
    // carry the Search background instead of vanishing for the slide's duration.
    let buf = client.paint(20, 10);
    assert_eq!(
        bg(&buf, 0, 0),
        Some(Color::Yellow),
        "the search match keeps its highlight on the sliding band"
    );
    assert_eq!(
        bg(&buf, 0, 1),
        Some(Color::Yellow),
        "every band row's match stays highlighted while sliding"
    );
}

#[test]
fn the_line_background_slides_with_the_band_instead_of_settling_early() {
    // Regression: a code block's `line_bg` tint must ride the scroll band, not stay
    // at the settled window position (TUI jumped the tint to its final row) or vanish
    // (GUI). The band carries its own `line_bg` (band-row indexed); the client must
    // paint *that* while sliding, ignoring the settled window's `line_bg`. Palette:
    // entry 0 = the code-block bg (rides the band), entry 1 = a decoy the settled
    // window carries but that must NOT show mid-slide.
    let styles = Value::Array(vec![
        style(vec![("bg", rgb(0x30, 0x32, 0x44))]), // 0: block bg
        style(vec![("bg", rgb(0xaa, 0x00, 0x00))]), // 1: settled-window decoy
    ]);
    // Band line background on band row 1 (→ screen row 1 at t≈0, off=0).
    let band_line_bg = Value::Array(vec![Value::Array(vec![
        Value::from(1u64),
        Value::from(0u64),
    ])]);
    let scroll = Value::Map(vec![
        (Value::from("from_row"), Value::from(0u64)),
        (Value::from("to_row"), Value::from(3u64)),
        (Value::from("from_cursor_row"), Value::from(0u64)),
        (Value::from("to_cursor_row"), Value::from(3u64)),
        (Value::from("duration_ms"), Value::from(10_000u64)), // long: paint at t≈0
        (
            Value::from("lines"),
            lines(&["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"]),
        ),
        (Value::from("line_bg"), band_line_bg),
    ]);
    // The settled window carries a *different* line_bg (row 2, decoy palette 1). It
    // must not be painted while the slide is in flight.
    let win_line_bg = Value::Array(vec![Value::Array(vec![
        Value::from(2u64),
        Value::from(1u64),
    ])]);
    let params = redraw(vec![
        ("lines", lines(&["l3", "l4", "l5"])),
        ("styles", styles),
        ("line_bg", win_line_bg),
        ("scroll", scroll),
    ]);

    let mut client = ScrollHarness::new();
    client.on_redraw(&params);
    assert!(client.animating(), "a non-zero scroll gesture must animate");

    let buf = client.paint(20, 10);
    // The band's tint paints on its sliding row (full window width, so any column
    // carries it) — without the fix the band `line_bg` is dropped and this is `None`.
    assert_eq!(
        bg(&buf, 5, 1),
        Some(Color::Rgb(0x30, 0x32, 0x44)),
        "the code-block background rides the sliding band at row 1"
    );
    // The settled window's line_bg must NOT show mid-slide (it would be the jump /
    // wrong-position bug).
    assert_ne!(
        bg(&buf, 5, 2),
        Some(Color::Rgb(0xaa, 0x00, 0x00)),
        "the settled window line_bg must not paint while the band slides"
    );
}

/// Build a `tabline` array value from `(label, modified, window_count)` triples.
fn tabline(tabs: &[(&str, bool, u64)]) -> Value {
    Value::Array(
        tabs.iter()
            .map(|(label, modified, count)| {
                Value::Map(vec![
                    (Value::from("label"), Value::from(*label)),
                    (Value::from("modified"), Value::from(*modified)),
                    (Value::from("window_count"), Value::from(*count)),
                ])
            })
            .collect(),
    )
}

#[test]
fn tabline_paints_a_top_row_and_pushes_the_window_down() {
    // Two tabs ⇒ a tabline on row 0 (the active cell reverse-video), and the
    // window's text starts on row 1 — the windows area shrank by the tabline row.
    let buf = paint(
        &view(vec![
            ("lines", lines(&["hello"])),
            (
                "tabline",
                tabline(&[("a.txt", false, 1), ("b.txt", true, 1)]),
            ),
            ("current_tab", Value::from(1u64)),
        ]),
        40,
        6,
    );

    let top = row_text(&buf, 0);
    assert!(top.contains("a.txt"), "tab 1 label on the tabline: {top:?}");
    assert!(top.contains("b.txt"), "tab 2 label on the tabline: {top:?}");
    assert!(
        top.contains('+'),
        "the modified tab shows a + flag: {top:?}"
    );

    // The window text was pushed below the tabline row.
    assert_eq!(row_text(&buf, 1).trim_end(), "hello");

    // The active (second) cell is reverse-video; the first is not. Cell 0 is the
    // leading space of tab 1 (normal); the `b` of "b.txt" sits past tab 1's text.
    let b_col = top.find("b.txt").unwrap() as u16;
    assert!(
        reversed(&buf, b_col, 0),
        "the active tab cell is highlighted"
    );
    assert!(!reversed(&buf, 1, 0), "the inactive tab cell is not");
}

#[test]
fn tabline_falls_back_to_the_status_line_colors() {
    // A colorscheme that themes `StatusLine` but leaves `TabLine`/`TabLineSel`/
    // `TabLineFill` undefined must still get a themed tabline: the bar takes the
    // status-line colors (the active cell their reverse) rather than dropping to
    // the terminal default with a reverse-video cell, which reads as unthemed
    // against the rest of the frame. This is the GUI's fallback, so both clients
    // paint the same bar.
    let buf = paint(
        &view(vec![
            ("lines", lines(&["hello"])),
            (
                "styles",
                Value::Array(vec![style(vec![
                    ("fg", rgb(0xab, 0xb2, 0xbf)),
                    ("bg", rgb(0x21, 0x25, 0x2b)),
                ])]),
            ),
            ("chrome", chrome(vec![("status_line", 0)])),
            (
                "tabline",
                tabline(&[("a.txt", false, 1), ("b.txt", false, 1)]),
            ),
            ("current_tab", Value::from(1u64)),
        ]),
        40,
        6,
    );

    let top = row_text(&buf, 0);
    let b_col = top.find("b.txt").unwrap() as u16;
    // The inactive cell and the fill past the last cell take the StatusLine look.
    assert_eq!(
        (fg(&buf, 1, 0), bg(&buf, 1, 0)),
        (
            Some(Color::Rgb(0xab, 0xb2, 0xbf)),
            Some(Color::Rgb(0x21, 0x25, 0x2b))
        ),
        "the inactive tab cell falls back to StatusLine: {top:?}"
    );
    assert_eq!(
        bg(&buf, 39, 0),
        Some(Color::Rgb(0x21, 0x25, 0x2b)),
        "the strip past the last cell falls back to StatusLine"
    );
    // The active cell is the reverse of that, painted as real colors (not a bare
    // REVERSED modifier over the terminal default).
    assert_eq!(
        (fg(&buf, b_col, 0), bg(&buf, b_col, 0)),
        (
            Some(Color::Rgb(0x21, 0x25, 0x2b)),
            Some(Color::Rgb(0xab, 0xb2, 0xbf))
        ),
        "the active tab cell is the StatusLine colors reversed"
    );
}

#[test]
fn no_tabline_with_a_single_tab() {
    // One (or zero) tab ⇒ no tabline; the window keeps row 0, unchanged from the
    // pre-tabs frame.
    let buf = paint(&view(vec![("lines", lines(&["hello"]))]), 40, 6);
    assert_eq!(row_text(&buf, 0).trim_end(), "hello");
}

/// Build a `region_tablines` map value with one region's `{ tabs, current, title }`.
fn region_tablines(region: &str, tabs: &[(&str, bool, u64)], current: u64, title: &str) -> Value {
    Value::Map(vec![(
        Value::from(region),
        Value::Map(vec![
            (Value::from("tabs"), tabline(tabs)),
            (Value::from("current"), Value::from(current)),
            (Value::from("title"), Value::from(title)),
        ]),
    )])
}

#[test]
fn a_top_dock_paints_its_own_tabline_above_its_content() {
    // 20×8 grid. A top dock of height 3 with two tabs: its own tabline on row 0,
    // its window content on rows 1..3, the dock border on row 3, main below.
    let windows = Value::Array(vec![
        // The dock tree lost its top row to the dock tabline (core: height 3-1=2).
        region_window(rect(0, 0, 20, 2), "dock_top", false, &["DOCKTEXT"]),
        region_window(rect(0, 0, 20, 4), "main", true, &["MAIN"]),
    ]);
    let v = view(vec![
        ("windows", windows),
        ("dock_top", Value::from(3u64)),
        (
            "region_tablines",
            region_tablines("top", &[("a.txt", false, 1), ("b.txt", true, 1)], 1, ""),
        ),
    ]);
    let buf = paint(&v, 20, 8);

    // Row 0 is the dock's own tabline (both tab labels, a `+` on the modified one).
    let tl = row_text(&buf, 0);
    assert!(
        tl.contains("a.txt") && tl.contains("b.txt"),
        "dock tabline: {tl:?}"
    );
    assert!(tl.contains('+'), "modified dock tab shows a +: {tl:?}");
    // The active (second) cell is reverse-video.
    let b_col = tl.find("b.txt").unwrap() as u16;
    assert!(reversed(&buf, b_col, 0), "active dock tab highlighted");
    // The dock's window content sits below its tabline (row 1), and the dock border
    // is on row 3 — the window was pushed down a row by its tabline.
    assert!(
        row_text(&buf, 1).starts_with("DOCKTEXT"),
        "dock content on row 1"
    );
    assert_eq!(row_text(&buf, 3), "━".repeat(20), "dock border on row 3");
    assert!(row_text(&buf, 4).starts_with("MAIN"), "main below the dock");
}

#[test]
fn a_single_tab_dock_draws_no_tabline() {
    // A dock with one tab (empty region_tablines entry) reserves no tabline row:
    // its content keeps row 0 of the band, exactly as before per-region tablines.
    let windows = Value::Array(vec![
        region_window(rect(0, 0, 20, 3), "dock_top", false, &["DOCKTEXT"]),
        region_window(rect(0, 0, 20, 4), "main", true, &["MAIN"]),
    ]);
    let v = view(vec![("windows", windows), ("dock_top", Value::from(3u64))]);
    let buf = paint(&v, 20, 8);
    assert!(
        row_text(&buf, 0).starts_with("DOCKTEXT"),
        "no dock tabline: content keeps row 0"
    );
}

#[test]
fn a_dock_title_paints_at_the_start_of_its_tabline_strip() {
    // A titled dock shows its strip even with one tab: the title leads the row,
    // then the tab cell, and the window content sits below.
    let windows = Value::Array(vec![
        region_window(rect(0, 0, 20, 2), "dock_top", false, &["DOCKTEXT"]),
        region_window(rect(0, 0, 20, 4), "main", true, &["MAIN"]),
    ]);
    let v = view(vec![
        ("windows", windows),
        ("dock_top", Value::from(3u64)),
        (
            "region_tablines",
            region_tablines("top", &[("a.txt", false, 1)], 0, "EXPLORER"),
        ),
    ]);
    let buf = paint(&v, 20, 8);
    let strip = row_text(&buf, 0);
    assert!(
        strip.contains("EXPLORER"),
        "the dock title leads the strip: {strip:?}"
    );
    assert!(
        strip.contains("a.txt"),
        "the tab cell follows the title: {strip:?}"
    );
    // The title sits before the tab cell.
    assert!(
        strip.find("EXPLORER").unwrap() < strip.find("a.txt").unwrap(),
        "title precedes the cells: {strip:?}"
    );
    assert!(
        row_text(&buf, 1).starts_with("DOCKTEXT"),
        "content below the strip"
    );
}

/// A `menu` redraw submap (the picker / `nx.ui.select` float-list). `items` are the
/// row labels; `spans` are the per-row matched-char ranges (parallel to `items`).
fn menu_value(items: &[&str], spans: &[&[(u16, u16)]], width: u64, query: Option<&str>) -> Value {
    let items_v = Value::Array(items.iter().map(|s| Value::from(*s)).collect());
    let spans_v = Value::Array(
        spans
            .iter()
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|&(s, e)| Value::Array(vec![Value::from(s), Value::from(e)]))
                        .collect(),
                )
            })
            .collect(),
    );
    let mut m = vec![
        (Value::from("items"), items_v),
        (Value::from("match_spans"), spans_v),
        (Value::from("selected"), Value::from(0u64)),
        (Value::from("row"), Value::from(0u64)),
        (Value::from("col"), Value::from(0u64)),
        (Value::from("width"), Value::from(width)),
        (Value::from("height"), Value::from(6u64)),
    ];
    if let Some(q) = query {
        m.push((Value::from("query"), Value::from(q)));
    }
    Value::Map(m)
}

/// The cell symbols spanning columns `x..x + n` of buffer row `y`, as a string.
fn run_text(buf: &Buffer, x: u16, y: u16, n: u16) -> String {
    (x..x + n)
        .map(|xx| buf.cell((xx, y)).map(|c| c.symbol()).unwrap_or(""))
        .collect()
}

#[test]
fn a_picker_row_keeps_the_file_name_when_the_path_overflows() {
    // A long path in a narrow picker box must not lose its file name: the leading
    // directories collapse behind a `…`, and the file name (the path tail) stays
    // whole — that's the thing you scan the list for.
    let long = "crates/nxvim-core/src/editor/operators.rs"; // 41 chars
    let menu = menu_value(
        &[long, "main.rs"],
        // Highlight "operators" (chars 29..38) on row 0 — it must follow the elision.
        &[&[(29, 38)], &[]],
        22,       // a list width the 41-char path can't fit
        Some(""), // a picker (carries a prompt)
    );
    let v = view(vec![("lines", lines(&["x"])), ("menu", menu)]);
    let buf = paint(&v, 60, 12);

    let rows: Vec<String> = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
    let shown = rows.join("\n");
    // The file name survives, behind a `…` that swallowed the leading directories.
    assert!(
        rows.iter().any(|r| r.contains("…editor/operators.rs")),
        "file name kept after eliding the head:\n{shown}"
    );
    // The head is gone — the leading directories never render (no tail truncation,
    // which would have cut the file name instead).
    assert!(
        !shown.contains("crates/nxvim-core"),
        "leading directories elided, not shown:\n{shown}"
    );
    // A short path that fits is shown verbatim, with no ellipsis.
    assert!(
        rows.iter()
            .any(|r| r.contains("main.rs") && !r.contains('…')),
        "a fitting path renders whole:\n{shown}"
    );

    // The match highlight tracks the elision: the underlined run still spells
    // "operators", proving the spans were remapped onto the truncated string.
    let mut hit = None;
    'scan: for y in 0..buf.area.height {
        for x in 0..buf.area.width.saturating_sub(9) {
            if run_text(&buf, x, y, 9) == "operators" {
                hit = Some((x, y));
                break 'scan;
            }
        }
    }
    let (sx, sy) = hit.expect("the elided path row carries an `operators` run");
    for x in sx..sx + 9 {
        assert!(
            underlined(&buf, x, sy),
            "matched char underlined at col {x}"
        );
    }
}

#[test]
fn picker_prompt_caret_lands_past_a_wide_query() {
    // The picker prompt caret follows `query_cursor` — a *char* offset (the
    // server's `cursor_chars()`) — but the caret column is display cells: a CJK
    // query ("日本": 2 chars, 4 cells) must push the caret its painted width.
    let menu = Value::Map(vec![
        (Value::from("items"), Value::Array(vec![Value::from("one")])),
        (Value::from("selected"), Value::from(0u64)),
        (Value::from("row"), Value::from(0u64)),
        (Value::from("col"), Value::from(0u64)),
        (Value::from("width"), Value::from(20u64)),
        (Value::from("height"), Value::from(6u64)),
        (Value::from("query"), Value::from("日本")),
        (Value::from("query_cursor"), Value::from(2u64)),
    ]);
    let v = view(vec![("lines", lines(&["x"])), ("menu", menu)]);
    let (buf, cursor) = paint_with_cursor(&v, 40, 12);
    // The prompt row (inside the fully-bordered box) paints `> 日本`: `>` at the
    // inner column, then the wide glyphs at two cells each.
    let (qy, qx) = find_text(&buf, "日").expect("query rendered in the prompt row");
    let qx = qx as u16;
    assert_eq!(buf.cell((qx + 2, qy)).unwrap().symbol(), "本");
    // Caret: "> " (2 cells) + 日本 (4 cells) past the inner column — 4 cells after
    // 日's first cell — not 2 (the char count).
    assert_eq!(
        cursor,
        Some((qx + 4, qy)),
        "picker caret measured in display cells, not chars"
    );
}

/// Every cell symbol painted into `buf`, concatenated. Used to assert the render
/// never emits a raw terminal control/escape byte the server (or file content the
/// server surfaced) supplied.
fn all_symbols(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(c) = buf.cell((x, y)) {
                out.push_str(c.symbol());
            }
        }
    }
    out
}

#[test]
fn server_supplied_escape_sequences_never_reach_the_terminal() {
    // SECURITY: a malicious server — or untrusted file content the server surfaces
    // (a buffer line, a message, a status segment) — must not be able to inject raw
    // ANSI/OSC escape sequences into the user's terminal. An OSC 52 would hijack the
    // clipboard, an OSC 0 would rewrite the title bar, a CSI could move the cursor or
    // clear the screen. The client renders all server text through ratatui spans,
    // which strip control characters (the ESC introducer included), so the bytes are
    // neutralised to inert printable residue. This pins that guarantee against a
    // future regression (a ratatui bump, or someone adding a raw write path).
    let payload = "x\u{1b}]52;c;cHduZWQ=\u{7}\u{1b}[2J\u{1b}]0;pwned\u{7}\u{1b}[31my";
    let v = view(vec![
        ("lines", lines(&[payload])),
        ("message", Value::from(payload)),
        ("status", status(&[(payload, None)])),
    ]);
    let buf = paint(&v, 40, 6);
    let painted = all_symbols(&buf);
    // The ESC introducer and the BEL terminator — the only bytes that make the rest
    // of the payload an executable escape sequence — must be gone everywhere.
    assert!(
        !painted.chars().any(|c| c.is_control()),
        "no control character may reach the terminal; got {painted:?}"
    );
    assert!(
        !painted.contains('\u{1b}'),
        "the ESC introducer must be stripped from every rendered cell"
    );
    // The inert printable residue still renders (the text passes through; only the
    // control bytes are removed), so this is sanitisation, not wholesale dropping.
    assert!(
        row_text(&buf, 0).contains("52;c;cHduZWQ="),
        "the printable residue of the buffer line still renders"
    );
}

#[test]
fn revealed_filter_boxes_paint_two_rows_and_hold_the_caret() {
    // The include/exclude boxes render as two labelled rows between the prompt and
    // the separator, and the caret follows the FOCUSED line — so typing lands where
    // the user is looking. `query_cursor` is that line's column, not the query's.
    let menu = Value::Map(vec![
        (Value::from("items"), Value::Array(vec![Value::from("one")])),
        (Value::from("selected"), Value::from(0u64)),
        (Value::from("row"), Value::from(0u64)),
        (Value::from("col"), Value::from(0u64)),
        (Value::from("width"), Value::from(24u64)),
        (Value::from("height"), Value::from(8u64)),
        (Value::from("query"), Value::from("hand")),
        (Value::from("query_cursor"), Value::from(6u64)),
        (
            Value::from("filters"),
            Value::Map(vec![
                (Value::from("include"), Value::from("src/**")),
                (Value::from("exclude"), Value::from("target")),
                (Value::from("focus"), Value::from("include")),
                (Value::from("expanded"), Value::from(true)),
            ]),
        ),
    ]);
    let v = view(vec![("lines", lines(&["x"])), ("menu", menu)]);
    let (buf, cursor) = paint_with_cursor(&v, 40, 14);

    let (iy, ix) = find_text(&buf, "include").expect("the include row is painted");
    let (ey, _) = find_text(&buf, "exclude").expect("the exclude row is painted");
    assert_eq!(ey, iy + 1, "the two rows are adjacent, under the prompt");
    let (qy, _) = find_text(&buf, "hand").expect("the query row is painted");
    assert_eq!(iy, qy + 1, "and follow the prompt line");
    // Each row shows its own raw text at the shared label column.
    assert_eq!(
        run_text(&buf, ix as u16 + 8, iy, 6),
        "src/**",
        "the include box's text starts past its label"
    );
    assert_eq!(run_text(&buf, ix as u16 + 8, ey, 6), "target");
    // The caret sits on the focused (include) row, at label width + 6 chars.
    assert_eq!(
        cursor,
        Some((ix as u16 + 8 + 6, iy)),
        "the caret belongs to the focused line, not the query"
    );
}

#[test]
fn a_collapsed_filter_box_shows_its_badge_on_the_prompt_row() {
    // Collapsed, the picker keeps its single-line shape — but an active filter must
    // still be visible, or a search that is hiding files looks like one that isn't.
    let menu = Value::Map(vec![
        (Value::from("items"), Value::Array(vec![Value::from("one")])),
        (Value::from("selected"), Value::from(0u64)),
        (Value::from("row"), Value::from(0u64)),
        (Value::from("col"), Value::from(0u64)),
        (Value::from("width"), Value::from(24u64)),
        (Value::from("height"), Value::from(8u64)),
        (Value::from("query"), Value::from("hand")),
        (Value::from("query_cursor"), Value::from(4u64)),
        (
            Value::from("filters"),
            Value::Map(vec![
                (Value::from("include"), Value::from("src/**")),
                (Value::from("exclude"), Value::from("target")),
                (Value::from("focus"), Value::from("query")),
                (Value::from("expanded"), Value::from(false)),
                (Value::from("badge"), Value::from("[+1 -1]")),
            ]),
        ),
    ]);
    let v = view(vec![("lines", lines(&["x"])), ("menu", menu)]);
    let (buf, _cursor) = paint_with_cursor(&v, 40, 14);

    let (qy, _) = find_text(&buf, "hand").expect("the prompt row is painted");
    let (by, _) = find_text(&buf, "[+1 -1]").expect("the badge is painted");
    assert_eq!(by, qy, "the badge rides the prompt row");
    assert!(
        find_text(&buf, "include").is_none(),
        "collapsed, the rows themselves stay hidden"
    );
}

#[test]
fn the_unfocused_window_status_bar_takes_status_line_nc() {
    // vim paints the focused window's status bar with `StatusLine` and every other
    // one with `StatusLineNC`, which is the only cue telling you which split has
    // focus. Both bars used to take `StatusLine`, so a split looked uniform.
    let windows = Value::Array(vec![
        window(rect(0, 0, 20, 4), false, "top.txt", &["top text"]),
        window(rect(0, 5, 20, 4), true, "bot.txt", &["bottom text"]),
    ]);
    let buf = paint(
        &view(vec![
            ("windows", windows),
            (
                "styles",
                Value::Array(vec![
                    style(vec![
                        ("fg", rgb(0xcd, 0xd6, 0xf4)),
                        ("bg", rgb(0x18, 0x18, 0x25)),
                    ]),
                    style(vec![
                        ("fg", rgb(0x45, 0x47, 0x5a)),
                        ("bg", rgb(0x11, 0x11, 0x1b)),
                    ]),
                ]),
            ),
            (
                "chrome",
                chrome(vec![("status_line", 0), ("status_line_nc", 1)]),
            ),
        ]),
        20,
        10,
    );

    // Row 3 is the unfocused (top) window's bar, row 8 the focused (bottom) one's.
    assert_eq!(
        (fg(&buf, 0, 3), bg(&buf, 0, 3)),
        (
            Some(Color::Rgb(0x45, 0x47, 0x5a)),
            Some(Color::Rgb(0x11, 0x11, 0x1b))
        ),
        "the unfocused bar takes StatusLineNC: {:?}",
        row_text(&buf, 3)
    );
    assert_eq!(
        (fg(&buf, 0, 8), bg(&buf, 0, 8)),
        (
            Some(Color::Rgb(0xcd, 0xd6, 0xf4)),
            Some(Color::Rgb(0x18, 0x18, 0x25))
        ),
        "the focused bar keeps StatusLine: {:?}",
        row_text(&buf, 8)
    );
}

#[test]
fn an_undefined_status_line_nc_falls_back_to_status_line() {
    // A colorscheme that themes only `StatusLine` must keep both bars themed —
    // dropping the unfocused one to reverse-video would look broken, not subtle.
    let windows = Value::Array(vec![
        window(rect(0, 0, 20, 4), false, "top.txt", &["top text"]),
        window(rect(0, 5, 20, 4), true, "bot.txt", &["bottom text"]),
    ]);
    let buf = paint(
        &view(vec![
            ("windows", windows),
            (
                "styles",
                Value::Array(vec![style(vec![
                    ("fg", rgb(0xcd, 0xd6, 0xf4)),
                    ("bg", rgb(0x18, 0x18, 0x25)),
                ])]),
            ),
            ("chrome", chrome(vec![("status_line", 0)])),
        ]),
        20,
        10,
    );

    for y in [3u16, 8] {
        assert_eq!(
            (fg(&buf, 0, y), bg(&buf, 0, y)),
            (
                Some(Color::Rgb(0xcd, 0xd6, 0xf4)),
                Some(Color::Rgb(0x18, 0x18, 0x25))
            ),
            "row {y} falls back to StatusLine when StatusLineNC is undefined"
        );
    }
}
