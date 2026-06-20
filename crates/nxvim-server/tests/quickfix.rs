//! Phase 1 of the quickfix / `errorformat` feature: the engine + list model,
//! exercised black-box through `setqflist`/`getqflist` (the parser fed explicit
//! lines + efm) and `:cgetbuffer` (the parser fed a buffer). These prove the
//! ported `efm_to_regpat` + parse state machine produce the right structured
//! entries — single-line, multi-line (`%A/%C/%Z`), the `%t`/`%n` field codes, the
//! `%D`/`%X` directory stack — and that a malformed `'errorformat'` fails loud.
//!
//! `setqflist` queues a server-side op drained after the chunk, so each test sets
//! the list in one `exec_lua` and reads it back in a *second* one (the `nx._qflist`
//! mirror is refreshed before every chunk). Assertions fold the entry into a single
//! string so they don't depend on rmpv map navigation.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    cursor, drain_to_latest_redraw, exec_lua, lines, map_get, message, start_attached, write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The current window's buffer name (`nvim_buf_get_name`).
async fn buf_name(rpc: &Rpc) -> String {
    rpc.request("nvim_buf_get_name", vec![Value::from(0u64)])
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The number of open windows (`nvim_list_wins`).
async fn win_count(rpc: &Rpc) -> usize {
    match rpc.request("nvim_list_wins", vec![]).await {
        Ok(Value::Array(a)) => a.len(),
        _ => 0,
    }
}

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Run `set_code` (a `setqflist` call), then evaluate `read_code` against the
/// refreshed list and return its string result.
async fn set_then_read(rpc: &Rpc, set_code: &str, read_code: &str) -> String {
    exec_lua(rpc, set_code).await;
    exec_lua(rpc, read_code)
        .await
        .as_str()
        .unwrap_or("<not a string>")
        .to_string()
}

/// Feed `keys`, then return the message line off the most-recent queued `redraw`
/// (take-latest, per the harness convention).
async fn message_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    while incoming.try_recv().is_ok() {}
    rpc.request("nx_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    for _ in 0..200 {
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return message(&map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no redraw arrived for {keys:?}");
}

/// Feed `keys`, settle, and return the most-recent queued `redraw` map.
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {}
    rpc.request("nx_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    for _ in 0..200 {
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return map;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no redraw arrived for {keys:?}");
}

/// The `region` string of every window painted in a `redraw` map (`"main"` /
/// `"dock_bottom"` / …).
fn regions(map: &[(Value, Value)]) -> Vec<String> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return Vec::new();
    };
    wins.iter()
        .filter_map(|w| match w {
            Value::Map(m) => map_get(m, "region")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

/// How many tab cells a dock region (`"bottom"` / …) projects in `region_tablines`
/// (`0` when the region is absent or its tabline is hidden — a single-tab dock at
/// the default `showtabline`).
fn region_tab_count(map: &[(Value, Value)], region: &str) -> usize {
    let Some(Value::Map(rts)) = map_get(map, "region_tablines") else {
        return 0;
    };
    let Some(Value::Map(r)) = map_get(rts, region) else {
        return 0;
    };
    match map_get(r, "tabs") {
        Some(Value::Array(t)) => t.len(),
        _ => 0,
    }
}

#[tokio::test]
async fn gcc_style_single_line_is_parsed_into_fields() {
    let (rpc, _incoming) = start().await;
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "main.c:10:5: error: expected ';'" }, efm = "%f:%l:%c: %m" })"#,
        r#"local q = vim.fn.getqflist()
           local e = q[1]
           return string.format("%d|%d|%d|%s|%s|%s", #q, e.lnum, e.col, e.type, e.text, e.filename)"#,
    )
    .await;
    assert_eq!(got, "1|10|5||error: expected ';'|main.c");
}

#[tokio::test]
async fn type_and_number_field_codes_are_parsed() {
    let (rpc, _incoming) = start().await;
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "x.c:3:E:42:bad token" }, efm = "%f:%l:%t:%n:%m" })"#,
        r#"local q = vim.fn.getqflist()
           local e = q[1]
           return string.format("%d|%d|%s|%d|%s", #q, e.lnum, e.type, e.nr, e.text)"#,
    )
    .await;
    assert_eq!(got, "1|3|E|42|bad token");
}

#[tokio::test]
async fn multiline_prefixes_fold_into_one_entry() {
    let (rpc, _incoming) = start().await;
    // `%E` starts the message, `%C` continues it, `%Z` (literal "end") closes it.
    // `%Z` precedes `%C` so the closing line isn't swallowed as a continuation.
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({}, " ", {
             lines = { "fatal: something broke", "  caused by X", "end" },
             efm = "%Efatal: %m,%Zend,%C%m",
           })"#,
        r#"local q = vim.fn.getqflist()
           local e = q[1]
           return string.format("%d|%s|%s", #q, e.type, (e.text:gsub("\n", "/")))"#,
    )
    .await;
    assert_eq!(got, "1|E|something broke/  caused by X");
}

#[tokio::test]
async fn directory_stack_resolves_relative_filenames() {
    let (rpc, _incoming) = start().await;
    // `%D` pushes a directory; the relative `src/a.c` resolves under it; `%X` pops.
    // The two directory lines become invalid (non-error) entries, the source line
    // the one valid entry — three in all.
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({}, " ", {
             lines = { "Entering dir /tmp/proj", "src/a.c:10:oops", "Leaving dir /tmp/proj" },
             efm = "%DEntering dir %f,%XLeaving dir %f,%f:%l:%m",
           })"#,
        r#"local q = vim.fn.getqflist()
           for _, e in ipairs(q) do
             if e.valid then return string.format("%d|%s|%d", #q, e.filename, e.lnum) end
           end
           return "no valid entry""#,
    )
    .await;
    assert_eq!(got, "3|/tmp/proj/src/a.c|10");
}

#[tokio::test]
async fn structured_setqflist_round_trips() {
    let (rpc, _incoming) = start().await;
    // The non-parsing form: a list of explicit item dicts.
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({
             { filename = "a.rs", lnum = 7, col = 2, text = "boom", type = "W" },
           }, " ")"#,
        r#"local q = vim.fn.getqflist()
           local e = q[1]
           return string.format("%d|%s|%d|%d|%s|%s", #q, e.filename, e.lnum, e.col, e.type, e.text)"#,
    )
    .await;
    assert_eq!(got, "1|a.rs|7|2|W|boom");
}

#[tokio::test]
async fn append_action_extends_the_list() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "a.c:1:one" }, efm = "%f:%l:%m" })"#,
    )
    .await;
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({}, "a", { lines = { "b.c:2:two" }, efm = "%f:%l:%m" })"#,
        r#"local q = vim.fn.getqflist()
           return string.format("%d|%s|%s", #q, q[1].text, q[2].text)"#,
    )
    .await;
    assert_eq!(got, "2|one|two");
}

#[tokio::test]
async fn cgetbuffer_parses_the_current_buffer() {
    let (rpc, mut incoming) = start().await;
    // Put compiler-style output in the buffer, set a matching efm, then ingest it.
    message_after(&rpc, &mut incoming, ":set efm=%f:%l:%c:%m<CR>").await;
    message_after(&rpc, &mut incoming, "ilib.rs:42:7:warn<Esc>").await;
    message_after(&rpc, &mut incoming, ":cgetbuffer<CR>").await;

    let got = exec_lua(
        &rpc,
        r#"local q = vim.fn.getqflist()
           local e = q[1]
           return string.format("%d|%s|%d|%d|%s", #q, e.filename, e.lnum, e.col, e.text)"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("1|lib.rs|42|7|warn"));
}

#[tokio::test]
async fn malformed_errorformat_fails_loud() {
    let (rpc, mut incoming) = start().await;
    // A bad directive (`%y` is not a field, not a prefix at this position) must
    // surface vim's E37x rather than silently producing an empty list.
    message_after(&rpc, &mut incoming, ":set efm=x%y<CR>").await;
    let msg = message_after(&rpc, &mut incoming, ":cgetbuffer<CR>").await;
    assert!(
        msg.contains("E377"),
        "an invalid errorformat should report E377, got {msg:?}"
    );
    // And the list stays empty — the bad parse didn't half-populate it.
    let count = exec_lua(&rpc, "return #vim.fn.getqflist()").await;
    assert_eq!(count.as_i64(), Some(0));
}

// --- Phase 2: the quickfix window + navigation -----------------------------

#[tokio::test]
async fn copen_renders_the_list() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({
             { filename = "a.c", lnum = 10, col = 5, text = "boom" },
             { filename = "b.c", lnum = 3, text = "later" },
           }, " ")"#,
    )
    .await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    // The quickfix window is focused; its buffer holds the rendered lines.
    let rendered = lines(&rpc).await;
    assert_eq!(rendered[0], "a.c|10 col 5| boom");
    assert_eq!(rendered[1], "b.c|3| later");
}

#[tokio::test]
async fn enter_in_qf_window_jumps_to_the_entry() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("qf_jump", "txt", "one\ntwo\nthree\nfour\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{ {{ filename = "{path}", lnum = 2, col = 1, text = "here" }} }}, " ")"#
        ),
    )
    .await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    // <CR> on the first entry jumps into the source file at line 2.
    message_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(buf_name(&rpc).await, path, "landed in the entry's file");
    assert_eq!(cursor(&rpc).await.0, 2, "landed on the entry's line");
}

/// Phase 1 of "search results → dock lists": a quickfix display hosted as a
/// **bottom-dock tab** must, on `<CR>`, open the entry in the **main** editing
/// layer — never inside the dock. The jump target resolves to a main-layer window
/// (`qf_prev_win`), and `set_current_window` already crosses layers, so this rides
/// existing machinery; the test guards that the file lands in main and the dock is
/// not split open to host it.
#[tokio::test]
async fn enter_in_dock_hosted_qf_jumps_into_the_main_layer() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("qf_dock_jump", "txt", "one\ntwo\nthree\nfour\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{ {{ filename = "{path}", lnum = 2, col = 1, text = "here" }} }}, " ")"#
        ),
    )
    .await;
    // Materialise the qf display buffer, grab its bufnr, close its main-layer split,
    // then host that same buffer as a bottom-dock tab — all existing APIs.
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    let qfbuf = exec_lua(&rpc, "return vim.api.nvim_get_current_buf()")
        .await
        .as_u64()
        .expect("qf display bufnr");
    exec_lua(&rpc, "vim.cmd('cclose')").await;
    exec_lua(
        &rpc,
        &format!("nx.dock.open{{ side = 'bottom', size = 10, buf = {qfbuf} }}"),
    )
    .await;
    // Precondition: we are focused in the dock-hosted qf display (not main), so the
    // jump below genuinely originates from the dock.
    let cur_buf = exec_lua(&rpc, "return vim.api.nvim_get_current_buf()")
        .await
        .as_u64();
    assert_eq!(
        cur_buf,
        Some(qfbuf),
        "focused in the dock-hosted qf display"
    );
    let wins_before = win_count(&rpc).await;
    assert_eq!(wins_before, 2, "one main window + the bottom-dock window");

    // <CR> on the entry, from the dock-hosted display, must jump into main.
    message_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(buf_name(&rpc).await, path, "landed in the entry's file");
    assert_eq!(cursor(&rpc).await.0, 2, "landed on the entry's line");
    assert_eq!(
        win_count(&rpc).await,
        wins_before,
        "the jump reused a main window — it did not split the dock to host the file"
    );
}

/// Phase 2: with `'qfdock'` on (the default — the nxvim way), `:copen` hosts the
/// quickfix display as a tab in the **bottom dock** (not a bottom split), `<CR>`
/// still jumps into the main layer, and the dock list-tab persists after the jump.
#[tokio::test]
async fn copen_hosts_the_list_in_the_bottom_dock_by_default() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("qfdock_on", "txt", "one\ntwo\nthree\nfour\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{ {{ filename = "{path}", lnum = 2, col = 1, text = "here" }} }}, " ")"#
        ),
    )
    .await;

    let rd = redraw_after(&rpc, &mut incoming, ":copen<CR>").await;
    assert!(
        regions(&rd).iter().any(|r| r == "dock_bottom"),
        "qf display hosted in the bottom dock, got regions {:?}",
        regions(&rd)
    );
    // Focused in the dock-hosted display, on the entry.
    let qfbuf = exec_lua(&rpc, "return vim.api.nvim_get_current_buf()")
        .await
        .as_u64();
    assert!(qfbuf.is_some(), "a qf display buffer is focused");

    // <CR> jumps into the entry's file, in the main layer.
    let rd2 = redraw_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(buf_name(&rpc).await, path, "jumped into the entry's file");
    assert_eq!(cursor(&rpc).await.0, 2, "landed on the entry's line");
    assert!(
        regions(&rd2).iter().any(|r| r == "dock_bottom"),
        "the dock list-tab persists after the jump, got {:?}",
        regions(&rd2)
    );
}

/// Phase 3: `nx.qf.send_to_loclist` (the telescope-style "send results to a list"
/// action) saves each search as its **own** tab in the bottom dock — independent
/// lists side by side — and `<CR>` on an entry jumps into the main layer.
#[tokio::test]
async fn send_to_loclist_saves_each_search_as_its_own_dock_tab() {
    let (rpc, mut incoming) = start().await;
    let a = write_temp("send_a", "txt", "one\ntwo\nthree\n");
    let b = write_temp("send_b", "txt", "alpha\nbeta\ngamma\n");

    // First search -> first dock tab.
    exec_lua(
        &rpc,
        &format!(
            r#"nx.qf.send_to_loclist({{ {{ filename = "{a}", lnum = 2, col = 1, text = "hit a" }} }}, {{ title = "Search A" }})"#
        ),
    )
    .await;
    let rd1 = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        regions(&rd1).iter().any(|r| r == "dock_bottom"),
        "the first search opened in the bottom dock"
    );

    // Second search -> a second, independent dock tab.
    exec_lua(
        &rpc,
        &format!(
            r#"nx.qf.send_to_loclist({{ {{ filename = "{b}", lnum = 3, col = 1, text = "hit b" }} }}, {{ title = "Search B" }})"#
        ),
    )
    .await;
    let rd2 = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        region_tab_count(&rd2, "bottom"),
        2,
        "two searches are saved as two bottom-dock tabs"
    );

    // The focused (second) tab carries only its own list — the searches are not a
    // single shared list.
    let cur = exec_lua(
        &rpc,
        r#"local l = vim.fn.getloclist(0)
           return string.format("%d|%s", #l, l[1] and l[1].text or "")"#,
    )
    .await;
    assert_eq!(cur.as_str(), Some("1|hit b"), "tab B holds search B");

    // Switch to the first tab (gT acts on the focused dock layer); it still holds
    // search A independently.
    redraw_after(&rpc, &mut incoming, "gT").await;
    let prev = exec_lua(
        &rpc,
        r#"local l = vim.fn.getloclist(0)
           return string.format("%d|%s", #l, l[1] and l[1].text or "")"#,
    )
    .await;
    assert_eq!(prev.as_str(), Some("1|hit a"), "tab A still holds search A");

    // <CR> on tab A's entry jumps into its file, in the main layer.
    let rd3 = redraw_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(buf_name(&rpc).await, a, "jumped into search A's file");
    assert_eq!(cursor(&rpc).await.0, 2, "landed on the entry's line");
    assert!(
        regions(&rd3).iter().any(|r| r == "dock_bottom"),
        "the dock lists persist after the jump"
    );
}

/// Phase 5: the `'qfdock'` option reads and writes through `nx.o` (the example's
/// toggle relies on it), defaulting on.
#[tokio::test]
async fn qfdock_option_reads_and_writes_via_nx_o() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        exec_lua(&rpc, "return nx.o.qfdock").await.as_bool(),
        Some(true),
        "qfdock defaults on"
    );
    exec_lua(&rpc, "nx.o.qfdock = false").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.o.qfdock").await.as_bool(),
        Some(false),
        "nx.o.qfdock round-trips"
    );
}

/// Phase 5: the global quickfix list shows as a **single** bottom-dock tab;
/// `send_to_qflist` fills it and `add_to_qflist` appends — one list, one reused tab.
#[tokio::test]
async fn send_and_add_to_qflist_use_one_dock_tab() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"nx.qf.send_to_qflist({ { filename = "a.c", lnum = 1, text = "one" } }, { title = "Q" })"#,
    )
    .await;
    let rd = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        regions(&rd).iter().any(|r| r == "dock_bottom"),
        "the quickfix list opened in the bottom dock"
    );

    exec_lua(
        &rpc,
        r#"nx.qf.add_to_qflist({ { filename = "b.c", lnum = 2, text = "two" } })"#,
    )
    .await;
    let rd2 = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        region_tab_count(&rd2, "bottom"),
        0,
        "still a single qf tab (a one-tab dock hides its tabline)"
    );
    let n = exec_lua(&rpc, "return #vim.fn.getqflist()").await;
    assert_eq!(
        n.as_i64(),
        Some(2),
        "add_to_qflist appended to the one list"
    );
}

/// Phase 5: `add_to_loclist` appends to the **focused** dock loclist tab rather than
/// opening a new one (telescope's add-to-list semantics).
#[tokio::test]
async fn add_to_loclist_appends_to_the_focused_dock_tab() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("addll", "txt", "1\n2\n3\n");
    exec_lua(
        &rpc,
        &format!(
            r#"nx.qf.send_to_loclist({{ {{ filename = "{path}", lnum = 1, text = "first" }} }}, {{ title = "L" }})"#
        ),
    )
    .await;
    redraw_after(&rpc, &mut incoming, "<Esc>").await;

    // Focused on the new dock loclist tab; add appends to it (no second tab).
    exec_lua(
        &rpc,
        &format!(
            r#"nx.qf.add_to_loclist({{ {{ filename = "{path}", lnum = 2, text = "second" }} }})"#
        ),
    )
    .await;
    let rd = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        region_tab_count(&rd, "bottom"),
        0,
        "still one loclist tab — add appended, did not open a new tab"
    );
    let n = exec_lua(&rpc, "return #vim.fn.getloclist(0)").await;
    assert_eq!(
        n.as_i64(),
        Some(2),
        "add_to_loclist appended to the same list"
    );
}

/// Phase 3: with `:set noqfdock`, `nx.qf.send_to_loclist` falls back to the classic
/// vim/telescope behavior — it replaces the current window's location list and opens
/// it in a split (no dock).
#[tokio::test]
async fn send_to_loclist_without_qfdock_replaces_and_splits() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("send_nodoc", "txt", "one\ntwo\nthree\n");
    exec_lua(&rpc, "vim.cmd('set noqfdock')").await;
    exec_lua(
        &rpc,
        &format!(
            r#"nx.qf.send_to_loclist({{ {{ filename = "{path}", lnum = 2, col = 1, text = "hit" }} }}, {{ title = "Results" }})"#
        ),
    )
    .await;
    let rd = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        !regions(&rd).iter().any(|r| r.starts_with("dock_")),
        "no dock with noqfdock, got {:?}",
        regions(&rd)
    );
    assert_eq!(win_count(&rpc).await, 2, "the loclist opened in a split");
}

/// Phase 2: in dock mode, `:cclose` closes the dock-hosted quickfix list (the
/// bottom dock goes away), mirroring how it closes the split in classic mode.
#[tokio::test]
async fn cclose_closes_the_dock_hosted_list() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a.c", lnum = 1, text = "x" } }, " ")"#,
    )
    .await;
    let rd = redraw_after(&rpc, &mut incoming, ":copen<CR>").await;
    assert!(
        regions(&rd).iter().any(|r| r == "dock_bottom"),
        "the list opened in the bottom dock"
    );
    let rd2 = redraw_after(&rpc, &mut incoming, ":cclose<CR>").await;
    assert!(
        !regions(&rd2).iter().any(|r| r == "dock_bottom"),
        ":cclose closed the dock list, got regions {:?}",
        regions(&rd2)
    );
}

/// Phase 2: `:set noqfdock` restores the classic vim/telescope behavior — `:copen`
/// opens a bottom **split** of the current window (no dock), and `<CR>` jumps to the
/// entry.
#[tokio::test]
async fn noqfdock_opens_the_classic_bottom_split() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("qfdock_off", "txt", "one\ntwo\nthree\nfour\n");
    message_after(&rpc, &mut incoming, ":set noqfdock<CR>").await;
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{ {{ filename = "{path}", lnum = 3, col = 1, text = "x" }} }}, " ")"#
        ),
    )
    .await;

    let rd = redraw_after(&rpc, &mut incoming, ":copen<CR>").await;
    assert!(
        !regions(&rd).iter().any(|r| r.starts_with("dock_")),
        "no dock is opened with noqfdock, got regions {:?}",
        regions(&rd)
    );
    assert_eq!(
        win_count(&rpc).await,
        2,
        "the qf split is a second main-layer window"
    );

    message_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(buf_name(&rpc).await, path, "jumped into the entry's file");
    assert_eq!(cursor(&rpc).await.0, 3, "landed on the entry's line");
}

/// Post-unification, the quickfix `<CR>` is an ordinary buffer-local default map (a
/// `FileType qf` autocmd), so it is **rebindable** for the first time: a user can map
/// `<CR>` to something else, or bind the jump to another key, with an ordinary
/// buffer-local map. Here `o` is bound to the jump action and jumps like `<CR>`.
#[tokio::test]
async fn quickfix_enter_is_rebindable() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        "nx.autocmd.create('FileType', { pattern = 'qf', callback = function(a)\n\
           nx.keymap.set('n', 'o', nx.qf.actions.jump, { buffer = a.buf })\n\
         end })",
    )
    .await;
    let path = write_temp("qf_rebind", "txt", "one\ntwo\nthree\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{ {{ filename = "{path}", lnum = 3, col = 1, text = "x" }} }}, " ")"#
        ),
    )
    .await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    message_after(&rpc, &mut incoming, "o").await; // the rebound jump key
    assert_eq!(
        buf_name(&rpc).await,
        path,
        "the rebound `o` jumped to the file"
    );
    assert_eq!(cursor(&rpc).await.0, 3, "landed on the entry's line");
}

#[tokio::test]
async fn cc_and_cnext_navigate_entries() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("qf_nav", "txt", "1\n2\n3\n4\n5\n6\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{
                 {{ filename = "{path}", lnum = 2, text = "a" }},
                 {{ filename = "{path}", lnum = 4, text = "b" }},
               }}, " ")"#
        ),
    )
    .await;
    // :cc 1 jumps to the first entry (line 2); :cnext steps to the second (line 4).
    message_after(&rpc, &mut incoming, ":cc 1<CR>").await;
    assert_eq!(cursor(&rpc).await.0, 2);
    message_after(&rpc, &mut incoming, ":cnext<CR>").await;
    assert_eq!(cursor(&rpc).await.0, 4);
    // :cprev steps back to the first.
    message_after(&rpc, &mut incoming, ":cprev<CR>").await;
    assert_eq!(cursor(&rpc).await.0, 2);
}

#[tokio::test]
async fn cnext_past_the_end_reports_e553() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("qf_e553", "txt", "1\n2\n3\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{ {{ filename = "{path}", lnum = 2, text = "only" }} }}, " ")"#
        ),
    )
    .await;
    message_after(&rpc, &mut incoming, ":cc<CR>").await; // on the sole entry
    let msg = message_after(&rpc, &mut incoming, ":cnext<CR>").await;
    assert!(
        msg.contains("E553"),
        "past the last item should be E553, got {msg:?}"
    );
}

#[tokio::test]
async fn copen_then_cclose_opens_and_closes_the_window() {
    let (rpc, mut incoming) = start().await;
    // Split-window open/close mechanics — opt out of the dock default.
    exec_lua(&rpc, "vim.cmd('set noqfdock')").await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a.c", lnum = 1, text = "x" } }, " ")"#,
    )
    .await;
    let before = win_count(&rpc).await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    assert_eq!(win_count(&rpc).await, before + 1, ":copen adds a window");
    message_after(&rpc, &mut incoming, ":cclose<CR>").await;
    assert_eq!(win_count(&rpc).await, before, ":cclose removes it");
}

/// The `nx.qf` location-list nav wrappers (`lopen`/`lclose`/`lnext`/…) drive the
/// `:l*` ex-commands, mirroring the quickfix wrappers but acting on the current
/// window's location list. Here the list is built with `nx.qf.setloclist`, opened
/// and closed entirely through the Lua surface — no `:l*` keystrokes.
#[tokio::test]
async fn nx_qf_loclist_wrappers_open_navigate_and_close() {
    let (rpc, _incoming) = start().await;
    // Split-window open/navigate/close mechanics — opt out of the dock default.
    exec_lua(&rpc, "vim.cmd('set noqfdock')").await;
    exec_lua(
        &rpc,
        r#"nx.qf.setloclist(0, {
             { filename = "a.c", lnum = 10, col = 5, text = "boom" },
             { filename = "b.c", lnum = 3, text = "later" },
           }, " ")"#,
    )
    .await;
    let before = win_count(&rpc).await;
    exec_lua(&rpc, "nx.qf.lopen()").await;
    assert_eq!(
        win_count(&rpc).await,
        before + 1,
        "nx.qf.lopen adds a window"
    );
    // The location-list window is focused; its buffer holds the rendered entries.
    let rendered = lines(&rpc).await;
    assert_eq!(rendered[0], "a.c|10 col 5| boom");
    assert_eq!(rendered[1], "b.c|3| later");
    exec_lua(&rpc, "nx.qf.lclose()").await;
    assert_eq!(win_count(&rpc).await, before, "nx.qf.lclose removes it");
}

#[tokio::test]
async fn quickfix_window_is_nomodifiable() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a.c", lnum = 1, text = "x" } }, " ")"#,
    )
    .await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    let before = lines(&rpc).await;
    // An edit attempt (`dd`) is refused with E21, exactly like vim's nomodifiable
    // quickfix buffer — the list is left intact, not silently no-op'd.
    let msg = message_after(&rpc, &mut incoming, "dd").await;
    assert!(
        msg.contains("E21"),
        "an edit should be refused with E21, got {msg:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        before,
        "the quickfix buffer is unchanged"
    );
}

#[tokio::test]
async fn copen_opens_a_small_window_at_the_bottom() {
    // A tall screen makes the size unambiguous: vim's default is ~10 rows, well
    // under half of 40. (The earlier draft split the focused window 50/50 with the
    // new window on *top* — this asserts the botright placement that replaced it.)
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 40).await;
    // This asserts the classic bottom-split geometry, so opt out of the dock default.
    exec_lua(&rpc, "vim.cmd('set noqfdock')").await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a.c", lnum = 1, text = "x" } }, " ")"#,
    )
    .await;
    message_after(&rpc, &mut incoming, ":copen<CR>").await;
    // The quickfix window is focused (win 0).
    let height = rpc
        .request("nvim_win_get_height", vec![Value::from(0u64)])
        .await
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let row = match rpc
        .request("nvim_win_get_position", vec![Value::from(0u64)])
        .await
    {
        Ok(Value::Array(a)) => a.first().and_then(Value::as_u64).unwrap_or(0),
        _ => 0,
    };
    assert!(
        height <= 12,
        "quickfix window should be small (~10), got {height}"
    );
    assert!(
        row >= 20,
        "quickfix window should sit at the bottom, got top row {row}"
    );
}

// --- Phase 3: :vimgrep (in-process) and :make / :grep (async producers) -----

/// One folded entry string (`#|filename|lnum|col|text`) from the live list, for
/// compact assertions.
async fn first_entry(rpc: &Rpc) -> String {
    exec_lua(
        rpc,
        r#"local q = vim.fn.getqflist()
           if #q == 0 then return "<empty>" end
           local e = q[1]
           return string.format("%d|%s|%d|%d|%s", #q, e.filename or "", e.lnum, e.col, e.text)"#,
    )
    .await
    .as_str()
    .unwrap_or("<not a string>")
    .to_string()
}

/// Poll `getqflist()` until it holds at least `want` entries (an async `:make` /
/// `:grep` fills it off the run loop), returning the final count.
async fn poll_qf_count(rpc: &Rpc, want: i64) -> i64 {
    for _ in 0..200 {
        let n = exec_lua(rpc, "return #vim.fn.getqflist()")
            .await
            .as_i64()
            .unwrap_or(0);
        if n >= want {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    exec_lua(rpc, "return #vim.fn.getqflist()")
        .await
        .as_i64()
        .unwrap_or(0)
}

#[tokio::test]
async fn vimgrep_finds_matches_and_jumps() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("vg", "txt", "alpha\nbeta TODO\ngamma\nTODO again\n");
    message_after(&rpc, &mut incoming, &format!(":vimgrep /TODO/ {path}<CR>")).await;
    // Two lines match → two entries; the cursor jumps to the first (line 2, the
    // match starts at byte col 6).
    assert_eq!(first_entry(&rpc).await, format!("2|{path}|2|6|beta TODO"));
    assert_eq!(buf_name(&rpc).await, path, "jumped into the searched file");
    assert_eq!(cursor(&rpc).await, (2, 5), "landed on the first match");
}

#[tokio::test]
async fn vimgrep_expands_percent_to_the_current_file() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("vgpct", "txt", "one\nTODO here\nthree\n");
    // Edit the file, then search it via `%` (vim's current-file token) instead of
    // spelling the path out.
    message_after(&rpc, &mut incoming, &format!(":e {path}<CR>")).await;
    message_after(&rpc, &mut incoming, ":vimgrep /TODO/ %<CR>").await;
    assert_eq!(first_entry(&rpc).await, format!("1|{path}|2|1|TODO here"));
    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "landed on the match in the current file"
    );
}

#[tokio::test]
async fn vimgrep_g_flag_matches_every_occurrence() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("vgg", "txt", "a a a\nb\n");
    // Without /g, one entry per matching line; with /g, one per match.
    message_after(&rpc, &mut incoming, &format!(":vimgrep /a/ {path}<CR>")).await;
    assert_eq!(
        exec_lua(&rpc, "return #vim.fn.getqflist()").await.as_i64(),
        Some(1)
    );
    message_after(&rpc, &mut incoming, &format!(":vimgrep /a/g {path}<CR>")).await;
    assert_eq!(
        exec_lua(&rpc, "return #vim.fn.getqflist()").await.as_i64(),
        Some(3)
    );
}

#[tokio::test]
async fn vimgrepadd_appends_to_the_list() {
    let (rpc, mut incoming) = start().await;
    let a = write_temp("vga", "txt", "x here\n");
    let b = write_temp("vgb", "txt", "x there\n");
    message_after(&rpc, &mut incoming, &format!(":vimgrep /x/ {a}<CR>")).await;
    message_after(&rpc, &mut incoming, &format!(":vimgrepadd /x/ {b}<CR>")).await;
    assert_eq!(
        exec_lua(&rpc, "return #vim.fn.getqflist()").await.as_i64(),
        Some(2),
        ":vimgrepadd keeps the prior entry and adds the new one"
    );
}

#[tokio::test]
async fn vimgrep_glob_argument_fails_loud() {
    let (rpc, mut incoming) = start().await;
    // Globbing isn't supported yet — it must say so, not silently match nothing.
    let msg = message_after(&rpc, &mut incoming, ":vimgrep /x/ *.txt<CR>").await;
    assert!(
        msg.contains("globbing is not yet supported"),
        "a glob arg should fail loud, got {msg:?}"
    );
}

#[tokio::test]
async fn make_runs_the_program_populates_and_jumps() {
    let (rpc, mut incoming) = start().await;
    let src = write_temp("mk", "c", "line1\nline2\nline3\nline4\nline5\n");
    // A makeprg that prints one gcc-style error against the temp file; a clean efm
    // makes the parse unambiguous.
    exec_lua(
        &rpc,
        &format!(
            r#"vim.o.errorformat = "%f:%l:%c:%m"
               vim.o.makeprg = [[printf '{src}:3:5:boom\n']]"#
        ),
    )
    .await;
    let before = win_count(&rpc).await;
    message_after(&rpc, &mut incoming, ":make<CR>").await;
    // The async job fills the list; assert the parsed entry, the auto-opened
    // window, and the jump to the first error.
    assert_eq!(
        poll_qf_count(&rpc, 1).await,
        1,
        "the job populated the list"
    );
    assert_eq!(first_entry(&rpc).await, format!("1|{src}|3|5|boom"));
    assert!(
        win_count(&rpc).await > before,
        ":make opened the quickfix window"
    );
    assert_eq!(buf_name(&rpc).await, src, "jumped into the error's file");
    assert_eq!(cursor(&rpc).await, (3, 4), "landed on the first error");
}

#[tokio::test]
async fn make_bang_does_not_jump() {
    let (rpc, mut incoming) = start().await;
    let src = write_temp("mkb", "c", "l1\nl2\nl3\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.o.errorformat = "%f:%l:%c:%m"
               vim.o.makeprg = [[printf '{src}:2:1: nope\n']]"#
        ),
    )
    .await;
    message_after(&rpc, &mut incoming, ":make!<CR>").await;
    assert_eq!(poll_qf_count(&rpc, 1).await, 1);
    // `:make!` still parses + opens, but leaves the cursor where it was (not in src).
    assert_ne!(
        buf_name(&rpc).await,
        src,
        ":make! must not jump to the error"
    );
}

#[tokio::test]
async fn grep_uses_grepprg_and_grepformat() {
    let (rpc, mut incoming) = start().await;
    let src = write_temp("gp", "txt", "one\ntwo\nthree\n");
    // grepprg output is parsed against grepformat (default `%f:%l:%c:%m,%f:%l:%m,…`).
    exec_lua(
        &rpc,
        &format!(r#"vim.o.grepprg = [[printf '{src}:2:found\n']]"#),
    )
    .await;
    message_after(&rpc, &mut incoming, ":grep<CR>").await;
    assert_eq!(poll_qf_count(&rpc, 1).await, 1, ":grep filled the list");
    assert_eq!(first_entry(&rpc).await, format!("1|{src}|2|0|found"));
    assert_eq!(cursor(&rpc).await.0, 2, ":grep jumped to the match");
}

// --- Phase 4: list stack, location lists, nx.qf / diagnostics ---------------

/// Poll the current window's location list until it holds at least `want`
/// entries, returning the final count (an async `:lmake`/`:lgrep` fills it off
/// the run loop).
async fn poll_loclist_count(rpc: &Rpc, want: i64) -> i64 {
    for _ in 0..200 {
        let n = exec_lua(rpc, "return #vim.fn.getloclist(0)")
            .await
            .as_i64()
            .unwrap_or(0);
        if n >= want {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    exec_lua(rpc, "return #vim.fn.getloclist(0)")
        .await
        .as_i64()
        .unwrap_or(0)
}

#[tokio::test]
async fn setloclist_getloclist_round_trip_is_window_scoped() {
    let (rpc, mut incoming) = start().await;
    // Two windows hold independent location lists. The split is a real ex-command
    // between chunks (so the two window ids actually differ — `vim.cmd` is deferred
    // within a chunk); explicit window ids make each `setloclist` window-scoped.
    exec_lua(
        &rpc,
        r#"_G.qf_a = vim.api.nvim_get_current_win()
           vim.fn.setloclist(_G.qf_a, { { filename = "a.rs", lnum = 1, text = "A" } })"#,
    )
    .await;
    message_after(&rpc, &mut incoming, ":vsplit<CR>").await;
    exec_lua(
        &rpc,
        r#"_G.qf_b = vim.api.nvim_get_current_win()
           vim.fn.setloclist(_G.qf_b, {
             { filename = "b.rs", lnum = 2, text = "B1" },
             { filename = "b.rs", lnum = 3, text = "B2" },
           })"#,
    )
    .await;
    let got = exec_lua(
        &rpc,
        r#"local la, lb = vim.fn.getloclist(_G.qf_a), vim.fn.getloclist(_G.qf_b)
           return string.format("%d|%d|%s|%s|%s", _G.qf_a ~= _G.qf_b and 1 or 0, #la, #lb, la[1].text, lb[1].text)"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("1|1|2|A|B1"));
}

#[tokio::test]
async fn colder_and_cnewer_walk_the_quickfix_stack() {
    let (rpc, mut incoming) = start().await;
    // Two fresh `setqflist(_, " ")` calls push two lists onto the stack.
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a", lnum = 1, text = "list1" } }, " ")
           vim.fn.setqflist({ { filename = "b", lnum = 2, text = "list2" } }, " ")"#,
    )
    .await;
    // The current list is the newest (list2); :colder restores the previous one.
    let cur = exec_lua(&rpc, "return vim.fn.getqflist()[1].text").await;
    assert_eq!(cur.as_str(), Some("list2"));
    message_after(&rpc, &mut incoming, ":colder<CR>").await;
    let older = exec_lua(&rpc, "return vim.fn.getqflist()[1].text").await;
    assert_eq!(
        older.as_str(),
        Some("list1"),
        ":colder restored the older list"
    );
    message_after(&rpc, &mut incoming, ":cnewer<CR>").await;
    let newer = exec_lua(&rpc, "return vim.fn.getqflist()[1].text").await;
    assert_eq!(
        newer.as_str(),
        Some("list2"),
        ":cnewer returned to the newer list"
    );
    // Stepping past the bottom of the stack reports E380.
    message_after(&rpc, &mut incoming, ":colder<CR>").await;
    let msg = message_after(&rpc, &mut incoming, ":colder<CR>").await;
    assert!(
        msg.contains("E380"),
        "past the bottom should be E380, got {msg:?}"
    );
}

#[tokio::test]
async fn setqflist_replace_action_swaps_the_current_list() {
    let (rpc, _incoming) = start().await;
    // `" "` makes the list, `"r"` replaces its items in place (no new stack entry).
    let got = set_then_read(
        &rpc,
        r#"vim.fn.setqflist({ { filename = "a", lnum = 1, text = "before" } }, " ")
           vim.fn.setqflist({ { filename = "b", lnum = 2, text = "after" } }, "r")"#,
        r#"local q = vim.fn.getqflist()
           return string.format("%d|%s", #q, q[1].text)"#,
    )
    .await;
    assert_eq!(got, "1|after");
}

#[tokio::test]
async fn lvimgrep_populates_the_window_loclist() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("lvg", "txt", "alpha\nbeta TODO\ngamma\nTODO again\n");
    message_after(&rpc, &mut incoming, &format!(":lvimgrep /TODO/ {path}<CR>")).await;
    // The matches land in the *location* list (not the quickfix list), and the
    // cursor jumps to the first match.
    let got = exec_lua(
        &rpc,
        r#"local l, q = vim.fn.getloclist(0), vim.fn.getqflist()
           return string.format("%d|%d|%s", #l, #q, l[1] and l[1].text or "")"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("2|0|beta TODO"));
    assert_eq!(buf_name(&rpc).await, path, "jumped into the searched file");
    assert_eq!(cursor(&rpc).await, (2, 5), "landed on the first match");
}

#[tokio::test]
async fn lopen_shows_the_loclist_and_enter_jumps() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("ll_jump", "txt", "one\ntwo\nthree\nfour\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setloclist(0, {{ {{ filename = "{path}", lnum = 3, col = 1, text = "here" }} }})"#
        ),
    )
    .await;
    let before = win_count(&rpc).await;
    message_after(&rpc, &mut incoming, ":lopen<CR>").await;
    assert_eq!(win_count(&rpc).await, before + 1, ":lopen adds a window");
    // The display buffer is read-only, like the quickfix window.
    let edit_msg = message_after(&rpc, &mut incoming, "dd").await;
    assert!(
        edit_msg.contains("E21"),
        "loclist window is nomodifiable, got {edit_msg:?}"
    );
    // <CR> on the entry jumps into the owner window's file at line 3.
    message_after(&rpc, &mut incoming, "<CR>").await;
    assert_eq!(buf_name(&rpc).await, path, "landed in the entry's file");
    assert_eq!(cursor(&rpc).await.0, 3, "landed on the entry's line");
}

#[tokio::test]
async fn lmake_populates_the_loclist_not_the_quickfix_list() {
    let (rpc, mut incoming) = start().await;
    let src = write_temp("lmk", "c", "l1\nl2\nl3\nl4\n");
    exec_lua(
        &rpc,
        &format!(
            r#"vim.o.errorformat = "%f:%l:%c:%m"
               vim.o.makeprg = [[printf '{src}:2:3:oops\n']]"#
        ),
    )
    .await;
    message_after(&rpc, &mut incoming, ":lmake<CR>").await;
    assert_eq!(
        poll_loclist_count(&rpc, 1).await,
        1,
        ":lmake filled the loclist"
    );
    // The quickfix list stays empty — :lmake is loclist-scoped.
    let q = exec_lua(&rpc, "return #vim.fn.getqflist()").await;
    assert_eq!(
        q.as_i64(),
        Some(0),
        ":lmake must not touch the quickfix list"
    );
    assert_eq!(
        buf_name(&rpc).await,
        src,
        ":lmake jumped into the error's file"
    );
    assert_eq!(cursor(&rpc).await, (2, 2), "landed on the first error");
}

#[tokio::test]
async fn diagnostic_setloclist_fills_a_navigable_loclist() {
    let (rpc, mut incoming) = start().await;
    let path = write_temp("diag_ll", "txt", "aa\nbb\ncc\ndd\n");
    // Open the file (so the buffer has a name to resolve), inject diagnostics for
    // it, then turn them into a location list via the diagnostics surface.
    message_after(&rpc, &mut incoming, &format!(":e {path}<CR>")).await;
    exec_lua(
        &rpc,
        r#"local b = vim.api.nvim_get_current_buf()
           nx._set_diagnostics(b, {
             { lnum = 2, col = 0, message = "boom", severity = 1 },
           })
           nx.diagnostic.setloclist({ open = false })"#,
    )
    .await;
    // 0-based diagnostic line 2 becomes 1-based loclist line 3.
    let got = exec_lua(
        &rpc,
        r#"local l = vim.fn.getloclist(0)
           return string.format("%d|%d|%s|%s", #l, l[1] and l[1].lnum or 0, l[1] and l[1].type or "", l[1] and l[1].text or "")"#,
    )
    .await;
    assert_eq!(got.as_str(), Some("1|3|E|boom"));
    // It is navigable: :ll jumps to the diagnostic's line (the entry is addressed
    // by buffer number, resolved to the file at jump time).
    message_after(&rpc, &mut incoming, ":ll<CR>").await;
    assert_eq!(
        buf_name(&rpc).await,
        path,
        ":ll stayed in the diagnostic's file"
    );
    assert_eq!(cursor(&rpc).await.0, 3, ":ll jumped to the diagnostic line");
}

#[tokio::test]
async fn cnfile_and_cpfile_step_by_file() {
    let (rpc, mut incoming) = start().await;
    let a = write_temp("nf_a", "txt", "1\n2\n3\n4\n5\n");
    let b = write_temp("nf_b", "txt", "1\n2\n3\n4\n5\n6\n7\n");
    // Two entries in file A, two in file B (in list order).
    exec_lua(
        &rpc,
        &format!(
            r#"vim.fn.setqflist({{
                 {{ filename = "{a}", lnum = 2, text = "a1" }},
                 {{ filename = "{a}", lnum = 4, text = "a2" }},
                 {{ filename = "{b}", lnum = 3, text = "b1" }},
                 {{ filename = "{b}", lnum = 6, text = "b2" }},
               }}, " ")"#
        ),
    )
    .await;
    message_after(&rpc, &mut incoming, ":cfirst<CR>").await; // file A, line 2
    assert_eq!(buf_name(&rpc).await, a);
    // :cnfile jumps to the *first* error in the next file (B, line 3).
    message_after(&rpc, &mut incoming, ":cnfile<CR>").await;
    assert_eq!(buf_name(&rpc).await, b, ":cnfile crossed to the next file");
    assert_eq!(cursor(&rpc).await.0, 3, ":cnfile landed on B's first error");
    // :cpfile jumps back to the *last* error in the previous file (A, line 4).
    message_after(&rpc, &mut incoming, ":cpfile<CR>").await;
    assert_eq!(
        buf_name(&rpc).await,
        a,
        ":cpfile crossed back to the previous file"
    );
    assert_eq!(cursor(&rpc).await.0, 4, ":cpfile landed on A's last error");
    // Past the last file → E553.
    message_after(&rpc, &mut incoming, ":cnfile<CR>").await; // to B
    let msg = message_after(&rpc, &mut incoming, ":cnfile<CR>").await; // past the end
    assert!(
        msg.contains("E553"),
        "past the last file should be E553, got {msg:?}"
    );
}

#[tokio::test]
async fn closing_a_loclist_owner_closes_its_loclist_window() {
    let (rpc, mut incoming) = start().await;
    // Split-window owner/loclist lifecycle — opt out of the dock default.
    exec_lua(&rpc, "vim.cmd('set noqfdock')").await;
    // A second code window first, so closing the owner doesn't hit the last-window
    // guard (which would force the loclist window to stay).
    message_after(&rpc, &mut incoming, ":split<CR>").await;
    let after_split = win_count(&rpc).await; // 2 code windows
    let owner = exec_lua(
        &rpc,
        r#"local w = vim.api.nvim_get_current_win()
           vim.fn.setloclist(w, { { filename = "a.rs", lnum = 1, text = "x" } })
           return w"#,
    )
    .await
    .as_u64()
    .expect("owner window id");
    message_after(&rpc, &mut incoming, ":lopen<CR>").await;
    assert_eq!(
        win_count(&rpc).await,
        after_split + 1,
        ":lopen added the loclist window"
    );
    // Close the owner; its loclist window must close with it.
    rpc.request(
        "nvim_win_close",
        vec![Value::from(owner), Value::from(true)],
    )
    .await
    .expect("nvim_win_close");
    assert_eq!(
        win_count(&rpc).await,
        after_split - 1,
        "closing the owner closed both it and its loclist window"
    );
}
