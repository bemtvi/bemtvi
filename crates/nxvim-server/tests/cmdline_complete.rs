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
    attach, drain_to_latest_redraw, feed, lines, map_get, spawn, temp_dir, wait_redraw,
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

/// The runnable `examples/cmdline-completion/` config sources cleanly and its
/// wildmenu opens — verifies the example end-to-end and guards it against drift
/// (the engine's `setup` API changing out from under the shipped config).
#[tokio::test]
async fn example_config_opens_menu() {
    let dir = temp_dir("cmdcomplete");
    let example = include_str!("../../../examples/cmdline-completion/init.lua");
    let (rpc, mut incoming) = start(&dir, example).await;

    feed(&rpc, ":tab<Tab>");
    let map = poll_menu(&rpc, &mut incoming)
        .await
        .expect("a wildmenu from the example config");
    let items = menu_items(&map);
    assert!(items.contains(&"tabnew".to_string()), "items: {items:?}");
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
    nxvim_test_harness::barrier(&rpc).await;
    assert_eq!(lines(&rpc).await, vec![""]);
}
