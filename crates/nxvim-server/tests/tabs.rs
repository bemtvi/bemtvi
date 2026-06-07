//! Behavior tests for tab pages, driven the way a real client drives the editor
//! (black-box RPC, exactly like `windows.rs` / `editing.rs`).
//!
//! Phase 1 pinned the model plus the **read-only** `nvim_tabpage_*` surface.
//! Phase 2 makes tabs real: creation (`:tabnew`/`:tabedit`, `<C-w>T`), switching
//! (`gt`/`gT`/`{count}gt`, `:tabnext`/`:tabprevious`/`:tablast`,
//! `nvim_set_current_tabpage`), closing (`:tabclose`/`:tabonly`), and the
//! tabline projected into the `redraw`. These tests drive those over RPC.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let init = ServerInit::default();
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

/// Issue a request and return the raw reply.
async fn req(rpc: &Rpc, method: &str, args: Vec<Value>) -> Value {
    rpc.request(method, args).await.expect(method)
}

/// Decode a reply that is a single u64 handle.
fn handle(v: &Value) -> u64 {
    v.as_u64().expect("u64 handle")
}

/// Decode a reply that is an array of u64 handles.
fn handles(v: &Value) -> Vec<u64> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_u64().expect("u64")).collect(),
        _ => panic!("expected an array, got {v:?}"),
    }
}

/// Type a string of vim key-notation.
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

#[tokio::test]
async fn one_tab_exists_at_startup_and_is_current() {
    let (rpc, _incoming) = start().await;

    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1], "exactly one tab, handle 1, at startup");

    let current = handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await);
    assert_eq!(current, 1, "the seeded tab is current");
}

#[tokio::test]
async fn tabpage_validity_and_number() {
    let (rpc, _incoming) = start().await;

    // The seeded tab is valid and is tab number 1.
    let valid = req(&rpc, "nvim_tabpage_is_valid", vec![Value::from(1u64)]).await;
    assert_eq!(valid, Value::from(true), "tab 1 is valid");
    let number = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(1u64)]).await);
    assert_eq!(number, 1, "tab 1 is the first (and only) tab");

    // Handle 0 resolves to the current tab.
    let cur_number = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(0u64)]).await);
    assert_eq!(cur_number, 1, "handle 0 == current tab, number 1");

    // An unknown handle is invalid and has no number.
    let unknown = req(&rpc, "nvim_tabpage_is_valid", vec![Value::from(99u64)]).await;
    assert_eq!(unknown, Value::from(false), "tab 99 does not exist");
    let unknown_num = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(99u64)]).await);
    assert_eq!(unknown_num, 0, "an unknown tab reports number 0");
}

#[tokio::test]
async fn current_tab_lists_the_window_set() {
    let (rpc, _incoming) = start().await;

    // With one window, the tab lists exactly that window, and it is the tab's
    // focused window — matching the global window surface.
    let wins = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(0u64)]).await);
    let global = handles(&req(&rpc, "nvim_list_wins", vec![]).await);
    assert_eq!(wins, global, "the current tab owns every open window");

    let tab_cur = handle(&req(&rpc, "nvim_tabpage_get_win", vec![Value::from(0u64)]).await);
    let global_cur = handle(&req(&rpc, "nvim_get_current_win", vec![]).await);
    assert_eq!(
        tab_cur, global_cur,
        "the tab's focused window is the current window"
    );
}

#[tokio::test]
async fn splits_grow_the_current_tabs_window_set() {
    let (rpc, _incoming) = start().await;

    // Splitting adds windows to the (only) tab — there is still one tab, now
    // with two windows under it.
    feed(&rpc, "<Esc><C-w>s");
    let wins = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(0u64)]).await);
    assert_eq!(wins.len(), 2, "the split's two windows both live in tab 1");

    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1], "splitting does not create a tab");

    // The tab's focused window follows the live focus.
    let tab_cur = handle(&req(&rpc, "nvim_tabpage_get_win", vec![Value::from(0u64)]).await);
    let global_cur = handle(&req(&rpc, "nvim_get_current_win", vec![]).await);
    assert_eq!(
        tab_cur, global_cur,
        "tab focus tracks the focused window after a split"
    );
}

// ----- Phase 2: creation & switching ----------------------------------------

/// A unique temp file seeded with `contents`, returned as an absolute path
/// string. Unique per call so parallel tests don't collide.
fn temp_file(tag: &str, contents: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let path: PathBuf =
        std::env::temp_dir().join(format!("nxvim_tab_{tag}_{}_{n}.txt", std::process::id()));
    std::fs::write(&path, contents).expect("write temp file");
    path.display().to_string()
}

/// Feed `keys` and wait for the editor to settle (a `nvim_get_mode` barrier).
async fn feed_sync(rpc: &Rpc, keys: &str) {
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
}

#[tokio::test]
async fn tabnew_creates_a_second_tab_and_switches_to_it() {
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, ":tabnew<CR>").await;

    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1, 2], "a second tab is appended after the first");
    let current = handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await);
    assert_eq!(current, 2, ":tabnew switches to the new tab");

    // The new tab owns its own (single) window, with a fresh id that doesn't
    // collide with tab 1's window.
    let wins2 = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(2u64)]).await);
    let wins1 = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(1u64)]).await);
    assert_eq!(wins1.len(), 1);
    assert_eq!(wins2.len(), 1);
    assert_ne!(wins1[0], wins2[0], "window ids stay unique across tabs");

    // `nvim_list_wins` spans every tab.
    let all = handles(&req(&rpc, "nvim_list_wins", vec![]).await);
    assert_eq!(
        all,
        vec![wins1[0], wins2[0]],
        "list_wins spans all tabpages"
    );
}

#[tokio::test]
async fn gt_and_gt_back_cycle_between_tabs() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>").await; // now on tab 2 of [1, 2]

    // `gt` wraps from the last tab back to the first.
    feed_sync(&rpc, "gt").await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        1,
        "gt from the last tab wraps to the first"
    );

    // `gt` again advances to tab 2.
    feed_sync(&rpc, "gt").await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        2
    );

    // `gT` goes back to tab 1.
    feed_sync(&rpc, "gT").await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        1,
        "gT steps to the previous tab"
    );
}

#[tokio::test]
async fn count_gt_jumps_to_an_absolute_tab_number() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>:tabnew<CR>").await; // tabs [1, 2, 3], on 3

    feed_sync(&rpc, "1gt").await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        1,
        "1gt jumps to tab number 1"
    );
    feed_sync(&rpc, "3gt").await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        3,
        "3gt jumps to tab number 3"
    );
}

#[tokio::test]
async fn nvim_set_current_tabpage_switches_and_restores_focus() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>").await;
    let win2 = handle(&req(&rpc, "nvim_get_current_win", vec![]).await);

    // Jump to tab 1 via the API; its focused window becomes current.
    req(&rpc, "nvim_set_current_tabpage", vec![Value::from(1u64)]).await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        1
    );
    let win1 = handle(&req(&rpc, "nvim_get_current_win", vec![]).await);
    assert_ne!(win1, win2, "tab 1's window is focused, not tab 2's");

    // Back to tab 2 restores its window.
    req(&rpc, "nvim_set_current_tabpage", vec![Value::from(2u64)]).await;
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_win", vec![]).await),
        win2,
        "returning to a tab restores its focused window"
    );
}

#[tokio::test]
async fn tabclose_removes_the_current_tab_but_refuses_the_last() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>:tabnew<CR>").await; // [1, 2, 3], on 3

    feed_sync(&rpc, ":tabclose<CR>").await;
    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1, 2], "the current tab is closed");
    // The neighbor (now the last) becomes current.
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        2
    );

    // Close down to one tab, then a final close is refused.
    feed_sync(&rpc, ":tabclose<CR>").await;
    feed_sync(&rpc, ":tabclose<CR>").await;
    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1], "the last tab cannot be closed");
}

#[tokio::test]
async fn tabonly_keeps_only_the_current_tab() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>:tabnew<CR>").await; // [1, 2, 3], on 3
    let keep = handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await);

    feed_sync(&rpc, ":tabonly<CR>").await;
    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![keep], ":tabonly closes every other tab");
}

#[tokio::test]
async fn ctrl_w_t_moves_the_focused_window_to_a_new_tab() {
    let (rpc, _incoming) = start().await;
    // Split so the tab has two windows, then move the focused one to a new tab.
    feed_sync(&rpc, "<C-w>s").await;
    let win_count_before =
        handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(1u64)]).await).len();
    assert_eq!(win_count_before, 2);

    feed_sync(&rpc, "<C-w>T").await;
    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1, 2], "<C-w>T opens a new tab");
    assert_eq!(
        handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await),
        2,
        "the new tab is focused"
    );
    // Tab 1 keeps the survivor window; tab 2 has the moved one.
    let wins1 = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(1u64)]).await);
    let wins2 = handles(&req(&rpc, "nvim_tabpage_list_wins", vec![Value::from(2u64)]).await);
    assert_eq!(wins1.len(), 1, "the source tab keeps one window");
    assert_eq!(wins2.len(), 1, "the moved window is alone in the new tab");
}

#[tokio::test]
async fn ctrl_w_t_is_a_noop_with_only_one_window() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "<C-w>T").await;
    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(
        tabs,
        vec![1],
        "moving the only window to a new tab does nothing"
    );
}

// ----- Phase 2: the tabline in the redraw -----------------------------------

/// One tabline cell parsed out of a redraw's `tabline` array.
#[derive(Debug, Clone)]
struct TabCell {
    label: String,
    modified: bool,
    window_count: u64,
}

fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The `tabline` cells and `current_tab` index of one redraw map.
fn parse_tabline(map: &[(Value, Value)]) -> (Vec<TabCell>, u64) {
    let cells = match map_get(map, "tabline") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                let m = match v {
                    Value::Map(m) => m.as_slice(),
                    _ => panic!("tabline cell is not a map"),
                };
                TabCell {
                    label: map_get(m, "label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    modified: map_get(m, "modified")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    window_count: map_get(m, "window_count")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    let current = map_get(map, "current_tab")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (cells, current)
}

/// Drain queued redraws and return the latest `(tabline, current_tab)` — the
/// take-latest rule (CLAUDE.md) so a stale frame never leaks in under load.
fn drain_to_latest_tabline(
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<(Vec<TabCell>, u64)> {
    let mut latest = None;
    while let Ok(msg) = incoming.try_recv() {
        if let Incoming::Notification { method, params } = msg {
            if method == "redraw" {
                if let Some(Value::Map(map)) = params.into_iter().next() {
                    latest = Some(parse_tabline(&map));
                }
            }
        }
    }
    latest
}

/// Feed `keys`, settle, and return the freshest `(tabline, current_tab)`.
async fn tabline_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> (Vec<TabCell>, u64) {
    while incoming.try_recv().is_ok() {} // discard earlier frames
    feed_sync(rpc, keys).await;
    if let Some(t) = drain_to_latest_tabline(incoming) {
        return t;
    }
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(t) = drain_to_latest_tabline(incoming) {
            return t;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

#[tokio::test]
async fn tabline_is_empty_with_one_tab() {
    let (rpc, mut incoming) = start().await;
    // Any input that produces a frame; one tab ⇒ no tabline (showtabline=1).
    let (cells, _) = tabline_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(cells.is_empty(), "no tabline is drawn with a single tab");
}

#[tokio::test]
async fn tabline_lists_both_tabs_with_labels_and_active_index() {
    let file = temp_file("label", "hello\n");
    let (rpc, mut incoming) = start().await;

    let (cells, current) =
        tabline_after(&rpc, &mut incoming, &format!(":tabedit {file}<CR>")).await;
    assert_eq!(cells.len(), 2, "two tabs ⇒ a two-cell tabline");
    assert_eq!(current, 1, "the new (second) tab is active");

    // Tab 1 is the seeded [No Name]; tab 2 shows the edited file's basename.
    assert_eq!(cells[0].label, "[No Name]");
    let base = std::path::Path::new(&file)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(cells[1].label, base, "the new tab labels its file");
    assert_eq!(cells[1].window_count, 1);

    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn tabline_marks_a_modified_buffer() {
    let (rpc, mut incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>").await;
    // Edit the new tab's buffer; its tabline cell gains the `+` modified flag.
    let (cells, current) = tabline_after(&rpc, &mut incoming, "ihi<Esc>").await;
    assert_eq!(current, 1);
    assert!(
        cells[1].modified,
        "the edited tab is flagged modified: {cells:?}"
    );
    assert!(!cells[0].modified, "the untouched tab is not");
}
