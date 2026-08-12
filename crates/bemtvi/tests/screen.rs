//! Tier 2: the full in-process stack — real server -> real `View` -> real
//! client paint — asserted on the painted cell grid. Deterministic: the
//! `barrier`/`lines` request guarantees all prior input was processed and its
//! redraw emitted before we read the screen. No sleeps.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, feed, start_attached};
use bemtvi_tui::{paint, paint_with_cursor};
use bemtvi_view::View;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24; // windows area is ROWS - 1 cmd row = 23; text is 22 (status inside)
/// Default number-column width for a small buffer: bemtvi ships with the hybrid
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
async fn default_statusline_shows_the_fileencoding() {
    // bemtvi's built-in status line shows the buffer's 'fileencoding'. A fresh
    // [No Name] buffer is utf-8.
    let (rpc, mut incoming) = start(None).await;
    let buf = screen(&rpc, &mut incoming).await;
    let status = row_text(&buf, ROWS - 2);
    assert!(
        status.contains("utf-8"),
        "default status line should show the encoding: {status:?}"
    );
}

#[tokio::test]
async fn default_statusline_shows_a_non_utf8_fileencoding() {
    // A latin1 file (0xe9 = é) decodes through the encoding seam, and the default
    // status line reports its detected fileencoding rather than the utf-8 default.
    let path = bemtvi_test_harness::temp_path("screen_enc");
    std::fs::write(&path, b"caf\xe9\n").expect("write latin1 file");
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    let buf = screen(&rpc, &mut incoming).await;
    let status = row_text(&buf, ROWS - 2);
    assert!(
        status.contains("latin1"),
        "status line should show the detected latin1 encoding: {status:?}"
    );
}

#[tokio::test]
async fn unprintable_control_byte_paints_as_a_highlighted_hex_token() {
    // A C1 control byte (0x81, an undefined windows-1252 high byte the latin1
    // fallback passes through as U+0081) would paint as a font tofu box. The
    // display projection shows it vim-style as `<81>`, painted in the SpecialKey
    // look (here the built-in LightMagenta fallback, no colorscheme loaded) so it
    // reads as non-text — distinct from the plain `a` / `b` around it.
    let path = bemtvi_test_harness::temp_path("screen_control");
    std::fs::write(&path, b"a\x81b\n").expect("write control-byte file");
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    let buf = screen(&rpc, &mut incoming).await;

    // Row 0: the hybrid gutter (4 cells) then `a<81>b`.
    assert_eq!(row_text(&buf, 0).trim_end(), "1   a<81>b");
    // `a` is at the first text cell; the `<81>` token's `<` is the next cell.
    assert_ne!(
        fg(&buf, GUTTER, 0),
        Some(Color::LightMagenta),
        "plain text is not painted in the SpecialKey color"
    );
    for (i, ch) in "<81>".chars().enumerate() {
        let x = GUTTER + 1 + i as u16;
        assert_eq!(
            buf.cell((x, 0)).map(|c| c.symbol()),
            Some(ch.to_string().as_str()),
            "the {ch:?} cell of the <81> token"
        );
        assert_eq!(
            fg(&buf, x, 0),
            Some(Color::LightMagenta),
            "the <81> token paints in the SpecialKey fallback color"
        );
    }
}

#[tokio::test]
async fn the_block_cursor_envelops_a_multi_cell_token() {
    // The cursor opens on the first byte — a 0x81 control shown as the 4-cell
    // `<81>` token. A block cursor envelops the whole token: the terminal's one
    // hardware cursor sits on the first cell (so it isn't reverse-painted in the
    // headless buffer), and its three trailing cells are painted reverse-video.
    let path = bemtvi_test_harness::temp_path("screen_cursor_width");
    std::fs::write(&path, b"\x81abc\n").expect("write control-byte file");
    let (rpc, mut incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    let buf = screen(&rpc, &mut incoming).await;

    // Row 0: gutter, then the `<81>` token (text cols GUTTER..GUTTER+4) and `abc`.
    assert_eq!(row_text(&buf, 0).trim_end(), "1   <81>abc");
    // The token's trailing three cells are reverse-video (the cursor block); the
    // first cell is the hardware cursor's, and the `a` after the token is not.
    assert!(
        reversed(&buf, GUTTER + 1, 0),
        "<81> cell 2 is under the cursor"
    );
    assert!(
        reversed(&buf, GUTTER + 2, 0),
        "<81> cell 3 is under the cursor"
    );
    assert!(
        reversed(&buf, GUTTER + 3, 0),
        "<81> cell 4 is under the cursor"
    );
    assert!(
        !reversed(&buf, GUTTER + 4, 0),
        "the 'a' after the token is outside the cursor"
    );
    // The cursor block is a clean reversed *default* cell, not the SpecialKey
    // colour reversed — otherwise the trailing cells would paint the token's
    // LightMagenta foreground as a coloured background instead of matching the
    // hardware cursor's plain block.
    assert_ne!(
        fg(&buf, GUTTER + 1, 0),
        Some(Color::LightMagenta),
        "the cursor block clears the SpecialKey colour"
    );
    // The terminal cursor is placed on the token's first cell.
    barrier(&rpc).await;
    let params = latest_redraw(&mut incoming).await.expect("a redraw");
    let (_buf, cursor) = paint_with_cursor(&View::from_redraw(&params), COLS, ROWS);
    assert_eq!(
        cursor,
        Some((GUTTER, 0)),
        "cursor on the token's first cell"
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

/// Whether a painted cell is struck through (the removed side of the `:s` diff
/// preview carries `Modifier::CROSSED_OUT`).
fn crossed(buf: &Buffer, x: u16, y: u16) -> bool {
    buf.cell((x, y))
        .map(|c| c.style().add_modifier.contains(Modifier::CROSSED_OUT))
        .unwrap_or(false)
}

#[tokio::test]
async fn substitute_replacement_preview_paints_a_diff_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo and foo<Esc>");
    // Type the substitute WITHOUT submitting: the live diff preview strikes each
    // "foo" (removed, red) and splices "xyz" after it (added, green). `/g` so both
    // matches on the line preview.
    feed(&rpc, ":%s/foo/xyz/g");
    let buf = screen(&rpc, &mut incoming).await;

    // The row now reads as the diff: struck "foo" with "xyz" spliced in after each
    // (past the `1   ` number gutter).
    assert_eq!(
        row_text(&buf, 0).trim_end(),
        "1   fooxyz and fooxyz",
        "the removed text stays, the replacement is shown inline after it"
    );

    let red = Color::Rgb(0xf3, 0x8b, 0xa8);
    let green = Color::Rgb(0xa6, 0xe3, 0xa1);
    // The first "foo" (cols GUTTER..+3) is struck through in red.
    for dx in 0..3 {
        assert_eq!(fg(&buf, GUTTER + dx, 0), Some(red), "removed 'foo' is red");
        assert!(crossed(&buf, GUTTER + dx, 0), "removed 'foo' is struck");
    }
    // The inserted "xyz" (cols GUTTER+3..+6) is green and not struck.
    assert_eq!(row_text(&buf, 0).get(4 + 3..4 + 6), Some("xyz"));
    for dx in 3..6 {
        assert_eq!(
            fg(&buf, GUTTER + dx, 0),
            Some(green),
            "added 'xyz' is green"
        );
        assert!(!crossed(&buf, GUTTER + dx, 0), "added text is not struck");
    }

    // Submitting applies the real substitute and clears the overlay.
    feed(&rpc, "<CR>");
    let buf = screen(&rpc, &mut incoming).await;
    assert_eq!(row_text(&buf, 0).trim_end(), "1   xyz and xyz");
    assert!(
        !crossed(&buf, GUTTER, 0),
        "no strike-through remains after submit"
    );
}

#[tokio::test]
async fn substitute_confirm_paints_the_diff_on_the_current_match() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo<Esc>");
    // `:s/foo/xyz/gc` opens the confirm prompt on the first match; that match shows
    // the diff (struck "foo" red + inline "xyz" green) while the pending one stays
    // on the plain yellow search highlight.
    feed(&rpc, ":s/foo/xyz/gc<CR>");
    let buf = screen(&rpc, &mut incoming).await;

    assert_eq!(row_text(&buf, 0).trim_end(), "1   fooxyz foo");
    let red = Color::Rgb(0xf3, 0x8b, 0xa8);
    let green = Color::Rgb(0xa6, 0xe3, 0xa1);
    // The current match: struck red "foo", then green "xyz", and NOT the yellow
    // search background (it yielded to the diff).
    assert_eq!(fg(&buf, GUTTER, 0), Some(red), "current match is red");
    assert!(crossed(&buf, GUTTER, 0), "current match is struck");
    assert_ne!(
        bg(&buf, GUTTER, 0),
        Some(Color::Yellow),
        "no yellow under the diff"
    );
    assert_eq!(
        fg(&buf, GUTTER + 3, 0),
        Some(green),
        "the inline 'xyz' is green"
    );
    // The pending second match ("foo" at text cols 8..11 after the "xyz" splice)
    // keeps its yellow search highlight.
    assert_eq!(
        bg(&buf, GUTTER + 8, 0),
        Some(Color::Yellow),
        "the pending match keeps the plain highlight"
    );

    // Prompt is up on the command row.
    assert!(row_text(&buf, ROWS - 1).contains("replace with xyz"));
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
async fn messages_command_renders_a_scratch_window_at_the_bottom() {
    let (rpc, mut incoming) = start(None).await;
    // Build one history line, then open the messages listing.
    feed(&rpc, ":lua print('hello panel')<CR>");
    feed(&rpc, ":messages<CR>");
    let buf = screen(&rpc, &mut incoming).await;

    // `:messages` now opens a read-only scratch buffer in a bottom split (not the old
    // grabbing panel): its history line renders in the window body, and a statusline
    // names the listing `[Messages]`.
    let rows: Vec<String> = (0..ROWS).map(|r| row_text(&buf, r)).collect();
    assert!(
        rows.iter().any(|r| r.contains("hello panel")),
        "expected the history line on screen; rows: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("Messages")),
        "expected a statusline naming the listing; rows: {rows:?}"
    );
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
    // the terminal cursor past the float's right border (bemtvi has no horizontal
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

/// An `editor`-relative float is centered on the **whole screen**, not on the
/// region that happens to be focused: with a 20-column left dock open, a centered
/// float still straddles the middle of the full 80 columns — painting over the dock
/// band it overlaps. (Centered in the dock-shrunk main region it would sit ten
/// columns to the right, its left border at 29.)
#[tokio::test]
async fn an_editor_float_centers_on_the_whole_screen_over_a_dock() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(&rpc, "btv.dock.open{ side = 'left', size = 20 }").await;
    let fb = new_buffer(&rpc).await;
    open_float(
        &rpc,
        fb,
        true,
        vec![
            ("relative", Value::from("editor")),
            ("align", Value::from("center")),
            ("width", Value::from(40u64)),
            ("height", Value::from(10u64)),
            ("border", Value::from("single")),
        ],
    )
    .await;
    feed(&rpc, "iFLOATBODY<Esc>");
    let buf = screen(&rpc, &mut incoming).await;

    let y = (0..ROWS)
        .find(|&y| row_text(&buf, y).contains("FLOATBODY"))
        .expect("the float's content is painted somewhere");
    let row: Vec<char> = row_text(&buf, y).chars().collect();
    // Outer box = 40 inner + one border cell per side = 42, centered on 80 → the
    // side borders land on columns 19 and 60.
    assert_eq!(
        row[19],
        '│',
        "left border at col 19: {:?}",
        row_text(&buf, y)
    );
    assert_eq!(
        row[60],
        '│',
        "right border at col 60: {:?}",
        row_text(&buf, y)
    );
}
