//! Tier 2: the full in-process stack — real server -> real `View` -> real
//! client paint — asserted on the painted cell grid. Deterministic: the
//! `barrier`/`lines` request guarantees all prior input was processed and its
//! redraw emitted before we read the screen. No sleeps.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, start_attached};
use nxvim_tui::{paint, paint_with_cursor};
use nxvim_view::View;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24; // windows area is ROWS - 1 cmd row = 23; text is 22 (status inside)
/// Default number-column width for a small buffer: nxvim ships with the hybrid
/// number column on, sized to 4 cells (vim's `numberwidth` minimum). Text,
/// selection, and cursor columns are all offset by this much.
const GUTTER: u16 = 4;

/// Start a server and attach with a windows-area height matching the paint grid
/// (ROWS minus the one global command row — the client now draws each window's
/// status line inside its rect), so the captured `View` fills the grid exactly.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(
        ServerInit {
            file,
            ..Default::default()
        },
        COLS,
        ROWS - 1,
    )
    .await
}

/// Barrier: awaiting this guarantees the server processed all prior input.
async fn barrier(rpc: &Rpc) {
    rpc.request(
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    .expect("barrier");
}

/// The most recent `redraw` params buffered on the connection. Yields once
/// first so the RPC reader task can dispatch any frame it has already read off
/// the duplex but not yet pushed into the channel — making the capture robust
/// regardless of how the duplex chunked the redraw and the barrier response.
async fn latest_redraw(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<Value>> {
    tokio::task::yield_now().await;
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = Some(params);
        }
    }
    latest
}

/// Drive input, then capture and paint the resulting real view.
async fn screen(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Buffer {
    barrier(rpc).await;
    let params = latest_redraw(incoming).await.expect("a redraw");
    paint(&View::from_redraw(&params), COLS, ROWS)
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

/// The background color of a painted cell — used to spot the search-match
/// highlight (the built-in yellow fallback with no colorscheme loaded).
fn bg(buf: &Buffer, x: u16, y: u16) -> Option<Color> {
    buf.cell((x, y)).and_then(|c| c.style().bg)
}

/// The foreground color of a painted cell — used to spot a `%#Group#` status
/// segment painting its resolved truecolor foreground.
fn fg(buf: &Buffer, x: u16, y: u16) -> Option<Color> {
    buf.cell((x, y)).and_then(|c| c.style().fg)
}

#[tokio::test]
async fn typed_text_is_painted_with_the_mode_in_the_status_line() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let buf = screen(&rpc, &mut incoming).await;
    // Row 0 is the cursor line: hybrid gutter shows its absolute number `1`
    // (left-aligned in the 4-cell column) followed by the text.
    assert_eq!(row_text(&buf, 0).trim_end(), "1   hello");
    // Status line is row ROWS - 2 (index 22 of rows 0..24), just above cmdline.
    assert!(
        row_text(&buf, ROWS - 2).contains("NORMAL"),
        "status: {:?}",
        row_text(&buf, ROWS - 2)
    );
}

#[tokio::test]
async fn a_custom_statusline_is_painted_end_to_end() {
    // A custom `'statusline'` runs the server's %-format engine, and the client
    // paints the projected segments verbatim. A field format (`%l,%c`) expands
    // from window state; the literal text around it shows as-is.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>gg"); // line 1, col 1
    feed(&rpc, r":set statusline=L%lC%c<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    assert_eq!(
        row_text(&buf, ROWS - 2).trim_end(),
        "L1C1",
        "status: {:?}",
        row_text(&buf, ROWS - 2)
    );
}

#[tokio::test]
async fn a_statusline_highlight_group_paints_its_color_end_to_end() {
    // `%#Group#` switches the highlight mid-statusline; the server resolves the
    // group to a palette style and the client paints that segment's truecolor
    // foreground (patched onto the reverse-video base, which has no colorscheme).
    let (rpc, mut incoming) = start(None).await;
    exec_lua(&rpc, "vim.api.nvim_set_hl(0, 'MyStl', { fg = '#ff0000' })").await;
    feed(&rpc, r":set statusline=ab%#MyStl#X<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    let y = ROWS - 2;
    assert_eq!(row_text(&buf, y).trim_end(), "abX");
    // The `ab` prefix has no group of its own (the base look)...
    assert_ne!(
        fg(&buf, 0, y),
        Some(Color::Rgb(255, 0, 0)),
        "prefix is base"
    );
    // ...while the `%#MyStl#X` run paints the group's red foreground.
    assert_eq!(fg(&buf, 2, y), Some(Color::Rgb(255, 0, 0)), "X is red");
    assert!(
        reversed(&buf, 2, y),
        "the styled run keeps the base REVERSED"
    );
}

#[tokio::test]
async fn laststatus_three_paints_a_single_global_status_bar() {
    // `laststatus=3` drops the per-window status row and paints one global bar on
    // the row just above the command line (ROWS - 2). The windows area shrinks by
    // one, so the text rows above keep painting normally.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>gg");
    feed(&rpc, r":set laststatus=3<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    // The buffer text still paints at the top.
    assert_eq!(row_text(&buf, 0).trim_end(), "1   hello");
    // The global bar (default look) sits on ROWS - 2, reverse-video like a status.
    let y = ROWS - 2;
    let bar = row_text(&buf, y);
    assert!(bar.contains("NORMAL"), "global bar shows the mode: {bar:?}");
    assert!(
        bar.contains("[No Name]"),
        "global bar shows the file: {bar:?}"
    );
    assert!(reversed(&buf, 0, y), "the global bar uses the status look");
}

#[tokio::test]
async fn laststatus_zero_paints_no_status_row() {
    // `laststatus=0` hides the status line; the freed bottom row of the windows
    // area becomes text (the `~` end-of-buffer filler), with no NORMAL/[No Name]
    // bar anywhere above the command line.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, r":set laststatus=0<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    // The row that held the status line (ROWS - 2) is now an empty `~` filler row,
    // not a reverse-video status bar.
    let y = ROWS - 2;
    assert!(
        !reversed(&buf, 0, y),
        "no status bar where the status used to be"
    );
    assert!(
        !row_text(&buf, y).contains("NORMAL"),
        "no mode bar: {:?}",
        row_text(&buf, y)
    );
}

#[tokio::test]
async fn hybrid_number_column_shows_absolute_then_relative() {
    let (rpc, mut incoming) = start(None).await;
    // Three lines; leave the cursor on the middle one.
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>kk"); // back up to line 1, then...
    feed(&rpc, "j"); // ...land on line 2 (the middle).
    let buf = screen(&rpc, &mut incoming).await;
    // Cursor line (row 1) shows its absolute number left-aligned; the lines
    // above and below show distance 1, right-aligned with a trailing space.
    assert_eq!(row_text(&buf, 0).trim_end(), "  1 one");
    assert_eq!(row_text(&buf, 1).trim_end(), "2   two");
    assert_eq!(row_text(&buf, 2).trim_end(), "  1 three");
}

#[tokio::test]
async fn a_visual_selection_is_highlighted_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0vll"); // select h,e,l -> screen cols [0,3)
    let buf = screen(&rpc, &mut incoming).await;
    // Selection columns are measured from the text, which starts past the gutter.
    assert!(
        !reversed(&buf, 0, 0),
        "the number gutter is never highlighted"
    );
    assert!(reversed(&buf, GUTTER, 0));
    assert!(reversed(&buf, GUTTER + 1, 0));
    assert!(reversed(&buf, GUTTER + 2, 0));
    assert!(!reversed(&buf, GUTTER + 3, 0));
}

#[tokio::test]
async fn search_matches_are_highlighted_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo and foo<Esc>");
    feed(&rpc, "/foo<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    // Both "foo" runs light up with the Search highlight (the built-in yellow
    // with no colorscheme loaded); the gap between them does not. Columns are
    // measured from the text, which starts past the number gutter.
    assert_eq!(
        bg(&buf, GUTTER, 0),
        Some(Color::Yellow),
        "first match start"
    );
    assert_eq!(
        bg(&buf, GUTTER + 2, 0),
        Some(Color::Yellow),
        "first match end"
    );
    assert_ne!(
        bg(&buf, GUTTER + 4, 0),
        Some(Color::Yellow),
        "the gap is not highlighted"
    );
    assert_eq!(bg(&buf, GUTTER + 8, 0), Some(Color::Yellow), "second match");
    // `:noh` clears the highlight until the next search.
    feed(&rpc, ":noh<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    assert_ne!(
        bg(&buf, GUTTER, 0),
        Some(Color::Yellow),
        ":noh clears the match highlight"
    );
}

#[tokio::test]
async fn wide_chars_align_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>");
    let buf = screen(&rpc, &mut incoming).await;
    // Wide glyphs start past the number gutter, each still occupying two cells.
    assert_eq!(buf.cell((GUTTER, 0)).unwrap().symbol(), "日");
    assert_eq!(buf.cell((GUTTER + 2, 0)).unwrap().symbol(), "本");
}

#[tokio::test]
async fn long_line_scrolls_horizontally_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    // A line wider than the 80-column window: a unique 'Z' at the far left, a
    // unique 'Q' at the far right, filler between.
    let line = format!("Z{}Q", "abcdefgh".repeat(12)); // 1 + 96 + 1 = 98 columns
    feed(&rpc, "i");
    feed(&rpc, &line);
    feed(&rpc, "<Esc>$"); // cursor on the trailing 'Q', off the right edge
    let buf = screen(&rpc, &mut incoming).await;

    let row = row_text(&buf, 0);
    // The viewport scrolled right to keep the cursor visible: the leading 'Z' is
    // no longer painted, while the trailing 'Q' (under the cursor) is. (`leftcol`
    // is sticky, vim-style — it was set while typing past the edge and isn't
    // re-minimized when the cursor stays on screen — so 'Q' need not sit on the
    // very last cell.)
    assert!(!row.contains('Z'), "leading column scrolled off: {row:?}");
    assert!(
        row.contains('Q'),
        "trailing cursor char stays visible: {row:?}"
    );
    // The number gutter is untouched by horizontal scroll: the cursor line still
    // shows its absolute line number at the far left.
    assert!(row.starts_with("1 "), "gutter intact: {row:?}");
}

#[tokio::test]
async fn editor_keeps_processing_when_the_ui_never_drains_redraws() {
    // Never drain `incoming` — the client-side application never consumes the
    // delivered redraws. The server's writer runs as its own task on an
    // unbounded queue, so the editor keeps processing input and never blocks on
    // UI acknowledgment. (This guards against an accidental synchronous wait on
    // the UI; it does NOT exercise socket back-pressure — the reader task still
    // drains the duplex into the unbounded channel continuously.)
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i");
    for _ in 0..200 {
        feed(&rpc, "x");
    }
    feed(&rpc, "<Esc>");
    // Read only the response to this request; never drain the redraws.
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    let line = match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    assert_eq!(line, vec!["x".repeat(200)]);
}

#[tokio::test]
async fn messages_command_renders_a_panel_at_the_bottom() {
    let (rpc, mut incoming) = start(None).await;
    // Build one history line, then open the messages panel.
    feed(&rpc, ":lua print('hello panel')<CR>");
    feed(&rpc, ":messages<CR>");
    let buf = screen(&rpc, &mut incoming).await;

    // Panel content height is 10, so it claims 11 rows (title + content). The
    // panel sits below the status line, above the single command row, so its
    // title bar lands at row ROWS-1-11 = 12.
    let bar = row_text(&buf, ROWS - 12);
    assert!(bar.contains("Messages"), "title bar: {bar:?}");
    assert!(bar.contains("[X]"), "close button: {bar:?}");
    // The first content row (just below the bar) shows the history line.
    assert_eq!(row_text(&buf, ROWS - 11).trim_end(), "hello panel");
}

// ----- floating windows (phase 2: painting the overlay) ---------------------
//
// Phase 1 made a float a real, queryable window over RPC; Phase 2 paints it on
// top of the tiled layout. These assert on the rendered cell grid: the float's
// content lands at its rect, it is opaque over the buffer beneath (`Clear`), its
// border/title draw, higher `zindex` wins an overlap, and a focused float owns
// the terminal cursor.

/// `nvim_create_buf(listed=false, scratch=true)` -> a fresh buffer id to bind a
/// float to (so its content is distinguishable from the window beneath).
async fn new_buffer(rpc: &Rpc) -> u64 {
    rpc.request(
        "nvim_create_buf",
        vec![Value::from(false), Value::from(true)],
    )
    .await
    .expect("create_buf")
    .as_u64()
    .expect("a buffer id")
}

/// `nvim_open_win(buf, enter, {…})` -> the float's window id. `buf` is a buffer
/// handle (`0` = current); `entries` are the float config keys.
async fn open_float(rpc: &Rpc, buf: u64, enter: bool, entries: Vec<(&str, Value)>) -> u64 {
    let config = Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    );
    rpc.request(
        "nvim_open_win",
        vec![Value::from(buf), Value::from(enter), config],
    )
    .await
    .expect("open float")
    .as_u64()
    .expect("a window id")
}

#[tokio::test]
async fn a_float_paints_over_the_buffer_beneath() {
    let (rpc, mut incoming) = start(None).await;
    // The tiled window holds "background" on its first line.
    feed(&rpc, "ibackground<Esc>");
    // A borderless float on its own buffer, covering the top-left, focused so we
    // can type its content in.
    let fb = new_buffer(&rpc).await;
    open_float(
        &rpc,
        fb,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("row", Value::from(0u64)),
            ("col", Value::from(0u64)),
            ("width", Value::from(40u64)),
            ("height", Value::from(3u64)),
        ],
    )
    .await;
    feed(&rpc, "iFLOAT<Esc>");
    let buf = screen(&rpc, &mut incoming).await;

    // The float (its own gutter shows line 1, then "FLOAT") replaces the cells it
    // covers: the buffer's "background" no longer shows through (the `Clear`).
    let row0 = row_text(&buf, 0);
    assert!(
        row0.contains("FLOAT"),
        "the float's text is painted: {row0:?}"
    );
    assert!(
        !row0.contains("background"),
        "the float is opaque over the buffer beneath: {row0:?}"
    );
}

#[tokio::test]
async fn a_float_paints_buf_set_lines_content_with_a_clean_gutter() {
    // Regression: a float bound to a scratch buffer whose content was written with
    // `nvim_buf_set_lines` (never entered / typed into) — the path diagnostic /
    // hover / completion popups use — must paint those lines IN FULL. A float
    // defaults to a clean gutter (no line-number column), so the content is not
    // squeezed or truncated. (Before: the float inherited the editor's 4-cell
    // number gutter, so `nvim_open_win({width=13})` content was clipped to 9 cells.)
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ibackground<Esc>");
    // A single-bordered float at (col 0, row 0), 11-cell *content* wide — exactly
    // the width of the line, so any gutter would clip it.
    exec_lua(
        &rpc,
        "local b = vim.api.nvim_create_buf(false, true)\n\
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'abcdefghijk' })\n\
         vim.api.nvim_open_win(b, false, { relative = 'editor', row = 0, col = 0,\n\
                                           width = 11, height = 1, border = 'single',\n\
                                           focusable = false })",
    )
    .await;
    let buf = screen(&rpc, &mut incoming).await;

    // The content row is inside the top border (row 1). With a clean gutter the
    // first cell past the left border is the first character, and the whole line
    // fits — no digits, no truncation.
    let content = row_text(&buf, 1);
    assert!(
        content.contains("abcdefghijk"),
        "the float paints its full set line, ungutted and untruncated: {content:?}"
    );
    assert_eq!(
        buf.cell((1, 1)).unwrap().symbol(),
        "a",
        "content starts right after the left border — no number gutter: {content:?}"
    );
}

#[tokio::test]
async fn a_bordered_float_draws_its_border_and_title() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ibackground<Esc>");
    let fb = new_buffer(&rpc).await;
    // A single-bordered, titled float at (col 2, row 1), 20x6 *content* — the
    // border is drawn outside it (neovim semantics), so the outer box is 22x8.
    open_float(
        &rpc,
        fb,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("row", Value::from(1u64)),
            ("col", Value::from(2u64)),
            ("width", Value::from(20u64)),
            ("height", Value::from(6u64)),
            ("border", Value::from("single")),
            ("title", Value::from("Hello")),
        ],
    )
    .await;
    feed(&rpc, "iinside<Esc>");
    let buf = screen(&rpc, &mut incoming).await;

    // The border box: top-left at the origin, bottom-right at the outer edges
    // (content 20x6 + one border cell per side = 22x8).
    assert_eq!(buf.cell((2, 1)).unwrap().symbol(), "┌", "top-left corner");
    assert_eq!(
        buf.cell((2 + 22 - 1, 1 + 8 - 1)).unwrap().symbol(),
        "┘",
        "bottom-right corner"
    );
    // The title rides on the top border row.
    let top = row_text(&buf, 1);
    assert!(top.contains("Hello"), "title on the top border: {top:?}");
    // The left border runs down the content rows, and the inset content shows the
    // float's text (one cell inside the border, past its own gutter).
    assert_eq!(buf.cell((2, 2)).unwrap().symbol(), "│", "left border");
    let content = row_text(&buf, 2);
    assert!(content.contains("inside"), "inset content: {content:?}");
}

#[tokio::test]
async fn the_cursor_stays_inside_a_focused_float_on_a_long_line() {
    // Regression: moving right (`l`) along a line wider than the float used to draw
    // the terminal cursor past the float's right border (nxvim has no horizontal
    // scroll), so it appeared to "keep going after the floating window". The cursor
    // must stay clamped to the float's text area.
    let (rpc, mut incoming) = start(None).await;
    let fb = new_buffer(&rpc).await;
    // A single-bordered float at (col 5, row 2), 20 wide *content* (neovim
    // semantics: border drawn outside, so the outer box is 22 wide, x in [5, 27)).
    // Inner text area: past the left border (x 6) and the 4-cell gutter (x 10),
    // running to the last content cell at x 25.
    open_float(
        &rpc,
        fb,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("row", Value::from(2u64)),
            ("col", Value::from(5u64)),
            ("width", Value::from(20u64)),
            ("height", Value::from(6u64)),
            ("border", Value::from("single")),
        ],
    )
    .await;
    // Type a line far wider than the float, then walk the cursor right past its edge.
    feed(&rpc, "i");
    feed(&rpc, &"x".repeat(60));
    feed(&rpc, "<Esc>0");
    feed(&rpc, &"l".repeat(40));
    barrier(&rpc).await;
    let params = latest_redraw(&mut incoming).await.expect("a redraw");
    let (_buf, cursor) = paint_with_cursor(&View::from_redraw(&params), COLS, ROWS);
    let (cx, cy) = cursor.expect("a cursor");
    // The float's outer rect spans x in [5, 27); the cursor must stay within it
    // (pinned to the last visible text cell, x 25), never escaping to the right.
    assert!(
        (5..27).contains(&cx),
        "cursor escaped the float horizontally: x={cx}"
    );
    assert_eq!(
        cx, 25,
        "cursor pinned to the float's last visible text column"
    );
    // And it stays on the float's text rows (row 2 border, 3..9 are the 6 content
    // rows that follow it).
    assert!(
        (3..9).contains(&cy),
        "cursor escaped the float vertically: y={cy}"
    );
}

#[tokio::test]
async fn a_higher_zindex_float_paints_over_a_lower_one() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ibackground<Esc>");

    // Open the HIGH-zindex float first, then a LOW-zindex float that covers it,
    // created later. If paint followed creation order the later (low-z) float
    // would win; zindex sorting must keep the high-z float on top.
    let top_buf = new_buffer(&rpc).await;
    open_float(
        &rpc,
        top_buf,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("row", Value::from(4u64)),
            ("col", Value::from(10u64)),
            ("width", Value::from(8u64)),
            ("height", Value::from(3u64)),
            ("border", Value::from("single")),
            ("zindex", Value::from(100u64)),
        ],
    )
    .await;
    feed(&rpc, "iTOP<Esc>");

    let under_buf = new_buffer(&rpc).await;
    open_float(
        &rpc,
        under_buf,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("row", Value::from(2u64)),
            ("col", Value::from(2u64)),
            ("width", Value::from(30u64)),
            ("height", Value::from(8u64)),
            ("border", Value::from("single")),
            ("zindex", Value::from(50u64)),
        ],
    )
    .await;
    feed(&rpc, "iUNDER<Esc>");
    let buf = screen(&rpc, &mut incoming).await;

    // The high-z float's top-left corner sits inside the low-z float's area; it is
    // still visible, so the high-z float won the overlap despite being older.
    assert_eq!(
        buf.cell((10, 4)).unwrap().symbol(),
        "┌",
        "the higher-zindex float paints over the lower one"
    );
}

/// Boot a server sourcing `examples/floats/init.lua`, opened on its sample file,
/// then attach a UI matching the paint grid — the end-to-end check that the
/// shipped example config actually opens a float in the real client.
async fn start_floats_example() -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/floats");
    start_attached(
        ServerInit {
            file: Some(dir.join("sample.txt").to_string_lossy().into_owned()),
            config_dir: Some(dir.clone()),
            runtimepath: vec![dir],
            ..Default::default()
        },
        COLS,
        ROWS - 1,
    )
    .await
}

#[tokio::test]
async fn the_floats_example_opens_a_visible_float_on_startup() {
    let (rpc, mut incoming) = start_floats_example().await;
    // The config opens a startup "hint" float (not focused) — so there are two
    // windows, and the main buffer (the sample file) keeps focus.
    let wins = rpc
        .request("nvim_list_wins", vec![])
        .await
        .expect("list_wins");
    assert_eq!(
        wins.as_array().map(Vec::len),
        Some(2),
        "the example opened the startup float on top of the tiled window"
    );
    let buf = screen(&rpc, &mut incoming).await;
    // The hint float is a single-bordered box at (col 20, row 0) titled
    // "nxvim floats"; its top-left corner and title paint on the top row.
    assert_eq!(
        buf.cell((20, 0)).unwrap().symbol(),
        "┌",
        "hint float border"
    );
    assert!(
        row_text(&buf, 0).contains("nxvim floats"),
        "hint float title: {:?}",
        row_text(&buf, 0)
    );
    // The buffer text beneath still shows on a row the 3-tall float doesn't cover
    // (display row 4 = sample line 5), so the float stole no space from the tiled
    // window.
    assert!(
        row_text(&buf, 4).contains("paints over them"),
        "the tiled window still renders the sample buffer: {:?}",
        row_text(&buf, 4)
    );
}

#[tokio::test]
async fn the_floats_example_move_command_repositions_the_float() {
    let (rpc, mut incoming) = start_floats_example().await;
    // `:FloatMove` calls nvim_win_set_config on the startup hint float with only
    // row/col — it should slide from (col 20, row 0) to the top-left corner while
    // keeping its border and title. The Phase 3 set_config path, driven through
    // the shipped config in the real client.
    feed(&rpc, ":FloatMove<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    assert_eq!(
        buf.cell((0, 0)).unwrap().symbol(),
        "┌",
        "the float's border moved to the top-left corner"
    );
    assert!(
        row_text(&buf, 0).starts_with("┌") && row_text(&buf, 0).contains("nxvim floats"),
        "the moved float keeps its title (size/border unchanged by the partial config): {:?}",
        row_text(&buf, 0)
    );
}

#[tokio::test]
async fn the_floats_example_grow_and_to_split_commands_work() {
    let (rpc, mut incoming) = start_floats_example().await;
    let float = rpc
        .request("nvim_list_wins", vec![])
        .await
        .expect("list_wins")
        .as_array()
        .and_then(|a| a.get(1).and_then(Value::as_u64))
        .expect("the startup float id");

    // `:FloatGrow` reads the float's config and resizes it (+8 wide). The startup
    // float is 40x3, so it becomes 48 wide — exercises get_config + set_config in
    // the shipped command body.
    feed(&rpc, ":FloatGrow<CR>");
    barrier(&rpc).await;
    let cfg = rpc
        .request("nvim_win_get_config", vec![Value::from(float)])
        .await
        .expect("get_config");
    let width = cfg
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("width")))
        .and_then(|(_, v)| v.as_u64());
    assert_eq!(width, Some(48), "FloatGrow widened the float by 8");

    // `:FloatToSplit` converts it into a tiled split (`relative = ""`): it leaves
    // the float layer, so the view now has a separator between two tiled windows.
    feed(&rpc, ":FloatToSplit<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    let relative = rpc
        .request("nvim_win_get_config", vec![Value::from(float)])
        .await
        .expect("get_config")
        .as_map()
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.as_str() == Some("relative"))
                .and_then(|(_, v)| v.as_str().map(str::to_string))
        });
    assert_eq!(
        relative.as_deref(),
        Some(""),
        "the float re-tiled into a split"
    );
    // A horizontal separator row now divides the two tiled windows.
    assert!(
        (0..ROWS).any(|y| row_text(&buf, y).contains("─")),
        "a split separator appears after the float converts to a tiled window"
    );
}

#[tokio::test]
async fn a_focused_float_owns_the_terminal_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ibackground<Esc>");
    let fb = new_buffer(&rpc).await;
    open_float(
        &rpc,
        fb,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("row", Value::from(3u64)),
            ("col", Value::from(5u64)),
            ("width", Value::from(20u64)),
            ("height", Value::from(5u64)),
            ("border", Value::from("single")),
        ],
    )
    .await;
    feed(&rpc, "ihi<Esc>"); // cursor rests on the 'i' (screen col 1) of "hi"
    let params = {
        barrier(&rpc).await;
        latest_redraw(&mut incoming).await.expect("a redraw")
    };
    let (_buf, cursor) = paint_with_cursor(&View::from_redraw(&params), COLS, ROWS);

    // The cursor lands inside the float: past its border (col 5 -> +1), at screen
    // column 1 of "hi" (+1) -> x = 7; one row down past the top border
    // (row 3 -> +1) -> y = 4. A float has a clean gutter (no number column), so
    // there is no gutter offset to add.
    assert_eq!(cursor, Some((5 + 1 + 1, 3 + 1)), "cursor inside the float");
}
