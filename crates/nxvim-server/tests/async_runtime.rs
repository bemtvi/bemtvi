//! Behavior tests for nxvim's async Lua runtime (the event loop):
//! `vim.schedule`, `vim.defer_fn`, `vim.uv`/`vim.fn` timers, and async
//! `vim.system`. Black-box per the project conventions — a real server over RPC,
//! driven with `nvim_exec_lua`, asserting on observable Lua state.
//!
//! Two observation patterns (see docs/plans/2026-06-06-async-lua-runtime.md → testing
//! appendix):
//!   * deferred-within-a-tick (`vim.schedule`) — assert on *ordering*, since the
//!     effect lands at convergence in the same handler;
//!   * off-tick (timers / async `vim.system`) — the *two-barrier* pattern: a
//!     barrier right after the trigger sees the effect ABSENT (it didn't run
//!     inline), then after a real sleep a second barrier sees it PRESENT (the
//!     actor fired it off-tick and the server settled).

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, lua_u64, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread (its runtime has timers enabled, so the
/// event-loop actor can sleep) and return a connected, UI-attached client.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

// ----- Phase 1: vim.schedule (deferred within the tick) ----------------------

#[tokio::test]
async fn schedule_runs_after_direct_work_not_inline() {
    let (rpc, _incoming) = start().await;
    // The scheduled callback must run AFTER the direct work that follows it in the
    // chunk — proof it deferred to convergence rather than running nested inline.
    exec_lua(
        &rpc,
        "_G.order = {}\n\
         vim.schedule(function() table.insert(_G.order, 'scheduled') end)\n\
         table.insert(_G.order, 'direct')",
    )
    .await;
    let order = exec_lua(&rpc, "return table.concat(_G.order, ',')").await;
    assert_eq!(order.as_str(), Some("direct,scheduled"));
}

#[tokio::test]
async fn a_scheduled_callback_can_schedule_more_work() {
    let (rpc, _incoming) = start().await;
    // A callback that schedules another must see both run — proof the fixpoint
    // picks up work queued mid-drain.
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         vim.schedule(function()\n\
           vim.schedule(function() _G.n = _G.n + 1 end)\n\
           _G.n = _G.n + 1\n\
         end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(2));
}

#[tokio::test]
async fn a_throwing_scheduled_callback_does_not_stop_the_next() {
    let (rpc, _incoming) = start().await;
    // The first scheduled callback throws; the second must still run (error
    // isolation — the drain catches and echoes E5108, never aborts).
    exec_lua(
        &rpc,
        "_G.ran = false\n\
         vim.schedule(function() error('boom') end)\n\
         vim.schedule(function() _G.ran = true end)",
    )
    .await;
    assert_eq!(lua_bool(&rpc, "return _G.ran").await, Some(true));
}

// ----- Phase 2: timers (off the input tick) ----------------------------------

#[tokio::test]
async fn defer_fn_fires_after_the_delay_not_before() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.fired = false\n\
         vim.defer_fn(function() _G.fired = true end, 40)",
    )
    .await;
    // Barrier #1, immediate: the deferred fn has NOT run (it didn't run inline).
    assert_eq!(lua_bool(&rpc, "return _G.fired").await, Some(false));
    // Past the delay: the actor fired it off-tick and the server settled.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_bool(&rpc, "return _G.fired").await, Some(true));
}

#[tokio::test]
async fn a_repeating_uv_timer_fires_repeatedly_and_stops() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.count = 0\n\
         _G.t = vim.uv.new_timer()\n\
         _G.t:start(20, 20, function() _G.count = _G.count + 1 end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(140)).await;
    let fired = lua_u64(&rpc, "return _G.count").await.unwrap();
    assert!(
        fired >= 2,
        "repeating timer should fire repeatedly, got {fired}"
    );

    // Stop it; the count must stop growing.
    exec_lua(&rpc, "_G.t:stop()").await;
    let at_stop = lua_u64(&rpc, "return _G.count").await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let after = lua_u64(&rpc, "return _G.count").await.unwrap();
    assert_eq!(after, at_stop, "a stopped timer must not fire again");
}

#[tokio::test]
async fn a_throwing_timer_callback_does_not_wedge_the_loop() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.ok = false\n\
         vim.defer_fn(function() error('boom') end, 20)\n\
         vim.defer_fn(function() _G.ok = true end, 50)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(lua_bool(&rpc, "return _G.ok").await, Some(true));
}

// ----- Phase 3: async vim.system ---------------------------------------------

#[tokio::test]
async fn vim_system_with_on_exit_runs_async() {
    let (rpc, _incoming) = start().await;
    // A child that sleeps briefly, so barrier #1 reliably observes "not done yet".
    exec_lua(
        &rpc,
        "_G.code = nil\n\
         vim.system({ 'sh', '-c', 'sleep 0.1' }, {}, function(r) _G.code = r.code end)",
    )
    .await;
    // Barrier #1: on_exit has not fired (it didn't run synchronously inline).
    assert_eq!(lua_bool(&rpc, "return _G.code ~= nil").await, Some(false));
    // Past the child's lifetime: on_exit fired off-tick with the result.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(lua_u64(&rpc, "return _G.code").await, Some(0));
}

#[tokio::test]
async fn vim_system_wait_without_on_exit_is_synchronous() {
    let (rpc, _incoming) = start().await;
    // The blocking `:wait()` branch the config `root_dir` path relies on: a
    // complete result returned synchronously, no event loop involved.
    let out = exec_lua(
        &rpc,
        "local r = vim.system({ 'sh', '-c', 'printf hello' }):wait()\n\
         return r.stdout",
    )
    .await;
    assert_eq!(out.as_str(), Some("hello"));
    let code = lua_u64(
        &rpc,
        "return vim.system({ 'sh', '-c', 'exit 3' }):wait().code",
    )
    .await;
    assert_eq!(code, Some(3));
}

// ----- Phase 4: robustness (leaks, schedule_wrap) ----------------------------

#[tokio::test]
async fn one_shot_callbacks_do_not_leak_the_registry() {
    let (rpc, _incoming) = start().await;
    // A long run of one-shot schedules must leave vim._cb_fns empty (each is
    // dropped after firing).
    exec_lua(&rpc, "for _ = 1, 64 do vim.schedule(function() end) end").await;
    let remaining = lua_u64(
        &rpc,
        "local n = 0; for _ in pairs(vim._cb_fns) do n = n + 1 end; return n",
    )
    .await;
    assert_eq!(remaining, Some(0), "scheduled one-shots must not leak");
}

#[tokio::test]
async fn schedule_wrap_defers_its_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         local w = vim.schedule_wrap(function(x) _G.got = x end)\n\
         w(42)\n\
         _G.during = _G.got",
    )
    .await;
    // During the chunk the wrapped call had not run yet (it scheduled); by the
    // time we read back it has (at convergence).
    assert_eq!(lua_bool(&rpc, "return _G.during == nil").await, Some(true));
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(42));
}

// ----- Phase 4: async vim.uv filesystem (the callback form) ------------------
// `vim.uv.fs_*` is dual-mode: synchronous without a trailing callback, async
// with one (returns immediately; `cb(err, value)` fires on a later loop
// iteration). The async form rides `vim.schedule`, so the callback lands at
// convergence — off the calling frame — and chained ops (open → write → close)
// all settle within one fixpoint. (Wired in nxvim-lua's prelude/uv.lua over the
// sync primitives in src/uvfs.rs.)

/// A unique temp path per test, forward-slashed for pasting into Lua. Removed
/// first so a prior run can't mask a write that didn't happen.
fn tmp_path(name: &str) -> String {
    let p = std::env::temp_dir().join(format!("nxvim-async-uvfs-{name}.txt"));
    let _ = std::fs::remove_file(&p);
    p.to_string_lossy().replace('\\', "/")
}

#[tokio::test]
async fn async_fs_write_chain_defers_the_callback_and_persists() {
    let (rpc, _incoming) = start().await;
    let path = tmp_path("write-chain");
    // Kick off open → write → close, all in the async (callback) form. Track
    // ordering against a direct insert: the callback must run AFTER 'direct',
    // proving it deferred rather than running nested inline.
    exec_lua(
        &rpc,
        &format!(
            "_G.order = {{}}\n\
             _G.done = false\n\
             local uv = vim.uv\n\
             uv.fs_open('{path}', 'w', tonumber('644', 8), function(oerr, fd)\n\
               table.insert(_G.order, 'callback')\n\
               assert(not oerr, tostring(oerr))\n\
               uv.fs_write(fd, 'async bytes\\n', -1, function(werr)\n\
                 assert(not werr, tostring(werr))\n\
                 uv.fs_close(fd, function() _G.done = true end)\n\
               end)\n\
             end)\n\
             table.insert(_G.order, 'direct')"
        ),
    )
    .await;
    // Deferred (not inline), and the whole chain settled within the fixpoint.
    assert_eq!(
        exec_lua(&rpc, "return table.concat(_G.order, ',')")
            .await
            .as_str(),
        Some("direct,callback")
    );
    assert_eq!(lua_bool(&rpc, "return _G.done").await, Some(true));
    // The bytes actually reached disk through the async path.
    let written = std::fs::read_to_string(path.replace('/', std::path::MAIN_SEPARATOR_STR))
        .expect("file written by async chain");
    assert_eq!(written, "async bytes\n");
}

#[tokio::test]
async fn async_fs_read_chain_delivers_data_off_the_calling_frame() {
    let (rpc, _incoming) = start().await;
    let path = tmp_path("read-chain");
    std::fs::write(
        path.replace('/', std::path::MAIN_SEPARATOR_STR),
        "from disk\n",
    )
    .expect("seed file");
    // The plenary.path:_read_async shape: open → fstat → read → close, each step
    // nested in the previous callback. The data must arrive in the callback, not
    // inline.
    exec_lua(
        &rpc,
        &format!(
            "_G.data = nil\n\
             _G.inline = '<unset>'\n\
             local uv = vim.uv\n\
             uv.fs_open('{path}', 'r', tonumber('644', 8), function(oerr, fd)\n\
               assert(not oerr, tostring(oerr))\n\
               uv.fs_fstat(fd, function(serr, st)\n\
                 assert(not serr, tostring(serr))\n\
                 uv.fs_read(fd, st.size, 0, function(rerr, chunk)\n\
                   assert(not rerr, tostring(rerr))\n\
                   _G.data = chunk\n\
                   uv.fs_close(fd, function() end)\n\
                 end)\n\
               end)\n\
             end)\n\
             _G.inline = _G.data"
        ),
    )
    .await;
    // It had NOT arrived inline (the open call returned before the callback ran)…
    assert_eq!(
        lua_bool(&rpc, "return _G.inline == nil").await,
        Some(true),
        "data must not be delivered inline"
    );
    // …and it DID arrive in the callback, with the right bytes.
    assert_eq!(
        exec_lua(&rpc, "return _G.data").await.as_str(),
        Some("from disk\n")
    );
}

#[tokio::test]
async fn async_fs_open_error_reaches_the_callback_not_a_raise() {
    let (rpc, _incoming) = start().await;
    // Opening a missing file for read fails. In the async form the failure must
    // be delivered to the callback as `err` (a string), never raised — so the
    // exec_lua settles cleanly and the callback observes (err, nil).
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         _G.fd = '<unset>'\n\
         vim.uv.fs_open('/no/such/nxvim/path.txt', 'r', tonumber('644', 8), function(err, fd)\n\
           _G.err = err\n\
           _G.fd = fd\n\
         end)",
    )
    .await;
    assert_eq!(
        lua_bool(&rpc, "return type(_G.err) == 'string'").await,
        Some(true),
        "the open error should reach the callback as a string"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.fd == nil").await,
        Some(true),
        "a failed open delivers no fd"
    );
}

// ----- Phase 5: luv loop-timer function-forms + vim.uv.fs_event --------------
// The function-form luv timer API (`vim.loop.timer_start(handle, …)`, what
// lualine's statusline refresh uses) and the `vim.uv.fs_event` filesystem watcher
// (lualine watches `.git/HEAD`). Both ride the same event-loop actor; fs_event is
// poll-backed there (the strategy luv's own new_fs_poll uses).

#[tokio::test]
async fn loop_timer_function_forms_fire_and_stop() {
    let (rpc, _incoming) = start().await;
    // vim.loop.timer_start(handle, timeout, repeat, cb) — the function form (handle
    // as first arg), not handle:start(...). Same table as vim.uv, so this is the
    // exact shape lualine calls.
    exec_lua(
        &rpc,
        "_G.count = 0\n\
         _G.t = vim.loop.new_timer()\n\
         vim.loop.timer_start(_G.t, 20, 20, function() _G.count = _G.count + 1 end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(140)).await;
    let fired = lua_u64(&rpc, "return _G.count").await.unwrap();
    assert!(
        fired >= 2,
        "function-form timer_start should fire repeatedly, got {fired}"
    );
    // vim.loop.timer_stop(handle) must halt it (the count stops growing).
    exec_lua(&rpc, "vim.loop.timer_stop(_G.t)").await;
    let at_stop = lua_u64(&rpc, "return _G.count").await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        lua_u64(&rpc, "return _G.count").await.unwrap(),
        at_stop,
        "function-form timer_stop must halt the timer"
    );
}

#[tokio::test]
async fn fs_event_fires_on_change_then_stop_silences_it() {
    let (rpc, _incoming) = start().await;
    let path = tmp_path("fsevent");
    let disk = path.replace('/', std::path::MAIN_SEPARATOR_STR);
    std::fs::write(&disk, "one\n").expect("seed the watched file");
    let basename = std::path::Path::new(&disk)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    // Watch the file: the callback records how many times it fired, the filename it
    // was handed, and the event flags. lualine wraps this in schedule_wrap; here we
    // record directly so the assertion is on the watcher itself.
    exec_lua(
        &rpc,
        &format!(
            "_G.fired = 0\n\
             _G.name = nil\n\
             _G.change = nil\n\
             _G.ev = vim.uv.new_fs_event()\n\
             _G.ev:start('{path}', {{}}, function(err, filename, events)\n\
               assert(not err, tostring(err))\n\
               _G.fired = _G.fired + 1\n\
               _G.name = filename\n\
               _G.change = events.change\n\
             end)"
        ),
    )
    .await;
    // Let the watcher capture its baseline (one poll interval), then confirm it has
    // NOT fired — nothing changed yet (it didn't fire spuriously on :start).
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_u64(&rpc, "return _G.fired").await, Some(0));

    // Modify the watched file (different length, so the stat signature moves even
    // at coarse mtime resolution); the poll detects it and fires off-tick.
    std::fs::write(&disk, "two two two\n").expect("modify the watched file");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let fired = lua_u64(&rpc, "return _G.fired").await.unwrap();
    assert!(fired >= 1, "fs_event should fire on change, got {fired}");
    // An in-place edit is a `change`, and the callback gets the file's basename.
    assert_eq!(
        lua_bool(&rpc, "return _G.change == true").await,
        Some(true),
        "an edit is reported as a change event"
    );
    assert_eq!(
        lua_bool(&rpc, &format!("return _G.name == '{basename}'")).await,
        Some(true),
        "the callback is handed the watched entry's filename"
    );

    // Stop the watch; further changes must not fire it.
    exec_lua(&rpc, "_G.ev:stop()").await;
    let at_stop = lua_u64(&rpc, "return _G.fired").await.unwrap();
    std::fs::write(&disk, "three three three three\n").expect("modify after stop");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        lua_u64(&rpc, "return _G.fired").await.unwrap(),
        at_stop,
        "a stopped fs_event watch must not fire again"
    );
}
