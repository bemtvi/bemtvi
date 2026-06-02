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
async fn unreadable_startup_file_keeps_its_name_and_echoes_the_error() {
    // A directory can't be read as text, so `Buffer::from_file` fails. The buffer
    // must still be bound to the path — not fall through to an unnamed scratch
    // buffer that a later `:w` would clobber a stray file from — and the failure
    // must be surfaced on the message line. (R4 in the 2026-06-02 review.)
    let dir = temp_dir("openfail");
    let path = dir.to_string_lossy().into_owned();
    let (rpc, mut incoming) = start(Some(path.clone())).await;

    // The buffer is named after the file the user asked for, not `[No Name]`.
    let name = rpc
        .request("nvim_buf_get_name", vec![Value::from(0u64)])
        .await
        .expect("buf_get_name")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(name, path, "unreadable startup file must keep its name");

    // And the error is echoed, naming the file, rather than silently swallowed.
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        msg.contains(&path),
        "startup error should name the file, got {msg:?}"
    );
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
async fn count_motion_emits_scroll() {
    let path = write_n_lines("count_j", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `30j` lands the cursor on line 30; ensure_visible drags top to 30+1-24 = 7.
    let map = redraw_after(&rpc, &mut incoming, "30j").await;

    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 7);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 30);
    assert_eq!(scroll_u64(&map, "base_line"), 0);
    assert_eq!(scroll_u64(&map, "duration_ms"), 80); // 7*8=56, clamped up to 80
    assert_eq!(scroll_lines_len(&map), 31); // |7 - 0| + 24
}

#[tokio::test]
async fn g_to_last_line_emits_capped_scroll() {
    let path = write_n_lines("big_g", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `G` jumps to line 99; top settles at 99+1-24 = 76. The raw travel is 76
    // lines, but it's capped to two screens (2*24 = 48) so the slide stays bounded.
    let map = redraw_after(&rpc, &mut incoming, "G").await;

    assert_eq!(scroll_u64(&map, "from_top"), 28); // 76 - 48 (cap)
    assert_eq!(scroll_u64(&map, "to_top"), 76);
    assert_eq!(scroll_u64(&map, "from_cursor"), 51); // 99 - 48 (cap)
    assert_eq!(scroll_u64(&map, "to_cursor"), 99);
    assert_eq!(scroll_u64(&map, "base_line"), 28);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160); // 48*8=384, clamped to 160
    assert_eq!(scroll_lines_len(&map), 72); // 48 + 24
}

#[tokio::test]
async fn gg_back_to_top_emits_capped_scroll() {
    let path = write_n_lines("gg", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let _ = redraw_after(&rpc, &mut incoming, "G").await; // jump to the bottom first
    let map = redraw_after(&rpc, &mut incoming, "gg").await; // ...then back to the top

    assert_eq!(scroll_u64(&map, "from_top"), 48); // 0 + 48 (cap)
    assert_eq!(scroll_u64(&map, "to_top"), 0);
    assert_eq!(scroll_u64(&map, "from_cursor"), 48);
    assert_eq!(scroll_u64(&map, "to_cursor"), 0);
    assert_eq!(scroll_u64(&map, "base_line"), 0);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160);
    assert_eq!(scroll_lines_len(&map), 72);
}

#[tokio::test]
async fn single_line_edge_scroll_is_not_animated() {
    let path = write_n_lines("edge", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Move to the last visible row (line 23) without scrolling, then step one
    // line further: the viewport nudges by exactly one line, which must stay
    // crisp rather than animate — otherwise held `j`/`k` would feel laggy.
    let _ = redraw_after(&rpc, &mut incoming, "23j").await;
    let map = redraw_after(&rpc, &mut incoming, "j").await;

    assert!(
        scroll(&map).is_none(),
        "a one-line viewport shift must carry no scroll gesture"
    );
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

#[cfg(unix)]
#[tokio::test]
async fn mkdir_honors_the_permissions_argument() {
    // `vim.fn.mkdir(path, "p", "0700")` must create a private directory, not one
    // with umask-default (world-readable) perms. init.lua runs at startup, so by
    // the time the server is up the directory exists with the requested mode.
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mkdir");
    let target = dir.join("private").join("nested");
    let init = format!(
        "vim.fn.mkdir('{}', 'p', '0700')\n",
        target.to_string_lossy()
    );
    let (_rpc, _incoming) = start_with_config(&dir, &init).await;

    let meta = std::fs::metadata(&target).expect("mkdir should have created the directory");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "mkdir should apply the prot argument, not the umask default"
    );
}

#[tokio::test]
async fn recursive_user_command_does_not_wedge_the_server() {
    // A user command whose callback re-invokes itself feeds run_pending's
    // fixpoint loop forever: each round runs the Lua callback, which queues the
    // command again. The server must cap the recursion, report it, and stay
    // responsive — not spin and wedge the single-threaded loop.
    let dir = temp_dir("recurse");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_user_command('Loop', function() vim.cmd('Loop') end, {})\n",
    )
    .await;

    // Before the fix this never returns (the server thread spins in
    // run_pending), so the whole exchange must complete within a timeout.
    let map = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        redraw_after(&rpc, &mut incoming, ":Loop<CR>"),
    )
    .await
    .expect("recursive command wedged the server: run_pending never converged");

    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E132: command recursion limit exceeded"),
        "self-recursive command should be capped with an error, not loop forever"
    );

    // The server is still alive and processing input after bailing out.
    feed(&rpc, "ihi<Esc>");
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), lines(&rpc))
        .await
        .expect("server unresponsive after capping recursion");
    assert_eq!(
        got,
        vec!["hi".to_string()],
        "editing should work normally once the runaway command is stopped"
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

// ----- bottom panel (`:messages`, `:ls`) ---------------------------------

/// Drain to the *latest* redraw — the one reflecting the settled state after the
/// preceding action. A barrier (`nvim_get_mode`) ensures that action's redraw is
/// already queued; unlike [`redraw_after`] this tolerates leftover redraws from
/// earlier fire-and-forget `feed`s/requests still in the channel.
async fn drain_latest(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Vec<(Value, Value)> {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    tokio::task::yield_now().await; // let the reader task push buffered frames
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = params.into_iter().next();
        }
    }
    match latest {
        Some(Value::Map(map)) => map,
        _ => panic!("no redraw arrived"),
    }
}

/// Feed `keys`, then drain to the latest redraw (see [`drain_latest`]).
async fn latest_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
    drain_latest(rpc, incoming).await
}

/// The `panel` sub-map from a redraw, or `None` when no panel is open.
fn panel(map: &[(Value, Value)]) -> Option<&Vec<(Value, Value)>> {
    match field(map, "panel") {
        Some(Value::Map(m)) => Some(m),
        _ => None,
    }
}

/// The panel's content lines (empty when no panel is open).
fn panel_lines(map: &[(Value, Value)]) -> Vec<String> {
    panel(map)
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.as_str() == Some("lines"))
                .and_then(|(_, v)| v.as_array())
        })
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// A field of the panel sub-map by key, as a u64 (`cursor_row`, `height`).
fn panel_u64(map: &[(Value, Value)], key: &str) -> u64 {
    panel(map)
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .and_then(|(_, v)| v.as_u64())
        })
        .unwrap_or(0)
}

/// The panel's title (empty when no panel is open).
fn panel_title(map: &[(Value, Value)]) -> String {
    panel(map)
        .and_then(|m| {
            m.iter()
                .find(|(k, _)| k.as_str() == Some("title"))
                .and_then(|(_, v)| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn messages_command_shows_history_in_a_panel() {
    let (rpc, mut incoming) = start(None).await;

    // Two printed lines build up the message history.
    feed(&rpc, ":lua print('alpha')<CR>");
    feed(&rpc, ":lua print('beta')<CR>");
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;

    // The panel opens with title "Messages" and the history (newest last).
    assert_eq!(panel_title(&map), "Messages");
    let lines = panel_lines(&map);
    assert!(
        lines.contains(&"alpha".to_string()) && lines.contains(&"beta".to_string()),
        "history was: {lines:?}"
    );
}

#[tokio::test]
async fn panel_navigates_and_closes_with_q() {
    let (rpc, mut incoming) = start(None).await;
    for i in 0..15 {
        feed(&rpc, &format!(":lua print('line{i}')<CR>"));
    }
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    // `:messages` opens scrolled to the end with the newest line selected, so the
    // cursor sits on the last visible row.
    let height = panel_u64(&map, "height");
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        height - 1,
        "opens at the bottom"
    );

    // `gg` returns to the top; `j` moves the panel cursor down.
    let map = latest_after(&rpc, &mut incoming, "gg").await;
    assert_eq!(panel_u64(&map, "cursor_row"), 0);
    let map = latest_after(&rpc, &mut incoming, "j").await;
    assert_eq!(panel_u64(&map, "cursor_row"), 1);

    // `G` jumps back to the last line; scrolled to the bottom again.
    let map = latest_after(&rpc, &mut incoming, "G").await;
    assert_eq!(panel_u64(&map, "cursor_row"), height - 1);

    // `q` closes the panel — the redraw no longer carries one.
    let map = latest_after(&rpc, &mut incoming, "q").await;
    assert!(panel(&map).is_none(), "q should close the panel");
}

#[tokio::test]
async fn panel_grabs_focus_so_the_buffer_is_not_edited() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>"); // buffer: "hello"
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    feed(&rpc, ":messages<CR>"); // open the panel (grabs focus)
                                 // While the panel is focused these keys drive the panel, not the buffer:
                                 // `i` and the letters are ignored, and the trailing <Esc> closes the panel.
    feed(&rpc, "iworld<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"], "buffer must be untouched");
}

#[tokio::test]
async fn panel_shrinks_the_text_window() {
    let (rpc, mut incoming) = start(None).await;
    // No panel: the text window fills the attached height.
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    let full = lines_len(&map);

    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    let with_panel = lines_len(&map);
    let panel_rows = panel_u64(&map, "height") + 1; // content + title bar
    assert_eq!(
        with_panel,
        full - panel_rows as usize,
        "the panel claims rows off the text window"
    );
}

// ----- scriptable panel API (`vim.panel.*`, `nxvim_panel_*`) -------------

#[tokio::test]
async fn lua_vim_panel_opens_sets_and_closes() {
    let (rpc, mut incoming) = start(None).await;
    // Drive via `nvim_command` (not focused keystrokes): once the panel is open
    // it grabs input focus, so a typed `:lua` would go to the panel — but a
    // scripted command still reaches the editor.
    let lua = |src: &str| rpc.request("nvim_command", vec![Value::from(format!("lua {src}"))]);

    lua("vim.panel.open('Custom', {'one', 'two'})")
        .await
        .expect("open");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_title(&map), "Custom");
    assert_eq!(panel_lines(&map), vec!["one", "two"]);

    // set_lines(lines) replaces the content, keeping the title.
    lua("vim.panel.set_lines({'alpha', 'beta', 'gamma'})")
        .await
        .expect("set_lines");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_title(&map), "Custom");
    assert_eq!(panel_lines(&map), vec!["alpha", "beta", "gamma"]);

    // close() dismisses it.
    lua("vim.panel.close()").await.expect("close");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert!(
        panel(&map).is_none(),
        "vim.panel.close() should close the panel"
    );
}

#[tokio::test]
async fn rpc_nxvim_panel_open_set_close_and_query() {
    let (rpc, mut incoming) = start(None).await;

    assert_eq!(
        rpc.request("nxvim_panel_is_open", vec![]).await.unwrap(),
        Value::from(false),
        "no panel open initially"
    );

    rpc.request(
        "nxvim_panel_open",
        vec![
            Value::from("RPC"),
            Value::Array(vec![Value::from("a"), Value::from("b")]),
        ],
    )
    .await
    .expect("panel_open");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_title(&map), "RPC");
    assert_eq!(panel_lines(&map), vec!["a", "b"]);
    assert_eq!(
        rpc.request("nxvim_panel_is_open", vec![]).await.unwrap(),
        Value::from(true)
    );

    rpc.request(
        "nxvim_panel_set_lines",
        vec![Value::Array(vec![Value::from("only")])],
    )
    .await
    .expect("panel_set_lines");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_lines(&map), vec!["only"]);

    rpc.request("nxvim_panel_close", vec![])
        .await
        .expect("panel_close");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert!(panel(&map).is_none());
    assert_eq!(
        rpc.request("nxvim_panel_is_open", vec![]).await.unwrap(),
        Value::from(false)
    );
}

#[tokio::test]
async fn scripted_panel_is_navigable_like_the_builtin_one() {
    let (rpc, mut incoming) = start(None).await;
    let many: Vec<String> = (0..20).map(|i| format!("row{i}")).collect();
    let lines = Value::Array(many.into_iter().map(Value::from).collect());
    rpc.request("nxvim_panel_open", vec![Value::from("Big"), lines])
        .await
        .expect("panel_open");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 0);

    // The panel grabs focus, so j/G navigate it (not the buffer).
    let map = latest_after(&rpc, &mut incoming, "G").await;
    let height = panel_u64(&map, "height");
    assert_eq!(panel_u64(&map, "cursor_row"), height - 1);
}

#[tokio::test]
async fn lua_vim_panel_opens_at_a_cursor_and_set_cursor_moves_it() {
    let (rpc, mut incoming) = start(None).await;
    let lua = |src: &str| rpc.request("nvim_command", vec![Value::from(format!("lua {src}"))]);

    // open(title, lines, on_select, cursor): the 1-based cursor selects (and
    // scrolls to) that line. 20 rows > the panel height, so line 20 scrolls to
    // the bottom and the cursor sits on the last visible row.
    lua("local t = {} for i = 1, 20 do t[i] = 'row' .. i end \
         vim.panel.open('Jump', t, nil, 20)")
    .await
    .expect("open");
    let map = drain_latest(&rpc, &mut incoming).await;
    let height = panel_u64(&map, "height");
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        height - 1,
        "opens scrolled to the requested line"
    );

    // set_cursor(line) moves the selection back to the top (1-based line 1).
    lua("vim.panel.set_cursor(1)").await.expect("set_cursor");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        0,
        "set_cursor moves to the top"
    );
}

#[tokio::test]
async fn rpc_nxvim_panel_open_cursor_and_set_cursor() {
    let (rpc, mut incoming) = start(None).await;
    let many: Vec<String> = (0..20).map(|i| format!("row{i}")).collect();
    let lines = Value::Array(many.into_iter().map(Value::from).collect());

    // open(title, lines, want_select, cursor): the 0-based cursor (19, the last
    // line) opens scrolled to the bottom.
    rpc.request(
        "nxvim_panel_open",
        vec![
            Value::from("Big"),
            lines,
            Value::from(false),
            Value::from(19u64),
        ],
    )
    .await
    .expect("panel_open");
    let map = drain_latest(&rpc, &mut incoming).await;
    let height = panel_u64(&map, "height");
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        height - 1,
        "opens at the cursor"
    );

    // set_cursor(line) moves the 0-based selection back to the top.
    rpc.request("nxvim_panel_set_cursor", vec![Value::from(0u64)])
        .await
        .expect("panel_set_cursor");
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        0,
        "set_cursor moves to the top"
    );
}

// ----- panel <CR> select handler (scriptable) ----------------------------

/// Barrier, then return the params of the most recent `want` notification (e.g.
/// `nxvim_panel_select`) buffered on the connection, or `None` if none arrived.
async fn drain_notify(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Option<Vec<Value>> {
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    tokio::task::yield_now().await;
    let mut found = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == want {
            found = Some(params);
        }
    }
    found
}

#[tokio::test]
async fn lua_panel_on_select_fires_on_enter() {
    let (rpc, mut incoming) = start(None).await;
    // Open with an on_select callback that echoes the selected line + 1-based
    // index, so we can observe it firing on the message line.
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.panel.open('P', {'aaa', 'bbb'}, \
             function(line, idx) print('sel:' .. line .. ':' .. idx) end)",
        )],
    )
    .await
    .expect("open");
    drain_latest(&rpc, &mut incoming).await;

    // Move to the second line (the panel has focus) and press <CR>.
    let map = latest_after(&rpc, &mut incoming, "j<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("sel:bbb:2"),
        "on_select(line, index) should fire for the focused line"
    );
}

#[tokio::test]
async fn lua_panel_on_select_setter_enables_enter() {
    let (rpc, mut incoming) = start(None).await;
    // Open without a handler, then attach one with the standalone setter.
    rpc.request(
        "nvim_command",
        vec![Value::from("lua vim.panel.open('P', {'only'})")],
    )
    .await
    .expect("open");
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.panel.on_select(function(line) print('got:' .. line) end)",
        )],
    )
    .await
    .expect("on_select");
    drain_latest(&rpc, &mut incoming).await;

    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:only")
    );
}

#[tokio::test]
async fn rpc_panel_select_notifies_when_select_enabled() {
    let (rpc, mut incoming) = start(None).await;
    rpc.request(
        "nxvim_panel_open",
        vec![
            Value::from("P"),
            Value::Array(vec![Value::from("x"), Value::from("y")]),
            Value::from(true), // want_select
        ],
    )
    .await
    .expect("open");
    drain_latest(&rpc, &mut incoming).await;

    rpc.notify("nvim_input", vec![Value::from("j<CR>")]);
    let params = drain_notify(&rpc, &mut incoming, "nxvim_panel_select")
        .await
        .expect("a panel_select notification");
    let map = match params.into_iter().next() {
        Some(Value::Map(m)) => m,
        _ => panic!("notification without a map"),
    };
    assert_eq!(field(&map, "index").and_then(Value::as_u64), Some(2)); // 1-based
    assert_eq!(field(&map, "line").and_then(Value::as_str), Some("y"));
}

#[tokio::test]
async fn enter_does_nothing_without_a_select_handler() {
    let (rpc, mut incoming) = start(None).await;
    // A built-in viewer (`:messages`) opts out of select events.
    rpc.request("nvim_command", vec![Value::from("messages")])
        .await
        .expect("messages");
    drain_latest(&rpc, &mut incoming).await;

    rpc.notify("nvim_input", vec![Value::from("<CR>")]);
    assert!(
        drain_notify(&rpc, &mut incoming, "nxvim_panel_select")
            .await
            .is_none(),
        "a panel with no select handler must not emit select events"
    );
}

// ----- search ( `/`, `?`, `n`, `N` ) ----------------------------------------

/// Build a small three-line buffer ("foo bar" / "baz foo" / "qux foo") and park
/// the cursor at the top, for the search tests below.
async fn search_fixture() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>baz foo<CR>qux foo<Esc>gg");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar", "baz foo", "qux foo"],
        "fixture buffer"
    );
    (rpc, incoming)
}

#[tokio::test]
async fn search_forward_jumps_to_next_match() {
    let (rpc, _incoming) = search_fixture().await;
    // From the "foo" under the cursor on line 1, `/foo` finds the next one.
    feed(&rpc, "/foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 4));
    // And again moves to the third.
    feed(&rpc, "/foo<CR>");
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn search_forward_wraps_to_top() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, "G$"); // last line, last "foo"
    let _ = lines(&rpc).await; // barrier: flush the navigation redraw before capturing
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 0));
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("search hit BOTTOM, continuing at TOP")
    );
}

#[tokio::test]
async fn search_backward_jumps_to_previous_match() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "G"); // line 3
    feed(&rpc, "?foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 4));
}

#[tokio::test]
async fn n_and_capital_n_repeat_the_search() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>"); // -> (2,4)
    feed(&rpc, "n"); // same direction -> (3,4)
    assert_eq!(cursor(&rpc).await, (3, 4));
    feed(&rpc, "N"); // opposite direction -> back to (2,4)
    assert_eq!(cursor(&rpc).await, (2, 4));
}

#[tokio::test]
async fn greedy_pattern_steps_to_the_next_match_not_into_itself() {
    // A greedy pattern matches one whole span per line ("foo bar" -> "foo",
    // "baz foo" -> "baz foo"). Navigation must step between those distinct
    // matches, not crawl one grapheme deeper into the match under the cursor:
    // searching from the start of line 1's match lands on line 2, and `n` then
    // moves to line 3 — never to (1,1) or (2,1) inside the current match.
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, r"/.+o<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
    feed(&rpc, "n");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn n_honors_a_count() {
    let (rpc, _incoming) = search_fixture().await;
    // First match is (2,4); `2n` skips ahead two: (3,4) then wrap to (1,0).
    feed(&rpc, "/foo<CR>");
    feed(&rpc, "2n");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn empty_pattern_repeats_the_last_search() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>"); // -> (2,4)
    feed(&rpc, "/<CR>"); // empty -> repeat forward -> (3,4)
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn missing_pattern_reports_e486_and_keeps_the_cursor() {
    let (rpc, mut incoming) = search_fixture().await;
    let map = redraw_after(&rpc, &mut incoming, "/zzz<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "cursor must not move on a miss");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: zzz")
    );
}

#[tokio::test]
async fn escape_cancels_the_search_prompt() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<Esc>");
    assert_eq!(cursor(&rpc).await, (1, 0), "Esc leaves the cursor put");
    // Back in normal mode: a plain motion works again.
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn command_line_shows_the_search_prefix_while_typing() {
    let (rpc, mut incoming) = search_fixture().await;
    let map = redraw_after(&rpc, &mut incoming, "/fo").await;
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("fo"));
    assert_eq!(
        field(&map, "cmdline_prefix").and_then(Value::as_str),
        Some("/")
    );
}

// ----- search options & history (phase 2) -----------------------------------

#[tokio::test]
async fn search_is_case_sensitive_by_default() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>bar<CR>foo<Esc>gg");
    let _ = lines(&rpc).await;
    let map = redraw_after(&rpc, &mut incoming, "/FOO<CR>").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "no case-insensitive match by default"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: FOO")
    );
}

#[tokio::test]
async fn ignorecase_matches_across_case() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>bar<CR>foo<Esc>gg");
    feed(&rpc, ":set ignorecase<CR>");
    feed(&rpc, "/FOO<CR>"); // folds to the "foo" on line 3
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn smartcase_makes_uppercase_patterns_sensitive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>foo<CR>Foo bar<Esc>gg");
    feed(&rpc, ":set ignorecase smartcase<CR>");
    // Lowercase pattern: case-insensitive, so the next line's "foo" matches.
    feed(&rpc, "/foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
    // Uppercase pattern: smartcase forces a case-sensitive match, skipping the
    // lowercase line to the capitalized "Foo" on line 3.
    feed(&rpc, "gg/Foo<CR>");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn counted_search_finds_the_nth_match() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "2/foo<CR>"); // 1st is (2,4), 2nd is (3,4)
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn nowrapscan_forward_reports_e385() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, ":set nowrapscan<CR>");
    feed(&rpc, "G$"); // past the last "foo"
    let _ = lines(&rpc).await;
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (3, 6), "cursor must not move");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E385: search hit BOTTOM without match for: foo")
    );
}

#[tokio::test]
async fn nowrapscan_backward_reports_e384() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, ":set nowrapscan<CR>");
    let _ = lines(&rpc).await;
    // Cursor is at the top, so nothing lies before it.
    let map = redraw_after(&rpc, &mut incoming, "?foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "cursor must not move");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E384: search hit TOP without match for: foo")
    );
}

#[tokio::test]
async fn search_history_recalls_previous_patterns() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>");
    feed(&rpc, "/qux<CR>");
    let _ = lines(&rpc).await; // barrier before capturing
                               // Open a search prompt and walk back: newest ("qux") then older ("foo").
    let map = redraw_after(&rpc, &mut incoming, "/<Up><Up>").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("foo"));
    assert_eq!(
        field(&map, "cmdline_prefix").and_then(Value::as_str),
        Some("/")
    );
}

// ----- search highlighting (phase 3: hlsearch / incsearch) ------------------

/// Per visible row, the search-match spans `[start, end)` (the `Search`
/// hlsearch highlight); an empty inner vec for rows with no match.
fn view_search(view: &[(Value, Value)]) -> Vec<Vec<(u64, u64)>> {
    view_get(view, "search")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| match v.as_array() {
                                    Some(p) if p.len() == 2 => Some((
                                        p[0].as_u64().unwrap_or(0),
                                        p[1].as_u64().unwrap_or(0),
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

#[tokio::test]
async fn hlsearch_highlights_every_match_of_the_pattern() {
    let (rpc, mut incoming) = search_fixture().await;
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    let search = view_search(&map);
    // "foo bar" / "baz foo" / "qux foo" → one "foo" match per line.
    assert_eq!(search.first().cloned().unwrap_or_default(), vec![(0, 3)]);
    assert_eq!(search.get(1).cloned().unwrap_or_default(), vec![(4, 7)]);
    assert_eq!(search.get(2).cloned().unwrap_or_default(), vec![(4, 7)]);
    // Rows past the end of the buffer carry no matches.
    assert!(search.iter().skip(3).all(Vec::is_empty));
}

#[tokio::test]
async fn nohlsearch_clears_the_match_highlight() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>");
    let _ = lines(&rpc).await; // barrier: flush the search redraw
    let map = redraw_after(&rpc, &mut incoming, ":noh<CR>").await;
    let search = view_search(&map);
    assert!(
        search.iter().all(Vec::is_empty),
        ":noh clears every match highlight, got {search:?}"
    );
}

#[tokio::test]
async fn incsearch_previews_the_next_match_while_typing() {
    let (rpc, mut incoming) = search_fixture().await;
    // Typing the pattern (no <CR>) hops the cursor to the next match live...
    let map = redraw_after(&rpc, &mut incoming, "/foo").await;
    assert_eq!(cursor(&rpc).await, (2, 4), "incsearch previews the match");
    // ...and the matches are already highlighted while still in the prompt.
    let search = view_search(&map);
    assert_eq!(search.get(1).cloned().unwrap_or_default(), vec![(4, 7)]);
}

#[tokio::test]
async fn escape_restores_the_origin_after_an_incsearch_preview() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo"); // preview hops the cursor to the line-2 match
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "<Esc>"); // ...and <Esc> rewinds to where the search began
    assert_eq!(cursor(&rpc).await, (1, 0), "Esc restores the search origin");
}

// ----- regex patterns (phase 4) ---------------------------------------------

#[tokio::test]
async fn dot_matches_any_character() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iac<CR>axc<Esc>gg");
    // `.` is a wildcard, so "axc" matches and the two-char "ac" does not.
    feed(&rpc, "/a.c<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn escaped_metacharacter_matches_literally() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaxc<CR>a.c<Esc>gg");
    // `\.` is a literal dot, so it skips "axc" for the line that really has one.
    feed(&rpc, "/a\\.c<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn anchor_caret_matches_line_start() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ixfoo<CR>foo bar<Esc>gg");
    // `^foo` ignores the "foo" embedded after x on line 1, taking line 2's start.
    feed(&rpc, "/^foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn anchor_dollar_matches_line_end() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ibar foo<CR>foo bar<Esc>gg");
    // `foo$` matches the trailing "foo" on line 1, not the one starting line 2.
    feed(&rpc, "/foo$<CR>");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn char_class_matches_a_digit() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<CR>a1c<Esc>gg");
    feed(&rpc, "/[0-9]<CR>");
    assert_eq!(cursor(&rpc).await, (2, 1));
}

#[tokio::test]
async fn quantifier_plus_requires_one_or_more() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iac<CR>abbbc<Esc>gg");
    // Canonical regex: bare `+` is the operator, so "ac" is skipped for "abbbc".
    feed(&rpc, "/ab+c<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn alternation_matches_either_branch() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifish<CR>dog<Esc>gg");
    // Canonical regex: bare `|` alternates (vim would need `\|`).
    feed(&rpc, "/cat|dog<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn word_boundary_matches_whole_word_only() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "icategory<CR>a cat<Esc>gg");
    // `\b` rejects the "cat" inside "category" for the standalone word.
    feed(&rpc, "/\\bcat\\b<CR>");
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn bare_plus_is_an_operator_not_a_literal() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia+b<CR>aaa<Esc>gg");
    // Canonical regex: `a+` matches one-or-more "a" (the "aaa" line), unlike vim
    // where a bare `+` is the literal character.
    feed(&rpc, "/a+<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn escaped_plus_matches_a_literal_plus() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>a+b<Esc>gg");
    // Escape with `\` to match the literal `+`, landing on the "a+b" line.
    feed(&rpc, "/a\\+b<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn inline_flag_forces_case_insensitive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ixxx<CR>FOO<Esc>gg");
    // Search is case-sensitive by default, but `(?i)` folds case for this pattern.
    feed(&rpc, "/(?i)foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn inline_flag_forces_case_sensitive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>foo<Esc>gg");
    feed(&rpc, ":set ignorecase<CR>");
    // `ignorecase` would land on line 1's "Foo", but `(?-i)` overrides it.
    feed(&rpc, "/(?-i)foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn invalid_pattern_reports_e383_and_keeps_the_cursor() {
    let (rpc, mut incoming) = search_fixture().await;
    // An unbalanced group is a compile error (the escaped `\(` would be a literal).
    let map = redraw_after(&rpc, &mut incoming, "/a(b<CR>").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "a pattern that does not compile must not move the cursor"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E383: Invalid search string: a(b")
    );
}

// ----- `*`/`#`, operator motion, offsets (phase 5) --------------------------

#[tokio::test]
async fn star_searches_word_under_cursor_forward() {
    let (rpc, _incoming) = search_fixture().await;
    // Cursor on "foo" (1,0); `*` jumps to the next whole-word "foo", then again.
    feed(&rpc, "*");
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "*");
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn hash_searches_word_under_cursor_backward() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>"); // land on the start of line 2's "foo" (2,4)
    feed(&rpc, "#"); // `#` searches the word backward → line 1's "foo"
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn star_matches_whole_word_only() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foobar<CR>foo<Esc>gg");
    // `*` on "foo" skips "foobar" (not a whole word) for the standalone "foo".
    feed(&rpc, "*");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn g_star_matches_a_partial_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foobar<Esc>gg");
    // `g*` drops the word boundaries, so "foo" matches inside "foobar".
    feed(&rpc, "g*");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn d_slash_deletes_up_to_the_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    // `d/world` deletes from the cursor up to (not including) the match.
    feed(&rpc, "d/world<CR>");
    assert_eq!(lines(&rpc).await, vec!["world"]);
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn c_slash_changes_up_to_the_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    feed(&rpc, "c/world<CR>"); // delete up to "world", land in insert mode
    feed(&rpc, "say <Esc>");
    assert_eq!(lines(&rpc).await, vec!["say world"]);
}

#[tokio::test]
async fn escape_during_an_operator_search_aborts_the_operator() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    feed(&rpc, "d/wor<Esc>"); // abandon the search → no delete
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
    assert_eq!(cursor(&rpc).await, (1, 0));
    // Back in normal mode: a plain edit still works.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["ello world"]);
}

#[tokio::test]
async fn search_offset_e_lands_on_the_match_end() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    // `/world/e` puts the cursor on the last char of the match ("d", col 10).
    feed(&rpc, "/world/e<CR>");
    assert_eq!(cursor(&rpc).await, (1, 10));
}

#[tokio::test]
async fn search_offset_e_makes_an_operator_inclusive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world foo<Esc>gg");
    // `d/world/e` deletes through the end of the match, leaving the rest.
    feed(&rpc, "d/world/e<CR>");
    assert_eq!(lines(&rpc).await, vec![" foo"]);
}

#[tokio::test]
async fn search_line_offset_moves_whole_lines() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb foo<CR>ccc<Esc>gg");
    // `/foo/+1` finds "foo" on line 2 and drops the cursor one line below.
    feed(&rpc, "/foo/+1<CR>");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

// ----- text objects --------------------------------------------------------

#[tokio::test]
async fn diw_deletes_the_word_under_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    // Cursor onto the middle word, delete it (leaving both surrounding spaces).
    feed(&rpc, "0wdiw");
    assert_eq!(lines(&rpc).await, vec!["foo  baz"]);
}

#[tokio::test]
async fn daw_deletes_the_word_and_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    feed(&rpc, "0wdaw");
    assert_eq!(lines(&rpc).await, vec!["foo baz"]);
}

#[tokio::test]
async fn daw_on_last_word_takes_leading_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>");
    // On the final word there is no trailing space, so the leading one goes.
    feed(&rpc, "$daw");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn ciw_changes_the_word_under_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    feed(&rpc, "0ciwqux<Esc>");
    assert_eq!(lines(&rpc).await, vec!["qux bar baz"]);
}

#[tokio::test]
async fn diw_on_whitespace_deletes_the_blank_run() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo   bar<Esc>");
    // Cursor into the run of spaces; `iw` is that whole run.
    feed(&rpc, "0llldiw");
    assert_eq!(lines(&rpc).await, vec!["foobar"]);
}

#[tokio::test]
async fn diw_on_punctuation_stops_at_the_class_boundary() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo...bar<Esc>");
    // On the dots, `iw` is just the punctuation run.
    feed(&rpc, "0llldiw");
    assert_eq!(lines(&rpc).await, vec!["foobar"]);
}

#[tokio::test]
async fn di_word_big_spans_punctuation() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo.bar baz<Esc>");
    // WORD ignores the `.` boundary, so `iW` is the whole "foo.bar".
    feed(&rpc, "0diW");
    assert_eq!(lines(&rpc).await, vec![" baz"]);
}

#[tokio::test]
async fn d2aw_deletes_two_words() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    feed(&rpc, "0d2aw");
    assert_eq!(lines(&rpc).await, vec!["baz"]);
}

#[tokio::test]
async fn viw_selects_the_word() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Cursor in the middle of "hello", select the inner word.
    feed(&rpc, "0llviw");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    let sel = view_selection(&view);
    // "hello" spans columns [0, 5).
    assert_eq!(sel.first().copied().flatten(), Some((0, 5)));
}

#[tokio::test]
async fn di_paren_deletes_inside_the_parens() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    // Cursor inside the parens (onto 'b'), then delete the inner text.
    feed(&rpc, "0lllldi(");
    assert_eq!(lines(&rpc).await, vec!["foo()baz"]);
}

#[tokio::test]
async fn da_paren_deletes_the_parens_too() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    feed(&rpc, "0llllda(");
    assert_eq!(lines(&rpc).await, vec!["foobaz"]);
}

#[tokio::test]
async fn di_paren_works_with_the_cursor_on_the_close_bracket() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    // Move onto the closing paren (column 7), then delete inside.
    feed(&rpc, "0llllllldi(");
    assert_eq!(lines(&rpc).await, vec!["foo()baz"]);
}

#[tokio::test]
async fn ci_brace_changes_innermost_nested_pair() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i{a{b}c}<Esc>");
    // Cursor onto the inner 'b' (column 3); change the innermost braces.
    feed(&rpc, "0lllci{X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["{a{X}c}"]);
}

#[tokio::test]
async fn dib_is_an_alias_for_di_paren() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    feed(&rpc, "0lllldib");
    assert_eq!(lines(&rpc).await, vec!["foo()baz"]);
}

#[tokio::test]
async fn di_brace_big_is_an_alias() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i{bar}<Esc>");
    feed(&rpc, "0diB");
    assert_eq!(lines(&rpc).await, vec!["{}"]);
}

#[tokio::test]
async fn da_angle_deletes_the_bracketed_text() {
    let (rpc, _incoming) = start(None).await;
    // `<lt>`/`<gt>` insert literal angle brackets (a bare `<x>` would parse as a
    // key). Buffer becomes "a<b>c".
    feed(&rpc, "ia<lt>b<gt>c<Esc>");
    // Cursor onto the '<' (column 1), then delete the angle-bracketed text.
    feed(&rpc, "0lda<");
    assert_eq!(lines(&rpc).await, vec!["ac"]);
}

#[tokio::test]
async fn di_bracket_spanning_lines() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix[a<CR>b]y<Esc>");
    // Cursor inside the brackets on the first line ('a', column 2).
    feed(&rpc, "gg0lldi[");
    // Charwise delete of "a\nb" joins the two lines around the brackets.
    assert_eq!(lines(&rpc).await, vec!["x[]y"]);
}

#[tokio::test]
async fn vi_paren_selects_inside() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i(abc)<Esc>");
    feed(&rpc, "0vi(");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    let sel = view_selection(&view);
    // "abc" sits at columns [1, 4).
    assert_eq!(sel.first().copied().flatten(), Some((1, 4)));
}

#[tokio::test]
async fn i_in_normal_mode_still_enters_insert() {
    let (rpc, _incoming) = start(None).await;
    // No operator and not visual: `i` must remain plain insert.
    feed(&rpc, "ifoo<Esc>");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn a_in_normal_mode_still_appends() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    // `a` after the 'f' appends, inserting between f and oo.
    feed(&rpc, "0aX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["fXoo"]);
}

#[tokio::test]
async fn unknown_text_object_cancels_the_operator() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>");
    // `diz` is not a text object; it should cancel and leave the line intact.
    feed(&rpc, "0diz");
    assert_eq!(lines(&rpc).await, vec!["foo bar"]);
}

// ----- quote text objects --------------------------------------------------

#[tokio::test]
async fn di_quote_deletes_inside_the_quotes() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\" ok<Esc>");
    // Cursor inside the quotes (onto 'h', column 5).
    feed(&rpc, "0llllldi\"");
    assert_eq!(lines(&rpc).await, vec!["say \"\" ok"]);
}

#[tokio::test]
async fn da_quote_deletes_quotes_and_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\" ok<Esc>");
    feed(&rpc, "0llllllda\"");
    assert_eq!(lines(&rpc).await, vec!["say ok"]);
}

#[tokio::test]
async fn ci_quote_changes_inside_the_quotes() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\" ok<Esc>");
    feed(&rpc, "0llllllci\"X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["say \"X\" ok"]);
}

#[tokio::test]
async fn di_quote_seeks_forward_on_the_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\"<Esc>");
    // Cursor before the quotes; vim seeks forward to the next pair on the line.
    feed(&rpc, "0di\"");
    assert_eq!(lines(&rpc).await, vec!["say \"\""]);
}

#[tokio::test]
async fn da_quote_takes_leading_space_when_no_trailing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix \"hi\"<Esc>");
    // No trailing whitespace after the closing quote, so the leading space goes.
    feed(&rpc, "0lllda\"");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn di_single_quote() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix'a'y<Esc>");
    // Cursor on 'a' (column 2).
    feed(&rpc, "0lldi'");
    assert_eq!(lines(&rpc).await, vec!["x''y"]);
}

#[tokio::test]
async fn da_backtick_deletes_quoted_span() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix`a`y<Esc>");
    feed(&rpc, "0llda`");
    assert_eq!(lines(&rpc).await, vec!["xy"]);
}

#[tokio::test]
async fn vi_quote_selects_inside() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i\"abc\"<Esc>");
    feed(&rpc, "0lvi\"");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    let sel = view_selection(&view);
    // "abc" sits at columns [1, 4).
    assert_eq!(sel.first().copied().flatten(), Some((1, 4)));
}

#[tokio::test]
async fn di_quote_without_a_pair_does_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ino quotes here<Esc>");
    feed(&rpc, "0di\"");
    assert_eq!(lines(&rpc).await, vec!["no quotes here"]);
}

#[tokio::test]
async fn di_quote_treats_escaped_quote_as_one_string_from_the_left() {
    let (rpc, _incoming) = start(None).await;
    // Buffer: "trib\"uto" — one string with an escaped quote in the middle.
    feed(&rpc, "i\"trib\\\"uto\"<Esc>");
    // Cursor in the "trib" half (column 2).
    feed(&rpc, "0lldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn di_quote_treats_escaped_quote_as_one_string_from_the_right() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i\"trib\\\"uto\"<Esc>");
    // Cursor in the "uto" half (column 8), past the escaped quote.
    feed(&rpc, "08ldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn da_quote_with_escaped_quote_deletes_the_whole_string() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix \"a\\\"b\"<Esc>");
    // Cursor inside; the escaped quote is not a delimiter.
    feed(&rpc, "0llllda\"");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn di_quote_escaped_backslash_keeps_the_closing_quote() {
    let (rpc, _incoming) = start(None).await;
    // Buffer: "a\\" — an escaped backslash, then a real closing quote.
    feed(&rpc, "i\"a\\\\\"<Esc>");
    feed(&rpc, "0ldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn di_quote_with_dangling_quote_works_on_the_left_side() {
    let (rpc, _incoming) = start(None).await;
    // Three unescaped quotes: "trib"uto" — a shared middle quote.
    feed(&rpc, "i\"trib\"uto\"<Esc>");
    // Cursor in the "trib" half (column 2).
    feed(&rpc, "0lldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\"uto\""]);
}

#[tokio::test]
async fn di_quote_with_dangling_quote_works_on_the_right_side() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i\"trib\"uto\"<Esc>");
    // Cursor in the "uto" half (column 7), past the shared middle quote.
    feed(&rpc, "0llllllldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"trib\"\""]);
}

#[tokio::test]
async fn ci_quote_two_strings_seeks_forward_over_the_gap() {
    let (rpc, _incoming) = start(None).await;
    // Even quote count, proper gap: cursor in the gap selects the next string,
    // it does not grab the inter-string space.
    feed(&rpc, "i\"a\" \"b\"<Esc>");
    // Cursor on the space between the strings (column 3).
    feed(&rpc, "0lllci\"X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["\"a\" \"X\""]);
}

// ----- paragraph & sentence text objects -----------------------------------

#[tokio::test]
async fn dap_deletes_the_paragraph_and_trailing_blank_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR><CR>three<Esc>");
    feed(&rpc, "ggdap");
    assert_eq!(lines(&rpc).await, vec!["three"]);
}

#[tokio::test]
async fn dip_deletes_just_the_paragraph() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR><CR>three<Esc>");
    feed(&rpc, "ggdip");
    assert_eq!(lines(&rpc).await, vec!["", "three"]);
}

#[tokio::test]
async fn dip_on_a_blank_line_deletes_the_blank_run() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR><CR><CR>two<Esc>");
    // Onto the middle blank line, delete the run of blank lines.
    feed(&rpc, "ggjdip");
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
}

#[tokio::test]
async fn vap_then_delete_matches_dap() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR><CR>three<Esc>");
    feed(&rpc, "ggvapd");
    assert_eq!(lines(&rpc).await, vec!["three"]);
}

#[tokio::test]
async fn das_deletes_a_sentence_with_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iHello world. Foo bar. Baz qux.<Esc>");
    feed(&rpc, "0das");
    assert_eq!(lines(&rpc).await, vec!["Foo bar. Baz qux."]);
}

#[tokio::test]
async fn dis_deletes_a_sentence_without_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iHello world. Foo bar.<Esc>");
    feed(&rpc, "0dis");
    assert_eq!(lines(&rpc).await, vec![" Foo bar."]);
}

#[tokio::test]
async fn das_on_a_middle_sentence() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iHello world. Foo bar. Baz qux.<Esc>");
    // Cursor onto the second sentence (column 13, 'F').
    feed(&rpc, "013ldas");
    assert_eq!(lines(&rpc).await, vec!["Hello world. Baz qux."]);
}

#[tokio::test]
async fn das_handles_a_terminator_before_a_closing_quote() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iSay \"Hi.\" Go.<Esc>");
    feed(&rpc, "0das");
    assert_eq!(lines(&rpc).await, vec!["Go."]);
}

#[tokio::test]
async fn cis_changes_the_current_sentence() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iOne. Two.<Esc>");
    feed(&rpc, "0cisHi<Esc>");
    assert_eq!(lines(&rpc).await, vec!["Hi Two."]);
}

// ----- linewise promotion of block objects ---------------------------------

#[tokio::test]
async fn di_paren_promotes_to_linewise_for_whole_line_content() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>    bar,<CR>    baz,<CR>)<Esc>");
    // Cursor on a content line, then delete the inner block.
    feed(&rpc, "ggjdi(");
    // The content lines go; the bracket lines stay (linewise).
    assert_eq!(lines(&rpc).await, vec!["foo(", ")"]);
}

#[tokio::test]
async fn di_brace_promotes_to_linewise_from_the_close_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifn() {<CR>    body();<CR>}<Esc>");
    // Cursor on the closing-brace line still finds the block.
    feed(&rpc, "di{");
    assert_eq!(lines(&rpc).await, vec!["fn() {", "}"]);
}

#[tokio::test]
async fn ci_brace_linewise_opens_a_line_for_insert() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifn() {<CR>    old();<CR>}<Esc>");
    feed(&rpc, "ggjci{new();<Esc>");
    assert_eq!(lines(&rpc).await, vec!["fn() {", "new();", "}"]);
}

#[tokio::test]
async fn da_paren_stays_charwise_for_whole_line_content() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>    bar,<CR>)<Esc>");
    // `a(` includes the brackets and is charwise: everything collapses.
    feed(&rpc, "ggjda(");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn vi_paren_stays_charwise_in_visual_mode() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>    bar,<CR>)<Esc>");
    // In visual mode the block object is charwise (no linewise promotion), so
    // deleting joins the bracket lines.
    feed(&rpc, "ggjvi(d");
    assert_eq!(lines(&rpc).await, vec!["foo()"]);
}

#[tokio::test]
async fn di_paren_linewise_with_no_content_lines_is_a_noop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>)<Esc>");
    feed(&rpc, "ggdi(");
    assert_eq!(lines(&rpc).await, vec!["foo(", ")"]);
}
