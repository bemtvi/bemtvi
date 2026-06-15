//! Behavior tests for `nx.complete` — the native completion engine over the
//! unified float-list widget (`docs/specs/2026-06-14-nx-ui-float-widget.md`,
//! Phase 4-A: the `buffer` word-scan source, the non-grabbing insert-mode popup,
//! the Rust matcher, and native accept).
//!
//! Black-box like the rest: a real server sources an `init.lua` that calls
//! `nx.complete.setup{}`, completion is driven over the same msgpack-RPC a UI
//! uses, and the assertions are on the projected `menu` redraw surface (rows,
//! selected, match spans) and on the resulting buffer/cursor after accept.
//!
//! The key difference from the picker suite: the buffer **is** the query, so
//! typing edits the document and the popup must NOT swallow it — the tests assert
//! the document holds the typed prefix while the menu is open, and the completed
//! word only after accept.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, lines, map_get, spawn, temp_dir,
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

/// Enable the engine with the `buffer` source and a 2-char trigger gate (so
/// typing single letters during setup never opens a spurious popup).
const BUFFER_INIT: &str = "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } } }";

/// Poll for the latest redraw whose `menu` key is a map (the take-latest pattern).
async fn poll_menu(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// Poll for the latest redraw whose `menu` key is *absent / nil* — i.e. no popup.
async fn poll_no_menu(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            !matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            let _ = map;
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}

fn menu_of(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(map, "menu") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a menu map, got {other:?}"),
    }
}

/// The menu's visible row labels, in order.
fn menu_items(menu: &[(Value, Value)]) -> Vec<String> {
    match map_get(menu, "items") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|row| match row {
                Value::Array(a) => a.first().and_then(Value::as_str).unwrap_or("").to_string(),
                Value::String(s) => s.as_str().unwrap_or("").to_string(),
                other => panic!("unexpected menu row {other:?}"),
            })
            .collect(),
        other => panic!("expected menu items array, got {other:?}"),
    }
}

fn menu_selected(menu: &[(Value, Value)]) -> u64 {
    map_get(menu, "selected")
        .and_then(Value::as_u64)
        .expect("menu has a selected index")
}

/// A completion menu is a promptless (cursor-anchored) list — it must NOT carry a
/// picker `query` line.
fn assert_no_query(menu: &[(Value, Value)]) {
    assert!(
        map_get(menu, "query").is_none(),
        "completion menu must be promptless (no query line), got {menu:?}"
    );
}

#[tokio::test]
async fn buffer_completion_opens_then_accepts_without_touching_the_buffer_until_accept() {
    let dir = temp_dir("complete_open");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    // Seed a word, then start typing a matching prefix. Typing the seed never opens
    // a popup (the only word is the partial being typed, which is excluded).
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["hello"]);
    assert_eq!(menu_selected(&menu), 0);
    assert_no_query(&menu);
    // The matched chars (`he`) are highlighted.
    assert!(
        matches!(map_get(&menu, "match_spans"), Some(Value::Array(a)) if !a.is_empty()),
        "match spans track the prefix"
    );
    // The document holds only what was typed — the popup did not swallow the keys.
    assert_eq!(lines(&rpc).await, vec!["hello he"]);

    // Accept: the typed `he` prefix is replaced with the completed word.
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["hello hello"]);
    // The popup is gone after accept.
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "popup closes on accept"
    );
}

#[tokio::test]
async fn abort_closes_the_popup_and_keeps_the_typed_prefix() {
    let dir = temp_dir("complete_abort");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    feed(&rpc, "ihello he");
    poll_menu(&rpc, &mut incoming).await.expect("popup opens");

    feed(&rpc, "<C-e>");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "popup closes on abort"
    );
    // Nothing was inserted — the prefix stands.
    assert_eq!(lines(&rpc).await, vec!["hello he"]);
}

#[tokio::test]
async fn navigation_moves_the_selection_and_accept_inserts_that_row() {
    let dir = temp_dir("complete_nav");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    // Two candidates both fuzzy-match `al`.
    feed(&rpc, "ialpha alpaca al");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert_eq!(items.len(), 2, "two candidates: {items:?}");
    assert_eq!(menu_selected(&menu), 0);

    // `<C-n>` advances the selection to the second row.
    feed(&rpc, "<C-n>");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("popup still open"),
    );
    assert_eq!(menu_selected(&menu), 1);

    // Accept inserts whichever row is highlighted (the second candidate).
    let chosen = items[1].clone();
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec![format!("alpha alpaca {chosen}")]);
}

#[tokio::test]
async fn backspacing_below_min_chars_closes_the_popup() {
    let dir = temp_dir("complete_bs");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    feed(&rpc, "ihello he");
    poll_menu(&rpc, &mut incoming).await.expect("popup opens");

    // One backspace leaves a 1-char prefix (`h`), below the 2-char gate → closed.
    feed(&rpc, "<BS>");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "popup closes below min_chars"
    );
    assert_eq!(lines(&rpc).await, vec!["hello h"]);
}

#[tokio::test]
async fn an_unknown_source_fails_loud() {
    let dir = temp_dir("complete_unknown_src");
    // No engine config here — set it up at runtime so the error surfaces to us.
    let (rpc, _incoming) = start(&dir, "").await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() nx.complete.setup { sources = { { 'lsp' } } } end) \
         return (not ok) and e or 'no error'",
    )
    .await;
    assert!(
        err.as_str()
            .unwrap_or_default()
            .contains("not yet implemented"),
        "unknown source must fail loud, got {err:?}"
    );
}

#[tokio::test]
async fn example_config_loads_and_completes() {
    // The shipped `examples/ui-complete` config must load and enable the engine.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ui-complete")
        .canonicalize()
        .expect("examples/ui-complete dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Type a seed word + a matching prefix; the example's buffer source completes.
    feed(&rpc, "iconfig con");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["config"]);
    // The example maps <CR> as an accept key too.
    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["config config"]);
}

fn menu_col(menu: &[(Value, Value)]) -> u64 {
    map_get(menu, "col")
        .and_then(Value::as_u64)
        .expect("menu has a col")
}

#[tokio::test]
async fn popup_anchors_at_the_word_start_not_the_cursor() {
    let dir = temp_dir("complete_anchor");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    // Line is "hello he" (8 cols), caret at col 8; the prefix "he" starts at col 6.
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["hello"]);
    // The box anchors under the START of the word (col 6), not under the caret (8),
    // so the list lines up with the text it will replace.
    assert_eq!(menu_col(&menu), 6, "popup anchored at the word start");
}

#[tokio::test]
async fn manual_trigger_opens_even_with_auto_off_and_below_min_chars() {
    let dir = temp_dir("complete_manual");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 5 } }, auto = false }",
    )
    .await;

    // auto = false → typing a matching prefix opens nothing on its own.
    feed(&rpc, "ialpha al");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "no auto popup when auto = false"
    );

    // An explicit trigger opens it, ignoring both `auto` and the 5-char gate.
    exec_lua(&rpc, "nx.complete.trigger()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("manual popup"));
    assert_eq!(menu_items(&menu), vec!["alpha"]);
}

#[tokio::test]
async fn a_mapped_trigger_key_opens_the_popup() {
    let dir = temp_dir("complete_trigger_key");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer' } }, auto = false, \
         keys = { trigger = '<C-b>' } }",
    )
    .await;

    // No auto popup as we type; the mapped key opens it on demand.
    feed(&rpc, "ialpha al");
    assert!(poll_no_menu(&rpc, &mut incoming).await, "no auto popup");
    feed(&rpc, "<C-b>");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("key opens popup"),
    );
    assert_eq!(menu_items(&menu), vec!["alpha"]);
    // And it still completes through the document, untouched until accept.
    assert_eq!(lines(&rpc).await, vec!["alpha al"]);
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["alpha alpha"]);
}
