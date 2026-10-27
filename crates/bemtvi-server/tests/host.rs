//! `btv.host`: the app embedding the editor (`ServerInit::host`) and the messages a config
//! sends it. `btv.host.send(name, payload)` rides out as a `bemtvi_host` notification
//! (`[name, payload]`), which the client hands to its host; with no host it raises.
//! `btv.host.forward` sends `"changed"` / `"wrote"` messages as the buffer changes.

use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    barrier, config_init, exec_lua, feed_sync, start_attached, temp_dir, temp_path,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

fn hosted() -> ServerInit {
    ServerInit {
        host: Some("testapp".into()),
        ..ServerInit::default()
    }
}

/// The next `bemtvi_host` notification's `(name, payload)`, polled for ~3s; `None` if none
/// came (which a "nothing is sent" assertion wants).
async fn next_message(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<(String, Value)> {
    for _ in 0..150 {
        barrier(rpc).await;
        while let Ok(msg) = incoming.try_recv() {
            if let Incoming::Notification { method, params } = msg {
                if method == "bemtvi_host" {
                    let mut params = params.into_iter();
                    let name = params.next().and_then(|n| n.as_str().map(str::to_owned));
                    let payload = params.next().unwrap_or(Value::Nil);
                    return Some((name.expect("a message has a name"), payload));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, val)| val),
        _ => None,
    }
}

#[tokio::test]
async fn the_host_is_named_and_hears_what_is_sent() {
    let (rpc, mut incoming) = start_attached(hosted(), 80, 24).await;

    assert_eq!(
        exec_lua(&rpc, "return btv.host.name()").await,
        Value::from("testapp")
    );
    exec_lua(
        &rpc,
        "btv.host.send('preview', { file = 'a.md', lines = { 1, 2 }, on = true })",
    )
    .await;

    let (name, payload) = next_message(&rpc, &mut incoming)
        .await
        .expect("a bemtvi_host notification");
    assert_eq!(name, "preview");
    assert_eq!(map_get(&payload, "file"), Some(&Value::from("a.md")));
    assert_eq!(
        map_get(&payload, "lines"),
        Some(&Value::Array(vec![Value::from(1), Value::from(2)]))
    );
    assert_eq!(map_get(&payload, "on"), Some(&Value::from(true)));
}

#[tokio::test]
async fn a_config_sees_its_host_as_it_runs() {
    let dir = temp_dir("host_config");
    let init = ServerInit {
        host: Some("testapp".into()),
        ..config_init(&dir, "btv.g.embedded_in = btv.host.name()\n")
    };
    let (rpc, _incoming) = start_attached(init, 80, 24).await;

    assert_eq!(
        exec_lua(&rpc, "return btv.g.embedded_in").await,
        Value::from("testapp")
    );
}

#[tokio::test]
async fn with_no_host_sending_raises_and_sends_nothing() {
    let (rpc, mut incoming) = start_attached(ServerInit::default(), 80, 24).await;

    assert_eq!(exec_lua(&rpc, "return btv.host.name()").await, Value::Nil);
    let refused = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.host.send, 'preview', {}) return { ok, tostring(err) }",
    )
    .await;
    let refused = refused.as_array().unwrap();
    assert_eq!(refused[0], Value::from(false));
    assert!(
        refused[1]
            .as_str()
            .unwrap()
            .contains("no app embeds this editor"),
        "{refused:?}"
    );
    assert_eq!(next_message(&rpc, &mut incoming).await, None);
}

#[tokio::test]
async fn a_message_needs_a_name() {
    let (rpc, mut incoming) = start_attached(hosted(), 80, 24).await;

    for call in [
        "btv.host.send('')",
        "btv.host.send(nil)",
        "btv.host.send(3)",
    ] {
        let ok = exec_lua(&rpc, &format!("return (pcall(function() {call} end))")).await;
        assert_eq!(ok, Value::from(false), "{call}");
    }
    assert_eq!(next_message(&rpc, &mut incoming).await, None);
}

#[tokio::test]
async fn forward_tells_the_host_about_edits_and_writes() {
    let path = temp_path("host_forward");
    std::fs::write(&path, "one\n").unwrap();
    let init = ServerInit {
        file: Some(path.to_string_lossy().into_owned()),
        ..hosted()
    };
    let (rpc, mut incoming) = start_attached(init, 80, 24).await;
    exec_lua(&rpc, "btv.host.forward({ 'changed', 'wrote' })").await;

    feed_sync(&rpc, "ozwei<Esc>").await;
    let (name, payload) = next_message(&rpc, &mut incoming)
        .await
        .expect("a changed message");
    assert_eq!(name, "changed");
    assert_eq!(map_get(&payload, "modified"), Some(&Value::from(true)));
    let named = map_get(&payload, "name").and_then(Value::as_str).unwrap();
    assert!(
        named.ends_with(path.file_name().unwrap().to_str().unwrap()),
        "{named}"
    );

    feed_sync(&rpc, ":w<CR>").await;
    let (name, payload) = next_message(&rpc, &mut incoming)
        .await
        .expect("a wrote message");
    assert_eq!(name, "wrote");
    let named = map_get(&payload, "name").and_then(Value::as_str).unwrap();
    assert!(
        named.ends_with(path.file_name().unwrap().to_str().unwrap()),
        "{named}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nzwei\n");

    // Forwarding nothing stops it.
    exec_lua(&rpc, "btv.host.forward({})").await;
    feed_sync(&rpc, "odrei<Esc>:w<CR>").await;
    assert_eq!(next_message(&rpc, &mut incoming).await, None);
}

#[tokio::test]
async fn forward_rejects_what_it_does_not_know() {
    let (rpc, _incoming) = start_attached(hosted(), 80, 24).await;

    let refused = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.host.forward, { 'scrolled' }) return { ok, tostring(err) }",
    )
    .await;
    let refused = refused.as_array().unwrap();
    assert_eq!(refused[0], Value::from(false));
    assert!(
        refused[1].as_str().unwrap().contains("unknown event"),
        "{refused:?}"
    );
}

/// Stopping `changed` stops it at once: an edit still waiting out the pause isn't sent
/// after the app asked for no more, and forwarding again doesn't send it twice.
#[tokio::test]
async fn forward_drops_a_pending_change_when_it_stops() {
    let (rpc, mut incoming) = start_attached(hosted(), 80, 24).await;
    exec_lua(&rpc, "btv.host.forward({ 'changed' })").await;

    // Typed, then stopped inside the 200 ms pause.
    feed_sync(&rpc, "ohello<Esc>").await;
    exec_lua(&rpc, "btv.host.forward({})").await;
    assert_eq!(next_message(&rpc, &mut incoming).await, None);

    // Typed, then forwarded again inside the pause: one message, not two.
    exec_lua(&rpc, "btv.host.forward({ 'changed' })").await;
    feed_sync(&rpc, "oagain<Esc>").await;
    exec_lua(&rpc, "btv.host.forward({ 'changed' })").await;
    feed_sync(&rpc, "oonce more<Esc>").await;
    assert!(next_message(&rpc, &mut incoming).await.is_some());
    assert_eq!(next_message(&rpc, &mut incoming).await, None, "sent once");
}

/// A payload msgpack can't carry (a function, a coroutine) raises, naming where it is,
/// rather than reaching the app as `nil`.
#[tokio::test]
async fn a_payload_msgpack_cannot_carry_raises() {
    let (rpc, mut incoming) = start_attached(hosted(), 80, 24).await;

    let refused = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.host.send, 'x', { cb = function() end }) \
         return { ok, tostring(err) }",
    )
    .await;
    let refused = refused.as_array().unwrap();
    assert_eq!(refused[0], Value::from(false));
    assert!(
        refused[1].as_str().unwrap().contains("payload.cb"),
        "{refused:?}"
    );
    assert_eq!(next_message(&rpc, &mut incoming).await, None);
}
