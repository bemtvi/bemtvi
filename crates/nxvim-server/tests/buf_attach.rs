//! Black-box tests for `nx.buf.attach` (the `nvim_buf_attach` `on_bytes` / `on_reload`
//! change channel). The server already projects every edit into neovim's byte-delta
//! tuple and fires it; this suite proves the public subscriber receives it with the
//! right values, that `detach()` and a `return true` both stop the stream, that a
//! wholesale rope replace (undo) fires `on_reload` instead, and that the shape guards
//! fail loud. Driven over RPC; edits are fed as real keystrokes.

use nxvim_rpc::Rpc;
use nxvim_test_harness::{exec_lua, feed, start_with_file as open};
use rmpv::Value;

/// A string read back from a Lua global.
async fn read(rpc: &Rpc, expr: &str) -> String {
    match exec_lua(rpc, &format!("return {expr}")).await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        Value::Nil => String::new(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Attach an `on_bytes` recorder that appends each delta tuple to `_G.__ev`.
async fn attach_bytes_recorder(rpc: &Rpc) {
    exec_lua(
        rpc,
        r#"_G.__ev = {}
           _G.__detach = nx.buf.attach(0, {
             on_bytes = function(_, _buf, _tick, sr, sc, sb, oer, oec, oeb, ner, nec, neb)
               _G.__ev[#_G.__ev + 1] =
                 string.format("%d,%d,%d/%d,%d,%d/%d,%d,%d", sr, sc, sb, oer, oec, oeb, ner, nec, neb)
             end,
           })"#,
    )
    .await;
}

#[tokio::test]
async fn on_bytes_fires_with_the_byte_delta_of_an_insert() {
    let (rpc, _inc) = open("ab\n").await;
    attach_bytes_recorder(&rpc).await;
    // Insert "X" at (0,0): start (0,0,0), removed nothing (0,0,0), added one col/byte (0,1,1).
    feed(&rpc, "iX");
    feed(&rpc, "<Esc>");
    assert_eq!(
        read(&rpc, r#"_G.__ev[#_G.__ev]"#).await,
        "0,0,0/0,0,0/0,1,1"
    );
}

#[tokio::test]
async fn on_bytes_fires_with_the_byte_delta_of_a_delete() {
    let (rpc, _inc) = open("abc\n").await;
    attach_bytes_recorder(&rpc).await;
    // `x` on the first char deletes byte [0,1): start (0,0,0), removed one col/byte (0,1,1),
    // added nothing (0,0,0).
    feed(&rpc, "x");
    assert_eq!(
        read(&rpc, r#"_G.__ev[#_G.__ev]"#).await,
        "0,0,0/0,1,1/0,0,0"
    );
}

#[tokio::test]
async fn detach_stops_the_callbacks() {
    let (rpc, _inc) = open("ab\n").await;
    attach_bytes_recorder(&rpc).await;
    feed(&rpc, "iX");
    feed(&rpc, "<Esc>");
    // One event so far; detach, reset, edit again → no further events.
    exec_lua(&rpc, r#"_G.__detach(); _G.__ev = {}"#).await;
    feed(&rpc, "iY");
    feed(&rpc, "<Esc>");
    assert_eq!(read(&rpc, r#"tostring(#_G.__ev)"#).await, "0");
}

#[tokio::test]
async fn a_callback_returning_true_self_detaches() {
    let (rpc, _inc) = open("ab\n").await;
    exec_lua(
        &rpc,
        r#"_G.__n = 0
           nx.buf.attach(0, {
             on_bytes = function() _G.__n = _G.__n + 1; return true end,
           })"#,
    )
    .await;
    feed(&rpc, "iX");
    feed(&rpc, "<Esc>");
    feed(&rpc, "iY");
    feed(&rpc, "<Esc>");
    // Fired once, then removed itself — the second edit sees no subscriber.
    assert_eq!(read(&rpc, "tostring(_G.__n)").await, "1");
}

#[tokio::test]
async fn on_reload_fires_on_a_wholesale_replace() {
    let (rpc, _inc) = open("abc\n").await;
    exec_lua(
        &rpc,
        r#"_G.__reloads = 0
           nx.buf.attach(0, {
             on_reload = function(_, _buf) _G.__reloads = _G.__reloads + 1 end,
           })"#,
    )
    .await;
    // Make an edit, then undo it — undo replaces the whole rope, which can't be a delta
    // stream, so the channel fires on_reload rather than on_bytes.
    feed(&rpc, "iZ");
    feed(&rpc, "<Esc>");
    feed(&rpc, "u");
    assert_eq!(read(&rpc, "tostring(_G.__reloads)").await, "1");
}

#[tokio::test]
async fn an_unsupported_callback_fails_loud() {
    let (rpc, _inc) = open("a\n").await;
    let ok = exec_lua(
        &rpc,
        "return (pcall(nx.buf.attach, 0, { on_lines = function() end }))",
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
}

#[tokio::test]
async fn no_callback_fails_loud() {
    let (rpc, _inc) = open("a\n").await;
    let ok = exec_lua(&rpc, "return (pcall(nx.buf.attach, 0, {}))").await;
    assert_eq!(ok.as_bool(), Some(false));
}

// ----- nx.buf.changedtick ----------------------------------------------------

// `nx.buf.changedtick` is the *pull* half of the same change signal `on_bytes`
// pushes: the canonical value a plugin memoizes derived state against, so it must
// move on every text change and stay put otherwise (a cursor move, a mode flip, an
// unrelated buffer's edit). Without the "stays put" half a memo keyed on it would
// recompute every tick and the field would be worthless.

#[tokio::test]
async fn changedtick_advances_on_an_edit_and_holds_still_otherwise() {
    let (rpc, _inc) = open("alpha\nbeta\ngamma\n").await;

    let tick0 = read(&rpc, "tostring(nx.buf.changedtick(0))").await;

    // A pure cursor move changes no text — the tick must not budge, or a memo keyed
    // on it would recompute on every CursorMoved (the case this field exists for).
    feed(&rpc, "j");
    feed(&rpc, "l");
    assert_eq!(
        read(&rpc, "tostring(nx.buf.changedtick(0))").await,
        tick0,
        "a cursor move leaves changedtick alone"
    );

    // An edit advances it.
    feed(&rpc, "ix");
    feed(&rpc, "<Esc>");
    let tick1 = read(&rpc, "tostring(nx.buf.changedtick(0))").await;
    assert_ne!(tick1, tick0, "an insert advances changedtick");

    // Undo is a text change too, so it advances again (it does not rewind to tick0).
    feed(&rpc, "u");
    let tick2 = read(&rpc, "tostring(nx.buf.changedtick(0))").await;
    assert_ne!(tick2, tick1, "undo advances changedtick");

    // An unknown buffer reports 0 rather than erroring.
    assert_eq!(
        read(&rpc, "tostring(nx.buf.changedtick(99999))").await,
        "0",
        "an unknown buffer reports 0"
    );
}

#[tokio::test]
async fn changedtick_is_per_buffer_and_matches_getbufinfo() {
    let (rpc, _inc) = open("one\n").await;
    // A second buffer; editing it must not move the first buffer's tick.
    exec_lua(&rpc, "nx.cmd('enew')").await;
    let other = read(&rpc, "tostring(nx.buf.current())").await;
    exec_lua(&rpc, &format!("_G.__first = {other} - 1")).await;

    let before = read(&rpc, "tostring(nx.buf.changedtick(_G.__first))").await;
    feed(&rpc, "iedit-the-new-one");
    feed(&rpc, "<Esc>");
    assert_eq!(
        read(&rpc, "tostring(nx.buf.changedtick(_G.__first))").await,
        before,
        "editing one buffer leaves another buffer's changedtick alone"
    );

    // `getbufinfo()` reports the same value (it used to hard-code 0 — a fake).
    let cur = read(&rpc, "tostring(nx.buf.changedtick(0))").await;
    assert_eq!(
        read(
            &rpc,
            "tostring(vim.fn.getbufinfo(nx.buf.current())[1].changedtick)"
        )
        .await,
        cur,
        "getbufinfo reports the real changedtick"
    );
    assert_eq!(
        read(
            &rpc,
            "tostring(vim.fn.getbufinfo(nx.buf.current())[1].changed)"
        )
        .await,
        "1",
        "getbufinfo reports the real modified flag"
    );
}
