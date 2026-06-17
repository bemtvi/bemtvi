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

// ---- Phase 3: docs preview sidebar -------------------------------------------------

/// The menu's `docs` sub-map lines, or `None` when no docs float is projected.
fn menu_docs(map: &[(Value, Value)]) -> Option<Vec<String>> {
    let Some(Value::Map(menu)) = map_get(map, "menu") else {
        return None;
    };
    let Some(Value::Map(docs)) = map_get(menu, "docs") else {
        return None;
    };
    match map_get(docs, "lines") {
        Some(Value::Array(lines)) => Some(
            lines
                .iter()
                .map(|l| match l {
                    // Each docs line is a chunk run `[[text, hl], …]`; join the texts.
                    Value::Array(chunks) => chunks
                        .iter()
                        .filter_map(|c| match c {
                            Value::Array(c) => c.first().and_then(Value::as_str),
                            _ => None,
                        })
                        .collect::<String>(),
                    Value::String(s) => s.as_str().unwrap_or("").to_string(),
                    other => panic!("unexpected docs line {other:?}"),
                })
                .collect(),
        ),
        other => panic!("expected docs lines array, got {other:?}"),
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
    let map = wait_redraw(&mut incoming, |m| menu_sel_is(m, 0, true)).await;
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
/// Each name is run via `nvim_command` — a *request*, so a following `barrier`
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
        rpc.request("nvim_command", vec![Value::from(name.as_str())])
            .await
            .expect("nvim_command");
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
