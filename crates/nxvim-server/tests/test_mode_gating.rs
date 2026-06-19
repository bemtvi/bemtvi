//! The `nx.test` framework + `nx._ui` mirror must be GATED behind plugin-test mode
//! (the `nx_enable_test_mode` RPC the `--test-plugin` runner sends): absent in a
//! normal editor session, present only once enabled.

use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, feed, start_attached};
use rmpv::Value;

#[tokio::test]
async fn test_api_is_absent_until_enabled() {
    let (rpc, _incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A normal session: no test API, no UI mirror.
    assert_eq!(
        exec_lua(&rpc, "return nx.test == nil").await,
        Value::Boolean(true),
        "nx.test must be nil in a normal session"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx._ui == nil").await,
        Value::Boolean(true),
        "nx._ui must be nil before test mode populates it"
    );

    // Enable test mode (what the runner does after attach).
    rpc.request("nx_enable_test_mode", vec![])
        .await
        .expect("enable test mode");

    assert_eq!(
        exec_lua(&rpc, "return type(nx.test)").await,
        Value::from("table"),
        "nx.test must be installed after enabling test mode"
    );
    assert_eq!(
        exec_lua(&rpc, "return type(nx.test.describe)").await,
        Value::from("function"),
    );

    // The UI mirror populates on the next redraw once test mode is on.
    feed(&rpc, "ihi");
    let mirrored = exec_lua(&rpc, "return nx._ui ~= nil").await;
    assert_eq!(
        mirrored,
        Value::Boolean(true),
        "nx._ui must be populated once test mode is on"
    );
}
