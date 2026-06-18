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

#[tokio::test]
async fn examples_autocmd_config_loads_and_drives_the_ex_commands() {
    // End-to-end check of the shipped `examples/autocmd/` config: source its real
    // init.lua (which uses the :augroup / :autocmd / :doautocmd ex-commands via
    // vim.cmd), then exercise the same surfaces interactively. Proves the example
    // is runnable (no Lua / unknown-command errors) and that the ex-commands drive
    // the shared autocmd store.
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/autocmd/init.lua");
    let init = std::fs::read_to_string(&example).expect("read examples/autocmd/init.lua");
    let dir = temp_dir("examples_autocmd");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    // §2 + §4 registered a Demo-group BufReadPost autocmd and a User Greet one
    // through `:autocmd` — both live in the same store nvim_get_autocmds reads.
    let demo = exec_lua(
        &rpc,
        "local a = vim.api.nvim_get_autocmds({ event = 'BufReadPost' }) \
         for _, au in ipairs(a) do if au.group_name == 'Demo' then return au.command end end \
         return ''",
    )
    .await;
    assert!(
        demo.as_str().unwrap_or("").contains("echo"),
        "the Demo augroup's BufReadPost command-string autocmd is registered"
    );

    // §4's `User Greet` autocmd runs `echo "Hello from the Greet autocmd"`; firing
    // it puts that text on the message line (the command-string + :echo path).
    let greet = message(&redraw_after(&rpc, &mut incoming, ":doautocmd User Greet<CR>").await);
    assert_eq!(greet, "Hello from the Greet autocmd", ":echo autocmd fired");

    // The interactive path: define another User autocmd via `:autocmd`, fire it
    // with `:doautocmd`, and see its `:echo` land on the message line.
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User Hi echo \"hi there\"<CR>",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":doautocmd User Hi<CR>").await);
    assert_eq!(msg, "hi there", ":autocmd + :doautocmd drove a fire");

    // `:autocmd! User` clears every User autocmd (the one just added and §4's).
    redraw_after(&rpc, &mut incoming, ":autocmd! User<CR>").await;
    let left = exec_lua(
        &rpc,
        "return #vim.api.nvim_get_autocmds({ event = 'User' })",
    )
    .await;
    assert_eq!(left.as_u64(), Some(0), ":autocmd! User cleared them all");
}
