//! Behavior tests for `nx.cmdline_complete` — command-line completion, the unified
//! float-list widget's **fifth orchestration** (`docs/specs/2026-06-14-nx-ui-float-widget.md`,
//! `docs/plans/2026-06-16-cmdline-completion.md`). Phase 1: `<Tab>` on an ex command
//! line opens a fuzzy list of matching command names; typing narrows it; `<Esc>`
//! closes the popup but keeps the line.
//!
//! Black-box like the rest: a real server sources an `init.lua` that calls
//! `nx.cmdline_complete.setup{}`, the line is driven over the same msgpack-RPC a UI
//! uses, and the assertions are on the projected `menu` redraw surface (rows). The
//! candidate set is owned by the bundled Lua catalog source; core fuzzy-ranks it
//! against the typed command-name token.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, barrier, drain_latest_redraw, drain_to_latest_redraw, exec_lua, feed, lines, map_get,
    message_of, spawn, temp_dir, wait_redraw,
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

const INIT: &str = "nx.cmdline_complete.setup {}";

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

/// Poll for the latest redraw whose `menu` key is *absent / nil* (no popup), and
/// return that frame so the caller can also assert on it (e.g. `command_mode`).
async fn poll_no_menu(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<(Value, Value)>> {
    for _ in 0..60 {
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            !matches!(map_get(m, "menu"), Some(Value::Map(_)))
        }) {
            return Some(map);
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    None
}

fn menu_items(map: &[(Value, Value)]) -> Vec<String> {
    let menu = match map_get(map, "menu") {
        Some(Value::Map(m)) => m,
        other => panic!("expected a menu map, got {other:?}"),
    };
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

fn command_mode(map: &[(Value, Value)]) -> bool {
    matches!(map_get(map, "command_mode"), Some(Value::Boolean(true)))
}

/// The menu's `(selected_row, selected_active)` — the highlighted index and whether
/// it is an active selection the client highlights (noselect until the first nav).
fn menu_selection(map: &[(Value, Value)]) -> (u64, bool) {
    let menu = match map_get(map, "menu") {
        Some(Value::Map(m)) => m,
        other => panic!("expected a menu map, got {other:?}"),
    };
    let selected = match map_get(menu, "selected") {
        Some(Value::Integer(i)) => i.as_u64().unwrap(),
        other => panic!("expected a selected index, got {other:?}"),
    };
    let active = matches!(map_get(menu, "selected_active"), Some(Value::Boolean(true)));
    (selected, active)
}

/// Predicate for [`wait_redraw`]: the frame carries an open menu whose highlighted
/// row is exactly `(selected, active)`. Robust against the take-latest race — we wait
/// for the *specific* selection a navigation key produces, not just "a menu is open".
fn menu_sel_is(m: &[(Value, Value)], selected: u64, active: bool) -> bool {
    let Some(Value::Map(menu)) = map_get(m, "menu") else {
        return false;
    };
    let sel_ok = matches!(map_get(menu, "selected"), Some(Value::Integer(i)) if i.as_u64() == Some(selected));
    let act_ok =
        matches!(map_get(menu, "selected_active"), Some(Value::Boolean(b)) if *b == active);
    sel_ok && act_ok
}

#[tokio::test]
async fn tab_opens_command_menu() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a completion menu after :e<Tab>");
    let items = menu_items(&map);
    assert!(items.contains(&"edit".to_string()), "items: {items:?}");
    assert!(items.contains(&"enew".to_string()), "items: {items:?}");
    // Fuzzy on `e`: a command with no `e` (no subsequence match) is excluded.
    assert!(!items.contains(&"quit".to_string()), "items: {items:?}");
}

#[tokio::test]
async fn typing_narrows_menu() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":e<Tab>");
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("menu after <Tab>");
    // Type `d` → the line is `:ed`; the open popup narrows live (no second <Tab>).
    feed(&rpc, "d");
    let map = poll_menu(&rpc, &mut incoming).await.expect("narrowed menu");
    let items = menu_items(&map);
    assert!(items.contains(&"edit".to_string()), "items: {items:?}");
    // `enew` has no `d`, so the `ed` subsequence drops it.
    assert!(!items.contains(&"enew".to_string()), "items: {items:?}");
}

#[tokio::test]
async fn esc_closes_menu_keeps_line() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":e<Tab>");
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("menu after <Tab>");
    // First <Esc> dismisses the wildmenu but leaves the command line open.
    feed(&rpc, "<Esc>");
    let map = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("a frame with no menu after <Esc>");
    assert!(
        command_mode(&map),
        "command line should stay open after the wildmenu closes"
    );
}

#[tokio::test]
async fn typing_without_tab_opens_nothing() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // On-demand: typing a command without <Tab> never opens the popup.
    feed(&rpc, ":e");
    let map = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("a frame with no menu");
    assert!(command_mode(&map));
}

#[tokio::test]
async fn disabled_without_setup() {
    let dir = temp_dir("cmdcomplete");
    // No nx.cmdline_complete.setup{} — the engine stays off.
    let (rpc, mut incoming) = start(&dir, "-- no completion").await;

    feed(&rpc, ":e<Tab>");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "no popup when the engine is not configured"
    );
}

#[tokio::test]
async fn enabled_by_default_via_server_init() {
    // The interactive binary sets `cmdline_complete_default`, which runs
    // `nx.cmdline_complete.setup{}` before init.lua — so `:`+<Tab> completes even
    // with an init.lua that never calls setup itself (the headless default is off,
    // which keeps the rest of the suite hermetic).
    let dir = temp_dir("cmdcomplete");
    std::fs::write(dir.join("init.lua"), "-- no explicit setup").expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        cmdline_complete_default: true,
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a completion menu after :e<Tab> with the default-on gate");
    let items = menu_items(&map);
    assert!(items.contains(&"edit".to_string()), "items: {items:?}");
    assert!(items.contains(&"enew".to_string()), "items: {items:?}");
}

#[tokio::test]
async fn config_setup_overrides_the_default_on_gate() {
    // The gate runs `nx.cmdline_complete.setup{}` (docs on) BEFORE init.lua, so a
    // config that re-runs setup with `docs = false` still wins (last config wins) —
    // proving the default is a default, not a lock.
    let dir = temp_dir("cmdcomplete");
    std::fs::write(
        dir.join("init.lua"),
        "nx.cmdline_complete.setup { docs = false }",
    )
    .expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        cmdline_complete_default: true,
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    feed(&rpc, ":ed<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert!(
        menu_docs(&map).is_none(),
        "the config's docs = false wins over the default-on gate's docs = true"
    );
}

// ---- Phase 2: navigation + accept + execute ---------------------------------------

#[tokio::test]
async fn tab_cycles_selection() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("menu after <Tab>");
    // The popup opens noselect — nothing is highlighted until the user navigates.
    assert!(!menu_selection(&map).1, "popup opens noselect");

    // A second <Tab> activates the selection on row 0; a third advances to row 1.
    feed(&rpc, "<Tab>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    feed(&rpc, "<Tab>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, 1, true)).await;
}

#[tokio::test]
async fn shift_tab_selects_last() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming).await.expect("menu");
    let last = menu_items(&map).len() as u64 - 1;
    // <S-Tab> from noselect highlights the LAST row (wrap-around backward).
    feed(&rpc, "<S-Tab>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, last, true)).await;
}

#[tokio::test]
async fn history_keys_navigate_when_open() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":e<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    // With the popup open <C-n>/<C-p> overload history recall and cycle the selection.
    feed(&rpc, "<C-n>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    feed(&rpc, "<C-n>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, 1, true)).await;
    feed(&rpc, "<C-p>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
}

#[tokio::test]
async fn cr_accepts_selection_then_executes() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // Give the current buffer content so :enew visibly switches to a new empty one.
    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // Fuzzy `ene` → `enew` is the top row; the first <Tab> opens, the second selects it.
    feed(&rpc, ":ene<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    let items = menu_items(&map);
    assert_eq!(items[0], "enew", "items: {items:?}");

    // <CR> accepts `enew` (rewriting the typed `ene` token) then executes :enew — a
    // new empty buffer replaces the `hello` one.
    feed(&rpc, "<CR>");
    nxvim_test_harness::barrier(&rpc).await;
    assert_eq!(lines(&rpc).await, vec![""]);
}

// ---- Phase 3: docs preview sidebar -------------------------------------------------

/// The `[CmdlineDocs]` doc-float **window** map from a redraw, or `None`. The wildmenu
/// docs are a real float window now (not a `menu.docs` overlay), read out of `windows[]`
/// by the scratch buffer's name.
fn cmdline_docs_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let Some(Value::Array(wins)) = map_get(map, "windows") else {
        return None;
    };
    wins.iter().find_map(|w| match w {
        Value::Map(wm)
            if map_get(wm, "file_name").and_then(Value::as_str) == Some("[CmdlineDocs]") =>
        {
            Some(wm.clone())
        }
        _ => None,
    })
}

/// The wildmenu docs float's plain-text lines, or `None` when no docs float is open.
fn menu_docs(map: &[(Value, Value)]) -> Option<Vec<String>> {
    let win = cmdline_docs_window(map)?;
    match map_get(&win, "lines") {
        Some(Value::Array(lines)) => Some(
            lines
                .iter()
                .map(|l| l.as_str().unwrap_or("").to_string())
                .collect(),
        ),
        other => panic!("expected docs window lines array, got {other:?}"),
    }
}

#[tokio::test]
async fn docs_pane_shows_selected_command() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // `ed` fuzzy-ranks `edit` first; selecting row 0 arms the docs float.
    feed(&rpc, ":ed<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    // The docs float opens in the same settle that activates the selection, so wait for
    // a frame carrying both the selection and the `[CmdlineDocs]` window.
    let map = wait_redraw(&mut incoming, |m| {
        menu_sel_is(m, 0, true) && cmdline_docs_window(m).is_some()
    })
    .await;
    assert_eq!(menu_items(&map)[0], "edit", "items: {:?}", menu_items(&map));

    let docs = menu_docs(&map).expect("a docs float beside the selected row");
    // The synopsis heads the float; the description follows the (skipped) blank line.
    assert_eq!(docs.first().map(String::as_str), Some(":edit [file]"));
    assert!(
        docs.iter().any(|l| l.contains("Edit [file]")),
        "docs: {docs:?}"
    );
}

#[tokio::test]
async fn docs_absent_until_a_row_is_selected() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // The popup opens noselect — nothing highlighted, so no docs float yet.
    feed(&rpc, ":ed<Tab>");
    let map = poll_menu(&rpc, &mut incoming).await.expect("menu");
    assert!(!menu_selection(&map).1, "popup opens noselect");
    assert!(
        menu_docs(&map).is_none(),
        "no docs float until a row is selected"
    );
}

#[tokio::test]
async fn docs_disabled_by_setup() {
    let dir = temp_dir("cmdcomplete");
    // Opt the docs pane out explicitly; the wildmenu still works.
    let (rpc, mut incoming) = start(&dir, "nx.cmdline_complete.setup { docs = false }").await;

    feed(&rpc, ":ed<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert!(
        menu_docs(&map).is_none(),
        "docs = false suppresses the docs float"
    );
}

// ---- Selecting previews the command in the line; <Esc> reverts ---------------------

/// The command-line text (the part after the `:` prefix), or `None` when absent.
fn cmdline_text(map: &[(Value, Value)]) -> Option<String> {
    match map_get(map, "cmdline") {
        Some(Value::String(s)) => s.as_str().map(str::to_string),
        _ => None,
    }
}

#[tokio::test]
async fn selecting_previews_command_in_line() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":ed<Tab>");
    let map = poll_menu(&rpc, &mut incoming).await.expect("menu");
    // Noselect: the line still shows exactly what was typed.
    assert_eq!(cmdline_text(&map).as_deref(), Some("ed"));

    // Selecting row 0 rewrites the line to the highlighted command, so <CR> runs
    // what is shown (not the typed `:ed`).
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    let selected = menu_items(&map)[0].clone();
    assert_eq!(cmdline_text(&map).as_deref(), Some(selected.as_str()));
    assert_eq!(selected, "edit");
}

#[tokio::test]
async fn cycling_tracks_the_previewed_command() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":ed<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    // Advance to row 1 — the line follows the new highlight, not the stale row 0.
    feed(&rpc, "<Tab>");
    wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 1, true)).await;
    assert_eq!(
        cmdline_text(&map).as_deref(),
        Some(menu_items(&map)[1].as_str())
    );
}

#[tokio::test]
async fn esc_reverts_the_previewed_command() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":ed<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(cmdline_text(&map).as_deref(), Some("edit"));

    // <Esc> dismisses the wildmenu AND restores the user's typed `:ed` — the preview
    // is undone, not committed.
    feed(&rpc, "<Esc>");
    let map = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("a frame with no menu after <Esc>");
    assert!(command_mode(&map), "command line stays open");
    assert_eq!(cmdline_text(&map).as_deref(), Some("ed"));
}

#[tokio::test]
async fn cr_without_selection_runs_typed_line() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;
    feed(&rpc, "ihello<Esc>");

    // Open the wildmenu but never navigate — it stays noselect.
    feed(&rpc, ":enew<Tab>");
    let map = poll_menu(&rpc, &mut incoming).await.expect("menu");
    assert!(!menu_selection(&map).1, "noselect");

    // <CR> runs the typed line (:enew) unchanged and dismisses the popup.
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(lines(&rpc).await, vec![""]);
}

// ---- Phase 4: plugin commands appear like built-ins; catalog coverage --------------

#[tokio::test]
async fn plugin_command_appears_in_wildmenu() {
    let dir = temp_dir("cmdcomplete");
    // A plugin registers a command with a description — it must show in the wildmenu
    // exactly as a built-in does, with its `desc` as the docs (the unified payoff).
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('Hello', 'echo hi', { desc = 'Greet the world' })";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, ":Hel<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a wildmenu including the plugin command");
    assert!(
        menu_items(&map).contains(&"Hello".to_string()),
        "items: {:?}",
        menu_items(&map)
    );

    // Selecting it shows its desc in the docs float (synopsis `:Hello`, then desc).
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "Hello");
    let docs = menu_docs(&map).expect("docs for the plugin command");
    assert_eq!(docs.first().map(String::as_str), Some(":Hello"));
    assert!(
        docs.iter().any(|l| l.contains("Greet the world")),
        "docs: {docs:?}"
    );
}

#[tokio::test]
async fn plugin_command_usage_heads_the_docs_like_a_builtin() {
    let dir = temp_dir("cmdcomplete");
    // A command that declares a `usage` argument signature: the docs synopsis must read
    // `:Greet [name]` — exactly how a built-in carries its arguments — so a plugin
    // command's parameters are discoverable in the same place.
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('Greet', 'echo hi', \
            { usage = '[name]', desc = 'Greet someone' })";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, ":Gre<Tab>");
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("a wildmenu including the plugin command");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "Greet");
    let docs = menu_docs(&map).expect("docs for the plugin command");
    // The synopsis carries the argument signature after the name.
    assert_eq!(docs.first().map(String::as_str), Some(":Greet [name]"));
    assert!(
        docs.iter().any(|l| l.contains("Greet someone")),
        "docs: {docs:?}"
    );
}

/// The content width of the wildmenu docs float (its window rect width minus the two
/// border cells), or 0 if absent.
fn menu_docs_width(map: &[(Value, Value)]) -> usize {
    let Some(win) = cmdline_docs_window(map) else {
        return 0;
    };
    let Some(Value::Map(r)) = map_get(&win, "rect") else {
        return 0;
    };
    (map_get(r, "width").and_then(Value::as_u64).unwrap_or(0) as usize).saturating_sub(2)
}

#[tokio::test]
async fn cmdline_docs_wrap_long_lines_to_the_box_width() {
    let dir = temp_dir("cmdcomplete_wrap");
    // A command whose description is one long line (no internal newlines), wider than
    // the docs float — it must word-wrap within the box instead of being cut off at
    // the right border.
    let long = "Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod \
                tempor incididunt ut labore et dolore magna aliqua ut enim ad minim veniam";
    let init = format!(
        "nx.cmdline_complete.setup {{}}\n\
         nx.user_command.create('Wrapcmd', function() end, {{ desc = '{long}' }})"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    feed(&rpc, ":Wrapcmd<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "Wrapcmd");

    let docs = menu_docs(&map).expect("docs float for the selected command");
    let width = menu_docs_width(&map);
    assert!(width > 0, "the docs float has a width");
    // Every wrapped line fits within the box width — nothing is cut off.
    assert!(
        docs.iter().all(|l| l.chars().count() <= width),
        "every line wraps within width {width}: {docs:?}"
    );
    // The long single-line description spans several wrapped rows (synopsis + blank +
    // the wrapped body), proving it wrapped rather than being windowed to one line.
    assert!(
        docs.len() > 3,
        "the long description wrapped onto multiple rows: {docs:?}"
    );
    // No row breaks a word in half — wrapping is word-aware (every token is intact).
    let joined = docs.join(" ");
    for word in ["consectetur", "adipiscing", "incididunt", "aliqua"] {
        assert!(
            joined.contains(word),
            "word `{word}` survived wrapping: {docs:?}"
        );
    }
}

/// The wildmenu floats over the full-width command line, so its docs sidebar must
/// be bounded by the WHOLE editor — not the focused window. A vertical split that
/// narrows the active pane must not narrow the docs float: there is no window
/// scoping the command line. Regression: the sidebar used the focused window's
/// text width as its bound.
#[tokio::test]
async fn cmdline_docs_use_the_whole_editor_width_not_the_focused_window() {
    let dir = temp_dir("cmdcomplete_split");
    // A wide-ish box (15-col name) plus a long description: bounded by a ~40-col
    // pane the sidebar is squeezed; bounded by the 80-col editor it is far wider.
    let long_desc = "Recalibrate the doohickey and frobnicate the whatsit thoroughly";
    let init = format!(
        "nx.cmdline_complete.setup {{}}\n\
         nx.user_command.create('LongCommandName', function() end, {{ desc = '{long_desc}' }})"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    // Split vertically; the focused window is now ~40 cols, the editor still 80.
    feed(&rpc, "<C-w>v");
    barrier(&rpc).await;

    feed(&rpc, ":Long<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "LongCommandName");

    let width = menu_docs_width(&map);
    assert!(
        width > 40,
        "cmdline docs must use the whole editor width, not the ~40-col focused pane, got {width}"
    );
}

#[tokio::test]
async fn cmdline_docs_keep_a_usable_width_instead_of_a_one_column_sliver() {
    let dir = temp_dir("cmdcomplete_sliver");
    // A long command name makes the wildmenu box wide; anchored near the line's
    // start, the docs float has no room to its right and flips left — where, with
    // the box hugging the left edge, the old placement squeezed it to a 1-col
    // sliver. It must instead keep a readable width (or, failing that, not show).
    let long_desc = "Frobnicate the whatsit and recalibrate the doohickey before the next cycle";
    let init = format!(
        "nx.cmdline_complete.setup {{}}\n\
         nx.user_command.create('AVeryLongUserCommandNameForCmdlineDocs', function() end, \
         {{ desc = '{long_desc}' }})"
    );
    let (rpc, mut incoming) = start(&dir, &init).await;

    feed(&rpc, ":AVery<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(
        menu_items(&map)[0],
        "AVeryLongUserCommandNameForCmdlineDocs"
    );

    // Either a readable docs float, or none at all — never a degenerate sliver.
    if menu_docs(&map).is_some() {
        let width = menu_docs_width(&map);
        assert!(
            width >= 10,
            "cmdline docs must keep a usable width, not a 1-col sliver, got {width}"
        );
    }
}

#[tokio::test]
async fn plugin_command_without_desc_shows_synopsis_only() {
    let dir = temp_dir("cmdcomplete");
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('Frobnicate', function() end)";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, ":Frob<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "Frobnicate");
    // No desc → the docs float is just the `:Frobnicate` synopsis line.
    let docs = menu_docs(&map).expect("docs for the plugin command");
    assert_eq!(docs, vec![":Frobnicate".to_string()]);
}

#[tokio::test]
async fn bundled_dock_command_shows_with_its_desc() {
    let dir = temp_dir("cmdcomplete");
    // The prelude registers the :Dock* commands via nx.command{ desc = … }; they show
    // in the wildmenu through the same user-command merge, with their desc as docs.
    let (rpc, mut incoming) = start(&dir, INIT).await;

    feed(&rpc, ":DockO<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a wildmenu including the bundled dock command");
    assert!(
        menu_items(&map).contains(&"DockOpen".to_string()),
        "items: {:?}",
        menu_items(&map)
    );

    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "DockOpen");
    let docs = menu_docs(&map).expect("docs for :DockOpen");
    assert_eq!(docs.first().map(String::as_str), Some(":DockOpen"));
    assert!(
        docs.iter().any(|l| l.contains("edge dock")),
        "docs: {docs:?}"
    );
}

/// Every command name the wildmenu source offers: the curated built-in catalog plus
/// the prelude-registered user commands (the `:Dock*` family). All must be commands
/// the editor recognizes, so the coverage sweep runs over the whole set.
async fn catalog_names(rpc: &Rpc) -> Vec<String> {
    let value = exec_lua(
        rpc,
        "local out = {}\n\
         for _, c in ipairs(nx._cmdline_complete_run('', 0)) do out[#out + 1] = c.label end\n\
         return out",
    )
    .await;
    match value {
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().expect("a string label").to_string())
            .collect(),
        other => panic!("expected an array of names, got {other:?}"),
    }
}

/// Coverage guard against catalog drift: every curated command name must be a command
/// the editor actually recognizes (no typos, no stale entries for a removed command).
/// Each name is run via `nx_command` — a *request*, so a following `barrier`
/// guarantees its redraw is delivered (no take-first race) — and we assert the
/// message line is not the `E492: Not an editor command: {name}` reply. Checking the
/// *name-specific* E492 text makes this robust against an error message persisting
/// from a prior command.
///
/// A command is skipped from *execution* (still guarded as a catalog entry) when
/// running it bare would harm the shared test process: terminate it (the quit family,
/// `:hide`), block it (`:sleep`), spawn a PTY / external process (`:terminal`,
/// `:make`/`:grep` family), mutate the **process-global** working directory other
/// parallel tests rely on (`:cd`/`:tcd`/`:lcd`), touch the filesystem (`:source`,
/// `:cfile`/`:lfile`), fetch over the network (`:TSUpdate`), or open an input prompt
/// that changes mode (`:LspRename`). These are all stable built-ins.
#[tokio::test]
async fn catalog_commands_are_recognized() {
    const SKIP: &[&str] = &[
        "quit",
        "quitall",
        "wq",
        "wqall",
        "xit",
        "hide",
        "terminal",
        "sleep",
        "cd",
        "tcd",
        "lcd",
        "make",
        "lmake",
        "grep",
        "lgrep",
        "cfile",
        "lfile",
        "source",
        "TSUpdate",
        "LspRename",
    ];
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    let names = catalog_names(&rpc).await;
    assert!(names.len() > 50, "catalog unexpectedly small: {names:?}");

    for name in &names {
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        rpc.request("nx_command", vec![Value::from(name.as_str())])
            .await
            .expect("nx_command");
        barrier(&rpc).await;
        let msg = drain_latest_redraw(&mut incoming)
            .map(|p| message_of(&p))
            .unwrap_or_default();
        assert!(
            !msg.contains(&format!("Not an editor command: {name}")),
            "catalog lists :{name}, but the editor does not recognize it (drift?) — msg: {msg:?}"
        );
    }
}

// ---- Phase 4: `:set` argument completes option names (with docs) -------------------

#[tokio::test]
async fn set_arg_completes_option_names() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // `:set nu` + <Tab>: the cursor is in the argument, so the source offers option
    // names (not commands). Fuzzy `nu` ranks `number` in.
    feed(&rpc, ":set nu<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("an option menu after :set nu<Tab>");
    let items = menu_items(&map);
    assert!(items.contains(&"number".to_string()), "items: {items:?}");
    // The context switched: command names are gone, only options are offered.
    assert!(!items.contains(&"edit".to_string()), "items: {items:?}");
}

#[tokio::test]
async fn set_arg_empty_lists_every_option() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // `:set ` + <Tab> with an empty argument token offers the whole option catalog.
    feed(&rpc, ":set <Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a menu of all options after :set <Tab>");
    let items = menu_items(&map);
    for opt in ["number", "tabstop", "ignorecase", "wrap"] {
        assert!(items.contains(&opt.to_string()), "missing {opt}: {items:?}");
    }
}

#[tokio::test]
async fn setlocal_arg_completes_option_names() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // `:setlocal` shares the option-argument completer with `:set`.
    feed(&rpc, ":setlocal ts<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("an option menu after :setlocal ts<Tab>");
    assert!(
        menu_items(&map).contains(&"tabstop".to_string()),
        "items: {:?}",
        menu_items(&map)
    );
}

#[tokio::test]
async fn set_option_docs_show_scope_kind_and_help() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // `ignorecase` is a unique fuzzy match, so it is row 0; selecting it arms the docs
    // float. The header names the option + abbreviation, a metadata line gives scope +
    // kind, and the description follows.
    feed(&rpc, ":set ignorecase<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_items(&map)[0], "ignorecase");

    let docs = menu_docs(&map).expect("a docs float for the selected option");
    assert_eq!(docs.first().map(String::as_str), Some("ignorecase (ic)"));
    assert!(
        docs.iter()
            .any(|l| l.contains("global") && l.contains("boolean")),
        "docs: {docs:?}"
    );
    assert!(
        docs.iter().any(|l| l.contains("Ignore case")),
        "docs: {docs:?}"
    );
}

#[tokio::test]
async fn set_arg_accept_rewrites_only_the_option_token() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // Selecting an option previews it in the line — replacing just the `nu` argument
    // token, leaving the `set ` command intact (the anchor is the arg word, not col 0).
    feed(&rpc, ":set nu<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("menu");
    feed(&rpc, "<Tab>");
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    let selected = menu_items(&map)[0].clone();
    assert_eq!(
        cmdline_text(&map).as_deref(),
        Some(&format!("set {selected}")[..])
    );
}

#[tokio::test]
async fn command_with_no_arg_completer_opens_no_menu() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // A command that is neither `:set`-family nor file/dir-taking has no argument
    // completer, so its argument region opens nothing (the wildmenu only covers what
    // has a source; file commands hand off to the picker instead — see below).
    feed(&rpc, ":echo foo<Tab>");
    assert!(
        poll_no_menu(&rpc, &mut incoming).await.is_some(),
        "no menu for an un-completable argument"
    );
}

/// Poll for the latest redraw whose `menu` map carries at least one item (the
/// picker streams its rows in asynchronously after opening, so the first frame can
/// be empty).
async fn poll_menu_nonempty(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<String> {
    for _ in 0..80 {
        if let Some(map) = poll_menu(rpc, incoming).await {
            let items = menu_items(&map);
            if !items.is_empty() {
                return items;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("picker never streamed any items");
}

/// Poll the picker until its visible rows equal `want` (the dynamic source re-lists
/// on a query edit; old rows stay visible until the new listing returns, so wait for
/// the specific set rather than the first non-empty frame).
async fn poll_menu_items_eq(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, want: &[&str]) {
    let mut last = Vec::new();
    for _ in 0..200 {
        if let Some(map) = poll_menu(rpc, incoming).await {
            last = menu_items(&map);
            if last == want {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("picker rows never became {want:?}; last={last:?}");
}

/// Poll the redraw until the command line's text equals `want`, returning that
/// frame. The picker confirm pastes the chosen path into the still-open command
/// line on a later tick (`nx._cmdline_set_arg` → `cmdline_replace_arg`), so this
/// waits for the line to fill — and the cmdline only renders in command mode, so a
/// match also proves the line stayed open (no auto-execute).
async fn poll_cmdline_eq(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Vec<(Value, Value)> {
    let mut last = None;
    for _ in 0..200 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| cmdline_text(m).is_some()) {
            last = cmdline_text(&map);
            if last.as_deref() == Some(want) {
                return map;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("cmdline never became {want:?}; last={last:?}");
}

/// The picker box's title (`nx.picker.open{ title = … }`), projected on the `menu`.
fn menu_title(map: &[(Value, Value)]) -> Option<String> {
    let Some(Value::Map(menu)) = map_get(map, "menu") else {
        return None;
    };
    map_get(menu, "title")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The preview pane's lines (the focused row's file head — or, for a directory, its
/// listing), in order. Empty when there is no preview pane.
fn menu_preview_lines(map: &[(Value, Value)]) -> Vec<String> {
    let Some(Value::Map(menu)) = map_get(map, "menu") else {
        return Vec::new();
    };
    let Some(Value::Map(pv)) = map_get(menu, "preview") else {
        return Vec::new();
    };
    match map_get(pv, "lines") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Poll the picker until its preview pane's lines equal `want`.
async fn poll_preview_eq(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, want: &[&str]) {
    let mut last = Vec::new();
    for _ in 0..200 {
        if let Some(map) = poll_menu(rpc, incoming).await {
            last = menu_preview_lines(&map);
            if last == want {
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("preview lines never became {want:?}; last={last:?}");
}

#[tokio::test]
async fn file_command_arg_pastes_the_chosen_path_into_the_open_cmdline() {
    let dir = temp_dir("cmdcomplete_files");
    // A known tree under a dedicated dir so the listing is deterministic.
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("alpha.txt"), "ALPHA\n").unwrap();
    std::fs::write(tree.join("beta.txt"), "BETA\n").unwrap();
    std::fs::write(tree.join("sub").join("nested.txt"), "NESTED\n").unwrap();

    let (rpc, mut incoming) = start(&dir, INIT).await;
    // Point the editor (and so the picker's `ctx.cwd`) at the tree.
    rpc.request(
        "nx_command",
        vec![Value::from(format!("cd {}", tree.display()))],
    )
    .await
    .expect("cd");
    barrier(&rpc).await;

    // `:e <Tab>` opens the file picker OVER the still-open command line, titled
    // "Select file" and listing the cwd with directories first (same-level entries).
    feed(&rpc, ":e <Tab>");
    let items = poll_menu_nonempty(&rpc, &mut incoming).await;
    assert_eq!(
        items,
        vec!["sub/", "alpha.txt", "beta.txt"],
        "the picker lists the cwd, directories first"
    );
    let map = poll_menu(&rpc, &mut incoming).await.expect("a menu frame");
    assert_eq!(
        menu_title(&map).as_deref(),
        Some("Select file"),
        "the file picker is titled"
    );

    // Typing narrows it (the picker grabs the keys — the document is untouched).
    feed(&rpc, "al");
    poll_menu_items_eq(&rpc, &mut incoming, &["alpha.txt"]).await;

    // Confirm PASTES the chosen path into the open `:e ` argument — it does NOT run
    // the command. The line fills and stays open for the user to press <CR>.
    feed(&rpc, "<CR>");
    let map = poll_cmdline_eq(&rpc, &mut incoming, "e alpha.txt").await;
    assert!(
        command_mode(&map),
        "the command line stays open after the paste (no auto-execute)"
    );
    // No file was opened — the buffer is still the empty startup buffer.
    assert_eq!(lines(&rpc).await, vec![""], "confirm did not execute :e");
}

#[tokio::test]
async fn picker_descends_dirs_previews_them_and_cd_lists_only_dirs() {
    let dir = temp_dir("cmdcomplete_descend");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("top.txt"), "TOP\n").unwrap();
    std::fs::write(tree.join("sub").join("nested.txt"), "NESTED\n").unwrap();

    let (rpc, mut incoming) = start(&dir, INIT).await;
    rpc.request(
        "nx_command",
        vec![Value::from(format!("cd {}", tree.display()))],
    )
    .await
    .expect("cd");
    barrier(&rpc).await;

    // `:cd <Tab>` lists ONLY directories (the dir-command branch), titled differently.
    feed(&rpc, ":cd <Tab>");
    let items = poll_menu_nonempty(&rpc, &mut incoming).await;
    assert_eq!(items, vec!["sub/"], "a :cd argument lists directories only");
    let map = poll_menu(&rpc, &mut incoming).await.expect("a menu frame");
    assert_eq!(menu_title(&map).as_deref(), Some("Select directory"));

    // One <Esc> closes the picker (the `:cd ` line stays open underneath); a second
    // cancels the command line back to Normal so the next `:` starts fresh.
    feed(&rpc, "<Esc>");
    poll_no_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<Esc>");
    poll_no_menu(&rpc, &mut incoming).await;

    // `:e <Tab>` lists the cwd; the focused row is the directory `sub/`, so its
    // preview pane shows the directory's CONTENTS (not a read error).
    feed(&rpc, ":e <Tab>");
    poll_menu_items_eq(&rpc, &mut incoming, &["sub/", "top.txt"]).await;
    poll_preview_eq(&rpc, &mut incoming, &["nested.txt"]).await;

    // Confirming the directory DESCENDS into it (the prompt becomes "sub/", the rows
    // are its entries by bare name) — the command line stays open underneath. Inside a
    // sub-directory the first row is the "<select directory>" action (use sub/ as-is).
    feed(&rpc, "<CR>");
    poll_menu_items_eq(&rpc, &mut incoming, &["<select directory>", "nested.txt"]).await;

    // Move past the action to the nested file and confirm — its descended path is
    // pasted into the command line.
    feed(&rpc, "<C-n>");
    feed(&rpc, "<CR>");
    poll_cmdline_eq(&rpc, &mut incoming, "e sub/nested.txt").await;
}

#[tokio::test]
async fn user_command_complete_dir_lists_directories_only() {
    let dir = temp_dir("cmdcomplete_uc_dir");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("top.txt"), "TOP\n").unwrap();

    // A user command declaring `complete = "dir"` gets the same directory-only argument
    // completion the built-in `:cd` family gets — the path that powers the GUI's
    // client-registered `:workspace <dir>`. Lowercase name (as the GUI registers it).
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('workspace', function() end, \
        { desc = 'Open a directory as a workspace', complete = 'dir' })";
    let (rpc, mut incoming) = start(&dir, init).await;
    rpc.request(
        "nx_command",
        vec![Value::from(format!("cd {}", tree.display()))],
    )
    .await
    .expect("cd");
    barrier(&rpc).await;

    // The command name still completes (and carries its desc as docs)…
    feed(&rpc, ":works<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("the command-name wildmenu");
    assert!(
        menu_items(&map).contains(&"workspace".to_string()),
        "items: {:?}",
        menu_items(&map)
    );
    feed(&rpc, "<Esc>");
    poll_no_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<Esc>");
    poll_no_menu(&rpc, &mut incoming).await;

    // …and `<Tab>` in its argument lists ONLY directories, titled like `:cd`.
    feed(&rpc, ":workspace <Tab>");
    let items = poll_menu_nonempty(&rpc, &mut incoming).await;
    assert_eq!(
        items,
        vec!["sub/"],
        "a dir-complete user command lists directories only"
    );
    let map = poll_menu(&rpc, &mut incoming).await.expect("a menu frame");
    assert_eq!(menu_title(&map).as_deref(), Some("Select directory"));
}

#[tokio::test]
async fn client_init_lua_registers_virtual_commands_with_completion() {
    let dir = temp_dir("cmdcomplete_client_init");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("top.txt"), "TOP\n").unwrap();
    std::fs::write(dir.join("init.lua"), "nx.cmdline_complete.setup {}").unwrap();

    // Mirror the GUI: register `:connect` / `:workspace` as no-op virtual commands via
    // the `client_init_lua` boot seam (run before init.lua). They must then participate
    // in command-name completion and — for `:workspace` — directory completion.
    let init = ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        client_init_lua: Some(
            "nx.user_command.create('connect', function() end, { desc = 'Connect to a daemon' })\n\
             nx.user_command.create('workspace', function() end, \
             { desc = 'Open a workspace', complete = 'dir' })"
                .to_string(),
        ),
        ..Default::default()
    };
    let (rpc, mut incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    rpc.request(
        "nx_command",
        vec![Value::from(format!("cd {}", tree.display()))],
    )
    .await
    .expect("cd");
    barrier(&rpc).await;

    // `:connect` completes as a command name (proof the seam registered it)…
    feed(&rpc, ":conn<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("the command-name wildmenu");
    assert!(
        menu_items(&map).contains(&"connect".to_string()),
        "items: {:?}",
        menu_items(&map)
    );
    feed(&rpc, "<Esc>");
    poll_no_menu(&rpc, &mut incoming).await;
    feed(&rpc, "<Esc>");
    poll_no_menu(&rpc, &mut incoming).await;

    // …and `:workspace` gets directory completion from its `complete = "dir"` spec.
    feed(&rpc, ":workspace <Tab>");
    let items = poll_menu_nonempty(&rpc, &mut incoming).await;
    assert_eq!(items, vec!["sub/"], "workspace lists directories only");
}

#[tokio::test]
async fn user_command_complete_file_lists_files() {
    let dir = temp_dir("cmdcomplete_uc_file");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("top.txt"), "TOP\n").unwrap();

    // `complete = "file"` gets the file picker (directories + files), titled "Select file".
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('Grepin', function() end, { complete = 'file' })";
    let (rpc, mut incoming) = start(&dir, init).await;
    rpc.request(
        "nx_command",
        vec![Value::from(format!("cd {}", tree.display()))],
    )
    .await
    .expect("cd");
    barrier(&rpc).await;

    feed(&rpc, ":Grepin <Tab>");
    poll_menu_items_eq(&rpc, &mut incoming, &["sub/", "top.txt"]).await;
    let map = poll_menu(&rpc, &mut incoming).await.expect("a menu frame");
    assert_eq!(menu_title(&map).as_deref(), Some("Select file"));
}

#[tokio::test]
async fn user_command_complete_fn_sync_lists_inline() {
    let dir = temp_dir("cmdcomplete_fn_sync");
    // A SYNC function completer returns a candidate list shown inline in the wildmenu
    // (core fuzzy-ranks it against the partial word).
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('Sub', function() end, { complete = function(_args) \
          return { 'add', 'remove', 'list' } end })";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, ":Sub <Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("an inline wildmenu from the sync completer");
    let items = menu_items(&map);
    for want in ["add", "remove", "list"] {
        assert!(items.contains(&want.to_string()), "items: {items:?}");
    }
}

#[tokio::test]
async fn user_command_complete_fn_receives_args_so_far() {
    let dir = temp_dir("cmdcomplete_fn_args");
    // The completer branches on the args typed so far: after `Sub add `, it returns the
    // add-specific candidates — proving `args` carries the prior words.
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('Sub', function() end, { complete = function(args) \
          if #args >= 1 and args[1] == 'add' then return { 'apple', 'apricot' } end \
          return { 'add', 'remove' } end })";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, ":Sub add <Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("the add-specific menu");
    let items = menu_items(&map);
    assert!(
        items.contains(&"apple".to_string()) && items.contains(&"apricot".to_string()),
        "completer should have seen args = {{'add'}}; items: {items:?}"
    );
    assert!(
        !items.contains(&"remove".to_string()),
        "top-level candidates must not leak into the add context; items: {items:?}"
    );
}

#[tokio::test]
async fn user_command_complete_fn_async_uses_picker() {
    let dir = temp_dir("cmdcomplete_fn_async");
    // An ASYNC completer (returns a promise — here an `nx.async` function) can't feed the
    // synchronous wildmenu, so it lists in the picker instead.
    let init = "nx.cmdline_complete.setup {}\n\
        nx.user_command.create('ASub', function() end, \
        { complete = nx.async(function(_args) return { 'alpha', 'beta' } end) })";
    let (rpc, mut incoming) = start(&dir, init).await;

    feed(&rpc, ":ASub <Tab>");
    poll_menu_items_eq(&rpc, &mut incoming, &["alpha", "beta"]).await;
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a picker frame");
    assert_eq!(menu_title(&map).as_deref(), Some(":ASub"));
}

#[tokio::test]
async fn select_directory_row_pastes_the_directory_itself() {
    let dir = temp_dir("cmdcomplete_seldir");
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub").join("nested.txt"), "N\n").unwrap();

    let (rpc, mut incoming) = start(&dir, INIT).await;
    rpc.request(
        "nx_command",
        vec![Value::from(format!("cd {}", tree.display()))],
    )
    .await
    .expect("cd");
    barrier(&rpc).await;

    // Descend into sub/ (its first row is the "<select directory>" action).
    feed(&rpc, ":e <Tab>");
    poll_menu_items_eq(&rpc, &mut incoming, &["sub/"]).await;
    feed(&rpc, "<CR>");
    poll_menu_items_eq(&rpc, &mut incoming, &["<select directory>", "nested.txt"]).await;

    // Confirming the first row (the action) uses the directory itself — `sub/` is
    // pasted into the command line, not descended into further.
    feed(&rpc, "<CR>");
    poll_cmdline_eq(&rpc, &mut incoming, "e sub/").await;
}

#[tokio::test]
async fn space_past_a_command_closes_the_wildmenu_and_does_not_auto_open_arg_completion() {
    // The wildmenu only NARROWS the same token as you type; typing PAST the completed
    // command into its argument (the space) must not auto-open a completion for the
    // new token — neither the file picker (`:e`) nor the `:set` option list. The user
    // re-opens with an explicit <Tab>. (Regression: the space used to launch the
    // picker the moment the cursor entered the argument region.)
    let dir = temp_dir("cmdcomplete_space");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // Open the command-name wildmenu and narrow it to `edit` (same-token edits).
    feed(&rpc, ":e<Tab>");
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("the command menu opens");
    feed(&rpc, "dit");
    poll_menu(&rpc, &mut incoming)
        .await
        .expect("the menu narrows, still open");

    // A space moves into the argument: the wildmenu closes and NOTHING auto-opens
    // (`poll_no_menu` would see the picker's `menu` map if it had launched).
    feed(&rpc, " ");
    let map = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("a frame with no menu after the space");
    assert!(command_mode(&map), "the command line stays open");
    assert_eq!(
        cmdline_text(&map).as_deref(),
        Some("edit "),
        "the space lands in the line; no completion was auto-triggered"
    );

    // The same space gives no menu for a `:set` argument either.
    feed(&rpc, "<Esc><Esc>");
    poll_no_menu(&rpc, &mut incoming).await;
    feed(&rpc, ":set<Tab>");
    poll_menu(&rpc, &mut incoming).await.expect("the :set menu");
    feed(&rpc, " ");
    let map = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("no option list auto-opens on the space");
    assert_eq!(cmdline_text(&map).as_deref(), Some("set "));

    // …but an explicit <Tab> in the argument region DOES open it (the option list).
    feed(&rpc, "<Tab>");
    let items = poll_menu(&rpc, &mut incoming)
        .await
        .map(|m| menu_items(&m))
        .expect("explicit <Tab> opens the option list");
    assert!(
        items.contains(&"number".to_string()),
        "the option completer ran on the explicit <Tab>: {items:?}"
    );
}

/// The shipped `examples/cmdline-completion` config loads end-to-end (so it can't
/// rot): the engine is on and the `cmdline_files` picker source the file-path
/// completion hands off to is registered.
#[tokio::test]
async fn example_cmdline_completion_config_loads() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/cmdline-completion");
    let init = ServerInit {
        config_dir: Some(root.clone()),
        runtimepath: vec![root],
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    let ok = exec_lua(
        &rpc,
        "return tostring(type(nx._cmdline_complete_run))
           .. '|' .. tostring(nx.picker._sources.cmdline_files ~= nil)",
    )
    .await;
    assert_eq!(
        ok.as_str(),
        Some("function|true"),
        "example loaded: completer + the cmdline_files picker source"
    );
}

/// Coverage guard: every option name the `:set` completer offers must be an option
/// the editor actually accepts (`:set {name}?` must not reply `E518: Unknown option`).
/// Since both the completer and `:set` read the same core catalog, this also proves
/// the catalog bridge (`nx._options_catalog`) is wired.
#[tokio::test]
async fn completed_options_are_recognized_by_set() {
    let dir = temp_dir("cmdcomplete");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    let value = exec_lua(
        &rpc,
        "local out = {}\n\
         for _, c in ipairs(nx._cmdline_complete_run('set ', 4)) do out[#out + 1] = c.label end\n\
         return out",
    )
    .await;
    let names: Vec<String> = match value {
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().expect("a string label").to_string())
            .collect(),
        other => panic!("expected an array of option names, got {other:?}"),
    };
    assert!(
        names.len() > 40,
        "option catalog unexpectedly small: {names:?}"
    );

    for name in &names {
        rpc.request("nx_command", vec![Value::from(format!("set {name}?"))])
            .await
            .expect("nx_command");
        barrier(&rpc).await;
        let msg = drain_latest_redraw(&mut incoming)
            .map(|p| message_of(&p))
            .unwrap_or_default();
        assert!(
            !msg.contains("Unknown option"),
            ":set completion offers {name}, but :set rejects it (drift?) — msg: {msg:?}"
        );
    }
}

// ---- Phase 3: mouse on the wildmenu (command-mode 'c' in 'mouse') ------------------

/// Command-mode mouse needs `'c'` in `'mouse'` (the default `"nvi"` omits it). The
/// wildmenu floats just above the command line — at the windows-area bottom, which in
/// the headless harness is the attached row count — and its list is painted *bottom-up*
/// (best match nearest the line, growing upward), so the visible candidate `r` sits
/// `r + 1` rows above the command line, its content one cell past the box's left border.
const ATTACH_ROWS: usize = 24;

fn menu_field_u64(map: &[(Value, Value)], key: &str) -> usize {
    let menu = match map_get(map, "menu") {
        Some(Value::Map(m)) => m,
        other => panic!("expected a menu map, got {other:?}"),
    };
    map_get(menu, key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("menu has a {key}")) as usize
}

#[tokio::test]
async fn clicking_a_wildmenu_candidate_selects_then_accepts_into_the_line() {
    let dir = temp_dir("cmdcomplete_mouse_click");
    let (rpc, mut incoming) = start(&dir, INIT).await;
    // Deliberately the *default* 'mouse' = "nvi" (no 'c'): the wildmenu is nxvim's own
    // interactive overlay and must be clickable without the user opting command-mode
    // mouse in. Only `nonumber norelativenumber` here, never `set mouse=a`.
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("wildmenu opens");
    let items = menu_items(&map);
    let height = menu_field_u64(&map, "height");
    let col = menu_field_u64(&map, "col");
    let want = 2usize; // "echo", a visible candidate
    assert!(
        want < height,
        "candidate {want} must be visible (height {height})"
    );
    // The list is drawn bottom-up, so logical candidate `want` is painted `want + 1`
    // rows above the command line — *not* `want` rows down from the box top. Clicking
    // the box-top-relative row (the old, buggy expectation) lands on candidate
    // `height - 1 - want` instead, which is the inversion bug this guards against.
    let click_row = ATTACH_ROWS - 1 - want;
    let click_col = col + 1;

    // Click the candidate: it highlights and previews on the command line; the menu
    // stays open (a wildmenu candidate isn't accepted on the first click).
    nxvim_test_harness::feed_mouse(&rpc, "left", "press", click_row, click_col);
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("redraw after click");
    let (sel, active) = menu_selection(&map);
    assert!(
        active && sel as usize == want,
        "the clicked candidate is highlighted (got {sel}, active {active})"
    );
    assert_eq!(
        cmdline_text(&map).as_deref(),
        Some(items[want].as_str()),
        "the line previews the highlighted candidate"
    );

    // Click the highlighted candidate again: it accepts into the line (like <CR> on
    // it) and the menu closes — the command stands, ready to run or edit further.
    nxvim_test_harness::feed_mouse(&rpc, "left", "press", click_row, click_col);
    let map = poll_no_menu(&rpc, &mut incoming)
        .await
        .expect("the menu closes on accept");
    assert_eq!(
        cmdline_text(&map).as_deref(),
        Some(items[want].as_str()),
        "the accepted command stands on the command line"
    );
    assert!(
        command_mode(&map),
        "still editing the command line after accept"
    );
}

#[tokio::test]
async fn wheeling_over_the_wildmenu_cycles_candidates() {
    let dir = temp_dir("cmdcomplete_mouse_wheel");
    let (rpc, mut incoming) = start(&dir, INIT).await;
    // Default 'mouse' = "nvi" (no 'c') on purpose — wheeling the wildmenu works without
    // command-mode mouse opted in, like clicking it. See the click test for why.
    nxvim_test_harness::command(&rpc, "set nonumber norelativenumber").await;

    feed(&rpc, ":e<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("wildmenu opens");
    let height = menu_field_u64(&map, "height");
    let col = menu_field_u64(&map, "col");
    // A cell on the top visible row of the box.
    let (r, c) = (ATTACH_ROWS - height, col + 1);

    // A wheel-down notch highlights the first candidate (from noselect)…
    nxvim_test_harness::feed_mouse(&rpc, "wheel", "down", r, c);
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
    assert_eq!(menu_selection(&map), (0, true));

    // …and another moves it down one, non-wrapping.
    nxvim_test_harness::feed_mouse(&rpc, "wheel", "down", r, c);
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 1, true)).await;
    assert_eq!(menu_selection(&map), (1, true));
}

#[tokio::test]
async fn prompt_wildmenu_anchors_past_a_multichar_prompt_label() {
    let dir = temp_dir("cmdcomplete_prompt_anchor");
    let (rpc, mut incoming) = start(&dir, INIT).await;

    // An `nx.ui.input` prompt whose label (`dap> `) is five cells wide. The client
    // paints that label ahead of the editable line, so the completed token sits five
    // cells right of the command-line origin. The wildmenu must anchor under the token,
    // not at the origin — the bug was that it ignored the prompt width and slid the
    // popup five cells left of the word it completes.
    exec_lua(
        &rpc,
        "nx.ui.input({ prompt = 'dap> ', complete = function()
           return { { label = 'print' }, { label = 'property' } }
         end }):next(function() end)",
    )
    .await;

    // Type a token at the very start of the line (anchor offset 0 within the line) and
    // open the wildmenu. Its `col` is the prompt width (5), proving it anchored past the
    // label rather than at column 0.
    feed(&rpc, "pr");
    feed(&rpc, "<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("the prompt wildmenu opens");
    assert_eq!(
        menu_field_u64(&map, "col"),
        5,
        "the wildmenu anchors under the token, past the `dap> ` prompt label"
    );
}
