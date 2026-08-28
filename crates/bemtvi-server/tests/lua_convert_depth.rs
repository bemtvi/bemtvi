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
    // A value that cannot cross the wire comes back as an ERROR naming why — the depth
    // guard's whole point is to say so rather than to recurse until the stack goes. (It
    // answered `Nil` before `nvim_exec_lua` learned to carry a Lua failure to its caller,
    // which made an unencodable value look like a chunk that returned nothing.)
    let err = rpc
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("local t = {}; t.self = t; return t"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .await
        .expect_err("a cyclic return value must not answer with a value");
    let err = err.to_string();
    assert!(
        err.contains("nesting too deep"),
        "the error names the depth guard, got {err:?}"
    );
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

// ------------------------------------------------- a sparse table is not a sequence
//
// The Lua->msgpack classifier decided "sequence" from `#t` alone. `#` returns a
// *border*, which for a table whose array part happens to end on its largest present
// index can exceed the number of keys in `1..=len`: `{[1]="a",[2]="b",[4]="d"}`
// reports `#t == 4`. Encoded as a sequence, key 4 silently became index 3 — a hole
// closed up and the value moved. A sequence now also requires every position to be
// present, which the entry count proves (Lua keys are unique, so count is coverage).

#[tokio::test]
async fn a_sparse_table_crosses_as_a_map_not_a_renumbered_array() {
    let (rpc, _i) = start().await;
    // Round-trip through the msgpack boundary `exec_lua`'s return value crosses.
    let v = exec_lua(
        &rpc,
        r#"local t = {} t[1] = "a" t[2] = "b" t[4] = "d" return t"#,
    )
    .await;
    match v {
        rmpv::Value::Map(entries) => {
            let mut got: Vec<(i64, String)> = entries
                .iter()
                .filter_map(|(k, val)| Some((k.as_i64()?, val.as_str()?.to_string())))
                .collect();
            got.sort();
            assert_eq!(
                got,
                vec![(1, "a".into()), (2, "b".into()), (4, "d".into())],
                "the hole must survive — key 4 is not index 3"
            );
        }
        rmpv::Value::Array(a) => {
            panic!("a sparse table crossed as an array, silently renumbering its keys: {a:?}")
        }
        other => panic!("unexpected shape {other:?}"),
    }
}

#[tokio::test]
async fn a_dense_table_still_crosses_as_an_array() {
    // The control: the fix must not turn every list into a map.
    let (rpc, _i) = start().await;
    let v = exec_lua(&rpc, r#"return { "a", "b", "c" }"#).await;
    match v {
        rmpv::Value::Array(a) => assert_eq!(a.len(), 3),
        other => panic!("a dense list must stay an array, got {other:?}"),
    }
}

// ---------------------------------------------- a non-UTF-8 Lua string still crosses
//
// Lua strings are byte strings. Converting one with `to_str()` and propagating the
// error failed the WHOLE call on bytes msgpack can carry perfectly well; they now
// cross as msgpack `bin`, the way neovim's encoder passes Lua string bytes through.

#[tokio::test]
async fn a_non_utf8_lua_string_crosses_as_binary_instead_of_failing() {
    let (rpc, _i) = start().await;
    let v = exec_lua(&rpc, r#"return string.char(0xff, 0xfe)"#).await;
    match v {
        rmpv::Value::Binary(b) => assert_eq!(b, vec![0xff, 0xfe]),
        other => {
            panic!("non-UTF-8 bytes must cross as msgpack `bin`, not fail the call: {other:?}")
        }
    }
}

#[tokio::test]
async fn a_utf8_lua_string_still_crosses_as_text() {
    // The control: valid text must not be demoted to binary.
    let (rpc, _i) = start().await;
    assert_eq!(
        exec_lua(&rpc, r#"return "héllo""#).await.as_str(),
        Some("héllo")
    );
}
