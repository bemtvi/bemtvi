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

/// Regression: nx.cursor.set must move a *view*-backed window's cursor to an exact
/// column. nx.view's own cursor is line-granular (`:set_cursor` lands at col 0), and
/// the help plugin's "<C-t> returns to the exact spot" refines it with the public
/// setter — in the same chunk, exactly as window.show does. Asserted through
/// nvim_win_get_cursor (the *real* cursor the user sees), not the Lua mirror.
#[tokio::test]
async fn nx_cursor_set_refines_a_view_window_cursor_column() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{ name = "[T]", filetype = "help" }
           vw:set_lines{ "page B line one", "page B line two" }
           vw:mount{ split = "split" }
           vw:set_cursor(2)"#,
    )
    .await;
    // The reused-window re-show window.show runs for `<C-t>`: swap the content, refocus
    // the already-mounted view, line-granular jump, then refine the column — all one
    // chunk. Row 1, col 6 → the 'l' of "line".
    exec_lua(
        &rpc,
        r#"vw:set_lines{ "first line here", "second line here" }
           vw:focus()
           vw:set_cursor(1)
           nx.cursor.set({ 1, 6 }, vw:winid())"#,
    )
    .await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 6),
        "nx.cursor.set refined the view window's cursor column on re-show"
    );
}

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

/// `:set_cursor(line)` focuses the view and lands the cursor on the 1-based `line`,
/// clamped to the content — the reveal / find-file primitive a docked tree uses.
#[tokio::test]
async fn view_set_cursor_focuses_and_positions() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await; // focus starts in the main area
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "one", "two", "three" }
           vw:mount{ dock = "left", size = 20 }"#,
    )
    .await;
    // Focus may be anywhere after mount; set_cursor must cross into the view.
    exec_lua(&rpc, "nx.layer.main()").await;
    exec_lua(&rpc, "vw:set_cursor(2)").await;
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "cursor landed on line 2, view focused"
    );

    // Out-of-range clamps to the last line, never panics or overshoots.
    exec_lua(&rpc, "vw:set_cursor(99)").await;
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "an over-range line clamped to the last"
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

// ===== nx.view float mount ===================================================

/// A view can mount in a **floating window** (`v:mount{ float = … }`): it adds a
/// window, shows the view's content, and is an actual float (`relative = "editor"`).
#[tokio::test]
async fn view_mounts_in_a_float() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    assert_eq!(win_count(&rpc).await, 1);
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "[ ] one", "[ ] two" }
           vw:mount{ float = { width = 30, height = 6 } }"#,
    )
    .await;
    assert_eq!(win_count(&rpc).await, 2, "the float added a window");
    assert_eq!(
        lines(&rpc).await,
        vec!["[ ] one", "[ ] two"],
        "the float is focused and shows the view"
    );
    let rel = exec_lua(&rpc, r#"return vim.api.nvim_win_get_config(0).relative"#).await;
    assert_eq!(
        rel.as_str(),
        Some("editor"),
        "the view's window is a real float"
    );
}

/// A view float sized with viewport fractions (`"50vw"`/`"50vh"`) and placed by the
/// high-level `align` resolves the same way a picker does — and because the `Extent`
/// is resolved against the live editor area **every layout**, it reflows on resize.
/// `nvim_win_get_config` reports the resolved inner cells (not the raw spec) and the
/// alignment word.
#[tokio::test]
async fn view_float_frac_size_aligns_and_reflows_on_resize() {
    let (rpc, _incoming) = start().await; // 80 x 24
    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "x" }
           vw:mount{ float = { width = '50vw', height = '50vh', align = 'center' } }"#,
    )
    .await;

    // The float is focused (win 0). Its reported size is resolved cells; ~50% of 80.
    let w0 = lua_u64(&rpc, "return vim.api.nvim_win_get_config(0).width")
        .await
        .expect("a resolved width");
    let align = exec_lua(&rpc, "return vim.api.nvim_win_get_config(0).align").await;
    assert_eq!(align.as_str(), Some("center"), "the align word round-trips");
    assert!(
        (34..=42).contains(&w0),
        "50vw of an 80-col screen ≈ 40 resolved cells, got {w0}"
    );

    // Grow the UI: the fractional float re-resolves against the larger viewport.
    rpc.request(
        "nx_ui_try_resize",
        vec![Value::from(120u64), Value::from(40u64)],
    )
    .await
    .expect("resize");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let w1 = lua_u64(&rpc, "return vim.api.nvim_win_get_config(0).width")
        .await
        .expect("a resolved width after resize");
    assert!(
        w1 > w0 + 10,
        "50vw reflows on resize (got {w0} -> {w1}, expected ~60)"
    );
}

/// A cell-sized `nvim_open_win` float round-trips its width/height **exactly**
/// through `nvim_win_get_config` (an integer stays `Extent::Cells(n)`, resolved back
/// to `n`) — the neovim-compat guarantee the unified geometry must not break.
#[tokio::test]
async fn nvim_open_win_cell_size_round_trips_exactly() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    let config = Value::Map(vec![
        (Value::from("relative"), Value::from("editor")),
        (Value::from("row"), Value::from(2u64)),
        (Value::from("col"), Value::from(3u64)),
        (Value::from("width"), Value::from(40u64)),
        (Value::from("height"), Value::from(10u64)),
    ]);
    let win = rpc
        .request(
            "nvim_open_win",
            vec![Value::from(0u64), Value::from(true), config],
        )
        .await
        .expect("open_win");
    let win = win.as_u64().expect("a window handle");

    let got = rpc
        .request("nvim_win_get_config", vec![Value::from(win)])
        .await
        .expect("get_config");
    let Value::Map(m) = got else {
        panic!("expected a config map, got {got:?}")
    };
    assert_eq!(
        map_get(&m, "width").and_then(Value::as_u64),
        Some(40),
        "an integer width round-trips exactly"
    );
    assert_eq!(
        map_get(&m, "height").and_then(Value::as_u64),
        Some(10),
        "an integer height round-trips exactly"
    );
}

/// A **grabbing** float view (`grab = true`, the default) hard-locks focus exactly
/// like the panel: `<C-w>w` / `<C-w>j` can't leave it.
#[tokio::test]
async fn view_float_grab_locks_focus() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "dialog" }
           vw:mount{ float = { width = 20, height = 4, grab = true } }"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["dialog"], "focus is on the float");

    feed_sync(&rpc, "<C-w>w").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["dialog"],
        "<C-w>w must not leave a grabbing float view"
    );
    feed_sync(&rpc, "<C-w>j").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["dialog"],
        "<C-w>j must not leave a grabbing float view"
    );
}

/// Dismissing a grabbing float view releases the lock and restores focus to the
/// window it sprang from (open → interact → dismiss → back where you were).
#[tokio::test]
async fn view_float_grab_unmount_restores_prior_focus() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "dialog" }
           vw:mount{ float = { width = 20, height = 4, grab = true } }"#,
    )
    .await;
    assert_eq!(win_count(&rpc).await, 2);

    exec_lua(&rpc, "vw:unmount()").await;
    assert_eq!(win_count(&rpc).await, 1, "unmount closed the float");
    assert_eq!(
        lines(&rpc).await,
        vec!["main"],
        "focus returned to the prior window"
    );
}

/// A non-grabbing float view (`grab = false`) is an ordinary focusable float — it
/// does NOT lock focus, so `<C-w>w` cycles back out to the main window.
#[tokio::test]
async fn view_float_without_grab_does_not_lock_focus() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "floaty" }
           vw:mount{ float = { width = 20, height = 4, grab = false } }"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["floaty"], "the float is focused");

    feed_sync(&rpc, "<C-w>w").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["main"],
        "<C-w>w leaves a non-grabbing float for the main window"
    );
}

/// An **async** `setup` is awaited before the first `render`: the component below
/// suspends in setup (`nx.await(nx.promise.delay)`), and only renders the message it sets
/// *after* the await resolves — so seeing that message on screen proves the framework
/// awaited setup, not raced past it. This is the `nx.view.component` async contract.
#[tokio::test]
async fn component_async_setup_is_awaited_before_render() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"_G.async_ran = false
           local C = nx.view.component({
             setup = function(ctx)
               nx.await(nx.promise.delay(5))   -- suspend the lifecycle
               _G.async_ran = true
               return ctx.reactive({ msg = "loaded async" })
             end,
             render = function(state)
               return { lines = { state.msg } }
             end,
           })
           C.mount({ float = { width = 30, height = 3, grab = true } })"#,
    )
    .await;

    // Pump until the awaited content lands.
    let mut shown = false;
    for _ in 0..100 {
        if lines(&rpc).await == vec!["loaded async"] {
            shown = true;
            break;
        }
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    }
    assert!(shown, "render showed the message setup set after awaiting");
    let ran = exec_lua(&rpc, "return _G.async_ran").await;
    assert_eq!(
        ran.as_bool(),
        Some(true),
        "setup's post-await body ran before render"
    );
}

/// `ctx.computed` is fine-grained: it re-evaluates only when a reactive value it actually
/// read has changed. A write to an UNRELATED reactive field re-renders (coarse) but the
/// computed returns its cached value (no getter call); a write to a field it READ forces a
/// recompute. We count getter calls in a global to prove both.
#[tokio::test]
async fn component_computed_caches_until_its_dependency_changes() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"_G.calls = 0
           local C = nx.view.component({
             setup = function(ctx)
               local s = ctx.reactive({ nums = { 1, 2, 3 }, noise = 0 })
               _G.s = s
               _G.sum = ctx.computed(function()
                 _G.calls = _G.calls + 1
                 local t = 0
                 for _, n in ipairs(s.nums) do t = t + n end
                 return t
               end)
               return s
             end,
             render = function()
               return { lines = { "sum=" .. tostring(_G.sum()) } }
             end,
           })
           C.mount({ float = { width = 20, height = 3, grab = true } })"#,
    )
    .await;

    // Wait out the lifecycle: first render computes the sum once.
    let mut ready = false;
    for _ in 0..100 {
        if lines(&rpc).await == vec!["sum=6"] {
            ready = true;
            break;
        }
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    }
    assert!(ready, "first render showed the computed sum");
    assert_eq!(
        lua_u64(&rpc, "return _G.calls").await,
        Some(1),
        "the getter ran once for the first render"
    );

    // Write an unrelated field: the component re-renders, but the computed is NOT a
    // function of `noise`, so it stays cached — no getter call.
    exec_lua(&rpc, "_G.s.noise = 99").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    assert_eq!(lines(&rpc).await, vec!["sum=6"], "sum unchanged");
    assert_eq!(
        lua_u64(&rpc, "return _G.calls").await,
        Some(1),
        "an unrelated write must NOT recompute the computed"
    );

    // Write a field the getter read: the computed invalidates and recomputes.
    exec_lua(&rpc, "_G.s.nums[1] = 10").await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    assert_eq!(lines(&rpc).await, vec!["sum=15"], "sum reflects the change");
    assert_eq!(
        lua_u64(&rpc, "return _G.calls").await,
        Some(2),
        "a dependency write recomputes the computed exactly once"
    );
}

/// Grabbing modal floats STACK: a second grab float mounted over a first nests on top
/// (focus pinned to the innermost), and closing it pops focus back down to the modal below
/// — and finally to the original window — rather than jumping straight out.
#[tokio::test]
async fn grabbing_view_floats_stack_and_focus_pops() {
    let (rpc, _incoming) = start().await;
    feed_sync(&rpc, "imain<Esc>").await;

    exec_lua(
        &rpc,
        r#"v1 = nx.view.create{}; v1:set_lines{ "modal-1" }
           v1:mount{ float = { width = 20, height = 4, grab = true } }"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["modal-1"], "focus on modal 1");

    exec_lua(
        &rpc,
        r#"v2 = nx.view.create{}; v2:set_lines{ "modal-2" }
           v2:mount{ float = { width = 20, height = 4, grab = true } }"#,
    )
    .await;
    assert_eq!(
        lines(&rpc).await,
        vec!["modal-2"],
        "modal 2 stacked on top and took focus"
    );

    // Focus is pinned to the TOP modal — `<C-w>w` can't reach modal 1 underneath it.
    feed_sync(&rpc, "<C-w>w").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["modal-2"],
        "the top modal holds the lock"
    );

    // Closing the top modal pops focus back to modal 1, which is STILL locked.
    exec_lua(&rpc, "v2:unmount()").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["modal-1"],
        "focus popped down to modal 1"
    );
    feed_sync(&rpc, "<C-w>w").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["modal-1"],
        "modal 1 is locked again now it's on top"
    );

    // Closing the last modal returns focus to the original window.
    exec_lua(&rpc, "v1:unmount()").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["main"],
        "focus returned to the original window"
    );
}

/// A view component can set buffer-local options on its backing buffer via `ctx.bo`
/// (the `vim.bo[buf]` table scoped to the view), valid in `setup`.
#[tokio::test]
async fn view_component_sets_buffer_options_via_ctx_bo() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"nx.component({
             setup = function(ctx)
               ctx.bo.shiftwidth = 8
               _G.vbuf = ctx.bufnr()
               return ctx.reactive({})
             end,
             render = function()
               return { lines = { "x" } }
             end,
           }).mount({ float = { width = 12, height = 3, grab = true } })"#,
    )
    .await;

    // Wait out the lifecycle (buffer ready -> setup runs ctx.bo), then read the option back.
    let mut ok = false;
    for _ in 0..100 {
        let sw = exec_lua(&rpc, "return _G.vbuf and vim.bo[_G.vbuf].shiftwidth or nil").await;
        if sw.as_u64() == Some(8) {
            ok = true;
            break;
        }
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    }
    assert!(ok, "ctx.bo set the view buffer's shiftwidth to 8");
}

/// A view component can set window-local options on the window showing it via `ctx.wo`
/// (the `vim.wo[win]` table scoped to the view's window, resolved through the view→window
/// mirror). A float defaults `number` off, so reading it back on proves the set landed.
#[tokio::test]
async fn view_component_sets_window_options_via_ctx_wo() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"nx.component({
             setup = function(ctx)
               ctx.wo.number = true
               _G.vwin = ctx.winid()
               return ctx.reactive({})
             end,
             render = function()
               return { lines = { "x" } }
             end,
           }).mount({ float = { width = 12, height = 3, grab = true } })"#,
    )
    .await;

    let mut ok = false;
    for _ in 0..100 {
        let n = exec_lua(&rpc, "return _G.vwin and vim.wo[_G.vwin].number or nil").await;
        if n.as_bool() == Some(true) {
            ok = true;
            break;
        }
        rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    }
    assert!(ok, "ctx.wo turned on number for the view's window");
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

/// Deleting a view's backing buffer (`:bd`) must clean up the view, not leave it
/// orphaned: a later `:set_lines` on the stale handle must be a safe no-op, and the
/// `bufnr` mirror must clear — regression test for a panic where `set_view_lines`
/// looked up the freed buffer id and hit `get_mut`'s `.expect`.
#[tokio::test]
async fn deleting_a_views_buffer_cleans_it_up_without_panicking() {
    let (rpc, mut _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"vw = nx.view.create{}
           vw:set_lines{ "one", "two" }
           vw:mount{ split = "split" }"#,
    )
    .await;
    // Delete the view's backing buffer out from under the handle (the user `:bd`).
    feed_sync(&rpc, ":bd!<CR>").await;
    // Writing to the now-orphaned handle must be a safe no-op, not a panic.
    exec_lua(&rpc, r#"vw:set_lines{ "three", "four" }"#).await;
    // The view is cleaned up: its bufnr mirror is cleared.
    let bufnr = exec_lua(&rpc, r#"return vw:bufnr()"#).await;
    assert!(
        bufnr.is_nil(),
        "view bufnr should be nil after its buffer is deleted, got {bufnr:?}"
    );
}
