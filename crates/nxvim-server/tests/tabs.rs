//! Behavior tests for tab pages, driven the way a real client drives the editor
//! (black-box RPC, exactly like `windows.rs` / `editing.rs`).
//!
//! Phase 1 pinned the model plus the **read-only** `nvim_tabpage_*` surface.
//! Phase 2 makes tabs real: creation (`:tabnew`/`:tabedit`, `<C-w>T`), switching
//! (`gt`/`gT`/`{count}gt`, `:tabnext`/`:tabprevious`/`:tablast`,
//! `nvim_set_current_tabpage`), closing (`:tabclose`/`:tabonly`), and the
//! tabline projected into the `redraw`. These tests drive those over RPC.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, map_get, start_attached};
use rmpv::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread and return a connected client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
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
    rpc.request("nx_input", vec![Value::from(keys)])
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

// ----- Phase 3: Lua tab API, showtabline & last-tab quit --------------------

#[tokio::test]
async fn lua_tabpage_reads_resolve_from_the_mirror() {
    let (rpc, _incoming) = start().await;
    // Two tabs, the second active (`:tabnew` switches to it).
    feed_sync(&rpc, ":tabnew<CR>").await;

    // `nvim_list_tabpages` / `nvim_get_current_tabpage` read the `nx._tabs`
    // mirror, agreeing with the RPC surface.
    let list = exec_lua(&rpc, "return vim.api.nvim_list_tabpages()").await;
    assert_eq!(handles(&list), vec![1, 2], "Lua lists both tabs in order");
    let cur = exec_lua(&rpc, "return vim.api.nvim_get_current_tabpage()").await;
    assert_eq!(handle(&cur), 2, "Lua sees the new tab as current");

    // Number / validity against the mirror.
    let num = exec_lua(&rpc, "return vim.api.nvim_tabpage_get_number(1)").await;
    assert_eq!(handle(&num), 1, "tab 1 is number 1");
    let valid = exec_lua(&rpc, "return vim.api.nvim_tabpage_is_valid(99)").await;
    assert_eq!(
        valid,
        Value::from(false),
        "an unknown tab is invalid in Lua"
    );

    // The per-tab window set, and `0` resolving to the current tab.
    let wins0 = handles(&exec_lua(&rpc, "return vim.api.nvim_tabpage_list_wins(0)").await);
    let win0 = handle(&exec_lua(&rpc, "return vim.api.nvim_tabpage_get_win(0)").await);
    assert_eq!(wins0.len(), 1, "the current tab owns one window");
    assert_eq!(wins0[0], win0, "its focused window is that window");
}

#[tokio::test]
async fn showtabline_zero_hides_the_tabline_even_with_two_tabs() {
    let (rpc, mut incoming) = start().await;
    feed_sync(&rpc, ":set showtabline=0<CR>").await;
    // Two tabs would normally draw a tabline; `showtabline=0` suppresses it.
    let (cells, _) = tabline_after(&rpc, &mut incoming, ":tabnew<CR>").await;
    assert!(
        cells.is_empty(),
        "showtabline=0 draws no tabline: {cells:?}"
    );

    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1, 2], "the tabs still exist, just unshown");
}

#[tokio::test]
async fn showtabline_two_shows_the_tabline_with_one_tab() {
    let (rpc, mut incoming) = start().await;
    // `showtabline=2` always draws the tabline — even with the single startup tab.
    let (cells, current) = tabline_after(&rpc, &mut incoming, ":set showtabline=2<CR>").await;
    assert_eq!(
        cells.len(),
        1,
        "showtabline=2 draws the lone tab: {cells:?}"
    );
    assert_eq!(current, 0, "the only tab is active (index 0)");
    assert_eq!(cells[0].label, "[No Name]");
}

/// Feed `keys`, settle, and return the freshest redraw's `message` line.
async fn message_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> String {
    while incoming.try_recv().is_ok() {}
    feed_sync(rpc, keys).await;
    for _ in 0..200 {
        let mut latest = None;
        while let Ok(msg) = incoming.try_recv() {
            if let Incoming::Notification { method, params } = msg {
                if method == "redraw" {
                    if let Some(Value::Map(map)) = params.into_iter().next() {
                        latest = map_get(&map, "message")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
            }
        }
        if let Some(m) = latest {
            return m;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no redraw with a message arrived for {keys:?}");
}

#[tokio::test]
async fn showtabline_query_echoes_the_value() {
    let (rpc, mut incoming) = start().await;
    feed_sync(&rpc, ":set showtabline=2<CR>").await;
    // `:set stal?` (the abbreviation) echoes the current value on the message line.
    let msg = message_after(&rpc, &mut incoming, ":set stal?<CR>").await;
    assert_eq!(
        msg, "showtabline=2",
        "the query echoes the canonical name=value"
    );
}

#[tokio::test]
async fn vim_o_showtabline_round_trips_and_drives_the_tabline() {
    let (rpc, mut incoming) = start().await;
    // Default reads as 1 (the core default, before any set).
    let def = exec_lua(&rpc, "return vim.o.showtabline").await;
    assert_eq!(def.as_u64(), Some(1), "vim.o.showtabline defaults to 1");

    // Writing 2 through vim.o reaches the core and draws the tabline with one tab.
    let (cells, _) = tabline_after(&rpc, &mut incoming, ":lua vim.o.showtabline = 2<CR>").await;
    assert_eq!(
        cells.len(),
        1,
        "vim.o write drew the lone-tab tabline: {cells:?}"
    );
    // And the read reflects the core value (the `stal` abbreviation too).
    let now = exec_lua(&rpc, "return vim.o.stal").await;
    assert_eq!(now.as_u64(), Some(2), "vim.o.stal reads back the set value");
}

#[tokio::test]
async fn showtabline_out_of_range_is_rejected_loudly_on_both_paths() {
    let (rpc, mut incoming) = start().await;

    // The `:set` path: above the range is E474, below it is E487 — and the value
    // is left unchanged (the default 1).
    let high = message_after(&rpc, &mut incoming, ":set showtabline=5<CR>").await;
    assert_eq!(high, "E474: Invalid argument: showtabline=5");
    let low = message_after(&rpc, &mut incoming, ":set showtabline=-1<CR>").await;
    assert_eq!(low, "E487: Argument must be positive: showtabline=-1");

    // The `vim.o` path rejects identically (the shared setter), so a bad write
    // from Lua surfaces the same error rather than silently no-op'ing.
    let lua_high = message_after(&rpc, &mut incoming, ":lua vim.o.showtabline = 5<CR>").await;
    assert_eq!(lua_high, "E474: Invalid argument: showtabline=5");

    // After all the rejected writes, the option is still its default.
    let still = exec_lua(&rpc, "return vim.o.showtabline").await;
    assert_eq!(
        still.as_u64(),
        Some(1),
        "a rejected write leaves the default"
    );
}

#[tokio::test]
async fn q_on_the_last_window_of_a_tab_closes_the_tab_not_the_editor() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, ":tabnew<CR>").await; // two tabs, on tab 2 (one window)

    // `:q` on the lone window of tab 2 closes the *tab*, dropping back to tab 1 —
    // the editor keeps running (other tabs exist).
    feed_sync(&rpc, ":q<CR>").await;
    let tabs = handles(&req(&rpc, "nvim_list_tabpages", vec![]).await);
    assert_eq!(tabs, vec![1], ":q closed tab 2, leaving tab 1");
    let cur = handle(&req(&rpc, "nvim_get_current_tabpage", vec![]).await);
    assert_eq!(cur, 1, "focus fell back to the surviving tab");

    // The server is still responsive (it did not quit).
    let mode = req(&rpc, "nvim_get_mode", vec![]).await;
    assert!(matches!(mode, Value::Map(_)), "the editor is still alive");
}

// ----- Phase 4: reorder & positional close ----------------------------------

/// Open two extra tabs so the order is [1, 2, 3], leaving focus on the last
/// (tab 3) — the state the reorder/close tests start from.
async fn three_tabs(rpc: &Rpc) {
    feed_sync(rpc, ":tabnew<CR>").await; // [1, 2], on 2
    feed_sync(rpc, ":tabnew<CR>").await; // [1, 2, 3], on 3
}

/// The current tabline order (tab ids, in tabline order).
async fn tab_order(rpc: &Rpc) -> Vec<u64> {
    handles(&req(rpc, "nvim_list_tabpages", vec![]).await)
}

/// The current tab's id.
async fn cur_tab(rpc: &Rpc) -> u64 {
    handle(&req(rpc, "nvim_get_current_tabpage", vec![]).await)
}

#[tokio::test]
async fn tabmove_to_first_and_last() {
    let (rpc, _incoming) = start().await;
    three_tabs(&rpc).await; // [1, 2, 3], current = 3

    // `:tabmove 0` sends the current tab to the front; it stays current.
    feed_sync(&rpc, ":tabmove 0<CR>").await;
    assert_eq!(tab_order(&rpc).await, vec![3, 1, 2]);
    assert_eq!(cur_tab(&rpc).await, 3, "the moved tab is still current");
    let num = handle(&req(&rpc, "nvim_tabpage_get_number", vec![Value::from(3u64)]).await);
    assert_eq!(num, 1, "tab 3 is now the first tab");

    // `:tabmove $` sends it back to the end.
    feed_sync(&rpc, ":tabmove $<CR>").await;
    assert_eq!(tab_order(&rpc).await, vec![1, 2, 3]);
    assert_eq!(cur_tab(&rpc).await, 3);
}

#[tokio::test]
async fn tabmove_relative_shifts_one_each_way() {
    let (rpc, _incoming) = start().await;
    three_tabs(&rpc).await;
    feed_sync(&rpc, ":tabfirst<CR>").await; // on tab 1

    feed_sync(&rpc, ":tabmove +1<CR>").await; // one place right
    assert_eq!(tab_order(&rpc).await, vec![2, 1, 3]);
    assert_eq!(cur_tab(&rpc).await, 1, "still on the moved tab");

    feed_sync(&rpc, ":tabmove -1<CR>").await; // back left
    assert_eq!(tab_order(&rpc).await, vec![1, 2, 3]);
}

#[tokio::test]
async fn tabmove_after_tab_n() {
    let (rpc, _incoming) = start().await;
    three_tabs(&rpc).await;
    feed_sync(&rpc, ":tabfirst<CR>").await; // on tab 1, [1, 2, 3]

    // `:tabmove 2` moves the current tab to after tab 2 -> [2, 1, 3].
    feed_sync(&rpc, ":tabmove 2<CR>").await;
    assert_eq!(tab_order(&rpc).await, vec![2, 1, 3]);
    assert_eq!(cur_tab(&rpc).await, 1);
}

#[tokio::test]
async fn tabmove_rejects_a_bad_arg_and_leaves_order() {
    let (rpc, mut incoming) = start().await;
    three_tabs(&rpc).await;
    let msg = message_after(&rpc, &mut incoming, ":tabmove xyz<CR>").await;
    assert_eq!(msg, "E474: Invalid argument: xyz");
    assert_eq!(
        tab_order(&rpc).await,
        vec![1, 2, 3],
        "a rejected move is a no-op"
    );
}

#[tokio::test]
async fn tabclose_n_closes_an_inactive_tab_keeping_focus() {
    let (rpc, _incoming) = start().await;
    three_tabs(&rpc).await; // [1, 2, 3], on tab 3

    // Closing tab 1 (not current) drops its stash; the live tab 3 is untouched.
    feed_sync(&rpc, ":tabclose 1<CR>").await;
    assert_eq!(tab_order(&rpc).await, vec![2, 3]);
    assert_eq!(cur_tab(&rpc).await, 3, "focus stays on tab 3");
}

#[tokio::test]
async fn tabclose_dollar_closes_the_last_tab() {
    let (rpc, _incoming) = start().await;
    three_tabs(&rpc).await;
    feed_sync(&rpc, ":tabfirst<CR>").await; // on tab 1

    feed_sync(&rpc, ":tabclose $<CR>").await; // close the last tab (3)
    assert_eq!(tab_order(&rpc).await, vec![1, 2]);
    assert_eq!(cur_tab(&rpc).await, 1, "focus stays on tab 1");
}

#[tokio::test]
async fn tabclose_refuses_last_tab_and_rejects_out_of_range() {
    let (rpc, mut incoming) = start().await;

    // With one tab, any close refuses (E784) before parsing the argument.
    let last = message_after(&rpc, &mut incoming, ":tabclose<CR>").await;
    assert_eq!(last, "E784: Cannot close last tab page");

    // With several tabs, an out-of-range number is E474 and closes nothing.
    three_tabs(&rpc).await;
    let bad = message_after(&rpc, &mut incoming, ":tabclose 9<CR>").await;
    assert_eq!(bad, "E474: Invalid argument: 9");
    assert_eq!(tab_order(&rpc).await, vec![1, 2, 3]);
}

// ----- Phase 4: the `:tab {cmd}` modifier and `:drop` / `:tab drop` ---------

/// The current buffer's handle (`nvim_get_current_buf`).
async fn cur_buf(rpc: &Rpc) -> u64 {
    handle(&req(rpc, "nvim_get_current_buf", vec![]).await)
}

/// The focused window's cursor as `(1-based row, 0-based col)`.
async fn cur_cursor(rpc: &Rpc) -> (u64, u64) {
    match req(rpc, "nvim_win_get_cursor", vec![Value::from(0u64)]).await {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0),
            a.get(1).and_then(Value::as_u64).unwrap_or(0),
        ),
        v => panic!("expected a cursor array, got {v:?}"),
    }
}

/// The current buffer's lines (`nvim_buf_get_lines`).
async fn cur_lines(rpc: &Rpc) -> Vec<String> {
    match req(
        rpc,
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn tab_split_clones_the_current_buffer_and_view_into_a_new_tab() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "iline one<CR>line two<CR>line three<Esc>").await;
    feed_sync(&rpc, "ggj").await; // land on line 2, deterministically
    let buf = cur_buf(&rpc).await;
    let pos = cur_cursor(&rpc).await;

    feed_sync(&rpc, ":tab split<CR>").await;

    assert_eq!(tab_order(&rpc).await, vec![1, 2], ":tab split adds a tab");
    assert_eq!(cur_tab(&rpc).await, 2, "the new tab is active");
    assert_eq!(
        cur_buf(&rpc).await,
        buf,
        ":tab split shows the same buffer (a cloned split), not a fresh one"
    );
    assert_eq!(
        cur_cursor(&rpc).await,
        pos,
        ":tab split preserves the cursor/view, unlike :tabnew"
    );
}

#[tokio::test]
async fn drop_focuses_an_existing_window_in_another_tab() {
    let file = temp_file("drop_jump", "alpha\nbeta\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":tabedit {file}<CR>")).await; // tab 2 shows the file
    let file_buf = cur_buf(&rpc).await;
    feed_sync(&rpc, ":tabfirst<CR>").await; // back to tab 1 ([No Name])
    assert_eq!(cur_tab(&rpc).await, 1);

    feed_sync(&rpc, &format!(":drop {file}<CR>")).await;

    assert_eq!(
        tab_order(&rpc).await,
        vec![1, 2],
        ":drop opens no new tab when the file is already shown"
    );
    assert_eq!(
        cur_tab(&rpc).await,
        2,
        ":drop jumps to the tab whose window shows the file"
    );
    assert_eq!(cur_buf(&rpc).await, file_buf);

    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn drop_edits_the_file_in_place_when_not_open_anywhere() {
    let file = temp_file("drop_edit", "alpha\nbeta\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":drop {file}<CR>")).await;

    assert_eq!(
        tab_order(&rpc).await,
        vec![1],
        ":drop of an unopened file makes no new tab"
    );
    assert_eq!(
        cur_lines(&rpc).await,
        vec!["alpha", "beta"],
        ":drop edits the file in the current window when it isn't open"
    );

    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn tab_drop_opens_an_unopened_file_in_a_new_tab() {
    let file = temp_file("tabdrop_new", "alpha\nbeta\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":tab drop {file}<CR>")).await;

    assert_eq!(
        tab_order(&rpc).await,
        vec![1, 2],
        ":tab drop of an unopened file opens a new tab"
    );
    assert_eq!(cur_tab(&rpc).await, 2);
    assert_eq!(cur_lines(&rpc).await, vec!["alpha", "beta"]);

    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn tab_drop_focuses_an_existing_window_instead_of_opening_a_tab() {
    let file = temp_file("tabdrop_jump", "alpha\nbeta\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":tabedit {file}<CR>")).await; // tab 2 shows the file
    let file_buf = cur_buf(&rpc).await;
    feed_sync(&rpc, ":tabfirst<CR>").await; // tab 1

    feed_sync(&rpc, &format!(":tab drop {file}<CR>")).await;

    assert_eq!(
        tab_order(&rpc).await,
        vec![1, 2],
        ":tab drop reuses the tab already showing the file (no third tab)"
    );
    assert_eq!(cur_tab(&rpc).await, 2);
    assert_eq!(cur_buf(&rpc).await, file_buf);

    let _ = std::fs::remove_file(&file);
}

// ----- `:bdelete` of a tab's last buffer closes the tab ('bdclosetab') --------

/// `:bd` on the only buffer shown in a tab — with other tabs open — closes the
/// tab page rather than loading a "random" sibling buffer into it (nxvim's
/// default 'bdclosetab'). The deleted buffer leaves the list; focus lands on a
/// surviving tab showing its own buffer.
#[tokio::test]
async fn bdelete_of_a_tabs_only_buffer_closes_the_tab() {
    let a = temp_file("bd_a", "aaa\n");
    let b = temp_file("bd_b", "bbb\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":edit {a}<CR>")).await; // tab 1 shows A
    feed_sync(&rpc, &format!(":tabedit {b}<CR>")).await; // tab 2 shows B, focused
    assert_eq!(tab_order(&rpc).await.len(), 2, "two tabs open");
    assert_eq!(cur_lines(&rpc).await, vec!["bbb"], "tab 2 shows B");

    feed_sync(&rpc, ":bdelete<CR>").await;

    assert_eq!(
        tab_order(&rpc).await.len(),
        1,
        "the tab closed with its last buffer, not a sibling loaded into it"
    );
    assert_eq!(
        cur_lines(&rpc).await,
        vec!["aaa"],
        "focus is on the surviving tab showing A"
    );

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// `:set nobdclosetab` restores the classic vim behavior: `:bd` keeps the tab
/// open and loads a sibling buffer into its window.
#[tokio::test]
async fn nobdclosetab_keeps_the_tab_and_loads_a_sibling() {
    let a = temp_file("nbd_a", "aaa\n");
    let b = temp_file("nbd_b", "bbb\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":edit {a}<CR>")).await; // tab 1 shows A
    feed_sync(&rpc, &format!(":tabedit {b}<CR>")).await; // tab 2 shows B, focused
    feed_sync(&rpc, ":set nobdclosetab<CR>").await;

    feed_sync(&rpc, ":bdelete<CR>").await;

    assert_eq!(
        tab_order(&rpc).await.len(),
        2,
        "with 'nobdclosetab' the tab stays open"
    );

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

/// A tab whose windows show *other* buffers besides the one being deleted is not
/// closed — only the focused window moves off the deleted buffer. 'bdclosetab'
/// closes a tab only when its every window showed the deleted buffer.
#[tokio::test]
async fn bdelete_does_not_close_a_tab_with_other_buffers_in_a_split() {
    let a = temp_file("bds_a", "aaa\n");
    let b = temp_file("bds_b", "bbb\n");
    let (rpc, _incoming) = start().await;

    feed_sync(&rpc, &format!(":edit {a}<CR>")).await; // tab 1 shows A
    feed_sync(&rpc, &format!(":tabedit {b}<CR>")).await; // tab 2 shows B (focused)
    feed_sync(&rpc, &format!(":split {a}<CR>")).await; // tab 2 now: A (top) / B (bottom)
    feed_sync(&rpc, "<C-w>j").await; // focus the B window
    assert_eq!(cur_lines(&rpc).await, vec!["bbb"], "focused on B");

    feed_sync(&rpc, ":bdelete<CR>").await;

    assert_eq!(
        tab_order(&rpc).await.len(),
        2,
        "the tab stays open: it still shows A in the other split"
    );

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}
