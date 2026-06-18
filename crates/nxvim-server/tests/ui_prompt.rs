//! Behavior tests for `nx.ui.input` (alias `vim.ui.input`) and `nx.ui.confirm` —
//! the two command-line prompt primitives of the `nx.ui.*` async UI surface
//! (`docs/specs/2026-06-11-native-plugin-api.md`). Both are PROMISE-ONLY and
//! non-blocking (ADR 0002 rule 3): the Lua call returns a promise at once and it
//! settles on a later tick when the user submits / cancels.
//!
//! Black-box like the rest: a real server sources an `init.lua`, the prompt is
//! driven over the same msgpack-RPC a UI uses, and the assertion is on the value
//! the promise's `:next` records, read back through `nvim_exec_lua`. Delivering the
//! prompt result fires the resolver and `apply_lua_effects` drains the `:next`
//! reaction to fixpoint in the same tick (the microtask-convergence path), so the
//! effect is visible on the next ordered read — no redraw timing needed.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, feed, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also the runtimepath), and return a connected client.
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

// ----- nx.ui.input -----------------------------------------------------------

#[tokio::test]
async fn input_returns_typed_text_on_enter() {
    let dir = temp_dir("ui_input_text");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = nil, false
         nx.ui.input({ prompt = 'Name: ' }):next(function(text)
           _G.result, _G.called = text, true
         end)",
    )
    .await;

    // The command line is now an open prompt; type into it and submit.
    feed(&rpc, "hello");
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("hello")
    );
}

#[tokio::test]
async fn input_cancel_resolves_with_nil() {
    let dir = temp_dir("ui_input_cancel");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = 'unset', false
         nx.ui.input({ prompt = 'Name: ' }):next(function(text) _G.result, _G.called = text, true end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // The promise resolved (so a caller can clean up) but with no text.
    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(exec_lua(&rpc, "return _G.result").await, Value::Nil);
}

#[tokio::test]
async fn input_prefills_default_and_returns_it_unedited() {
    let dir = temp_dir("ui_input_default");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result = nil
         nx.ui.input({ prompt = 'File: ', default = 'init.lua' }):next(function(t) _G.result = t end)",
    )
    .await;

    // Submit without editing: the prefilled default comes back verbatim.
    feed(&rpc, "<CR>");

    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("init.lua")
    );
}

#[tokio::test]
async fn input_empty_enter_is_empty_string_not_nil() {
    let dir = temp_dir("ui_input_empty");
    let (rpc, _incoming) = start(&dir, "").await;

    // An empty submission is "" (not a cancel) — matching neovim's vim.ui.input.
    exec_lua(
        &rpc,
        "_G.result = 'unset'
         nx.ui.input({}):next(function(t) _G.result = t end)",
    )
    .await;

    feed(&rpc, "<CR>");

    assert_eq!(exec_lua(&rpc, "return _G.result").await.as_str(), Some(""));
}

#[tokio::test]
async fn input_backspace_past_start_does_not_cancel() {
    let dir = temp_dir("ui_input_bs");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = 'unset', false
         nx.ui.input({ prompt = 'Name: ' }):next(function(text) _G.result, _G.called = text, true end)",
    )
    .await;

    // Type a char, then backspace it AND backspace again past the empty start.
    // Unlike the ex/search command line, a vim.ui.input prompt must stay open
    // (only <Esc> cancels) — so the promise has not settled yet.
    feed(&rpc, "a<BS><BS>");
    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(false),
        "backspacing past the start must not cancel the input"
    );

    // The prompt is still live: typing and submitting delivers the text.
    feed(&rpc, "ok<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("ok")
    );
}

#[tokio::test]
async fn vim_ui_input_callback_alias_still_works() {
    let dir = temp_dir("ui_input_alias");
    let (rpc, _incoming) = start(&dir, "").await;

    // vim.ui.input keeps neovim's callback shape (the compat layer adapts the
    // nx.ui.input promise back to on_confirm) — plugins that pass a callback work.
    exec_lua(
        &rpc,
        "_G.result = nil
         vim.ui.input({ prompt = 'Name: ' }, function(text) _G.result = text end)",
    )
    .await;
    feed(&rpc, "via alias<CR>");
    assert_eq!(
        exec_lua(&rpc, "return _G.result").await.as_str(),
        Some("via alias")
    );
}

#[tokio::test]
async fn nx_ui_input_rejects_the_old_callback_shape() {
    let dir = temp_dir("ui_input_guard");
    let (rpc, _incoming) = start(&dir, "").await;

    // Passing an on_confirm to the promise-only nx.ui.input fails loudly (no silent
    // no-op) — the error names the migration. pcall captures the raised message.
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.ui.input({}, function() end) end)
         return ok == false and e or '<no error>'",
    )
    .await;
    assert!(
        err.as_str().unwrap_or("").contains("promise-only"),
        "expected a promise-only migration error, got {err:?}"
    );
}

// ----- nx.ui.confirm ---------------------------------------------------------

#[tokio::test]
async fn confirm_yes_is_true() {
    let dir = temp_dir("ui_confirm_yes");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.answer, _G.called = nil, false
         nx.ui.confirm('Delete file?'):next(function(ok) _G.answer, _G.called = ok, true end)",
    )
    .await;

    feed(&rpc, "y");

    assert_eq!(
        exec_lua(&rpc, "return _G.called").await,
        Value::Boolean(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(true)
    );
}

#[tokio::test]
async fn confirm_no_is_false() {
    let dir = temp_dir("ui_confirm_no");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.answer = nil
         nx.ui.confirm('Delete file?'):next(function(ok) _G.answer = ok end)",
    )
    .await;

    feed(&rpc, "n");

    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(false)
    );
}

#[tokio::test]
async fn confirm_enter_takes_the_default() {
    let dir = temp_dir("ui_confirm_default");
    let (rpc, _incoming) = start(&dir, "").await;

    // Default Yes: <CR> accepts.
    exec_lua(
        &rpc,
        "_G.a = nil
         nx.ui.confirm('Save?'):next(function(ok) _G.a = ok end)",
    )
    .await;
    feed(&rpc, "<CR>");
    assert_eq!(exec_lua(&rpc, "return _G.a").await, Value::Boolean(true));

    // default = false: <CR> declines.
    exec_lua(
        &rpc,
        "_G.b = nil
         nx.ui.confirm('Overwrite?', { default = false }):next(function(ok) _G.b = ok end)",
    )
    .await;
    feed(&rpc, "<CR>");
    assert_eq!(exec_lua(&rpc, "return _G.b").await, Value::Boolean(false));
}

#[tokio::test]
async fn confirm_escape_is_false() {
    let dir = temp_dir("ui_confirm_esc");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.answer = nil
         nx.ui.confirm('Quit without saving?'):next(function(ok) _G.answer = ok end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // Cancel is a "no" — never a silent true.
    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(false)
    );
}
