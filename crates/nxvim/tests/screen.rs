//! Tier 2: the full in-process stack — real server -> real `View` -> real
//! client paint — asserted on the painted cell grid. Deterministic: the
//! `barrier`/`lines` request guarantees all prior input was processed and its
//! redraw emitted before we read the screen. No sleeps.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_tui::{paint, View};
use ratatui::buffer::Buffer;
use ratatui::style::Modifier;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24; // text area is ROWS - 2 chrome rows = 22
/// Default number-column width for a small buffer: nxvim ships with the hybrid
/// number column on, sized to 4 cells (vim's `numberwidth` minimum). Text,
/// selection, and cursor columns are all offset by this much.
const GUTTER: u16 = 4;

/// Start a server and attach with a text-area height matching the paint grid
/// (ROWS - 2 chrome rows), so the captured `View` fills the grid exactly.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(
            server_end,
            ServerInit {
                file,
                ..Default::default()
            },
        ));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(COLS as u64),
            Value::from((ROWS - 2) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
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
async fn wide_chars_align_on_screen() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>");
    let buf = screen(&rpc, &mut incoming).await;
    // Wide glyphs start past the number gutter, each still occupying two cells.
    assert_eq!(buf.cell((GUTTER, 0)).unwrap().symbol(), "日");
    assert_eq!(buf.cell((GUTTER + 2, 0)).unwrap().symbol(), "本");
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
