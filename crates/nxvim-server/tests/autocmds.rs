//! Behavior tests for the autocmd substrate, driven black-box over RPC exactly
//! like `editing.rs` / `buffers.rs`. Phase 1 proves the *bridge* in isolation —
//! registration, augroup `clear`, manual firing via `nvim_exec_autocmds`, and
//! deletion — with **zero** editor lifecycle wiring: a callback that `print`s a
//! marker is fired manually (through `:lua`), and the marker on the message line
//! is the observable assertion. (Editor-emitted events arrive in Phases 2–3.)
//!
//! Integration-test files don't share a module, so the `start*/feed/...` helpers
//! here are copied from the `editing.rs` pattern rather than imported.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, drain_to_latest_redraw, exec_lua, message, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also used as the runtimepath), and return a connected client.
async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
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

/// Like [`start_with_config`] but also opens `file` in the initial buffer, so the
/// startup lifecycle seed (`BufReadPost`→`FileType`→`BufEnter`) fires for it.
async fn start_with_file_and_config(
    dir: &std::path::Path,
    file: &str,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        file: Some(file.to_string()),
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Feed `keys`, then return the `redraw` map the server emitted for that input.
///
/// The server processes messages serially, writing each message's response then
/// its `redraw`; we send `nx_input` then a `nvim_get_mode` barrier, so once the
/// barrier `.await` resolves the input's redraw is already queued. We take the
/// *most recent* queued redraw, not the first: a frame still in flight from
/// earlier in the test (the startup frame, or a previous call's trailing barrier
/// repaint) can land in `incoming` after the pre-drain below when the reader task
/// lags under load, and taking the first would then return that stale frame. The
/// input's redraw is the newest one present (the barrier changes no state and its
/// repaint trails), so draining to the latest skips the stragglers.
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // drop notifications buffered earlier
    rpc.request("nx_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
        return map;
    }
    // The barrier guarantees the input's redraw is queued before its response, so
    // the drain above should have found it. Under heavy load the reader task can
    // lag; poll a bounded while rather than failing on the first miss.
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return map;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

/// Feed a `:lua <chunk><CR>` line and return the resulting message line — the
/// channel a fired callback's `print` lands on.
async fn lua_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, chunk: &str) -> String {
    message(&redraw_after(rpc, incoming, &format!(":lua {chunk}<CR>")).await)
}

#[tokio::test]
async fn exec_autocmds_runs_callback_with_buffer_and_match_args() {
    // A callback registered for a custom event runs on nvim_exec_autocmds and
    // sees the buffer/pattern it was fired with, surfaced via print().
    let dir = temp_dir("au_exec");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('User', {\n\
         \x20 pattern = 'Marker',\n\
         \x20 callback = function(a) print('buf=' .. tostring(a.buf) .. ' match=' .. tostring(a.match)) end,\n\
         })\n",
    )
    .await;
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'Marker', buffer = 7 })",
    )
    .await;
    assert_eq!(msg, "buf=7 match=Marker");
}

#[tokio::test]
async fn augroup_clear_drops_prior_autocmds_no_double_fire() {
    // Re-running nvim_create_augroup(name, {clear=true}) must remove the group's
    // previous autocmd, so firing the event runs the callback exactly once even
    // after a re-register (the re-sourcing-a-config case).
    let dir = temp_dir("au_clear");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local function register(tag)\n\
         \x20 local g = vim.api.nvim_create_augroup('G', { clear = true })\n\
         \x20 vim.api.nvim_create_autocmd('User', { group = g, pattern = 'M',\n\
         \x20   callback = function() print('fire ' .. tag) end })\n\
         end\n\
         register('first')\n\
         register('second')\n",
    )
    .await;
    // Both registrations cleared the group first, so only the 'second' callback
    // survives; firing prints it once, not once per registration.
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M' })",
    )
    .await;
    assert_eq!(msg, "fire second");
}

#[tokio::test]
async fn del_autocmd_stops_the_callback_firing() {
    let dir = temp_dir("au_del");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.au = vim.api.nvim_create_autocmd('User', { pattern = 'M',\n\
         \x20 callback = function() print('should-not-fire') end })\n",
    )
    .await;
    // Delete it, then fire: the callback must not run, so the message line is
    // whatever the del+fire line itself prints (a sentinel) — not the callback.
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_del_autocmd(_G.au) \
         vim.api.nvim_exec_autocmds('User', { pattern = 'M' }) print('done')",
    )
    .await;
    assert_eq!(msg, "done");
}

#[tokio::test]
async fn get_autocmds_reflects_clear_and_del() {
    // The introspection affordance: after a clear + a del, only the live autocmd
    // remains, and nvim_get_autocmds reports its event.
    let dir = temp_dir("au_get");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_augroup('G', { clear = true })\n\
         vim.api.nvim_create_autocmd('FileType', { group = 'G', callback = function() end })\n\
         vim.api.nvim_create_augroup('G', { clear = true })\n\
         _G.keep = vim.api.nvim_create_autocmd('User', { callback = function() end })\n",
    )
    .await;
    // Scope the queries to this test's own group / event: group `G` is empty after
    // the clear (its `FileType` autocmd gone), and the kept `User` autocmd remains.
    // (An unfiltered or by-event `FileType` query would now also include nxvim's
    // built-in ftplugin autocmds — `FileType nxdir`/`qf` install the explorer /
    // quickfix buffer-local maps — exactly as neovim's own built-ins do.)
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "local g = vim.api.nvim_get_autocmds({ group = 'G' }) \
         local u = vim.api.nvim_get_autocmds({ event = 'User' }) \
         print(#g .. ':' .. #u .. ':' .. u[1].event)",
    )
    .await;
    assert_eq!(msg, "0:1:User");
}

#[tokio::test]
async fn buffer_local_autocmd_only_fires_for_its_buffer() {
    // opts.buffer scopes an autocmd: firing for a different buffer is a no-op,
    // firing for its buffer runs it.
    let dir = temp_dir("au_buflocal");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('User', { buffer = 3, pattern = 'M',\n\
         \x20 callback = function() print('ran') end })\n",
    )
    .await;
    let other = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M', buffer = 9 }) print('miss')",
    )
    .await;
    assert_eq!(
        other, "miss",
        "buffer-local autocmd must not fire for buffer 9"
    );
    let mine = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M', buffer = 3 })",
    )
    .await;
    assert_eq!(
        mine, "ran",
        "buffer-local autocmd must fire for its own buffer"
    );
}

#[tokio::test]
async fn buf_get_name_and_expand_read_the_snapshot() {
    // The snapshot backs nvim_buf_get_name(0) and expand('%'...): set it, then
    // read the path and its modifiers.
    let dir = temp_dir("au_snapshot");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "nx._set_cur_buf(4, '/tmp/foo/bar.rs') \
         print(vim.api.nvim_buf_get_name(0) .. '|' .. vim.fn.expand('%:t') .. '|' .. vim.fn.expand('%:h'))",
    )
    .await;
    assert_eq!(msg, "/tmp/foo/bar.rs|bar.rs|/tmp/foo");
}

// ----- Phase 2: editor-emitted buffer lifecycle events -----------------------

#[tokio::test]
async fn opening_a_file_fires_filetype_with_filetype_and_path() {
    // The startup seed fires FileType for the opened buffer, with the pattern set
    // to the detected filetype and `args.file` the buffer's path.
    let dir = temp_dir("au_ft");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'ft=' .. a.match .. ' file=' .. a.file end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, '|'))").await;
    assert_eq!(msg, format!("ft=rust file={}", file.display()));
}

#[tokio::test]
async fn lifecycle_order_is_bufreadpost_filetype_bufenter() {
    // First open of a file fires the three events in neovim's order.
    let dir = temp_dir("au_order");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         local function rec(tag) return function() _G.log[#_G.log+1] = tag end end\n\
         vim.api.nvim_create_autocmd('BufReadPost', { callback = rec('read') })\n\
         vim.api.nvim_create_autocmd('FileType', { callback = rec('ft') })\n\
         vim.api.nvim_create_autocmd('BufEnter', { callback = rec('enter') })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "read,ft,enter");
}

#[tokio::test]
async fn switching_buffers_fires_bufenter_but_not_refire_filetype() {
    // Opening a second file announces it (FileType fires once); switching back to
    // the first, already-announced buffer fires BufEnter only — no FileType re-fire.
    let dir = temp_dir("au_switch");
    let a = dir.join("a.rs");
    let b = dir.join("b.lua");
    std::fs::write(&a, "fn main() {}\n").expect("write a");
    std::fs::write(&b, "return {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'ft' .. a.buf end })\n\
         vim.api.nvim_create_autocmd('BufEnter', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'be' .. a.buf end })\n",
    )
    .await;
    // startup: buffer 1 (a.rs) -> ft1, be1.
    // :edit b.lua -> buffer 2 announced -> ft2, be2.
    // :b1 -> back to buffer 1, already announced -> be1 only.
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    redraw_after(&rpc, &mut incoming, ":b1<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "ft1,be1,ft2,be2,be1");
}

#[tokio::test]
async fn bufreadpost_callback_reads_buffer_name_from_snapshot() {
    // A BufReadPost callback resolves the buffer that fired via the snapshot —
    // nvim_buf_get_name(0) returns its path.
    let dir = temp_dir("au_readname");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function() _G.seen = vim.api.nvim_buf_get_name(0) end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, file.display().to_string());
}

// ----- Phase 3: mode event (InsertEnter) -------------------------------------

#[tokio::test]
async fn entering_insert_fires_insertenter_once_per_entry() {
    // InsertEnter fires on the transition *into* insert — once on the `i`, not
    // per typed character — and again on a fresh entry via `o`.
    let dir = temp_dir("au_insert");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('InsertEnter', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    // `iabc<Esc>`: enter insert (fires once), type three chars (stay in insert —
    // no re-fire), leave. The count proves typing doesn't re-trigger the event.
    redraw_after(&rpc, &mut incoming, "iabc<Esc>").await;
    let after_i = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(
        after_i, "1",
        "InsertEnter fires once on i, not per typed char"
    );
    // `o<Esc>`: open a line (a fresh insert entry) and leave — fires again.
    redraw_after(&rpc, &mut incoming, "o<Esc>").await;
    let after_o = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(
        after_o, "2",
        "re-entering insert via o fires InsertEnter again"
    );
}

#[tokio::test]
async fn insertenter_sees_buffer_context() {
    // The InsertEnter callback resolves the current buffer via the snapshot, just
    // like the buffer events do.
    let dir = temp_dir("au_insert_ctx");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('InsertEnter', {\n\
         \x20 callback = function(a) _G.seen = a.buf .. ':' .. vim.api.nvim_buf_get_name(0) end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, format!("1:{}", file.display()));
}

// ----- Phase 5: window lifecycle events --------------------------------------

#[tokio::test]
async fn splitting_fires_winnew_winleave_winenter_in_order() {
    // `<C-w>s` creates a window (WinNew), leaves the old one (WinLeave), and
    // enters the new one (WinEnter). The `match` is the window id, so each marker
    // carries which window fired.
    let dir = temp_dir("au_win_split");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinNew', { callback = rec('new') })\n\
         vim.api.nvim_create_autocmd('WinLeave', { callback = rec('leave') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n",
    )
    .await;
    // Drop the startup WinEnter(1) so we observe only the split's events.
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    redraw_after(&rpc, &mut incoming, "<C-w>s").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "new2,leave1,enter2");
}

#[tokio::test]
async fn closing_a_window_fires_winclosed_then_winenter_survivor() {
    // `<C-w>c` on the focused window fires WinClosed for it and WinEnter for the
    // survivor that takes focus.
    let dir = temp_dir("au_win_close");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinClosed', { callback = rec('closed') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n",
    )
    .await;
    // Split (focus moves to the new window 2), then clear the log.
    redraw_after(&rpc, &mut incoming, "<C-w>s").await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // Close the focused window 2; window 1 survives and takes focus.
    redraw_after(&rpc, &mut incoming, "<C-w>c").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "closed2,enter1");
}

#[tokio::test]
async fn focus_motion_fires_winleave_and_winenter() {
    let dir = temp_dir("au_win_focus");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinLeave', { callback = rec('leave') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "<C-w>s").await; // two windows, focus on 2
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // `<C-w>j` moves focus from window 2 (top) down to window 1 (bottom).
    redraw_after(&rpc, &mut incoming, "<C-w>j").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "leave2,enter1");
}

// ----- Phase 3: tab lifecycle events -----------------------------------------

#[tokio::test]
async fn tabnew_fires_tabnew_tableave_tabenter_in_order() {
    // `:tabnew` creates a tab (TabNew), leaves the old one (TabLeave), and enters
    // the new one (TabEnter). The `match` is the tab id, so each marker says which.
    let dir = temp_dir("au_tab_new");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('TabNew', { callback = rec('new') })\n\
         vim.api.nvim_create_autocmd('TabLeave', { callback = rec('leave') })\n\
         vim.api.nvim_create_autocmd('TabEnter', { callback = rec('enter') })\n",
    )
    .await;
    // Drop any startup events so we observe only the `:tabnew` transition.
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    redraw_after(&rpc, &mut incoming, ":tabnew<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "new2,leave1,enter2");
}

#[tokio::test]
async fn tab_switch_brackets_the_window_events() {
    // A tab switch fires the bracket `TabLeave → WinLeave → WinEnter → TabEnter`.
    // Recording only the tags (no ids) makes the assertion purely about ordering.
    let dir = temp_dir("au_tab_bracket");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function() _G.log[#_G.log+1] = tag end end\n\
         vim.api.nvim_create_autocmd('TabLeave', { callback = rec('TL') })\n\
         vim.api.nvim_create_autocmd('WinLeave', { callback = rec('WL') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('WE') })\n\
         vim.api.nvim_create_autocmd('TabEnter', { callback = rec('TE') })\n",
    )
    .await;
    // Two tabs (now on tab 2), then clear the log so only the switch is observed.
    redraw_after(&rpc, &mut incoming, ":tabnew<CR>").await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // `gT` switches from tab 2 back to tab 1.
    redraw_after(&rpc, &mut incoming, "gT").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "TL,WL,WE,TE", "tab events bracket the window events");
}

#[tokio::test]
async fn tabclose_fires_tabenter_survivor_then_tabclosed() {
    // Closing a tab enters the survivor (TabEnter) and then announces the gone tab
    // (TabClosed) — the tab, and its windows, are already removed.
    let dir = temp_dir("au_tab_close");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('TabEnter', { callback = rec('enter') })\n\
         vim.api.nvim_create_autocmd('TabClosed', { callback = rec('closed') })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":tabnew<CR>").await; // on tab 2
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    redraw_after(&rpc, &mut incoming, ":tabclose<CR>").await; // back to tab 1
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "enter1,closed2");
}

// ----- :autocmd / :augroup / :doautocmd ex-commands --------------------------
// The Vimscript front-end (nx._ex_autocmd / _ex_augroup / _ex_doautocmd) drives
// the same store the nvim_* API uses. These feed the `:`-command forms over RPC
// and observe firing through a command-string autocmd's `print` / a counter.

#[tokio::test]
async fn ex_autocmd_defines_a_command_autocmd_that_doautocmd_fires() {
    // `:autocmd {event} {pat} {cmd}` registers a command-string autocmd;
    // `:doautocmd {event} {pat}` fires it, running the command (here a `:lua
    // print`). Uses the `:au` / `:doau` abbreviations to prove those resolve too.
    let dir = temp_dir("ex_au_define");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":au User Marker lua print('fired')<CR>",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":doau User Marker<CR>").await);
    assert_eq!(msg, "fired");
}

#[tokio::test]
async fn ex_augroup_block_assigns_the_current_group() {
    // `:augroup Foo` … `:augroup END` groups the autocmds defined between them, so
    // nvim_get_autocmds reports the group name — exactly as the API's `group=`.
    let dir = temp_dir("ex_aug_block");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(&rpc, &mut incoming, ":augroup Foo<CR>").await;
    redraw_after(&rpc, &mut incoming, ":autocmd User M lua print('x')<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup END<CR>").await;
    let name = exec_lua(
        &rpc,
        "local a = vim.api.nvim_get_autocmds({ event = 'User' }) return a[1].group_name",
    )
    .await;
    assert_eq!(
        name.as_str(),
        Some("Foo"),
        "the autocmd landed in group Foo"
    );
}

#[tokio::test]
async fn ex_autocmd_bang_clears_matching_autocmds() {
    // `:autocmd! {event}` removes every autocmd for that event, so a later
    // `:doautocmd` fires nothing. The sentinel printed by the fire line itself is
    // what lands on the message line — never the (now-cleared) callback.
    let dir = temp_dir("ex_au_bang");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User M lua print('should-not-fire')<CR>",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":autocmd! User<CR>").await;
    let gone = exec_lua(
        &rpc,
        "return #vim.api.nvim_get_autocmds({ event = 'User' })",
    )
    .await;
    assert_eq!(gone.as_u64(), Some(0), ":autocmd! User cleared the autocmd");
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.cmd('doautocmd User M') print('done')",
    )
    .await;
    assert_eq!(msg, "done", "the cleared autocmd did not fire");
}

#[tokio::test]
async fn ex_augroup_bang_deletes_the_group_and_its_autocmds() {
    // `:augroup! Foo` deletes the group and every autocmd in it.
    let dir = temp_dir("ex_aug_bang");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(&rpc, &mut incoming, ":augroup Foo<CR>").await;
    redraw_after(&rpc, &mut incoming, ":autocmd User M lua print('x')<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup END<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup! Foo<CR>").await;
    let gone = exec_lua(
        &rpc,
        "return #vim.api.nvim_get_autocmds({ event = 'User' })",
    )
    .await;
    assert_eq!(gone.as_u64(), Some(0), "the group's autocmd was removed");
    let id = exec_lua(&rpc, "return nx._augroups.Foo == nil").await;
    assert_eq!(id.as_bool(), Some(true), "the group name was deleted");
}

#[tokio::test]
async fn example_autocmd_config_loads_and_lifecycle_events_fire() {
    // The shipped examples/autocmd config must load, and its §5 lifecycle autocmds
    // must fire end-to-end: editing the buffer triggers TextChanged (notified onto
    // the message line) and `:w` triggers the `*.txt` BufWritePost. The example's
    // `init.lua` is copied into a throwaway dir alongside a throwaway `.txt` sample,
    // so the test's `:w` never touches the shipped file (hermetic — see the harness).
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/autocmd/init.lua")
        .canonicalize()
        .expect("examples/autocmd/init.lua");
    let init_lua = std::fs::read_to_string(&example).expect("read example init.lua");
    let dir = temp_dir("au_example");
    let sample = dir.join("sample.txt");
    std::fs::write(&sample, "hello from the autocmd example\n").expect("write sample");
    let (rpc, mut incoming) =
        start_with_file_and_config(&dir, sample.to_str().unwrap(), &init_lua).await;

    // A Normal-mode edit fires the example's TextChanged handler.
    let changed = message(&redraw_after(&rpc, &mut incoming, "x").await);
    assert_eq!(
        changed, "buffer changed",
        "TextChanged notify reached the line"
    );

    // Saving the `.txt` sample fires the `*.txt` BufWritePost handler.
    let saved = message(&redraw_after(&rpc, &mut incoming, ":w<CR>").await);
    assert_eq!(
        saved,
        format!("saved {}", sample.display()),
        "the *.txt BufWritePost glob fired on save"
    );
}

// ----- write events: BufWritePre / BufWritePost -----------------------------

#[tokio::test]
async fn writing_a_file_fires_bufwritepre_then_bufwritepost() {
    // A successful `:w` fires BufWritePre then BufWritePost, each carrying the
    // written file's path as `args.file` — the order is the neovim contract.
    let dir = temp_dir("au_write");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'pre:' .. a.file end })\n\
         vim.api.nvim_create_autocmd('BufWritePost', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'post:' .. a.file end })\n",
    )
    .await;
    // Modify the buffer, then write it.
    redraw_after(&rpc, &mut incoming, "ax<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(
        msg,
        format!("pre:{f},post:{f}", f = file.display()),
        "BufWritePre precedes BufWritePost, both with the file path"
    );
}

#[tokio::test]
async fn bufwritepost_sees_the_written_buffer_as_unmodified() {
    // After a `:w`, the BufWritePost callback resolves the saved buffer via the
    // snapshot and `vim.bo.modified` reads the now-cleared `[+]` flag.
    let dir = temp_dir("au_write_clean");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('BufWritePost', {\n\
         \x20 callback = function() _G.seen = vim.api.nvim_buf_get_name(0) end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "ax<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, file.display().to_string());
}

#[tokio::test]
async fn write_autocmd_with_a_glob_pattern_matches_by_extension() {
    // A `BufWritePost *.txt` autocmd fires for a `.txt` file (the glob matches the
    // path tail) but a `*.rs` one does not — the file-pattern matching the events
    // need to be useful (format-on-save is `BufWritePre *.rs`).
    let dir = temp_dir("au_write_glob");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufWritePost', { pattern = '*.txt',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'txt' end })\n\
         vim.api.nvim_create_autocmd('BufWritePost', { pattern = '*.rs',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'rs' end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "ax<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "txt", "only the *.txt glob matches a .txt file");
}

// ----- BufNewFile vs BufReadPost --------------------------------------------

#[tokio::test]
async fn opening_a_nonexistent_file_fires_bufnewfile_not_bufreadpost() {
    // Editing a path with no file on disk fires BufNewFile (with the path), and
    // *not* BufReadPost — matching `vim file-that-does-not-exist`.
    let dir = temp_dir("au_newfile");
    let file = dir.join("brand_new.rs"); // deliberately not created
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufNewFile', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'new:' .. a.file end })\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'read' end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, format!("new:{}", file.display()));
}

// ----- BufLeave / BufDelete --------------------------------------------------

#[tokio::test]
async fn switching_buffers_fires_bufleave_for_the_old_buffer() {
    // `:edit b` fires BufLeave for the buffer we leave, then BufEnter for the new
    // one (vim's BufLeave → BufEnter bracket).
    let dir = temp_dir("au_bufleave");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "a\n").expect("write a");
    std::fs::write(&b, "b\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufLeave', {\n\
         \x20 callback = function(x) _G.log[#_G.log+1] = 'leave' .. x.buf end })\n\
         vim.api.nvim_create_autocmd('BufEnter', {\n\
         \x20 callback = function(x) _G.log[#_G.log+1] = 'enter' .. x.buf end })\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await; // drop startup events
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "leave1,enter2");
}

#[tokio::test]
async fn deleting_a_buffer_fires_bufdelete_for_it() {
    // `:bdelete` fires BufDelete, with `args.buf` the deleted buffer's number.
    let dir = temp_dir("au_bufdelete");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "a\n").expect("write a");
    std::fs::write(&b, "b\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('BufDelete', { callback = function(x) _G.seen = x.buf end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await; // buffer 2
    redraw_after(&rpc, &mut incoming, ":bdelete<CR>").await; // delete buffer 2
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, "2", "BufDelete fired for the deleted buffer");
}

// ----- InsertLeave -----------------------------------------------------------

#[tokio::test]
async fn leaving_insert_fires_insertleave_once_per_exit() {
    // InsertLeave fires on the transition *out* of insert — once per `<Esc>`, the
    // mirror of InsertEnter.
    let dir = temp_dir("au_insertleave");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('InsertLeave', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "iabc<Esc>").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "InsertLeave fires once on <Esc>");
    redraw_after(&rpc, &mut incoming, "o<Esc>").await;
    let after2 = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after2, "2", "a fresh insert via o fires InsertLeave again");
}

// ----- TextChanged / TextChangedI -------------------------------------------

#[tokio::test]
async fn editing_in_normal_fires_textchanged() {
    // A change in Normal mode (`x` deletes a char) fires TextChanged.
    let dir = temp_dir("au_textchanged");
    let file = dir.join("f.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('TextChanged', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "x").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "deleting a char in Normal fires TextChanged");
    // A pure motion does not change text — no re-fire.
    redraw_after(&rpc, &mut incoming, "l").await;
    let after2 = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after2, "1", "a motion alone doesn't fire TextChanged");
}

#[tokio::test]
async fn typing_in_insert_fires_textchangedi_per_change() {
    // Each character typed in insert fires TextChangedI (entering insert with `i`
    // doesn't, leaving with `<Esc>` doesn't).
    let dir = temp_dir("au_textchangedi");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('TextChangedI', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "iab<Esc>").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "2", "two typed chars fire TextChangedI twice");
}

// ----- CursorMoved / CursorMovedI -------------------------------------------

#[tokio::test]
async fn moving_the_cursor_fires_cursormoved() {
    // A motion in Normal mode fires CursorMoved each time the cursor lands somewhere
    // new; switching to the command line to read the counter doesn't move it.
    let dir = temp_dir("au_cursormoved");
    let file = dir.join("f.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('CursorMoved', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "j").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "moving down a line fires CursorMoved");
    redraw_after(&rpc, &mut incoming, "j").await;
    let after2 = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after2, "2", "a second motion fires it again");
}

#[tokio::test]
async fn moving_the_cursor_in_insert_fires_cursormovedi() {
    // Moving within insert mode (`<Right>`, no text change) fires CursorMovedI;
    // entering insert with `i` and leaving with `<Esc>` do not.
    let dir = temp_dir("au_cursormovedi");
    let file = dir.join("f.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('CursorMovedI', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "i<Right><Esc>").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "<Right> in insert fires CursorMovedI once");
}

#[tokio::test]
async fn ex_autocmd_once_fires_exactly_once() {
    // `++once` self-removes after the first fire: firing the event twice runs the
    // command (a counter bump) only once.
    let dir = temp_dir("ex_au_once");
    let (rpc, mut incoming) = start_with_config(&dir, "_G.n = 0\n").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User M ++once lua _G.n = _G.n + 1<CR>",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":doautocmd User M<CR>").await;
    redraw_after(&rpc, &mut incoming, ":doautocmd User M<CR>").await;
    let n = exec_lua(&rpc, "return _G.n").await;
    assert_eq!(n.as_u64(), Some(1), "++once autocmd fired exactly once");
}
