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

/// The float's content lines as plain text. Each wire line is a chunk run
/// `[[text, style_id], …]` (the `virt_lines` form), so concatenate the chunk texts
/// to recover the rendered text.
fn float_lines(float: &[(Value, Value)]) -> Vec<String> {
    match map_get(float, "lines") {
        Some(Value::Array(rows)) => rows.iter().map(line_text).collect(),
        other => panic!("expected lines array, got {other:?}"),
    }
}

/// The concatenated text of one wire line (a chunk run `[[text, style_id], …]`).
fn line_text(row: &Value) -> String {
    row.as_array()
        .map(|chunks| {
            chunks
                .iter()
                .filter_map(|c| c.as_array()?.first()?.as_str().map(str::to_string))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// One wire line's chunks as `(text, style_id)` pairs — the styled form a
/// "pretty" float (which-key) ships, so a test can assert per-segment highlight.
fn line_chunks(row: &Value) -> Vec<(String, Option<u64>)> {
    row.as_array()
        .map(|chunks| {
            chunks
                .iter()
                .filter_map(|c| {
                    let c = c.as_array()?;
                    Some((c.first()?.as_str()?.to_string(), c.get(1)?.as_u64()))
                })
                .collect()
        })
        .unwrap_or_default()
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

/// A styled float (the Phase 4 capability): a line may be a chunk list
/// `{ {text, hl_group?}, … }`, so a "pretty" caller (which-key) colours its
/// segments. The wire ships each line as `[[text, style_id], …]`, the style id
/// resolving against the redraw's `styles` palette — proof per-segment highlight
/// threads core → view → wire.
#[tokio::test]
async fn styled_chunk_lines_carry_per_segment_highlights() {
    let dir = temp_dir("ui_float_styled");
    let (rpc, mut incoming) = start(&dir, "").await;

    // Define two distinct groups so the ids resolve regardless of colorscheme.
    exec_lua(
        &rpc,
        "nx.hl.define(0, 'FloatKey', { fg = '#7dcfff', bold = true })\n\
         nx.hl.define(0, 'FloatDesc', { fg = '#565f89' })\n\
         nx.ui.float({ { { 'w', 'FloatKey' }, { '  write', 'FloatDesc' } } }, { border = 'none' })",
    )
    .await;

    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("a float redraw surface");
    let float = float_of(&map);

    // The plain text recovers verbatim from the chunk run.
    assert_eq!(float_lines(&float), vec!["w  write"]);

    // The row carries two chunks with DISTINCT, non-nil style ids.
    let rows = match map_get(&float, "lines") {
        Some(Value::Array(a)) => a.clone(),
        other => panic!("expected lines array, got {other:?}"),
    };
    let chunks = line_chunks(&rows[0]);
    assert_eq!(chunks.len(), 2, "two styled chunks: {chunks:?}");
    assert_eq!(chunks[0].0, "w");
    assert_eq!(chunks[1].0, "  write");
    let key_id = chunks[0].1.expect("key chunk has a style id");
    let desc_id = chunks[1].1.expect("desc chunk has a style id");
    assert_ne!(key_id, desc_id, "key and description are styled distinctly");

    // The key id resolves to the cyan + bold style we defined.
    let styles = match map_get(&map, "styles") {
        Some(Value::Array(a)) => a.clone(),
        other => panic!("expected styles palette, got {other:?}"),
    };
    let key_style = styles[key_id as usize].as_map().expect("style is a map");
    assert_eq!(
        map_get(key_style, "fg").and_then(Value::as_u64),
        Some(0x7dcfff),
        "key chunk resolved to FloatKey's fg"
    );
    assert_eq!(
        map_get(key_style, "bold").and_then(Value::as_bool),
        Some(true),
        "key chunk is bold"
    );
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
async fn a_persistent_float_survives_keystrokes() {
    let dir = temp_dir("ui_float_persist");
    let (rpc, mut incoming) = start(&dir, "").await;

    // persist = true returns a handle and the float is NOT dismissed by the next
    // key (the which-key shape: it observes keys via nx.on_key while staying open).
    exec_lua(
        &rpc,
        "_G.wk = nx.ui.float({ 'pending: f g h' }, { persist = true })",
    )
    .await;
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the persistent float opens");

    // Several keys go by; the float stays.
    feed(&rpc, "jjk");
    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the persistent float is still open after keys");
    assert_eq!(float_lines(&float_of(&map)), vec!["pending: f g h"]);
    assert_eq!(
        exec_lua(&rpc, "return _G.wk:is_open()").await,
        Value::Boolean(true)
    );
}

#[tokio::test]
async fn update_replaces_the_persistent_float_in_place() {
    let dir = temp_dir("ui_float_update");
    let (rpc, mut incoming) = start(&dir, "").await;

    exec_lua(&rpc, "_G.wk = nx.ui.float({ 'first' }, { persist = true })").await;
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the float opens");

    // :update swaps the content under the same handle id.
    exec_lua(&rpc, "_G.wk:update({ 'second', 'third' })").await;
    let map = poll_float(&rpc, &mut incoming, |f| match f {
        Value::Map(m) => {
            matches!(map_get(m, "height").and_then(Value::as_u64), Some(2))
        }
        _ => false,
    })
    .await
    .expect("the updated float surface");
    assert_eq!(float_lines(&float_of(&map)), vec!["second", "third"]);
}

#[tokio::test]
async fn close_dismisses_the_persistent_float() {
    let dir = temp_dir("ui_float_close");
    let (rpc, mut incoming) = start(&dir, "").await;

    exec_lua(&rpc, "_G.wk = nx.ui.float({ 'open' }, { persist = true })").await;
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the float opens");

    // The handle closes it explicitly; is_open() flips false and the surface clears.
    assert_eq!(
        exec_lua(&rpc, "_G.wk:close(); return _G.wk:is_open()").await,
        Value::Boolean(false)
    );
    poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
        .await
        .expect("the float is gone after :close()");
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

/// The `nx.component` "float" backend: the Vue-shaped component model rendered onto a
/// non-focus content float (the which-key surface). A reactive write re-renders into the
/// float in place; an EMPTY render hides it. Proves the component model is surface-agnostic
/// — the same setup/render/reactive core drives a float, not just a focus-taking view.
#[tokio::test]
async fn float_backed_component_renders_and_hides_with_reactive_state() {
    let dir = temp_dir("float_component");
    let (rpc, mut incoming) = start(
        &dir,
        r#"
        nx.component({
          surface = "float",
          setup = function(ctx)
            _G.s = ctx.reactive({ rows = { "alpha" } })
            return _G.s
          end,
          render = function(s)
            return { lines = s.rows, title = " demo " }
          end,
        }).mount({ relative = "editor" })
        "#,
    )
    .await;

    // The first render opens the float with the initial content.
    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the float-backed component opened a float");
    assert_eq!(float_lines(&float_of(&map)), vec!["alpha"]);

    // A reactive write re-renders → the float updates in place (two lines now).
    exec_lua(&rpc, "_G.s.rows = { 'beta', 'gamma' }").await;
    let map = poll_float(
        &rpc,
        &mut incoming,
        |f| matches!(f, Value::Map(m) if map_get(m, "height").and_then(Value::as_u64) == Some(2)),
    )
    .await
    .expect("the float updated to two lines");
    assert_eq!(float_lines(&float_of(&map)), vec!["beta", "gamma"]);

    // An empty render hides the float entirely.
    exec_lua(&rpc, "_G.s.rows = {}").await;
    assert!(
        poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Nil))
            .await
            .is_some(),
        "an empty render closed the float"
    );
}

/// Two float-backed components can't share the single content-float slot: the first to
/// display owns it, and a second one trying to display FAILS LOUD (a notify) instead of
/// silently clobbering the first. Guards the no-silent-clobber rule for the float surface.
#[tokio::test]
async fn a_second_float_component_fails_loud_instead_of_clobbering() {
    let dir = temp_dir("float_component_collide");
    let (rpc, mut incoming) = start(
        &dir,
        r#"
        local function comp(text)
          return nx.component({
            surface = "float",
            setup = function(ctx) return ctx.reactive({ t = text }) end,
            render = function(s) return { lines = { s.t }, relative = "editor" } end,
          })
        end
        comp("first"):mount({})
        comp("second"):mount({})
        "#,
    )
    .await;

    // The first component opened the float; the second must NOT replace its content.
    let map = poll_float(&rpc, &mut incoming, |f| matches!(f, Value::Map(_)))
        .await
        .expect("the first float component opened a float");
    assert_eq!(
        float_lines(&float_of(&map)),
        vec!["first"],
        "the first owns the slot; the second did not clobber it"
    );

    // The collision was reported loudly (the message rides a redraw).
    let mut reported = false;
    for _ in 0..40 {
        nxvim_test_harness::barrier(&rpc).await;
        if drain_to_latest_redraw(&mut incoming, |m| {
            map_get(m, "message")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("already displaying"))
        })
        .is_some()
        {
            reported = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        reported,
        "the second float component reported the collision"
    );
}
