//! Behavior tests for the autocmd substrate, driven black-box over RPC exactly
//! like `editing.rs` / `buffers.rs`. Phase 1 proves the *bridge* in isolation —
//! registration, augroup `clear`, manual firing via `nvim_exec_autocmds`, and
//! deletion — with **zero** editor lifecycle wiring: a callback that `print`s a
//! marker is fired manually (through `:lua`), and the marker on the message line
//! is the observable assertion. (Editor-emitted events arrive in Phases 2–3.)
//!
//! Integration-test files don't share a module, so the `start*/feed/...` helpers
//! here are copied from the `editing.rs` pattern rather than imported.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
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
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");
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
    };
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

/// Drain every `redraw` currently queued in `incoming` and return the *most
/// recent* one (skipping non-redraw notifications), or `None` when none is
/// buffered. Redraws are full-state projections, so the latest reflects the
/// freshest editor state.
fn drain_to_latest_redraw(
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    let mut latest = None;
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => latest = Some(map),
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue,
            Err(_) => return latest,
        }
    }
}

/// Feed `keys`, then return the `redraw` map the server emitted for that input.
///
/// The server processes messages serially, writing each message's response then
/// its `redraw`; we send `nvim_input` then a `nvim_get_mode` barrier, so once the
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
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    if let Some(map) = drain_to_latest_redraw(incoming) {
        return map;
    }
    // The barrier guarantees the input's redraw is queued before its response, so
    // the drain above should have found it. Under heavy load the reader task can
    // lag; poll a bounded while rather than failing on the first miss.
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if let Some(map) = drain_to_latest_redraw(incoming) {
            return map;
        }
    }
    panic!("no redraw arrived for {keys:?}");
}

fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The message line from a redraw.
fn message(map: &[(Value, Value)]) -> String {
    field(map, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Feed a `:lua <chunk><CR>` line and return the resulting message line — the
/// channel a fired callback's `print` lands on.
async fn lua_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, chunk: &str) -> String {
    message(&redraw_after(rpc, incoming, &format!(":lua {chunk}<CR>")).await)
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// `nvim_exec_lua(code)` -> its return value (a synchronous Lua getter).
async fn exec_lua(rpc: &Rpc, code: &str) -> Value {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .expect("nvim_exec_lua")
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
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "local a = vim.api.nvim_get_autocmds({}) \
         print(#a .. ':' .. a[1].event)",
    )
    .await;
    assert_eq!(msg, "1:User");
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
        "vim._set_cur_buf(4, '/tmp/foo/bar.rs') \
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

#[tokio::test]
async fn nvim_open_win_creates_a_window_and_fires_winnew() {
    // The programmatic split form of nvim_open_win: open a fresh buffer in a new
    // window. WinNew fires for the new window, and it ends up in vim's window list.
    let dir = temp_dir("au_open_win");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.new = {}\n\
         vim.api.nvim_create_autocmd('WinNew', {\n\
         \x20 callback = function(a) _G.new[#_G.new+1] = a.match end })\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.new = {}").await;
    // Open the current buffer (handle 0) in a new split window.
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "_G.win = vim.api.nvim_open_win(0, true, { vertical = false }); \
         print(#vim.api.nvim_list_wins() .. ',' .. tostring(_G.win))",
    )
    .await;
    // Two windows now; the returned handle is the new window's id.
    let parts: Vec<&str> = msg.split(',').collect();
    assert_eq!(parts[0], "2", "open_win added a second window: {msg}");
    // WinNew fired for the new window (its id matches the returned handle).
    let fired = lua_message(&rpc, &mut incoming, "print(table.concat(_G.new, ','))").await;
    assert_eq!(fired, parts[1], "WinNew fired for the opened window {msg}");
}

#[tokio::test]
async fn opening_and_closing_a_float_fires_window_autocmds() {
    // A float is a window, so the lifecycle diff spans it: opening+entering fires
    // WinNew then WinEnter for the float; closing it fires WinClosed.
    let dir = temp_dir("au_float_life");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinNew', { callback = rec('new') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n\
         vim.api.nvim_create_autocmd('WinClosed', { callback = rec('closed') })\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // Open+enter the float (fires WinNew/WinEnter into _G.log). Read its id with a
    // synchronous getter — `print(_G.f)` in the open chunk is wiped by the float's
    // own WinEnter (which clears the message line).
    lua_message(
        &rpc,
        &mut incoming,
        "_G.f = vim.api.nvim_open_win(0, true, \
         { relative='editor', row=1, col=1, width=10, height=4 })",
    )
    .await;
    let id = exec_lua(&rpc, "return _G.f")
        .await
        .as_u64()
        .expect("float id");
    let opened = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(
        opened,
        format!("new{id},enter{id}"),
        "opening+entering a float fired WinNew then WinEnter"
    );

    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    lua_message(&rpc, &mut incoming, "vim.api.nvim_win_close(_G.f, false)").await;
    let closed = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    let want = format!("closed{id}");
    assert!(
        closed.contains(&want),
        "closing the float fired WinClosed: {closed}"
    );
}

#[tokio::test]
async fn resizing_a_float_with_set_config_fires_winresized() {
    // `nvim_win_set_config` that changes a float's size makes its rect change,
    // which the lifecycle diff reports as WinResized.
    let dir = temp_dir("au_float_resize");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.resized = {}\n\
         vim.api.nvim_create_autocmd('WinResized', {\n\
         \x20 callback = function(a) _G.resized[#_G.resized+1] = a.match end })\n",
    )
    .await;
    lua_message(
        &rpc,
        &mut incoming,
        "_G.f = vim.api.nvim_open_win(0, true, \
         { relative='editor', row=1, col=1, width=10, height=4 })",
    )
    .await;
    let id = exec_lua(&rpc, "return _G.f")
        .await
        .as_u64()
        .expect("float id");
    let id = id.to_string();
    // Clear any layout churn from the open before measuring the resize.
    lua_message(&rpc, &mut incoming, "_G.resized = {}").await;
    lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_win_set_config(_G.f, \
         { relative='editor', row=1, col=1, width=20, height=8 })",
    )
    .await;
    let fired = lua_message(&rpc, &mut incoming, "print(table.concat(_G.resized, ','))").await;
    assert!(
        fired.contains(id.as_str()),
        "resizing the float fired WinResized for it: {fired}"
    );
}

#[tokio::test]
async fn examples_windows_config_loads_and_drives_the_window_api() {
    // End-to-end check of the shipped `examples/windows/` config: source its real
    // init.lua, then exercise the helper commands it defines. Proves the example
    // is runnable (no Lua errors) and that its window API + autocmds work.
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/windows/init.lua");
    let init = std::fs::read_to_string(&example).expect("read examples/windows/init.lua");
    let dir = temp_dir("examples_windows");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    // The config defines :WinDemo (opens a split via nvim_open_win) and :WinList.
    redraw_after(&rpc, &mut incoming, ":WinDemo<CR>").await;
    let count = exec_lua(&rpc, r#"return #vim.api.nvim_list_wins()"#).await;
    assert_eq!(count.as_u64(), Some(2), ":WinDemo opened a second window");

    // The window autocmds recorded the lifecycle (at least the WinNew/WinEnter the
    // split produced).
    let logged = exec_lua(&rpc, r#"return #_G.win_log"#).await;
    assert!(
        logged.as_u64().unwrap_or(0) >= 1,
        "window events were recorded"
    );
}
