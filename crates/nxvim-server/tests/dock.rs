//! Behavior tests for permanent **docks** — nxvim's VSCode-style edge panels —
//! driven black-box over RPC exactly like `tabs.rs` / `windows`-style suites.
//!
//! Docks are global (cross-tab) editable window regions pinned to a screen edge.
//! `nx.dock.open/close/focus` create and address them; `<C-w><C-w>` crosses focus
//! between the main area and the docks while single `<C-w>` stays within the
//! focused layer. These tests drive that surface and assert on buffer content,
//! the window list, and the projected `redraw`.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{drain_to_latest_redraw, exec_lua, feed, lines, map_get, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

async fn req(rpc: &Rpc, method: &str, args: Vec<Value>) -> Value {
    rpc.request(method, args).await.expect(method)
}

/// `nvim_list_wins` as a vec of handles.
async fn win_count(rpc: &Rpc) -> usize {
    match req(rpc, "nvim_list_wins", vec![]).await {
        Value::Array(a) => a.len(),
        v => panic!("expected array, got {v:?}"),
    }
}

/// The latest redraw map (any frame).
fn latest(incoming: &mut UnboundedReceiver<Incoming>) -> Vec<(Value, Value)> {
    drain_to_latest_redraw(incoming, |_| true).expect("a redraw frame")
}

/// The set of window `region` strings present in a redraw map.
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

fn band(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
}

#[tokio::test]
async fn open_left_dock_adds_a_window_and_reserves_a_band() {
    let (rpc, mut incoming) = start().await;
    assert_eq!(win_count(&rpc).await, 1, "one window at startup");

    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    assert_eq!(win_count(&rpc).await, 2, "the dock adds a window");

    let rd = latest(&mut incoming);
    assert_eq!(band(&rd, "dock_left"), 20, "left dock reserves its width");
    let regs = regions(&rd);
    assert!(
        regs.iter().any(|r| r == "main"),
        "main window present: {regs:?}"
    );
    assert!(
        regs.iter().any(|r| r == "dock_left"),
        "a dock_left window present: {regs:?}"
    );
}

#[tokio::test]
async fn focus_crosses_into_and_out_of_a_dock() {
    let (rpc, _incoming) = start().await;
    // Type into the main buffer.
    feed(&rpc, "imain<Esc>");
    assert_eq!(lines(&rpc).await, vec!["main"]);

    // Opening a dock focuses it; typing lands in the dock's (scratch) buffer.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["dock"],
        "edits go to the dock buffer"
    );

    // From a dock any directional `<C-w><C-w>` returns to the main area.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(lines(&rpc).await, vec!["main"], "back in the main buffer");

    // `<C-w><C-w>h` from the main area focuses the left dock again.
    feed(&rpc, "<C-w><C-w>h");
    assert_eq!(lines(&rpc).await, vec!["dock"], "back in the dock buffer");
}

#[tokio::test]
async fn single_ctrl_w_splits_within_the_focused_dock() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await;
    assert_eq!(win_count(&rpc).await, 2);

    // Focus is in the dock; a single `<C-w>s` splits *within* the dock.
    feed(&rpc, "<C-w>s");
    assert_eq!(win_count(&rpc).await, 3, "dock gained a split window");
}

#[tokio::test]
async fn double_ctrl_w_v_from_main_splits_the_last_dock() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'right', size = 30 }").await;
    // Cross back to main (dock -> main).
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(win_count(&rpc).await, 2);
    // `<C-w><C-w>v` crosses to the last-focused dock and vsplits there.
    feed(&rpc, "<C-w><C-w>v");
    assert_eq!(win_count(&rpc).await, 3, "the right dock was split");
}

#[tokio::test]
async fn closing_a_dock_reclaims_its_band() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    assert_eq!(win_count(&rpc).await, 2);

    exec_lua(&rpc, "nx.dock.close('left')").await;
    assert_eq!(win_count(&rpc).await, 1, "the dock window is gone");
    let rd = latest(&mut incoming);
    assert_eq!(band(&rd, "dock_left"), 0, "the left band is reclaimed");
    assert!(
        !regions(&rd).iter().any(|r| r == "dock_left"),
        "no dock_left window remains"
    );
}

#[tokio::test]
async fn closing_the_last_dock_window_with_ctrl_w_c_closes_the_dock() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 8 }").await;
    assert_eq!(win_count(&rpc).await, 2);
    // Focused in the dock with one window: `<C-w>c` closes the whole dock.
    feed(&rpc, "<C-w>c");
    assert_eq!(win_count(&rpc).await, 1, "the dock collapsed");
}

#[tokio::test]
async fn dock_is_global_across_tabs() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    // New tab: must not disturb the dock (and must not panic while a dock is
    // focused — `new_tab` crosses to main first).
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    assert!(
        win_count(&rpc).await >= 2,
        "the dock window still exists on the new tab"
    );
    let rd = latest(&mut incoming);
    assert_eq!(band(&rd, "dock_left"), 20, "dock band shows on the new tab");
    assert!(
        regions(&rd).iter().any(|r| r == "dock_left"),
        "dock window rendered on the new tab"
    );
}

#[tokio::test]
async fn tab_switch_while_dock_focused_does_not_corrupt_state() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain-one<Esc>");
    // Open and stay focused in the dock, then switch tabs (the risky path).
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    // The new tab's main buffer is empty and editable; no panic occurred.
    feed(&rpc, "inew-tab<Esc>");
    assert_eq!(lines(&rpc).await, vec!["new-tab"]);
    // Back to the first tab: its main buffer survived intact.
    exec_lua(&rpc, "vim.cmd('tabprevious')").await;
    assert_eq!(lines(&rpc).await, vec!["main-one"]);
}

#[tokio::test]
async fn dock_open_ex_command_opens_a_dock() {
    let (rpc, _incoming) = start().await;
    // The `:DockOpen` ex-command (Lua prelude wrapper over `nx.dock.open`).
    exec_lua(&rpc, "vim.cmd('DockOpen right 24')").await;
    assert_eq!(win_count(&rpc).await, 2, ":DockOpen opened the right dock");
}

#[tokio::test]
async fn invalid_dock_side_is_reported_not_silently_ignored() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'sideways', size = 10 }").await;
    assert_eq!(win_count(&rpc).await, 1, "no dock opened for a bad side");
    let rd = latest(&mut incoming);
    let msg = map_get(&rd, "message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        msg.contains("Invalid dock side"),
        "a loud error is shown, got {msg:?}"
    );
}

#[tokio::test]
async fn example_config_opens_its_docks() {
    // Run the shipped `examples/dock/init.lua` end-to-end: it must load without
    // error and open its left + bottom docks (guards the example against drift).
    let (rpc, _incoming) = start().await;
    let init = include_str!("../../../examples/dock/init.lua");
    exec_lua(&rpc, init).await;
    assert_eq!(
        win_count(&rpc).await,
        3,
        "the example opens a left and a bottom dock (+ main)"
    );
}

#[tokio::test]
async fn four_docks_keep_a_nondegenerate_main_area() {
    let (rpc, mut incoming) = start().await;
    for side in ["left", "right", "top", "bottom"] {
        exec_lua(
            &rpc,
            &format!("nx.dock.open{{ side = '{side}', size = 6 }}"),
        )
        .await;
    }
    // Five windows: main + four docks.
    assert_eq!(win_count(&rpc).await, 5);
    let rd = latest(&mut incoming);
    for (key, present) in [
        ("dock_left", "dock_left"),
        ("dock_right", "dock_right"),
        ("dock_top", "dock_top"),
        ("dock_bottom", "dock_bottom"),
    ] {
        assert!(band(&rd, key) > 0, "{key} reserved");
        assert!(
            regions(&rd).iter().any(|r| r == present),
            "{present} rendered"
        );
    }
    // The main window keeps a positive rect.
    let wins = match map_get(&rd, "windows") {
        Some(Value::Array(a)) => a.clone(),
        _ => panic!("windows array"),
    };
    let main = wins
        .iter()
        .find_map(|w| match w {
            Value::Map(m) if map_get(m, "region").and_then(Value::as_str) == Some("main") => {
                Some(m.clone())
            }
            _ => None,
        })
        .expect("a main window");
    let rect = match map_get(&main, "rect") {
        Some(Value::Map(r)) => r.clone(),
        _ => panic!("main rect"),
    };
    assert!(map_get(&rect, "width").and_then(Value::as_u64).unwrap() >= 1);
    assert!(map_get(&rect, "height").and_then(Value::as_u64).unwrap() >= 1);
}
