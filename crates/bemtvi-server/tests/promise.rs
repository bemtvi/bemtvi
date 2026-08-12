//! Behavior tests for `btv.promise` — the Promises/A+ surface shaped like the
//! browser's — and its `btv.async`/`btv.await` coroutine sugar. Black-box per the
//! project conventions: a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state.
//!
//! Promise reactions run as MICROTASKS (`btv.schedule`), so they settle at the end
//! of the tick that scheduled them — within the same convergence the server runs
//! after each `nvim_exec_lua`. The pattern below therefore mirrors the
//! `vim.schedule` tests: set up a chain in one chunk writing its outcome to a
//! global, then read that global back in a second chunk (by which point every
//! microtask generation has flushed). Off-tick behaviour (`btv.promise.delay`) uses
//! the two-barrier pattern with a real sleep.

use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, lua_bool, lua_u64, start_attached};
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
         btv.promise.resolve(41):next(function(v) _G.got = v + 1 end)",
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
         btv.promise.reject('boom'):catch(function(e) _G.err = e end)",
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
         btv.promise.resolve(1)\n\
           :next(function(v) return v + 1 end)\n\
           :next(function(v) return btv.promise.resolve(v * 10) end)\n\
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
         btv.promise.resolve(1)\n\
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
         btv.promise.resolve(7)\n\
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
         btv.promise.resolve(1):next(function() table.insert(_G.order, 'reaction') end)\n\
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
         btv.promise.all({ btv.promise.resolve(1), btv.promise.resolve(2), 3 })\n\
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
         btv.promise.all({ btv.promise.resolve(1), btv.promise.reject('nope') })\n\
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
         btv.promise.all_settled({ btv.promise.resolve(9), btv.promise.reject('x') })\n\
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
         btv.promise.any({ btv.promise.reject('a'), btv.promise.resolve(5) })\n\
           :next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(5));
}

// ----- btv.async / btv.await ---------------------------------------------------

#[tokio::test]
async fn async_await_reads_top_to_bottom() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         local run = btv.async(function(start)\n\
           local a = btv.await(btv.promise.resolve(start))\n\
           local b = btv.await(btv.promise.resolve(a + 10))\n\
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
    // promise btv.async returned — caught by :catch on the result.
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         local run = btv.async(function()\n\
           btv.await(btv.promise.reject('deep'))\n\
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
        "local ok, err = pcall(function() return btv.await(btv.promise.resolve(1)) end)\n\
         return (not ok) and string.find(err, 'btv.await') ~= nil",
    )
    .await;
    assert_eq!(ok, Some(true));
}

// ----- off-tick: btv.promise.delay --------------------------------------------

#[tokio::test]
async fn delay_fulfils_after_the_wall_clock_wait() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.done = false\n\
         btv.promise.delay(40, 'v'):next(function(v) _G.done = (v == 'v') end)",
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
           btv.promise.resolve(1):next(function(v) return v end):catch(function() end)\n\
         end",
    )
    .await;
    assert_eq!(
        lua_u64(
            &rpc,
            "local n = 0; for _ in pairs(btv._cb_fns) do n = n + 1 end; return n"
        )
        .await,
        Some(0)
    );
}

// ----- btv.promise.try (fold a sync throw + async result into one chain) -------

#[tokio::test]
async fn try_runs_fn_and_resolves_with_its_value() {
    let (rpc, _incoming) = start().await;
    // A plain return value flows through like btv.promise.resolve(value).
    exec_lua(
        &rpc,
        "_G.got = nil\n\
         btv.promise.try(function(a, b) return a + b end, 2, 3):next(function(v) _G.got = v end)",
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
         btv.promise.try(function() error('boom') end):catch(function(e) _G.err = tostring(e) end)",
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
         btv.promise.try(function() return btv.promise.resolve(99) end):next(function(v) _G.got = v end)",
    )
    .await;
    assert_eq!(lua_u64(&rpc, "return _G.got").await, Some(99));
}

// ----- btv.on_next_tick / btv.wait_for: cross-tick deferral --------------------

// btv.on_next_tick defers to a LATER tick — not inline, not the same convergence.
#[tokio::test]
async fn on_next_tick_defers_to_a_later_tick() {
    let (rpc, _incoming) = start().await;
    // Arming it does NOT run it inline: `inline` captured right after is still false.
    exec_lua(
        &rpc,
        "_G.ran = false\n\
         btv.on_next_tick(function() _G.ran = true end)\n\
         _G.inline = _G.ran",
    )
    .await;
    assert_eq!(
        lua_bool(&rpc, "return _G.inline").await,
        Some(false),
        "on_next_tick must not run its fn inline"
    );
    // It fires on a later loop turn (within a short wall-clock window).
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        lua_bool(&rpc, "return _G.ran").await,
        Some(true),
        "on_next_tick fn should have run on a later tick"
    );
}

// btv.wait_for polls a predicate across ticks and fulfils with its truthy value.
#[tokio::test]
async fn wait_for_polls_across_ticks_and_resolves_with_the_value() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.n = 0\n\
         _G.got = nil\n\
         btv.wait_for(function()\n\
           _G.n = _G.n + 1\n\
           if _G.n >= 3 then return 'ready' end\n\
         end):next(function(v) _G.got = v end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        lua_string(&rpc, "return _G.got").await,
        Some("ready".to_string()),
        "wait_for resolves with the predicate's truthy value once it holds"
    );
    assert_eq!(
        lua_bool(&rpc, "return _G.n >= 3").await,
        Some(true),
        "the predicate is polled across several ticks"
    );
}

// btv.wait_for rejects (with the given message) when the condition never holds within
// the bounded `tries`.
#[tokio::test]
async fn wait_for_rejects_after_bounded_tries() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         btv.wait_for(function() return false end, { tries = 3, message = 'nope' })\n\
           :catch(function(e) _G.err = tostring(e and e.message or e) end)",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        lua_string(&rpc, "return _G.err").await,
        Some("nope".to_string()),
        "an exhausted wait_for rejects with the configured message"
    );
}
