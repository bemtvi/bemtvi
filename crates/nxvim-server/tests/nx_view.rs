//! Behavior tests for `nx.open` / `nx.layer` (the layer-cross surface) and
//! `nx.view` (the plugin-owned, dockable read-only content surface that generalizes
//! the bottom panel). Driven black-box over RPC like `dock.rs`.
//!
//! `nx.open(path, { where = "main" })` opens a file in the main editor area even
//! when fired from a dock keymap; `nx.layer.focus` / `nx.layer.main` cross focus
//! between the main area and the docks. A `nx.view` is an inert (read-only) buffer a
//! plugin fills with `:set_lines`, decorates with `:set_decor`, mounts in a dock /
//! split, and whose `<CR>` dispatches to `:on_select`.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    cursor, drain_to_latest_redraw, exec_lua, feed, lines, lua_u64, map_get, mode, start_attached,
    write_temp,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Feed `keys`, then a `nvim_get_mode` barrier so the input is fully processed
/// before the following read.
async fn feed_sync(rpc: &Rpc, keys: &str) {
    feed(rpc, keys);
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
}

async fn win_count(rpc: &Rpc) -> usize {
    match rpc.request("nvim_list_wins", vec![]).await.expect("wins") {
        Value::Array(a) => a.len(),
        v => panic!("expected array, got {v:?}"),
    }
}

/// The latest redraw map (any frame).
fn latest(incoming: &mut UnboundedReceiver<Incoming>) -> Vec<(Value, Value)> {
    drain_to_latest_redraw(incoming, |_| true).expect("a redraw frame")
}

fn band(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
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

// ===== nx.open / nx.layer ====================================================

/// `nx.open(file, { where = "main" })` fired while a dock is focused opens the file
/// in the **main** editor area (focus crosses back to main), leaving the dock's own
/// buffer untouched — the plumbing a dock file-tree uses to open files.
#[tokio::test]
async fn nx_open_where_main_lands_in_the_main_area_from_a_dock() {
    let (rpc, _incoming) = start().await;
    let file = write_temp("nxopen_main", "txt", "file-contents\n");

    feed_sync(&rpc, "imain<Esc>").await; // main buffer reads "main"
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed_sync(&rpc, "idock<Esc>").await; // dock buffer reads "dock"

    // Open the file with where="main" from inside the dock.
    exec_lua(&rpc, &format!("nx.open({:?}, {{ where = 'main' }})", file)).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["file-contents"],
        "the file opened and focus crossed to the main area"
    );

    // The dock still shows its own buffer.
    exec_lua(&rpc, "nx.layer.focus('left')").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["dock"],
        "the dock's buffer was left untouched"
    );
}

/// `nx.open` without `where` (the default) opens in the **current** window — fired
/// from a dock, the file lands inside the dock, not the main area.
#[tokio::test]
async fn nx_open_default_opens_in_the_current_window() {
    let (rpc, _incoming) = start().await;
    let file = write_temp("nxopen_cur", "txt", "in-the-dock\n");

    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    exec_lua(&rpc, &format!("nx.open({:?})", file)).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["in-the-dock"],
        "the default opens in the (focused dock) current window"
    );
}

/// `nx.layer.main()` / `nx.layer.focus(side)` round-trip focus between the main
/// area and a dock.
#[tokio::test]
async fn nx_layer_focus_round_trips_main_and_dock() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(&rpc, "nx.dock.open{ side = 'right', size = 20 }").await;
    feed_sync(&rpc, "idock<Esc>").await;

    exec_lua(&rpc, "nx.layer.main()").await;
    assert_eq!(lines(&rpc).await, vec!["main"], "focused the main area");

    exec_lua(&rpc, "nx.layer.focus('right')").await;
    assert_eq!(lines(&rpc).await, vec!["dock"], "focused the dock back");
}

/// An unknown layer name is reported, not silently ignored.
#[tokio::test]
async fn nx_layer_focus_reports_an_unknown_name() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "nx.layer.focus('sideways')").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let rd = latest(&mut incoming);
    let msg = map_get(&rd, "message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(msg.contains("Invalid layer"), "loud error, got {msg:?}");
}

// ===== nx.view ===============================================================

/// `nx.view.create` + `:set_lines` produce a buffer whose lines a plugin controls
/// (read back via `nvim_buf_get_lines` against the mirrored buffer number).
#[tokio::test]
async fn view_create_and_set_lines_controls_the_buffer() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{ name = "nx-tree", filetype = "nxtree" }
           vw:set_lines{ "alpha", "beta", "gamma" }"#,
    )
    .await;
    // The buffer number arrives on the next tick's mirror; read the lines off it.
    let joined = exec_lua(
        &rpc,
        r#"local b = vw:bufnr()
           return table.concat(vim.api.nvim_buf_get_lines(b, 0, -1, false), "|")"#,
    )
    .await;
    assert_eq!(joined.as_str(), Some("alpha|beta|gamma"));
}

/// A view is plugin-owned content with no disk backing, so `:set_lines` must leave it
/// UNMODIFIED — the wholesale rope rewrite (`mark_resync`) sets the modified flag, and
/// `set_view_lines` must clear it. Otherwise every view (file tree, symbol list, …)
/// reads as `[+]` and blocks `:qa` with E37, as if it wanted saving.
#[tokio::test]
async fn view_set_lines_does_not_mark_modified() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "alpha", "beta", "gamma" }"#,
    )
    .await;
    let modified = exec_lua(&rpc, r#"return vim.bo[vw:bufnr()].modified"#).await;
    assert_eq!(
        modified.as_bool(),
        Some(false),
        "a view buffer must never read as modified"
    );
    // A second set_lines (a re-render) must not flip it either.
    exec_lua(&rpc, r#"vw:set_lines{ "one", "two" }"#).await;
    let modified = exec_lua(&rpc, r#"return vim.bo[vw:bufnr()].modified"#).await;
    assert_eq!(
        modified.as_bool(),
        Some(false),
        "re-render must stay unmodified"
    );
}

/// A view mounted in a left dock renders there: the dock band is reserved, a
/// `dock_left` window is painted, and it shows the view's content.
#[tokio::test]
async fn view_mounts_in_a_left_dock() {
    let (rpc, mut incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{ name = "tree" }
           vw:set_lines{ "src/", "README.md" }
           vw:mount{ dock = "left", size = 24 }"#,
    )
    .await;
    assert_eq!(win_count(&rpc).await, 2, "the dock mount added a window");
    // The dock is focused after mount, so the current buffer is the view.
    assert_eq!(lines(&rpc).await, vec!["src/", "README.md"]);

    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let rd = latest(&mut incoming);
    assert_eq!(band(&rd, "dock_left"), 24, "the dock band is reserved");
    assert!(
        regions(&rd).iter().any(|r| r == "dock_left"),
        "a dock_left window is painted"
    );
}

/// A view mounted as a split shows in a new window of the main area.
#[tokio::test]
async fn view_mounts_in_a_split() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    assert_eq!(win_count(&rpc).await, 1);
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "one", "two" }
           vw:mount{ split = "vsplit" }"#,
    )
    .await;
    assert_eq!(win_count(&rpc).await, 2, "the split added a window");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two"],
        "the split window shows the view"
    );
}

/// `:set_decor` lays extmarks on the view buffer (read back via
/// `nvim_buf_get_extmarks`), the rendering layer for icons / indent guides / signs.
#[tokio::test]
async fn view_set_decor_lays_extmarks() {
    let (rpc, _incoming) = start().await;
    // Tick 1: create + mount (the buffer comes into existence).
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:mount{ dock = "left", size = 20 }
           vw:set_lines{ "aaa", "bbb", "ccc" }"#,
    )
    .await;
    // Tick 2: the bufnr is mirrored now; decorate two lines.
    exec_lua(
        &rpc,
        r#"ns = nx.ns.create("nxview-decor")
           vw:set_decor(ns, {
             { line = 0, col = 0, end_row = 0, end_col = 3, hl_group = "Comment" },
             { line = 1, col = 0, end_row = 1, end_col = 3, hl_group = "String" },
           })"#,
    )
    .await;
    // Tick 3: the extmark op has drained; count the marks in the namespace.
    let n = lua_u64(
        &rpc,
        r#"return #vim.api.nvim_buf_get_extmarks(vw:bufnr(), ns, 0, -1, {})"#,
    )
    .await;
    assert_eq!(
        n,
        Some(2),
        "both decoration marks landed on the view buffer"
    );
}

/// `<CR>` on a focused view fires `:on_select(line, userdata)` with the 1-based
/// cursor line and that line's userdata entry.
#[tokio::test]
async fn view_on_select_fires_with_line_and_userdata() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "alpha", "beta", "gamma" }
           vw:set_userdata{ "A", "B", "C" }
           vw:on_select(function(line, ud) _G.picked = line .. "/" .. tostring(ud) end)
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    // Move to line 2 (beta) and confirm.
    feed_sync(&rpc, "j").await;
    feed_sync(&rpc, "<CR>").await;
    let picked = exec_lua(&rpc, "return _G.picked").await;
    assert_eq!(
        picked.as_str(),
        Some("2/B"),
        "on_select got the 1-based line and its userdata"
    );
}

/// Navigation keys move the cursor within the view (and stay clamped to it).
#[tokio::test]
async fn view_navigation_moves_the_cursor() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "one", "two", "three" }
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    feed_sync(&rpc, "G").await; // jump to last
    assert_eq!(cursor(&rpc).await, (3, 0), "G went to the last line");
    feed_sync(&rpc, "k").await;
    assert_eq!(cursor(&rpc).await, (2, 0), "k moved up one");
    feed_sync(&rpc, "gg").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "gg went to the first line");
}

/// A view is inert to the editing grammar: text-mutating keys can't corrupt the
/// plugin-owned content, and `i` never enters insert mode.
#[tokio::test]
async fn view_is_inert_to_editing_keys() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "keep", "me" }
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    feed_sync(&rpc, "dd").await; // would delete a line in a real buffer
    feed_sync(&rpc, "x").await; // would delete a char
    feed_sync(&rpc, "iHELLO").await; // would insert; `i` must be inert
    assert_eq!(mode(&rpc).await, "n", "`i` did not enter insert mode");
    assert_eq!(
        lines(&rpc).await,
        vec!["keep", "me"],
        "the view content is unchanged by editing keys"
    );
}

/// A view is read-only at the **edit chokepoints** too — not just the input router.
/// An ex-command edit (`:d`, `:s`, `:normal`) reaches the chokepoints, which consult
/// `modifiable()`; a view is `nomodifiable` there (like quickfix / a live terminal),
/// so the edit is refused with E21 instead of corrupting the plugin's content.
#[tokio::test]
async fn view_is_read_only_to_ex_command_edits() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "alpha", "beta", "gamma" }
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    feed_sync(&rpc, ":d<CR>").await; // delete the current line
    feed_sync(&rpc, ":s/beta/HACKED/<CR>").await; // substitute on a line
    feed_sync(&rpc, ":2<CR>:normal dd<CR>").await; // :normal-driven delete
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta", "gamma"],
        "ex-command edits are refused on a view, not applied"
    );
}

/// Run the shipped `examples/nxview/init.lua` end-to-end: it must load without
/// error and open its left-dock view (guards the example against drift).
#[tokio::test]
async fn example_config_opens_its_view() {
    let (rpc, mut incoming) = start().await;
    let init = include_str!("../../../examples/nxview/init.lua");
    exec_lua(&rpc, init).await;
    assert_eq!(
        win_count(&rpc).await,
        2,
        "the example mounts a left-dock view (+ main)"
    );
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let rd = latest(&mut incoming);
    assert!(
        regions(&rd).iter().any(|r| r == "dock_left"),
        "the view's dock is painted"
    );
    // The example lands focus back in the main editor (the empty startup buffer)
    // after mounting — not in the sidebar (whose first line is the sample entry).
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "focus is in the main area after build, not the view"
    );
}

/// `:unmount` removes the view from view but keeps it alive; a later `:mount`
/// reshows the same content.
#[tokio::test]
async fn view_unmount_then_remount_keeps_content() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "persisted" }
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    assert_eq!(win_count(&rpc).await, 2);

    exec_lua(&rpc, "vw:unmount()").await;
    assert_eq!(win_count(&rpc).await, 1, "unmount closed the dock");

    exec_lua(&rpc, "vw:mount{ dock = 'left', size = 20 }").await;
    assert_eq!(win_count(&rpc).await, 2, "remounted");
    assert_eq!(
        lines(&rpc).await,
        vec!["persisted"],
        "the content survived the unmount/remount"
    );
}
