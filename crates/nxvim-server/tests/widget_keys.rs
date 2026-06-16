//! Behavior tests for **configurable widget keys** (Phase 1: the picker). Every
//! picker key is an ordinary `picker`-mode keymap, not a hardcoded grab, so a user
//! `nx.keymap.set('picker', …)` rebinds it, an empty-function map disables it, and
//! an editing-mode map never leaks into the widget. Plan:
//! `docs/plans/2026-06-16-configurable-widget-keys.md`.
//!
//! Black-box like `picker.rs`: a real server sources an `init.lua`, the picker is
//! driven over RPC, and the outcome is read back through `confirm` (`_G.picked`) so
//! a test asserts on *which item the rebound key selected* rather than on the
//! redraw internals.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, barrier, exec_lua, feed, lines, spawn, temp_dir};
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

/// A static fruit source whose `confirm` records the chosen fruit in `_G.picked`,
/// plus whatever extra config a test prepends. The list is `apple, apricot, banana,
/// cherry`, so "down one then confirm" lands on `apricot`.
fn src(extra: &str) -> String {
    format!(
        "{extra}\n\
         nx.picker.source {{\n\
           name = 'fruits',\n\
           items = function(ctx)\n\
             for _, t in ipairs({{ 'apple', 'apricot', 'banana', 'cherry' }}) do\n\
               ctx.push {{ text = t, fruit = t }}\n\
             end\n\
           end,\n\
           confirm = function(item) _G.picked = item.fruit end,\n\
         }}"
    )
}

/// Open the picker and wait for it to settle (a barrier flushes the open + source).
async fn open(rpc: &Rpc) {
    exec_lua(rpc, "_G.picked = nil; nx.picker.open('fruits')").await;
    barrier(rpc).await;
}

async fn picked(rpc: &Rpc) -> Option<String> {
    exec_lua(rpc, "return _G.picked")
        .await
        .as_str()
        .map(str::to_string)
}

/// A user `nx.keymap.set('picker', …)` rebinds a picker action to a new key: `<C-j>`
/// moves the selection down even though it is not a default picker key.
#[tokio::test]
async fn user_rebind_moves_selection() {
    let dir = temp_dir("widget_keys_rebind");
    let (rpc, _incoming) = start(
        &dir,
        &src("nx.keymap.set('picker', '<C-j>', nx.picker.actions.next)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-j>"); // rebound: down one (apple -> apricot)
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("apricot"));
}

/// Binding a default picker key to an empty function disables it: `<C-n>` no longer
/// moves the selection, so confirm picks the still-first item.
#[tokio::test]
async fn empty_map_disables_a_default_key() {
    let dir = temp_dir("widget_keys_disable");
    let (rpc, _incoming) = start(
        &dir,
        &src("nx.keymap.set('picker', '<C-n>', function() end)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-n>"); // disabled: selection stays on apple
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("apple"));
}

/// An editing-mode map for the same key does NOT leak into the picker: a normal-mode
/// `<C-n>` map stays dormant while the picker owns input, and the picker's own
/// `<C-n>` (next) still fires.
#[tokio::test]
async fn editing_map_does_not_leak_into_picker() {
    let dir = temp_dir("widget_keys_noleak");
    let (rpc, _incoming) = start(
        &dir,
        &src("_G.leaked = false\n\
              nx.keymap.set('n', '<C-n>', function() _G.leaked = true end)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-n>"); // picker's next, NOT the normal-mode map
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(
        picked(&rpc).await.as_deref(),
        Some("apricot"),
        "picker's own <C-n> moved the selection"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.leaked").await.as_bool(),
        Some(false),
        "the normal-mode <C-n> map never fired inside the picker"
    );
}

/// Rebinding `confirm` to a new key works end to end: `<C-y>` confirms the first item.
#[tokio::test]
async fn rebind_confirm_key() {
    let dir = temp_dir("widget_keys_confirm");
    let (rpc, _incoming) = start(
        &dir,
        &src("nx.keymap.set('picker', '<C-y>', nx.picker.actions.confirm)"),
    )
    .await;
    open(&rpc).await;

    feed(&rpc, "<C-y>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("apple"));
}

/// An unmapped printable key is the picker's text fallthrough: it edits the query
/// (narrowing the list) and never reaches the document buffer.
#[tokio::test]
async fn unmapped_printable_edits_query() {
    let dir = temp_dir("widget_keys_query");
    let (rpc, _incoming) = start(&dir, &src("")).await;
    open(&rpc).await;

    // 'b' is not a default picker map → inserts into the query, narrowing to the
    // fruits containing "b" and confirming the first of them (banana).
    feed(&rpc, "b");
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    assert_eq!(picked(&rpc).await.as_deref(), Some("banana"));
    // The keystroke never reached the document.
    assert_eq!(lines(&rpc).await, vec![""]);
}
