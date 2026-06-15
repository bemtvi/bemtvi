//! Behavior tests for `nx.ui.float` — the list-less **content float**, the
//! sibling of the selectable-list widget on the shared float layer
//! (`docs/specs/2026-06-14-nx-ui-float-widget.md`, "What stays out of this
//! widget").
//!
//! Black-box like the rest: a real server sources an `init.lua`, the float is
//! opened over the same msgpack-RPC a UI uses, and the assertions are on the
//! projected `float` redraw surface (lines / geometry / border) and on its
//! dismissal by the next key. The surface check polls for the latest redraw (the
//! reader task ferries notifications asynchronously).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, map_get, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll for the latest redraw whose `float` key satisfies `want` (a map when open,
/// `Nil` when closed), retrying so the reader task settles (take-latest pattern).
async fn poll_float(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: impl Fn(&Value) -> bool,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..40 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) =
            drain_to_latest_redraw(incoming, |m| map_get(m, "float").is_some_and(&want))
        {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// The `float` sub-map of a redraw (already known to be a map).
fn float_of(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(map, "float") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a float map, got {other:?}"),
    }
}

/// The float's content lines.
fn float_lines(float: &[(Value, Value)]) -> Vec<String> {
    match map_get(float, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|l| l.as_str().unwrap_or("").to_string())
            .collect(),
        other => panic!("expected lines array, got {other:?}"),
    }
}

#[tokio::test]
async fn float_projects_its_content_lines_and_border() {
    let dir = temp_dir("ui_float_lines");
    let (rpc, mut incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "nx.ui.float({ 'alpha', 'beta', 'gamma' }, { border = 'rounded' })",
    )
    .await;

    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("a float redraw surface");
    let float = float_of(&map);
    assert_eq!(float_lines(&float), vec!["alpha", "beta", "gamma"]);
    assert_eq!(
        map_get(&float, "border").and_then(Value::as_str),
        Some("rounded")
    );
    // Cursor placement (the default) reports a positive width/height box.
    assert!(map_get(&float, "width").and_then(Value::as_u64).unwrap() >= 5);
    assert_eq!(map_get(&float, "height").and_then(Value::as_u64), Some(3));
}

#[tokio::test]
async fn string_contents_split_on_newlines() {
    let dir = temp_dir("ui_float_string");
    let (rpc, mut incoming) = start(&dir, "").await;

    // A trailing newline must not render a blank last row.
    exec_lua(&rpc, "nx.ui.float('one\\ntwo\\n', {})").await;

    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("a float redraw surface");
    assert_eq!(float_lines(&float_of(&map)), vec!["one", "two"]);
}

#[tokio::test]
async fn the_next_key_dismisses_the_float() {
    let dir = temp_dir("ui_float_dismiss");
    let (rpc, mut incoming) = start(&dir, "").await;

    exec_lua(&rpc, "nx.ui.float({ 'transient' }, {})").await;
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the float opens");

    // Any key dismisses it (it is non-grabbing: the key is still handled normally).
    feed(&rpc, "<Esc>");
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("the float is gone after the next key");
}

#[tokio::test]
async fn editor_placement_centers_the_float() {
    let dir = temp_dir("ui_float_editor");
    let (rpc, mut incoming) = start(&dir, "").await;

    exec_lua(
        &rpc,
        "nx.ui.float({ 'centered content here' }, { relative = 'editor' })",
    )
    .await;

    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("a float redraw surface");
    let float = float_of(&map);
    let col = map_get(&float, "col").and_then(Value::as_u64).unwrap();
    // An 80-col viewport centering a ~20-col box lands the left edge well inside,
    // not at the cursor's column 0 (which is what cursor placement would give).
    assert!(col > 10, "editor placement should center, got col {col}");
}

#[tokio::test]
async fn example_config_loads_and_opens_a_float() {
    // The shipped `examples/ui-float` config must load (it references nx.ui.float
    // and nx.lsp.buf.hover at setup time) and wire its leader maps.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ui-float")
        .canonicalize()
        .expect("examples/ui-float dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // `\f` (leader = "\") opens the cursor-anchored info float the example defines.
    feed(&rpc, "\\f");
    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the example's \\f map opens a float");
    let lines = float_lines(&float_of(&map));
    assert!(
        lines.iter().any(|l| l.contains("nx.ui.float")),
        "expected the example's float content, got {lines:?}"
    );
}

#[tokio::test]
async fn unknown_border_is_rejected_loud() {
    let dir = temp_dir("ui_float_badborder");
    let (rpc, mut incoming) = start(&dir, "").await;

    // No silent fallback: an unknown border keyword skips the float and echoes.
    exec_lua(&rpc, "nx.ui.float({ 'x' }, { border = 'bogus' })").await;
    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("no float opens for a bad border");
    let message = map_get(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        message.contains("border") && message.contains("bogus"),
        "expected a loud border error, got message {message:?}"
    );
}
