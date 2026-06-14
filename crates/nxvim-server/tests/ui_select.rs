//! Behavior tests for `nx.ui.select` (alias `vim.ui.select`) — the floating
//! selectable-list widget (`docs/specs/2026-06-14-nx-ui-float-widget.md`).
//!
//! Black-box like the rest: a real server sources an `init.lua`, the menu is
//! driven over the same msgpack-RPC a UI uses, and the assertions are on the
//! captured `on_choice` result (read back through `nvim_exec_lua`) and the
//! projected `menu` redraw surface. The callback side-effects round-trip through
//! request/reply so they need no redraw timing; the surface check polls for the
//! latest redraw (the reader task ferries notifications asynchronously).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, map_get, spawn, temp_dir,
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
async fn confirm_returns_chosen_item_and_one_based_index() {
    let dir = temp_dir("ui_select_confirm");
    let (rpc, _incoming) = start(&dir, "").await;

    // Open a three-item chooser; the callback records the choice into globals.
    exec_lua(
        &rpc,
        "_G.item, _G.idx, _G.called = nil, nil, false
         nx.ui.select({ 'alpha', 'beta', 'gamma' }, {}, function(item, idx)
           _G.item, _G.idx, _G.called = item, idx, true
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
    // 1-based index, as neovim's vim.ui.select hands its callback.
    assert_eq!(exec_lua(&rpc, "return _G.idx").await.as_u64(), Some(2));
}

#[tokio::test]
async fn cancel_fires_callback_with_nil() {
    let dir = temp_dir("ui_select_cancel");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.item, _G.called = 'unset', false
         nx.ui.select({ 'a', 'b' }, {}, function(item) _G.item, _G.called = item, true end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // The callback fired (so a caller can clean up) but with no item.
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

    // Items are tables; format_item renders a label, but on_choice must receive
    // the original table — only the index crosses the bridge.
    exec_lua(
        &rpc,
        "_G.chosen = nil
         nx.ui.select(
           { { id = 10 }, { id = 20 }, { id = 30 } },
           { format_item = function(it) return 'row ' .. it.id end },
           function(item) _G.chosen = item.id end)",
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

    // An empty list resolves to a cancel in the Lua wrapper, synchronously,
    // without queuing a menu — so `called` is already true on return.
    exec_lua(
        &rpc,
        "_G.item, _G.called = 'unset', false
         nx.ui.select({}, {}, function(item) _G.item, _G.called = item, true end)",
    )
    .await;

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(exec_lua(&rpc, "return _G.item").await, Value::Nil);
}

#[tokio::test]
async fn example_config_loads_and_opens_a_menu() {
    // The shipped `examples/ui-select` config must load and wire its leader maps.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ui-select")
        .canonicalize()
        .expect("examples/ui-select dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // `\p` (leader = "\") opens the fruit chooser the example defines.
    feed(&rpc, "\\p");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("the example's \\p map opens a menu");
    let menu = menu_of(&map);
    let items = match map_get(&menu, "items") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
        other => panic!("expected items array, got {other:?}"),
    };
    assert_eq!(items, vec!["apple", "banana", "cherry"]);
}

#[tokio::test]
async fn menu_surface_projects_items_and_tracks_selection() {
    let dir = temp_dir("ui_select_surface");
    let (rpc, mut incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "nx.ui.select({ 'one', 'two', 'three' }, {}, function() end)",
    )
    .await;

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
