//! Behavior tests for **configurable widget keys** (Phase 1: the picker). Every
//! picker key is an ordinary `picker`-mode keymap, not a hardcoded grab, so a user
//! `nx.keymap.set('picker', …)` rebinds it, an empty-function map disables it, and
//! an editing-mode map never leaks into the widget. Plan:
//! `docs/plans/2026-06-16-configurable-widget-keys.md`.
//!
//! Black-box like `picker.rs`: a real server sources an `init.lua`, the picker is
//! driven over RPC, and the outcome is read back through `confirm` (`_G.picked`) so
//! a test asserts on *which item the rebound key selected* rather than on the
//! redraw internals.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, barrier, cursor, exec_lua, feed, lines, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

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

/// A static fruit source whose `confirm` records the chosen fruit in `_G.picked`,
/// plus whatever extra config a test prepends. The list is `apple, apricot, banana,
/// cherry`, so "down one then confirm" lands on `apricot`.
fn src(extra: &str) -> String {
    format!(
        "{extra}\n\
         nx.picker.source {{\n\
           name = 'fruits',\n\
           items = function(ctx)\n\
             for _, t in ipairs({{ 'apple', 'apricot', 'banana', 'cherry' }}) do\n\
               ctx.push {{ text = t, fruit = t }}\n\
             end\n\
           end,\n\
           confirm = function(item) _G.picked = item.fruit end,\n\
         }}"
    )
}

/// Open the picker and wait for it to settle (a barrier flushes the open + source).
async fn open(rpc: &Rpc) {
    exec_lua(rpc, "_G.picked = nil; nx.picker.open('fruits')").await;
    barrier(rpc).await;
}

async fn picked(rpc: &Rpc) -> Option<String> {
    exec_lua(rpc, "return _G.picked")
        .await
        .as_str()
        .map(str::to_string)
}

/// A user `nx.keymap.set('picker', …)` rebinds a picker action to a new key: `<C-j>`
/// moves the selection down even though it is not a default picker key.
#[tokio::test]
async fn user_rebind_moves_selection() {
    let dir = temp_dir("widget_keys_rebind");
    let (rpc, _incoming) = start(
        &dir,
        &src("nx.keymap.set('picker', '<C-j>', nx.picker.actions.next)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-j>"); // rebound: down one (apple -> apricot)
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("apricot"));
}

/// Binding a default picker key to an empty function disables it: `<C-n>` no longer
/// moves the selection, so confirm picks the still-first item.
#[tokio::test]
async fn empty_map_disables_a_default_key() {
    let dir = temp_dir("widget_keys_disable");
    let (rpc, _incoming) = start(
        &dir,
        &src("nx.keymap.set('picker', '<C-n>', function() end)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-n>"); // disabled: selection stays on apple
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("apple"));
}

/// An editing-mode map for the same key does NOT leak into the picker: a normal-mode
/// `<C-n>` map stays dormant while the picker owns input, and the picker's own
/// `<C-n>` (next) still fires.
#[tokio::test]
async fn editing_map_does_not_leak_into_picker() {
    let dir = temp_dir("widget_keys_noleak");
    let (rpc, _incoming) = start(
        &dir,
        &src("_G.leaked = false\n\
              nx.keymap.set('n', '<C-n>', function() _G.leaked = true end)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-n>"); // picker's next, NOT the normal-mode map
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(
        picked(&rpc).await.as_deref(),
        Some("apricot"),
        "picker's own <C-n> moved the selection"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.leaked").await.as_bool(),
        Some(false),
        "the normal-mode <C-n> map never fired inside the picker"
    );
}

/// Rebinding `confirm` to a new key works end to end: `<C-y>` confirms the first item.
#[tokio::test]
async fn rebind_confirm_key() {
    let dir = temp_dir("widget_keys_confirm");
    let (rpc, _incoming) = start(
        &dir,
        &src("nx.keymap.set('picker', '<C-y>', nx.picker.actions.confirm)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-y>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("apple"));
}

/// An unmapped printable key is the picker's text fallthrough: it edits the query
/// (narrowing the list) and never reaches the document buffer.
#[tokio::test]
async fn unmapped_printable_edits_query() {
    let dir = temp_dir("widget_keys_query");
    let (rpc, _incoming) = start(&dir, &src("")).await;
    open(&rpc).await;

    // 'b' is not a default picker map → inserts into the query, narrowing to the
    // fruits containing "b" and confirming the first of them (banana).
    feed(&rpc, "b");
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("banana"));
    // The keystroke never reached the document.
    assert_eq!(lines(&rpc).await, vec![""]);
}

// ===== Phase 2: nx.ui.select (the promptless list, 'select' bucket) ==========

/// Open a three-item `nx.ui.select` whose `:next` records the chosen item in
/// `_G.chosen`; a barrier settles the open.
async fn open_select(rpc: &Rpc, extra: &str) {
    exec_lua(
        rpc,
        &format!(
            "{extra}\n\
             _G.chosen = nil\n\
             nx.ui.select({{ 'alpha', 'beta', 'gamma' }}, {{}}):next(function(item)\n\
               _G.chosen = item\n\
             end)"
        ),
    )
    .await;
    barrier(rpc).await;
}

async fn chosen(rpc: &Rpc) -> Option<String> {
    exec_lua(rpc, "return _G.chosen")
        .await
        .as_str()
        .map(str::to_string)
}

/// The default select keys still navigate + confirm through the keymap engine:
/// `j` moves down one (alpha -> beta), `<CR>` confirms.
#[tokio::test]
async fn select_default_keys_navigate_and_confirm() {
    let dir = temp_dir("widget_keys_select_default");
    let (rpc, _incoming) = start(&dir, "").await;
    open_select(&rpc, "").await;

    feed(&rpc, "j");
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(chosen(&rpc).await.as_deref(), Some("beta"));
}

/// `gg` is a two-key `select` default map (the multi-key widget map the trie
/// handles): from the last item it jumps back to the first.
#[tokio::test]
async fn select_gg_two_key_map_jumps_to_first() {
    let dir = temp_dir("widget_keys_select_gg");
    let (rpc, _incoming) = start(&dir, "").await;
    open_select(&rpc, "").await;

    feed(&rpc, "G"); // last (gamma)
    feed(&rpc, "gg"); // back to first (alpha)
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(chosen(&rpc).await.as_deref(), Some("alpha"));
}

/// A user `nx.keymap.set('select', …)` rebinds a select action to a new key.
#[tokio::test]
async fn select_user_rebind() {
    let dir = temp_dir("widget_keys_select_rebind");
    let (rpc, _incoming) = start(&dir, "").await;
    open_select(
        &rpc,
        "nx.keymap.set('select', '<C-j>', nx.ui.select_actions.last)",
    )
    .await;

    feed(&rpc, "<C-j>"); // rebound: jump to last (gamma)
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(chosen(&rpc).await.as_deref(), Some("gamma"));
}

/// An editing-mode `j` map does NOT leak into the select list: `j` still moves the
/// highlight (the select map), and the normal-mode map never fires.
#[tokio::test]
async fn select_editing_map_does_not_leak() {
    let dir = temp_dir("widget_keys_select_noleak");
    let (rpc, _incoming) = start(&dir, "").await;
    open_select(
        &rpc,
        "_G.leaked = false\n\
         nx.keymap.set('n', 'j', function() _G.leaked = true end)",
    )
    .await;

    feed(&rpc, "j"); // select's next, NOT the normal-mode map
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(chosen(&rpc).await.as_deref(), Some("beta"));
    assert_eq!(
        exec_lua(&rpc, "return _G.leaked").await.as_bool(),
        Some(false),
        "the normal-mode j map never fired inside the select list"
    );
}

// ===== the file explorer (an ordinary nomodifiable buffer + buffer-local maps) =
// Post-unification (docs/plans/2026-06-16-unify-special-buffer-kinds.md): the
// listing is `filetype=nxdir`, navigation is plain normal-mode motion, and `<CR>`
// open / `-` parent are buffer-local default maps from the `FileType nxdir` autocmd.

/// A temp directory with two files and one sub-directory, opened as the explorer in
/// a fresh server. The listing is `../`, `sub/`, `alpha.txt`, `beta.txt` (up entry,
/// then directories, then files). The config/init dir is separate so `init.lua`
/// never pollutes the listing.
async fn open_explorer(init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let cfg = temp_dir("widget_keys_explorer_cfg");
    let content = temp_dir("widget_keys_explorer_dir");
    std::fs::write(content.join("alpha.txt"), "alpha-body\n").expect("write alpha");
    std::fs::write(content.join("beta.txt"), "beta-body\n").expect("write beta");
    std::fs::create_dir(content.join("sub")).expect("mkdir sub");
    let (rpc, incoming) = start(&cfg, init_lua).await;
    feed(&rpc, &format!(":e {}<CR>", content.display()));
    // The explorer (a Lua plugin) fills the listing asynchronously (`nx.fs.readdir`
    // settles off the tick), so poll until the entries appear — a single barrier can lag
    // under parallel test load.
    let want = vec!["../", "sub/", "alpha.txt", "beta.txt"];
    let mut got = lines(&rpc).await;
    for _ in 0..100 {
        if got == want {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        got = lines(&rpc).await;
    }
    assert_eq!(got, want, "explorer listed the fixture directory");
    (rpc, incoming)
}

/// `jj` (ordinary normal motion) moves to `alpha.txt` (row 3) and the buffer-local
/// `<CR>` default map (from the `FileType nxdir` autocmd) opens it.
#[tokio::test]
async fn explorer_default_keys_navigate_and_open() {
    let (rpc, _incoming) = open_explorer("").await;
    feed(&rpc, "jj<CR>");
    barrier(&rpc).await;
    assert_eq!(lines(&rpc).await, vec!["alpha-body"]);
}

/// `gg`/`G` are ordinary normal-mode motions on the listing now (not explorer-bucket
/// maps): `G` goes to the last row, `gg` back to the first (`../`).
#[tokio::test]
async fn explorer_gg_jumps_to_first() {
    let (rpc, _incoming) = open_explorer("").await;
    feed(&rpc, "G"); // last row (beta.txt; 1-based line 4)
    assert_eq!(cursor(&rpc).await.0, 4, "G moved to the last entry");
    feed(&rpc, "gg"); // back to the first row (../; 1-based line 1)
    assert_eq!(cursor(&rpc).await.0, 1, "gg jumped to the first entry");
}

/// The new model: rebind / extend the explorer with an ordinary **buffer-local**
/// map in a `FileType nxdir` autocmd (vim's ftplugin model), not a widget bucket.
/// Here `<C-j>` is bound to `open`, so `jj<C-j>` opens `alpha.txt` like `<CR>` would.
#[tokio::test]
async fn explorer_user_rebind() {
    let (rpc, _incoming) = open_explorer(
        "nx.autocmd.create('FileType', { pattern = 'nxdir', callback = function(a)\n\
         \x20 nx.keymap.set('n', '<C-j>', nx.explorer.actions.open, { buffer = a.buf })\n\
         end })",
    )
    .await;
    feed(&rpc, "jj<C-j>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha-body"],
        "the buffer-local rebind opened the entry"
    );
}

/// `:` falls through to the command line — the explorer's one residual non-map key.
/// A `:lua` command run from the listing proves the command line opened.
#[tokio::test]
async fn explorer_colon_falls_through_to_cmdline() {
    let (rpc, _incoming) = open_explorer("").await;
    feed(&rpc, ":lua _G.from_cmdline = true<CR>");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.from_cmdline").await.as_bool(),
        Some(true),
        ": opened the command line from the explorer listing"
    );
}

/// The explorer is now an ordinary `nomodifiable` buffer (not a grabbing widget), so
/// `j`/`k` are plain normal-mode motions on it **and** an ordinary global normal-mode
/// map applies on it — the inverse of the old widget-bucket isolation.
#[tokio::test]
async fn explorer_is_an_ordinary_buffer() {
    let (rpc, _incoming) = open_explorer(
        "_G.fired = false\n\
         nx.keymap.set('n', 'X', function() _G.fired = true end)",
    )
    .await;

    feed(&rpc, "jj"); // ordinary motions move down the listing
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "j is a normal motion on the listing (row 3 = alpha.txt)"
    );
    feed(&rpc, "X");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.fired").await.as_bool(),
        Some(true),
        "a global normal-mode map applies on the ordinary explorer buffer"
    );
}

// ===== Phase 4: the command line (the 'cmdline' alias over the 'c' bucket) ====

/// The default `submit` (`<CR>`) runs the line and `cancel` (`<Esc>`) abandons it,
/// both now through the keymap engine.
#[tokio::test]
async fn cmdline_default_submit_and_cancel() {
    let dir = temp_dir("widget_keys_cmdline_submit");
    let (rpc, _incoming) = start(&dir, "").await;

    feed(&rpc, ":lua _G.ran = true<CR>"); // submit runs it
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.ran").await.as_bool(),
        Some(true),
        "submit ran the command line"
    );

    feed(&rpc, ":lua _G.ran2 = true<Esc>"); // cancel abandons it
    barrier(&rpc).await;
    assert!(
        exec_lua(&rpc, "return _G.ran2").await.is_nil(),
        "cancel abandoned the line without running it"
    );
}

/// `history_prev` (`<C-p>`) recalls the previous ex command, then `submit` runs it —
/// both default `cmdline` maps.
#[tokio::test]
async fn cmdline_history_prev_recalls() {
    let dir = temp_dir("widget_keys_cmdline_history");
    let (rpc, _incoming) = start(&dir, "").await;

    feed(&rpc, ":lua _G.h = 7<CR>"); // remembered in ex history
    barrier(&rpc).await;
    exec_lua(&rpc, "_G.h = nil").await;

    feed(&rpc, ":"); // open a fresh ex line
    feed(&rpc, "<C-p>"); // history_prev recalls "lua _G.h = 7"
    feed(&rpc, "<CR>"); // submit runs the recalled line
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.h").await.as_i64(),
        Some(7),
        "history_prev recalled the command and submit ran it"
    );
}

/// `to_start` (`<Home>`) moves the command cursor and a typed character inserts
/// there — the cursor-motion map plus the residual text fallthrough.
#[tokio::test]
async fn cmdline_to_start_then_insert() {
    let dir = temp_dir("widget_keys_cmdline_home");
    let (rpc, _incoming) = start(&dir, "").await;

    // Type the command missing its leading `l`, jump to the start, insert it.
    feed(&rpc, ":ua _G.p = 9");
    feed(&rpc, "<Home>"); // to_start
    feed(&rpc, "l"); // inserts at the start -> "lua _G.p = 9"
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.p").await.as_i64(),
        Some(9),
        "to_start moved the cursor and the inserted char landed at the front"
    );
}

/// `backspace` (`<BS>`) deletes the char before the cursor — a default `cmdline` map.
#[tokio::test]
async fn cmdline_backspace_deletes() {
    let dir = temp_dir("widget_keys_cmdline_bs");
    let (rpc, _incoming) = start(&dir, "").await;

    feed(&rpc, ":lua _G.b = 1XX"); // two stray chars
    feed(&rpc, "<BS><BS>"); // delete both -> "lua _G.b = 1"
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.b").await.as_i64(),
        Some(1),
        "backspace deleted the stray chars before submit"
    );
}

/// `<C-r>{register}` still inserts the register's text into the command line: `<C-r>`
/// is a `cmdline` map (arms the read) and the register name after it is read raw.
#[tokio::test]
async fn cmdline_insert_register_raw_read() {
    let dir = temp_dir("widget_keys_cmdline_creg");
    let (rpc, _incoming) = start(&dir, "").await;

    // Put WORD into register a (yank the inner word), then pull it into a command.
    feed(&rpc, "iWORD<Esc>");
    feed(&rpc, "\"ayiw");
    feed(&rpc, ":lua _G.reg = '");
    feed(&rpc, "<C-r>a"); // arms via the map, then reads 'a' raw -> inserts WORD
    feed(&rpc, "'<CR>");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.reg").await.as_str(),
        Some("WORD"),
        "<C-r>a inserted register a's contents into the command line"
    );
}

/// A user `nx.keymap.set('cmdline', …)` rebinds a cmdline action to a new key:
/// `<C-j>` submits the line.
#[tokio::test]
async fn cmdline_user_rebind() {
    let dir = temp_dir("widget_keys_cmdline_rebind");
    let (rpc, _incoming) = start(
        &dir,
        "nx.keymap.set('cmdline', '<C-j>', nx.cmdline.actions.submit)",
    )
    .await;

    feed(&rpc, ":lua _G.rb = 1");
    feed(&rpc, "<C-j>"); // rebound submit
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.rb").await.as_i64(),
        Some(1),
        "the rebound <C-j> submitted the command line"
    );
}

/// Binding a default cmdline key to an empty function disables it: `<CR>` no longer
/// submits, so the line never runs (it stays open until cancelled).
#[tokio::test]
async fn cmdline_empty_map_disables_submit() {
    let dir = temp_dir("widget_keys_cmdline_disable");
    let (rpc, _incoming) = start(&dir, "nx.keymap.set('cmdline', '<CR>', function() end)").await;

    feed(&rpc, ":lua _G.d = 1");
    feed(&rpc, "<CR>"); // disabled: does nothing, the line is not run
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getcmdtype()").await.as_str(),
        Some(":"),
        "the command line is still open (disabled <CR> did not submit)"
    );
    assert!(
        exec_lua(&rpc, "return _G.d").await.is_nil(),
        "the disabled <CR> never ran the command"
    );
    feed(&rpc, "<Esc>"); // clean up
}

/// A normal-mode `<C-p>` map does NOT leak into the command line: `<C-p>` recalls
/// history (the cmdline map), and the normal-mode map never fires.
#[tokio::test]
async fn cmdline_editing_map_does_not_leak() {
    let dir = temp_dir("widget_keys_cmdline_noleak");
    let (rpc, _incoming) = start(
        &dir,
        "_G.leaked = false\n\
         nx.keymap.set('n', '<C-p>', function() _G.leaked = true end)",
    )
    .await;

    feed(&rpc, ":lua _G.n = 5<CR>"); // remembered
    barrier(&rpc).await;
    exec_lua(&rpc, "_G.n = nil").await;

    feed(&rpc, ":");
    feed(&rpc, "<C-p>"); // cmdline history_prev, NOT the normal-mode map
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.n").await.as_i64(),
        Some(5),
        "the cmdline's own <C-p> recalled history"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.leaked").await.as_bool(),
        Some(false),
        "the normal-mode <C-p> map never fired in the command line"
    );
}
