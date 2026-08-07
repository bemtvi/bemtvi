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
    attach, drain_to_latest_redraw, exec_lua, feed, feed_mouse, lines, map_get, menu_items,
    menu_of, poll_menu, poll_no_menu, spawn, temp_dir,
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "popup closes on accept"
    );
}

/// The word the cursor sits *inside* must never be offered as a completion of its own
/// prefix. With a single `AN_EXAMPLE` on the line and the caret in the middle (after
/// `AN_EX`), completing `AN_EX` must not suggest `AN_EXAMPLE` — that is the very word
/// being typed. The only buffer word is the one under the cursor, so nothing opens.
#[tokio::test]
async fn does_not_suggest_the_word_under_the_cursor() {
    let dir = temp_dir("complete_word_under_cursor");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, auto = false }",
    )
    .await;

    // A single AN_EXAMPLE, caret parked in the middle (after AN_EX — 5 chars from end).
    feed(&rpc, "iAN_EXAMPLE<Left><Left><Left><Left><Left>");
    exec_lua(&rpc, "nx.complete.trigger()").await;
    // The only buffer word is the one under the cursor: if a popup opens at all it must
    // not offer AN_EXAMPLE (completing a word to itself).
    if let Some(map) = poll_menu(&rpc, &mut incoming).await {
        let menu = menu_of(&map);
        assert!(
            !menu_items(&menu).contains(&"AN_EXAMPLE".to_string()),
            "the word under the cursor must not complete to itself, got {:?}",
            menu_items(&menu)
        );
    }
}

/// A *distinct* occurrence of the same text elsewhere is still a valid suggestion —
/// only the exact instance the cursor sits inside is excluded, not every word with
/// that spelling. Two `AN_EXAMPLE`s: caret in the middle of the second still offers
/// the first.
#[tokio::test]
async fn suggests_a_distinct_occurrence_of_the_word_under_the_cursor() {
    let dir = temp_dir("complete_distinct_occurrence");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, auto = false }",
    )
    .await;

    feed(
        &rpc,
        "iAN_EXAMPLE<CR>AN_EXAMPLE<Left><Left><Left><Left><Left>",
    );
    exec_lua(&rpc, "nx.complete.trigger()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["AN_EXAMPLE"]);
}

/// Accepting a completion while the caret sits in the *middle* of a word replaces the
/// **whole word** by default (`accept = "replace"`), not just the typed prefix: with
/// the caret after `AN_` in `AN_EXAMPLE`, accepting `AN_OTHER` yields `AN_OTHER`, not
/// `AN_OTHEREXAMPLE`.
#[tokio::test]
async fn accept_replaces_the_whole_word_by_default() {
    let dir = temp_dir("complete_accept_replace");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, auto = false }",
    )
    .await;

    // `AN_OTHER` on line 1 is the candidate; type `AN_EXAMPLE` on line 2, caret after
    // `AN_` (7 chars from the end of the 10-char word).
    feed(
        &rpc,
        "iAN_OTHER<CR>AN_EXAMPLE<Left><Left><Left><Left><Left><Left><Left>",
    );
    exec_lua(&rpc, "nx.complete.trigger()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["AN_OTHER"]);
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["AN_OTHER", "AN_OTHER"]);
}

/// `accept = "insert"` keeps the suffix past the cursor — the old behavior: with the
/// caret after `AN_` in `AN_EXAMPLE`, accepting `AN_OTHER` replaces only the `AN_`
/// prefix, leaving `EXAMPLE` → `AN_OTHEREXAMPLE`.
#[tokio::test]
async fn accept_insert_behavior_keeps_the_word_suffix() {
    let dir = temp_dir("complete_accept_insert");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, auto = false, \
         accept = 'insert' }",
    )
    .await;

    feed(
        &rpc,
        "iAN_OTHER<CR>AN_EXAMPLE<Left><Left><Left><Left><Left><Left><Left>",
    );
    exec_lua(&rpc, "nx.complete.trigger()").await;
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["AN_OTHER"]);
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["AN_OTHER", "AN_OTHEREXAMPLE"]);
}

/// `nx.complete.accept{ behavior = … }` is remappable: two keys can accept under
/// different behaviors regardless of the configured default. Here the default is
/// `replace`, but a `<C-j>` mapped to `nx.complete.accept('insert')` keeps the suffix,
/// while a `<C-l>` mapped to `nx.complete.accept('replace')` swaps the whole word.
#[tokio::test]
async fn mapped_accept_keys_choose_behavior() {
    let dir = temp_dir("complete_accept_mapped");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, auto = false }\n\
         nx.keymap.set('i', '<C-j>', function() nx.complete.accept('insert') end)\n\
         nx.keymap.set('i', '<C-l>', function() nx.complete.accept('replace') end)",
    )
    .await;

    // First word (line 2): accept with the insert-mapped key → suffix kept.
    feed(
        &rpc,
        "iAN_OTHER<CR>AN_EXAMPLE<Left><Left><Left><Left><Left><Left><Left>",
    );
    exec_lua(&rpc, "nx.complete.trigger()").await;
    assert_eq!(
        menu_items(&menu_of(
            &poll_menu(&rpc, &mut incoming).await.expect("popup")
        )),
        vec!["AN_OTHER"]
    );
    feed(&rpc, "<C-j>");
    assert_eq!(lines(&rpc).await, vec!["AN_OTHER", "AN_OTHEREXAMPLE"]);

    // Second word (line 3): accept with the replace-mapped key → whole word swapped.
    feed(
        &rpc,
        "<Esc>oAN_EXAMPLE<Left><Left><Left><Left><Left><Left><Left>",
    );
    exec_lua(&rpc, "nx.complete.trigger()").await;
    poll_menu(&rpc, &mut incoming).await.expect("popup reopens");
    feed(&rpc, "<C-l>");
    assert_eq!(
        lines(&rpc).await,
        vec!["AN_OTHER", "AN_OTHEREXAMPLE", "AN_OTHER"]
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
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

/// A manual trigger opens a **session**: with `auto = false` the popup keeps
/// following the prefix as you type (vim's ins-completion narrows its menu the same
/// way) instead of dying on the next keystroke with nothing left to reopen it. The
/// session ends when the popup does — abort it and typing no longer resurrects it.
#[tokio::test]
async fn manual_trigger_follows_the_prefix_while_typing() {
    let dir = temp_dir("complete_manual_sticky");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer' } }, auto = false }",
    )
    .await;

    // Two candidates on their own lines, then a fresh line to complete on. With
    // `auto = false` typing the prefix opens nothing.
    feed(&rpc, "ihello<CR>helper<CR>he");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "auto = false: typing must not open the popup"
    );

    feed(&rpc, "<C-Space>");
    let mut items = menu_items(&menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("the trigger key opens the popup"),
    ));
    items.sort();
    assert_eq!(items, vec!["hello", "helper"]);

    // Typing narrows the SAME session rather than closing it — and the keystrokes
    // still reach the document (the popup never grabs input).
    feed(&rpc, "lp");
    assert_eq!(
        menu_items(&menu_of(
            &poll_menu(&rpc, &mut incoming)
                .await
                .expect("the manual popup follows the typed prefix")
        )),
        vec!["helper"]
    );
    assert_eq!(lines(&rpc).await, vec!["hello", "helper", "help"]);

    // Backspacing back to a shorter prefix widens it again (the manual session keeps
    // bypassing `min_chars`, so a 1-char prefix still completes).
    feed(&rpc, "<BS><BS><BS>");
    let mut items = menu_items(&menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("the session survives deleting back to one char"),
    ));
    items.sort();
    assert_eq!(items, vec!["hello", "helper"]);

    // Aborting ends the session: typing on is plain insert again, no popup.
    feed(&rpc, "<C-e>");
    feed(&rpc, "el");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "an aborted session must not resurrect on the next keystroke"
    );
    assert_eq!(lines(&rpc).await, vec!["hello", "helper", "hel"]);
}

#[tokio::test]
async fn abort_closes_the_popup_and_keeps_the_typed_prefix() {
    let dir = temp_dir("complete_abort");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    feed(&rpc, "ihello he");
    poll_menu(&rpc, &mut incoming).await.expect("popup opens");

    feed(&rpc, "<C-e>");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
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

/// `<CR>` is a **default** confirm key (alongside `<C-y>`), but an *unnavigated*
/// popup must NOT eat the Enter — nothing is selected yet, so `<CR>` inserts a
/// newline (cmp-style `select = false`). You only accept after moving the
/// selection. No `keys` override here: this guards the built-in default.
#[tokio::test]
async fn cr_inserts_a_newline_until_you_navigate() {
    let dir = temp_dir("complete_cr_noselect");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

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

/// The complement: `<CR>` (a default confirm key) accepts the highlighted row once
/// a navigation has activated the selection. No `keys` override — the built-in
/// default.
#[tokio::test]
async fn cr_accepts_once_you_have_navigated() {
    let dir = temp_dir("complete_cr_navigated");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

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

/// `nx.complete.setup { confirm = "first" }` flips the noselect default: a confirm
/// key accepts the TOP row even when nothing has been navigated to (Enter-to-accept).
#[tokio::test]
async fn confirm_first_accepts_the_top_row_unnavigated() {
    let dir = temp_dir("complete_confirm_first");
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, confirm = 'first' }",
    )
    .await;

    // Popup opens with nothing selected — but under `confirm = "first"`, <CR> accepts
    // the first row instead of inserting a newline.
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(
        map_get(&menu, "selected_active").and_then(Value::as_bool),
        Some(false),
        "still noselect — nothing is preselected"
    );
    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["hello hello"]);
}

/// The default (`confirm = "selected"`, unset) is unchanged by the new option: an
/// unnavigated `<CR>` still makes a newline. Guards that the flag defaults off.
#[tokio::test]
async fn confirm_selected_is_the_default_and_keeps_the_newline() {
    let dir = temp_dir("complete_confirm_selected_default");
    // Explicit `confirm = "selected"` — must behave exactly like the built-in default.
    let (rpc, mut incoming) = start(
        &dir,
        "nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, confirm = 'selected' }",
    )
    .await;

    feed(&rpc, "ihello he");
    poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["hello he", ""]);
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
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
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "no auto popup"
    );
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

/// A source registered with `nx.complete.source{}` **after** `nx.complete.setup{}` has
/// already run — and never named in `setup{ sources }` — still joins the live engine
/// and contributes candidates. This is the incremental seam a plugin relies on: it adds
/// completions by registering, without the user re-listing every source up front.
#[tokio::test]
async fn a_source_registered_after_setup_joins_the_live_engine() {
    let dir = temp_dir("complete_incremental_join");
    // Set the engine up with only `buffer` — no async source, so `has_async` starts
    // false and no off-input dispatch is armed.
    let (rpc, mut incoming) = start(&dir, "nx.complete.setup { sources = { { 'buffer' } } }").await;

    // Now register an echo source at *runtime*, after setup. `reconcile` (called from
    // `source{}`) must re-derive the active set — flipping `has_async` on and arming the
    // dispatch — even though the user never touched `setup{}` again.
    exec_lua(
        &rpc,
        "nx.complete.source {\n\
           name = 'late', debounce = 0,\n\
           complete = function(ctx)\n\
             if ctx.prefix ~= '' then ctx.push(ctx.prefix .. '_late') end\n\
           end,\n\
         }",
    )
    .await;

    // Typing dispatches the just-registered source; its candidate lands in the popup.
    feed(&rpc, "iab");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert!(
        menu_items(&menu).contains(&"ab_late".to_string()),
        "the after-setup source contributes: {:?}",
        menu_items(&menu)
    );
}

/// `nx.complete.setup { exclusive = true }` opts out of auto-join: a registered source
/// that is *not* named in `setup{ sources }` stays dormant, restoring tight control for
/// a config that wants exactly its listed sources and nothing a plugin registers.
#[tokio::test]
async fn exclusive_setup_ignores_an_unlisted_registered_source() {
    let dir = temp_dir("complete_exclusive");
    let init = "\
nx.complete.source {\n\
  name = 'echo', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then ctx.push(ctx.prefix .. '_async') end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'buffer', min_chars = 2 } }, exclusive = true }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // `hello` is a buffer word; typing `he` opens the popup on the buffer match alone —
    // the registered-but-unlisted `echo` source does NOT contribute under `exclusive`.
    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert!(
        items.contains(&"hello".to_string()),
        "the listed buffer source still contributes: {items:?}"
    );
    assert!(
        !items.contains(&"he_async".to_string()),
        "the unlisted source is dormant under exclusive: {items:?}"
    );
}

/// Entering Select mode (`nx.win.select_range`, the seam a plugin snippet engine uses
/// to land on a tabstop) closes any open completion popup — otherwise a popup left up
/// from typing would linger and follow the cursor to the next tabstop.
#[tokio::test]
async fn entering_select_mode_closes_the_completion_popup() {
    let dir = temp_dir("complete_select_closes");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    // A buffer word `hello`, then a matching prefix `he` opens the popup.
    feed(&rpc, "ihello he");
    assert!(
        poll_menu(&rpc, &mut incoming).await.is_some(),
        "the completion popup opens"
    );
    // Enter Select mode over `he` (as a tabstop jump would); the popup must close.
    exec_lua(
        &rpc,
        "nx.win.select_range(0, 0, 0, 0, 2, { on_escape = 'insert' })",
    )
    .await;
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "the popup closes when Select mode is entered"
    );
}

/// The merged view is **fuzzy-first**: a clearly better match wins regardless of
/// source. A buffer word that matches the prefix well outranks a low-priority-source
/// row that only scatter-matches it — despite the source's bias.
#[tokio::test]
async fn merge_is_fuzzy_first_a_better_match_beats_source_bias() {
    let dir = temp_dir("complete_blend_fuzzy_first");
    let init = "\
nx.complete.source {\n\
  name = 'snip', priority = 5, debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then ctx.push { text = 'supreme' } end end,\n\
}\n\
nx.complete.setup { sources = { { 'buffer', min_chars = 2 }, { 'snip' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // `prefix` is a buffer word (a strong prefix match for `pre`); the snippet source
    // pushes `supreme` (a weak scattered match). The strong match wins despite the bias.
    feed(&rpc, "iprefix pre");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert!(
        items.contains(&"supreme".to_string()),
        "the source row is present: {items:?}"
    );
    assert_eq!(
        items[0], "prefix",
        "the better fuzzy match (buffer) outranks the biased weak match: {items:?}"
    );
}

/// Among **equally-good** matches, the source bias breaks the tie: a snippet-source
/// row (bias 5) edges out a buffer word (bias 0) with the same fuzzy score.
#[tokio::test]
async fn merge_source_bias_breaks_a_tie() {
    let dir = temp_dir("complete_blend_tiebreak");
    let init = "\
nx.complete.source {\n\
  name = 'snip', priority = 5, debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then ctx.push { text = 'copy' } end end,\n\
}\n\
nx.complete.setup { sources = { { 'buffer', min_chars = 2 }, { 'snip' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Both `count` (buffer) and `copy` (snippet) are clean prefix matches for `co` — the
    // same fuzzy score — so the snippet's bias tips it to the top.
    feed(&rpc, "icount co");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let items = menu_items(&menu);
    assert!(
        items.contains(&"count".to_string()) && items.contains(&"copy".to_string()),
        "both rows are present: {items:?}"
    );
    assert_eq!(
        items[0], "copy",
        "the equally-good snippet row edges out the buffer word via its bias: {items:?}"
    );
}

/// The kind labels align in one column: the server projects `kind_col` just past the
/// widest label, and sizes the box to `widest_label + gap + widest_kind`.
#[tokio::test]
async fn kinds_align_in_a_single_column() {
    let dir = temp_dir("complete_kind_align");
    let init = "\
nx.complete.source {\n\
  name = 'k', debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then\n\
    ctx.push { text = 'ab', kind = 'Snippet' }\n\
    ctx.push { text = 'abcdefgh', kind = 'X' }\n\
  end end,\n\
}\n\
nx.complete.setup { sources = { { 'k' } }, min_chars = 1 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    // Kinds start just past the widest label (`abcdefgh` = 8) → column 9, for every row.
    assert_eq!(
        map_get(&menu, "kind_col").and_then(Value::as_u64),
        Some(9),
        "kind_col aligns past the widest label"
    );
    // The box holds widest label + a gap + widest kind (`Snippet` = 7): 8 + 1 + 7 = 16.
    assert_eq!(map_get(&menu, "width").and_then(Value::as_u64), Some(16));
}

/// A candidate too long for the remaining width must not push the aligned kind column
/// off the screen. The projected `width` is the box's **content**; the client draws a
/// one-cell border each side (and shifts the top-borderless completion popup one cell
/// left so its left border doesn't sit on the word), then clamps the whole box to the
/// window. Regression: the content was sized to the *full* remaining text width, so the
/// clamp ate the last column and the kind label lost its final character
/// (`Function` → `Functio`).
#[tokio::test]
async fn a_too_long_candidate_keeps_the_kind_column_on_screen() {
    let dir = temp_dir("complete_kind_clamped");
    let init = "\
nx.complete.source {\n\
  name = 'k', debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then\n\
    ctx.push { text = 'abshort', kind = 'Snippet' }\n\
    ctx.push { text = 'ab' .. string.rep('x', 120), kind = 'Function' }\n\
  end end,\n\
}\n\
nx.complete.setup { sources = { { 'k' } }, min_chars = 1 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Indent so the popup is anchored mid-line and its left border has a cell to sit in.
    feed(&rpc, "i        ab");
    let map = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    let menu = menu_of(&map);
    let win = focused_window(&map);
    // The region the box is bounded by — the same quantity `redraw()` passes as
    // `text_width` (the window minus its number gutter).
    let text_width = win_rect(&win, "width") - mu64(&win, "number_width");

    let col = mu64(&menu, "col");
    let width = mu64(&menu, "width");
    // The completion popup omits its top border and shifts one cell left, so its box
    // spans `[col - 1, col - 1 + width + 2)`; every column of it must be on screen.
    let shift = col.min(1);
    assert!(
        col - shift + width + 2 <= text_width,
        "the bordered box fits the text area: col {col} + width {width} + border \
         vs text_width {text_width}"
    );
    // ...and within that box the widest kind (`Function` = 8) still has its own column.
    let kind_col = map_get(&menu, "kind_col")
        .and_then(Value::as_u64)
        .expect("the popup projects an aligned kind column");
    assert!(
        kind_col + 8 <= width,
        "the widest kind fits after kind_col {kind_col} in a {width}-wide box"
    );
}

/// One outlier candidate must not stretch the popup across the whole window. The box
/// is sized to its **widest** row, so a single 200-column label — a generated
/// identifier, a word scanned out of a minified line — used to blow it out to every
/// column the window had left, and the short rows were then read against a box built
/// for the one row they aren't. `'pummaxwidth'` caps it; the outlier elides.
#[tokio::test]
async fn a_huge_candidate_does_not_stretch_the_popup_across_the_window() {
    let dir = temp_dir("complete_pummaxwidth");
    let init = "\
nx.complete.source {\n\
  name = 'k', debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then\n\
    ctx.push { text = 'abshort', kind = 'Snippet' }\n\
    ctx.push { text = 'ab' .. string.rep('x', 200), kind = 'Function' }\n\
  end end,\n\
}\n\
nx.complete.setup { sources = { { 'k' } }, min_chars = 1 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    let map = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    let width = mu64(&menu_of(&map), "width");
    let text_width = {
        let win = focused_window(&map);
        win_rect(&win, "width") - mu64(&win, "number_width")
    };
    assert!(
        width <= 50,
        "the popup is capped at the default 'pummaxwidth' (50), got {width}"
    );

    // ...and it is the *cap* doing it, not the window edge: with the cap off the box
    // claims everything the window has left, which is the shape being fixed.
    exec_lua(&rpc, "nx.o.pummaxwidth = 0").await;
    let uncapped = mu64(
        &menu_of(&poll_menu(&rpc, &mut incoming).await.unwrap()),
        "width",
    );
    assert!(
        uncapped > 50 && uncapped + 2 >= text_width,
        "uncapped, the same popup spans the window ({uncapped} of {text_width})"
    );
}

/// The cap is a *maximum*, not a width: a popup whose rows are all short keeps its
/// snug box (and its aligned kind column) rather than being padded out to it.
#[tokio::test]
async fn short_candidates_are_not_padded_out_to_the_cap() {
    let dir = temp_dir("complete_pummaxwidth_short");
    let init = "\
nx.complete.source {\n\
  name = 'k', debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then\n\
    ctx.push { text = 'ab', kind = 'Snippet' }\n\
    ctx.push { text = 'abcdefgh', kind = 'X' }\n\
  end end,\n\
}\n\
nx.complete.setup { sources = { { 'k' } }, min_chars = 1 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    // Widest label (8) + gap + widest kind (`Snippet` = 7) — unchanged by the cap.
    assert_eq!(map_get(&menu, "width").and_then(Value::as_u64), Some(16));
}

/// A `buffer`-source word carries the `Text` kind in the popup's kind column.
#[tokio::test]
async fn buffer_words_carry_the_text_kind() {
    let dir = temp_dir("complete_buffer_text_kind");
    let (rpc, mut incoming) = start(&dir, BUFFER_INIT).await;

    feed(&rpc, "ihello he");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    let kinds = menu_kinds(&menu);
    assert!(
        !kinds.is_empty() && kinds.iter().all(|k| k.as_deref() == Some("Text")),
        "every buffer row shows the Text kind: {kinds:?}"
    );
}

/// `nx.complete.choice(items, { range })` opens a non-grabbing cursor dropdown; a pick
/// splices the chosen alternative over the range (the seam a plugin snippet engine's
/// `${1|a,b,c|}` choice tabstop rides). It is NOT the grabbing `nx.ui.select`.
#[tokio::test]
async fn choice_api_opens_nongrabbing_popup_and_replaces_the_range() {
    let dir = temp_dir("complete_choice_api");
    let (rpc, mut incoming) = start(&dir, "nx.complete.setup{}").await;

    // A line `x = a`; open a choice over the `a` (cols 4..5) offering a/b/c.
    feed(&rpc, "ix = a");
    exec_lua(
        &rpc,
        "nx.complete.choice({ 'a', 'b', 'c' }, { range = { 0, 4, 0, 5 } })",
    )
    .await;
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("choice popup opens"),
    );
    assert_eq!(menu_items(&menu), vec!["a", "b", "c"]);
    // The popup is non-grabbing (a completion popup): typing/editing still flows to the
    // buffer, and the document holds the seeded text while it's open.
    assert_eq!(lines(&rpc).await, vec!["x = a"]);

    // Preselected on the current value `a`; <C-n> moves to `b`, <C-y> accepts — the pick
    // replaces the range.
    feed(&rpc, "<C-n><C-y>");
    assert_eq!(lines(&rpc).await, vec!["x = b"]);
}

/// The menu's per-row kind labels (the `kinds` array), parallel to [`menu_items`];
/// empty when the key is absent (no row carries a kind).
fn menu_kinds(menu: &[(Value, Value)]) -> Vec<Option<String>> {
    match map_get(menu, "kinds") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// A `nx.complete.source` item's `kind` field rides the push wire and projects onto
/// the row's kind column (`menu.kinds`), the same surface the native `lsp`/`snippets`
/// sources use — so a plugin source can label its rows (`"Module"`, `"Function"`, …).
#[tokio::test]
async fn async_source_item_kind_projects_to_the_menu() {
    let dir = temp_dir("complete_async_kind");
    // A source that pushes a table item carrying an explicit `kind`.
    let init = "\
nx.complete.source {\n\
  name = 'kinded', debounce = 0,\n\
  complete = function(ctx)\n\
    if ctx.prefix ~= '' then ctx.push { text = ctx.prefix .. '_mod', kind = 'Module' } end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'kinded' } }, min_chars = 2 }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "iab");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup opens"));
    assert_eq!(menu_items(&menu), vec!["ab_mod"]);
    assert_eq!(
        menu_kinds(&menu),
        vec![Some("Module".to_string())],
        "the source item's kind reaches the row's kind column"
    );
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "a source that streams nothing leaves no popup open"
    );
    assert_eq!(lines(&rpc).await, vec!["ab"]);
}

#[tokio::test]
async fn per_source_min_chars_gates_each_source_independently() {
    let dir = temp_dir("complete_per_source_min");
    // Buffer at min_chars=3, an async echo at min_chars=2. The two thresholds are
    // honored *independently*: at 2 chars only the async row shows (buffer gated);
    // at 3 the buffer word joins. Proves per-source min_chars (not the old single
    // global gate that only read the `buffer` source).
    let init = "\
nx.complete.source {\n\
  name = 'echo', debounce = 0,\n\
  complete = function(ctx) if ctx.prefix ~= '' then ctx.push(ctx.prefix .. '_async') end end,\n\
}\n\
nx.complete.setup { sources = { { 'buffer', min_chars = 3 }, { 'echo', min_chars = 2 } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // Seed a buffer word, then complete on a fresh line below it.
    feed(&rpc, "iheythere<Esc>o");

    // Two chars: the echo source (min 2) fires; the buffer word (min 3) is gated out —
    // the popup opens for the lower-threshold source alone.
    feed(&rpc, "he");
    let menu = menu_of(
        &poll_menu(&rpc, &mut incoming)
            .await
            .expect("popup opens at 2"),
    );
    let items = menu_items(&menu);
    assert!(
        items.contains(&"he_async".to_string()),
        "async source (min 2) fires at 2 chars: {items:?}"
    );
    assert!(
        !items.iter().any(|i| i == "heythere"),
        "buffer source (min 3) is gated at 2 chars: {items:?}"
    );

    // Third char: the buffer source now meets its own gate and contributes the word.
    feed(&rpc, "y");
    let menu = menu_of(&poll_menu(&rpc, &mut incoming).await.expect("popup at 3"));
    let items = menu_items(&menu);
    assert!(
        items.contains(&"heythere".to_string()),
        "buffer word appears once the prefix reaches min 3: {items:?}"
    );
    assert!(
        items.contains(&"hey_async".to_string()),
        "async source still present at 3: {items:?}"
    );
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

/// The `[CompletionDocs]` doc-float **window** map from a top-level redraw, or `None`.
/// The completion docs are a real float window now (not a `menu.docs` overlay), so
/// tests read them out of the `windows` array by the scratch buffer's name.
fn docs_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return None;
    };
    wins.iter().find_map(|w| match w {
        Value::Map(wm)
            if map_get(wm, "file_name").and_then(Value::as_str) == Some("[CompletionDocs]") =>
        {
            Some(wm.clone())
        }
        _ => None,
    })
}

/// A window map's visible `lines` (the viewport slice — so it reflects a scroll).
fn win_lines(win: &[(Value, Value)]) -> Vec<String> {
    match map_get(win, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|l| l.as_str().unwrap_or("").to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// A window map's `rect` field (`x`/`y`/`width`/`height`) — the OUTER box, region cells.
fn win_rect(win: &[(Value, Value)], key: &str) -> u64 {
    match map_get(win, "rect") {
        Some(Value::Map(r)) => map_get(r, key)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("rect has a {key}")),
        other => panic!("window has a rect map, got {other:?}"),
    }
}

/// Poll for the latest redraw carrying the completion docs float window whose visible
/// lines satisfy `want`, returning that window map.
async fn poll_docs_win(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: impl Fn(&[String]) -> bool,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            docs_window(m).is_some_and(|w| want(&win_lines(&w)))
        }) {
            if let Some(w) = docs_window(&map) {
                return Some(w);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// The completion docs float's lines of the latest redraw carrying it.
async fn poll_docs_lines(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    poll_docs_win(rpc, incoming, |_| true)
        .await
        .map(|w| win_lines(&w))
}

/// A source that pushes a **fenced code block** as its `doc` (the shape a plugin
/// snippet engine uses to preview a snippet body) renders that body in the docs float
/// when the row is selected — the "function docs" surface, for snippet rows.
#[tokio::test]
async fn async_source_fenced_body_doc_previews_in_the_float() {
    let dir = temp_dir("complete_fenced_doc");
    let init = "\
nx.complete.source {\n\
  name = 'snip', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('logg'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'logg', doc = '```lua\\nLOGBODY($1)$0\\n```' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'snip' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "ilog");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let lines = poll_docs_lines(&rpc, &mut incoming)
        .await
        .expect("docs float appears");
    assert!(
        lines.iter().any(|l| l.contains("LOGBODY($1)$0")),
        "the fenced snippet body renders in the docs float: {lines:?}"
    );
}

/// Inline `code` in the docs float renders with the colorscheme's `@markup.raw` style
/// (a resolved style id on the span), so a docstring's code stands out from prose — the
/// end-to-end proof of the "code isn't visible" fix (the renderer emits the span, the
/// colorscheme styles it).
#[tokio::test]
async fn inline_code_in_the_docs_float_is_styled_under_a_colorscheme() {
    let dir = temp_dir("complete_docs_code_style");
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'call `foo()` now' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } } }\n\
vim.cmd('colorscheme nxvim')";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.iter().any(|l| l.contains("foo()"))
    })
    .await
    .expect("docs float appears");
    // Find the `@markup.raw` span in the docs window highlights; its 4th field (the
    // resolved style id) must be non-nil — i.e. the colorscheme actually coloured it.
    let Some(Value::Array(hl)) = map_get(&win, "highlights") else {
        panic!("docs window has highlights");
    };
    let styled_raw = hl
        .iter()
        .flat_map(|row| match row {
            Value::Array(spans) => spans.clone(),
            _ => Vec::new(),
        })
        .any(|span| match span {
            Value::Array(f) => {
                f.get(2).and_then(Value::as_str) == Some("@markup.raw")
                    && f.get(3).is_some_and(|s| !matches!(s, Value::Nil))
            }
            _ => false,
        });
    assert!(
        styled_raw,
        "inline code carries a styled @markup.raw span: {hl:?}"
    );
}

/// A fenced code block in the docs float renders its body with the fences stripped (a
/// block with a language additionally gets per-language syntax colouring, covered by the
/// hover tests where a grammar is loaded).
#[tokio::test]
async fn code_blocks_in_the_docs_float_drop_their_fences() {
    let dir = temp_dir("complete_docs_codeblock");
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'run:\\n\\n```\\nmake build\\n```' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.iter().any(|l| l.contains("make build"))
    })
    .await
    .expect("docs float appears");
    let lines = win_lines(&win);
    assert!(
        lines.iter().any(|l| l.trim() == "make build") && lines.iter().all(|l| !l.contains("```")),
        "code block body renders without fences: {lines:?}"
    );
}

/// A long paragraph (one reflowed markdown line) that wraps within the float sizes the
/// float to its **wrapped** row count — not one visible body row with the rest clipped.
/// (The buffer holds one line; the window wraps it, and the float height must fit the
/// wrapped rows.)
#[tokio::test]
async fn a_wrapped_paragraph_sizes_the_docs_float_to_its_wrapped_height() {
    let dir = temp_dir("complete_docs_wrap_height");
    // ~250 columns of prose on a single source line — markdown keeps it one paragraph,
    // which wraps to several display rows within the ≤60-col float.
    let para = "word ".repeat(50);
    let init = format!(
        "\
nx.complete.source {{\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push {{ text = 'hello', doc = '{para}' }}\n\
    end\n\
  end,\n\
}}\n\
nx.complete.setup {{ sources = {{ {{ 'doc' }} }} }}"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.iter().any(|l| l.contains("word"))
    })
    .await
    .expect("docs float appears");
    // The float is sized to the wrapped display rows: several body rows are visible, and
    // the outer height (rows + 2 border) is far taller than the 3 a raw-line-count height
    // (1 body + border) would give.
    assert!(
        win_lines(&win).len() > 1,
        "several wrapped body rows are visible, got {:?}",
        win_lines(&win)
    );
    let h = win_rect(&win, "height");
    assert!(
        h >= 5,
        "the float sizes to the wrapped paragraph height (>=5 rows), got {h}"
    );
}

/// `nx.complete.setup { docs_wrap = false }` is accepted (the configurable-wrap knob)
/// and the docs float still opens beside the popup.
#[tokio::test]
async fn docs_wrap_is_configurable() {
    let dir = temp_dir("complete_docs_wrap");
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'docs for hello' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } }, docs_wrap = false }";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let docs = poll_docs_lines(&rpc, &mut incoming)
        .await
        .expect("docs float appears with docs_wrap = false");
    assert!(
        docs.iter().any(|l| l.contains("docs for hello")),
        "{docs:?}"
    );
}

/// The completion docs render in a real **float window** (in `windows[]`, backed by
/// the `[CompletionDocs]` scratch buffer) beside the popup — stripped markdown lines
/// plus `@markup.*` highlight spans, the same wire hover uses. This is the win of the
/// migration off the bespoke text-only `menu.docs` overlay: syntax highlighting for free.
#[tokio::test]
async fn completion_docs_render_in_a_highlighted_float_window() {
    let dir = temp_dir("complete_docs_window");
    let init = "\
nx.complete.source {\n\
  name = 'md', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = '# Heading\\n\\nUses **bold** text.' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'md' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.iter().any(|l| l.trim() == "Heading")
    })
    .await
    .expect("the docs float window appears");
    // A real float window.
    assert_eq!(
        map_get(&win, "floating").and_then(Value::as_bool),
        Some(true)
    );
    // Stripped markdown lines (no raw `#` / `**`).
    let lines = win_lines(&win);
    assert!(
        lines.iter().any(|l| l.trim() == "Heading")
            && lines.iter().any(|l| l.contains("Uses bold text.")),
        "stripped markdown: {lines:?}"
    );
    assert!(
        lines.iter().all(|l| !l.contains('#') && !l.contains("**")),
        "no raw markers: {lines:?}"
    );
    // The `highlights` wire carries the `@markup.*` spans (heading + strong).
    let Some(Value::Array(hl)) = map_get(&win, "highlights") else {
        panic!("docs window has a highlights array");
    };
    let groups: Vec<String> = hl
        .iter()
        .flat_map(|row| match row {
            Value::Array(spans) => spans.clone(),
            _ => Vec::new(),
        })
        .filter_map(|span| match span {
            Value::Array(fields) => fields.get(2).and_then(Value::as_str).map(String::from),
            _ => None,
        })
        .collect();
    assert!(
        groups.iter().any(|g| g.starts_with("@markup.heading"))
            && groups.iter().any(|g| g == "@markup.strong"),
        "docs highlights carry the @markup.* spans: {groups:?}"
    );
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
    let docs = poll_docs_lines(&rpc, &mut incoming)
        .await
        .expect("docs float appears");
    assert!(
        docs.iter().any(|l| l.contains("A smiley face")),
        "the plugin row's inline doc shows in the sidebar: {docs:?}"
    );
}

/// A row's inline `doc` is markdown, so the docs sidebar renders it *stripped* (no
/// `#`, `**`, or backticks) via the shared renderer — the same treatment as hover,
/// text-only here since the sidebar carries no highlight channel.
#[tokio::test]
async fn a_plugin_source_markdown_doc_is_rendered_stripped_in_the_sidebar() {
    let dir = temp_dir("complete_md_docs");
    let init = "\
nx.complete.source {\n\
  name = 'md', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = '# Heading\\n\\nUses **bold** and `code`.' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'md' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let docs = poll_docs_lines(&rpc, &mut incoming)
        .await
        .expect("docs float appears");

    assert!(
        docs.iter().any(|l| l.trim() == "Heading"),
        "heading rendered without '#': {docs:?}"
    );
    assert!(
        docs.iter().any(|l| l.contains("Uses bold and code.")),
        "inline markup stripped: {docs:?}"
    );
    assert!(
        docs.iter()
            .all(|l| !l.contains("**") && !l.contains('#') && !l.contains('`')),
        "no raw markdown markers remain: {docs:?}"
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
    feed(&rpc, "<C-n>"); // select row 0 so the docs float shows

    let win = poll_docs_win(&rpc, &mut incoming, |_| true)
        .await
        .expect("docs float appears");
    // The float's inner content width (its outer rect minus the two border cells) must
    // stay usable — it floats over the whole editor, not the narrow ~40-col split pane.
    let width = win_rect(&win, "width").saturating_sub(2);
    assert!(
        width >= 20,
        "the docs float must keep a usable width over the whole editor after a split, got {width}"
    );
}

/// The latest top-level redraw carrying BOTH the completion popup `menu` and its docs
/// float window (so a test can read the menu geometry, the focused window's gutter
/// widths, and the docs window rect from one consistent frame).
async fn poll_menu_top_with_docs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_))) && docs_window(m).is_some()
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

/// The docs float must butt against the popup's right border — its content one cell
/// past that border. Its region-relative column therefore counts the *whole* gutter the
/// popup box sits behind: the sign column AND the number column. Regression: it counted
/// only the number column, sliding the float `sign_width` cells left of the popup (a
/// visible gap once a sign column shows).
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
    feed(&rpc, "<C-n>"); // select row 0 so the docs float shows

    let map = poll_menu_top_with_docs(&rpc, &mut incoming)
        .await
        .expect("docs float appears");
    let menu = menu_of(&map);
    let docs = docs_window(&map).expect("docs float window");
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
    // The docs float's OUTER box (its rect.x, border included) sits one cell past the
    // popup's right border: the full gutter (sign + number) + the box content span + 1
    // (the popup's right border, then the float's own left border is the outer box). Its
    // content (rect.x + 1) is thus 2 cells past the popup content — flush adjacency.
    assert_eq!(
        win_rect(&docs, "x"),
        sign_width + number_width + menu_col + menu_width + 1,
        "docs float butts against the popup including the sign column"
    );
    // ...and the float itself is a popup, not an editing window: the editing window's
    // gutter options must NOT leak into it. Its own sign column stays collapsed, so the
    // documentation text starts at the float's left edge instead of being inset (and
    // truncated) by two cells of empty gutter.
    assert_eq!(
        mu64(&docs, "sign_width"),
        0,
        "the docs float must not inherit signcolumn=yes from the editing window"
    );
}

/// A float is a popup, not an editing window: it must not inherit the editing window's
/// **gutter and decoration** options. Regression: `open_float_window` cleared only
/// `number` / `relativenumber`, so `set signcolumn=yes` (or a `foldcolumn`) reserved an
/// empty gutter *inside* the completion-docs float, insetting its text and shrinking the
/// width its content had to render into — and `cursorline` / `colorcolumn` painted a
/// highlight bar and a vertical ruler across the popup's body.
#[tokio::test]
async fn docs_float_does_not_inherit_the_gutter_options() {
    let dir = temp_dir("complete_docs_float_gutter");
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

    // Every gutter/decoration option the editing window can carry, all on.
    exec_lua(
        &rpc,
        "vim.cmd[[set signcolumn=yes:2]] vim.cmd[[set foldcolumn=2]] \
         vim.cmd[[set number]] vim.cmd[[set relativenumber]] \
         vim.cmd[[set cursorline]] vim.cmd[[set colorcolumn=4]]",
    )
    .await;

    feed(&rpc, "ihel");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>"); // select row 0 so the docs float shows

    let map = poll_menu_top_with_docs(&rpc, &mut incoming)
        .await
        .expect("docs float appears");
    let docs = docs_window(&map).expect("docs float window");
    assert_eq!(
        mu64(&docs, "sign_width"),
        0,
        "the docs float must not inherit 'signcolumn'"
    );
    assert_eq!(
        mu64(&docs, "number_width"),
        0,
        "the docs float must not inherit 'number'/'relativenumber'"
    );
    assert_eq!(
        mu64(&docs, "foldcolumn_width"),
        0,
        "the docs float must not inherit 'foldcolumn'"
    );
    assert_eq!(
        map_get(&docs, "cursorline").and_then(Value::as_bool),
        Some(false),
        "the docs float must not inherit 'cursorline'"
    );
    assert_eq!(
        map_get(&docs, "colorcolumn"),
        Some(&Value::Array(Vec::new())),
        "the docs float must not inherit 'colorcolumn'"
    );
    // The whole float width is content: the text is flush with the left edge.
    assert_eq!(
        win_lines(&docs).first().map(String::as_str),
        Some("docs for hello"),
        "the docs text renders in full, uninset by a gutter"
    );
}

/// The sidebar is an `editor`-relative float, so its geometry is in windows-area
/// (screen) cells — it must be placed past a left dock's band and still stop at the
/// editor's right edge. Regression: it was computed in the popup's *region* cells,
/// so the box spilled past the right edge by the dock band's width.
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
        .expect("docs float appears");
    let docs = docs_window(&map).expect("docs float window");
    // The docs float's OUTER rect is in windows-area cells. Its right edge
    // (rect.x + rect.width, border included) must stay within the 80-column editor.
    let docs_col = win_rect(&docs, "x");
    let docs_outer_width = win_rect(&docs, "width");
    assert!(
        docs_col + docs_outer_width <= 80,
        "docs float must stay within the editor's right edge: x {docs_col} + width {docs_outer_width} > 80"
    );
    // And it really is in screen cells: the popup it butts against lives in the main
    // region, which starts one cell past the 20-col dock.
    assert!(
        docs_col > 20,
        "the float sits past the dock band it is placed beyond, got x {docs_col}"
    );
    // ...and still be usable, not collapsed to a sliver (inner width past the border).
    assert!(
        docs_outer_width.saturating_sub(2) >= 20,
        "the float keeps a usable width past the dock, got {docs_outer_width}"
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
    let docs = poll_docs_lines(&rpc, &mut incoming)
        .await
        .expect("docs float appears after resolve");
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
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
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "scrolling the text closes the completion popup"
    );
}

#[tokio::test]
async fn wheeling_over_the_completion_docs_sidebar_scrolls_it() {
    let dir = temp_dir("complete_docs_scroll");
    // One row carrying a TALL inline doc (more lines than the float's 12-row cap), so
    // the docs float window has content to scroll. A **code block** so the 30 lines stay
    // distinct — outside a code fence, markdown collapses the single newlines into one
    // wrapped paragraph.
    let init = "\
nx.complete.source {\n\
  name = 'docs', debounce = 0,\n\
  complete = function(ctx)\n\
    local d = {}\n\
    for i = 1, 30 do d[i] = string.format('doc line %02d', i) end\n\
    ctx.push { text = 'alpha', doc = '```\\n' .. table.concat(d, '\\n') .. '\\n```' }\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'docs' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;
    // Drop the gutter so the docs float's region cells are the global screen cells.
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, "ial");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    // Select row 0 so the docs float shows (it renders only for an active row).
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) == Some("doc line 01")
    })
    .await
    .expect("docs float opens at the top");
    // The float is a real window now — a wheel over it scrolls it natively (no bespoke
    // hit-test). Feed the wheel inside its box (past the border).
    let (rx, ry) = (win_rect(&win, "x") as usize, win_rect(&win, "y") as usize);

    // Three wheel-down notches over the docs window scroll it down three lines — the
    // wheel acts on the docs, NOT the highlight (which stays on row 0).
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "down", ry + 1, rx + 1);
    }
    let scrolled = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) != Some("doc line 01")
    })
    .await
    .expect("the wheel scrolled the docs window");
    assert_eq!(
        win_lines(&scrolled).first().map(String::as_str),
        Some("doc line 10"),
        "three wheel-down notches advanced the docs by 3×3 lines (mousescroll ver:3): {:?}",
        win_lines(&scrolled)
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

    // Wheeling back up returns to the top (clamped at line 01).
    for _ in 0..5 {
        feed_mouse(&rpc, "wheel", "up", ry + 1, rx + 1);
    }
    let back = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) == Some("doc line 01")
    })
    .await
    .expect("the docs scrolled back to the top");
    assert_eq!(
        win_lines(&back).first().map(String::as_str),
        Some("doc line 01")
    );
}

/// With `docs_wrap` on (the default) a wide doc line wraps within the float, so the
/// float must NOT scroll horizontally — a `<S-ScrollWheel>` / horizontal wheel over it
/// is a no-op (vim disables horizontal scroll under `wrap`). Regression: it scrolled.
#[tokio::test]
async fn a_wrapped_docs_float_does_not_scroll_horizontally() {
    let dir = temp_dir("complete_docs_nohscroll");
    // A single doc line far wider than the ~60-col float (100 cols of a digit run).
    let wide = "0123456789".repeat(10);
    let init = format!(
        "\
nx.complete.source {{\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push {{ text = 'hello', doc = '`{wide}`' }}\n\
    end\n\
  end,\n\
}}\n\
nx.complete.setup {{ sources = {{ {{ 'doc' }} }} }}"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.first().is_some_and(|l| l.starts_with("0123456789"))
    })
    .await
    .expect("docs float opens");
    assert_eq!(mu64(&win, "leftcol"), 0, "starts unscrolled");
    let (rx, ry) = (win_rect(&win, "x") as usize, win_rect(&win, "y") as usize);

    // A horizontal wheel over the wrapped float must not scroll it sideways.
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "right", ry + 1, rx + 1);
    }
    nxvim_test_harness::barrier(&rpc).await;
    let win = poll_docs_win(&rpc, &mut incoming, |_| true)
        .await
        .expect("docs float still open");
    assert_eq!(
        mu64(&win, "leftcol"),
        0,
        "a wrapped docs float never scrolls horizontally"
    );
}

/// A 30-line inline doc (a code block, so the lines stay distinct rather than
/// collapsing into one wrapped markdown paragraph), for the dock-aware geometry tests.
const TALL_DOC_INIT: &str = "\
nx.complete.source {\n\
  name = 'docs', debounce = 0,\n\
  complete = function(ctx)\n\
    local d = {}\n\
    for i = 1, 30 do d[i] = string.format('doc line %02d', i) end\n\
    ctx.push { text = 'alpha', doc = '```\\n' .. table.concat(d, '\\n') .. '\\n```' }\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'docs' } } }";

/// The sidebar's height is clamped against the editor's bottom edge in windows-area
/// cells, so a tall doc opened low in a region pushed down by a top dock still fits
/// on screen. Regression: the box was computed in the popup's *region* cells, which
/// (measured against the full editor height) let it run past the bottom edge.
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
        .expect("docs float appears");
    let docs = docs_window(&map).expect("docs float window");
    // The docs float's OUTER rect is in windows-area cells: its bottom edge must stay
    // within the 24-row editor.
    let docs_row = win_rect(&docs, "y");
    let docs_height = win_rect(&docs, "height");
    assert!(
        docs_row + docs_height <= 24,
        "docs float must stay within the editor's bottom edge: y {docs_row} + height {docs_height} > 24"
    );
    // And it really is in screen cells: the popup it is placed beside sits in the main
    // region, which starts one row past the 5-row top dock.
    assert!(
        docs_row > 5,
        "the float sits past the dock band it is placed beyond, got y {docs_row}"
    );
}

/// The docs float is laid out in screen cells while the tree that owns it lays out in
/// its region's — so with a dock shifting that region's origin, the mouse hit-test
/// (which resolves a GLOBAL cell back to a window) must still find the float under the
/// wheel and scroll it (via the native window mouse path, not the retired bespoke stash).
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
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) == Some("doc line 01")
    })
    .await
    .expect("docs float opens at the top");
    // The float rect is already in windows-area cells (it sits past the dock band the
    // main region starts after), so it IS the global cell. Wheel inside the box (past
    // the border).
    let (rx, ry) = (win_rect(&win, "x") as usize, win_rect(&win, "y") as usize);
    assert!(rx > 20, "the float is placed past the dock band");
    for _ in 0..3 {
        feed_mouse(&rpc, "wheel", "down", ry + 1, rx + 1);
    }
    let scrolled = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.first().map(String::as_str) != Some("doc line 01")
    })
    .await
    .expect("the wheel scrolled the docs window past the dock");
    assert_eq!(
        win_lines(&scrolled).first().map(String::as_str),
        Some("doc line 10"),
        "three notches over the global box advanced the docs by 3×3 lines: {:?}",
        win_lines(&scrolled)
    );
}

/// The docs float window's `line_bg` layer as `(row, style_id)` pairs — the per-row
/// line-background (neovim's `line_hl_group`) the client paints full-width under the
/// text. The server only emits a row whose group resolved, so `style_id` is
/// `Some(..)` in practice; `None` guards a malformed entry.
fn win_line_bg(win: &[(Value, Value)]) -> Vec<(u64, Option<u64>)> {
    match map_get(win, "line_bg") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|e| {
                let e = e.as_array()?;
                Some((e.first()?.as_u64()?, e.get(1).and_then(Value::as_u64)))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A fenced code block in the docs float carries a full-width `line_hl_group`
/// background on its body lines (the `@markup.raw.block` region under `:colorscheme
/// nxvim`) — projected as the `line_bg` layer, *not* as a `@markup.raw.block` span in
/// the winner-takes-cell `highlights` merge (the reverted approach that let syntax
/// override it). The surrounding prose carries no background.
#[tokio::test]
async fn a_fenced_code_block_in_the_docs_float_carries_a_line_background() {
    let dir = temp_dir("complete_docs_codeblock_bg");
    let init = "\
nx.complete.source {\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push { text = 'hello', doc = 'run:\\n\\n```\\nmake build\\n```' }\n\
    end\n\
  end,\n\
}\n\
nx.complete.setup { sources = { { 'doc' } } }\n\
vim.cmd('colorscheme nxvim')";
    let (rpc, mut incoming) = start(&dir, init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.iter().any(|l| l.contains("make build"))
    })
    .await
    .expect("docs float appears");
    let vis = win_lines(&win);
    let code_row = vis
        .iter()
        .position(|l| l.trim() == "make build")
        .expect("the code body row is visible") as u64;
    let prose_row = vis
        .iter()
        .position(|l| l.contains("run:"))
        .expect("the prose row is visible") as u64;
    let line_bg = win_line_bg(&win);
    // The fenced body line carries a resolved line background...
    assert!(
        matches!(
            line_bg.iter().find(|(r, _)| *r == code_row),
            Some((_, Some(_)))
        ),
        "the code row {code_row} carries a resolved line_bg, got {line_bg:?}"
    );
    // ...while the prose above it does not — only the fenced region is a code block.
    assert!(
        !line_bg.iter().any(|(r, _)| *r == prose_row),
        "the prose row {prose_row} carries no line_bg, got {line_bg:?}"
    );
    // Regression guard for the reverted fix: the block background is the `line_bg`
    // layer, never a `@markup.raw.block` span in `highlights` (where the winner-takes-
    // cell merge let syntax spans override it). So syntax colouring composes on top.
    if let Some(Value::Array(hl)) = map_get(&win, "highlights") {
        let as_span = hl
            .iter()
            .flat_map(|row| match row {
                Value::Array(spans) => spans.clone(),
                _ => Vec::new(),
            })
            .any(|span| {
                matches!(&span, Value::Array(f)
                    if f.get(2).and_then(Value::as_str) == Some("@markup.raw.block"))
            });
        assert!(
            !as_span,
            "the code-block background must be a line_bg layer, not a highlights span: {hl:?}"
        );
    }
}

/// A code line wider than the docs float **wraps**, and every wrapped display row
/// carries the `line_bg` background — a `line_hl_group` marks the *buffer* line, and
/// the projection walks screen rows, so each continuation row of the code line is
/// backed (not just its first row).
#[tokio::test]
async fn a_wrapped_code_line_backs_every_continuation_row() {
    let dir = temp_dir("complete_docs_codeblock_wrap");
    // One code line far wider than the ≤~60-col docs float, so it wraps to several rows.
    let long = "x".repeat(200);
    let init = format!(
        "\
nx.complete.source {{\n\
  name = 'doc', debounce = 0,\n\
  complete = function(ctx)\n\
    if ('hello'):find(ctx.prefix, 1, true) == 1 then\n\
      ctx.push {{ text = 'hello', doc = '```\\n{long}\\n```' }}\n\
    end\n\
  end,\n\
}}\n\
nx.complete.setup {{ sources = {{ {{ 'doc' }} }} }}\n\
vim.cmd('colorscheme nxvim')"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;
    feed(&rpc, "ihe");
    let _ = poll_menu(&rpc, &mut incoming).await.expect("popup opens");
    feed(&rpc, "<C-n>");
    let win = poll_docs_win(&rpc, &mut incoming, |ls| {
        ls.iter().any(|l| l.contains("xxxx"))
    })
    .await
    .expect("docs float appears");
    let line_bg = win_line_bg(&win);
    // The single code line wraps to ≥2 display rows, each carrying a resolved background.
    assert!(
        line_bg.len() >= 2,
        "the wrapped code line backs every continuation row (>=2), got {line_bg:?}"
    );
    assert!(
        line_bg.iter().all(|(_, s)| s.is_some()),
        "every wrapped code row carries a resolved line_bg, got {line_bg:?}"
    );
}
