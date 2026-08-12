//! Black-box tests for a completion source item's `on_accept` callback (P4): a
//! `btv.complete.source` item carrying `on_accept` OWNS the edit when its row is
//! accepted — the callback runs instead of core splicing the item's `insert`, and is
//! handed the trigger RANGE under the cursor. This is the seam a pure-Lua snippet
//! engine expands through. The suite proves the callback fires with the right range,
//! that it replaces the literal insert, that a plain item still inserts literally
//! (the two coexist), and that a throwing callback doesn't crash the server.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, feed, lines, poll_menu, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

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

/// A source whose single candidate carries an `on_accept` that stashes the handed-in
/// range into `_G.__ctx` and replaces the trigger with `REPLACED` (proving the
/// callback both receives the range and owns the edit — the item's `insert` is `foo`,
/// which must NOT appear).
const ON_ACCEPT_INIT: &str = "\
btv.complete.source {\n\
  name = 'snip', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then\n\
      ctx.push({\n\
        text = 'foo',\n\
        on_accept = function(_item, c)\n\
          _G.__ctx = string.format('%d,%d,%d,%d', c.start_row, c.start_col, c.end_row, c.end_col)\n\
          btv.buf.set_text(c.buf, c.start_row, c.start_col, c.end_row, c.end_col, { 'REPLACED' })\n\
        end,\n\
      })\n\
    end\n\
  end,\n\
}\n\
btv.complete.setup { sources = { { 'snip' } }, min_chars = 2 }";

/// Open the popup on a 2-char prefix, select row 0, accept.
async fn type_and_accept(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, prefix: &str) {
    feed(rpc, &format!("i{prefix}"));
    assert!(
        poll_menu(rpc, incoming).await.is_some(),
        "popup opens on the prefix"
    );
    feed(rpc, "<C-n>"); // select row 0 (noselect → first activates)
    feed(rpc, "<C-y>"); // accept
}

#[tokio::test]
async fn on_accept_runs_instead_of_the_literal_insert() {
    let dir = temp_dir("on_accept_owns_edit");
    let (rpc, mut incoming) = start(&dir, ON_ACCEPT_INIT).await;
    type_and_accept(&rpc, &mut incoming, "fo").await;
    // The callback replaced the trigger; the item's literal `insert` ("foo") never applied.
    assert_eq!(lines(&rpc).await, vec!["REPLACED"]);
}

#[tokio::test]
async fn on_accept_receives_the_trigger_range() {
    let dir = temp_dir("on_accept_range");
    let (rpc, mut incoming) = start(&dir, ON_ACCEPT_INIT).await;
    type_and_accept(&rpc, &mut incoming, "fo").await;
    // The word "fo" under the cursor: (0,0)..(0,2), end-exclusive.
    assert_eq!(
        exec_lua(&rpc, "return _G.__ctx").await,
        Value::String("0,0,0,2".into())
    );
}

#[tokio::test]
async fn a_plain_item_without_on_accept_still_inserts_literally() {
    let dir = temp_dir("on_accept_plain_coexists");
    // Same engine, a source whose item has NO on_accept: its `insert` applies natively.
    let init = "\
btv.complete.source {\n\
  name = 'plain', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then ctx.push('foobar') end\n\
  end,\n\
}\n\
btv.complete.setup { sources = { { 'plain' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;
    type_and_accept(&rpc, &mut incoming, "fo").await;
    assert_eq!(lines(&rpc).await, vec!["foobar"]);
}

#[tokio::test]
async fn a_throwing_on_accept_is_surfaced_without_crashing_the_server() {
    let dir = temp_dir("on_accept_throws");
    let init = "\
btv.complete.source {\n\
  name = 'boom', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then\n\
      ctx.push({ text = 'foo', on_accept = function() error('boom') end })\n\
    end\n\
  end,\n\
}\n\
btv.complete.setup { sources = { { 'boom' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;
    type_and_accept(&rpc, &mut incoming, "fo").await;
    // The buffer is untouched (the callback threw before editing) and the server is
    // still responsive — a follow-up round-trip succeeds.
    assert_eq!(lines(&rpc).await, vec!["fo"]);
    assert_eq!(
        exec_lua(&rpc, "return 1 + 1").await,
        Value::Integer(2.into())
    );
}

#[tokio::test]
async fn a_non_function_on_accept_fails_loud() {
    let dir = temp_dir("on_accept_bad_type");
    // A source that pushes a bad item (on_accept not a function) inside pcall, recording
    // whether the push raised.
    let init = "\
_G.__ok = true\n\
btv.complete.source {\n\
  name = 'bad', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then\n\
      _G.__ok = pcall(ctx.push, { text = 'foo', on_accept = 42 })\n\
    end\n\
  end,\n\
}\n\
btv.complete.setup { sources = { { 'bad' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "ifo");
    // Give the (debounce=0) source a chance to run and reject the bad push.
    for _ in 0..40 {
        bemtvi_test_harness::barrier(&rpc).await;
        if exec_lua(&rpc, "return _G.__ok").await.as_bool() == Some(false) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let _ = &mut incoming;
    assert_eq!(
        exec_lua(&rpc, "return _G.__ok").await.as_bool(),
        Some(false),
        "a non-function on_accept must raise from ctx.push"
    );
}
