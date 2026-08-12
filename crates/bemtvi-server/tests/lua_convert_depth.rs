use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{exec_lua, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

#[tokio::test]
async fn json_encode_cyclic_table_errors_not_crash() {
    let (rpc, _i) = start().await;
    let ok = exec_lua(
        &rpc,
        "local t = {}; t.self = t; local ok = pcall(vim.json.encode, t); return ok",
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
    assert_eq!(exec_lua(&rpc, "return 1 + 1").await.as_u64(), Some(2)); // alive
}

#[tokio::test]
async fn exec_lua_returning_cyclic_table_errors_not_crash() {
    let (rpc, _i) = start().await;
    let res = exec_lua(&rpc, "local t = {}; t.self = t; return t").await;
    assert_eq!(res, rmpv::Value::Nil);
    assert_eq!(exec_lua(&rpc, "return 7").await.as_u64(), Some(7)); // alive
}

#[tokio::test]
async fn json_encode_moderately_deep_table_succeeds() {
    let (rpc, _i) = start().await;
    let ok = exec_lua(
        &rpc,
        r#"
        local t = {}; local cur = t
        for _ = 1, 50 do cur.child = {}; cur = cur.child end
        local ok, s = pcall(vim.json.encode, t)
        return ok and type(s) == "string""#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(true));
}
