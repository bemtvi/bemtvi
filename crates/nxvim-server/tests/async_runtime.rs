//! Behavior tests for nxvim's async Lua runtime (the event loop):
//! `vim.schedule`, `vim.defer_fn`, and async `vim.system`. Black-box per the
//! project conventions — a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state.
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

// ----- nx.utils.debounce (trailing-edge debounce over nx.timer) --------------------

#[tokio::test]
async fn debounce_collapses_a_burst_to_one_trailing_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.last = nil\n\
         _G.d = nx.utils.debounce(function(x) _G.n = _G.n + 1; _G.last = x end, 40)\n\
         _G.d('a'); _G.d('b'); _G.d('c')",
    )
    .await;
    // Immediate: nothing ran inline — the burst is still pending.
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(0));
    // Past the delay: exactly one trailing call, with the MOST RECENT arguments.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
    assert_eq!(exec_lua(&rpc, "return _G.last").await.as_str(), Some("c"));
}

#[tokio::test]
async fn debounce_cancel_drops_the_pending_call() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.d = nx.utils.debounce(function() _G.n = _G.n + 1 end, 40)\n\
         _G.d(); _G.d:cancel()",
    )
    .await;
    // The pending call was cancelled — it never fires.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(0));
}

#[tokio::test]
async fn debounce_flush_runs_the_pending_call_now() {
    let (rpc, _incoming) = start().await;
    // A long delay so only :flush() can make it fire promptly.
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.last = nil\n\
         _G.d = nx.utils.debounce(function(x) _G.n = _G.n + 1; _G.last = x end, 5000)\n\
         _G.d('z'); _G.d:flush()",
    )
    .await;
    // flush is synchronous: the call already ran, with its captured arguments.
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
    assert_eq!(exec_lua(&rpc, "return _G.last").await.as_str(), Some("z"));
    // And flushing consumed the pending timer — no second, delayed fire.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(lua_u64(&rpc, "return _G.n").await, Some(1));
}

// ----- Phase 3: async vim.system ---------------------------------------------

// ----- Phase 4: robustness (leaks, schedule_wrap) ----------------------------

#[tokio::test]
async fn one_shot_callbacks_do_not_leak_the_registry() {
    let (rpc, _incoming) = start().await;
    // A long run of one-shot schedules must leave nx._cb_fns empty (each is
    // dropped after firing).
    exec_lua(&rpc, "for _ = 1, 64 do vim.schedule(function() end) end").await;
    let remaining = lua_u64(
        &rpc,
        "local n = 0; for _ in pairs(nx._cb_fns) do n = n + 1 end; return n",
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
