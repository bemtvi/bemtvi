//! Behavior tests for the native snippet engine (`nx.snippet`) — the LSP-syntax
//! parser, the tabstop session (`<Tab>` / `<S-Tab>` navigation), mirrored
//! placeholders, and the `snippets` completion source.
//!
//! Black-box like the rest: a real server sources an `init.lua`, snippets are
//! driven over the same msgpack-RPC a UI uses, and assertions are on the resulting
//! buffer lines and cursor after expansion / navigation / accept.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, cursor, drain_to_latest_redraw, exec_lua, feed, lines, map_get, spawn, temp_dir,
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

/// Expand a body via `nx.snippet.expand` and let the effect drain run.
async fn expand(rpc: &Rpc, body: &str) {
    // `body` is embedded in a `[[ ]]` long string, so it needs no escaping.
    exec_lua(rpc, &format!("nx.snippet.expand([[{body}]])")).await;
}

#[tokio::test]
async fn expand_places_and_tabs_through_tabstops() {
    let dir = temp_dir("snippet-tabs");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    expand(&rpc, "foo($1, $2)$0").await;
    // The literal text lands with placeholders empty; the cursor sits at $1.
    assert_eq!(lines(&rpc).await, vec!["foo(, )".to_string()]);
    assert_eq!(cursor(&rpc).await, (1, 4));

    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["foo(x, )".to_string()]);

    // <Tab> jumps to $2 (just before the close paren).
    feed(&rpc, "<Tab>");
    assert_eq!(cursor(&rpc).await, (1, 7));
    feed(&rpc, "y");
    assert_eq!(lines(&rpc).await, vec!["foo(x, y)".to_string()]);

    // <S-Tab> jumps back to $1, re-selecting its now-filled content `x` (Select
    // mode), so the cursor sits ON the `x` (col 4) ready to retype it.
    feed(&rpc, "<S-Tab>");
    assert_eq!(cursor(&rpc).await, (1, 4));
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_get_mode().mode").await,
        Value::from("s")
    );

    // <Tab> to $2, then to the final $0 (after the close paren), ending the session.
    feed(&rpc, "<Tab><Tab>");
    assert_eq!(cursor(&rpc).await, (1, 9));
}

#[tokio::test]
async fn placeholder_default_and_mirror_sync() {
    let dir = temp_dir("snippet-mirror");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // ${1:v} provides a default that a bare $1 mirrors.
    expand(&rpc, "${1:v}=$1").await;
    assert_eq!(lines(&rpc).await, vec!["v=v".to_string()]);
    // The default is SELECTED (Select mode), so the first keystroke replaces it —
    // `v` becomes `x` (not `vx`) — and the mirror follows in lockstep.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["x=x".to_string()]);
    // After the replace we're in ordinary Insert, so further typing appends.
    feed(&rpc, "y");
    assert_eq!(lines(&rpc).await, vec!["xy=xy".to_string()]);
}

#[tokio::test]
async fn two_defaulted_mirrors_sync_across_multiple_keystrokes() {
    let dir = temp_dir("snippet-two-mirrors");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // The examples/snippets `loc` snippet: BOTH occurrences carry a default. The
    // first keystroke replaces the selected default; subsequent ones append. Through
    // it all the mirror must stay in lockstep — an earlier bug left the mirror extmark
    // degenerate after the first sync, so a later keystroke duplicated/garbled it.
    expand(&rpc, "local ${1:x} = ${1:x}").await;
    assert_eq!(lines(&rpc).await, vec!["local x = x".to_string()]);

    // `y` replaces the selected `x` default (both occurrences).
    feed(&rpc, "y");
    assert_eq!(lines(&rpc).await, vec!["local y = y".to_string()]);
    // `z` now appends (ordinary Insert) — the mirror sync must not duplicate.
    feed(&rpc, "z");
    assert_eq!(lines(&rpc).await, vec!["local yz = yz".to_string()]);
}

/// Landing on a placeholder enters Select mode over its default, and `<Esc>` keeps
/// the default (continuing in Insert) while `<Tab>` skips to the next stop keeping it.
#[tokio::test]
async fn placeholder_select_mode_esc_keeps_default_tab_skips() {
    let dir = temp_dir("snippet-select-skip");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    expand(&rpc, "fn ${1:name}(${2:args})").await;
    assert_eq!(lines(&rpc).await, vec!["fn name(args)".to_string()]);
    // The first placeholder's default is selected.
    let mode = exec_lua(&rpc, "return vim.api.nvim_get_mode().mode").await;
    assert_eq!(mode, Value::from("s"), "landed in Select mode");

    // <Tab> skips $1 without editing (keeps `name`) and selects $2's default.
    feed(&rpc, "<Tab>");
    assert_eq!(lines(&rpc).await, vec!["fn name(args)".to_string()]);
    // Typing now replaces the selected `args`.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["fn name(x)".to_string()]);
}

/// `<Esc>` on a selected placeholder keeps its default and drops into Insert past it,
/// so the default survives and can still be edited (append), not replaced.
#[tokio::test]
async fn placeholder_esc_keeps_and_appends() {
    let dir = temp_dir("snippet-select-esc");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    expand(&rpc, "print(${1:msg})$0").await;
    assert_eq!(lines(&rpc).await, vec!["print(msg)".to_string()]);
    // Esc keeps the selected `msg` and lands in Insert just past it.
    feed(&rpc, "<Esc>");
    let mode = exec_lua(&rpc, "return vim.api.nvim_get_mode().mode").await;
    assert_eq!(mode, Value::from("i"), "Esc keeps the default in Insert");
    feed(&rpc, "!");
    assert_eq!(lines(&rpc).await, vec!["print(msg!)".to_string()]);
}

#[tokio::test]
async fn multiline_body_reindents_continuation_lines() {
    let dir = temp_dir("snippet-indent");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // Open insert at an indented column so continuation lines inherit the indent.
    feed(&rpc, "i\t");
    expand(&rpc, "if $1 then\n\t$0\nend").await;
    assert_eq!(
        lines(&rpc).await,
        vec![
            "\tif  then".to_string(),
            "\t\t".to_string(),
            "\tend".to_string(),
        ]
    );
}

/// Poll for the latest redraw carrying a completion `menu` map.
async fn poll_menu(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if drain_to_latest_redraw(incoming, |m| {
            matches!(map_get(m, "menu"), Some(Value::Map(_)))
        })
        .is_some()
        {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn snippets_completion_source_expands_on_accept() {
    let dir = temp_dir("snippet-source");
    // Register a snippet for the no-name buffer's (empty) filetype and enable the
    // `snippets` completion source.
    let init = "nx.snippet.setup{}\n\
        nx.snippet.add('', { { trigger = 'fn', body = 'function $1()$0 end' } })\n\
        nx.complete.setup { sources = { { 'snippets' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "ifn");
    assert!(poll_menu(&rpc, &mut incoming).await, "snippet menu opened");

    // Select the row and accept: the trigger word is replaced by the expanded body.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["function () end".to_string()]);
}

/// Poll for the latest redraw carrying a completion `menu` map, returning the map.
async fn poll_menu_map(
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

/// The `menu.kinds` array (per-row kind labels), or empty when the key is absent.
fn menu_kinds(map: &[(Value, Value)]) -> Vec<Option<String>> {
    let Some(Value::Map(menu)) = map_get(map, "menu") else {
        panic!("expected a menu map");
    };
    match map_get(menu, "kinds") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// A `snippets`-source row carries the kind label `"Snippet"` in the projected
/// `menu.kinds` array, so the popup can distinguish it from a buffer word / LSP item.
#[tokio::test]
async fn snippet_row_projects_snippet_kind() {
    let dir = temp_dir("snippet-kind");
    let init = "nx.snippet.setup{}\n\
        nx.snippet.add('', { { trigger = 'fn', body = 'function $1()$0 end' } })\n\
        nx.complete.setup { sources = { { 'snippets' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    // A short trigger (`fn`) — the popup must still be sized wide enough to fit
    // `label + " Snippet"`, so the client can render the kind column (a too-narrow box
    // is what hid the kind for the short `loc` trigger in examples/snippets).
    feed(&rpc, "ifn");
    let map = poll_menu_map(&rpc, &mut incoming)
        .await
        .expect("snippet menu opened");
    let menu = menu_of(&map);
    let kinds = menu_kinds(&map);
    assert!(
        kinds.iter().any(|k| k.as_deref() == Some("Snippet")),
        "the snippet row must carry the \"Snippet\" kind, got {kinds:?}"
    );
    let width = map_get(&menu, "width").and_then(Value::as_u64).unwrap();
    assert!(
        width >= "fn".len() as u64 + " Snippet".len() as u64,
        "the popup must be wide enough for label + kind (got width {width})"
    );
}

/// The `menu` submap of a redraw (panics if absent), for width/geometry assertions.
fn menu_of(map: &[(Value, Value)]) -> Vec<(Value, Value)> {
    match map_get(map, "menu") {
        Some(Value::Map(m)) => m.clone(),
        other => panic!("expected a menu map, got {other:?}"),
    }
}

/// The menu's visible row labels, in order.
fn menu_items(map: &[(Value, Value)]) -> Vec<String> {
    let menu = menu_of(map);
    match map_get(&menu, "items") {
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

/// A choice tabstop (`${1|a,b,c|}`) opens a dropdown of its alternatives; navigating
/// and accepting replaces the value (and mirrors follow), rather than treating the
/// first alternative as a plain type-over default.
#[tokio::test]
async fn choice_tabstop_dropdown_pick_replaces_and_mirrors() {
    let dir = temp_dir("snippet-choice");
    let (rpc, mut incoming) = start(&dir, "nx.snippet.setup{}").await;

    // Two mirrored choice occurrences: the first alternative renders as the value.
    expand(&rpc, "${1|a,b,c|}-${1|a,b,c|}").await;
    assert_eq!(lines(&rpc).await, vec!["a-a".to_string()]);

    // A dropdown of the alternatives is open.
    let map = poll_menu_map(&rpc, &mut incoming)
        .await
        .expect("choice dropdown opens");
    assert_eq!(menu_items(&map), vec!["a", "b", "c"]);

    // Navigate to `c` and accept — both occurrences become `c`.
    feed(&rpc, "<C-n><C-n>");
    feed(&rpc, "<C-y>");
    assert_eq!(lines(&rpc).await, vec!["c-c".to_string()]);
}

/// `<Tab>` off a choice dropdown keeps the current value and jumps to the next stop.
#[tokio::test]
async fn choice_tabstop_tab_keeps_value_and_jumps() {
    let dir = temp_dir("snippet-choice-tab");
    let (rpc, mut incoming) = start(&dir, "nx.snippet.setup{}").await;

    expand(&rpc, "x=${1|a,b,c|};$0").await;
    assert_eq!(lines(&rpc).await, vec!["x=a;".to_string()]);
    assert!(poll_menu(&rpc, &mut incoming).await, "dropdown opens");

    // Tab keeps `a` and jumps to $0 (after the `;`), ending the session there.
    feed(&rpc, "<Tab>");
    assert_eq!(lines(&rpc).await, vec!["x=a;".to_string()]);
    assert_eq!(cursor(&rpc).await, (1, 4));
    // Typing now lands after the `;` (the $0 stop), not back in the choice.
    feed(&rpc, "z");
    assert_eq!(lines(&rpc).await, vec!["x=a;z".to_string()]);
}

/// Poll for the latest redraw whose windows include the completion docs float
/// (`[CompletionDocs]`), returning its stripped-markdown lines.
async fn poll_docs(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<String>> {
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
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| docs_window(m).is_some()) {
            let win = docs_window(&map)?;
            return match map_get(&win, "lines") {
                Some(Value::Array(lines)) => Some(
                    lines
                        .iter()
                        .map(|l| l.as_str().unwrap_or("").to_string())
                        .collect(),
                ),
                _ => Some(Vec::new()),
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

/// Selecting a `snippets` row opens the completion docs float previewing the
/// snippet's **body** (the template, tabstops shown) — the "function docs" surface
/// LSP rows already use, now populated for snippets via the row's inline `doc`.
#[tokio::test]
async fn selecting_a_snippet_row_previews_its_body_in_the_docs_float() {
    let dir = temp_dir("snippet-preview");
    let init = "nx.snippet.setup{}\n\
        nx.snippet.add('', { { trigger = 'fn', body = 'function $1()$0 end' } })\n\
        nx.complete.setup { sources = { { 'snippets' } } }";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, "ifn");
    assert!(poll_menu(&rpc, &mut incoming).await, "snippet menu opened");
    // Highlight the snippet row so its docs float opens (the popup is noselect until
    // navigated).
    feed(&rpc, "<C-n>");
    let docs = poll_docs(&rpc, &mut incoming)
        .await
        .expect("docs float opens for the selected snippet");
    assert!(
        docs.iter().any(|l| l.contains("function $1()$0 end")),
        "the docs float previews the snippet body: {docs:?}"
    );
}

#[tokio::test]
async fn unsupported_construct_errors_loud() {
    let dir = temp_dir("snippet-unsupported");
    let (rpc, _incoming) = start(&dir, "nx.snippet.setup{}").await;

    // A variable (`$TM_FILENAME`) is unsupported: it must not insert raw text.
    expand(&rpc, "$TM_FILENAME").await;
    assert_eq!(lines(&rpc).await, vec![String::new()]);
}
