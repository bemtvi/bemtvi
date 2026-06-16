//! Behavior tests for `nx.ui.input` (alias `vim.ui.input`) and `nx.ui.confirm` —
//! the two command-line prompt primitives of the `nx.ui.*` async UI surface
//! (`docs/specs/2026-06-11-native-plugin-api.md`). Both are callback-shaped and
//! non-blocking (ADR 0002 rule 3): the Lua call returns at once and the result
//! arrives on a later tick when the user submits / cancels.
//!
//! Black-box like the rest: a real server sources an `init.lua`, the prompt is
//! driven over the same msgpack-RPC a UI uses, and the assertion is on the
//! captured callback result, read back through `nvim_exec_lua`. The callback
//! side-effects round-trip through request/reply (the feed is processed before the
//! read on the same ordered connection), so they need no redraw timing.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, feed, lines, spawn, temp_dir};
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
         nx.ui.input({ prompt = 'Name: ' }, function(text)
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
async fn input_cancel_fires_callback_with_nil() {
    let dir = temp_dir("ui_input_cancel");
    let (rpc, _incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "_G.result, _G.called = 'unset', false
         nx.ui.input({ prompt = 'Name: ' }, function(text) _G.result, _G.called = text, true end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // The callback fired (so a caller can clean up) but with no text.
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
         nx.ui.input({ prompt = 'File: ', default = 'init.lua' }, function(t) _G.result = t end)",
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
         nx.ui.input({}, function(t) _G.result = t end)",
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
         nx.ui.input({ prompt = 'Name: ' }, function(text) _G.result, _G.called = text, true end)",
    )
    .await;

    // Type a char, then backspace it AND backspace again past the empty start.
    // Unlike the ex/search command line, a vim.ui.input prompt must stay open
    // (only <Esc> cancels) — so the callback has not fired yet.
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
async fn vim_ui_input_is_the_alias() {
    let dir = temp_dir("ui_input_alias");
    let (rpc, _incoming) = start(&dir, "").await;

    assert_eq!(
        exec_lua(&rpc, "return vim.ui.input == nx.ui.input").await,
        Value::Boolean(true)
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
         nx.ui.confirm('Delete file?', function(ok) _G.answer, _G.called = ok, true end)",
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
         nx.ui.confirm('Delete file?', function(ok) _G.answer = ok end)",
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
         nx.ui.confirm('Save?', function(ok) _G.a = ok end)",
    )
    .await;
    feed(&rpc, "<CR>");
    assert_eq!(exec_lua(&rpc, "return _G.a").await, Value::Boolean(true));

    // default = false: <CR> declines.
    exec_lua(
        &rpc,
        "_G.b = nil
         nx.ui.confirm('Overwrite?', { default = false }, function(ok) _G.b = ok end)",
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
         nx.ui.confirm('Quit without saving?', function(ok) _G.answer = ok end)",
    )
    .await;

    feed(&rpc, "<Esc>");

    // Cancel is a "no" — never a silent true.
    assert_eq!(
        exec_lua(&rpc, "return _G.answer").await,
        Value::Boolean(false)
    );
}

// ----- the shipped example config --------------------------------------------

#[tokio::test]
async fn example_config_loads_and_confirm_map_acts() {
    // The shipped `examples/ui-prompt` config must load (it references nx.ui.input
    // and nx.ui.confirm at setup time) and wire its leader maps. Drive the `\d`
    // confirm map end-to-end: accepting it runs the line delete in the callback.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ui-prompt")
        .canonicalize()
        .expect("examples/ui-prompt dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example],
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Give the buffer two lines, cursor on the first.
    feed(&rpc, "iline one<CR>line two<Esc>gg");
    assert_eq!(lines(&rpc).await, vec!["line one", "line two"]);

    // `\d` (leader = "\") opens the yes/no confirm; `y` accepts and the callback
    // runs `normal! dd`, deleting the current line.
    feed(&rpc, "\\d");
    feed(&rpc, "y");

    assert_eq!(lines(&rpc).await, vec!["line two"]);
}
