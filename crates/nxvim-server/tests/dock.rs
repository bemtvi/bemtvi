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
use nxvim_test_harness::{
    drain_to_latest_redraw, exec_lua, feed, feed_mouse, lines, map_get, start_attached,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

async fn req(rpc: &Rpc, method: &str, args: Vec<Value>) -> Value {
    rpc.request(method, args).await.expect(method)
}

/// Feed `keys` and wait for the editor to settle (a `nvim_get_mode` barrier), so a
/// following redraw drain sees this input's frame rather than a stale one.
async fn feed_sync(rpc: &Rpc, keys: &str) {
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
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

/// The rect height of the first window painted in `region` (`main`/`dock_left`/…).
fn region_win_height(map: &[(Value, Value)], region: &str) -> Option<u64> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return None;
    };
    wins.iter().find_map(|w| {
        let Value::Map(m) = w else { return None };
        if map_get(m, "region").and_then(Value::as_str) != Some(region) {
            return None;
        }
        let Some(Value::Map(r)) = map_get(m, "rect") else {
            return None;
        };
        map_get(r, "height").and_then(Value::as_u64)
    })
}

/// One region's sub-map inside the redraw `region_tablines` (key `main`/`left`/
/// `right`/`top`/`bottom`).
fn region_tabline<'a>(map: &'a [(Value, Value)], region: &str) -> Option<&'a [(Value, Value)]> {
    let Some(Value::Map(rts)) = map_get(map, "region_tablines") else {
        return None;
    };
    match map_get(rts, region) {
        Some(Value::Map(r)) => Some(r),
        _ => None,
    }
}

/// How many tab cells a region's tabline projects (`0` when hidden — e.g. a
/// single-tab region at the default `showtabline`).
fn region_tab_count(map: &[(Value, Value)], region: &str) -> usize {
    match region_tabline(map, region).and_then(|r| map_get(r, "tabs")) {
        Some(Value::Array(a)) => a.len(),
        _ => 0,
    }
}

/// A region's active tab index, as projected in `region_tablines`.
fn region_current(map: &[(Value, Value)], region: &str) -> usize {
    region_tabline(map, region)
        .and_then(|r| map_get(r, "current"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

/// A region's dock title, as projected in `region_tablines`.
fn region_title(map: &[(Value, Value)], region: &str) -> String {
    region_tabline(map, region)
        .and_then(|r| map_get(r, "title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
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

/// A left-click inside a dock's window focuses that dock (and places the cursor),
/// even when the main area is focused — the mouse analogue of `<C-w><C-w>h`.
#[tokio::test]
async fn click_in_a_dock_focuses_it() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    // Cross to the main area so the dock is not focused.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(
        lines(&rpc).await,
        vec!["main"],
        "main is focused before the click"
    );
    // The left dock's window occupies cols 0..20, rows 0.. (single-tab → no dock
    // tabline, and main's tabline is hidden at one tab). Click inside it.
    feed_mouse(&rpc, "left", "press", 3, 5);
    assert_eq!(
        lines(&rpc).await,
        vec!["dock"],
        "the click focused the dock"
    );
    // Edits now land in the dock buffer.
    feed(&rpc, "A!<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["dock!"],
        "typing goes to the clicked dock"
    );
}

/// A left-click in the main area focuses it back from a focused dock — the inverse
/// crossing, by mouse.
#[tokio::test]
async fn click_in_main_focuses_it_from_a_dock() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    // Open the dock (it takes focus) and leave focus there.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["dock"],
        "the dock is focused before the click"
    );
    // The main area sits to the right of the 20-col dock (+ its separator), so it
    // starts at col 21; click well inside it.
    feed_mouse(&rpc, "left", "press", 3, 40);
    assert_eq!(
        lines(&rpc).await,
        vec!["main"],
        "the click focused the main area"
    );
    feed(&rpc, "A!<Esc>");
    assert_eq!(lines(&rpc).await, vec!["main!"], "typing goes back to main");
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
async fn dock_is_global_across_main_tabs() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    // Cross back to the main area, then open a new *main* tab. Docks are global —
    // they live outside the main tab stack, so the dock must persist untouched on
    // the new main tab.
    feed(&rpc, "<C-w><C-w>l");
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
async fn tabnew_while_dock_focused_adds_a_dock_tab_not_a_main_tab() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain-one<Esc>");
    // Open and stay focused in the dock, then `:tabnew` — it must add a *dock* tab
    // (the focused region), not a main tab.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock-one<Esc>");
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    feed(&rpc, "idock-two<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["dock-two"],
        "typing lands in the dock's new tab"
    );
    // `gT` cycles the dock's *own* tabs back to its first tab.
    feed(&rpc, "gT");
    assert_eq!(
        lines(&rpc).await,
        vec!["dock-one"],
        "gT cycles only the focused dock's tabs"
    );
    // Cross to main: its single tab and buffer are untouched by the dock's tabbing.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(
        lines(&rpc).await,
        vec!["main-one"],
        "the main area never gained a tab"
    );
}

#[tokio::test]
async fn dock_and_main_tab_counts_are_independent() {
    let (rpc, mut incoming) = start().await;
    // Always-on tablines so single-tab regions still project their cell count.
    feed_sync(&rpc, ":set showtabline=2<CR>").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    // Dock focused: two `:tabnew`s give the dock three tabs.
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    let rd = latest(&mut incoming);
    assert_eq!(region_tab_count(&rd, "left"), 3, "the dock has three tabs");
    assert_eq!(region_tab_count(&rd, "main"), 1, "main is left at one tab");
    assert_eq!(
        region_current(&rd, "left"),
        2,
        "the dock's last tab is active"
    );
}

#[tokio::test]
async fn gt_in_one_region_leaves_the_other_regions_active_tab() {
    let (rpc, mut incoming) = start().await;
    feed_sync(&rpc, ":set showtabline=2<CR>").await;
    // Main gets a second tab (active index 1), then we open a dock and give it a
    // second tab (active index 1) while the dock is focused.
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 6 }").await;
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    // `gT` in the focused dock wraps it back to tab 0; main's active tab is intact.
    feed_sync(&rpc, "gT").await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_current(&rd, "bottom"),
        0,
        "gT moved the dock's active tab"
    );
    assert_eq!(
        region_current(&rd, "main"),
        1,
        "main's active tab is unchanged"
    );
}

#[tokio::test]
async fn a_dock_tabline_shrinks_its_window_by_a_row() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 8 }").await;
    let rd = latest(&mut incoming);
    let before = region_win_height(&rd, "dock_bottom").expect("a dock window");
    // A second dock tab makes the dock's own tabline appear (showtabline=1), eating
    // the band's first row — the dock tree lays out one row shorter below it.
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    let rd = latest(&mut incoming);
    let after = region_win_height(&rd, "dock_bottom").expect("a dock window");
    assert_eq!(
        after,
        before - 1,
        "the dock tree gave its top row to its tabline"
    );
}

#[tokio::test]
async fn closing_the_docks_last_tab_closes_the_dock() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    assert_eq!(win_count(&rpc).await, 2, "main + dock");
    // Give the dock a second tab, then close tabs: the first close drops a tab but
    // keeps the dock; closing its *last* tab collapses the whole dock.
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    assert_eq!(win_count(&rpc).await, 3, "main + two dock-tab windows");
    exec_lua(&rpc, "vim.cmd('tabclose')").await;
    assert_eq!(win_count(&rpc).await, 2, "dock survives with one tab left");
    exec_lua(&rpc, "vim.cmd('tabclose')").await;
    assert_eq!(
        win_count(&rpc).await,
        1,
        "closing the dock's last tab closes the dock"
    );
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
async fn per_region_tabs_example_config_runs() {
    // Run the shipped examples/per-region-tabs/init.lua end-to-end: it opens its
    // left + bottom docks (each with an always-on titled tabline), and its `:T`
    // command grows only the focused region's own tab stack (guards the example).
    let (rpc, mut incoming) = start().await;
    let init = include_str!("../../../examples/per-region-tabs/init.lua");
    exec_lua(&rpc, init).await;
    assert_eq!(win_count(&rpc).await, 3, "main + a left and a bottom dock");
    let rd = latest(&mut incoming);
    // All three regions show their always-on strips (showtabline=2), each with one
    // tab to start; the docks carry their titles.
    assert_eq!(region_tab_count(&rd, "main"), 1, "main's strip is on");
    assert_eq!(
        region_tab_count(&rd, "left"),
        1,
        "the left dock's strip is on"
    );
    assert_eq!(
        region_tab_count(&rd, "bottom"),
        1,
        "the bottom dock's strip is on"
    );
    assert_eq!(region_title(&rd, "left"), "EXPLORER");
    assert_eq!(region_title(&rd, "bottom"), "TERMINAL");
    // Focus the bottom tray and grow ITS tab stack with the example's `:T` — the
    // other regions' tablines stay put (per-region tab independence).
    exec_lua(&rpc, "nx.dock.focus('bottom')").await;
    feed_sync(&rpc, ":T 2<CR>").await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_tab_count(&rd, "bottom"),
        3,
        ":T added two tabs to the focused bottom dock"
    );
    assert_eq!(
        region_tab_count(&rd, "main"),
        1,
        "main's tabs are untouched"
    );
    assert_eq!(
        region_tab_count(&rd, "left"),
        1,
        "the left dock's tabs are untouched"
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

// ----- Phase 6: per-dock options (the dock scope) ---------------------------

#[tokio::test]
async fn per_dock_showtabline_override_forces_the_strip() {
    let (rpc, mut incoming) = start().await;
    // Default showtabline=1: a single-tab dock shows no tabline.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    let rd = latest(&mut incoming);
    assert_eq!(region_tab_count(&rd, "left"), 0, "1 tab, default: no strip");
    // Per-dock showtabline=2 forces this dock's strip on, even with one tab.
    exec_lua(&rpc, "nx.dock.opt('left').showtabline = 2").await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_tab_count(&rd, "left"),
        1,
        "override 2: strip shows the lone tab"
    );
    // Main still follows the global default (no strip with its single tab).
    assert_eq!(
        region_tab_count(&rd, "main"),
        0,
        "main unaffected by the dock override"
    );
}

#[tokio::test]
async fn per_dock_showtabline_zero_hides_even_with_two_tabs() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, showtabline = 0 }",
    )
    .await;
    exec_lua(&rpc, "vim.cmd('tabnew')").await; // two dock tabs
    let rd = latest(&mut incoming);
    assert_eq!(
        region_tab_count(&rd, "left"),
        0,
        "override 0 hides the strip"
    );
}

#[tokio::test]
async fn dock_size_option_resizes_the_band() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 8 }").await;
    let rd = latest(&mut incoming);
    assert_eq!(band(&rd, "dock_bottom"), 8, "opened at size 8");
    exec_lua(&rpc, "nx.dock.opt('bottom').size = 15").await;
    let rd = latest(&mut incoming);
    assert_eq!(band(&rd, "dock_bottom"), 15, "size option regrew the band");
}

#[tokio::test]
async fn dock_title_projects_and_forces_the_strip() {
    let (rpc, mut incoming) = start().await;
    // A title shows the strip even with one tab (and projects in region_tablines).
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, title = 'EXPLORER' }",
    )
    .await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_title(&rd, "left"),
        "EXPLORER",
        "the dock title projects"
    );
    assert!(
        region_tab_count(&rd, "left") >= 1,
        "the title forces the strip on"
    );
}

/// A left-click on an open dock's own tabline switches *that dock's* active tab
/// and crosses focus into the dock — even while the main area is focused. The
/// per-region generalization of vim's tabline click.
#[tokio::test]
async fn click_a_dock_tabline_switches_that_docks_tab() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    // A bottom dock with two (empty, unnamed) tabs: at the default showtabline=1
    // its own tabline shows once it has the second tab; current tab = 1.
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 8 }").await;
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    // Cross back to the main area; the dock keeps its active tab (1).
    feed_sync(&rpc, "<C-w><C-w>k").await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_current(&rd, "bottom"),
        1,
        "the dock starts on its 2nd tab"
    );
    // Geometry: the test attaches the full 24-row height as the windows area (a
    // real client reserves the cmdline itself), no top dock, and a single-tab main
    // (its tabline hidden). The middle band is 24 − 9 (bottom band) = 15 rows, then
    // the bottom band spends its first row on the dock separator, so the dock's own
    // tabline is row 16. Two ` [No Name] ` cells → tab 0 covers cols 0..11.
    feed_mouse(&rpc, "left", "press", 16, 3);
    req(&rpc, "nvim_get_mode", vec![]).await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_current(&rd, "bottom"),
        0,
        "the dock-tabline click switched the dock to its first tab"
    );
    // Focus crossed into the dock: typing lands in its (empty) first tab, not main.
    feed(&rpc, "ihi<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["hi"],
        "focus crossed into the clicked dock's tab"
    );
}

/// The main area's tabline click still works when a **top dock** offsets it off
/// row 0 — the regression the pre-dock hit-test had (it assumed the main tabline
/// was always row 0). A click on the top dock's own band (row 0) must not switch
/// main's tab; the click on the main tabline's real row must.
#[tokio::test]
async fn main_tabline_click_survives_a_top_dock_offset() {
    let (rpc, mut incoming) = start().await;
    // Main gets a 2nd tab (active index 1) so its tabline shows; then a 3-row top
    // dock pushes the main tabline down to row 4 (top band 3 + its 1-row separator).
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'top', size = 3 }").await;
    feed_sync(&rpc, "itop<Esc>").await; // mark the dock's buffer to tell focus apart
    let rd = latest(&mut incoming);
    assert_eq!(region_tab_count(&rd, "main"), 2, "main has two tabs");
    assert_eq!(region_current(&rd, "main"), 1, "main starts on its 2nd tab");
    // Row 0 is the top dock's own band (its single tab shows no tabline) — a click
    // there is not the main tabline and must not switch main's tab.
    feed_mouse(&rpc, "left", "press", 0, 3);
    req(&rpc, "nvim_get_mode", vec![]).await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_current(&rd, "main"),
        1,
        "a click in the top dock's band left main's tab alone"
    );
    // The main tabline really sits at row 4; clicking tab 0's cell (cols 0..11)
    // switches main to it and crosses focus back to main.
    feed_mouse(&rpc, "left", "press", 4, 3);
    req(&rpc, "nvim_get_mode", vec![]).await;
    let rd = latest(&mut incoming);
    assert_eq!(
        region_current(&rd, "main"),
        0,
        "the top-dock-offset main tabline is still clickable"
    );
    feed(&rpc, "ix<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["x"],
        "focus crossed back to main's first tab"
    );
}

#[tokio::test]
async fn dock_winhighlight_is_reported_not_silently_ignored() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    exec_lua(&rpc, "nx.dock.opt('left').winhighlight = 'Normal:NormalSB'").await;
    let rd = latest(&mut incoming);
    let msg = map_get(&rd, "message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        msg.contains("winhighlight") && msg.contains("not implemented"),
        "winhighlight fails loud, got {msg:?}"
    );
}
