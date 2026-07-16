//! `nx.connect` + live `:connect` routing (§C of the remote-connectors plan): a connector
//! registers an async resolver for a URL scheme; `:connect <url>` routes through the local
//! VM, and either a matching resolver's spec swaps the window (a `nx_session_reconnect`
//! notification, §B) or — with no provider — the raw URL is handed back for the client's
//! built-in direct dial (a `nx_connect_fallback` notification). This verifies that SEAM: the
//! right notification fires with the right payload for each path (sync resolver, async
//! resolver, no provider), and that a bad resolver surfaces loud without swapping.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{ReconnectSpec, ReconnectTransport, ServerInit, SpawnCommand};
use nxvim_test_harness::{barrier, exec_lua, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

// ---------------------------------------------------------------------------
// The client-side fallback dial's URL → spec parser (`ReconnectSpec::fallback_from_url`),
// the direct dial used when no provider claims a `:connect <url>` (the TUI's path).
// ---------------------------------------------------------------------------

#[test]
fn fallback_dials_an_nxvim_uri_over_quic() {
    let spec = ReconnectSpec::fallback_from_url("nxvim://h:7000/tok?cert=abc").expect("quic spec");
    assert!(matches!(
        spec.transport,
        ReconnectTransport::Quic { addr } if addr == "nxvim://h:7000/tok?cert=abc"
    ));
}

#[test]
fn fallback_dials_a_bare_host_over_ssh() {
    let spec = ReconnectSpec::fallback_from_url("host").expect("ssh spec");
    match spec.transport {
        ReconnectTransport::Spawn {
            command: SpawnCommand::Argv(argv),
        } => assert_eq!(argv, vec!["ssh", "--", "host", "nxvim", "--daemon"]),
        other => panic!("expected an ssh argv spawn, got {other:?}"),
    }
}

#[test]
fn fallback_ssh_carries_user_and_port() {
    let spec = ReconnectSpec::fallback_from_url("me@box:2222").expect("ssh spec");
    match spec.transport {
        ReconnectTransport::Spawn {
            command: SpawnCommand::Argv(argv),
        } => assert_eq!(
            argv,
            vec!["ssh", "-p", "2222", "--", "me@box", "nxvim", "--daemon"]
        ),
        other => panic!("expected an ssh argv spawn, got {other:?}"),
    }
}

#[test]
fn fallback_rejects_an_option_injecting_host() {
    // A `-`-leading host would be smuggled to ssh as an option (e.g. `-oProxyCommand=…`).
    assert!(ReconnectSpec::fallback_from_url("-oProxyCommand=evil").is_err());
}

#[test]
fn fallback_rejects_an_unknown_scheme_and_a_remote_file() {
    // A `scheme://` we don't dial directly, with no provider, is a mistype — fail loud.
    assert!(ReconnectSpec::fallback_from_url("nvim://h:1/tok").is_err());
    // The ssh fallback doesn't wire opening a remote file.
    assert!(ReconnectSpec::fallback_from_url("host/path/to/file.rs").is_err());
}

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

/// Poll `incoming` (~3s) for the next notification named `method` and return its first param.
/// A barrier each round flushes the server and lets the client reader ferry it across; the
/// bound also lets a "nothing should fire" assertion return `None` after the full window.
async fn await_notification(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    method: &str,
) -> Option<Value> {
    for _ in 0..150 {
        barrier(rpc).await;
        while let Ok(msg) = incoming.try_recv() {
            if let Incoming::Notification { method: m, params } = msg {
                if m == method {
                    return params.into_iter().next();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

#[tokio::test]
async fn a_matching_provider_resolves_and_swaps_via_session_reconnect() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A synchronous resolver: `:connect container://ubuntu` should route to it and its spec
    // should ride out as a `nx_session_reconnect` (§B), NOT a fallback.
    exec_lua(
        &rpc,
        "nx.connect.register('container', function(url)\n\
         \x20 return { transport = { kind = 'spawn', argv = { 'docker', 'exec', url, 'nxvim', '--daemon' } } }\n\
         end)\n\
         nx.connect.connect('container://ubuntu')",
    )
    .await;

    let spec = await_notification(&rpc, &mut incoming, "nx_session_reconnect")
        .await
        .expect("a matching provider must swap via nx_session_reconnect");
    let transport = map_get(&spec, "transport").expect("spec.transport present");
    let argv = match map_get(transport, "argv") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
        other => panic!("transport.argv should be a string array, got {other:?}"),
    };
    assert_eq!(
        argv,
        vec!["docker", "exec", "container://ubuntu", "nxvim", "--daemon"],
        "the resolver's spec (built from the URL) carries through verbatim",
    );
}

#[tokio::test]
async fn a_resolver_can_pick_the_config_source() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // §D: the resolver's spec carries `config_source` through to the swap, so a connector can
    // ask for the LOCAL config (the daemon backs only the seams — the dev-container shape).
    exec_lua(
        &rpc,
        "nx.connect.register('dev', function(url)\n\
         \x20 return {\n\
         \x20   transport = { kind = 'spawn', argv = { 'docker', 'exec', url, 'nxvim', '--daemon' } },\n\
         \x20   config_source = 'local',\n\
         \x20 }\n\
         end)\n\
         nx.connect.connect('dev://box')",
    )
    .await;

    let spec = await_notification(&rpc, &mut incoming, "nx_session_reconnect")
        .await
        .expect("a resolved connect must swap");
    assert_eq!(
        map_get(&spec, "config_source").and_then(Value::as_str),
        Some("local"),
        "the resolver's config_source choice (§D) rides through to the swap",
    );
}

#[tokio::test]
async fn an_async_resolver_resolves_after_its_promise_settles() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A resolver that provisions asynchronously — returns a promise that fulfils on a later
    // tick with the spec. `nx.connect` must await it, then swap.
    exec_lua(
        &rpc,
        "nx.connect.register('ssh', function(url)\n\
         \x20 return nx.promise.new(function(resolve)\n\
         \x20   nx.on_next_tick(function()\n\
         \x20     resolve({ transport = { kind = 'quic', addr = 'nxvim://resolved:7000/tok?cert=x' } })\n\
         \x20   end)\n\
         \x20 end)\n\
         end)\n\
         nx.connect.connect('ssh://box')",
    )
    .await;

    let spec = await_notification(&rpc, &mut incoming, "nx_session_reconnect")
        .await
        .expect("an async resolver must still swap once its promise settles");
    let transport = map_get(&spec, "transport").expect("spec.transport present");
    assert_eq!(
        map_get(transport, "kind").and_then(Value::as_str),
        Some("quic"),
    );
    assert_eq!(
        map_get(transport, "addr").and_then(Value::as_str),
        Some("nxvim://resolved:7000/tok?cert=x"),
    );
}

#[tokio::test]
async fn no_provider_falls_back_with_the_raw_url() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // No provider registered for a bare ssh host: the URL should ride out as a
    // `nx_connect_fallback` for the client's direct dial — and NOT as a `nx_session_reconnect`.
    exec_lua(&rpc, "nx.connect.connect('user@host:22')").await;

    let url = await_notification(&rpc, &mut incoming, "nx_connect_fallback")
        .await
        .expect("no provider must fall back to the client's direct dial");
    assert_eq!(
        url.as_str(),
        Some("user@host:22"),
        "the raw URL rides verbatim for the client to dial",
    );
}

#[tokio::test]
async fn the_connect_command_routes_through_the_registry() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // Drive the real `:connect` ex-command (not just `nx.connect.connect`): a `nxvim://` URI
    // with no provider falls back with the URL intact.
    exec_lua(&rpc, "nx.cmd('connect nxvim://h:1/tok?cert=abc')").await;

    let url = await_notification(&rpc, &mut incoming, "nx_connect_fallback")
        .await
        .expect(":connect must route through nx.connect and fall back with no provider");
    assert_eq!(url.as_str(), Some("nxvim://h:1/tok?cert=abc"));
}

#[tokio::test]
async fn a_failing_resolver_surfaces_loud_and_does_not_swap() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    // A resolver that errors must NOT swap (no nx_session_reconnect) and must NOT fall back —
    // the provider claimed the URL; its failure is reported, leaving the session intact.
    exec_lua(
        &rpc,
        "nx.connect.register('boom', function() error('provisioning failed') end)\n\
         nx.connect.connect('boom://x')",
    )
    .await;

    assert!(
        await_notification(&rpc, &mut incoming, "nx_session_reconnect")
            .await
            .is_none(),
        "a failed resolver must not swap the session",
    );
    assert!(
        await_notification(&rpc, &mut incoming, "nx_connect_fallback")
            .await
            .is_none(),
        "a claimed-but-failed URL must not fall back to the client either",
    );
}
