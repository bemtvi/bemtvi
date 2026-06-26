//! Behavior tests for `nx.ui.select` (alias `vim.ui.select`) — the floating
//! selectable-list widget (`docs/specs/2026-06-14-nx-ui-float-widget.md`).
//!
//! `nx.ui.select` is PROMISE-ONLY: `nx.ui.select(items, opts)` resolves to the
//! chosen item (nil on cancel). The `vim.ui.select` compat alias keeps neovim's
//! `(item, index)` callback shape.
//!
//! Black-box like the rest: a real server sources an `init.lua`, the menu is
//! driven over the same msgpack-RPC a UI uses, and the assertions are on the value
//! the promise's `:next` records (read back through `nvim_exec_lua`) and the
//! projected `menu` redraw surface. Delivering the menu result fires the resolver
//! and `apply_lua_effects` drains the `:next` reaction in the same tick, so the
//! effect is visible on the next ordered read; the surface check polls for the
//! latest redraw (the reader task ferries notifications asynchronously).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, drain_to_latest_redraw, exec_lua, feed, feed_mouse, map_get, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also the runtimepath), and return a connected client.
async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll for the latest redraw whose `menu` key is a map, retrying a bounded
/// number of times so the client reader task has settled (the take-latest
/// pattern the harness conventions require). `None` once the channel is dry and
/// no menu redraw was seen.
async fn poll_menu(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..40 {
        // A barrier flushes the server's queued redraw onto the wire.
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// The `menu` sub-map of a redraw (already known to be a map).
fn menu_of(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(map, "menu") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a menu map, got {other:?}"),
    }
}

#[tokio::test]
async fn select_promise_resolves_with_chosen_item() {
    let dir = temp_dir("ui_select_confirm");
    let (rpc, _incoming) = start(&dir, "").await;

    // Open a three-item chooser; the :next handler records the choice into globals.
    exec_lua(
        &rpc,
        "_G.item, _G.called = nil, false
         nx.ui.select({ 'alpha', 'beta', 'gamma' }, {}):next(function(item)
           _G.item, _G.called = item, true
         end)",
    )
    .await;

    // Move the highlight down one (alpha -> beta), then confirm.
    feed(&rpc, "j");
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.item").await.as_str(),
        Some("beta")
    );
}

#[tokio::test]
async fn vim_ui_select_callback_keeps_item_and_one_based_index() {
    let dir = temp_dir("ui_select_vim_alias");
    let (rpc, _incoming) = start(&dir, "").await;

    // The vim.ui.select compat alias keeps neovim's (item, index) callback shape —
    // plugins that read the 1-based index still work.
    exec_lua(
        &rpc,
        "_G.item, _G.idx = nil, nil
         vim.ui.select({ 'alpha', 'beta', 'gamma' }, {}, function(item, idx)
           _G.item, _G.idx = item, idx
         end)",
    )
    .await;

    feed(&rpc, "j");
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.item").await.as_str(),
        Some("beta")
    );
    assert_eq!(exec_lua(&rpc, "return _G.idx").await.as_u64(), Some(2));
}

#[tokio::test]
async fn nx_ui_select_rejects_the_old_callback_shape() {
    let dir = temp_dir("ui_select_guard");
    let (rpc, _incoming) = start(&dir, "").await;

    // Passing an on_choice to the promise-only nx.ui.select fails loudly.
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.ui.select({ 'a' }, {}, function() end) end)
         return ok == false and e or '<no error>'",
    )
    .await;
    assert!(
        err.as_str().unwrap_or("").contains("promise-only"),
        "expected a promise-only migration error, got {err:?}"
    );
}

#[tokio::test]
async fn cancel_resolves_with_nil() {
    let dir = temp_dir("ui_select_cancel");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.item, _G.called = 'unset', false
         nx.ui.select({ 'a', 'b' }, {}):next(function(item) _G.item, _G.called = item, true end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // The promise resolved (so a caller can clean up) but with no item.
    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(exec_lua(&rpc, "return _G.item").await, Value::Nil);
}

#[tokio::test]
async fn format_item_displays_label_but_callback_gets_original() {
    let dir = temp_dir("ui_select_format");
    let (rpc, _incoming) = start(&dir, "").await;

    // Items are tables; format_item renders a label, but the promise must resolve
    // to the original table — only the index crosses the bridge.
    exec_lua(
        &rpc,
        "_G.chosen = nil
         nx.ui.select(
           { { id = 10 }, { id = 20 }, { id = 30 } },
           { format_item = function(it) return 'row ' .. it.id end }
         ):next(function(item) _G.chosen = item.id end)",
    )
    .await;

    feed(&rpc, "G"); // jump to the last row (id = 30)
    feed(&rpc, "<CR>");

    assert_eq!(exec_lua(&rpc, "return _G.chosen").await.as_u64(), Some(30));
}

#[tokio::test]
async fn empty_list_cancels_without_opening() {
    let dir = temp_dir("ui_select_empty");
    let (rpc, _incoming) = start(&dir, "").await;

    // An empty list resolves to a cancel in the Lua wrapper without queuing a menu;
    // the resolve runs synchronously in the executor and the :next reaction drains
    // at convergence of this exec_lua tick — so `called` is true on the next read.
    exec_lua(
        &rpc,
        "_G.item, _G.called = 'unset', false
         nx.ui.select({}, {}):next(function(item) _G.item, _G.called = item, true end)",
    )
    .await;

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(exec_lua(&rpc, "return _G.item").await, Value::Nil);
}

#[tokio::test]
async fn menu_surface_projects_items_and_tracks_selection() {
    let dir = temp_dir("ui_select_surface");
    let (rpc, mut incoming) = start(&dir, "").await;

    // The returned promise is unconsumed here — we only assert on the menu surface.
    exec_lua(&rpc, "nx.ui.select({ 'one', 'two', 'three' }, {})").await;

    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a menu redraw surface");
    let menu = menu_of(&map);
    let items = match map_get(&menu, "items") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
        other => panic!("expected items array, got {other:?}"),
    };
    assert_eq!(items, vec!["one", "two", "three"]);
    assert_eq!(map_get(&menu, "selected").and_then(Value::as_u64), Some(0));

    // Moving the highlight tracks on the surface.
    feed(&rpc, "j");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a menu redraw surface after move");
    let menu = menu_of(&map);
    assert_eq!(map_get(&menu, "selected").and_then(Value::as_u64), Some(1));
}

// ── Mouse: a `select` grabs the mouse modally like a picker (Phase 2). It is
//    cursor-anchored with a full border and no prompt. ────────────────────────

fn menu_u64(menu: &[(Value, Value)], key: &str) -> usize {
    map_get(menu, key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("menu has a {key}")) as usize
}

#[tokio::test]
async fn clicking_a_select_row_highlights_then_confirms() {
    let dir = temp_dir("ui_select_mouse");
    let (rpc, mut incoming) = start(&dir, "").await;
    // Disable the number gutter so the box's text-area cells are global cells.
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(
        &rpc,
        "_G.item = nil
         nx.ui.select({ 'alpha', 'beta', 'gamma' }, {}):next(function(it) _G.item = it end)",
    )
    .await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("select opens"));

    // A promptless, full-bordered cursor popup: the first row sits one cell below the
    // box top (the top border), the content one cell past the left border.
    let row = menu_u64(&menu, "row");
    let col = menu_u64(&menu, "col");
    let (gamma_row, content_col) = (row + 1 + 2, col + 1);

    // Click "gamma": it highlights, nothing resolves yet.
    feed_mouse(&rpc, "left", "press", gamma_row, content_col);
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("redraw after click"),
    );
    assert_eq!(
        menu_u64(&menu, "selected"),
        2,
        "the clicked row is highlighted"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.item").await,
        Value::Nil,
        "highlighting does not resolve the chooser"
    );

    // Click the highlighted row again: the promise resolves with that item.
    feed_mouse(&rpc, "left", "press", gamma_row, content_col);
    assert_eq!(
        exec_lua(&rpc, "return _G.item").await.as_str(),
        Some("gamma"),
        "clicking the highlighted row confirms it"
    );
}

#[tokio::test]
async fn clicking_off_a_select_box_cancels_it() {
    let dir = temp_dir("ui_select_mouse_cancel");
    let (rpc, mut incoming) = start(&dir, "").await;
    command(&rpc, "set nonumber norelativenumber").await;

    exec_lua(
        &rpc,
        "_G.item, _G.called = nil, false
         nx.ui.select({ 'alpha', 'beta', 'gamma' }, {})
           :next(function(it) _G.item, _G.called = it, true end)",
    )
    .await;
    poll_menu(&rpc, &mut incoming).await.expect("select opens");

    // The cursor-anchored box never reaches the top-left corner — a press there
    // lands off it, which cancels the chooser (the promise resolves with nil).
    feed_mouse(&rpc, "left", "press", 0, 0);
    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true),
        "a click off the box resolves the chooser"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.item").await,
        Value::Nil,
        "a click off the box cancels (resolves with nil) without choosing a row"
    );
}
