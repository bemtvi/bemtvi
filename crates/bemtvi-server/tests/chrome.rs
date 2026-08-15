//! The editor-chrome highlight groups the server resolves per frame and relays as
//! `chrome` (`name -> style_id` into the frame's `styles` palette), for the regions a
//! client can't theme on its own.
//!
//! `Cursor` is the one under test here: the GUI paints its own text cursor (the TUI
//! hands that to the terminal), and it reads this entry for the block's colour and the
//! colour it re-draws the covered glyph in. An entry that never arrives leaves it on
//! reverse video against `Normal` — a fine fallback, but then a colorscheme's cursor
//! colour would be silently ignored, which is exactly what this pins down.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{drain_to_latest_redraw, exec_lua, map_get, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The `(fg, bg)` of the global chrome region `key`, resolved through the frame's
/// `styles` palette. `None` when the colorscheme leaves the group undefined — the
/// server omits the entry entirely, and the client keeps its built-in look.
fn chrome_colors(map: &[(Value, Value)], key: &str) -> Option<(Option<u64>, Option<u64>)> {
    let Value::Map(chrome) = map_get(map, "chrome")? else {
        return None;
    };
    let id = map_get(chrome, key)?.as_u64()? as usize;
    let Value::Map(style) = map_get(map, "styles")?.as_array()?.get(id)? else {
        return None;
    };
    Some((
        map_get(style, "fg").and_then(Value::as_u64),
        map_get(style, "bg").and_then(Value::as_u64),
    ))
}

#[tokio::test]
async fn the_cursor_group_reaches_the_frame_chrome() {
    let (rpc, mut incoming) = start().await;
    // The conventional spelling of a cursor: the block takes the text's colour, the
    // glyph on it the background's — `hi Cursor guifg=#282c34 guibg=#528bff`.
    exec_lua(
        &rpc,
        "btv.hl.define(0, 'Cursor', { fg = '#282c34', bg = '#528bff' })",
    )
    .await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame = drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw arrived");
    assert_eq!(
        chrome_colors(&frame, "cursor"),
        Some((Some(0x28_2c_34), Some(0x52_8b_ff))),
        "the `Cursor` group must reach the client that paints the cursor"
    );
}
