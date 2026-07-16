//! The client-persistent session-swap control message (§B of the remote-connectors plan):
//! `nx.session.reconnect(spec)` (Lua) queues a client-directed swap that the server drains
//! into a `nx_session_reconnect` notification pushed OUT to the client. The client (TUI/GUI)
//! owns the window + transport and performs the actual reload — not exercised here; this
//! verifies the SEAM: the notification fires with the normalized spec, and a malformed spec
//! fails loud instead of emitting anything.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{barrier, exec_lua, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Look up `key` in an rmpv map value.
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, val)| val),
        _ => None,
    }
}

/// Poll `incoming` (~3s) for the next `nx_session_reconnect` notification and return its
/// single spec param. A barrier each round flushes the server and lets the client reader
/// task ferry the notification across before we drain.
async fn await_reconnect(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Option<Value> {
    for _ in 0..150 {
        barrier(rpc).await;
        while let Ok(msg) = incoming.try_recv() {
            if let Incoming::Notification { method, params } = msg {
                if method == "nx_session_reconnect" {
                    return params.into_iter().next();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn reconnect_emits_a_client_notification_with_the_normalized_spawn_spec() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    exec_lua(
        &rpc,
        "nx.session.reconnect({\n\
         \x20 transport = { kind = 'spawn', cmd = 'ssh host nxvim --daemon' },\n\
         \x20 config_source = 'local',\n\
         \x20 keep_buffers = true,\n\
         })",
    )
    .await;

    let spec = await_reconnect(&rpc, &mut incoming)
        .await
        .expect("a nx_session_reconnect notification must be emitted");

    let transport = map_get(&spec, "transport").expect("spec.transport present");
    assert_eq!(
        map_get(transport, "kind").and_then(Value::as_str),
        Some("spawn"),
        "transport.kind carried through",
    );
    assert_eq!(
        map_get(transport, "cmd").and_then(Value::as_str),
        Some("ssh host nxvim --daemon"),
        "transport.cmd carried through",
    );
    assert_eq!(
        map_get(&spec, "config_source").and_then(Value::as_str),
        Some("local"),
        "config_source carried through",
    );
    assert_eq!(
        map_get(&spec, "keep_buffers").and_then(Value::as_bool),
        Some(true),
        "keep_buffers carried through",
    );
}

#[tokio::test]
async fn reconnect_carries_a_structured_argv_spawn_spec() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    exec_lua(
        &rpc,
        "nx.session.reconnect({\n\
         \x20 transport = { kind = 'spawn', argv = { 'ssh', 'host', 'nxvim', '--daemon' } },\n\
         })",
    )
    .await;

    let spec = await_reconnect(&rpc, &mut incoming)
        .await
        .expect("a nx_session_reconnect notification must be emitted");
    let transport = map_get(&spec, "transport").expect("spec.transport present");
    let argv = match map_get(transport, "argv") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
        other => panic!("transport.argv should be a string array, got {other:?}"),
    };
    assert_eq!(
        argv,
        vec!["ssh", "host", "nxvim", "--daemon"],
        "the structured argv carries through verbatim (no shell)",
    );
}

#[tokio::test]
async fn reconnect_defaults_config_source_to_remote_and_keep_buffers_to_false() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    exec_lua(
        &rpc,
        "nx.session.reconnect({ transport = { kind = 'quic', addr = 'nxvim://host:7000/tok?cert=abc' } })",
    )
    .await;

    let spec = await_reconnect(&rpc, &mut incoming)
        .await
        .expect("a nx_session_reconnect notification must be emitted");

    let transport = map_get(&spec, "transport").expect("spec.transport present");
    assert_eq!(
        map_get(transport, "kind").and_then(Value::as_str),
        Some("quic"),
    );
    assert_eq!(
        map_get(transport, "addr").and_then(Value::as_str),
        Some("nxvim://host:7000/tok?cert=abc"),
    );
    // Defaults filled by the normalizer.
    assert_eq!(
        map_get(&spec, "config_source").and_then(Value::as_str),
        Some("remote"),
        "config_source defaults to remote",
    );
    assert_eq!(
        map_get(&spec, "keep_buffers").and_then(Value::as_bool),
        Some(false),
        "keep_buffers defaults to false",
    );
}

#[tokio::test]
async fn a_malformed_spec_fails_loud_and_emits_nothing() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A bad transport kind: the Lua validator errors (pcall reports false), and NO
    // notification is emitted.
    let ok = exec_lua(
        &rpc,
        "return pcall(function() nx.session.reconnect({ transport = { kind = 'carrier-pigeon' } }) end)",
    )
    .await;
    assert_eq!(
        ok.as_bool(),
        Some(false),
        "a bad transport kind must raise, not silently no-op",
    );
    // A spawn transport missing its cmd also fails.
    let ok2 = exec_lua(
        &rpc,
        "return pcall(function() nx.session.reconnect({ transport = { kind = 'spawn' } }) end)",
    )
    .await;
    assert_eq!(ok2.as_bool(), Some(false), "a spawn spec needs a cmd");

    assert!(
        await_reconnect(&rpc, &mut incoming).await.is_none(),
        "a malformed spec must not emit a nx_session_reconnect notification",
    );
}

#[tokio::test]
async fn config_source_merged_is_reserved_and_fails_loud() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // `"merged"` (§D) is reserved but not implemented — it must raise with a targeted
    // "not implemented" message (not silently pick a side), and emit no swap.
    let err = exec_lua(
        &rpc,
        "local ok, err = pcall(function()\n\
         \x20 nx.session.reconnect({\n\
         \x20   transport = { kind = 'quic', addr = 'nxvim://h:1/t?cert=c' },\n\
         \x20   config_source = 'merged',\n\
         \x20 })\n\
         end)\n\
         return err",
    )
    .await;
    let msg = err.as_str().unwrap_or("");
    assert!(
        msg.contains("merged") && msg.contains("not implemented"),
        "config_source 'merged' must raise a targeted reserved-not-implemented error, got {msg:?}",
    );
    assert!(
        await_reconnect(&rpc, &mut incoming).await.is_none(),
        "a reserved config_source must not emit a swap",
    );
}
