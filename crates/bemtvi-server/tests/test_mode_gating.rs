//! The `btv.test` framework + `btv._ui` mirror must be GATED behind plugin-test mode
//! (the `btv_enable_test_mode` RPC the `--test-plugin` runner sends): absent in a
//! normal editor session, present only once enabled.

use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, feed, start_attached};
use rmpv::Value;

#[tokio::test]
async fn test_api_is_absent_until_enabled() {
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A normal session: no test API, no UI mirror.
    assert_eq!(
        exec_lua(&rpc, "return btv.test == nil").await,
        Value::Boolean(true),
        "btv.test must be nil in a normal session"
    );
    assert_eq!(
        exec_lua(&rpc, "return btv._ui == nil").await,
        Value::Boolean(true),
        "btv._ui must be nil before test mode populates it"
    );

    // Enable test mode (what the runner does after attach).
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    assert_eq!(
        exec_lua(&rpc, "return type(btv.test)").await,
        Value::from("table"),
        "btv.test must be installed after enabling test mode"
    );
    assert_eq!(
        exec_lua(&rpc, "return type(btv.test.describe)").await,
        Value::from("function"),
    );

    // The UI mirror populates on the next redraw once test mode is on.
    feed(&rpc, "ihi");
    let mirrored = exec_lua(&rpc, "return btv._ui ~= nil").await;
    assert_eq!(
        mirrored,
        Value::Boolean(true),
        "btv._ui must be populated once test mode is on"
    );
}

#[tokio::test]
async fn ui_statusline_mirror_carries_the_rendered_segment_text() {
    // `t:statusline()` (the `btv._ui.statusline` mirror) must reflect the actual
    // rendered global status line. It mirrors `global_status` — an array of
    // `{ text, style }` segment maps — so the text extractor has to read each map's
    // `text` key. (Regression: it used to only handle chunk-pair arrays and dropped
    // the map text entirely, leaving the mirror empty for every statusline.)
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;
    rpc.request("btv_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    // A single global bar (laststatus=3) with a literal `%`-format, so the rendered
    // text is deterministic regardless of file/cursor.
    feed(&rpc, ":set laststatus=3<CR>");
    feed(&rpc, ":set statusline=hello-status<CR>");
    // A keystroke to force a redraw that refreshes the mirror.
    feed(&rpc, "<Esc>");

    let sl = exec_lua(&rpc, "return btv._ui.statusline").await;
    assert_eq!(
        sl,
        Value::from("hello-status"),
        "the statusline mirror must carry the rendered global-bar text"
    );
}
