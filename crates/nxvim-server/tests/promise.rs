//! Behavior tests for `nx.promise` — the Promises/A+ surface shaped like the
//! browser's — and its `nx.async`/`nx.await` coroutine sugar. Black-box per the
//! project conventions: a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state.
//!
//! Promise reactions run as MICROTASKS (`nx.schedule`), so they settle at the end
//! of the tick that scheduled them — within the same convergence the server runs
//! after each `nvim_exec_lua`. The pattern below therefore mirrors the
//! `vim.schedule` tests: set up a chain in one chunk writing its outcome to a
//! global, then read that global back in a second chunk (by which point every
//! microtask generation has flushed). Off-tick behaviour (`nx.promise.delay`) uses
//! the two-barrier pattern with a real sleep.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, lua_u64, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Read a `return`-style chunk as an owned `String` (`None` if not a string).
async fn lua_string(rpc: &Rpc, code: &str) -> Option<String> {
    exec_lua(rpc, code).await.as_str().map(str::to_owned)
}

// ----- core: resolve / reject / chaining -------------------------------------

#[tokio::test]
async fn resolved_value_flows_to_next() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         nx.promise.resolve(41):next(function(v) _G.got = v + 1 end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(42));
}

#[tokio::test]
async fn rejection_is_caught() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         nx.promise.reject('boom'):catch(function(e) _G.err = e end)",
    )
    .await;
    assert_eq!(
        lua_string(&rpc, "return _G.err").await.as_deref(),
        Some("boom")
    );
}

#[tokio::test]
async fn next_chains_transform_the_value() {
    let (rpc, _incoming) = start().await;
    // Each :next returns a new promise resolved with the handler's return; a
    // returned promise is adopted, not nested.
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         nx.promise.resolve(1)\n\
           :next(function(v) return v + 1 end)\n\
           :next(function(v) return nx.promise.resolve(v * 10) end)\n\
           :next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(20));
}

#[tokio::test]
async fn a_throw_midchain_is_caught_at_the_end() {
    let (rpc, _incoming) = start().await;
    // A throw anywhere in the chain skips later :next handlers and lands in the
    // trailing :catch — the property that makes one terminal catch enough.
    exec_lua(
        &rpc,
        "_G.reached = false\n_G.err = nil\n\
         nx.promise.resolve(1)\n\
           :next(function() error('mid') end)\n\
           :next(function() _G.reached = true end)\n\
           :catch(function(e) _G.err = e end)",
    )
    .await;
    assert_eq!(lua_bool(&rpc, "return _G.reached").await, Some(false));
    assert_eq!(
        lua_bool(
            &rpc,
            "return _G.err ~= nil and string.find(_G.err, 'mid') ~= nil"
        )
        .await,
        Some(true)
    );
}

#[tokio::test]
async fn finally_runs_and_passes_the_value_through() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.ran = false\n_G.got = nil\n\
         nx.promise.resolve(7)\n\
           :finally(function() _G.ran = true end)\n\
           :next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_bool(&rpc, "return _G.ran").await, Some(true));
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(7));
}

#[tokio::test]
async fn reactions_are_async_never_inline() {
    let (rpc, _incoming) = start().await;
    // Even an already-resolved promise runs its :next on a later microtask, so
    // 'direct' (after the :next call) is recorded before 'reaction'.
    exec_lua(
        &rpc,
        "_G.order = {}\n\
         nx.promise.resolve(1):next(function() table.insert(_G.order, 'reaction') end)\n\
         table.insert(_G.order, 'direct')",
    )
    .await;
    assert_eq!(
        lua_string(&rpc, "return table.concat(_G.order, ',')")
            .await
            .as_deref(),
        Some("direct,reaction")
    );
}

// ----- combinators -----------------------------------------------------------

#[tokio::test]
async fn all_collects_values_in_order() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.sum = nil\n\
         nx.promise.all({ nx.promise.resolve(1), nx.promise.resolve(2), 3 })\n\
           :next(function(vs) _G.sum = vs[1] * 100 + vs[2] * 10 + vs[3] end)",
    )
    .await;
    // Plain (non-promise) values pass straight through, in input order.
    assert_eq!(lua_u64(&rpc, "return _G.sum").await, Some(123));
}

#[tokio::test]
async fn all_rejects_on_first_rejection() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         nx.promise.all({ nx.promise.resolve(1), nx.promise.reject('nope') })\n\
           :catch(function(e) _G.err = e end)",
    )
    .await;
    assert_eq!(
        lua_string(&rpc, "return _G.err").await.as_deref(),
        Some("nope")
    );
}

#[tokio::test]
async fn all_settled_reports_each_outcome() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.report = nil\n\
         nx.promise.all_settled({ nx.promise.resolve(9), nx.promise.reject('x') })\n\
           :next(function(rs)\n\
             _G.report = rs[1].status .. ':' .. tostring(rs[1].value)\n\
               .. '|' .. rs[2].status .. ':' .. tostring(rs[2].reason)\n\
           end)",
    )
    .await;
    assert_eq!(
        lua_string(&rpc, "return _G.report").await.as_deref(),
        Some("fulfilled:9|rejected:x")
    );
}

#[tokio::test]
async fn any_takes_the_first_fulfilment() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         nx.promise.any({ nx.promise.reject('a'), nx.promise.resolve(5) })\n\
           :next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(5));
}

// ----- nx.async / nx.await ---------------------------------------------------

#[tokio::test]
async fn async_await_reads_top_to_bottom() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         local run = nx.async(function(start)\n\
           local a = nx.await(nx.promise.resolve(start))\n\
           local b = nx.await(nx.promise.resolve(a + 10))\n\
           return a + b\n\
         end)\n\
         run(1):next(function(v) _G.got = v end)",
    )
    .await;
    // a = 1, b = 11, result 12.
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(12));
}

#[tokio::test]
async fn await_of_a_rejection_rejects_the_async_result() {
    let (rpc, _incoming) = start().await;
    // A rejected await re-raises inside the coroutine; uncaught, it rejects the
    // promise nx.async returned — caught by :catch on the result.
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         local run = nx.async(function()\n\
           nx.await(nx.promise.reject('deep'))\n\
           return 'unreached'\n\
         end)\n\
         run():catch(function(e) _G.err = e end)",
    )
    .await;
    assert_eq!(
        lua_bool(
            &rpc,
            "return _G.err ~= nil and string.find(_G.err, 'deep') ~= nil"
        )
        .await,
        Some(true)
    );
}

#[tokio::test]
async fn await_outside_async_fails_loud() {
    let (rpc, _incoming) = start().await;
    // Per the no-silent-stubs rule: awaiting off a coroutine raises a named error.
    let ok = lua_bool(
        &rpc,
        "local ok, err = pcall(function() return nx.await(nx.promise.resolve(1)) end)\n\
         return (not ok) and string.find(err, 'nx.await') ~= nil",
    )
    .await;
    assert_eq!(ok, Some(true));
}

// ----- off-tick: nx.promise.delay --------------------------------------------

#[tokio::test]
async fn delay_fulfils_after_the_wall_clock_wait() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.done = false\n\
         nx.promise.delay(40, 'v'):next(function(v) _G.done = (v == 'v') end)",
    )
    .await;
    // Barrier #1: not yet — delay rides the loop, off the input tick.
    assert_eq!(lua_bool(&rpc, "return _G.done").await, Some(false));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(lua_bool(&rpc, "return _G.done").await, Some(true));
}

// ----- registry hygiene ------------------------------------------------------

#[tokio::test]
async fn settled_promise_chains_do_not_leak_callbacks() {
    let (rpc, _incoming) = start().await;
    // A burst of fully-settled chains must leave the shared callback registry
    // empty — every microtask is a one-shot dropped after it fires.
    exec_lua(
        &rpc,
        "for _ = 1, 32 do\n\
           nx.promise.resolve(1):next(function(v) return v end):catch(function() end)\n\
         end",
    )
    .await;
    assert_eq!(
        lua_u64(
            &rpc,
            "local n = 0; for _ in pairs(nx._cb_fns) do n = n + 1 end; return n"
        )
        .await,
        Some(0)
    );
}

// ----- the shipped example must keep working ---------------------------------

#[tokio::test]
async fn the_promise_example_config_runs_end_to_end() {
    // Source examples/promise/init.lua exactly as `NXVIM_CONFIG` would and let its
    // timers (delay 200ms, two 80ms awaits) settle — proof the demo isn't stale.
    let example = include_str!("../../../examples/promise/init.lua");
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, example).await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    // Synchronous chains.
    assert_eq!(
        lua_u64(&rpc, "return _G.promise_demo.basic").await,
        Some(21)
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.promise_demo.caught ~= nil").await,
        Some(true)
    );
    // Off-tick: delay, all (collected in order), and the async/await result.
    assert_eq!(
        lua_string(&rpc, "return _G.promise_demo.delayed")
            .await
            .as_deref(),
        Some("woke up")
    );
    assert_eq!(
        lua_u64(
            &rpc,
            "local a = _G.promise_demo.all; return a[1] * 100 + a[2] * 10 + a[3]"
        )
        .await,
        Some(123)
    );
    assert_eq!(
        lua_u64(&rpc, "return _G.promise_demo.async").await,
        Some(15)
    );
}

// ----- nx.promise.try (fold a sync throw + async result into one chain) -------

#[tokio::test]
async fn try_runs_fn_and_resolves_with_its_value() {
    let (rpc, _incoming) = start().await;
    // A plain return value flows through like nx.promise.resolve(value).
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         nx.promise.try(function(a, b) return a + b end, 2, 3):next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(5));
}

#[tokio::test]
async fn try_turns_a_synchronous_throw_into_a_rejection() {
    let (rpc, _incoming) = start().await;
    // A function that errors SYNCHRONOUSLY (before returning) rejects the promise —
    // caught by the same :catch that an async rejection would land in.
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         nx.promise.try(function() error('boom') end):catch(function(e) _G.err = tostring(e) end)",
    )
    .await;
    assert_eq!(
        lua_bool(&rpc, "return _G.err ~= nil and _G.err:find('boom') ~= nil").await,
        Some(true),
        "a sync throw is caught as a rejection"
    );
}

#[tokio::test]
async fn try_adopts_a_returned_promise() {
    let (rpc, _incoming) = start().await;
    // When fn returns a promise, try adopts it (waits on it) rather than fulfilling
    // with the promise object — so one chain handles sync-throw and async alike.
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         nx.promise.try(function() return nx.promise.resolve(99) end):next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(99));
}
