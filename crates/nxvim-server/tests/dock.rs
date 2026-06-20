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
    command, cursor, drain_to_latest_redraw, exec_lua, feed, feed_mouse, lines, lua_u64, map_get,
    mode, serial_lock, start_attached, wait_redraw, write_temp,
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
    rpc.request("nx_input", vec![Value::from(keys)])
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

/// `nvim_list_bufs` count — to tell *hidden* (buffer stays loaded) from *closed*.
async fn buf_count(rpc: &Rpc) -> usize {
    match req(rpc, "nvim_list_bufs", vec![]).await {
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

/// The collapsed-dock chip labels projected in a redraw map (`hidden_docks`).
fn hidden_docks(map: &[(Value, Value)]) -> Vec<String> {
    match map_get(map, "hidden_docks") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
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

/// Whether the first window painted in `region` (`main`/`dock_left`/…) draws its
/// own status row (`status_visible`), or `None` when the region has no window.
fn region_status_visible(map: &[(Value, Value)], region: &str) -> Option<bool> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return None;
    };
    wins.iter().find_map(|w| {
        let Value::Map(m) = w else { return None };
        if map_get(m, "region").and_then(Value::as_str) != Some(region) {
            return None;
        }
        map_get(m, "status_visible").and_then(Value::as_bool)
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

/// The `<C-w><C-w>` dock chord works from **insert** mode: it leaves insert
/// cleanly and crosses to the dock, so navigation is one chord away even
/// mid-typing.
#[tokio::test]
async fn dock_chord_crosses_from_insert_mode() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>"); // dock buffer reads "dock"
    feed(&rpc, "<C-w><C-w>l"); // cross to the (empty) main area
    feed(&rpc, "imain"); // start typing in main, still in insert mode
    assert_eq!(mode(&rpc).await, "i", "in insert mode in main");

    // The chord, from insert, leaves insert and focuses the dock (Normal there).
    feed(&rpc, "<C-w><C-w>h");
    assert_eq!(mode(&rpc).await, "n", "the chord left insert mode");
    assert_eq!(lines(&rpc).await, vec!["dock"], "focused the dock");

    // Crossing back **resumes insert** where it was left — and the held `<C-w>`s
    // never reached the main buffer, so typing continues "main" cleanly.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(mode(&rpc).await, "i", "returning to main resumes insert");
    feed(&rpc, "tail<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["maintail"],
        "resumed insert appended where it left off"
    );
}

/// Regression: with a dock open, every keystroke runs `is_quickfix_buffer`, which
/// walks the *cross-layer* `window_ids()` (main tree + every open dock). It used to
/// index `self.windows` — the *current* layer's tree only — and panicked on the
/// dock's foreign-layer window id ("current window id is always valid"), killing the
/// server on the next input. A window-scoped location list and an open dock must
/// coexist: input keeps flowing and the loclist still resolves across the layers.
#[tokio::test]
async fn quickfix_context_survives_an_open_dock() {
    let (rpc, _incoming) = start().await;
    // The main window owns a one-entry location list (a real `loclist_bufnr` for
    // `qf_context_of_buffer` to scan for).
    exec_lua(
        &rpc,
        r#"vim.fn.setloclist(vim.api.nvim_get_current_win(),
             { { filename = "a.rs", lnum = 1, text = "A" } })"#,
    )
    .await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;

    // Before the fix this very input panicked the server: `is_quickfix_buffer` on
    // the input path called `windows.get` with the dock's id.
    feed(&rpc, "<C-w><C-w>l"); // cross into the (empty) main area
    feed(&rpc, "ihello<Esc>");
    assert_eq!(mode(&rpc).await, "n", "input still flows with a dock open");
    assert_eq!(lines(&rpc).await, vec!["hello"], "the edit landed in main");

    // The location list is intact and still resolvable across the layers.
    let n = exec_lua(&rpc, "return #vim.fn.getloclist(0)").await;
    assert_eq!(n.as_i64(), Some(1), "loclist survived the open dock");
}

/// The `<C-w><C-w>` dock chord works from **visual** mode (where a single `<C-w>`
/// is otherwise unbound): it drops the selection and crosses to the dock.
#[tokio::test]
async fn dock_chord_crosses_from_visual_mode() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    feed(&rpc, "<C-w><C-w>l"); // main area
    feed(&rpc, "ihello<Esc>0v$"); // select the line in visual mode
    assert_eq!(mode(&rpc).await, "v", "in visual mode in main");

    feed(&rpc, "<C-w><C-w>h");
    assert_eq!(mode(&rpc).await, "n", "the chord left visual mode");
    assert_eq!(lines(&rpc).await, vec!["dock"], "focused the dock");
}

/// A lone `<C-w>` in insert mode is *not* swallowed: when the next key isn't a
/// second `<C-w>`, the held one is replayed into insert, so typing past it is
/// unaffected.
#[tokio::test]
async fn lone_ctrl_w_in_insert_is_replayed() {
    let (rpc, _incoming) = start().await;
    // `<C-w>` in insert mode inserts a literal "w" (its pre-existing behavior);
    // the chord interceptor holds it one key then replays it, so `abc` still
    // lands right after it.
    feed(&rpc, "i<C-w>abc<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["wabc"],
        "the held <C-w> was replayed, typing continued"
    );
}

/// The `<C-w><C-w>` dock chord works from **terminal-job** mode: it leaves the
/// job for Normal and crosses to the dock, instead of forwarding `<C-w>` to the
/// child. Hermetic — only POSIX `cat` is spawned.
#[tokio::test]
async fn dock_chord_crosses_from_terminal_mode() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    feed(&rpc, "<C-w><C-w>l"); // main area
    command(&rpc, "terminal cat").await; // enters terminal-job mode in main
    assert_eq!(mode(&rpc).await, "t", "in terminal-job mode");

    feed(&rpc, "<C-w><C-w>h");
    assert_eq!(mode(&rpc).await, "n", "the chord left terminal mode");
    assert_eq!(lines(&rpc).await, vec!["dock"], "focused the dock");

    // Crossing back **resumes terminal-job mode** — you land back at the shell.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(
        mode(&rpc).await,
        "t",
        "returning to main resumes the terminal job"
    );
}

/// Visual mode is resumed (with its selection) when the dock chord crosses back —
/// the round trip is mode-transparent.
#[tokio::test]
async fn dock_chord_resumes_visual_mode_on_return() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    feed(&rpc, "<C-w><C-w>l"); // main area
    feed(&rpc, "ihello<Esc>0v$"); // visual selection over the line
    assert_eq!(mode(&rpc).await, "v", "visual in main");

    feed(&rpc, "<C-w><C-w>h"); // to the dock (Normal)
    assert_eq!(mode(&rpc).await, "n", "Normal in the dock");
    feed(&rpc, "<C-w><C-w>l"); // back to main
    assert_eq!(mode(&rpc).await, "v", "returning to main resumes visual");
    // The resumed selection is still over "hello": deleting it empties the line.
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec![""], "the resumed selection deleted");
}

/// An ordinary (non-chord) focus change does **not** resume a parked mode: leaving
/// a dock via the chord while in insert parks insert on that dock, but `<C-w>w`
/// (single-prefix) focus from the main area lands Normal, and the parked mode is
/// consumed so it can never resurrect later.
#[tokio::test]
async fn parked_mode_does_not_leak_into_plain_focus() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    // In the dock, start insert, then chord away to main: insert is parked on the
    // dock window.
    feed(&rpc, "idock");
    assert_eq!(mode(&rpc).await, "i", "insert in the dock");
    feed(&rpc, "<C-w><C-w>l"); // to main (Normal); dock parked insert
    assert_eq!(mode(&rpc).await, "n", "Normal in main");
    // A single-`<C-w>` window focus cycle is Normal-only and never resumes a parked
    // mode; crossing back via the chord now finds the park already cleared.
    feed(&rpc, "<C-w><C-w>h"); // back to the dock via the chord
    assert_eq!(mode(&rpc).await, "i", "the dock resumes its parked insert");
    feed(&rpc, "<Esc>"); // leave insert in the dock; nothing parked now
    feed(&rpc, "<C-w><C-w>l"); // to main
    feed(&rpc, "<C-w><C-w>h"); // back to the dock — must be Normal, not insert
    assert_eq!(mode(&rpc).await, "n", "no stale insert resurrected");
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

/// A left-click on a dock window's status line focuses that dock (vim focuses a
/// window when its status line is clicked), without entering the text.
#[tokio::test]
async fn click_a_dock_status_line_focuses_it() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>");
    feed(&rpc, "<C-w><C-w>l"); // focus main
    assert_eq!(
        lines(&rpc).await,
        vec!["main"],
        "main is focused before the click"
    );
    // The single-window left dock fills rows 0..24 (the test attaches 24 windows-area
    // rows); its status line is the last one, row 23. It has no window below it, so
    // it isn't a resize handle — the click focuses the dock instead.
    feed_mouse(&rpc, "left", "press", 23, 5);
    assert_eq!(
        lines(&rpc).await,
        vec!["dock"],
        "clicking the dock's status line focused it"
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
async fn tablast_targets_the_focused_docks_last_tab() {
    let (rpc, _incoming) = start().await;
    // Main stays at a single tab.
    feed(&rpc, "imain-one<Esc>");
    // Focus a dock and give it three tabs (more than main's one), so a `:tablast`
    // that mistakenly counted main's tabs would stop short of the dock's last.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock-one<Esc>");
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    feed(&rpc, "idock-two<Esc>");
    exec_lua(&rpc, "vim.cmd('tabnew')").await;
    feed(&rpc, "idock-three<Esc>");
    // Back to the dock's first tab, then `:tablast` must reach its *third* tab —
    // counting the focused layer's tabs, not main's.
    feed_sync(&rpc, ":tabfirst<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["dock-one"],
        ":tabfirst lands on the dock's first tab"
    );
    feed_sync(&rpc, ":tablast<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["dock-three"],
        ":tablast reaches the focused dock's last tab, not main's"
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
    // Wait for the frame that actually reflects both tabnews — the redraw channel
    // can lag a stale single-tab frame under load (CLAUDE.md's take-latest race).
    let rd = wait_redraw(&mut incoming, |m| region_tab_count(m, "left") == 3).await;
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
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") > 0).await;
    assert_eq!(region_tab_count(&rd, "left"), 0, "1 tab, default: no strip");
    // Per-dock showtabline=2 forces this dock's strip on, even with one tab.
    exec_lua(&rpc, "nx.dock.opt('left').showtabline = 2").await;
    let rd = wait_redraw(&mut incoming, |m| region_tab_count(m, "left") == 1).await;
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
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") > 0).await;
    assert_eq!(
        region_tab_count(&rd, "left"),
        0,
        "override 0 hides the strip"
    );
}

#[tokio::test]
async fn per_dock_laststatus_hides_the_dock_statusline() {
    let (rpc, mut incoming) = start().await;
    // Default laststatus=2: both the main area and the dock draw their own status row.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") > 0).await;
    assert_eq!(
        region_status_visible(&rd, "dock_left"),
        Some(true),
        "the dock shows its statusline by default"
    );
    assert_eq!(
        region_status_visible(&rd, "main"),
        Some(true),
        "main shows its statusline"
    );

    // Per-dock laststatus=0 hides this dock's statusline only.
    exec_lua(&rpc, "nx.dock.opt('left').laststatus = 0").await;
    let rd = wait_redraw(&mut incoming, |m| {
        region_status_visible(m, "dock_left") == Some(false)
    })
    .await;
    assert_eq!(
        region_status_visible(&rd, "dock_left"),
        Some(false),
        "override 0 hides the dock's statusline"
    );
    assert_eq!(
        region_status_visible(&rd, "main"),
        Some(true),
        "main keeps its statusline — the override is per-dock"
    );
}

#[tokio::test]
async fn per_dock_laststatus_accepts_the_inline_open_form() {
    let (rpc, mut incoming) = start().await;
    // The option also rides `nx.dock.open{...}` inline, like showtabline/title.
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'bottom', size = 8, laststatus = 0 }",
    )
    .await;
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_bottom") > 0).await;
    assert_eq!(
        region_status_visible(&rd, "dock_bottom"),
        Some(false),
        "inline laststatus=0 opens the dock with no statusline"
    );
}

#[tokio::test]
async fn dock_size_option_resizes_the_band() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 8 }").await;
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_bottom") == 8).await;
    assert_eq!(band(&rd, "dock_bottom"), 8, "opened at size 8");
    exec_lua(&rpc, "nx.dock.opt('bottom').size = 15").await;
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_bottom") == 15).await;
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
    let rd = wait_redraw(&mut incoming, |m| region_title(m, "left") == "EXPLORER").await;
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

// `winhighlight` is a real dock option now (the per-window highlight remap): a
// well-formed value is accepted and round-trips through the option surface, and a
// malformed entry is *reported* (not silently dropped, and not the old "not
// implemented" stub). The rendering effect is covered in the highlight suite.
#[tokio::test]
async fn dock_winhighlight_valid_value_round_trips() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    exec_lua(&rpc, "nx.dock.opt('left').winhighlight = 'Normal:NormalSB'").await;
    let got = exec_lua(&rpc, "return nx.dock.opt('left').winhighlight").await;
    assert_eq!(
        got.as_str(),
        Some("Normal:NormalSB"),
        "winhighlight is accepted and read back"
    );
}

#[tokio::test]
async fn dock_winhighlight_malformed_entry_is_reported() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    // `bogus` has no `:` — a malformed pair. The well-formed pair still applies, but
    // the bad entry is echoed rather than silently ignored.
    exec_lua(
        &rpc,
        "nx.dock.opt('left').winhighlight = 'Normal:NormalSB,bogus'",
    )
    .await;
    let rd = wait_redraw(&mut incoming, |m| {
        map_get(m, "message")
            .and_then(Value::as_str)
            .is_some_and(|s| s.contains("winhighlight"))
    })
    .await;
    let msg = map_get(&rd, "message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        msg.contains("malformed") && msg.contains("bogus"),
        "malformed winhighlight entry is reported, got {msg:?}"
    );
    assert!(
        !msg.contains("not implemented"),
        "the fail-loud stub is gone, got {msg:?}"
    );
}

/// The frame-palette `fg` color of the first highlight span on `row` of the window
/// in `region` — follows the span's style-id (`[start, end, group, style_id]`) into
/// the redraw's top-level `styles` palette. `None` when that window/row has no
/// resolved span. Proves a *resolved* remap, since the wire span keeps the original
/// group name and only its style id changes.
fn region_span_fg(map: &[(Value, Value)], region: &str, row: usize) -> Option<u64> {
    let windows = map_get(map, "windows")?.as_array()?;
    let win = windows.iter().find_map(|w| match w {
        Value::Map(m) if map_get(m, "region").and_then(Value::as_str) == Some(region) => Some(m),
        _ => None,
    })?;
    let span = map_get(win, "highlights")?
        .as_array()?
        .get(row)?
        .as_array()?
        .first()?
        .as_array()?;
    let style_id = span.get(3)?.as_u64()? as usize;
    let Value::Map(style) = map_get(map, "styles")?.as_array()?.get(style_id)? else {
        return None;
    };
    map_get(style, "fg").and_then(Value::as_u64)
}

// Phase 3: a dock's `winhighlight` remaps the highlight groups its windows resolve.
// An extmark tagged `Foo` paints `Foo`'s color until the dock sets
// `winhighlight = 'Foo:Bar'`, after which the *same* span resolves `Bar`'s color —
// in the dock window only. (A `Normal:NormalSB`-style chrome remap is Phase 4; this
// proves the per-window *content* path.)
#[tokio::test]
async fn dock_winhighlight_remaps_content_highlight_group() {
    let (rpc, mut incoming) = start().await;
    // A dock with a line of text, plus two distinctly-colored groups.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await;
    feed(&rpc, "ihello<Esc>");
    exec_lua(&rpc, "nx.hl.define(0, 'Foo', { fg = '#ff0000' })").await;
    exec_lua(&rpc, "nx.hl.define(0, 'Bar', { fg = '#00ff00' })").await;
    // Tag the whole word with `Foo` via an extmark on the (focused) dock buffer.
    exec_lua(
        &rpc,
        r#"local ns = vim.api.nvim_create_namespace("winhl_t")
           vim.api.nvim_buf_set_extmark(0, ns, 0, 0, { end_row = 0, end_col = 5, hl_group = "Foo" })"#,
    )
    .await;

    // Before any remap: the dock span resolves `Foo` (#ff0000).
    let before = wait_redraw(&mut incoming, |m| {
        region_span_fg(m, "dock_left", 0).is_some()
    })
    .await;
    assert_eq!(
        region_span_fg(&before, "dock_left", 0),
        Some(0xff0000),
        "the extmark paints Foo's color before any winhighlight"
    );

    // After `winhighlight = 'Foo:Bar'`: the same span resolves `Bar` (#00ff00).
    exec_lua(&rpc, "nx.dock.opt('left').winhighlight = 'Foo:Bar'").await;
    let after = wait_redraw(&mut incoming, |m| {
        region_span_fg(m, "dock_left", 0) == Some(0x00ff00)
    })
    .await;
    assert_eq!(
        region_span_fg(&after, "dock_left", 0),
        Some(0x00ff00),
        "winhighlight 'Foo:Bar' remaps the dock window's resolved style to Bar"
    );
}

/// The `bg` color of the window-in-`region`'s `chrome[key]` override — follows the
/// per-window override's style id into the top-level `styles` palette. `None` when
/// the window carries no override for `key` (it then falls back to global chrome).
fn region_chrome_bg(map: &[(Value, Value)], region: &str, key: &str) -> Option<u64> {
    let windows = map_get(map, "windows")?.as_array()?;
    let win = windows.iter().find_map(|w| match w {
        Value::Map(m) if map_get(m, "region").and_then(Value::as_str) == Some(region) => Some(m),
        _ => None,
    })?;
    let Value::Map(chrome) = map_get(win, "chrome")? else {
        return None;
    };
    let style_id = map_get(chrome, key)?.as_u64()? as usize;
    let Value::Map(style) = map_get(map, "styles")?.as_array()?.get(style_id)? else {
        return None;
    };
    map_get(style, "bg").and_then(Value::as_u64)
}

// Phase 4: a dock's `winhighlight` also remaps *chrome* (the background / gutter /
// EOB groups resolved globally). `Normal:NormalSB` gives the dock window a per-window
// `chrome.normal` override resolving NormalSB's background, while the main window
// carries no override (it falls back to the global `Normal`). This is the sidebar
// look — the headline use of `winhighlight`.
#[tokio::test]
async fn dock_winhighlight_overrides_chrome_normal_per_window() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await;
    // A distinctly-colored sidebar background group.
    exec_lua(&rpc, "nx.hl.define(0, 'NormalSB', { bg = '#202030' })").await;
    exec_lua(&rpc, "nx.dock.opt('left').winhighlight = 'Normal:NormalSB'").await;

    let rd = wait_redraw(&mut incoming, |m| {
        region_chrome_bg(m, "dock_left", "normal").is_some()
    })
    .await;
    assert_eq!(
        region_chrome_bg(&rd, "dock_left", "normal"),
        Some(0x202030),
        "the dock window's chrome.normal override resolves NormalSB's background"
    );
    assert_eq!(
        region_chrome_bg(&rd, "main", "normal"),
        None,
        "the main window carries no chrome override — it uses the global Normal"
    );
}

// `winhighlight` is per-window, also reachable on a plain (non-dock) window through
// `nx.wo` — backing the window-scope claim the example config documents.
#[tokio::test]
async fn nx_wo_winhighlight_remaps_a_plain_window() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.hl.define(0, 'NormalSB', { bg = '#202030' })").await;
    exec_lua(&rpc, "nx.wo.winhighlight = 'Normal:NormalSB'").await;
    let rd = wait_redraw(&mut incoming, |m| {
        region_chrome_bg(m, "main", "normal").is_some()
    })
    .await;
    assert_eq!(
        region_chrome_bg(&rd, "main", "normal"),
        Some(0x202030),
        "nx.wo.winhighlight remaps the focused (non-dock) window's chrome"
    );
}

// The shipped examples/dock-winhighlight config must load and render the sidebar:
// its left dock carries the `Normal:NormalSB` override resolving NormalSB's
// background (#181825), proving the example works end-to-end (not just that it loads).
#[tokio::test]
async fn example_dock_winhighlight_config_renders_the_sidebar() {
    let (rpc, mut incoming) = start().await;
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/dock-winhighlight/init.lua")
        .canonicalize()
        .expect("examples/dock-winhighlight/init.lua");
    let init_lua = std::fs::read_to_string(&example).expect("read example init.lua");
    exec_lua(&rpc, &init_lua).await;
    let rd = wait_redraw(&mut incoming, |m| {
        region_chrome_bg(m, "dock_left", "normal").is_some()
    })
    .await;
    assert_eq!(
        region_chrome_bg(&rd, "dock_left", "normal"),
        Some(0x181825),
        "the example's left dock paints on the NormalSB sidebar background"
    );
}

// ---------------------------------------------------------------------------
// Toggle / auto-hide — collapse a dock from view while keeping its content, the
// VSCode-style counterpart of close (which drops the content).
// ---------------------------------------------------------------------------

/// `nx.dock.toggle` hides a visible dock (it leaves the window list and the band
/// collapses) and, toggled again, brings the *same content* back — not a fresh
/// scratch. The whole point of toggle over close/open.
#[tokio::test]
async fn toggle_hides_then_shows_a_dock_preserving_content() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "ialpha<Esc>02l"); // type into the dock; cursor to a non-trivial col
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "cursor parked at col 2 before hiding"
    );
    assert_eq!(win_count(&rpc).await, 2, "main + the dock window");
    let bufs = buf_count(&rpc).await;

    // Hide it (toggle from main works on the layer state directly).
    exec_lua(&rpc, "nx.dock.toggle('left')").await;
    assert_eq!(
        win_count(&rpc).await,
        1,
        "the hidden dock leaves the window list"
    );
    assert_eq!(
        buf_count(&rpc).await,
        bufs,
        "hiding keeps every buffer loaded"
    );
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") == 0).await;
    assert_eq!(band(&rd, "dock_left"), 0, "a hidden dock reserves no band");
    assert!(
        !regions(&rd).iter().any(|r| r == "dock_left"),
        "no dock_left window is painted while hidden"
    );

    // Show it again: the dock returns, focused, with its text and cursor intact.
    exec_lua(&rpc, "nx.dock.toggle('left')").await;
    assert_eq!(win_count(&rpc).await, 2, "the dock is back");
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha"],
        "the typed text survived the hide"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "the cursor position survived too"
    );
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") == 20).await;
    assert_eq!(band(&rd, "dock_left"), 20, "the band is restored");
}

/// Hiding preserves a dock's *internal* layout: an internal split survives the
/// round-trip, which a close/reopen (fresh single scratch) would not.
#[tokio::test]
async fn toggle_preserves_internal_splits() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await;
    feed(&rpc, "<C-w>v"); // split inside the dock → two dock windows
    assert_eq!(win_count(&rpc).await, 3, "main + two dock windows");

    exec_lua(&rpc, "nx.dock.hide('left')").await;
    assert_eq!(win_count(&rpc).await, 1, "both dock windows are hidden");

    exec_lua(&rpc, "nx.dock.show('left')").await;
    assert_eq!(
        win_count(&rpc).await,
        3,
        "the internal split is restored, not collapsed to one scratch"
    );
}

/// A *hidden* dock is not a *closed* one: its buffer stays loaded and reopening
/// `nx.dock.open` reveals the same content, whereas closing drops it so a reopen
/// mints a fresh scratch.
#[tokio::test]
async fn hidden_is_distinct_from_closed() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "ikeep<Esc>");

    // Hide, then re-open by side: the same buffer (text) comes back.
    exec_lua(&rpc, "nx.dock.hide('left')").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left' }").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["keep"],
        "open un-hides the existing content"
    );

    // Now close it: the buffer stays loaded, but a fresh open mints a new scratch.
    exec_lua(&rpc, "nx.dock.close('left')").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left' }").await;
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "after a real close, reopening is a fresh empty scratch"
    );
}

/// An `autohide` dock collapses itself the moment focus crosses out of it (here
/// via `<C-w><C-w>`), and re-shows with its content intact.
#[tokio::test]
async fn autohide_dock_collapses_when_focus_leaves() {
    let (rpc, mut incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, autohide = true }",
    )
    .await;
    feed(&rpc, "ipanel<Esc>");
    assert_eq!(
        win_count(&rpc).await,
        2,
        "the autohide dock is open and focused"
    );

    // Cross back to main: leaving the dock auto-hides it.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(lines(&rpc).await, vec!["main"], "focus is back in main");
    assert_eq!(
        win_count(&rpc).await,
        1,
        "the dock collapsed on focus-leave"
    );
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") == 0).await;
    assert_eq!(
        band(&rd, "dock_left"),
        0,
        "the autohide dock reserves no band"
    );

    // Toggling it back restores the content.
    exec_lua(&rpc, "nx.dock.toggle('left')").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["panel"],
        "the dock's content survived autohide"
    );
}

/// Auto-hide also fires for a mouse crossing — clicking into the main area
/// collapses the focused autohide dock.
#[tokio::test]
async fn autohide_collapses_on_a_mouse_click_in_main() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, autohide = true }",
    )
    .await;
    feed(&rpc, "ipanel<Esc>");
    assert_eq!(win_count(&rpc).await, 2);

    // The main area starts past the 20-col dock + separator (col 21); click inside.
    feed_mouse(&rpc, "left", "press", 3, 40);
    assert_eq!(lines(&rpc).await, vec!["main"], "the click focused main");
    assert_eq!(
        win_count(&rpc).await,
        1,
        "the autohide dock collapsed when the click left it"
    );
}

/// A non-autohide dock stays put when focus leaves it (the default).
#[tokio::test]
async fn a_plain_dock_stays_open_when_focus_leaves() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "ipanel<Esc>");
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(
        win_count(&rpc).await,
        2,
        "a dock without autohide stays open across a focus cross"
    );
}

/// Toggling a side that has no dock is a reported no-op, not a panic or a new dock.
#[tokio::test]
async fn toggle_on_an_absent_side_is_a_noop() {
    let (rpc, _incoming) = start().await;
    assert_eq!(win_count(&rpc).await, 1);
    exec_lua(&rpc, "nx.dock.toggle('right')").await;
    assert_eq!(
        win_count(&rpc).await,
        1,
        "toggling an absent dock neither panics nor opens one"
    );
}

/// The `:DockToggle` ex-command drives the same path as `nx.dock.toggle`.
#[tokio::test]
async fn dock_toggle_ex_command_drives_it() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 6 }").await;
    feed(&rpc, "ibot<Esc>");
    assert_eq!(win_count(&rpc).await, 2);

    feed(&rpc, ":DockToggle bottom<CR>");
    assert_eq!(win_count(&rpc).await, 1, ":DockToggle hid the dock");
    feed(&rpc, ":DockToggle bottom<CR>");
    assert_eq!(win_count(&rpc).await, 2, ":DockToggle showed it again");
    assert_eq!(
        lines(&rpc).await,
        vec!["bot"],
        "content preserved across the ex-command"
    );
}

// ---------------------------------------------------------------------------
// Collapsed-dock indicator — a hidden dock still advertises itself as a clickable
// chip on the idle command-line row (Phase 5).
// ---------------------------------------------------------------------------

/// Hiding a dock projects a `▸{title}` chip; showing it clears the chip.
#[tokio::test]
async fn a_hidden_dock_projects_a_chip() {
    let (rpc, mut incoming) = start().await;
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, title = 'EXPLORER' }",
    )
    .await;
    exec_lua(&rpc, "nx.dock.hide('left')").await;
    let rd = wait_redraw(&mut incoming, |m| !hidden_docks(m).is_empty()).await;
    assert_eq!(
        hidden_docks(&rd),
        vec!["EXPLORER"],
        "the hidden dock advertises its title as a chip"
    );

    // Showing it again removes the chip.
    exec_lua(&rpc, "nx.dock.show('left')").await;
    let rd = wait_redraw(&mut incoming, |m| band(m, "dock_left") == 20).await;
    assert!(
        hidden_docks(&rd).is_empty(),
        "a visible dock contributes no chip, got {:?}",
        hidden_docks(&rd)
    );
}

/// An untitled hidden dock falls back to its side keyword for the chip label.
#[tokio::test]
async fn an_untitled_hidden_dock_chip_uses_the_side_keyword() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.dock.open{ side = 'bottom', size = 6 }").await;
    exec_lua(&rpc, "nx.dock.hide('bottom')").await;
    let rd = wait_redraw(&mut incoming, |m| !hidden_docks(m).is_empty()).await;
    assert_eq!(hidden_docks(&rd), vec!["bottom"], "untitled → side keyword");
}

/// Clicking a collapsed-dock chip on the command-line row re-shows that dock.
#[tokio::test]
async fn clicking_a_hidden_dock_chip_reshows_it() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "imain<Esc>");
    exec_lua(
        &rpc,
        "nx.dock.open{ side = 'left', size = 20, title = 'EXPLORER' }",
    )
    .await;
    feed(&rpc, "ipanel<Esc>");
    // Hide it; focus is back in main, the chip `▸EXPLORER` sits at cols 0.. of the
    // command-line row (row 24 = the windows-area height the harness attaches).
    exec_lua(&rpc, "nx.dock.hide('left')").await;
    assert_eq!(win_count(&rpc).await, 1, "the dock is hidden");

    feed_mouse(&rpc, "left", "press", 24, 3); // inside `▸EXPLORER`
    assert_eq!(
        win_count(&rpc).await,
        2,
        "clicking the chip re-showed the dock"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["panel"],
        "the chip click focuses the re-shown dock, content intact"
    );
}

/// The buffer list is **per-layer**: `:ls` lists only the buffers of the focused
/// region — a dock reports just its own, the main area just its own.
#[tokio::test]
async fn ls_lists_only_the_focused_layers_buffers() {
    let (rpc, _incoming) = start().await;
    // Widen so each buffer's absolute-path listing fits on one (un-wrapped) panel
    // row — otherwise the one-row-per-buffer count this asserts on breaks.
    req(
        &rpc,
        "nx_ui_try_resize",
        vec![Value::from(400u64), Value::from(40u64)],
    )
    .await;
    let a = write_temp("dock_ls_a", "txt", "AAA\n");
    let b = write_temp("dock_ls_b", "txt", "BBB\n");
    let c = write_temp("dock_ls_c", "txt", "CCC\n");

    // Main area: two buffers (A reuses the startup [No Name]; B is current).
    command(&rpc, &format!("e {a}")).await;
    command(&rpc, &format!("e {b}")).await;

    // `:ls` in the main area lists A and B (its `[Buffers]` listing buffer becomes
    // the focused window). `:ls` is scoped to the focused layer.
    command(&rpc, "ls").await;
    let rows = lines(&rpc).await;
    assert_eq!(rows.len(), 2, "main lists its two buffers: {rows:?}");
    assert!(rows.iter().any(|r| r.contains(&a)), "A present: {rows:?}");
    assert!(rows.iter().any(|r| r.contains(&b)), "B present: {rows:?}");

    // Open a left dock and load a third file into its scratch buffer; focus is in the
    // dock now.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await;
    command(&rpc, &format!("e {c}")).await;
    assert_eq!(lines(&rpc).await, vec!["CCC"], "the dock shows C");

    // `:ls` from the dock lists only the dock's buffer (C), never the main A/B —
    // proving the listing is scoped to the focused layer.
    command(&rpc, "ls").await;
    let rows = lines(&rpc).await;
    assert_eq!(
        rows.len(),
        1,
        "the dock lists only its own buffer: {rows:?}"
    );
    assert!(rows[0].contains(&c), "the dock's row is C: {rows:?}");

    for f in [a, b, c] {
        std::fs::remove_file(&f).ok();
    }
}

/// Closing a document in the main area must never pull a dock's buffer into the
/// main window: with no other *main* buffer left, it opens a fresh `[No Name]`.
#[tokio::test]
async fn closing_a_main_document_does_not_load_a_dock_buffer() {
    let (rpc, _incoming) = start().await;
    let a = write_temp("dock_close_a", "txt", "AAA\n");
    let c = write_temp("dock_close_c", "txt", "CCC\n");

    // Main area: a single document, A.
    command(&rpc, &format!("e {a}")).await;

    // A dock holding its own document, C.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await;
    command(&rpc, &format!("e {c}")).await;
    assert_eq!(lines(&rpc).await, vec!["CCC"], "the dock shows C");

    // Back in the main area, A is the only main buffer.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(lines(&rpc).await, vec!["AAA"], "main shows A");

    // Closing A leaves the main window on a fresh empty buffer — *not* the dock's C.
    command(&rpc, "bd").await;
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "main fell back to a fresh [No Name], not the dock's C"
    );
    // C is still loaded (closing A never touched the dock), alongside the new buffer.
    assert_eq!(buf_count(&rpc).await, 2, "C plus the fresh [No Name]");

    for f in [a, c] {
        std::fs::remove_file(&f).ok();
    }
}

/// `nx.buf.list` defaults to every buffer across all layers; `{ focused = true }`
/// scopes it to the focused region (the per-region list `:ls` shows).
#[tokio::test]
async fn buf_list_focused_option_scopes_to_the_focused_layer() {
    let (rpc, _incoming) = start().await;
    let a = write_temp("dock_blist_a", "txt", "AAA\n");
    let c = write_temp("dock_blist_c", "txt", "CCC\n");

    command(&rpc, &format!("e {a}")).await; // buffer 1 = A (main)
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 30 }").await; // scratch buffer 2 (dock)
    command(&rpc, &format!("e {c}")).await; // buffer 2 = C (dock)

    // Focused on the dock: the default list spans both layers; the focused list is
    // the dock's single buffer.
    assert_eq!(
        lua_u64(&rpc, "return #nx.buf.list()").await,
        Some(2),
        "the default list spans every layer"
    );
    assert_eq!(
        lua_u64(&rpc, "return #nx.buf.list{ focused = true }").await,
        Some(1),
        "the focused list is the dock alone"
    );
    assert_eq!(
        lua_u64(&rpc, "return nx.buf.list{ focused = true }[1]").await,
        Some(2),
        "and that buffer is the dock's (#2)"
    );

    // Cross to the main area; the focused list now reports main's buffer.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(
        lua_u64(&rpc, "return nx.buf.list{ focused = true }[1]").await,
        Some(1),
        "main's focused list is buffer #1"
    );
    assert_eq!(
        lua_u64(&rpc, "return #nx.buf.list()").await,
        Some(2),
        "the default list still spans every layer"
    );

    for f in [a, c] {
        std::fs::remove_file(&f).ok();
    }
}

/// `<C-w>H/J/K/L` swaps the focused window's buffer with the neighbor in that
/// direction (within the layer), and focus follows the buffer to the neighbor.
#[tokio::test]
async fn ctrl_w_capital_swaps_buffers_between_splits() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "iAAA<Esc>"); // window 1 (left) shows buffer A
    feed(&rpc, "<C-w>v"); // vsplit; the right window is focused (also A)
    command(&rpc, "enew").await; // give the right window a distinct buffer B
    feed(&rpc, "iBBB<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["BBB"],
        "focused right window shows B"
    );

    // Swap with the window to the LEFT; B should move there and focus follow it.
    feed(&rpc, "<C-w>H");
    assert_eq!(
        lines(&rpc).await,
        vec!["BBB"],
        "B moved into the left window, focus followed it"
    );
    // The right window now holds A.
    feed(&rpc, "<C-w>l");
    assert_eq!(
        lines(&rpc).await,
        vec!["AAA"],
        "A is now in the right window"
    );
}

/// `<C-w><C-w>H/J/K/L` moves the focused buffer to the dock on that edge: the
/// buffer follows into the dock, and the source window falls back to a fresh
/// `[No Name]` when it had no other buffer of its own.
#[tokio::test]
async fn ctrl_w_ctrl_w_capital_moves_a_buffer_to_the_dock() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "iMAIN<Esc>"); // main buffer = "MAIN"
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "idock<Esc>"); // the dock's scratch buffer = "dock"
    feed(&rpc, "<C-w><C-w>l"); // back to the main area
    assert_eq!(lines(&rpc).await, vec!["MAIN"], "main shows MAIN");

    // Move MAIN to the left dock — it follows there.
    feed(&rpc, "<C-w><C-w>H");
    assert_eq!(
        lines(&rpc).await,
        vec!["MAIN"],
        "the buffer followed into the dock"
    );
    assert_eq!(
        buf_count(&rpc).await,
        3,
        "MAIN, the dock scratch, and the main area's fresh [No Name]"
    );

    // The main area fell back to a fresh empty buffer, not the dock's content.
    feed(&rpc, "<C-w><C-w>l");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "main fell back to a fresh [No Name]"
    );
}

/// From a dock, `<C-w><C-w>H/J/K/L` moves the buffer back to the main area (any
/// direction), leaving the dock on a fresh empty buffer.
#[tokio::test]
async fn ctrl_w_ctrl_w_capital_moves_a_dock_buffer_back_to_main() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "iMAIN<Esc>");
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "iDOCKBUF<Esc>"); // the dock's buffer
    assert_eq!(
        lines(&rpc).await,
        vec!["DOCKBUF"],
        "focused on the dock buffer"
    );

    // From the dock, any capital direction sends the buffer to the main area.
    feed(&rpc, "<C-w><C-w>L");
    assert_eq!(
        lines(&rpc).await,
        vec!["DOCKBUF"],
        "the buffer followed into the main area"
    );

    // The dock fell back to a fresh empty buffer.
    feed(&rpc, "<C-w><C-w>h");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "the dock fell back to a fresh [No Name]"
    );
}

/// `<C-w><C-w>H/J/K/L` is a no-op when the dock on that edge is closed (mirroring
/// the lowercase focus cross), leaving the buffer in the main area.
#[tokio::test]
async fn ctrl_w_ctrl_w_capital_is_a_noop_when_the_target_dock_is_closed() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "iMAIN<Esc>");
    assert_eq!(win_count(&rpc).await, 1, "no dock open");

    feed(&rpc, "<C-w><C-w>H"); // left dock is closed → nothing happens
    assert_eq!(
        lines(&rpc).await,
        vec!["MAIN"],
        "buffer stayed in the main area"
    );
    assert_eq!(win_count(&rpc).await, 1, "still no dock");
}

/// Regression: a buffer split in the main area, then moved to a dock, lingers in
/// the other split window while also living in the dock. Deleting it must rebind
/// the lingering window instead of leaving it dangling on the freed id (which
/// crashed the editor).
#[tokio::test]
async fn bdelete_a_buffer_split_then_moved_to_a_dock_does_not_crash() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "iMAIN<Esc>"); // buffer "MAIN"
    feed(&rpc, "<C-w>v"); // split: two main windows both show MAIN
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "<C-w><C-w>l"); // back to the main area
    feed(&rpc, "<C-w><C-w>H"); // move MAIN into the left dock

    // MAIN now lives in the dock *and* still shows in the other main split window.
    command(&rpc, "bd!").await;

    // The editor is still responsive — these reads would panic on a dangling window.
    assert_eq!(
        buf_count(&rpc).await,
        2,
        "MAIN gone; the dock scratch and main's fresh empty remain"
    );
    feed(&rpc, "<C-w><C-w>l"); // hop to the main area
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "the focused main window shows a valid (empty) buffer"
    );
    feed(&rpc, "<C-w>w"); // the formerly-lingering split window
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "the lingering split window was rebound, not left dangling"
    );
}

#[tokio::test]
async fn opening_a_picker_in_a_too_small_dock_does_not_crash() {
    // Regression: a picker focused in a dock too short to fit its prompt+separator
    // chrome computed `height.clamp(chrome + 1, max_h)` with `max_h < chrome + 1`,
    // and `usize::clamp` panics when `min > max` — taking the server down on the
    // redraw that projects the box. A focused tiny dock must just yield a cramped
    // (but valid) picker, never a crash.
    let _g = serial_lock();
    let (rpc, _incoming) = start().await; // 80x24

    // A 2-row bottom dock — too short for the picker's 3-row minimum chrome.
    exec_lua(&rpc, "nx.dock.open({ side = 'bottom', size = 2 })").await;
    // A hermetic static source (no `rg` spawn).
    exec_lua(
        &rpc,
        r#"nx.picker.source {
            name = 'fruits',
            items = function(ctx) ctx.push { text = 'apple' } end,
            confirm = function() end,
        }"#,
    )
    .await;
    // Focus the tiny dock, then open the picker there — this drove the panic.
    exec_lua(&rpc, "nx.dock.focus('bottom')").await;
    exec_lua(&rpc, "nx.picker.open('fruits')").await;

    // The server is still alive: a request round-trips (a panicked server thread
    // would drop the connection and this `expect` would fire).
    let alive = req(&rpc, "nvim_get_mode", vec![]).await;
    assert!(matches!(alive, Value::Map(_) | Value::Array(_)));
    // And the picker is actually open (not silently dropped).
    let open = exec_lua(&rpc, "return nx._picker ~= nil").await;
    assert_eq!(open, Value::Boolean(true), "the picker opened in the dock");
}
