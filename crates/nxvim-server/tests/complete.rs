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

    // Select the first row, then accept: the typed `he` prefix is replaced with the
    // completed word. (Noselect — accept needs an explicit selection first.)
    feed(&rpc, "<C-n>");
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

    // First `<C-n>` activates the first row (noselect → row 0); a second advances
    // to the second row.
    feed(&rpc, "<C-n>");
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
        "local ok, e = pcall(function() nx.complete.setup { sources = { { 'made_up' } } } end) \
         return (not ok) and e or 'no error'",
    )
    .await;
    assert!(
        err.as_str().unwrap_or_default().contains("not found"),
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
    // The example maps <CR> as an accept key — but only after selecting a row
    // (noselect), so navigate first, then <CR> accepts.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["config config"]);
}

/// With `<CR>` mapped as a confirm key, an *unnavigated* popup must NOT eat the
/// Enter — nothing is selected yet, so `<CR>` inserts a newline (cmp-style
/// `select = false`). You only accept after explicitly moving the selection.
#[tokio::test]
async fn cr_inserts_a_newline_until_you_navigate() {
    let dir = temp_dir("complete_cr_noselect");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, \
         keys = { confirm = { '<C-y>', '<CR>' } } }",
    )
    .await;

    // Popup opens, but nothing is selected yet.
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["hello"]);
    assert_eq!(
        map_get(&menu, "selected_active").and_then(Value::as_bool),
        Some(false),
        "nothing is preselected"
    );

    // <CR> with nothing selected → a newline, not an accept.
    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["hello he", ""]);
}

#[tokio::test]
async fn cr_accepts_once_you_have_navigated() {
    let dir = temp_dir("complete_cr_navigated");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, \
         keys = { confirm = { '<C-y>', '<CR>' } } }",
    )
    .await;

    feed(&rpc, "ihello he");
    poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    // Navigate to the first row (now there IS an active selection)…
    feed(&rpc, "<C-n>");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup open"));
    assert_eq!(
        map_get(&menu, "selected_active").and_then(Value::as_bool),
        Some(true),
        "navigation activates the selection"
    );
    // …so <CR> now accepts it.
    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["hello hello"]);
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
    // so the list lines up with the text it will replace. (`col` is the logical
    // content anchor; each client offsets the box left by its own border width.)
    assert_eq!(menu_col(&menu), 6, "popup anchored at the word start");
    // It also drops its top border so it sits flush with the line below the cursor.
    assert_eq!(
        map_get(&menu, "border_top").and_then(Value::as_bool),
        Some(false),
        "completion popup has no top border"
    );
}

#[tokio::test]
async fn select_menu_keeps_its_full_border() {
    // A `select` (the other Cursor-placed menu) must stay fully bordered — the
    // borderless/flush treatment is completion-only.
    let dir = temp_dir("complete_select_border");
    let (rpc, mut incoming) = start(&dir, "").await;
    exec_lua(&rpc, "nx.ui.select({ 'one', 'two' }, {}, function() end)").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("select opens"));
    assert!(
        map_get(&menu, "border_top").is_none(),
        "select keeps its top border (no border_top override)"
    );
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

// ---- Phase 4-B: async sources (nx.complete.source{}) --------------------------

/// An async source registered with `debounce = 0` (so it dispatches synchronously
/// within the settle and the assertions stay timing-free) that echoes the prefix
/// back as a candidate — a faithful source that *reacts to its input* rather than
/// returning a canned value.
const ECHO_INIT: &str = "\
nx.complete.source {\n\
  name = 'echo', debounce = 0,\n\
  complete = function(ctx, push, done)\n\
    if ctx.prefix ~= '' then push(ctx.prefix .. '_async') end\n\
    done()\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'buffer', min_chars = 2 }, { 'echo' } } }";

#[tokio::test]
async fn async_source_streams_candidates_alongside_buffer_and_accepts() {
    let dir = temp_dir("complete_async_stream");
    let (rpc, mut incoming) = start(&dir, ECHO_INIT).await;

    // `hello` is a buffer word; `he` is the partial being typed. The popup carries
    // the buffer match *and* the async echo (`he_async`) — proof the async source
    // ran off the input path and its push landed in the same widget.
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert!(
        items.contains(&"hello".to_string()) && items.contains(&"he_async".to_string()),
        "popup carries both the buffer word and the async candidate: {items:?}"
    );
    // The document still holds only the typed prefix — the popup did not grab keys.
    assert_eq!(lines(&rpc).await, vec!["hello he"]);

    // Navigate to the async row and accept it: its `insert` text replaces the prefix.
    let async_row = items.iter().position(|i| i == "he_async").unwrap();
    for _ in 0..=async_row {
        feed(&rpc, "<C-n>");
    }
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["hello he_async"]);
}

#[tokio::test]
async fn async_only_source_drives_the_popup_and_reacts_to_the_prefix() {
    let dir = temp_dir("complete_async_only");
    // No `buffer` source — the popup is driven entirely by the async echo, so its
    // single row must equal the *current* prefix (it reacts to input, not a canned
    // value), and re-running on a longer prefix swaps the row by generation.
    let init = "\
nx.complete.source {\n\
  name = 'echo', debounce = 0,\n\
  complete = function(ctx, push, done)\n\
    if ctx.prefix ~= '' then push(ctx.prefix .. '_async') end\n\
    done()\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'echo' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["ab_async"]);

    // One more char re-dispatches the source at a new generation; the stale `ab_async`
    // row is atomically replaced by the new prefix's candidate (no stacking).
    feed(&rpc, "c");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("popup refreshes"),
    );
    assert_eq!(menu_items(&menu), vec!["abc_async"]);
    assert_eq!(lines(&rpc).await, vec!["abc"]);
}

#[tokio::test]
async fn async_source_with_no_matches_closes_the_confirmed_empty_popup() {
    let dir = temp_dir("complete_async_empty");
    // An async-only source that never pushes: after it `done()`s with nothing, the
    // popup is confirmed-empty and must close (completion has no prompt to keep up).
    let init = "\
nx.complete.source {\n\
  name = 'silent', debounce = 0,\n\
  complete = function(_ctx, _push, done) done() end,\n\
}\n\
nx.complete.setup { sources = { { 'silent' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "a source that streams nothing leaves no popup open"
    );
    assert_eq!(lines(&rpc).await, vec!["ab"]);
}

#[tokio::test]
async fn registering_a_reserved_builtin_name_fails_loud() {
    let dir = temp_dir("complete_reserved");
    let (rpc, _incoming) = start(&dir, "").await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() \
           nx.complete.source { name = 'buffer', complete = function() end } end) \
         return (not ok) and e or 'no error'",
    )
    .await;
    assert!(
        err.as_str().unwrap_or_default().contains("reserved"),
        "shadowing a built-in source name must fail loud, got {err:?}"
    );
}

#[tokio::test]
async fn a_stale_in_flight_async_push_is_dropped_by_generation() {
    let dir = temp_dir("complete_async_gen");
    // A source that DEFERS its push: it stashes a `flush` closure (capturing this
    // run's generation) in a global so the test controls exactly when each reply
    // lands — no timers, no flakiness. Typing past a prefix while its reply is in
    // flight must drop that reply (it is a generation behind the live prefix).
    let init = "\
_G.deferred = {}\n\
nx.complete.source {\n\
  name = 'deferred', debounce = 0,\n\
  complete = function(ctx, push, done)\n\
    table.insert(_G.deferred, function() push(ctx.prefix .. '_X'); done() end)\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'deferred' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Two triggers, two in-flight replies: gen-for-`ab` (stale) and gen-for-`abc`.
    feed(&rpc, "iab");
    feed(&rpc, "c");

    // Land the STALE reply first — it is a generation behind, so it is dropped and
    // nothing appears.
    exec_lua(&rpc, "_G.deferred[1]()").await;
    // Then land the live reply — its candidate is the only row shown.
    exec_lua(&rpc, "_G.deferred[2]()").await;

    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec!["abc_X"],
        "only the live generation's candidate survives"
    );
    assert_eq!(lines(&rpc).await, vec!["abc"]);
}
