//! Behavior tests for nxvim, driven the way a real client drives it.
//!
//! These are deliberately *black box*: every test starts a real server on its
//! own thread, connects over the same msgpack-RPC a UI uses, sends vim
//! key-notation via `nvim_input`, and asserts on observable results — buffer
//! contents (`nvim_buf_get_lines`), the bytes written to disk, or the rendered
//! screen. Nothing reaches into the editor's internals. We verify *what the
//! editor does*, not how it's built.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with(ServerInit {
        file,
        ..Default::default()
    })
    .await
}

/// Like [`start`], but with a fully-specified [`ServerInit`] — used by tests
/// that need an explicit config dir / runtimepath (kept off the host's home).
async fn start_with(init: ServerInit) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });

    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);

    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");

    (rpc, incoming)
}

/// Type a string of vim key-notation.
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Fetch all buffer lines. Also serves as a barrier: awaiting the response
/// guarantees the server has processed every message sent before it.
async fn lines(rpc: &Rpc) -> Vec<String> {
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
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Cursor position as `(1-based line, 0-based column)`.
async fn cursor(rpc: &Rpc) -> (usize, usize) {
    let result = rpc
        .request("nvim_win_get_cursor", vec![Value::from(0u64)])
        .await
        .expect("get_cursor");
    match result {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0) as usize,
            a.get(1).and_then(Value::as_u64).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    }
}

/// Feed `keys`, then deterministically return the `redraw` map the server
/// emitted *for that input*. Works because the server processes messages
/// serially: it writes each message's response and then its `redraw`. We send
/// `nvim_input` then `nvim_get_mode`; the wire order is input-response,
/// input-redraw, barrier-response, barrier-redraw. Since the input's redraw
/// is written before the barrier's response, by the time the barrier `.await`
/// resolves the input's redraw is already queued in `incoming` — so the first
/// redraw we drain is the one for this input.
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // discard any buffered notifications from earlier in the test

    // request (not notify): the server responds *then* redraws, and the barrier below relies on that ordering
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => return map,
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue,
            Err(_) => panic!("no redraw arrived for {keys:?}"),
        }
    }
}

/// Look up a top-level key in a redraw map.
fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Number of entries in the redraw's `lines` array.
fn lines_len(map: &[(Value, Value)]) -> usize {
    field(map, "lines")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0)
}

/// The `scroll` sub-map, or `None` when the redraw carries no scroll gesture.
fn scroll(map: &[(Value, Value)]) -> Option<&Vec<(Value, Value)>> {
    match field(map, "scroll") {
        Some(Value::Map(m)) => Some(m),
        _ => None,
    }
}

/// Read a u64 field out of the `scroll` sub-map.
fn scroll_u64(map: &[(Value, Value)], key: &str) -> u64 {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or_else(|| panic!("scroll.{key} missing"))
}

/// Number of entries in `scroll.lines`.
fn scroll_lines_len(map: &[(Value, Value)]) -> usize {
    let s = scroll(map).expect("scroll present");
    s.iter()
        .find(|(k, _)| k.as_str() == Some("lines"))
        .and_then(|(_, v)| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Write `n` lines ("line1".."lineN") to a temp file and return its path string.
fn write_n_lines(tag: &str, n: usize) -> String {
    let path = temp_path(tag);
    let body: String = (1..=n).map(|i| format!("line{i}\n")).collect();
    std::fs::write(&path, body).expect("write temp file");
    path.to_string_lossy().into_owned()
}

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}.txt", std::process::id()))
}

/// Create and return a fresh, uniquely-named temp directory for a test fixture
/// (e.g. a throwaway config dir / runtimepath).
fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn inserting_text_appears_in_the_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn opening_lines_and_navigating() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifirst<Esc>osecond<Esc>othird<Esc>");
    assert_eq!(lines(&rpc).await, vec!["first", "second", "third"]);
}

#[tokio::test]
async fn dd_deletes_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // Back to the middle line and delete it.
    feed(&rpc, "kdd");
    assert_eq!(lines(&rpc).await, vec!["one", "three"]);
}

#[tokio::test]
async fn cw_changes_a_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    // Start of line, change first word.
    feed(&rpc, "0cwqux<Esc>");
    assert_eq!(lines(&rpc).await, vec!["qux bar baz"]);
}

#[tokio::test]
async fn undo_reverts_the_last_change() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "ddu");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
}

#[tokio::test]
async fn yank_and_paste_duplicates_a_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "yyp");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn ex_write_persists_changes_to_disk() {
    let path = temp_path("write");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Jump to the last line, open a new one, type, leave insert, then save.
    feed(&rpc, "Gothree<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
        .await
        .expect("write");

    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, "one\ntwo\nthree\n");
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn lua_vim_cmd_drives_the_editor() {
    // A Lua chunk that opens a file should change what the buffer shows.
    let path = temp_path("lua");
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let (rpc, _incoming) = start(None).await;
    let chunk = format!("lua vim.cmd(\"edit {}\")", path.to_string_lossy());
    rpc.request("nvim_command", vec![Value::from(chunk.as_str())])
        .await
        .expect("lua command");

    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn vertical_motion_preserves_desired_column() {
    let (rpc, _incoming) = start(None).await;
    // Long, short, long — the classic case where j/k must remember the column.
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "ohi<Esc>");
    feed(&rpc, "ogoodbye world<Esc>");

    // Top line, move to column 8 ('r' in "hello world").
    feed(&rpc, "gg8l");
    assert_eq!(cursor(&rpc).await, (1, 8));

    // Down onto the short line: cursor clamps to its last column...
    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (2, 1));

    // ...and down again onto a long line: the remembered column is restored.
    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (3, 8));

    // Back up through the short line restores it too.
    feed(&rpc, "kk");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

#[tokio::test]
async fn dollar_sticks_to_end_of_line_through_j() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "oto<Esc>");
    feed(&rpc, "oomega!<Esc>");

    // `$` on the first line, then move down: each line lands on its own end.
    feed(&rpc, "gg$");
    assert_eq!(cursor(&rpc).await, (1, 4)); // "alpha" -> last col

    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (2, 1)); // "to" -> last col

    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await, (3, 5)); // "omega!" -> last col

    // A horizontal move clears the end-of-line stickiness.
    feed(&rpc, "gg0jj");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn view_reflects_typed_text_and_mode() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello");
    // Barrier: ensure the input (and its redraw) have been processed.
    let _ = lines(&rpc).await;

    let view = latest_view(&mut incoming).expect("a redraw view");

    let first = view_lines(&view);
    assert_eq!(first.first().map(String::as_str), Some("hello"));
    assert_eq!(view_str(&view, "mode_label"), "INSERT");
}

/// The most recent `redraw` view map currently buffered on the connection.
fn latest_view(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<(Value, Value)>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            if let Some(Value::Map(map)) = params.into_iter().next() {
                latest = Some(map);
            }
        }
    }
    latest
}

fn view_lines(view: &[(Value, Value)]) -> Vec<String> {
    view_get(view, "lines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Per visible row, the highlighted screen-column span `[start, end)`, or
/// `None` for rows with no visual selection.
fn view_selection(view: &[(Value, Value)]) -> Vec<Option<(u64, u64)>> {
    view_get(view, "selection")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| match v.as_array() {
                    Some(pair) if pair.len() == 2 => {
                        Some((pair[0].as_u64().unwrap_or(0), pair[1].as_u64().unwrap_or(0)))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn view_str(view: &[(Value, Value)], key: &str) -> String {
    view_get(view, key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn view_u64(view: &[(Value, Value)], key: &str) -> u64 {
    view_get(view, key).and_then(Value::as_u64).unwrap_or(0)
}

fn view_get<'a>(view: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    view.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

#[tokio::test]
async fn screen_column_accounts_for_wide_characters() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>"); // each CJK char is 3 bytes wide, 2 cells wide
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");
    // Cursor rests on the last char 本: byte column 3, screen column 2.
    assert_eq!(view_u64(&view, "cursor_col"), 3);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 2);
}

#[tokio::test]
async fn screen_column_expands_tabs_to_the_next_tabstop() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i<Tab>x<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    // Cursor on 'x' at byte column 1; the leading tab puts it at screen col 8.
    assert_eq!(view_u64(&view, "cursor_col"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 8);
}

#[tokio::test]
async fn charwise_visual_highlights_the_selected_columns() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Back to column 0, then select three characters inclusively (h, e, l).
    feed(&rpc, "0vll");
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Cursor rests on the third char, which is included → columns [0, 3).
    assert_eq!(sel.first().copied().flatten(), Some((0, 3)));
    // No other visible row is selected.
    assert!(sel.iter().skip(1).all(Option::is_none));
}

#[tokio::test]
async fn charwise_visual_spanning_lines_marks_the_newline_cell() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>");
    // Top of buffer, column 0, then select down onto the second line's 'b'.
    feed(&rpc, "gg0vj");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // First line is fully selected plus one trailing cell for the newline.
    assert_eq!(sel.first().copied().flatten(), Some((0, 4)));
    // Second line is selected up to and including the char under the cursor.
    assert_eq!(sel.get(1).copied().flatten(), Some((0, 1)));
}

#[tokio::test]
async fn linewise_visual_highlights_the_whole_line_width() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    feed(&rpc, "V");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Linewise selection fills the line to the text edge: the viewport (attached
    // at 80) minus the default 4-cell number gutter, so the highlight stops at
    // the text area and never bleeds into the gutter.
    assert_eq!(sel.first().copied().flatten(), Some((0, 76)));
}

#[tokio::test]
async fn linewise_visual_fills_full_width_without_a_gutter() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    // With no number column the whole viewport width is text again.
    feed(&rpc, ":set nonumber norelativenumber<CR>");
    feed(&rpc, "V");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    assert_eq!(sel.first().copied().flatten(), Some((0, 80)));
}

#[tokio::test]
async fn charwise_visual_selecting_backwards_orders_the_span() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    // Cursor rests on 'o' (col 4); select leftwards back to 'l' (col 2).
    feed(&rpc, "vhh");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Anchor 'o' and cursor 'l' are both inclusive → columns [2, 5).
    assert_eq!(sel.first().copied().flatten(), Some((2, 5)));
}

#[tokio::test]
async fn leaving_visual_mode_clears_the_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "0vll<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    assert!(sel.iter().all(Option::is_none));
}

#[tokio::test]
async fn horizontal_motion_steps_over_multibyte_chars() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "in\u{e9}on<Esc>"); // "néon": n é(2 bytes) o n
    feed(&rpc, "0");
    assert_eq!(cursor(&rpc).await, (1, 0)); // 'n'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 1)); // 'é'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 3)); // 'o' — skipped é's second byte
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 4)); // last 'n'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 4)); // stays put at end of line
    feed(&rpc, "hh");
    assert_eq!(cursor(&rpc).await, (1, 1)); // back across 'o' and onto 'é'
}

#[tokio::test]
async fn x_deletes_a_whole_grapheme_cluster() {
    let (rpc, _incoming) = start(None).await;
    // 'e' + combining acute accent (one grapheme, three bytes) followed by 'x'.
    feed(&rpc, "ie\u{0301}x<Esc>");
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn x_deletes_a_wide_char_and_leaves_the_rest() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>");
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec!["本"]);
}

#[tokio::test]
async fn charwise_paste_keeps_a_combining_grapheme_intact() {
    // "éx" is e + combining acute, then x. Yank the é cluster, then paste it
    // after the cursor: it must land whole after é, never split between the
    // base and its combining mark.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ie\u{0301}x<Esc>");
    feed(&rpc, "0ylp");
    assert_eq!(lines(&rpc).await, vec!["e\u{0301}e\u{0301}x"]);
}

#[tokio::test]
async fn r_replaces_a_whole_grapheme_cluster() {
    // `r` removes its range directly (it does not go through the grapheme-aware
    // snap_range that `x` uses), so grapheme-stepping the advance is what keeps
    // the combining mark from being orphaned onto the replacement character.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ie\u{0301}x<Esc>"); // "éx" as e + combining acute + x
    feed(&rpc, "0rz"); // replace the first grapheme (é) with 'z'
    assert_eq!(lines(&rpc).await, vec!["zx"]);
}

#[tokio::test]
async fn insert_backspace_deletes_a_precomposed_char() {
    let (rpc, _incoming) = start(None).await;
    // Type "aé" (é precomposed, 2 bytes) then backspace once: the whole 'é' goes.
    feed(&rpc, "ia\u{e9}");
    feed(&rpc, "<BS>");
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn insert_backspace_deletes_a_combining_grapheme() {
    let (rpc, _incoming) = start(None).await;
    // Type "a" then "e" + combining acute (one grapheme). Backspace must remove
    // the WHOLE cluster (base + mark), not just the combining mark.
    feed(&rpc, "iae\u{0301}");
    feed(&rpc, "<BS>");
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn dw_deletes_a_multibyte_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ih\u{e9}llo w\u{f6}rld<Esc>"); // "héllo wörld"
    feed(&rpc, "0dw");
    assert_eq!(lines(&rpc).await, vec!["w\u{f6}rld"]);
}

#[tokio::test]
async fn b_and_e_handle_multibyte_words() {
    // "foo wörld": w is byte 4, ö spans bytes 5..7, d is byte 9.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo w\u{f6}rld<Esc>");
    // `b` lands on a word boundary, never inside ö's continuation byte.
    feed(&rpc, "$b");
    assert_eq!(cursor(&rpc).await, (1, 4)); // start of "wörld"
    feed(&rpc, "b");
    assert_eq!(cursor(&rpc).await, (1, 0)); // start of "foo"

    // `e` lands on the last char of each word, stepping over the wide cluster.
    feed(&rpc, "e");
    assert_eq!(cursor(&rpc).await, (1, 2)); // last 'o' of "foo"
    feed(&rpc, "e");
    assert_eq!(cursor(&rpc).await, (1, 9)); // 'd' at the end of "wörld"
}

#[tokio::test]
async fn vertical_motion_keeps_screen_column_across_wide_chars() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i日本x<Esc>"); // screen columns: 日@0, 本@2, x@4
    feed(&rpc, "oabcdef<Esc>"); // an ASCII line below it
    feed(&rpc, "gg"); // line 1, on 日
    feed(&rpc, "l"); // → 本, byte col 3, screen col 2
    assert_eq!(cursor(&rpc).await, (1, 3));
    feed(&rpc, "j"); // down: screen col 2 → byte col 2 ('c')
    assert_eq!(cursor(&rpc).await, (2, 2));
    feed(&rpc, "k"); // back up: screen col 2 → byte col 3 (本)
    assert_eq!(cursor(&rpc).await, (1, 3));
}

#[tokio::test]
async fn vertical_motion_keeps_screen_column_across_a_tab() {
    // A leading tab expands to 8 cells, so 'x' sits at screen column 8 even
    // though it is byte 1. Vertical motion must map that screen column onto the
    // ASCII line below (where byte == screen column).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i<Tab>x<Esc>"); // line 1: "\tx"
    feed(&rpc, "oabcdefghij<Esc>"); // line 2: ASCII
    feed(&rpc, "ggl"); // line 1, onto 'x' at byte 1 / screen col 8
    assert_eq!(cursor(&rpc).await, (1, 1));
    feed(&rpc, "j"); // down: screen col 8 → byte 8 ('i')
    assert_eq!(cursor(&rpc).await, (2, 8));
    feed(&rpc, "k"); // back up: screen col 8 → byte 1 ('x')
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn dl_deletes_a_trailing_multibyte_grapheme() {
    // `dl` on the last char must delete that whole grapheme (like `x`) and keep
    // the line's newline. This relies on `l` advancing its motion target to
    // end-of-line (s.len()) so the exclusive operator range covers the last
    // character; clamping `l` short of EOL would make `dl` a no-op here.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "in\u{e9}on<Esc>"); // "néon"
    feed(&rpc, "$dl"); // on last 'n' -> delete it
    assert_eq!(lines(&rpc).await, vec!["n\u{e9}o"]);
    feed(&rpc, "$dl"); // on 'o' -> delete it
    assert_eq!(lines(&rpc).await, vec!["n\u{e9}"]);
    feed(&rpc, "$dl"); // on 'é' -> delete the whole 2-byte cluster
    assert_eq!(lines(&rpc).await, vec!["n"]);
}

#[tokio::test]
async fn redraw_has_no_scroll_for_plain_motion() {
    let path = write_n_lines("noscroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "j").await;

    assert!(
        scroll(&map).is_none(),
        "a plain `j` must carry no scroll gesture"
    );
    assert_eq!(lines_len(&map), 24, "viewport stays one screen tall");
}

#[tokio::test]
async fn ctrl_d_emits_half_page_scroll() {
    let path = write_n_lines("cd", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "<C-d>").await;

    // Viewport height 24 → half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 12);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
    assert_eq!(scroll_u64(&map, "base_line"), 0);
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8, within [80,160]
                                                     // Window = |to-from| + height = 12 + 24.
    assert_eq!(scroll_lines_len(&map), 36);
}

#[tokio::test]
async fn ctrl_f_emits_full_page_scroll() {
    let path = write_n_lines("cf", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "<C-f>").await;

    // Full page = height - 2 = 22.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 22);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160); // 22*8=176, clamped to 160
    assert_eq!(scroll_lines_len(&map), 46); // 22 + 24
}

#[tokio::test]
async fn ctrl_u_at_top_is_not_a_scroll() {
    let path = write_n_lines("cu", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Already at the top: top can't move up, so no slide.
    let map = redraw_after(&rpc, &mut incoming, "<C-u>").await;

    assert!(
        scroll(&map).is_none(),
        "no viewport movement → no scroll gesture"
    );
}

#[tokio::test]
async fn scroll_window_pads_past_end_of_buffer() {
    let path = write_n_lines("eof", 30);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "<C-f>").await;

    assert_eq!(scroll_u64(&map, "to_top"), 22);
    assert_eq!(scroll_lines_len(&map), 46); // window length is fixed regardless of EOF
                                            // The 30-line buffer fills rows 0..30; the rest are "~".
    let s = scroll(&map).unwrap();
    let lines = s
        .iter()
        .find(|(k, _)| k.as_str() == Some("lines"))
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert_eq!(lines.last().and_then(Value::as_str), Some("~"));
}

#[tokio::test]
async fn ctrl_u_mid_buffer_scrolls_up() {
    let path = write_n_lines("cu_mid", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Scroll down a full page first so there's room to scroll back up.
    let _ = redraw_after(&rpc, &mut incoming, "<C-f>").await; // top 0 -> 22
    let map = redraw_after(&rpc, &mut incoming, "<C-u>").await; // top 22 -> 10

    assert_eq!(scroll_u64(&map, "from_top"), 22);
    assert_eq!(scroll_u64(&map, "to_top"), 10);
    assert_eq!(scroll_u64(&map, "from_cursor"), 22);
    assert_eq!(scroll_u64(&map, "to_cursor"), 10);
    assert_eq!(scroll_u64(&map, "base_line"), 10); // min(from, to)
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8
    assert_eq!(scroll_lines_len(&map), 36); // |22 - 10| + 24
}

#[tokio::test]
async fn sleep_blocks_the_editor_for_the_requested_duration() {
    let (rpc, _incoming) = start(None).await;
    // The command is acknowledged promptly; the server then sleeps. The next
    // request can only be handled once the sleep finishes, so its round-trip
    // time is a reliable *lower bound* on the sleep (lower bounds never flake).
    rpc.request("nvim_command", vec![Value::from("sleep 150m")])
        .await
        .expect("sleep command");
    let begin = std::time::Instant::now();
    let _ = lines(&rpc).await;
    assert!(
        begin.elapsed() >= std::time::Duration::from_millis(120),
        "follow-up returned too soon: {:?}",
        begin.elapsed()
    );
}

// ----- line-number column ---------------------------------------------------

/// Read a top-level bool field out of a redraw map.
fn field_bool(map: &[(Value, Value)], key: &str) -> bool {
    field(map, key).and_then(Value::as_bool).unwrap_or(false)
}

/// The redraw's per-row `numbers` array as `Option<u64>` (None = `~` filler).
fn numbers(map: &[(Value, Value)]) -> Vec<Option<u64>> {
    field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::as_u64).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn number_column_is_on_by_default() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    assert!(field_bool(&map, "number"), "number on by default");
    assert!(
        field_bool(&map, "relativenumber"),
        "relativenumber on by default"
    );
    // Small buffer → 4-cell gutter (vim's numberwidth minimum).
    assert_eq!(field(&map, "number_width").and_then(Value::as_u64), Some(4));
}

#[tokio::test]
async fn numbers_track_buffer_lines_and_filler_rows() {
    let path = write_n_lines("nums", 2);
    let (rpc, mut incoming) = start(Some(path)).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    let nums = numbers(&map);
    // Two real lines numbered 1, 2; everything below is a `~` filler (None).
    assert_eq!(nums[0], Some(1));
    assert_eq!(nums[1], Some(2));
    assert!(
        nums[2..].iter().all(|n| n.is_none()),
        "fillers carry no number"
    );
}

#[tokio::test]
async fn set_nonumber_disables_the_gutter() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set nonumber norelativenumber<CR>").await;

    assert!(!field_bool(&map, "number"));
    assert!(!field_bool(&map, "relativenumber"));
    assert_eq!(
        field(&map, "number_width").and_then(Value::as_u64),
        Some(0),
        "no number option → zero-width gutter"
    );
}

#[tokio::test]
async fn set_toggles_and_abbreviations_work() {
    let (rpc, mut incoming) = start(None).await;

    // `nu!` toggles `number` off; `rnu` abbreviation stays on.
    let map = redraw_after(&rpc, &mut incoming, ":set nu!<CR>").await;
    assert!(!field_bool(&map, "number"), "nu! toggled number off");
    assert!(
        field_bool(&map, "relativenumber"),
        "relativenumber untouched"
    );

    // `invnumber` toggles it back on.
    let map = redraw_after(&rpc, &mut incoming, ":set invnumber<CR>").await;
    assert!(field_bool(&map, "number"), "invnumber toggled number on");
}

// ----- Lua plugin runtime (init.lua + require over the runtimepath) ----------

#[tokio::test]
async fn init_lua_runs_at_startup_and_require_resolves_runtimepath_modules() {
    // A throwaway config dir doubling as a runtimepath entry. `init.lua` pulls a
    // module off the runtimepath via `require` and prints the value it returns;
    // observing it on the message line proves both the module search
    // (`package.path` seeded from the runtimepath) and startup sourcing.
    let dir = temp_dir("rtp");
    std::fs::create_dir_all(dir.join("lua")).expect("create lua dir");
    std::fs::write(
        dir.join("lua").join("probe.lua"),
        "return { greeting = 'loaded from probe' }\n",
    )
    .expect("write probe module");
    std::fs::write(
        dir.join("init.lua"),
        "local probe = require('probe')\nprint(probe.greeting)\n",
    )
    .expect("write init.lua");

    let (rpc, mut incoming) = start_with(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    })
    .await;

    // Empty input is a no-op edit that still triggers a redraw, carrying the
    // message `init.lua` left behind at startup.
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("loaded from probe"),
        "init.lua should run and require() should resolve modules on the runtimepath"
    );
}

#[tokio::test]
async fn missing_init_lua_is_harmless() {
    // A config dir with no init.lua must start cleanly (no config is normal).
    let dir = temp_dir("noinit");
    let (rpc, mut incoming) = start_with(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    })
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some(""),
        "no init.lua → no startup message or error"
    );
}

// ----- vim.* surface (Phase 2): helpers, options, user commands -------------

/// Start a server whose config dir / runtimepath is `dir`, after writing
/// `init_lua` to `<dir>/init.lua`. Returns the connected client.
async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    start_with(ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    })
    .await
}

/// The message line from the redraw produced by a no-op input — i.e. whatever
/// `init.lua` left behind at startup.
async fn startup_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> String {
    let map = redraw_after(rpc, incoming, "").await;
    field(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn vim_tbl_deep_extend_merges_nested_tables() {
    let dir = temp_dir("tbl");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local r = vim.tbl_deep_extend('force', {a=1, b={c=2}}, {b={d=3}})\n\
         print(r.a .. ',' .. r.b.c .. ',' .. r.b.d)\n",
    )
    .await;
    assert_eq!(startup_message(&rpc, &mut incoming).await, "1,2,3");
}

#[tokio::test]
async fn vim_g_round_trips_a_global() {
    let dir = temp_dir("vimg");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.g.colors_name = 'mocha'\nprint(vim.g.colors_name)\n",
    )
    .await;
    assert_eq!(startup_message(&rpc, &mut incoming).await, "mocha");
}

#[tokio::test]
async fn vim_cmd_is_callable_and_indexable() {
    // The indexable form `vim.cmd.set("number")` must build and run `:set
    // number`, observable as the redraw's `number` flag flipping on.
    let dir = temp_dir("vimcmd");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.set('number')\n").await;
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert!(
        field(&map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "vim.cmd.set('number') should enable the number option"
    );
}

#[tokio::test]
async fn vim_fn_stdpath_returns_an_nxvim_path() {
    let dir = temp_dir("stdpath");
    let (rpc, mut incoming) = start_with_config(&dir, "print(vim.fn.stdpath('cache'))\n").await;
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        !msg.is_empty() && msg.contains("nxvim"),
        "stdpath('cache') should be a non-empty nxvim path, got {msg:?}"
    );
}

#[tokio::test]
async fn user_command_registers_and_dispatches() {
    // Register `:Greet` from init.lua, then invoke it with an argument; its
    // callback's print() should land on the message line.
    let dir = temp_dir("usercmd");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_user_command('Greet', function(o) print('hi ' .. o.args) end, {})\n",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":Greet there<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hi there"),
        "typed :Greet should dispatch to the Lua user command"
    );
}

#[tokio::test]
async fn unknown_command_still_reports_the_standard_error() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":Frobnicate<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E492: Not an editor command: Frobnicate"),
        "a command with no core handler and no user command is still an error"
    );
}

#[tokio::test]
async fn colorscheme_style_plugin_load_runs_clean() {
    // A miniature plugin mimicking catppuccin's shape: setup() merges config,
    // load() sets options/globals and fires nvim_set_hl (incl. a link), and it
    // registers a user command and an autocmd. The whole load must run without a
    // Lua error — proving the Phase 2 surface is broad enough for that pattern.
    let dir = temp_dir("scheme");
    std::fs::create_dir_all(dir.join("lua").join("minischeme")).expect("create module dir");
    std::fs::write(
        dir.join("lua").join("minischeme").join("init.lua"),
        "local M = { options = {} }\n\
         function M.setup(conf)\n\
           M.options = vim.tbl_deep_extend('force', { flavour = 'default' }, conf or {})\n\
         end\n\
         function M.load()\n\
           if not M.options.flavour then M.setup() end\n\
           vim.o.termguicolors = true\n\
           vim.g.colors_name = 'minischeme-' .. M.options.flavour\n\
           vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
           vim.api.nvim_set_hl(0, 'Comment', { fg = '#6c7086', italic = true })\n\
           vim.api.nvim_set_hl(0, '@keyword', { link = 'Keyword' })\n\
           vim.api.nvim_create_user_command('MiniScheme', function() M.load() end, {})\n\
           vim.api.nvim_create_autocmd('ColorScheme', { pattern = 'minischeme', callback = function() end })\n\
         end\n\
         return M\n",
    )
    .expect("write module");

    let (rpc, mut incoming) = start_with_config(
        &dir,
        "require('minischeme').setup({ flavour = 'mocha' })\n\
         require('minischeme').load()\n\
         print('ok ' .. tostring(vim.g.colors_name) .. ' tgc=' .. tostring(vim.o.termguicolors))\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "ok minischeme-mocha tgc=true",
        "the colorscheme-style load path should complete without error"
    );
}

// ----- highlight registry (Phase 3): nvim_set_hl, links, captures, colorscheme

/// `#rrggbb` as the `0xRRGGBB` integer the highlight RPCs report colors as.
fn hex(rgb: &str) -> u64 {
    u32::from_str_radix(rgb.trim_start_matches('#'), 16).expect("hex color") as u64
}

/// Resolve a highlight group via `nvim_get_hl(0, { name = group })`, returning
/// its concrete-style map (empty when the group is unstyled/absent).
async fn get_hl(rpc: &Rpc, group: &str) -> Vec<(Value, Value)> {
    let opts = Value::Map(vec![(Value::from("name"), Value::from(group))]);
    let result = rpc
        .request("nvim_get_hl", vec![Value::from(0u64), opts])
        .await
        .expect("get_hl");
    match result {
        Value::Map(map) => map,
        _ => Vec::new(),
    }
}

/// Resolve a treesitter capture name through the `@`-group fallback chain;
/// `None` when nothing in the registry matches.
async fn resolve_capture(rpc: &Rpc, capture: &str) -> Option<Vec<(Value, Value)>> {
    let result = rpc
        .request("nxvim_resolve_capture", vec![Value::from(capture)])
        .await
        .expect("resolve_capture");
    match result {
        Value::Map(map) => Some(map),
        _ => None,
    }
}

/// A color field (`fg`/`bg`/`sp`) from a resolved-style map.
fn hl_color(map: &[(Value, Value)], key: &str) -> Option<u64> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
}

/// Whether a boolean attribute (`bold`, `italic`, …) is set in a style map.
fn hl_flag(map: &[(Value, Value)], key: &str) -> bool {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_bool())
        .unwrap_or(false)
}

#[tokio::test]
async fn nvim_set_hl_stores_resolved_colors_and_attrs() {
    // catppuccin-mocha-ish: Normal carries fg+bg, Comment fg+italic. The
    // registry stores them and nvim_get_hl reads them back as RGB ints + flags.
    let dir = temp_dir("hlset");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
         vim.api.nvim_set_hl(0, 'Comment', { fg = '#6c7086', italic = true })\n",
    )
    .await;
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1e1e2e")));
    let comment = get_hl(&rpc, "Comment").await;
    assert_eq!(hl_color(&comment, "fg"), Some(hex("6c7086")));
    assert!(hl_flag(&comment, "italic"), "Comment should be italic");
}

#[tokio::test]
async fn nvim_get_hl_follows_links_to_the_target_color() {
    // `@keyword` is a pure link to `Keyword`; resolving it must yield Keyword's
    // concrete color and attributes, not an empty alias.
    let dir = temp_dir("hllink");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Keyword', { fg = '#cba6f7', bold = true })\n\
         vim.api.nvim_set_hl(0, '@keyword', { link = 'Keyword' })\n",
    )
    .await;
    let kw = get_hl(&rpc, "@keyword").await;
    assert_eq!(hl_color(&kw, "fg"), Some(hex("cba6f7")));
    assert!(
        hl_flag(&kw, "bold"),
        "linked group inherits the target's bold"
    );
}

#[tokio::test]
async fn capture_resolves_through_the_group_fallback_chain() {
    // Only the broad groups are themed; specific captures must fall through to
    // them. `string` -> String (green); `function.call` -> @function.call ->
    // @function -> Function (blue); an unknown capture resolves to nothing.
    let dir = temp_dir("capfb");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'String', { fg = '#a6e3a1' })\n\
         vim.api.nvim_set_hl(0, 'Function', { fg = '#89b4fa' })\n",
    )
    .await;
    let string = resolve_capture(&rpc, "string")
        .await
        .expect("string resolves");
    assert_eq!(hl_color(&string, "fg"), Some(hex("a6e3a1")));
    let call = resolve_capture(&rpc, "function.call")
        .await
        .expect("function.call resolves via fallback");
    assert_eq!(hl_color(&call, "fg"), Some(hex("89b4fa")));
    assert!(
        resolve_capture(&rpc, "frobnicate").await.is_none(),
        "an unknown capture has no resolved style"
    );
}

#[tokio::test]
async fn colorscheme_sources_the_file_and_fires_the_autocmd() {
    // `:colorscheme cat` must source colors/cat.lua (populating the registry)
    // and fire the ColorScheme autocmd registered in init.lua.
    let dir = temp_dir("colo");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('ColorScheme', \
           { pattern = 'cat', callback = function(o) print('themed:' .. o.match) end })\n",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":colorscheme cat<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("themed:cat"),
        "the ColorScheme autocmd should fire with the scheme name"
    );
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1e1e2e")));
}

#[tokio::test]
async fn init_lua_colorscheme_themes_the_first_frame() {
    // A colorscheme loaded from init.lua must be in effect before the first
    // frame is served — so the startup redraw already carries resolved chrome,
    // not bare defaults. (The real-plugin version of this is the Tier-3 PTY
    // test `catppuccin_repaints_the_editor_in_truecolor`.)
    let dir = temp_dir("startup_theme");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("cat.lua"),
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n",
    )
    .expect("write colorscheme");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.colorscheme('cat')\n").await;

    // The startup frame's `chrome.normal` indexes a `styles` entry carrying
    // catppuccin's base background — i.e. the theme painted the very first frame.
    let map = redraw_after(&rpc, &mut incoming, "").await;
    let normal_id = field(&map, "chrome")
        .and_then(|c| chrome_id(c, "normal"))
        .expect("Normal resolved in the startup frame's chrome");
    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("styles palette");
    let normal = match &styles[normal_id] {
        Value::Map(m) => m.as_slice(),
        _ => panic!("style entry is not a map"),
    };
    assert_eq!(hl_color(normal, "bg"), Some(hex("1e1e2e")));
    assert_eq!(hl_color(normal, "fg"), Some(hex("cdd6f4")));
}

/// The `style_id` a redraw's `chrome` map assigns to region `key`, if resolved.
fn chrome_id(chrome: &Value, key: &str) -> Option<usize> {
    match chrome {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_u64())
            .map(|n| n as usize),
        _ => None,
    }
}

#[tokio::test]
async fn colorscheme_missing_file_reports_e185() {
    let dir = temp_dir("colomiss");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let map = redraw_after(&rpc, &mut incoming, ":colorscheme nope<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E185: Cannot find color scheme 'nope'"),
        "a colorscheme with no file on the runtimepath is an error"
    );
}

#[tokio::test]
async fn hi_clear_empties_the_registry() {
    let dir = temp_dir("hiclear");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4' })\n",
    )
    .await;
    assert_eq!(
        hl_color(&get_hl(&rpc, "Normal").await, "fg"),
        Some(hex("cdd6f4"))
    );
    let _ = redraw_after(&rpc, &mut incoming, ":hi clear<CR>").await;
    assert!(
        get_hl(&rpc, "Normal").await.is_empty(),
        ":hi clear should empty the registry back to defaults"
    );
}

// ----- compile step (Phase 4): bytecode round-trip + on-disk cache -----------

/// Install a colorscheme fixture that exercises catppuccin's real compile
/// mechanics under `dir`: its `load()` serializes a highlight table to Lua
/// source, `loadstring`s it, `string.dump(fn, true)`s the result to bytecode,
/// writes that to `<compile_path>/<flavour>` via `io.open(..., "wb")`, then on
/// load `loadfile`s the cached bytecode and runs it (firing `nvim_set_hl`). A
/// `vim.g._compiles` counter makes cache reuse observable. This mirrors the real
/// plugin's `lib/compiler.lua` + `init.lua` load path; the actual catppuccin
/// checkout is wired up in Phase 6. `compile_path` is a subdir of `dir` so the
/// test can assert the cache file without touching `~/.cache`.
fn write_compiler_fixture(dir: &std::path::Path) {
    let module = dir.join("lua").join("compilescheme");
    std::fs::create_dir_all(&module).expect("create module dir");
    let compile_path = dir.join("cache");
    std::fs::write(
        module.join("init.lua"),
        format!(
            "local M = {{ options = {{ compile_path = {path:?}, flavour = 'mocha' }} }}\n\
             local sep = package.config:sub(1, 1)\n\
             local function inspect(t)\n\
               local list = {{}}\n\
               for k, v in pairs(t) do\n\
                 if type(v) == 'string' then\n\
                   list[#list + 1] = string.format('%s = \"%s\"', k, v)\n\
                 else\n\
                   list[#list + 1] = string.format('%s = %s', k, tostring(v))\n\
                 end\n\
               end\n\
               return '{{ ' .. table.concat(list, ', ') .. ' }}'\n\
             end\n\
             local function compile(flavour)\n\
               vim.g._compiles = (vim.g._compiles or 0) + 1\n\
               local theme = {{\n\
                 Normal = {{ fg = '#cdd6f4', bg = '#1e1e2e' }},\n\
                 Comment = {{ fg = '#6c7086', italic = true }},\n\
                 Keyword = {{ fg = '#cba6f7' }},\n\
                 ['@keyword'] = {{ link = 'Keyword' }},\n\
               }}\n\
               local lines = {{\n\
                 'return string.dump(function(flavour)\\n'\n\
                 .. 'vim.o.termguicolors = true\\n'\n\
                 .. 'vim.g.colors_name = \"compilescheme-' .. flavour .. '\"\\n'\n\
                 .. 'local h = vim.api.nvim_set_hl',\n\
               }}\n\
               for group, color in pairs(theme) do\n\
                 lines[#lines + 1] = string.format('h(0, \"%s\", %s)', group, inspect(color))\n\
               end\n\
               lines[#lines + 1] = 'end, true)'\n\
               if vim.fn.isdirectory(M.options.compile_path) == 0 then\n\
                 vim.fn.mkdir(M.options.compile_path, 'p')\n\
               end\n\
               local f = assert(loadstring(table.concat(lines, '\\n')), 'compile failed')\n\
               local file = assert(io.open(M.options.compile_path .. sep .. flavour, 'wb'))\n\
               file:write(f())\n\
               file:close()\n\
             end\n\
             function M.setup(conf) M.options = vim.tbl_deep_extend('force', M.options, conf or {{}}) end\n\
             function M.load(flavour)\n\
               flavour = flavour or M.options.flavour\n\
               local compiled = M.options.compile_path .. sep .. flavour\n\
               local f = loadfile(compiled)\n\
               if not f then\n\
                 compile(flavour)\n\
                 f = assert(loadfile(compiled), 'could not load cache')\n\
               end\n\
               f(flavour)\n\
               print('compiles=' .. tostring(vim.g._compiles or 0))\n\
             end\n\
             return M\n",
            path = compile_path.to_string_lossy(),
        ),
    )
    .expect("write module");
    std::fs::create_dir_all(dir.join("colors")).expect("create colors dir");
    std::fs::write(
        dir.join("colors").join("compilescheme.lua"),
        "require('compilescheme').load()\n",
    )
    .expect("write colors file");
}

#[tokio::test]
async fn colorscheme_compiles_to_bytecode_then_reuses_the_cache() {
    // Strategy A end-to-end: the first `:colorscheme` compiles (serialize ->
    // loadstring -> string.dump -> io.write), loads the cached bytecode via
    // loadfile, and runs it to populate the registry. The second reuses the
    // on-disk cache without recompiling (the compile counter stays at 1).
    let dir = temp_dir("compile");
    write_compiler_fixture(&dir);
    let (rpc, mut incoming) = start_with(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    })
    .await;

    // First load: no cache yet, so it compiles exactly once.
    let first = redraw_after(&rpc, &mut incoming, ":colorscheme compilescheme<CR>").await;
    assert_eq!(
        field(&first, "message").and_then(Value::as_str),
        Some("compiles=1"),
        "first colorscheme load should compile once"
    );

    // The bytecode cache file was written to disk.
    assert!(
        dir.join("cache").join("mocha").is_file(),
        "the compiled flavour should be cached on disk"
    );

    // The registry is populated through the real bytecode load path.
    let normal = get_hl(&rpc, "Normal").await;
    assert_eq!(hl_color(&normal, "fg"), Some(hex("cdd6f4")));
    assert_eq!(hl_color(&normal, "bg"), Some(hex("1e1e2e")));
    assert!(hl_flag(&get_hl(&rpc, "Comment").await, "italic"));
    assert_eq!(
        hl_color(&get_hl(&rpc, "@keyword").await, "fg"),
        Some(hex("cba6f7")),
        "the linked @keyword resolves through the compiled table"
    );

    // Second load: the cache exists, so loadfile succeeds and no recompile
    // happens — the counter is still 1.
    let second = redraw_after(&rpc, &mut incoming, ":colorscheme compilescheme<CR>").await;
    assert_eq!(
        field(&second, "message").and_then(Value::as_str),
        Some("compiles=1"),
        "second load should reuse the cached bytecode, not recompile"
    );
}
