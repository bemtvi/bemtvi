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
