//! `nvim_exec_lua` reports a failing chunk **to the caller**.
//!
//! A request has someone waiting on its answer, so a chunk that raises has to come back
//! as an RPC error — the way neovim answers it. Replying `Nil` instead made a raising
//! chunk indistinguishable from a chunk that returned nil, which is the exact shape
//! CLAUDE.md's no-silent-stubs rule exists to prevent: a typo'd chunk (in a config, a
//! plugin, or a test) looked like a feature that answered nil, and the caller carried on
//! with the wrong value. (The event-driven Lua entry points — keymaps, autocmds, LSP
//! hooks — keep echoing `E5108`: there is no caller there to hand an error to.)
//!
//! These drive the RPC directly rather than through the harness's `exec_lua`, which
//! unwraps the response — that unwrap is precisely what turns this into a loud test
//! failure everywhere else in the suite.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The raw request, so the error response can be inspected instead of unwrapped.
async fn raw(rpc: &Rpc, code: &str) -> Result<Value, String> {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn a_raising_chunk_answers_with_an_error_not_nil() {
    let (rpc, _incoming) = start().await;
    let err = raw(&rpc, "error('boom')")
        .await
        .expect_err("a chunk that raises must not answer with a value");
    assert!(
        err.contains("boom"),
        "the error must carry the Lua message, got {err:?}"
    );
    assert!(
        err.contains("E5108"),
        "…under the same E5108 code the other Lua entry points use, got {err:?}"
    );
}

#[tokio::test]
async fn a_syntax_error_answers_with_an_error_too() {
    let (rpc, _incoming) = start().await;
    let err = raw(&rpc, "this is not lua")
        .await
        .expect_err("a chunk that does not compile must not answer with a value");
    assert!(
        err.contains("E5108"),
        "a load failure is reported like a runtime one, got {err:?}"
    );
}

#[tokio::test]
async fn an_error_inside_a_btv_call_reaches_the_caller() {
    // The realistic shape: a real API called wrongly (`btv.on(event, opts, fn)` given a
    // pattern string where the options table goes). This raised inside the prelude and
    // answered `Nil`, so the caller saw "registered, and the handler never fires".
    let (rpc, _incoming) = start().await;
    let err = raw(&rpc, "btv.on('BufReadPost', '*', function() end)")
        .await
        .expect_err("a bad call into btv.* must reach the caller");
    assert!(
        err.contains("E5108"),
        "expected the Lua failure, got {err:?}"
    );
}

#[tokio::test]
async fn a_chunk_that_returns_nil_still_succeeds() {
    // The other half: nil is a perfectly good answer, and must stay one. Otherwise the
    // fix would turn every getter with nothing to report into an error.
    let (rpc, _incoming) = start().await;
    assert_eq!(
        raw(&rpc, "return nil").await,
        Ok(Value::Nil),
        "a chunk that returns nil is a success, not a failure"
    );
    assert_eq!(
        raw(&rpc, "local x = 1").await,
        Ok(Value::Nil),
        "…and so is one that returns nothing at all"
    );
}

#[tokio::test]
async fn a_pcall_still_swallows_its_own_error() {
    // A chunk that handles its own failure is not a failed chunk — the error has to come
    // from the chunk escaping, not from the word `error` appearing in it.
    let (rpc, _incoming) = start().await;
    assert_eq!(
        raw(
            &rpc,
            "local ok = pcall(function() error('inner') end) return ok"
        )
        .await,
        Ok(Value::Boolean(false)),
        "a caught error answers with the chunk's own value"
    );
}

#[tokio::test]
async fn the_connection_survives_a_failed_chunk() {
    // The error is an answer to one request, not a teardown: whatever the chunk managed
    // to do before it raised stands, and the next request is served normally.
    let (rpc, _incoming) = start().await;
    let _ = raw(&rpc, "btv.g.marker = 'set' error('boom')").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.g.marker").await.as_str(),
        Some("set"),
        "the work the chunk did before raising is not rolled back"
    );
    assert_eq!(
        exec_lua(&rpc, "return 1 + 1").await.as_i64(),
        Some(2),
        "and the connection still serves requests"
    );
}
