//! Tier 2: when a GUI turns the platform's input methods on — real server ->
//! real `View`. An input method (Japanese, Chinese, Korean…) composes the keys
//! typed into it. `'imdisable'` (vim's option) turns it off; it's off by
//! default, so input methods are on in every mode until a config says otherwise —
//! from a keymap (`:set imdisable!` or `btv.o.imdisable`), or per mode from a
//! `ModeChanged` autocmd.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{exec_lua, redraw_after, start_attached};
use bemtvi_view::View;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

fn view_of(map: Vec<(Value, Value)>) -> View {
    View::from_redraw(&[Value::Map(map)])
}

/// Feed `keys`, and whether the resulting view leaves input methods on.
async fn input_method_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> bool {
    !view_of(redraw_after(rpc, incoming, keys).await).imdisable
}

#[tokio::test]
async fn input_methods_are_on_in_every_mode_by_default() {
    let (rpc, mut incoming) = start_attached(Default::default(), 80, 23).await;
    let incoming = &mut incoming;

    assert!(input_method_after(&rpc, incoming, "").await, "normal mode");
    assert!(input_method_after(&rpc, incoming, "i").await, "insert mode");
    assert!(
        input_method_after(&rpc, incoming, "<Esc>v").await,
        "visual mode"
    );
    assert!(
        input_method_after(&rpc, incoming, "<Esc>:").await,
        "the command line"
    );
}

#[tokio::test]
async fn imdisable_turns_input_methods_off_from_set_lua_and_a_keymap() {
    let (rpc, mut incoming) = start_attached(Default::default(), 80, 23).await;
    let incoming = &mut incoming;

    assert!(
        !input_method_after(&rpc, incoming, ":set imdisable<CR>").await,
        "`:set imdisable`"
    );
    assert!(
        input_method_after(&rpc, incoming, ":set noimd<CR>").await,
        "`:set noimd`"
    );

    exec_lua(&rpc, "btv.o.imdisable = true").await;
    assert!(
        !input_method_after(&rpc, incoming, "").await,
        "`btv.o.imdisable = true`"
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.imdisable").await.as_bool(),
        Some(true),
        "reads back"
    );

    exec_lua(
        &rpc,
        "btv.keymap.set('n', '<F7>', ':set imdisable!<CR>') \
         btv.keymap.set('n', '<F8>', function() btv.o.imdisable = not btv.o.imdisable end)",
    )
    .await;
    assert!(
        input_method_after(&rpc, incoming, "<F7>").await,
        "a keymap toggles it back on"
    );
    assert!(
        !input_method_after(&rpc, incoming, "<F7>").await,
        "and off again"
    );
    assert!(
        input_method_after(&rpc, incoming, "<F8>").await,
        "a Lua keymap toggles it on"
    );
    assert!(!input_method_after(&rpc, incoming, "<F8>").await, "and off");
}

#[tokio::test]
async fn a_mode_changed_autocmd_keeps_input_methods_to_insert_mode() {
    let (rpc, mut incoming) = start_attached(Default::default(), 80, 23).await;
    let incoming = &mut incoming;

    exec_lua(
        &rpc,
        "btv.o.imdisable = true \
         btv.autocmd.create('ModeChanged', { callback = function(a) \
           btv.o.imdisable = not a.match:find(':i$') \
         end })",
    )
    .await;

    assert!(!input_method_after(&rpc, incoming, "").await, "normal mode");
    assert!(input_method_after(&rpc, incoming, "i").await, "insert mode");
    assert!(
        input_method_after(&rpc, incoming, "日本").await,
        "still inserting"
    );
    assert!(
        !input_method_after(&rpc, incoming, "<Esc>").await,
        "back to normal"
    );
}
