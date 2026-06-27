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
    attach, drain_to_latest_redraw, exec_lua, feed, feed_mouse, feed_mouse_mod, lines, map_get,
    spawn, temp_dir,
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

/// Resolve the `menu.styles[region]` palette id against the frame's top-level
/// `styles` palette and return its `attr` (`"fg"` / `"bg"`) color as `0xRRGGBB`.
fn menu_style_color(map: &[(Value, Value)], region: &str, attr: &str) -> Option<u32> {
    let menu = match map_get(map, "menu") {
        Some(Value::Map(m)) => m,
        _ => return None,
    };
    let styles = match map_get(menu, "styles") {
        Some(Value::Map(s)) => s,
        _ => return None,
    };
    let id = map_get(styles, region)?.as_u64()? as usize;
    let palette = match map_get(map, "styles") {
        Some(Value::Array(a)) => a,
        _ => return None,
    };
    match palette.get(id)? {
        Value::Map(style) => map_get(style, attr)?.as_u64().map(|n| n as u32),
        _ => None,
    }
}

/// The completion popup resolves its colors from nvim-cmp's highlight groups so a
/// colorscheme themes it automatically: `Pmenu`/`PmenuSel` the popup + selection,
/// `CmpItemAbbrMatch` the matched characters. The server resolves them to
/// `menu.styles` palette ids (each with a fallback chain), so no client hardcodes
/// the popup look.
#[tokio::test]
async fn completion_styles_resolve_from_cmp_groups() {
    let dir = temp_dir("complete_cmp_style");
    let init = format!(
        "vim.api.nvim_set_hl(0, 'Pmenu',            {{ bg = '#1e1e2e' }})\n\
         vim.api.nvim_set_hl(0, 'PmenuSel',         {{ bg = '#45475a' }})\n\
         vim.api.nvim_set_hl(0, 'CmpItemAbbrMatch', {{ fg = '#89b4fa', bold = true }})\n\
         vim.api.nvim_set_hl(0, 'CmpDocumentation', {{ bg = '#11111b' }})\n{BUFFER_INIT}"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    feed(&rpc, "ihello he");
    let map = poll_menu(&rpc, &mut incoming).await.expect("popup opens");

    assert_eq!(
        menu_style_color(&map, "bg", "bg"),
        Some(0x001e_1e2e),
        "the popup background uses Pmenu's bg"
    );
    assert_eq!(
        menu_style_color(&map, "sel", "bg"),
        Some(0x0045_475a),
        "the selected row uses PmenuSel's bg"
    );
    assert_eq!(
        menu_style_color(&map, "match", "fg"),
        Some(0x0089_b4fa),
        "matched characters use CmpItemAbbrMatch's fg"
    );
    assert_eq!(
        menu_style_color(&map, "doc", "bg"),
        Some(0x0011_111b),
        "the docs sidebar uses CmpDocumentation's bg"
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

/// The completion trigger is a Lua keymap installed by `nx.complete.setup` (it is no
/// longer a Rust native default). With `auto = false`, typing never opens the popup —
/// only the default `<C-Space>` trigger key does. Pressing it opens the menu, proving
/// the moved keymap fires `nx.complete.trigger()`.
#[tokio::test]
async fn trigger_key_opens_the_popup() {
    let dir = temp_dir("complete_trigger_key");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer' } }, auto = false }",
    )
    .await;

    // Seed a word on line 1, then type a matching prefix on line 2. With `auto =
    // false` the popup stays shut as we type.
    feed(&rpc, "ihello<CR>he");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "auto = false: typing must not open the popup"
    );

    // The default trigger key opens it — proving the `nx.complete.setup`-installed
    // `<C-Space>` map fires `nx.complete.trigger()`.
    feed(&rpc, "<C-Space>");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("the trigger key opens the popup"),
    );
    assert_eq!(menu_items(&menu), vec!["hello"]);
    // The trigger key did not type into the document.
    assert_eq!(lines(&rpc).await, vec!["hello", "he"]);
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

fn menu_row(menu: &[(Value, Value)]) -> u64 {
    map_get(menu, "row")
        .and_then(Value::as_u64)
        .expect("menu has a row")
}

/// Whether the popup has an **active** highlight (a row chosen), not the noselect
/// state a fresh popup opens in.
fn menu_active(menu: &[(Value, Value)]) -> bool {
    matches!(map_get(menu, "selected_active"), Some(Value::Boolean(true)))
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
    exec_lua(&rpc, "nx.ui.select({ 'one', 'two' }, {})").await;
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
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then ctx.push(ctx.prefix .. '_async') end\n\
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
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then ctx.push(ctx.prefix .. '_async') end\n\
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
  complete = function(_ctx) end,\n\
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
  complete = function(ctx)\n\
    return nx.promise.new(function(resolve)\n\
      table.insert(_G.deferred, function() ctx.push(ctx.prefix .. '_X'); resolve() end)\n\
    end)\n\
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

// ---- Phase 4-E: trigger-char sources + inline docs ----------------------------

/// An emoji-style trigger-char source: it declares `trigger = { chars = { ':' } }`,
/// so the engine wakes it only after a `:` and folds the `:` into the prefix. It
/// offers `:smile:` (inserting `SMILE`, with inline docs) while the prefix is a
/// prefix of that label. Alongside the `buffer` source, so the tests also prove the
/// buffer words are suppressed in a trigger context.
const EMOJI_INIT: &str = "\
nx.complete.source {\n\
  name = 'emoji', debounce = 0,\n\
  trigger = { chars = { ':' } },\n\
  complete = function(ctx)\n\
    if (':smile:'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = ':smile:', insert = 'SMILE', doc = 'A smiley face' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'buffer', min_chars = 2 }, { 'emoji' } } }";

/// The docs-sidebar lines of the latest redraw whose menu carries a `docs` sub-map.
async fn poll_menu_docs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| match map_get(m, "menu") {
            Some(Value::Map(menu)) => matches!(map_get(menu, "docs"), Some(Value::Map(_))),
            _ => false,
        }) {
            let Some(Value::Map(menu)) = map_get(&map, "menu") else {
                continue;
            };
            let Some(Value::Map(docs)) = map_get(menu, "docs") else {
                continue;
            };
            if let Some(Value::Array(lines)) = map_get(docs, "lines") {
                return Some(
                    lines
                        .iter()
                        .map(|l| l.as_str().unwrap_or("").to_string())
                        .collect(),
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// The latest redraw's docs sub-map (geometry + lines), or `None` if none carries one.
async fn poll_docs_map(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| match map_get(m, "menu") {
            Some(Value::Map(menu)) => matches!(map_get(menu, "docs"), Some(Value::Map(_))),
            _ => false,
        }) {
            if let Some(Value::Map(menu)) = map_get(&map, "menu") {
                if let Some(Value::Map(docs)) = map_get(menu, "docs") {
                    return Some(docs.clone());
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

fn docs_lines(docs: &[(Value, Value)]) -> Vec<String> {
    match map_get(docs, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|l| l.as_str().unwrap_or("").to_string())
            .collect(),
        other => panic!("expected docs lines array, got {other:?}"),
    }
}

fn docs_u64(docs: &[(Value, Value)], key: &str) -> usize {
    map_get(docs, key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("docs has a {key}")) as usize
}

/// A `<S-ScrollWheel>` / horizontal wheel over the completion **docs sidebar** scrolls
/// it sideways so a doc wider than the pane can be read past its right edge — the docs
/// counterpart of the picker preview's horizontal scroll.
#[tokio::test]
async fn shift_or_horizontal_wheel_scrolls_the_docs_sidebar_sideways() {
    let dir = temp_dir("complete_docs_hwheel");
    // A single doc line far wider than the ~60-col docs pane (100 cols of a digit run).
    let wide = "0123456789".repeat(10);
    let init = format!(
        "\
nx.complete.source {{\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push {{ text = 'hello', doc = '{wide}' }}\n\
    end\n\
  end,\n\
}}\n\
nx.complete.setup {{ sources = {{ {{ 'doc' }} }} }}"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let docs = poll_docs_map(&rpc, &mut incoming)
        .await
        .expect("docs sidebar");
    let before = docs_lines(&docs)[0].clone();
    assert!(before.starts_with("0123456789"), "unscrolled: {before:?}");
    let (row, col) = (docs_u64(&docs, "row"), docs_u64(&docs, "col"));

    // A horizontal wheel-right notch over the docs box scrolls it 6 columns right.
    feed_mouse(&rpc, "wheel", "right", row + 1, col + 1);
    let docs = poll_docs_map(&rpc, &mut incoming)
        .await
        .expect("docs after h-wheel");
    let after = docs_lines(&docs)[0].clone();
    assert_eq!(
        after,
        before.chars().skip(6).collect::<String>(),
        "the doc line scrolled 6 columns right"
    );

    // Shift + wheel-down is the same gesture: another 6 columns right.
    feed_mouse_mod(&rpc, "wheel", "down", "S", row + 1, col + 1);
    let docs = poll_docs_map(&rpc, &mut incoming)
        .await
        .expect("docs after S-wheel");
    assert_eq!(
        docs_lines(&docs)[0],
        before.chars().skip(12).collect::<String>()
    );

    // Wheel-left scrolls back; clamped at column 0 (never negative).
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "left", row + 1, col + 1);
    }
    let docs = poll_docs_map(&rpc, &mut incoming)
        .await
        .expect("docs after lefts");
    assert_eq!(docs_lines(&docs)[0], before, "scrolled back to column 0");
}

#[tokio::test]
async fn a_trigger_char_source_wakes_after_its_char_and_anchors_at_it() {
    let dir = temp_dir("complete_trigger_char");
    let (rpc, mut incoming) = start(&dir, EMOJI_INIT).await;

    // `hello` is a buffer word; then `:sm` is a trigger-char prefix. The popup must
    // carry ONLY `:smile:` — the emoji source woke on the `:`, and the `buffer`
    // source is suppressed in a trigger context (its words can't lead with `:`).
    feed(&rpc, "ihello :sm");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        menu_items(&menu),
        vec![":smile:"],
        "only the trigger-char source's row shows in a trigger context"
    );
    // The document still holds the typed text, `:` and all (the popup didn't grab).
    assert_eq!(lines(&rpc).await, vec!["hello :sm"]);

    // Accept it: the anchor is at the `:`, so the whole `:sm` is replaced by the
    // emoji's `insert` text — proof the trigger char was folded into the prefix.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["hello SMILE"]);
}

#[tokio::test]
async fn a_plain_prefix_leaves_the_trigger_char_source_dormant() {
    let dir = temp_dir("complete_trigger_dormant");
    let (rpc, mut incoming) = start(&dir, EMOJI_INIT).await;

    // A plain word prefix (no `:`) must NOT wake the emoji source — it offers the
    // buffer word `hello` and nothing else.
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert!(
        items.contains(&"hello".to_string()) && !items.contains(&":smile:".to_string()),
        "a trigger-char source stays dormant without its char: {items:?}"
    );
}

#[tokio::test]
async fn a_plugin_source_inline_doc_shows_in_the_docs_sidebar() {
    let dir = temp_dir("complete_inline_docs");
    let (rpc, mut incoming) = start(&dir, EMOJI_INIT).await;

    feed(&rpc, "i:sm");
    // Open + select row 0 so the docs sidebar (only shown for an active selection)
    // renders the emoji's inline `doc`.
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let docs = poll_menu_docs(&rpc, &mut incoming)
        .await
        .expect("docs sidebar appears");
    assert!(
        docs.iter().any(|l| l.contains("A smiley face")),
        "the plugin row's inline doc shows in the sidebar: {docs:?}"
    );
}

/// The docs sidebar must float over the WHOLE editor, not just the focused
/// window: when a vertical split narrows the active pane, a sidebar bounded by
/// that pane has no room beside the popup and collapses to a one-column sliver.
/// Bounded by the whole editor (floating over the other split) it keeps a usable
/// width. Regression: it used the focused window's text width as its bound.
#[tokio::test]
async fn docs_sidebar_spans_the_whole_editor_after_a_split() {
    let dir = temp_dir("complete_docs_split");
    // A long doc so the sidebar can't fit beside the popup within a ~40-col pane,
    // but does within the 80-col editor.
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'This is a fairly long documentation string for the row' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Split vertically; the focused window is now ~40 cols wide.
    feed(&rpc, "<C-w>v");
    nxvim_test_harness::barrier(&rpc).await;

    feed(&rpc, "ihel");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>"); // select row 0 so the docs sidebar shows

    let mut width = 0u64;
    for _ in 0..60 {
        nxvim_test_harness::barrier(&rpc).await;
        if let Some(map) = drain_to_latest_redraw(&mut incoming, |m| match map_get(m, "menu") {
            Some(Value::Map(menu)) => matches!(map_get(menu, "docs"), Some(Value::Map(_))),
            _ => false,
        }) {
            let Some(Value::Map(menu)) = map_get(&map, "menu") else {
                continue;
            };
            let Some(Value::Map(docs)) = map_get(menu, "docs") else {
                continue;
            };
            width = map_get(docs, "width").and_then(Value::as_u64).unwrap_or(0);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        width >= 20,
        "the docs sidebar must keep a usable width over the whole editor after a split, got {width}"
    );
}

/// The latest top-level redraw whose `menu` carries a `docs` sub-map (so a test
/// can read both the menu geometry and the focused window's gutter widths).
async fn poll_menu_top_with_docs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| match map_get(m, "menu") {
            Some(Value::Map(menu)) => matches!(map_get(menu, "docs"), Some(Value::Map(_))),
            _ => false,
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// A `u64` value at `key`, panicking when absent (geometry keys are always set).
fn mu64(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("missing u64 key {key:?}"))
}

/// The `docs` sub-map of a menu.
fn docs_of(menu: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(menu, "docs") {
        Some(Value::Map(d)) => d.clone(),
        other => panic!("expected a docs map, got {other:?}"),
    }
}

/// The focused window's map from a top-level redraw.
fn focused_window(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        panic!("no windows array in redraw");
    };
    for w in wins {
        if let Value::Map(wm) = w {
            if map_get(wm, "focused").and_then(Value::as_bool) == Some(true) {
                return wm.clone();
            }
        }
    }
    panic!("no focused window in redraw");
}

/// The focused window's region-relative `rect.x` (0 for a single window).
fn rect_x(win: &[(Value, Value)]) -> u64 {
    match map_get(win, "rect") {
        Some(Value::Map(r)) => map_get(r, "x").and_then(Value::as_u64).unwrap_or(0),
        _ => 0,
    }
}

/// The docs sidebar must butt against the popup's right border — its content one
/// cell past that border. Its region-relative column therefore counts the *whole*
/// gutter the popup box sits behind: the sign column AND the number column.
/// Regression: it counted only the number column, sliding the sidebar
/// `sign_width` cells left of the popup (a visible gap once a sign column shows).
#[tokio::test]
async fn docs_sidebar_butts_against_the_popup_past_the_sign_column() {
    let dir = temp_dir("complete_docs_signcolumn");
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'docs for hello' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Reserve a 2-cell sign column so the gutter before the text is non-empty.
    exec_lua(&rpc, "vim.cmd[[set signcolumn=yes]]").await;

    feed(&rpc, "ihel");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>"); // select row 0 so the docs sidebar shows

    let map = poll_menu_top_with_docs(&rpc, &mut incoming)
        .await
        .expect("docs sidebar appears");
    let menu = menu_of(&map);
    let docs = docs_of(&menu);
    let win = focused_window(&map);
    let sign_width = mu64(&win, "sign_width");
    let number_width = mu64(&win, "number_width");
    assert!(
        sign_width >= 2,
        "signcolumn=yes reserves a sign column, got {sign_width}"
    );
    assert_eq!(
        rect_x(&win),
        0,
        "the single window sits at the region origin"
    );

    let menu_col = mu64(&menu, "col");
    let menu_width = mu64(&menu, "width");
    let docs_col = mu64(&docs, "col");
    // The window has no padding and sits at the region origin, so the sidebar's
    // region-relative content column is the full gutter (sign + number) + the box
    // content span + 2 (the box's right border, then the sidebar's own left border).
    assert_eq!(
        docs_col,
        sign_width + number_width + menu_col + menu_width + 2,
        "docs sidebar butts against the popup including the sign column"
    );
}

/// With a left dock open, the sidebar's region-relative geometry must be bounded
/// by the editor width MINUS that region's left offset (the dock band), or it
/// overruns the editor's right edge. Regression: it was bounded by the full editor
/// width, so the box spilled past the right edge by the dock band's width.
#[tokio::test]
async fn docs_sidebar_respects_the_right_edge_past_a_left_dock() {
    let dir = temp_dir("complete_docs_left_dock");
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'This is a fairly long documentation string for the row, long enough to want clamping' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Open a 20-col left dock (its band is 21 incl. the separator). Opening a dock
    // focuses it, so cross back to the main window before completing.
    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "<C-w><C-w>l");
    nxvim_test_harness::barrier(&rpc).await;

    feed(&rpc, "ihel");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");

    let map = poll_menu_top_with_docs(&rpc, &mut incoming)
        .await
        .expect("docs sidebar appears");
    let menu = menu_of(&map);
    let docs = docs_of(&menu);
    let docs_col = mu64(&docs, "col");
    let docs_width = mu64(&docs, "width");
    // The main region begins one cell past the 20-col dock (band 21). The editor is
    // 80 cols, so the region-relative right edge is 80 − 21 = 59; the sidebar's box
    // (content plus its right border) must stay within it.
    let region_right = 80 - (20 + 1);
    assert!(
        docs_col + docs_width <= region_right,
        "docs sidebar must stay within the editor's right edge: col {docs_col} + width {docs_width} > {region_right}"
    );
    // ...and still be usable, not collapsed to a sliver.
    assert!(
        docs_width >= 20,
        "the sidebar keeps a usable width past the dock, got {docs_width}"
    );
}

#[tokio::test]
async fn a_plugin_source_resolve_callback_fills_the_sidebar_lazily() {
    let dir = temp_dir("complete_resolve_docs");
    // The source pushes a row with NO inline `doc` but a `resolve` callback. The
    // sidebar must stay empty until the row is selected, then fill from `resolve`'s
    // response — proof the lazy-docs path round-trips (server asks, source answers).
    let init = "\
nx.complete.source {\n\
  name = 'lazy', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then ctx.push { text = ctx.prefix .. '_lazy', insert = 'LAZY' } end\n\
  end,\n\
  resolve = function(item)\n\
    return nx.promise.resolve { doc = 'resolved docs for ' .. item.text }\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'lazy' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    // Select the row — the server resolves its docs off the input path.
    feed(&rpc, "<C-n>");
    let docs = poll_menu_docs(&rpc, &mut incoming)
        .await
        .expect("docs sidebar appears after resolve");
    assert!(
        docs.iter().any(|l| l.contains("resolved docs for ab_lazy")),
        "the resolve callback's docs fill the sidebar: {docs:?}"
    );
}

#[tokio::test]
async fn a_resolve_function_must_be_a_function() {
    let dir = temp_dir("complete_resolve_bad");
    let (rpc, _incoming) = start(&dir, "").await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(function() \
           nx.complete.source { name = 's', complete = function() end, resolve = 42 } end) \
         return (not ok) and e or 'no error'",
    )
    .await;
    assert!(
        err.as_str()
            .unwrap_or_default()
            .contains("resolve must be a function"),
        "a non-function resolve must fail loud, got {err:?}"
    );
}

// ── Mouse: the popup is a non-grabbing overlay the client forwards raw cells for;
//    the core hit-tests the click/wheel back to a row (Phase 1). ──────────────

#[tokio::test]
async fn clicking_a_completion_row_selects_it_then_accepts_on_a_second_click() {
    let dir = temp_dir("complete_mouse_click");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;
    // Drop the number gutter so the menu's text-area columns are global cells (the
    // client offsets by the gutter; core's hit-test does the same — exercised here by
    // making it zero so the clicked column is unambiguous).
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    // Two earlier words match the typed prefix `he`, so the popup has two rows.
    feed(&rpc, "ihello hero he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert_eq!(items.len(), 2, "two candidates match `he`, got {items:?}");
    assert!(!menu_active(&menu), "a fresh popup opens noselect");
    // The borderless top means the first list row sits on the box's top row.
    let col = menu_col(&menu) as usize;
    let row0 = menu_row(&menu) as usize;

    // Click the second row: it highlights (like navigating to it with <C-n>), the
    // document is untouched, and nothing is accepted yet.
    feed_mouse(&rpc, "left", "press", row0 + 1, col);
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("redraw after click"),
    );
    assert!(menu_active(&menu), "the click activates the highlight");
    assert_eq!(
        menu_selected(&menu),
        1,
        "the clicked (second) row is highlighted"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["hello hero he"],
        "highlighting a row does not edit the document"
    );

    // Click the already-highlighted row again: it accepts (replacing the `he` prefix
    // with that row's word), like pressing <C-y>.
    feed_mouse(&rpc, "left", "press", row0 + 1, col);
    assert_eq!(lines(&rpc).await, vec![format!("hello hero {}", items[1])]);
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "the popup closes on accept"
    );
}

#[tokio::test]
async fn wheeling_over_the_completion_popup_moves_the_highlight_without_wrapping() {
    let dir = temp_dir("complete_mouse_wheel");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, "ihello hero he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu).len(), 2);
    let col = menu_col(&menu) as usize;
    let row0 = menu_row(&menu) as usize;

    // A wheel-down notch over the popup highlights the first row (from noselect)…
    feed_mouse(&rpc, "wheel", "down", row0, col);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("redraw"));
    assert!(menu_active(&menu), "the wheel activates the highlight");
    assert_eq!(menu_selected(&menu), 0);

    // …another moves it down one…
    feed_mouse(&rpc, "wheel", "down", row0 + 1, col);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("redraw"));
    assert_eq!(menu_selected(&menu), 1);

    // …and a third stays on the last row (a wheel is a scrollbar, not <C-n>'s wrap).
    feed_mouse(&rpc, "wheel", "down", row0 + 1, col);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("redraw"));
    assert_eq!(menu_selected(&menu), 1);

    // A wheel-up notch walks it back toward the top.
    feed_mouse(&rpc, "wheel", "up", row0, col);
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("redraw"));
    assert_eq!(menu_selected(&menu), 0);
}

#[tokio::test]
async fn clicking_away_closes_the_completion_popup() {
    let dir = temp_dir("complete_click_away");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let row0 = menu_row(&menu) as usize;

    // Click away from the popup (a row below it): the cursor leaves the word, so the
    // popup must close instead of following.
    feed_mouse(&rpc, "left", "press", row0 + 4, 0);
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "clicking away closes the completion popup"
    );
}

#[tokio::test]
async fn scrolling_the_text_closes_the_completion_popup() {
    let dir = temp_dir("complete_scroll_away");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let row0 = menu_row(&menu) as usize;

    // A wheel over the text (away from the popup) scrolls the view, so the popup
    // must close instead of trailing the cursor.
    feed_mouse(&rpc, "wheel", "down", row0 + 4, 0);
    assert!(
        poll_no_menu(&rpc, &mut incoming).await,
        "scrolling the text closes the completion popup"
    );
}

/// The docs sidebar `(row, col, lines)` of the latest redraw whose `menu.docs`
/// sub-map's lines satisfy `want` — `row`/`col` are its text-area content cells
/// (global cells once the gutter is off).
async fn poll_docs_box(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: impl Fn(&[String]) -> bool,
) -> Option<(usize, usize, Vec<String>)> {
    let docs_lines = |docs: &[(Value, Value)]| -> Vec<String> {
        match map_get(docs, "lines") {
            Some(Value::Array(ls)) => ls
                .iter()
                .map(|l| l.as_str().unwrap_or("").to_string())
                .collect(),
            _ => Vec::new(),
        }
    };
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| match map_get(m, "menu") {
            Some(Value::Map(menu)) => match map_get(menu, "docs") {
                Some(Value::Map(docs)) => want(&docs_lines(docs)),
                _ => false,
            },
            _ => false,
        }) {
            let Some(Value::Map(menu)) = map_get(&map, "menu") else {
                continue;
            };
            let Some(Value::Map(docs)) = map_get(menu, "docs") else {
                continue;
            };
            let row = map_get(docs, "row").and_then(Value::as_u64)? as usize;
            let col = map_get(docs, "col").and_then(Value::as_u64)? as usize;
            return Some((row, col, docs_lines(docs)));
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

#[tokio::test]
async fn wheeling_over_the_completion_docs_sidebar_scrolls_it() {
    let dir = temp_dir("complete_docs_scroll");
    // One row carrying a TALL inline doc (more lines than the sidebar's 12-row cap),
    // so the docs float has content to scroll.
    let init = "\
nx.complete.source {\n\
  name = 'docs', debounce = 0,\n\
  complete = function(ctx)\n\
    local d = {}\n\
    for i = 1, 30 do d[i] = string.format('doc line %02d', i) end\n\
    ctx.push { text = 'alpha', doc = table.concat(d, '\\n') }\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'docs' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;
    // Drop the gutter so the docs sidebar's text-area cells are global screen cells.
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, "ial");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    // Select row 0 so the docs sidebar shows (it renders only for an active row).
    feed(&rpc, "<C-n>");
    let (row, col, top) = poll_docs_box(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) == Some("doc line 01")
    })
    .await
    .expect("docs sidebar opens at the top");
    assert_eq!(top.first().map(String::as_str), Some("doc line 01"));

    // Three wheel-down notches over the docs float scroll it down three lines — the
    // wheel acts on the docs, NOT the highlight (which stays on row 0).
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "down", row + 1, col + 1);
    }
    let (_, _, scrolled) = poll_docs_box(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) != Some("doc line 01")
    })
    .await
    .expect("the wheel scrolled the docs sidebar");
    assert_eq!(
        scrolled.first().map(String::as_str),
        Some("doc line 04"),
        "three wheel-down notches advanced the docs by three lines: {scrolled:?}"
    );
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("menu still open"),
    );
    assert_eq!(
        menu_selected(&menu),
        0,
        "wheeling the docs did not move the popup highlight"
    );

    // Wheeling back up returns to the top, non-wrapping (stops at line 01).
    for _ in 0..5 {
        feed_mouse(&rpc, "wheel", "up", row + 1, col + 1);
    }
    let (_, _, back) = poll_docs_box(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) == Some("doc line 01")
    })
    .await
    .expect("the docs scrolled back to the top");
    assert_eq!(back.first().map(String::as_str), Some("doc line 01"));
}

/// A 30-line inline doc, for the dock-aware geometry tests below.
const TALL_DOC_INIT: &str = "\
nx.complete.source {\n\
  name = 'docs', debounce = 0,\n\
  complete = function(ctx)\n\
    local d = {}\n\
    for i = 1, 30 do d[i] = string.format('doc line %02d', i) end\n\
    ctx.push { text = 'alpha', doc = table.concat(d, '\\n') }\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'docs' } } }";

/// With a top dock open, the sidebar's region-relative height must be clamped
/// against the editor height MINUS the region's top offset (the dock band), or a
/// tall doc opened low in the region runs past the editor's bottom edge.
/// Regression: it was clamped against the full editor height.
#[tokio::test]
async fn docs_sidebar_respects_the_bottom_edge_past_a_top_dock() {
    let dir = temp_dir("complete_docs_top_dock");
    let (rpc, mut incoming) = start(&dir, TALL_DOC_INIT).await;

    // A 5-row top dock pushes the main region's screen origin down to row 6 (band 6).
    // Opening a dock focuses it, so cross down into the main window.
    exec_lua(&rpc, "nx.dock.open{ side = 'top', size = 5 }").await;
    feed(&rpc, "<C-w><C-w>j");
    nxvim_test_harness::barrier(&rpc).await;

    // Put the cursor low in the main region so the popup — and its tall docs sidebar —
    // opens near the bottom, where an unclamped height would overrun the editor.
    feed(&rpc, "i");
    for _ in 0..14 {
        feed(&rpc, "<CR>");
    }
    feed(&rpc, "al");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");

    let map = poll_menu_top_with_docs(&rpc, &mut incoming)
        .await
        .expect("docs sidebar appears");
    let docs = docs_of(&menu_of(&map));
    let docs_row = mu64(&docs, "row");
    let docs_height = mu64(&docs, "height");
    // The main region begins one row past the 5-row top dock (band 6). The editor is
    // 24 rows, so the region-relative bottom edge is 24 − 6 = 18; the sidebar's rows
    // must stay within it.
    let region_bottom = 24 - (5 + 1);
    assert!(
        docs_row + docs_height <= region_bottom,
        "docs sidebar must stay within the editor's bottom edge: row {docs_row} + height {docs_height} > {region_bottom}"
    );
}

/// The sidebar's geometry is region-relative, but its wheel hit-test box is stashed
/// in GLOBAL cells — so with a dock shifting the region's screen origin, a wheel
/// over the sidebar's on-screen cells must still scroll it. Regression: the box was
/// stashed in region cells, so a wheel past a dock missed it.
#[tokio::test]
async fn wheeling_the_docs_sidebar_hit_tests_in_global_cells_past_a_dock() {
    let dir = temp_dir("complete_docs_scroll_dock");
    let (rpc, mut incoming) = start(&dir, TALL_DOC_INIT).await;
    // Drop the gutter and the tabline so the main region's screen origin is just the
    // dock band: col 21 (a 20-col dock + its separator), row 0.
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber showtabline=0").await;

    exec_lua(&rpc, "nx.dock.open{ side = 'left', size = 20 }").await;
    feed(&rpc, "<C-w><C-w>l");
    nxvim_test_harness::barrier(&rpc).await;

    feed(&rpc, "ial");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let (row, col, top) = poll_docs_box(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) == Some("doc line 01")
    })
    .await
    .expect("docs sidebar opens at the top");
    assert_eq!(top.first().map(String::as_str), Some("doc line 01"));

    // Wheel over the sidebar at GLOBAL cells: the main region's origin (col 21) plus
    // the region-relative box column. Before the fix the box was stashed in region
    // cells, so this landed off it and nothing scrolled.
    let region_x = 20 + 1; // the dock content width + its separator
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "down", row + 1, region_x + col + 1);
    }
    let (_, _, scrolled) = poll_docs_box(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) != Some("doc line 01")
    })
    .await
    .expect("the wheel scrolled the docs sidebar past the dock");
    assert_eq!(
        scrolled.first().map(String::as_str),
        Some("doc line 04"),
        "three notches over the global box advanced the docs by three lines: {scrolled:?}"
    );
}

#[tokio::test]
async fn example_mouse_widgets_config_loads_and_completion_is_clickable() {
    // The shipped examples/mouse-widgets config must load (mouse=a + the four widget
    // setups) and its completion popup must be mouse-clickable end-to-end.
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/mouse-widgets")
        .canonicalize()
        .expect("examples/mouse-widgets dir");
    let init = ServerInit {
        config_dir: Some(example.clone()),
        runtimepath: vec![example],
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    // Seed a word, then type a matching prefix — the example's `nx.complete` (buffer
    // source) opens a popup; `mouse=a` from the config lets the click land.
    feed(&rpc, "ifunction fun");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("the example opens a popup"),
    );
    assert_eq!(menu_items(&menu), vec!["function"]);
    let col = menu_col(&menu) as usize;
    let row0 = menu_row(&menu) as usize;

    // Click the row to select it, then click it again to accept — the prefix `fun`
    // is replaced by `function`, proving the example's widget is core-mouse-driven.
    feed_mouse(&rpc, "left", "press", row0, col);
    feed_mouse(&rpc, "left", "press", row0, col);
    assert_eq!(lines(&rpc).await, vec!["function function"]);
}
