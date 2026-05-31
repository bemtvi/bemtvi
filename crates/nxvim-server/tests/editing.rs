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
    let (server_end, client_end) = tokio::io::duplex(1 << 16);

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, ServerInit { file }));
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

fn temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}.txt", std::process::id()))
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
