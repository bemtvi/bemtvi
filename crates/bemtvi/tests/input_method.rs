//! Tier 2: when a GUI turns the platform's input methods on — real server ->
//! real `View`. An input method (Japanese, Chinese, Korean…) composes the keys
//! typed into it, so it's on only while keys type text: insert, replace and
//! select modes, the command line, a picker's query. In normal and visual mode
//! keys are commands, and an IME left on turns `hjkl` into kana.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{exec_lua, poll_menu, poll_no_menu, redraw_after, start_attached};
use bemtvi_view::View;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

fn view_of(map: Vec<(Value, Value)>) -> View {
    View::from_redraw(&[Value::Map(map)])
}

/// Feed `keys`, and whether the resulting view types text.
async fn types_text_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> bool {
    view_of(redraw_after(rpc, incoming, keys).await).types_text()
}

#[tokio::test]
async fn keys_type_text_only_in_the_modes_that_take_it() {
    let (rpc, mut incoming) = start_attached(Default::default(), 80, 23).await;
    let incoming = &mut incoming;

    assert!(!types_text_after(&rpc, incoming, "").await, "normal mode");
    assert!(types_text_after(&rpc, incoming, "i").await, "insert mode");
    assert!(
        types_text_after(&rpc, incoming, "hello").await,
        "still inserting"
    );
    assert!(
        !types_text_after(&rpc, incoming, "<Esc>").await,
        "back to normal"
    );
    assert!(types_text_after(&rpc, incoming, "R").await, "replace mode");
    assert!(
        !types_text_after(&rpc, incoming, "<Esc>").await,
        "normal after replace"
    );
    assert!(!types_text_after(&rpc, incoming, "v").await, "visual mode");
    assert!(
        types_text_after(&rpc, incoming, "<C-g>").await,
        "select mode"
    );
    assert!(
        !types_text_after(&rpc, incoming, "<Esc>").await,
        "normal after select"
    );
    assert!(
        !types_text_after(&rpc, incoming, "r").await,
        "`r` waiting for its char"
    );
    assert!(
        !types_text_after(&rpc, incoming, "<Esc>").await,
        "normal after `r`"
    );
    assert!(
        types_text_after(&rpc, incoming, ":").await,
        "the command line"
    );
    assert!(
        !types_text_after(&rpc, incoming, "<Esc>").await,
        "normal after `:`"
    );
    assert!(types_text_after(&rpc, incoming, "/").await, "a search");
    assert!(
        !types_text_after(&rpc, incoming, "<Esc>").await,
        "normal after `/`"
    );
}

#[tokio::test]
async fn a_picker_s_query_types_text() {
    let (rpc, mut incoming) = start_attached(Default::default(), 80, 23).await;

    exec_lua(
        &rpc,
        "btv.picker.source{ name='ime', items=function(ctx) ctx.push{ text='日本' } end }; \
         btv.picker.open('ime')",
    )
    .await;
    let open = poll_menu(&rpc, &mut incoming)
        .await
        .expect("the picker opened");
    assert!(view_of(open).types_text(), "a picker's query");

    redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let closed = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("the picker closed");
    assert!(
        !view_of(closed).types_text(),
        "normal mode after the picker"
    );
}
