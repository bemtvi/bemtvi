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

    // Attach a 25-row windows area: each window spends its bottom row on a
    // status line, so the text viewport these tests reason about is 24 rows
    // (25 − 1). (The attached height is the windows-area height — the frame minus
    // the client's command row — not the text height.)
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(25u64), Value::Map(vec![])],
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

/// Drain every `redraw` currently queued in `incoming` and return the most
/// recent one for which `keep` holds (skipping non-redraw notifications and
/// redraws `keep` rejects), or `None` when none qualifies.
fn drain_to_latest_redraw(
    incoming: &mut UnboundedReceiver<Incoming>,
    keep: impl Fn(&[(Value, Value)]) -> bool,
) -> Option<Vec<(Value, Value)>> {
    let mut latest = None;
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => {
                        if keep(&map) {
                            latest = Some(map);
                        }
                    }
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue, // a non-redraw notification — ignore
            Err(_) => return latest,
        }
    }
}

/// Feed `keys`, then return the most recent queued `redraw` satisfying `keep`.
///
/// The server processes messages serially, writing each message's response and
/// then its `redraw`. We send `nvim_input` then a `nvim_get_mode` barrier; the
/// wire order is input-response, input-redraw, barrier-response, barrier-redraw,
/// and the client's reader task ferries it into `incoming` in that same order.
/// So once the barrier `.await` resolves, the input's redraw is guaranteed
/// queued.
///
/// We take the most recent qualifying redraw, not the first. A redraw still in
/// flight from earlier in the test — the startup frame, or a previous call's
/// trailing barrier repaint — can land in `incoming` after the pre-drain below
/// when the reader task lags under load, and taking the first would then return
/// that stale frame (the source of the intermittent failures). `keep` lets a
/// caller pin the exact frame it means: the default takes the freshest state
/// (the barrier's repaint is state-identical to the input's), while scroll tests
/// pass [`has_scroll`] to single out the input's frame, the only one carrying
/// the one-shot `scroll` gesture (which the trailing barrier repaint lacks).
async fn redraw_after_matching(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
    keep: impl Fn(&[(Value, Value)]) -> bool,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // discard any buffered notifications from earlier in the test

    // request (not notify): the server responds *then* redraws, and the barrier below relies on that ordering
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");

    if let Some(map) = drain_to_latest_redraw(incoming, &keep) {
        return map;
    }
    // The barrier guarantees the input's redraw is queued before its response, so
    // the drain above should have found it. Under heavy load the reader task can
    // still lag; poll a bounded while rather than failing on the first miss.
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(map) = drain_to_latest_redraw(incoming, &keep) {
            return map;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// Feed `keys` and return the freshest resulting `redraw` — the common case.
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    redraw_after_matching(rpc, incoming, keys, |_| true).await
}

/// Feed `keys` and return the `redraw` carrying the one-shot `scroll` gesture —
/// the input's own frame, not the state-only barrier repaint that trails it.
async fn scroll_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    redraw_after_matching(rpc, incoming, keys, |map| scroll(map).is_some()).await
}

/// Look up a key in a redraw map: a global key at the top level, or a per-window
/// key (lines, cursor_*, selection, search, numbers, scroll, …) under the first
/// window (`windows[0]`). Key names don't collide between the two, so the
/// windows[0] fallback is unambiguous and keeps the single-window test helpers
/// working unchanged across the per-window protocol move.
fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .or_else(|| window0_field(map, key))
}

/// A per-window key from the first window's sub-map (`windows[0]`).
fn window0_field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    let windows = field_top(map, "windows")?.as_array()?;
    let Value::Map(win) = windows.first()? else {
        return None;
    };
    win.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// A strictly top-level lookup (no window fallback), for resolving `windows`
/// itself without recursing.
fn field_top<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
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

/// The first visible buffer line of the focused window (`lines[0]`) — reveals the
/// viewport `top` for a content-numbered buffer (e.g. `write_n_lines`).
fn first_visible_line(map: &[(Value, Value)]) -> String {
    field(map, "lines")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
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
async fn named_register_round_trips() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Yank the first line into register `a`, then paste it below the last line.
    feed(&rpc, "gg\"ayy");
    feed(&rpc, "G\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta", "alpha"]);
}

#[tokio::test]
async fn uppercase_register_appends() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // `"ayy` then `"Ayy` accumulates both lines in register `a`.
    feed(&rpc, "gg\"ayy");
    feed(&rpc, "j\"Ayy");
    feed(&rpc, "G\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta", "alpha", "beta"]);
}

#[tokio::test]
async fn delete_ring_shifts_through_numbered_registers() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // Two linewise deletes fill the ring: `"1` = "two", `"2` = "one".
    feed(&rpc, "ggdd");
    feed(&rpc, "dd");
    // Buffer is just ["three"]; paste `"1` then `"2` back in.
    feed(&rpc, "\"1p\"2p");
    assert_eq!(lines(&rpc).await, vec!["three", "two", "one"]);
}

#[tokio::test]
async fn small_delete_uses_the_dash_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    // A within-line `x` is a *small* delete → the `"-` register, not the ring.
    feed(&rpc, "0x");
    feed(&rpc, "$\"-p");
    assert_eq!(lines(&rpc).await, vec!["elloh"]);
}

#[tokio::test]
async fn yank_register_zero_survives_an_intervening_delete() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Yank fills `"0`; a later delete fills the ring/unnamed but never `"0`.
    feed(&rpc, "ggyy");
    feed(&rpc, "jdd");
    feed(&rpc, "\"0p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn black_hole_register_leaves_unnamed_intact() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    feed(&rpc, "ggyy");
    // `"_dd` discards "beta" without clobbering the unnamed register…
    feed(&rpc, "j\"_dd");
    // …so a plain paste still yields the yanked "alpha".
    feed(&rpc, "p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn register_carries_through_a_count_either_order() {
    // `"a3dd`: register before count.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>ofour<Esc>");
    feed(&rpc, "gg\"a3dd");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["four", "one", "two", "three"]);

    // `3"add`: count before register — same three-line capture.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>ofour<Esc>");
    feed(&rpc, "gg3\"add");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["four", "one", "two", "three"]);
}

#[tokio::test]
async fn paste_from_the_search_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello bar<Esc>");
    // The search sets `"/`; paste it onto a fresh line.
    feed(&rpc, "/bar<CR>");
    feed(&rpc, "o<Esc>\"/p");
    assert_eq!(lines(&rpc).await, vec!["hello bar", "bar"]);
}

#[tokio::test]
async fn paste_from_the_filename_register() {
    let path = temp_path("regname");
    std::fs::write(&path, "content\n").unwrap();
    let name = path.to_string_lossy().into_owned();
    let (rpc, _incoming) = start(Some(name.clone())).await;
    // `"%` is the current file name; paste it onto a new last line.
    feed(&rpc, "Go<Esc>\"%p");
    assert_eq!(lines(&rpc).await, vec!["content", name.as_str()]);
}

#[tokio::test]
async fn registers_command_lists_populated_registers() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "\"ayy");
    let map = latest_after(&rpc, &mut incoming, ":registers<CR>").await;

    assert_eq!(panel_title(&map), "Registers");
    let lines = panel_lines(&map);
    assert_eq!(lines.first().map(String::as_str), Some("Type Name Content"));
    // The linewise yank into `a` shows the `l` type and a trailing `^J`.
    assert!(
        lines
            .iter()
            .any(|l| l.contains("\"a") && l.contains("alpha^J")),
        "registers were: {lines:?}"
    );
}

#[tokio::test]
async fn registers_command_filters_by_argument() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "\"ayy\"byy");
    let map = latest_after(&rpc, &mut incoming, ":reg a<CR>").await;

    let lines = panel_lines(&map);
    assert!(lines.iter().any(|l| l.contains("\"a")), "want a: {lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("\"b")),
        "b should be filtered out: {lines:?}"
    );
}

#[tokio::test]
async fn read_only_register_refuses_a_delete() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    // `"/dd` targets the read-only search register — vim beeps and changes
    // nothing, so the buffer is untouched.
    feed(&rpc, "gg\"/dd");
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
}

// ---- Phase 4: the Lua register surface (setreg / getreg / getregtype) + :put ----

#[tokio::test]
async fn setreg_then_paste_round_trips() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // `setreg` fills register `a` from Lua; `"ap` pastes it back (charwise:
    // inserted after the cursor, which rests on the final `a` after `<Esc>`).
    feed(&rpc, ":lua vim.fn.setreg('a', 'hi')<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["alphahi"]);
}

#[tokio::test]
async fn setreg_linewise_option_pastes_as_a_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // The `l` flag makes the register linewise, so `"ap` opens a new line below.
    feed(&rpc, ":lua vim.fn.setreg('a', 'beta', 'l')<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn setreg_list_value_is_linewise() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // A list value is linewise, one item per line.
    feed(&rpc, ":lua vim.fn.setreg('a', {'one', 'two'})<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "one", "two"]);
}

#[tokio::test]
async fn setreg_append_flag_concatenates() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<Esc>");
    // The `a` flag appends to the register's current contents.
    feed(&rpc, ":lua vim.fn.setreg('a', 'foo')<CR>");
    feed(&rpc, ":lua vim.fn.setreg('a', 'bar', 'a')<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["xfoobar"]);
}

#[tokio::test]
async fn getreg_and_getregtype_read_the_core_register_file() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // A linewise yank into `a`; `getreg`/`getregtype` must read it from core.
    // Route the answer back through the buffer (trailing newline trimmed) so a
    // plain `lines()` assertion proves the read.
    feed(&rpc, "\"ayy");
    feed(
        &rpc,
        ":lua vim.api.nvim_buf_set_lines(0, 0, -1, false, \
         { vim.fn.getregtype('a') .. '|' .. (vim.fn.getreg('a'):gsub('%s+$', '')) })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["V|alpha"]);
}

#[tokio::test]
async fn getregtype_is_charwise_for_an_empty_register() {
    let (rpc, _incoming) = start(None).await;
    // An untouched register is charwise ("v"); its contents are "".
    feed(
        &rpc,
        ":lua vim.api.nvim_buf_set_lines(0, 0, -1, false, \
         { vim.fn.getregtype('z') .. '|' .. vim.fn.getreg('z') })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["v|"]);
}

#[tokio::test]
async fn getreg_reads_the_search_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello bar<Esc>");
    // The search sets `"/`; `getreg('/')` projects it like any read.
    feed(&rpc, "/bar<CR>");
    feed(
        &rpc,
        ":lua vim.api.nvim_buf_set_lines(0, 0, -1, false, { vim.fn.getreg('/') })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["bar"]);
}

#[tokio::test]
async fn setreg_rejects_a_read_only_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // Writing the read-only filename register `%` must raise, not silently
    // no-op: the pcall fails, so the buffer reads "ERR".
    feed(
        &rpc,
        ":lua local ok = pcall(vim.fn.setreg, '%', 'x'); \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, { ok and 'OK' or 'ERR' })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["ERR"]);
}

#[tokio::test]
async fn put_inserts_register_below_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Yank "alpha" into `a`, move to the top, then `:put a` drops it below line 1
    // as a whole line — even though the cursor sits mid-line.
    feed(&rpc, "gg\"ayy");
    feed(&rpc, ":put a<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha", "beta"]);
}

#[tokio::test]
async fn put_bang_inserts_above_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    feed(&rpc, "gg\"ayy");
    // `:put!` inserts above the addressed line instead of below.
    feed(&rpc, "G:put! a<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha", "beta"]);
}

#[tokio::test]
async fn put_of_a_charwise_register_is_still_linewise() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // A charwise register set from Lua; `:put` inserts it as a whole line, not
    // spliced into the current line.
    feed(&rpc, ":lua vim.fn.setreg('a', 'beta')<CR>");
    feed(&rpc, ":put a<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
}

/// The shipped `examples/registers/` config sources cleanly and its Lua
/// register surface actually drives core: the seeded `"h` / `"t` registers
/// paste, and the `:Stash` user command round-trips a line through `setreg` →
/// `:put`. Proves the example isn't just "loads" but works end-to-end.
#[tokio::test]
async fn registers_example_config_runs() {
    let dir = temp_dir("registers-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/registers/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(!msg.contains("Error"), "example left an error: {msg:?}");

    feed(&rpc, "ialpha<Esc>");
    // The seeded linewise list register `"t` pastes as its own two lines.
    feed(&rpc, ":put t<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "- buy milk", "- water plants"]
    );

    // `:Stash` writes the current line into `"s` via setreg; `:Stashed` reads it
    // back with getreg and puts it below — a full Lua round-trip through core.
    feed(&rpc, "gg:Stash<CR>");
    feed(&rpc, ":Stashed<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "alpha", "- buy milk", "- water plants"]
    );
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

#[tokio::test]
async fn capital_r_enters_replace_mode_and_overwrites() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello");
    feed(&rpc, "<Esc>");

    // `R` enters Replace mode: the status line reflects it...
    feed(&rpc, "0R");
    let _ = lines(&rpc).await; // barrier
    let view = latest_view(&mut incoming).expect("a redraw view");
    assert_eq!(view_str(&view, "mode_label"), "REPLACE");

    // ...and typed characters overwrite rather than insert.
    feed(&rpc, "HE");
    assert_eq!(lines(&rpc).await, vec!["HEllo"]);

    // Leaving Replace mode returns to normal.
    feed(&rpc, "<Esc>");
    let _ = lines(&rpc).await; // barrier
    let view = latest_view(&mut incoming).expect("a redraw view");
    assert_eq!(view_str(&view, "mode_label"), "NORMAL");
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
        .or_else(|| window0_field(view, key))
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
    // Cursor on 'x' at byte column 1; the leading tab puts it at the next tabstop
    // (the default tabstop is 4), screen col 4.
    assert_eq!(view_u64(&view, "cursor_col"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 4);
}

/// With `nowrap` (nxvim's only text-window mode today), a cursor driven past the
/// window's text width scrolls the viewport horizontally (`leftcol`) to keep the
/// cursor on screen, and scrolls all the way back at column 0 — vim's `w_leftcol`.
#[tokio::test]
async fn nowrap_scrolls_horizontally_to_keep_cursor_visible() {
    let (rpc, mut incoming) = start(None).await;
    // A line far wider than the 80-column window.
    feed(&rpc, "i");
    feed(&rpc, &"abcdefghij".repeat(20)); // 200 columns

    // At column 0 the window is not horizontally scrolled.
    let at_start = redraw_after(&rpc, &mut incoming, "<Esc>0").await;
    assert_eq!(view_u64(&at_start, "leftcol"), 0);
    let text_width = 80 - view_u64(&at_start, "number_width");

    // Jumping to end-of-line scrolls the viewport right to keep the cursor visible.
    let at_end = redraw_after(&rpc, &mut incoming, "$").await;
    let leftcol = view_u64(&at_end, "leftcol");
    let csc = view_u64(&at_end, "cursor_screen_col");
    assert!(leftcol > 0, "leftcol must advance for an off-screen cursor");
    assert!(
        csc >= leftcol && csc - leftcol < text_width,
        "cursor (screen col {csc}) must be visible within [{leftcol}, {leftcol}+{text_width})"
    );

    // Returning to column 0 scrolls the viewport all the way back.
    let back = redraw_after(&rpc, &mut incoming, "0").await;
    assert_eq!(view_u64(&back, "leftcol"), 0);
}

/// `sidescrolloff` keeps a margin of columns between the cursor and the window
/// edge while horizontally scrolling, mirroring vim's option.
#[tokio::test]
async fn sidescrolloff_keeps_a_margin_to_the_right_of_the_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i");
    feed(&rpc, &"abcdefghij".repeat(20)); // 200 columns
    feed(&rpc, "<Esc>:set sidescrolloff=8<CR>0");

    // Land the cursor mid-line, well past the right edge, scrolling right.
    let map = redraw_after(&rpc, &mut incoming, "120l").await;
    let leftcol = view_u64(&map, "leftcol");
    let csc = view_u64(&map, "cursor_screen_col");
    let text_width = 80 - view_u64(&map, "number_width");
    assert!(
        csc >= leftcol && csc - leftcol < text_width,
        "cursor must be visible"
    );
    // There is text beyond the cursor, so the 8-column right margin is preserved.
    let right_margin = text_width - (csc - leftcol) - 1;
    assert_eq!(
        right_margin, 8,
        "sidescrolloff keeps 8 columns right of the cursor"
    );
}

/// The horizontal-scroll options are queryable via `:set ss?` and settable via
/// `:set ss=N`, like any number option.
#[tokio::test]
async fn set_sidescroll_query_echoes_the_value() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set sidescroll?<CR>").await;
    assert_eq!(view_str(&map, "message"), "sidescroll=1");
    let map = redraw_after(&rpc, &mut incoming, ":set sidescroll=5<CR>:set ss?<CR>").await;
    assert_eq!(view_str(&map, "message"), "sidescroll=5");
}

/// The shipped `examples/horizontal-scroll/` config sources cleanly and actually
/// configures the editor (not just "loads"): its `:set sidescrolloff=8` takes
/// effect, observable through `:set siso?`.
#[tokio::test]
async fn horizontal_scroll_example_config_runs() {
    let dir = temp_dir("hscroll-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/horizontal-scroll/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(!msg.contains("Error"), "example left an error: {msg:?}");

    // The example's `vim.cmd("set sidescrolloff=8")` reached the core.
    let map = redraw_after(&rpc, &mut incoming, ":set siso?<CR>").await;
    assert_eq!(view_str(&map, "message"), "sidescrolloff=8");
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
    // A leading tab expands to the default tabstop (4), so 'x' sits at screen
    // column 4 even though it is byte 1. Vertical motion must map that screen
    // column onto the ASCII line below (where byte == screen column).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i<Tab>x<Esc>"); // line 1: "\tx"
    feed(&rpc, "oabcdefghij<Esc>"); // line 2: ASCII
    feed(&rpc, "ggl"); // line 1, onto 'x' at byte 1 / screen col 4
    assert_eq!(cursor(&rpc).await, (1, 1));
    feed(&rpc, "j"); // down: screen col 4 → byte 4 ('e')
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "k"); // back up: screen col 4 → byte 1 ('x')
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

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;

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
async fn page_down_acts_like_ctrl_d() {
    let path = write_n_lines("pgdn", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "<PageDown>").await;

    // Identical to <C-d>: viewport height 24 → half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 12);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
}

#[tokio::test]
async fn page_up_acts_like_ctrl_u() {
    let path = write_n_lines("pgup", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Scroll down a full page first so there's room to scroll back up.
    let _ = redraw_after(&rpc, &mut incoming, "<C-f>").await; // top 0 -> 22
    let map = scroll_after(&rpc, &mut incoming, "<PageUp>").await; // top 22 -> 10

    // Identical to <C-u>: half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 22);
    assert_eq!(scroll_u64(&map, "to_top"), 10);
    assert_eq!(scroll_u64(&map, "from_cursor"), 22);
    assert_eq!(scroll_u64(&map, "to_cursor"), 10);
}

#[tokio::test]
async fn ctrl_f_emits_full_page_scroll() {
    let path = write_n_lines("cf", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "<C-f>").await;

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

    let map = scroll_after(&rpc, &mut incoming, "<C-f>").await;

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
    let map = scroll_after(&rpc, &mut incoming, "<C-u>").await; // top 22 -> 10

    assert_eq!(scroll_u64(&map, "from_top"), 22);
    assert_eq!(scroll_u64(&map, "to_top"), 10);
    assert_eq!(scroll_u64(&map, "from_cursor"), 22);
    assert_eq!(scroll_u64(&map, "to_cursor"), 10);
    assert_eq!(scroll_u64(&map, "base_line"), 10); // min(from, to)
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8
    assert_eq!(scroll_lines_len(&map), 36); // |22 - 10| + 24
}

#[tokio::test]
async fn ctrl_e_scrolls_one_line_keeping_the_cursor_line() {
    let path = write_n_lines("ce", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "10G"); // cursor mid-screen (line 10), top still 0
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    // The window scrolled down one line, but the cursor held its buffer line —
    // the defining difference from <C-d>, which drags the cursor with the view.
    assert_eq!(first_visible_line(&map), "line2", "top moved down one line");
    assert_eq!(cursor(&rpc).await, (10, 0), "cursor stayed on its line");
}

#[tokio::test]
async fn ctrl_e_pulls_the_cursor_when_it_would_scroll_off_top() {
    let path = write_n_lines("ce_pull", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "gg"); // cursor on line 1 — the top visible row
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    // Scrolling down one line would push line 1 off the top, so the cursor is
    // pulled to the new top line (scrolloff is 0).
    assert_eq!(first_visible_line(&map), "line2");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "cursor pulled to the new top line"
    );
}

#[tokio::test]
async fn ctrl_y_scrolls_one_line_up_keeping_the_cursor_line() {
    let path = write_n_lines("cy", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "<C-f>"); // top 0 -> 22, cursor lands on line 23 (top row)
    let map = redraw_after(&rpc, &mut incoming, "<C-y>").await;

    // View scrolls back up one line; the cursor (now one row down) holds line 23.
    assert_eq!(first_visible_line(&map), "line22", "top moved up one line");
    assert_eq!(cursor(&rpc).await, (23, 0), "cursor stayed on its line");
}

#[tokio::test]
async fn ctrl_y_at_the_top_does_nothing() {
    let path = write_n_lines("cy_top", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "gg");
    let map = redraw_after(&rpc, &mut incoming, "<C-y>").await;

    assert!(scroll(&map).is_none(), "no viewport movement at the top");
    assert_eq!(first_visible_line(&map), "line1");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn count_motion_emits_scroll() {
    let path = write_n_lines("count_j", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `30j` lands the cursor on line 30; ensure_visible drags top to 30+1-24 = 7.
    let map = scroll_after(&rpc, &mut incoming, "30j").await;

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
    let map = scroll_after(&rpc, &mut incoming, "G").await;

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
    let map = scroll_after(&rpc, &mut incoming, "gg").await; // ...then back to the top

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

// ----- vim.* surface the lsp/<server>.lua configs reach for --------------------
// nvim-lspconfig's `lsp/rust_analyzer.lua` (loaded by `vim.lsp.enable`) calls
// vim.tbl_get / vim.fs.relpath / vim.system / vim.json / vim.lsp.get_clients in
// its `root_dir`; before these existed, enabling it raised
// "attempt to call field 'tbl_get' (a nil value)". These cover the surface.

#[tokio::test]
async fn vim_tbl_get_follows_a_nested_key_path() {
    let dir = temp_dir("tblget");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local t = { a = { b = { c = 42 } } }\n\
         print(tostring(vim.tbl_get(t, 'a', 'b', 'c')) .. ' '\n\
           .. tostring(vim.tbl_get(t, 'a', 'x', 'c')) .. ' '\n\
           .. tostring(vim.tbl_get(t, 'a', 'b', 'c', 'd')))\n",
    )
    .await;
    // Present path -> value; a missing intermediate key -> nil; descending past a
    // scalar (c is 42, not a table) -> nil rather than an error.
    assert_eq!(startup_message(&rpc, &mut incoming).await, "42 nil nil");
}

#[tokio::test]
async fn vim_fs_relpath_is_segment_aware() {
    let dir = temp_dir("relpath");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "print(vim.fs.relpath('/a/b', '/a/b/c/d') .. ' '\n\
           .. tostring(vim.fs.relpath('/a/b', '/a/bc')) .. ' '\n\
           .. vim.fs.relpath('/a/b', '/a/b'))\n",
    )
    .await;
    // Subpath -> relative remainder; "/a/bc" is NOT under "/a/b" (segment
    // boundary) -> nil; an equal path -> ".".
    assert_eq!(startup_message(&rpc, &mut incoming).await, "c/d nil .");
}

#[tokio::test]
async fn vim_json_decodes_and_encodes() {
    let dir = temp_dir("vimjson");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local d = vim.json.decode('{\"workspace_root\":\"/w\",\"n\":3,\"arr\":[10,20]}')\n\
         print(d.workspace_root .. ' ' .. d.n .. ' ' .. d.arr[2] .. ' ' .. vim.json.encode({ a = 1 }))\n",
    )
    .await;
    // Object -> string-keyed table, array -> 1-based sequence; encode emits an
    // object for a non-sequence table.
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "/w 3 20 {\"a\":1}"
    );
}

#[tokio::test]
async fn vim_lsp_get_clients_starts_empty() {
    let dir = temp_dir("getclients");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "print(#vim.lsp.get_clients() .. ' ' .. #vim.lsp.get_clients({ name = 'nope' }))\n",
    )
    .await;
    // No server has attached, so the list is empty with and without a filter.
    assert_eq!(startup_message(&rpc, &mut incoming).await, "0 0");
}

#[cfg(unix)]
#[tokio::test]
async fn lspconfig_style_root_dir_resolves_through_the_new_surface() {
    // A miniature `lsp/<name>.lua` shaped exactly like rust_analyzer's: its
    // `root_dir` reaches for vim.tbl_get, vim.system (shelling out), vim.json,
    // vim.fs.relpath and vim.lsp.get_clients. Driven through `vim.lsp.enable` +
    // the FileType dispatcher, the whole config must evaluate without a Lua error
    // — the regression the user hit ("attempt to call field 'tbl_get'").
    let dir = temp_dir("lspprobe");
    std::fs::create_dir_all(dir.join("lsp")).expect("create lsp dir");
    std::fs::write(
        dir.join("lsp").join("probe.lua"),
        // root_dir deliberately does NOT call on_dir: this asserts the API
        // surface a config evaluates, not the (separately covered) server spawn.
        r#"return {
  cmd = { 'true' },
  filetypes = { 'probe' },
  root_dir = function(bufnr, on_dir)
    local deep = vim.tbl_get(vim.lsp.config['probe'], 'settings', 'probe', 'missing')
    local res = vim.system({ '/bin/echo', '{"workspace_root":"/tmp/proj","n":2}' }, { text = true }):wait()
    local decoded = vim.json.decode(res.stdout)
    local rel = vim.fs.relpath('/a/b', '/a/x')
    local nclients = #vim.lsp.get_clients({ name = 'probe' })
    print(string.format('probe %s %s %d %s %d',
      decoded.workspace_root, tostring(deep), decoded.n, tostring(rel), nclients))
  end,
}
"#,
    )
    .expect("write lsp config");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.lsp.enable('probe')\nvim.lsp._on_filetype(0, 'probe')\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "probe /tmp/proj nil 2 nil 0",
        "an lspconfig-style root_dir should evaluate the new vim.* surface cleanly"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn host_primitives_for_lspconfig_are_available() {
    // The libuv/process/version surface the configs build defaults from. Bundled
    // into one assertion: cwd is resolvable, getpid is positive, a ubiquitous
    // binary (`sh`) is executable, vim.version() stringifies, vim.trim trims,
    // vim.empty_dict is empty, and the vim.lsp.rpc.start shim hands a cmd builder
    // back its argv (the mechanism behind the 20-plus rpc.start configs).
    let dir = temp_dir("hostprim");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local parts = {\n\
         \x20 tostring(vim.uv.cwd() ~= nil),\n\
         \x20 tostring(vim.fn.getpid() > 0),\n\
         \x20 tostring(vim.fn.executable('sh') == 1),\n\
         \x20 tostring(vim.version()),\n\
         \x20 vim.trim('  hi  '),\n\
         \x20 tostring(next(vim.empty_dict()) == nil),\n\
         \x20 table.concat(vim.lsp.rpc.start({ 'mybin', '--stdio' }, {}), ','),\n\
         }\n\
         print(table.concat(parts, ' '))\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "true true true 0.11.0 hi true mybin,--stdio"
    );
}

#[tokio::test]
async fn vim_iter_handles_iterators_and_find_any() {
    // vim.iter must accept a stateless iterator triple (what vim.fs.parents
    // returns) — the fennel_ls/vala_ls root_dir pattern — and expose :find/:any.
    let dir = temp_dir("vimiter");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local p = vim.iter(vim.fs.parents('/a/b/c')):totable()\n\
         print(p[1] .. ' ' .. p[2] .. ' '\n\
           .. tostring(vim.iter({ 10, 20, 30 }):find(function(x) return x == 20 end)) .. ' '\n\
           .. tostring(vim.iter({ 1, 2, 3 }):any(function(x) return x > 2 end)))\n",
    )
    .await;
    // Ancestors of /a/b/c are /a/b, /a, … (the walk stops at /, not the cwd).
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "/a/b /a 20 true"
    );
}

#[tokio::test]
async fn vim_fs_root_resolves_priority_tiers() {
    // vim.fs.root treats a list marker as an ordered priority chain (neovim 0.11):
    // the highest-priority tier with a match anywhere up the tree wins regardless
    // of depth, and a nested list is an equal-priority tier. Lay out a tree and
    // check both the ordered-beats-proximity rule and nested-tier matching.
    let dir = temp_dir("fsroot");
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join("sub").join("deep")).expect("mkdir tree");
    std::fs::write(proj.join("low"), "").expect("low marker"); // high up
    std::fs::write(proj.join("sub").join("g1"), "").expect("g1 marker"); // closer
    let src = proj.join("sub").join("deep").join("src.txt");
    let p = proj.to_string_lossy();

    // marker1: prefer 'top' (absent), then the equal-priority {g1,g2} tier (g1 is
    // at proj/sub) -> proj/sub. marker2: 'low' tier first (at proj) beats the
    // closer 'g1' (at proj/sub) -> proj.
    let init = format!(
        "print(vim.fs.root('{src}', {{ 'top', {{ 'g1', 'g2' }}, 'low' }}) .. ' | '\n\
         \x20 .. vim.fs.root('{src}', {{ 'low', 'g1' }}))\n",
        src = src.to_string_lossy()
    );
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        format!("{p}/sub | {p}")
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
async fn panelopen_reopens_the_last_panel() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":lua print('alpha')<CR>");
    feed(&rpc, ":lua print('beta')<CR>");

    // Open the messages panel, then close it.
    let map = latest_after(&rpc, &mut incoming, ":messages<CR>").await;
    let opened = panel_lines(&map);
    assert!(opened.contains(&"alpha".to_string()));
    let map = latest_after(&rpc, &mut incoming, "q").await;
    assert!(panel(&map).is_none(), "q closed the panel");

    // `:panelopen` brings the same panel back with identical title and content.
    let map = latest_after(&rpc, &mut incoming, ":panelopen<CR>").await;
    assert_eq!(panel_title(&map), "Messages", "the last panel reopens");
    assert_eq!(
        panel_lines(&map),
        opened,
        "reopened with the same content it had"
    );
}

#[tokio::test]
async fn panelopen_with_no_prior_panel_reports_nothing() {
    let (rpc, mut incoming) = start(None).await;
    // Nothing has ever been shown in a panel.
    let map = latest_after(&rpc, &mut incoming, ":panelopen<CR>").await;
    assert!(panel(&map).is_none(), "no panel to reopen, so none opens");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("No panel to reopen"),
    );
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
async fn clicking_a_panel_row_selects_the_wrapped_entry() {
    // The mouse path: the client maps a click to a content display row and sends
    // `nxvim_panel_click(row)`. The panel word-wraps (width 80), so a display row
    // must map back to its logical entry — the second half of a wrapped entry
    // selects that whole entry, not the next one.
    let (rpc, mut incoming) = start(None).await;
    let long = "x".repeat(100); // wraps to two display rows at width 80
    let content = Value::Array(vec![
        Value::from("aaa"),
        Value::from(long.as_str()),
        Value::from("ccc"),
    ]);
    rpc.notify(
        "nxvim_panel_open",
        vec![
            Value::from("Picks"),
            content,
            Value::from(false),
            Value::from(0u64),
        ],
    );
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 0, "opens on the first entry");

    // Display rows: 0="aaa", 1..2=wrapped long entry, 3="ccc". Clicking row 2 (the
    // second half of the wrapped entry) selects that entry: its first row is 1 and
    // it spans 2 rows.
    rpc.notify("nxvim_panel_click", vec![Value::from(2u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(
        panel_u64(&map, "cursor_row"),
        1,
        "the wrapped entry is selected"
    );
    assert_eq!(
        panel_u64(&map, "cursor_span"),
        2,
        "its whole span is focused"
    );

    // Clicking row 3 lands on the entry past the wrap (a single-row entry).
    rpc.notify("nxvim_panel_click", vec![Value::from(3u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 3);
    assert_eq!(panel_u64(&map, "cursor_span"), 1);

    // A row past the content clamps to the last entry, never wrapping around.
    rpc.notify("nxvim_panel_click", vec![Value::from(99u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 3, "clamps to the last entry");
}

#[tokio::test]
async fn clicking_the_selected_panel_row_activates_it() {
    // Select-then-confirm: the first click selects a row (`nxvim_panel_click`),
    // and a click on the already-selected row activates it — which the client
    // sends as `<CR>`. On a select-enabled panel that emits `nxvim_panel_select`.
    let (rpc, mut incoming) = start(None).await;
    let content = Value::Array(vec![
        Value::from("one"),
        Value::from("two"),
        Value::from("three"),
    ]);
    rpc.notify(
        "nxvim_panel_open",
        vec![
            Value::from("Picks"),
            content,
            Value::from(true), // wants_select
            Value::from(0u64),
        ],
    );
    drain_latest(&rpc, &mut incoming).await;

    // Click row 2 to select "three".
    rpc.notify("nxvim_panel_click", vec![Value::from(2u64)]);
    let map = drain_latest(&rpc, &mut incoming).await;
    assert_eq!(panel_u64(&map, "cursor_row"), 2);

    // The client sends <CR> for a click on the already-selected row; the server
    // emits a select event for that entry (1-based index, line text).
    rpc.notify("nvim_input", vec![Value::from("<CR>")]);
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    tokio::task::yield_now().await;
    let mut selected = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "nxvim_panel_select" {
            selected = params.into_iter().next();
        }
    }
    let Some(Value::Map(sel)) = selected else {
        panic!("no nxvim_panel_select notification arrived");
    };
    assert_eq!(field(&sel, "index").and_then(Value::as_u64), Some(3));
    assert_eq!(field(&sel, "line").and_then(Value::as_str), Some("three"));
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

// ----- vim.fn.substitute (vim-regex compatibility) --------------------------

/// `vim.fn.substitute(input, pat, sub, flags)` via the live VM. `pat`/`sub` ride
/// Lua long-bracket literals so vim backslashes pass through unescaped.
async fn substitute(rpc: &Rpc, input: &str, pat: &str, sub: &str, flags: &str) -> String {
    let code =
        format!("return vim.fn.substitute({input:?}, [==[{pat}]==], [==[{sub}]==], {flags:?})");
    exec_lua(rpc, &code)
        .await
        .as_str()
        .unwrap_or("<not a string>")
        .to_string()
}

#[tokio::test]
async fn substitute_matches_vim_magic_semantics() {
    let (rpc, _incoming) = start(None).await;

    // Literal `\.` (magic: bare `.` is the wildcard, `\.` the literal), global.
    assert_eq!(substitute(&rpc, "a.b.c", r"\.", "/", "g").await, "a/b/c");
    // Bare `.` IS the wildcard in magic — first match only without `g`.
    assert_eq!(substitute(&rpc, "abc", ".", "X", "").await, "Xbc");
    // Escaped backslash → a literal backslash (Windows-path normalisation).
    assert_eq!(substitute(&rpc, r"a\b\c", r"\\", "/", "g").await, "a/b/c");
    // `\(\)` groups + `\+` one-or-more; `\1` in the replacement.
    assert_eq!(
        substitute(&rpc, "hello", r"\(l\+\)", r"[\1]", "").await,
        "he[ll]o"
    );
    // `&` is the whole match.
    assert_eq!(substitute(&rpc, "cat", "a", "[&]", "").await, "c[a]t");
    // `[^=]\+=` — a magic char class with `\+`.
    assert_eq!(substitute(&rpc, "VAR=val", r"[^=]\+=", "", "").await, "val");
    // POSIX class inside `[]`.
    assert_eq!(
        substitute(&rpc, "  hi  ", r"^[[:space:]]*", "", "").await,
        "hi  "
    );
}

#[tokio::test]
async fn substitute_handles_non_greedy_groups_and_anchors() {
    let (rpc, _incoming) = start(None).await;

    // The lspconfig `strip_archive_subpath` shape: `.\{-}` is non-greedy, so the
    // group stops at the FIRST `::`, not the last.
    assert_eq!(
        substitute(
            &rpc,
            "zipfile:///path/to/a::b::c",
            r"zipfile://\(.\{-}\)::.*$",
            r"\1",
            ""
        )
        .await,
        "/path/to/a"
    );
    // `$` anchors to the end; `^` to the start.
    assert_eq!(
        substitute(&rpc, "foobar", r"bar$", "BAZ", "").await,
        "fooBAZ"
    );
    assert_eq!(substitute(&rpc, "foofoo", r"^foo", "X", "g").await, "Xfoo");
}

#[tokio::test]
async fn substitute_very_magic_and_case_modifiers() {
    let (rpc, _incoming) = start(None).await;

    // `\v` very magic: bare `\d` class, `+`/`(` operators without backslashes.
    assert_eq!(
        substitute(&rpc, "a1b22c", r"\v\d+", "#", "g").await,
        "a#b#c"
    );
    assert_eq!(
        substitute(&rpc, "key: val", r"\v(\w+): (\w+)", r"\2=\1", "").await,
        "val=key"
    );
    // `\u&` upper-cases the first letter of each match (Title Case).
    assert_eq!(
        substitute(&rpc, "hello world", r"\w\+", r"\u&", "g").await,
        "Hello World"
    );
    // `\U…\E` upper-cases a span.
    assert_eq!(
        substitute(&rpc, "abc", r"\(b\)", r"\U\1\E", "").await,
        "aBc"
    );
    // The `i` flag folds case for matching.
    assert_eq!(substitute(&rpc, "FoO", "o", "0", "gi").await, "F00");
}

#[tokio::test]
async fn substitute_fails_loud_on_unsupported_constructs() {
    let (rpc, _incoming) = start(None).await;
    // `\zs` has no RE2 equivalent — it must raise (named), not silently mis-match.
    // pcall captures the error so we can assert on its message.
    let err = exec_lua(
        &rpc,
        r"local ok, e = pcall(vim.fn.substitute, 'xy', [==[x\zsy]==], 'z', '')
          return tostring(e)",
    )
    .await;
    let err = err.as_str().expect("a string error from the raise");
    assert!(
        err.contains("substitute") && err.contains(r"\zs"),
        "the error names the unsupported construct (fail loud): {err:?}"
    );
}

// ----- vim.ui.select / vim.ui.input (Phase 8) -------------------------------

#[tokio::test]
async fn vim_ui_select_routes_the_pick_to_on_choice() {
    let (rpc, mut incoming) = start(None).await;
    // `vim.ui.select` lists the choices in the panel; a `<CR>` on the focused row
    // hands the item + 1-based index to `on_choice`, which echoes them so we can
    // observe the pick.
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.ui.select({'alpha', 'beta'}, { prompt = 'Pick:' }, \
             function(item, idx) print('chose:' .. item .. ':' .. idx) end)",
        )],
    )
    .await
    .expect("select");
    drain_latest(&rpc, &mut incoming).await;

    // Move to the second row (the panel has focus) and pick it.
    let map = latest_after(&rpc, &mut incoming, "j<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("chose:beta:2"),
        "on_choice(item, index) fires for the focused row"
    );
}

#[tokio::test]
async fn vim_ui_select_format_item_renders_the_rows() {
    let (rpc, mut incoming) = start(None).await;
    // `opts.format_item` controls the displayed text while `on_choice` still
    // receives the original item — here items are tables rendered by `.label`.
    rpc.request(
        "nvim_command",
        vec![Value::from(
            "lua vim.ui.select({ { label = 'One', id = 11 }, { label = 'Two', id = 22 } }, \
             { format_item = function(it) return it.label end }, \
             function(item) print('id:' .. item.id) end)",
        )],
    )
    .await
    .expect("select");
    let map = drain_latest(&rpc, &mut incoming).await;
    // The panel shows the formatted labels, not the raw tables.
    assert_eq!(
        panel_lines(&map),
        vec!["One".to_string(), "Two".to_string()]
    );

    // Picking the first row hands the original table to on_choice.
    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("id:11"),
        "on_choice receives the original item, not the formatted string"
    );
}

#[tokio::test]
async fn vim_ui_input_hands_the_typed_line_to_on_confirm() {
    let (rpc, mut incoming) = start(None).await;
    // `vim.ui.input` opens a command-line prompt; the typed text reaches
    // `on_confirm` on `<CR>`.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.ui.input({ prompt = 'Name: ' }, function(t) print('got:' .. tostring(t)) end)<CR>",
    )
    .await;
    // The prompt is open: command mode, with the label projected for the client.
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Name: "),
        "the input label is projected into the redraw"
    );

    // Type a line and submit: the callback fires with the text.
    let map = latest_after(&rpc, &mut incoming, "Bob<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:Bob")
    );
    // The prompt closed — back to normal mode.
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(false)
    );
}

#[tokio::test]
async fn vim_ui_input_default_prefills_and_is_editable() {
    let (rpc, mut incoming) = start(None).await;
    // `opts.default` prefills the line; the user edits it before submitting.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.ui.input({ prompt = 'Q: ', default = 'foo' }, \
         function(t) print('got:' .. tostring(t)) end)<CR>",
    )
    .await;
    // Append "bar" to the prefilled "foo" and submit.
    let map = latest_after(&rpc, &mut incoming, "bar<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:foobar"),
        "the default is prefilled and editable"
    );
}

#[tokio::test]
async fn vim_ui_input_cancel_hands_nil() {
    let (rpc, mut incoming) = start(None).await;
    // Cancelling the prompt (`<Esc>`) delivers `nil`, matching neovim's
    // `on_confirm(nil)`.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.ui.input({ prompt = 'Name: ' }, function(t) print('got:' .. tostring(t)) end)<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:nil"),
        "a cancelled input hands the callback nil"
    );
}

#[tokio::test]
async fn phase8_example_config_drives_select_and_input() {
    // The shipped `examples/phase8-ui` config sources cleanly and its keymaps
    // actually drive the vim.ui surfaces end-to-end (not just "loads").
    let dir = temp_dir("phase8-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/phase8-ui/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    // Startup is clean (no E5108 load error left on the message line).
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        !msg.contains("Error"),
        "example config left an error: {msg:?}"
    );

    // `<Space>s` opens the fruit picker; pick the second row.
    drain_latest(&rpc, &mut incoming).await;
    feed(&rpc, " s");
    drain_latest(&rpc, &mut incoming).await;
    let map = latest_after(&rpc, &mut incoming, "j<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("you picked: banana (row 2)"),
        "the example's vim.ui.select keymap works"
    );

    // `<Space>i` opens the name prompt (prefilled "anon"); append and submit.
    let map = latest_after(&rpc, &mut incoming, " i").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Your name: ")
    );
    let map = latest_after(&rpc, &mut incoming, "X<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hello, anonX!"),
        "the example's vim.ui.input keymap works"
    );
}

// ----- vim.fn.input / vim.fn.confirm (synchronous prompts) ------------------
//
// Unlike the async `vim.ui.input` (callback) surface above, these *block* the
// calling Lua chunk and return the answer inline: `input` returns the typed
// string (`""` on cancel), `confirm` a 1-based button index (`0` on cancel).
// They are driven through a coroutine the entry point (`:lua`, a keymap, a user
// command) runs the chunk inside, so a `coroutine.yield` parks the chunk on the
// command-line prompt and the prompt result resumes it. Tests open the prompt
// with a `:lua …<CR>` trigger (a notification, so it never deadlocks an RPC
// reply), feed the answer, and observe the inline result via `print`.

#[tokio::test]
async fn vim_fn_input_returns_typed_text() {
    let (rpc, mut incoming) = start(None).await;
    // The chunk blocks on `vim.fn.input`; the prompt opens and the chunk parks.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input('Name: '))<CR>",
    )
    .await;
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true),
        "the prompt opens (command mode) while the chunk is parked"
    );
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Name: "),
        "the input label is projected into the redraw"
    );
    // Typing the answer and submitting resumes the parked chunk, which returns
    // the line inline and prints it.
    let map = latest_after(&rpc, &mut incoming, "Bob<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:Bob"),
        "vim.fn.input returns the typed line inline"
    );
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(false),
        "the prompt closed once the answer was submitted"
    );
}

#[tokio::test]
async fn vim_fn_input_esc_returns_empty_string() {
    let (rpc, mut incoming) = start(None).await;
    // Cancelling `vim.fn.input` returns "" (an empty string), NOT nil — the key
    // contract difference from `vim.ui.input`, which hands its callback nil.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got[' .. vim.fn.input('Name: ') .. ']')<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got[]"),
        "a cancelled vim.fn.input returns an empty string"
    );
}

#[tokio::test]
async fn vim_fn_input_default_prefills_and_is_editable() {
    let (rpc, mut incoming) = start(None).await;
    // The positional `(prompt, default)` form prefills the line; the user edits
    // it before submitting.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input('Q: ', 'foo'))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "bar<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:foobar"),
        "the default is prefilled and editable"
    );
}

#[tokio::test]
async fn vim_fn_input_accepts_table_opts() {
    let (rpc, mut incoming) = start(None).await;
    // The neovim `vim.fn.input({ prompt = …, default = … })` table form.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input({ prompt = 'P: ', default = 'x' }))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("got:x"),
        "the table form's prompt/default are honored"
    );
}

#[tokio::test]
async fn vim_fn_input_works_from_a_keymap_callback() {
    let (rpc, mut incoming) = start(None).await;
    // A keymap RHS is also a pumped entry: a mapping that calls vim.fn.input can
    // block and use the answer.
    rpc.request(
        "nvim_exec_lua",
        vec![
            Value::from(
                "vim.keymap.set('n', '<Space>n', function() \
                   print('hi ' .. vim.fn.input('who? ')) end)",
            ),
            Value::Array(vec![]),
        ],
    )
    .await
    .expect("set keymap");
    let map = latest_after(&rpc, &mut incoming, " n").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("who? "),
        "the keymap's vim.fn.input opens its prompt"
    );
    let map = latest_after(&rpc, &mut incoming, "Sam<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hi Sam"),
        "the keymap callback gets the inline answer"
    );
}

#[tokio::test]
async fn editor_is_responsive_after_an_input_prompt() {
    let (rpc, mut incoming) = start(None).await;
    // Resolving a prompt must leave the editor cleanly back in normal mode — no
    // residual command-line state.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('got:' .. vim.fn.input('X: '))<CR>",
    )
    .await;
    feed(&rpc, "answer<CR>");
    // Normal editing works immediately afterward.
    feed(&rpc, "ihello world<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn vim_fn_input_outside_a_pumped_context_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    // A scheduled callback runs outside the coroutine-pumped entry path, so a
    // blocking prompt there cannot suspend. It must fail loud (E5108), never
    // hang the editor or fabricate a value.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua vim.schedule(function() vim.fn.input('X: ') end)<CR>",
    )
    .await;
    let msg = field(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        msg.contains("E5108") && msg.contains("input"),
        "a blocking prompt outside a pumped context fails loud: {msg:?}"
    );
}

#[tokio::test]
async fn vim_fn_confirm_accelerator_key_picks_the_button() {
    let (rpc, mut incoming) = start(None).await;
    // `confirm` lists the buttons and resolves on a single accelerator keypress
    // (the char after `&`), returning that button's 1-based index.
    let map = latest_after(
        &rpc,
        &mut incoming,
        ":lua print('c=' .. vim.fn.confirm('Save?', '&Yes\\n&No'))<CR>",
    )
    .await;
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Save? [Y]es, [N]o: "),
        "the confirm message and buttons are projected"
    );
    let map = latest_after(&rpc, &mut incoming, "n").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("c=2"),
        "pressing the 'N' accelerator returns button 2"
    );
}

#[tokio::test]
async fn vim_fn_confirm_enter_picks_the_default() {
    let (rpc, mut incoming) = start(None).await;
    // `<CR>` resolves to the default button (the 3rd arg, 1-based).
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('c=' .. vim.fn.confirm('Q', '&Yes\\n&No', 1))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("c=1"),
        "Enter selects the default button"
    );
}

#[tokio::test]
async fn vim_fn_confirm_esc_returns_zero() {
    let (rpc, mut incoming) = start(None).await;
    // Cancelling (`<Esc>`) returns 0.
    latest_after(
        &rpc,
        &mut incoming,
        ":lua print('c=' .. vim.fn.confirm('Q', '&Yes\\n&No', 1))<CR>",
    )
    .await;
    let map = latest_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("c=0"),
        "a cancelled confirm returns 0"
    );
}

#[tokio::test]
async fn sync_prompts_example_config_drives_input_and_confirm() {
    // The shipped `examples/sync-prompts` config sources cleanly and its keymaps
    // actually drive vim.fn.input / vim.fn.confirm end-to-end (not just "loads").
    let dir = temp_dir("sync-prompts-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/sync-prompts/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        !msg.contains("Error"),
        "example config left an error: {msg:?}"
    );

    // `<Space>i` opens the name prompt (prefilled "anon"); append and submit.
    let map = latest_after(&rpc, &mut incoming, " i").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Your name: ")
    );
    let map = latest_after(&rpc, &mut incoming, "X<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hello, anonX!"),
        "the example's vim.fn.input keymap returns and uses the typed line"
    );

    // Put a line in the buffer, then `<Space>d` → the confirm dialog → 'y' deletes
    // it (proving confirm's single-key accept and the inline return both work).
    feed(&rpc, "ineedle<Esc>");
    assert_eq!(lines(&rpc).await, vec!["needle"]);
    let map = latest_after(&rpc, &mut incoming, " d").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Delete the line? [Y]es, [N]o, [C]ancel: "),
        "the example's vim.fn.confirm dialog renders its buttons"
    );
    let map = latest_after(&rpc, &mut incoming, "y").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("deleted")
    );
    assert_eq!(lines(&rpc).await, vec![""], "Yes deleted the line");

    // `<Space>r` chains input() THEN confirm() in one body — two yields on the
    // same coroutine. Type the new text, submit, then confirm: proves a nested
    // prompt re-parks and resumes the same blocked call cleanly.
    feed(&rpc, "iold<Esc>");
    let map = latest_after(&rpc, &mut incoming, " r").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("New text: ")
    );
    // Submitting the input opens the SECOND (confirm) prompt from the same body.
    let map = latest_after(&rpc, &mut incoming, "new<CR>").await;
    assert_eq!(
        field(&map, "cmdline_prompt").and_then(Value::as_str),
        Some("Replace this line? [Y]es, [N]o: "),
        "input() resolved and confirm() opened in the same keymap body"
    );
    let map = latest_after(&rpc, &mut incoming, "y").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("replaced")
    );
    assert_eq!(lines(&rpc).await, vec!["new"], "the chained rename applied");
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

#[tokio::test]
async fn command_history_recalls_previous_commands() {
    // `:<Up>` walks back through previously-submitted ex commands (newest first),
    // replacing the typed line — the ex-command analogue of search history.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set number<CR>");
    feed(&rpc, ":set nonumber<CR>");
    let _ = lines(&rpc).await; // barrier before capturing
    let map = redraw_after(&rpc, &mut incoming, ":<Up>").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("set nonumber")
    );
    assert_eq!(
        field(&map, "cmdline_prefix").and_then(Value::as_str),
        Some(":")
    );
    // A second <Up> (still in the open prompt) reaches the older command.
    let map = redraw_after(&rpc, &mut incoming, "<Up>").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("set number")
    );
}

#[tokio::test]
async fn cmdline_left_arrow_inserts_mid_line() {
    // <Left> backs the command cursor over one char; typing then inserts there
    // rather than at the end. ":abc" + <Left> + "X" → "abXc".
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":abc<Left>X").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("abXc"));
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(3)
    );
}

#[tokio::test]
async fn cmdline_backspace_and_delete_act_at_the_cursor() {
    let (rpc, mut incoming) = start(None).await;
    // <Left> puts the cursor between b and c; <BS> removes the char before it (b).
    let map = redraw_after(&rpc, &mut incoming, ":abc<Left><BS>").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("ac"));
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(1)
    );
    // Fresh line: Home then <Del> removes the char under the cursor (the first).
    let map = redraw_after(&rpc, &mut incoming, "<Esc>:abc<Home><Del>").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("bc"));
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(0)
    );
}

#[tokio::test]
async fn cmdline_home_and_end_jump_to_the_ends() {
    let (rpc, mut incoming) = start(None).await;
    // Home sends the cursor to the start; inserting prepends.
    let map = redraw_after(&rpc, &mut incoming, ":abc<Home>X").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("Xabc"));
    // End jumps back to the tail; inserting appends.
    let map = redraw_after(&rpc, &mut incoming, "<End>Y").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("XabcY")
    );
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(5)
    );
}

#[tokio::test]
async fn cmdline_mid_line_edit_changes_the_executed_command() {
    // The point of in-line editing: fix a command before running it. Backing up
    // and inserting the missing space turns ":setnumber" into ":set number",
    // which enables the number option observably.
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(
        &rpc,
        &mut incoming,
        ":setnumber<Left><Left><Left><Left><Left><Left><Space><CR>",
    )
    .await;
    assert!(
        field(&map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "inserting a space mid-line should run :set number, enabling the option"
    );
}

#[tokio::test]
async fn command_history_up_arrow_reruns_last_command() {
    // The workflow that matters: open `:`, press <Up> to recall the last command,
    // <CR> to rerun it. Here recalling and submitting `:set number` re-enables the
    // number option observably in the redraw.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set number<CR>");
    feed(&rpc, ":set nonumber<CR>");
    let _ = lines(&rpc).await; // barrier
    let map = redraw_after(&rpc, &mut incoming, ":<Up><Up><CR>").await;
    assert!(
        field(&map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "rerunning :set number from history should re-enable the number option"
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

// ----- f/t/F/T find-char motions -------------------------------------------

#[tokio::test]
async fn f_moves_onto_the_target_char() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0fo");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn f_with_a_count_finds_the_nth_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // 'l' is at columns 2, 3, 9; the 3rd is column 9.
    feed(&rpc, "03fl");
    assert_eq!(cursor(&rpc).await, (1, 9));
}

#[tokio::test]
async fn t_stops_before_the_target_char() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0to");
    assert_eq!(cursor(&rpc).await, (1, 3));
}

#[tokio::test]
async fn cap_f_searches_backward() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Cursor rests on the final 'd' (column 10); back to the 'o' at column 7.
    feed(&rpc, "Fo");
    assert_eq!(cursor(&rpc).await, (1, 7));
}

#[tokio::test]
async fn cap_t_stops_after_the_backward_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "To");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

#[tokio::test]
async fn f_does_nothing_when_the_char_is_absent() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0fz");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn dfx_deletes_through_the_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0dfo");
    assert_eq!(lines(&rpc).await, vec![" world"]);
}

#[tokio::test]
async fn dtx_deletes_up_to_the_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0dto");
    assert_eq!(lines(&rpc).await, vec!["o world"]);
}

#[tokio::test]
async fn d_cap_f_deletes_backward_excluding_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Cursor on 'd' (col 10); dFo deletes "orl" (cols 7..10), keeping 'd'.
    feed(&rpc, "dFo");
    assert_eq!(lines(&rpc).await, vec!["hello wd"]);
}

#[tokio::test]
async fn semicolon_repeats_the_find() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0fo;");
    // 'o' at col 4, then the next 'o' at col 7.
    assert_eq!(cursor(&rpc).await, (1, 7));
}

#[tokio::test]
async fn comma_repeats_the_find_reversed() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // fo -> col 4, ; -> col 7, , reverses back to col 4.
    feed(&rpc, "0fo;,");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn semicolon_after_t_skips_the_adjacent_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia-b-c-d<Esc>");
    // t- lands at col 0 (before the '-' at col 1); ; must advance, not stick.
    feed(&rpc, "0t-;");
    assert_eq!(cursor(&rpc).await, (1, 2));
}

#[tokio::test]
async fn v_f_then_delete_includes_the_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0vfod");
    assert_eq!(lines(&rpc).await, vec![" world"]);
}

// ----- Phase 6: buffer / window Lua API ----------------------------------
//
// These drive the *Lua* buffer API (`vim.api.nvim_buf_*` / `nvim_win_get_cursor`
// / `vim.bo`) through `nvim_exec_lua`, which reads the Rust→Lua mirror the server
// refreshes before the eval. The native RPC `nvim_buf_get_lines` (`lines`) reads
// the real editor directly, so it independently confirms the queued mutation
// reached the rope, not just the Lua-side write-through.

/// Run a Lua chunk and return its value (the Phase-6 getters surface here).
async fn exec_lua(rpc: &Rpc, code: &str) -> Value {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .expect("nvim_exec_lua")
}

#[tokio::test]
async fn buf_set_lines_then_get_lines_round_trips_within_one_chunk() {
    let (rpc, _incoming) = start(None).await;
    // Write-through must agree with the eventual real apply: set then get in one
    // chunk reads the Lua mirror; `lines` then proves the rope caught up.
    let got = exec_lua(
        &rpc,
        r#"
        vim.api.nvim_buf_set_lines(0, 0, -1, false, {"alpha", "beta", "gamma"})
        return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n")
        "#,
    )
    .await;
    assert_eq!(got.as_str(), Some("alpha\nbeta\ngamma"));
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta", "gamma"]);
}

#[tokio::test]
async fn buf_get_lines_honors_negative_and_ranged_indices() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"a", "b", "c", "d", "e"})"#,
    )
    .await;
    let last = exec_lua(
        &rpc,
        r#"return table.concat(vim.api.nvim_buf_get_lines(0, -2, -1, false), ",")"#,
    )
    .await;
    assert_eq!(last.as_str(), Some("e"), "(-2,-1) is the last line");
    let mid = exec_lua(
        &rpc,
        r#"return table.concat(vim.api.nvim_buf_get_lines(0, 1, 3, false), ",")"#,
    )
    .await;
    assert_eq!(mid.as_str(), Some("b,c"), "(1,3) is end-exclusive");
}

#[tokio::test]
async fn buf_set_lines_append_replace_all_and_delete() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"one", "two", "three"})"#,
    )
    .await;
    // Append after the last line.
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, -1, -1, false, {"four"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["one", "two", "three", "four"]);
    // Delete the first line (empty replacement).
    exec_lua(&rpc, r#"vim.api.nvim_buf_set_lines(0, 0, 1, false, {})"#).await;
    assert_eq!(lines(&rpc).await, vec!["two", "three", "four"]);
    // Replace everything.
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"only"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["only"]);
}

#[tokio::test]
async fn buf_set_lines_on_a_fresh_empty_buffer() {
    let (rpc, _incoming) = start(None).await;
    // A fresh [No Name] buffer is [""]. Inserting at (0,0) keeps the empty line…
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, 0, false, {"first"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["first", ""]);
    // …while (0,-1) replaces through the last real line (the phantom-newline guard).
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"first"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["first"]);
}

#[tokio::test]
async fn buf_set_lines_reflected_in_the_rendered_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iuntouched<Esc>");
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"scripted edit"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["scripted edit"]);
}

#[tokio::test]
async fn win_get_cursor_reflects_the_real_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar baz<Esc>");
    // Land somewhere unambiguous: line 2, a few columns in.
    feed(&rpc, "gg0jll");
    let pos = exec_lua(
        &rpc,
        r#"local c = vim.api.nvim_win_get_cursor(0); return c[1] * 1000 + c[2]"#,
    )
    .await;
    // Row 2 (1-based), column 2 (0-based).
    assert_eq!(pos.as_u64(), Some(2 * 1000 + 2));
}

#[tokio::test]
async fn get_current_win_is_the_single_window_handle() {
    let (rpc, _incoming) = start(None).await;
    // Phase 5: window handles are the editor's real ids (the first window is 1),
    // and the current window is among those `nvim_list_wins` reports.
    let win = exec_lua(&rpc, r#"return vim.api.nvim_get_current_win()"#).await;
    assert_eq!(win.as_u64(), Some(1));
    let listed = exec_lua(
        &rpc,
        r#"local w = vim.api.nvim_list_wins(); return #w == 1 and w[1] or -1"#,
    )
    .await;
    assert_eq!(
        listed.as_u64(),
        Some(1),
        "the one window is listed by its id"
    );
}

#[tokio::test]
async fn buf_is_loaded_is_true_for_open_and_false_for_unknown() {
    let (rpc, _incoming) = start(None).await;
    let open = exec_lua(
        &rpc,
        r#"return vim.api.nvim_buf_is_loaded(vim.api.nvim_get_current_buf())"#,
    )
    .await;
    assert_eq!(open.as_bool(), Some(true));
    let unknown = exec_lua(&rpc, r#"return vim.api.nvim_buf_is_loaded(9999)"#).await;
    assert_eq!(unknown.as_bool(), Some(false));
}

#[tokio::test]
async fn bo_option_write_is_observable_and_filetype_still_resolves() {
    let (rpc, _incoming) = start(None).await;
    // A write to the per-buffer option store reads back.
    let stored = exec_lua(&rpc, r#"vim.bo.shiftwidth = 2; return vim.bo.shiftwidth"#).await;
    assert_eq!(stored.as_u64(), Some(2));
    // nvim_set_option_value lands in the same store.
    let via_api = exec_lua(
        &rpc,
        r#"vim.api.nvim_set_option_value("tabstop", 8, { buf = 0 }); return vim.bo.tabstop"#,
    )
    .await;
    assert_eq!(via_api.as_u64(), Some(8));
}

#[tokio::test]
async fn vim_o_global_option_reaches_core_search() {
    let (rpc, _incoming) = start(None).await;
    // A global search option set through vim.o must reach the core, not just a
    // Lua table: with ignorecase on, a lowercase pattern matches uppercase text.
    feed(&rpc, "iaXYZb<Esc>0");
    exec_lua(&rpc, r#"vim.o.ignorecase = true"#).await;
    feed(&rpc, "/xyz<CR>");
    // The match "XYZ" sits at byte column 1; the cursor jumps there only because
    // ignorecase reached the editor (off, "xyz" never matches and it stays at 0).
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn vim_o_global_read_reflects_set_ex_command() {
    let (rpc, _incoming) = start(None).await;
    // Reading vim.o reflects the core's value, including one set via the `:set`
    // ex path (the server-pushed mirror, not just a Lua write-through).
    feed(&rpc, ":set ignorecase<CR>");
    let via_o = exec_lua(&rpc, r#"return vim.o.ignorecase"#).await;
    assert_eq!(via_o.as_bool(), Some(true));
    // The abbreviation resolves to the same canonical option.
    let via_abbrev = exec_lua(&rpc, r#"return vim.o.ic"#).await;
    assert_eq!(via_abbrev.as_bool(), Some(true));
}

#[tokio::test]
async fn vim_o_window_option_routes_to_current_window() {
    let (rpc, _incoming) = start(None).await;
    // vim.o forwards a window-local option to the current window: the write must
    // reach the core, observed by reading it back through vim.wo in a fresh chunk
    // (which reads the server-refreshed window mirror).
    exec_lua(&rpc, r#"vim.o.number = false"#).await;
    let via_wo = exec_lua(&rpc, r#"return vim.wo.number"#).await;
    assert_eq!(via_wo.as_bool(), Some(false));
}

#[tokio::test]
async fn vim_o_buffer_option_reaches_core_indent() {
    let (rpc, _incoming) = start(None).await;
    // vim.o forwards a buffer-local option to the current buffer: tabstop set
    // through vim.o drives the width expandtab fills to.
    exec_lua(&rpc, r#"vim.o.tabstop = 2"#).await;
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "i<Tab>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["  x"]);
}

#[tokio::test]
async fn vim_o_unwired_option_round_trips_observably() {
    let (rpc, _incoming) = start(None).await;
    // An option the core does not yet honor stays observable: it round-trips
    // through the plain store, and the seeded defaults read back.
    let tgc = exec_lua(
        &rpc,
        r#"vim.o.termguicolors = true; return vim.o.termguicolors"#,
    )
    .await;
    assert_eq!(tgc.as_bool(), Some(true));
    let bg = exec_lua(&rpc, r#"return vim.o.background"#).await;
    assert_eq!(bg.as_str(), Some("dark"));
}

#[tokio::test]
async fn expandtab_inserts_spaces_to_the_next_tabstop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=4<CR>");
    feed(&rpc, "i<Tab>x<Esc>");
    // expandtab turns the Tab into spaces up to the next tabstop (4).
    assert_eq!(lines(&rpc).await, vec!["    x"]);
}

#[tokio::test]
async fn expandtab_aligns_a_partial_tab_to_the_next_stop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=4<CR>");
    // From virtual column 2 ("ab"), a Tab fills only to column 4: two spaces.
    feed(&rpc, "iab<Tab>c<Esc>");
    assert_eq!(lines(&rpc).await, vec!["ab  c"]);
}

#[tokio::test]
async fn noexpandtab_inserts_a_literal_tab() {
    let (rpc, _incoming) = start(None).await;
    // The default (noexpandtab) keeps a real tab character.
    feed(&rpc, "i<Tab>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["\tx"]);
}

#[tokio::test]
async fn tabstop_drives_the_screen_column_of_a_tab() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set tabstop=2<CR>");
    feed(&rpc, "i<Tab>x<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    // The literal tab now expands to 2 cells (not the default 4), so 'x' sits at
    // screen column 2 while still at byte column 1.
    assert_eq!(view_u64(&view, "cursor_col"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 2);
}

#[tokio::test]
async fn default_indent_chain_resolves_to_four() {
    let (rpc, _incoming) = start(None).await;
    // Defaults: tabstop=4, shiftwidth=0 (follow tabstop), softtabstop=-1 (follow
    // shiftwidth). So a Tab's width resolves down the chain to 4 with no explicit
    // tabstop/softtabstop set — here observed through expandtab's spaces.
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "i<Tab>z<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    z"]);
}

#[tokio::test]
async fn softtabstop_drives_tab_independent_of_tabstop() {
    let (rpc, _incoming) = start(None).await;
    // softtabstop is the width a <Tab> keypress moves, distinct from tabstop (the
    // display width of a real tab). With sts=4 the Tab fills 4 columns even though
    // a literal tab would be 8 wide.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab>q<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    q"]);
}

#[tokio::test]
async fn softtabstop_backspace_removes_a_whole_unit() {
    let (rpc, _incoming) = start(None).await;
    // With softtabstop, <BS> right after a <Tab> deletes the whole soft-tab of
    // spaces it inserted, not one space.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab><BS>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn typed_spaces_backspace_one_at_a_time() {
    let (rpc, _incoming) = start(None).await;
    // Spaces the user typed (not a <Tab>) are deleted one at a time, even though
    // softtabstop is on — only Tab-inserted whitespace collapses as a unit.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i    <BS>x<Esc>"); // four typed spaces, then one <BS>
    assert_eq!(lines(&rpc).await, vec!["   x"]);
}

#[tokio::test]
async fn typing_after_a_tab_breaks_the_soft_tab() {
    let (rpc, _incoming) = start(None).await;
    // A keystroke between the <Tab> and the <BS> ends the soft-tab window, so the
    // backspace removes just that character, leaving the tab's spaces intact.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab>a<BS>b<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    b"]);
}

#[tokio::test]
async fn consecutive_tabs_backspace_unit_by_unit() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    // Two <Tab>s build two soft-tab units (8 spaces); one <BS> peels one unit,
    // leaving four spaces.
    feed(&rpc, "i<Tab><Tab><BS>z<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    z"]);
    // On a fresh line, two <BS>s peel both units back to nothing.
    feed(&rpc, "o<Tab><Tab><BS><BS>w<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    z", "w"]);
}

#[tokio::test]
async fn set_parses_a_numeric_option_assignment() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set shiftwidth=3<CR>");
    let sw = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("shiftwidth", {})"#,
    )
    .await;
    assert_eq!(sw.as_u64(), Some(3));
}

#[tokio::test]
async fn buffer_local_options_are_independent_per_buffer() {
    let (rpc, _incoming) = start(None).await;
    // A second, non-current buffer.
    let other = rpc
        .request(
            "nvim_create_buf",
            vec![Value::Boolean(true), Value::Boolean(false)],
        )
        .await
        .expect("create_buf")
        .as_u64()
        .expect("buffer id");
    // Set tabstop on the background buffer only.
    exec_lua(
        &rpc,
        &format!(r#"vim.api.nvim_set_option_value("tabstop", 2, {{ buf = {other} }})"#),
    )
    .await;
    let other_ts = exec_lua(
        &rpc,
        &format!(r#"return vim.api.nvim_get_option_value("tabstop", {{ buf = {other} }})"#),
    )
    .await;
    let cur_ts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("tabstop", {})"#,
    )
    .await;
    assert_eq!(
        other_ts.as_u64(),
        Some(2),
        "background buffer took the value"
    );
    assert_eq!(cur_ts.as_u64(), Some(4), "current buffer kept the default");
}

#[tokio::test]
async fn get_option_value_reads_the_core_default() {
    let (rpc, _incoming) = start(None).await;
    // Never set, so the read reflects the core default, not nil.
    let ts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("tabstop", {})"#,
    )
    .await;
    assert_eq!(ts.as_u64(), Some(4));
    let et = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("expandtab", {})"#,
    )
    .await;
    assert_eq!(et.as_bool(), Some(false));
    // shiftwidth defaults to 0 ("follow tabstop") and softtabstop to -1 ("follow
    // shiftwidth"), the modern follow-chain.
    let sw = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("shiftwidth", {})"#,
    )
    .await;
    assert_eq!(sw.as_i64(), Some(0));
    let sts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("softtabstop", {})"#,
    )
    .await;
    assert_eq!(sts.as_i64(), Some(-1));
}

#[tokio::test]
async fn bo_write_drives_tab_insertion() {
    let (rpc, _incoming) = start(None).await;
    // Writing vim.bo must reach the core and change how Tab indents.
    exec_lua(&rpc, r#"vim.bo.expandtab = true; vim.bo.tabstop = 4"#).await;
    feed(&rpc, "i<Tab>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    x"]);
}

#[tokio::test]
async fn set_ex_command_is_visible_through_get_option_value() {
    let (rpc, _incoming) = start(None).await;
    // A value set via the :set ex-command path is readable back through the Lua
    // option surface (the Rust->Lua option mirror), not just the value last
    // written from Lua.
    feed(&rpc, ":set tabstop=4<CR>");
    let ts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("tabstop", {})"#,
    )
    .await;
    assert_eq!(ts.as_u64(), Some(4));
}

#[tokio::test]
async fn buf_set_lines_targets_a_non_current_buffer() {
    let (rpc, _incoming) = start(None).await;
    // Create a second buffer (stays non-current) and edit it by id from Lua.
    let other = rpc
        .request(
            "nvim_create_buf",
            vec![Value::Boolean(true), Value::Boolean(false)],
        )
        .await
        .expect("create_buf")
        .as_u64()
        .expect("buffer id");
    exec_lua(
        &rpc,
        &format!(r#"vim.api.nvim_buf_set_lines({other}, 0, -1, false, {{"in the background"}})"#),
    )
    .await;
    // The current buffer is untouched…
    assert_eq!(lines(&rpc).await, vec![""]);
    // …and the background buffer got the edit (native RPC read, by id).
    let got = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(other),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    let got: Vec<String> = match got {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    assert_eq!(got, vec!["in the background"]);
}

#[tokio::test]
async fn buf_set_lines_strict_indexing_raises_out_of_range() {
    let (rpc, _incoming) = start(None).await;
    // pcall captures the strict-indexing error; a clamped (non-strict) call would
    // silently succeed, so this guards the fail-loud contract.
    let ok = exec_lua(
        &rpc,
        r#"return pcall(vim.api.nvim_buf_set_lines, 0, 50, 50, true, {"x"})"#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false), "strict out-of-range must error");
}

// ----- Phase 7: vim.lsp.util.* real implementations -----------------------
//
// These exercise the helpers a config calls inside on_attach / handlers, driven
// through `nvim_exec_lua`. The param builders read the real cursor/buffer (the
// Phase-6 mirror) and convert byte columns to the LSP offset encoding; the editing
// helpers (`apply_workspace_edit`, `show_document`) queue an LspOp the server
// drains into the native workspace-edit / goto paths, so `lines` / `cursor` (native
// RPC reads of the real editor) independently confirm the effect landed.

#[tokio::test]
async fn make_position_params_reflects_the_cursor_and_encoding() {
    let (rpc, _incoming) = start(None).await;
    // "é" is 2 UTF-8 bytes / 1 UTF-16 unit, so the cursor on 'c' sits at byte
    // column 4 but UTF-16 character 3 — the two must not be conflated.
    feed(&rpc, "iéabc<Esc>");
    let utf16 = exec_lua(
        &rpc,
        r#"
        local p = vim.lsp.util.make_position_params(0, "utf-16")
        return p.position.line * 1000 + p.position.character
        "#,
    )
    .await;
    assert_eq!(utf16.as_u64(), Some(3), "line 0, UTF-16 character 3");
    let utf8 = exec_lua(
        &rpc,
        r#"return vim.lsp.util.make_position_params(0, "utf-8").position.character"#,
    )
    .await;
    assert_eq!(utf8.as_u64(), Some(4), "UTF-8 column is the byte index");
}

#[tokio::test]
async fn byte_to_position_char_handles_surrogate_pairs() {
    let (rpc, _incoming) = start(None).await;
    // A 4-byte char (😀) is a surrogate pair — 2 code units — under UTF-16, but a
    // single codepoint under UTF-32. Drive the helper on a Lua literal directly.
    let got = exec_lua(
        &rpc,
        r#"
        local s = "😀"
        return vim._byte_to_position_char(s, #s, "utf-16") * 10
             + vim._byte_to_position_char(s, #s, "utf-32")
        "#,
    )
    .await;
    assert_eq!(got.as_u64(), Some(2 * 10 + 1));
}

#[tokio::test]
async fn make_given_range_params_converts_marks_to_an_exclusive_range() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Mark positions are { row (1-based), col (0-based byte) }; the end is made
    // exclusive (+1 char), matching neovim.
    let out = exec_lua(
        &rpc,
        r#"
        local r = vim.lsp.util.make_given_range_params({1, 0}, {1, 4}, 0, "utf-8").range
        return r.start.line * 1000 + r.start.character * 100 + r["end"].character
        "#,
    )
    .await;
    // Packed line*1000 + start_char*100 + end_char: start {line 0, char 0};
    // end char = 4 + 1 = 5 (exclusive). -> 5.
    assert_eq!(out.as_u64(), Some(5));
}

#[tokio::test]
async fn locations_to_items_builds_sorted_loclist_items() {
    let path = temp_path("loclist");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Two locations, given out of order; the items come back sorted by position,
    // and the `text` is read from the open buffer backing the URI.
    let out = exec_lua(
        &rpc,
        r#"
        local uri = vim.uri_from_bufnr(0)
        local items = vim.lsp.util.locations_to_items({
          { uri = uri, range = { start = { line = 2, character = 0 }, ["end"] = { line = 2, character = 0 } } },
          { uri = uri, range = { start = { line = 0, character = 0 }, ["end"] = { line = 0, character = 0 } } },
        }, "utf-8")
        return items[1].lnum * 1000 + items[2].lnum * 10
             + (items[1].text == "alpha" and items[2].text == "gamma" and 1 or 0)
        "#,
    )
    .await;
    // Packed item1.lnum*1000 + item2.lnum*10 + texts_matched: sorted, item 1 ->
    // line 1 ("alpha"), item 2 -> line 3 ("gamma"). -> 1031.
    assert_eq!(out.as_u64(), Some(1031));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn get_effective_tabstop_prefers_shiftwidth_then_tabstop() {
    let (rpc, _incoming) = start(None).await;
    // Defaults: shiftwidth=0 ("follow tabstop") + tabstop=4 -> 4.
    let dflt = exec_lua(&rpc, r#"return vim.lsp.util.get_effective_tabstop(0)"#).await;
    assert_eq!(dflt.as_u64(), Some(4));
    // A non-zero shiftwidth is preferred, even when tabstop differs.
    let sw = exec_lua(
        &rpc,
        r#"vim.bo.tabstop = 8; vim.bo.shiftwidth = 2; return vim.lsp.util.get_effective_tabstop(0)"#,
    )
    .await;
    assert_eq!(sw.as_u64(), Some(2));
    // shiftwidth=0 is the "follow tabstop" sentinel -> fall through to tabstop.
    let ts = exec_lua(
        &rpc,
        r#"vim.bo.shiftwidth = 0; return vim.lsp.util.get_effective_tabstop(0)"#,
    )
    .await;
    assert_eq!(ts.as_u64(), Some(8));
}

#[tokio::test]
async fn open_floating_preview_shows_content_in_the_panel() {
    let (rpc, mut incoming) = start(None).await;
    let map = latest_after(
        &rpc,
        &mut incoming,
        r#":lua vim.lsp.util.open_floating_preview({"preview one", "preview two"}, "markdown", {title = "Docs"})<CR>"#,
    )
    .await;
    assert_eq!(panel_title(&map), "Docs");
    let lines = panel_lines(&map);
    assert!(
        lines.contains(&"preview one".to_string()) && lines.contains(&"preview two".to_string()),
        "panel content was: {lines:?}"
    );
}

#[tokio::test]
async fn apply_workspace_edit_edits_the_open_buffer() {
    let path = temp_path("wsedit");
    std::fs::write(&path, "hello world\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    // Replace "world" (chars 6..11 on line 0) with "neovim" via a WorkspaceEdit,
    // routed through the native apply path. The buffer has no attached server, so
    // its URI resolves by canonicalized path and the encoding is UTF-8 (char == byte).
    exec_lua(
        &rpc,
        r#"
        local uri = vim.uri_from_bufnr(0)
        vim.lsp.util.apply_workspace_edit({
          changes = {
            [uri] = {
              { range = { start = { line = 0, character = 6 },
                          ["end"] = { line = 0, character = 11 } },
                newText = "neovim" },
            },
          },
        })
        "#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["hello neovim"]);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn show_document_jumps_the_cursor_to_the_location() {
    let path = temp_path("showdoc");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let (rpc, _incoming) = start(Some(path.to_string_lossy().into_owned())).await;
    exec_lua(
        &rpc,
        r#"
        local uri = vim.uri_from_bufnr(0)
        vim.lsp.util.show_document(
          { uri = uri, range = { start = { line = 2, character = 0 },
                                 ["end"] = { line = 2, character = 0 } } },
          "utf-8")
        "#,
    )
    .await;
    // Jumped to line 3 (1-based), column 0.
    assert_eq!(cursor(&rpc).await, (3, 0));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn show_document_external_location_raises() {
    let (rpc, _incoming) = start(None).await;
    // An `external = true` location has no nxvim surface, so it must fail loud
    // rather than silently no-op (the no-silent-stubs rule).
    let ok = exec_lua(
        &rpc,
        r#"return pcall(vim.lsp.util.show_document, { uri = "https://example.com", external = true })"#,
    )
    .await;
    assert_eq!(
        ok.as_bool(),
        Some(false),
        "external show_document must raise"
    );
}

// ---- Phase 0: ex range parsing -------------------------------------------
//
// A bare range (no command) resolves and moves the cursor to the *last*
// address, landing on its first non-blank — vim's behavior for `:5<CR>`,
// `:1,5<CR>`, `:%<CR>`. These exercise the range parser without `:s` yet.

async fn range_fixture() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    // Five lines; line 4 is indented so we can see the cursor land on the
    // first non-blank rather than column 0.
    feed(&rpc, "ione<CR>two<CR>three<CR>    four<CR>five<Esc>gg");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "    four", "five"],
        "fixture buffer"
    );
    (rpc, incoming)
}

#[tokio::test]
async fn ex_range_absolute_line_jumps() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":3<CR>");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn ex_range_dollar_jumps_to_last_line() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":$<CR>");
    assert_eq!(cursor(&rpc).await, (5, 0));
}

#[tokio::test]
async fn ex_range_dot_offset_moves_relative() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":3<CR>"); // on line 3
    feed(&rpc, ":.+2<CR>"); // +2 -> line 5
    assert_eq!(cursor(&rpc).await, (5, 0));
    feed(&rpc, ":.-1<CR>"); // -1 -> line 4 (indented)
    assert_eq!(cursor(&rpc).await, (4, 4), "lands on first non-blank");
}

#[tokio::test]
async fn ex_range_bare_offset_is_relative_to_cursor() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":2<CR>"); // on line 2
    feed(&rpc, ":+2<CR>"); // a leading +/- offset is relative to the cursor
    assert_eq!(cursor(&rpc).await, (4, 4));
}

#[tokio::test]
async fn ex_range_pair_moves_to_last_address() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":2,4<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (4, 4),
        "a pair moves to the last address"
    );
}

#[tokio::test]
async fn ex_range_percent_moves_to_last_line() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":%<CR>");
    assert_eq!(cursor(&rpc).await, (5, 0));
}

#[tokio::test]
async fn ex_range_out_of_buffer_clamps() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":999<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (5, 0),
        "an over-large line clamps to last"
    );
}

#[tokio::test]
async fn ex_range_reversed_errors_loudly() {
    let (rpc, mut incoming) = range_fixture().await;
    // vim would prompt to swap; we can't prompt, so fail loud rather than
    // silently swap (the no-silent-errors rule).
    let map = redraw_after(&rpc, &mut incoming, ":3,1<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E493"),
        "expected E493 backwards-range error, got {msg:?}"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "cursor stays put on a bad range"
    );
}

#[tokio::test]
async fn ex_range_unknown_mark_errors_loudly() {
    let (rpc, mut incoming) = range_fixture().await;
    // Marks aren't implemented; a mark address must fail loud, not resolve to
    // a bogus line.
    let map = redraw_after(&rpc, &mut incoming, ":'a<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E20"),
        "expected E20 mark-not-set error, got {msg:?}"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "cursor stays put on a bad range"
    );
}

// ---- Phase 1: the :substitute command -----------------------------------
//
// Pattern + replacement are canonical regex (the dialect `/` search uses):
// `(\w+)` captures, `$1` back-refs, `\r` -> newline in the replacement.

#[tokio::test]
async fn substitute_replaces_first_match_on_current_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>");
    feed(&rpc, ":s/foo/baz<CR>"); // trailing delimiter optional
    assert_eq!(lines(&rpc).await, vec!["baz bar foo"], "first match only");
}

#[tokio::test]
async fn substitute_g_flag_replaces_every_match_on_the_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>");
    feed(&rpc, ":s/foo/baz/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["baz bar baz"]);
}

#[tokio::test]
async fn substitute_percent_range_spans_the_whole_buffer() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "bar"]);
}

#[tokio::test]
async fn substitute_line_range_limits_the_edit() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":1,2s/foo/bar<CR>");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "foo"]);
}

#[tokio::test]
async fn substitute_expands_capture_groups() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Canonical-regex groups `(\w+)`, PCRE-style `$1`/`$2` back-refs (the
    // documented divergence from vim's `\(\)` / `\1`).
    feed(&rpc, ":s/(\\w+) (\\w+)/$2 $1/<CR>");
    assert_eq!(lines(&rpc).await, vec!["world hello"]);
}

#[tokio::test]
async fn substitute_empty_replacement_deletes_the_match() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoobar<Esc>");
    feed(&rpc, ":s/o//g<CR>");
    assert_eq!(lines(&rpc).await, vec!["fbar"]);
}

#[tokio::test]
async fn substitute_carriage_return_splits_the_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia, b, c<Esc>");
    feed(&rpc, ":s/, /\\r/g<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "b", "c"],
        "\\r in the replacement splits one line into three"
    );
}

#[tokio::test]
async fn substitute_case_override_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iFOO foo<Esc>");
    feed(&rpc, ":s/foo/x/gi<CR>"); // i: ignore case -> both match
    assert_eq!(lines(&rpc).await, vec!["x x"]);
}

#[tokio::test]
async fn substitute_n_flag_counts_without_changing_the_buffer() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/x/gn<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo foo foo"],
        "n flag makes no edits"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("3 matches on 1 line")
    );
}

#[tokio::test]
async fn substitute_unknown_flag_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/bar/z<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo"], "no edit on a bad flag");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E488"),
        "expected trailing-chars error, got {msg:?}"
    );
}

#[tokio::test]
async fn substitute_no_match_reports_e486_and_keeps_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>gg0");
    let before = cursor(&rpc).await;
    let map = redraw_after(&rpc, &mut incoming, ":s/zzz/x/<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar"],
        "buffer untouched on a miss"
    );
    assert_eq!(cursor(&rpc).await, before, "cursor stays put on a miss");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: zzz")
    );
}

#[tokio::test]
async fn substitute_reports_count_message() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<CR>foo foo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":%s/foo/bar/g<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("6 substitutions on 3 lines")
    );
}

#[tokio::test]
async fn substitute_is_a_single_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "bar"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "foo", "foo"],
        "one u undoes the whole :%s"
    );
}

#[tokio::test]
async fn substitute_cursor_lands_on_last_changed_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>foo<Esc>gg");
    feed(&rpc, ":%s/foo/baz<CR>");
    // Cursor on the last line a substitution happened (line 3), first non-blank.
    assert_eq!(cursor(&rpc).await, (3, 0));
}

// ---- Phase 2: pattern reuse, repeat, count, delimiters ------------------
//
// Bare `:s` / `:&` / `:&&` repeat the last substitute; `~` recalls the last
// replacement; alternate delimiters and a trailing count round out the parser.

#[tokio::test]
async fn substitute_bare_s_repeats_last_resetting_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar/g<CR>"); // line 1: both replaced
    feed(&rpc, "2G");
    feed(&rpc, ":s<CR>"); // repeat on line 2 — flags reset, so first match only
    assert_eq!(lines(&rpc).await, vec!["bar bar", "bar foo"]);
}

#[tokio::test]
async fn substitute_bare_s_accepts_new_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar<CR>"); // line 1: first match only (no g)
    feed(&rpc, "2G");
    feed(&rpc, ":s g<CR>"); // repeat with a fresh g flag -> every match
    assert_eq!(lines(&rpc).await, vec!["bar foo", "bar bar"]);
}

#[tokio::test]
async fn substitute_ampersand_repeats_resetting_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar/g<CR>");
    feed(&rpc, "2G");
    feed(&rpc, ":&<CR>"); // `:&` repeats with flags reset, like bare `:s`
    assert_eq!(lines(&rpc).await, vec!["bar bar", "bar foo"]);
}

#[tokio::test]
async fn substitute_double_ampersand_keeps_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar/g<CR>");
    feed(&rpc, "2G");
    feed(&rpc, ":&&<CR>"); // `:&&` keeps the previous flags (g)
    assert_eq!(lines(&rpc).await, vec!["bar bar", "bar bar"]);
}

#[tokio::test]
async fn substitute_bare_s_without_previous_errors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo"], "nothing to repeat");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E33"), "expected E33, got {msg:?}");
}

#[tokio::test]
async fn substitute_trailing_count_applies_to_n_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>gg");
    feed(&rpc, ":s/foo/bar/ 2<CR>"); // current line + 1 more
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "foo"]);
}

#[tokio::test]
async fn substitute_accepts_alternate_delimiters() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "i/usr/bin<Esc>");
    feed(&rpc, ":s#/usr#/opt#<CR>"); // `#` delimiter so `/` is literal in the pattern
    assert_eq!(lines(&rpc).await, vec!["/opt/bin"]);
    feed(&rpc, ":s,/bin,/sbin,<CR>"); // `,` delimiter
    assert_eq!(lines(&rpc).await, vec!["/opt/sbin"]);
}

#[tokio::test]
async fn substitute_tilde_recalls_previous_replacement() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo baz<Esc>");
    feed(&rpc, ":s/foo/bar/<CR>"); // -> "bar baz", remembers replacement "bar"
    feed(&rpc, ":s/baz/~/<CR>"); // `~` expands to the previous replacement "bar"
    assert_eq!(lines(&rpc).await, vec!["bar bar"]);
}

#[tokio::test]
async fn substitute_tilde_without_previous_errors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/~/<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo"], "no previous replacement");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E33"), "expected E33, got {msg:?}");
}

// ----- Phase 3: the `c` (confirm) flag -----

#[tokio::test]
async fn substitute_confirm_prompts_then_y_replaces_n_skips() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>"); // opens a confirm prompt on the 1st match
    feed(&rpc, "y"); // replace #1
    feed(&rpc, "n"); // skip    #2
    feed(&rpc, "y"); // replace #3
    assert_eq!(lines(&rpc).await, vec!["bar foo bar"]);
}

#[tokio::test]
async fn substitute_confirm_shows_the_replace_prompt() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/bar/c<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo"],
        "nothing changes until the prompt is answered"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("replace with bar (y/n/a/l/q/^E/^Y)?")
    );
}

#[tokio::test]
async fn substitute_confirm_a_replaces_all_remaining() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "a"); // this match and every remaining one, no more prompts
    assert_eq!(lines(&rpc).await, vec!["bar bar bar"]);
}

#[tokio::test]
async fn substitute_confirm_q_quits_without_touching_the_rest() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "y"); // replace #1
    feed(&rpc, "q"); // quit before #2
    assert_eq!(lines(&rpc).await, vec!["bar foo foo"]);
}

#[tokio::test]
async fn substitute_confirm_esc_quits_like_q() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "y"); // replace #1
    feed(&rpc, "<Esc>"); // quit before #2
    assert_eq!(lines(&rpc).await, vec!["bar foo foo"]);
}

#[tokio::test]
async fn substitute_confirm_l_replaces_current_then_stops() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "n"); // skip #1
    feed(&rpc, "l"); // replace #2 and stop (last)
    assert_eq!(lines(&rpc).await, vec!["foo bar foo"]);
}

#[tokio::test]
async fn substitute_confirm_spans_a_range_with_y_and_n() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/c<CR>"); // one match per line (no g)
    feed(&rpc, "y"); // line 1
    feed(&rpc, "n"); // line 2
    feed(&rpc, "y"); // line 3
    assert_eq!(lines(&rpc).await, vec!["bar", "foo", "bar"]);
}

#[tokio::test]
async fn substitute_confirm_reports_count_when_done() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/c<CR>");
    feed(&rpc, "y");
    feed(&rpc, "y");
    let map = redraw_after(&rpc, &mut incoming, "y").await;
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "bar"]);
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("3 substitutions on 3 lines")
    );
}

#[tokio::test]
async fn substitute_confirm_is_a_single_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "y");
    feed(&rpc, "y");
    feed(&rpc, "y");
    assert_eq!(lines(&rpc).await, vec!["bar bar bar"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo foo foo"],
        "one u undoes it all"
    );
}

#[tokio::test]
async fn substitute_confirm_carriage_return_split_then_continue() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia, b, c<Esc>");
    feed(&rpc, ":s/, /\\r/gc<CR>"); // each ", " can split the line in two
    feed(&rpc, "y"); // split after "a" -> "a" / "b, c"
    feed(&rpc, "y"); // the walk continues onto the pushed-down tail
    assert_eq!(lines(&rpc).await, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn substitute_confirm_n_flag_overrides_c_and_only_counts() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo<Esc>");
    // `n` wins over `c`: a counting pass, no prompt, no edit.
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/bar/gnc<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo foo"], "n makes no edits");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("2 matches on 1 line")
    );
}

#[tokio::test]
async fn substitute_confirm_ctrl_e_scrolls_without_consuming_the_match() {
    let path = write_n_lines("conf_ce", 100); // lines "line1".."line100"
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, "gg");
    feed(&rpc, ":%s/line/LINE/c<CR>"); // prompt opens on line 1's match
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    // The peek scrolled the window down a line but kept the prompt up and made
    // no edit — `^E` is not an answer. (nxvim keeps the cursor on screen every
    // frame, so the view-cursor rides along; the pending match lives in the
    // confirm state, not the cursor, so the answer still lands on it.)
    assert_eq!(first_visible_line(&map), "line2", "view scrolled one line");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("replace with LINE (y/n/a/l/q/^E/^Y)?"),
        "prompt still up after the scroll"
    );
    assert_eq!(
        lines(&rpc).await[0],
        "line1",
        "no substitution happened on the scroll key"
    );

    // The still-pending match answers to `y` as if the scroll never happened.
    feed(&rpc, "y");
    feed(&rpc, "q"); // stop after the first
    let after = lines(&rpc).await;
    assert_eq!(
        after[0], "LINE1",
        "y substituted the originally-prompted match"
    );
    assert_eq!(after[1], "line2", "and only that one");
}

#[tokio::test]
async fn substitute_confirm_cursor_lands_on_last_changed_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>gg0");
    feed(&rpc, ":%s/foo/bar/c<CR>");
    feed(&rpc, "y"); // line 1
    feed(&rpc, "n"); // skip line 2
    feed(&rpc, "y"); // line 3 — the last change
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "cursor on the last changed line"
    );
}

#[tokio::test]
async fn substitute_confirm_all_skipped_pushes_no_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ofoo foo<Esc>"); // line 2 is "foo foo"; line 1 stays ""
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "n"); // skip #1
    feed(&rpc, "n"); // skip #2 -> prompt ends, nothing changed
    assert_eq!(lines(&rpc).await, vec!["", "foo foo"], "buffer untouched");
    // The skipped substitute pushed no undo entry, so `u` reverts the prior edit
    // (the `o` insert) — not a phantom no-op snapshot.
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "u undoes the insert, not the :s"
    );
}

// ----- statusline option plumbing (string-valued global option, Phase 1) -----

#[tokio::test]
async fn vim_opt_statusline_round_trips_through_core() {
    let (rpc, _incoming) = start(None).await;
    // The `statusline` string global written through vim.opt reaches the core and
    // reads back the same value via vim.o / vim.opt and the `stl` abbreviation —
    // proving the String OptionValue threads through the Lua bridge and mirror,
    // not just a Lua-side table.
    exec_lua(&rpc, r#"vim.opt.statusline = "%f %l,%c""#).await;
    let via_o = exec_lua(&rpc, r#"return vim.o.statusline"#).await;
    assert_eq!(via_o.as_str(), Some("%f %l,%c"));
    let via_opt = exec_lua(&rpc, r#"return vim.opt.statusline"#).await;
    assert_eq!(via_opt.as_str(), Some("%f %l,%c"));
    let via_abbrev = exec_lua(&rpc, r#"return vim.o.stl"#).await;
    assert_eq!(via_abbrev.as_str(), Some("%f %l,%c"));
}

#[tokio::test]
async fn vim_o_statusline_read_reflects_set_ex_command() {
    let (rpc, _incoming) = start(None).await;
    // Reading vim.o.statusline reflects a value set via the `:set` ex path (the
    // server-pushed mirror), the same home the Lua write reaches.
    feed(&rpc, ":set statusline=%f<CR>");
    let via_o = exec_lua(&rpc, r#"return vim.o.statusline"#).await;
    assert_eq!(via_o.as_str(), Some("%f"));
}

#[tokio::test]
async fn set_statusline_query_echoes_value_with_escaped_spaces() {
    let (rpc, mut incoming) = start(None).await;
    // `:set statusline=…` carries spaces via vim's `\ ` escaping (the value would
    // otherwise split into separate `:set` tokens). The escaped space survives as
    // a real space, and `:set statusline?` echoes the stored value back.
    feed(&rpc, r":set statusline=%f\ %l,%c<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set statusline?<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert_eq!(msg, "statusline=%f %l,%c");
}

#[tokio::test]
async fn set_statusline_reset_clears_to_default() {
    let (rpc, _incoming) = start(None).await;
    // `:set statusline&` resets the option to its default (empty).
    feed(&rpc, ":set statusline=%f<CR>");
    assert_eq!(
        exec_lua(&rpc, r#"return vim.o.statusline"#).await.as_str(),
        Some("%f")
    );
    feed(&rpc, ":set statusline&<CR>");
    assert_eq!(
        exec_lua(&rpc, r#"return vim.o.statusline"#).await.as_str(),
        Some("")
    );
}

// ----- :global / :vglobal (and the :delete / :print they drive) -----

#[tokio::test]
async fn ex_delete_removes_the_range_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<CR>ddd<Esc>");
    feed(&rpc, ":2,3d<CR>");
    assert_eq!(lines(&rpc).await, vec!["aaa", "ddd"]);
}

#[tokio::test]
async fn ex_delete_bare_removes_the_current_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg");
    feed(&rpc, ":d<CR>"); // current line (1) only
    assert_eq!(lines(&rpc).await, vec!["bbb", "ccc"]);
}

#[tokio::test]
async fn global_deletes_every_matching_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ikeep<CR>drop me<CR>keep<CR>drop<CR>keep<Esc>");
    feed(&rpc, ":g/drop/d<CR>");
    assert_eq!(lines(&rpc).await, vec!["keep", "keep", "keep"]);
}

#[tokio::test]
async fn vglobal_deletes_every_non_matching_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ikeep<CR>drop me<CR>keep<CR>drop<CR>x<Esc>");
    feed(&rpc, ":v/drop/d<CR>"); // delete lines NOT matching "drop"
    assert_eq!(lines(&rpc).await, vec!["drop me", "drop"]);
}

#[tokio::test]
async fn global_bang_is_vglobal() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ikeep<CR>drop<CR>keep<Esc>");
    feed(&rpc, ":g!/drop/d<CR>"); // == :v/drop/d
    assert_eq!(lines(&rpc).await, vec!["drop"]);
}

#[tokio::test]
async fn global_runs_substitute_on_matching_lines_only() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo 1<CR>bar 1<CR>foo 1<Esc>");
    feed(&rpc, ":g/foo/s/1/X/<CR>"); // substitute only on the "foo" lines
    assert_eq!(
        lines(&rpc).await,
        vec!["foo X", "bar 1", "foo X"],
        "the bar line is skipped even though it also contains 1"
    );
}

#[tokio::test]
async fn global_default_range_is_the_whole_file() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "idrop<CR>a<CR>drop<Esc>G"); // cursor on the last line
    feed(&rpc, ":g/drop/d<CR>"); // no range → whole file, not the current line
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn global_range_limits_the_scan() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ix<CR>x<CR>x<CR>x<Esc>");
    feed(&rpc, ":2,3g/x/d<CR>"); // only lines 2..3 are eligible
    assert_eq!(lines(&rpc).await, vec!["x", "x"]);
}

#[tokio::test]
async fn global_is_a_single_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ix<CR>y<CR>x<CR>y<CR>x<Esc>");
    feed(&rpc, ":g/x/d<CR>");
    assert_eq!(lines(&rpc).await, vec!["y", "y"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["x", "y", "x", "y", "x"],
        "one u restores every :g/x/d deletion"
    );
}

#[tokio::test]
async fn global_empty_pattern_reuses_last_search() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>foo<Esc>");
    feed(&rpc, "/foo<CR>"); // sets the last search pattern
    feed(&rpc, ":g//d<CR>"); // empty pattern → reuse "foo"
    assert_eq!(lines(&rpc).await, vec!["bar"]);
}

#[tokio::test]
async fn global_prints_matching_lines_when_no_command() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<CR>beta<CR>also alpha<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":g/alpha/<CR>").await; // default cmd = print
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta", "also alpha"],
        "print changes nothing"
    );
    // The last printed line shows on the message line.
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("also alpha")
    );
}

#[tokio::test]
async fn global_nested_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ix<CR>y<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":g/x/g/y/d<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["x", "y"], "nothing deleted");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E147"), "expected E147, got {msg:?}");
}

#[tokio::test]
async fn global_no_match_reports_e486() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":g/zzz/d<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo", "bar"], "buffer untouched");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: zzz")
    );
}

// ----- statusline rendering (the %-format engine, Phase 3) -----
//
// These assert on the per-window `status` segment array the server now projects
// (text + a style-palette id per highlighted run), driven by the `'statusline'`
// option through the core engine. The default UI is 80 cols, so the short
// formats below never hit `%<` truncation.

/// The first window's `status` segments from a redraw, as `(text, style_id)` —
/// `style_id` is `None` for a segment painted in the base `StatusLine` look.
fn status_segments(map: &[(Value, Value)]) -> Vec<(String, Option<usize>)> {
    field(map, "status")
        .and_then(Value::as_array)
        .expect("a status segment array")
        .iter()
        .map(|seg| {
            let Value::Map(m) = seg else {
                panic!("status segment is not a map")
            };
            let get = |key: &str| {
                m.iter()
                    .find(|(k, _)| k.as_str() == Some(key))
                    .map(|(_, v)| v)
            };
            let text = get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let style = get("style").and_then(Value::as_u64).map(|n| n as usize);
            (text, style)
        })
        .collect()
}

/// The whole status line as one string — the analogue of nvim's
/// `nvim_eval_statusline(...).str`.
fn status_text(map: &[(Value, Value)]) -> String {
    status_segments(map).into_iter().map(|(t, _)| t).collect()
}

#[tokio::test]
async fn statusline_literal_renders_verbatim() {
    let (rpc, mut incoming) = start(None).await;
    // A literal format (no %-items) paints exactly its text — no fill without a
    // `%=`, so it isn't padded to the window width.
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=hello<CR>").await;
    assert_eq!(status_text(&map), "hello");
    assert_eq!(status_segments(&map), vec![("hello".to_string(), None)]);
}

#[tokio::test]
async fn statusline_fields_expand_from_window_state() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>defgh<Esc>gg"); // 2 lines; gg -> line 1, col 1
    let map = redraw_after(&rpc, &mut incoming, r":set statusline=%f\ %l,%c<CR>").await;
    assert_eq!(status_text(&map), "[No Name] 1,1");
}

#[tokio::test]
async fn statusline_highlight_group_resolves_to_palette_style() {
    let (rpc, mut incoming) = start(None).await;
    // `%#Group#` switches the highlight for the text that follows; the server
    // resolves it to a style-palette id (the base segment before it has none).
    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'MyStl', { fg = '#ff0000', bg = '#00ff00' })",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=a%#MyStl#b<CR>").await;
    let segs = status_segments(&map);
    assert_eq!(segs[0], ("a".to_string(), None));
    assert_eq!(segs[1].0, "b");
    let id = segs[1]
        .1
        .expect("the %#MyStl# run carries a resolved style");

    let styles = field(&map, "styles")
        .and_then(Value::as_array)
        .expect("style palette");
    let Value::Map(style) = &styles[id] else {
        panic!("style entry is not a map")
    };
    assert_eq!(hl_color(style, "fg"), Some(hex("ff0000")));
    assert_eq!(hl_color(style, "bg"), Some(hex("00ff00")));
}

#[tokio::test]
async fn statusline_whole_vlua_expression_renders_result() {
    let (rpc, mut incoming) = start(None).await;
    // `%!expr` — the whole statusline is the eval result. Only v:lua.* is
    // supported; the prefix is stripped to the bare Lua call.
    exec_lua(&rpc, "_G.my_stl = function() return 'HELLO' end").await;
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=%!v:lua.my_stl()<CR>").await;
    assert_eq!(status_text(&map), "HELLO");
}

#[tokio::test]
async fn statusline_embedded_vlua_expression_renders_result() {
    let (rpc, mut incoming) = start(None).await;
    // `%{expr}` — the result is literal text spliced into the surrounding format.
    exec_lua(&rpc, "_G.tag = function() return 'OK' end").await;
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=[%{v:lua.tag()}]<CR>").await;
    assert_eq!(status_text(&map), "[OK]");
}

#[tokio::test]
async fn statusline_default_shows_mode_file_and_ruler() {
    let (rpc, mut incoming) = start(None).await;
    // Empty 'statusline' renders the built-in default through the same engine:
    // ` MODE  file %= line,col `.
    let map = redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    let text = status_text(&map);
    assert!(text.contains("NORMAL"), "default shows the mode: {text:?}");
    assert!(
        text.contains("[No Name]"),
        "default shows the file: {text:?}"
    );
    assert!(
        text.trim_end().ends_with("1,1"),
        "default ends with the line,col ruler: {text:?}"
    );
}

#[tokio::test]
async fn statusline_non_vlua_expression_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    // A non-`v:lua` expression is unsupported (no Vimscript); it renders a loud
    // error naming the expression rather than silently expanding to nothing.
    let map = redraw_after(&rpc, &mut incoming, ":set statusline=%{somevar}<CR>").await;
    let text = status_text(&map);
    assert!(text.contains("E:statusline:"), "loud, not empty: {text:?}");
    assert!(text.contains("somevar"), "names the expression: {text:?}");
}
