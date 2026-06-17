//! Behavior tests for the `nxtree` file explorer — the pure-Lua, dockable file tree
//! shipped under `examples/nxtree/`, built entirely on `nx.view` + `nx.fs` +
//! `nx.open` + `nx.dock` + extmarks (the dogfooding proof for the explorer surfaces).
//!
//! Black-box per the project conventions: a real server over RPC, the plugin loaded
//! by pointing Lua's `package.path` at `examples/nxtree/lua`, driven with key input
//! and `nvim_exec_lua`, asserting on the view buffer's lines / the editor windows.
//! The tree's directory listing is an off-tick `nx.fs.readdir`, so — like the `fs.rs`
//! suite — each test POLLS the view buffer until the async render settles.

use std::fs;
use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{cursor, exec_lua, feed, lines, mode, start_attached, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The absolute path to the shipped plugin's `lua/` dir (require root).
const PLUGIN_LUA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/nxtree/lua");

/// Lua-escape a path for embedding in a double-quoted string literal.
fn q(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Feed `keys`, then an `nvim_get_mode` barrier so the input is processed before the
/// following read.
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

/// Load the plugin and open (build + mount) the tree rooted at `root`. Watch is off
/// for determinism — refreshes are driven explicitly. Build ends by landing focus in
/// the main area; `focus_tree` (a later tick) moves it into the sidebar.
async fn open_tree(rpc: &Rpc, root: &Path) {
    let code = format!(
        r#"package.path = "{p}/?.lua;{p}/?/init.lua;" .. package.path
           require("nxtree").setup{{ root = "{root}", watch = false, open_on_start = true }}"#,
        p = PLUGIN_LUA,
        root = q(root),
    );
    exec_lua(rpc, &code).await;
}

/// Move focus into the sidebar so key input drives the tree. Done on its own tick,
/// after the build's `nx.layer.main()` has drained (layer-ops drain after view-focus
/// ops within a tick, so focusing must not share the build's tick).
async fn focus_tree(rpc: &Rpc) {
    exec_lua(rpc, r#"require("nxtree").open()"#).await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
}

/// Poll the tree's view buffer until its render has settled (more than the initial
/// empty line), then return its lines. ~3s budget, like the `fs.rs` off-tick polls.
async fn tree_lines(rpc: &Rpc) -> Vec<String> {
    let code = r#"local b = require("nxtree").bufnr()
                  if not b then return nil end
                  local ls = vim.api.nvim_buf_get_lines(b, 0, -1, false)
                  if #ls == 1 and ls[1] == "" then return nil end
                  return ls"#;
    for _ in 0..150 {
        if let Value::Array(a) = exec_lua(rpc, code).await {
            return a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nxtree view never populated");
}

/// Poll until the view has exactly `n` lines (waiting out an async re-render).
async fn wait_line_count(rpc: &Rpc, n: usize) -> Vec<String> {
    for _ in 0..150 {
        let ls = tree_lines(rpc).await;
        if ls.len() == n {
            return ls;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "nxtree view never reached {n} lines; last = {:?}",
        tree_lines(rpc).await
    );
}

/// A temp directory tree: `dir/zebra/` (a subdir holding `inner.txt`), `dir/apple.txt`,
/// `dir/mango.lua`. Returns the root.
fn sample_tree(tag: &str) -> std::path::PathBuf {
    let root = temp_dir(tag);
    fs::create_dir(root.join("zebra")).unwrap();
    fs::write(root.join("zebra/inner.txt"), "INNER\n").unwrap();
    fs::write(root.join("apple.txt"), "APPLE\n").unwrap();
    fs::write(root.join("mango.lua"), "-- mango\n").unwrap();
    root
}

/// True if some line contains `needle`.
fn has(ls: &[String], needle: &str) -> bool {
    ls.iter().any(|l| l.contains(needle))
}

/// The 0-based index of the first line containing `needle`.
fn index_of(ls: &[String], needle: &str) -> usize {
    ls.iter()
        .position(|l| l.contains(needle))
        .expect("line present")
}

// ===== render ================================================================

/// The freshly-opened tree lists the root's children, directories first then files
/// alphabetically — one view line per node, the root header on top.
#[tokio::test]
async fn initial_render_lists_children_dirs_first() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_render");
    open_tree(&rpc, &root).await;

    let ls = wait_line_count(&rpc, 4).await; // root header + zebra/ + apple.txt + mango.lua
    assert!(has(&ls, "zebra"), "the subdirectory is listed: {ls:?}");
    assert!(
        has(&ls, "apple.txt") && has(&ls, "mango.lua"),
        "files listed: {ls:?}"
    );
    assert!(
        index_of(&ls, "zebra") < index_of(&ls, "apple.txt"),
        "the directory sorts before files: {ls:?}"
    );
    assert!(
        index_of(&ls, "apple.txt") < index_of(&ls, "mango.lua"),
        "files sort alphabetically: {ls:?}"
    );
    // `zebra` is a directory, shown with a trailing slash.
    assert!(
        has(&ls, "zebra/"),
        "directories show a trailing slash: {ls:?}"
    );
    // The subdir's contents are NOT loaded until it is expanded (lazy).
    assert!(
        !has(&ls, "inner.txt"),
        "subdir contents are lazy, not shown yet: {ls:?}"
    );
}

// ===== expand / collapse =====================================================

/// `<CR>` on a directory expands it — lazily scandir'ing its children on first open —
/// and a second `<CR>` collapses it again.
#[tokio::test]
async fn enter_on_dir_expands_and_collapses() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_expand");
    open_tree(&rpc, &root).await;
    wait_line_count(&rpc, 4).await;
    focus_tree(&rpc).await;

    // Cursor starts on the root (line 1); move down to `zebra/` (line 2) and expand.
    feed_sync(&rpc, "j").await;
    feed_sync(&rpc, "<CR>").await;
    let ls = wait_line_count(&rpc, 5).await; // + inner.txt
    assert!(
        has(&ls, "inner.txt"),
        "expanding the dir revealed its child: {ls:?}"
    );
    assert!(
        index_of(&ls, "inner.txt") > index_of(&ls, "zebra"),
        "the child renders under its parent: {ls:?}"
    );

    // Collapse again.
    feed_sync(&rpc, "<CR>").await;
    let ls = wait_line_count(&rpc, 4).await;
    assert!(
        !has(&ls, "inner.txt"),
        "collapsing hid the child again: {ls:?}"
    );
}

// ===== open in main ==========================================================

/// `<CR>` on a file opens it in the MAIN editor area (focus crosses out of the
/// sidebar), leaving the dock and the tree in place.
#[tokio::test]
async fn enter_on_file_opens_in_main() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_open");
    open_tree(&rpc, &root).await;
    let ls = wait_line_count(&rpc, 4).await;
    focus_tree(&rpc).await;

    // Move to apple.txt and open it.
    let target = index_of(&ls, "apple.txt"); // 0-based view line
    feed_sync(&rpc, &format!("{}G", target + 1)).await; // 1-based line
    feed_sync(&rpc, "<CR>").await;

    assert_eq!(
        lines(&rpc).await,
        vec!["APPLE"],
        "the file opened in the focused main window"
    );
    assert_eq!(
        win_count(&rpc).await,
        2,
        "the sidebar dock is still open alongside main"
    );
}

// ===== read-only =============================================================

/// The tree buffer is inert to the editing grammar: text-mutating keys can't corrupt
/// the plugin-owned content and `i` never enters insert mode.
#[tokio::test]
async fn tree_is_inert_to_editing_keys() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_inert");
    open_tree(&rpc, &root).await;
    let before = wait_line_count(&rpc, 4).await;
    focus_tree(&rpc).await;

    feed_sync(&rpc, "dd").await; // would delete a line
    feed_sync(&rpc, "x").await; // would delete a char
    feed_sync(&rpc, "iHELLO").await; // would insert; `i` must be inert here
    assert_eq!(
        mode(&rpc).await,
        "n",
        "`i` did not enter insert mode in the tree"
    );

    let after = tree_lines(&rpc).await;
    assert_eq!(
        after, before,
        "editing keys left the tree content unchanged"
    );
}

// ===== navigation ============================================================

/// Plain motion works inside the tree (it's a normal nomodifiable buffer).
#[tokio::test]
async fn navigation_moves_within_the_tree() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_nav");
    open_tree(&rpc, &root).await;
    wait_line_count(&rpc, 4).await;
    focus_tree(&rpc).await;

    // Assert the row only — the column lands on the first non-blank (after the icon
    // glyph), which isn't what navigation is about.
    feed_sync(&rpc, "G").await;
    assert_eq!(cursor(&rpc).await.0, 4, "G went to the last node");
    feed_sync(&rpc, "gg").await;
    assert_eq!(cursor(&rpc).await.0, 1, "gg went to the root header");
}

// ===== refresh ===============================================================

/// `:NxTreeRefresh` (and the watch path it shares) re-scans the tree and surfaces a
/// file created on disk after the initial render — without collapsing the tree.
#[tokio::test]
async fn refresh_surfaces_a_new_file() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_refresh");
    open_tree(&rpc, &root).await;
    wait_line_count(&rpc, 4).await;

    // Create a file on disk, then refresh.
    fs::write(root.join("alpha.md"), "# new\n").unwrap();
    exec_lua(&rpc, r#"require("nxtree").refresh()"#).await;

    let ls = wait_line_count(&rpc, 5).await;
    assert!(
        has(&ls, "alpha.md"),
        "refresh picked up the new file: {ls:?}"
    );
}

// ===== shipped example =======================================================

/// The shipped `examples/nxtree/init.lua` loads end-to-end: it requires the plugin
/// off the runtimepath, mounts the sidebar dock (+ the main window), and lands focus
/// back in the main area — guarding the example against drift.
#[tokio::test]
async fn example_config_loads_and_mounts() {
    let (rpc, _incoming) = start().await;
    // The example requires `nxtree` / `git_signs` off the runtimepath; seed it.
    let init = include_str!("../../../examples/nxtree/init.lua");
    let code = format!(
        r#"package.path = "{p}/?.lua;{p}/?/init.lua;" .. package.path
           {init}"#,
        p = PLUGIN_LUA,
        init = init,
    );
    exec_lua(&rpc, &code).await;

    assert_eq!(
        win_count(&rpc).await,
        2,
        "the example mounts a left-dock sidebar alongside the main window"
    );
    // Focus is in the main area after build (the empty startup buffer), not the tree.
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "focus landed in main, not the sidebar"
    );
}

// ===== read-only state =======================================================

/// The tree buffer is plugin-owned content with no disk backing, so it must NEVER
/// read as modified — otherwise `:qa` reports E37 ("no write since last change") and
/// the statusline shows a `[+]`, as if it wanted saving. (Regression guard: rendering
/// rewrites the rope wholesale via `mark_resync`, which used to leave `modified`.)
#[tokio::test]
async fn tree_buffer_is_never_modified() {
    let (rpc, _incoming) = start().await;
    let root = sample_tree("nxtree_modified");
    open_tree(&rpc, &root).await;
    wait_line_count(&rpc, 4).await;

    let q = r#"return vim.bo[require("nxtree").bufnr()].modified"#;
    assert_eq!(
        exec_lua(&rpc, q).await.as_bool(),
        Some(false),
        "the freshly-rendered tree buffer must not be modified"
    );

    // Re-render (expand a dir) — a second wholesale rewrite must not flip it either.
    focus_tree(&rpc).await;
    feed_sync(&rpc, "j").await;
    feed_sync(&rpc, "<CR>").await;
    wait_line_count(&rpc, 5).await;
    assert_eq!(
        exec_lua(&rpc, q).await.as_bool(),
        Some(false),
        "the tree buffer must stay unmodified across re-renders"
    );
}
