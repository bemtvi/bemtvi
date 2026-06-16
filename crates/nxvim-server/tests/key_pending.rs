//! Behavior tests for the `nx.on_key_pending` event — the engine-computed
//! pending-key signal that backs a native which-key / showcmd, driven black-box
//! over RPC exactly like `keymaps.rs`. The event fires whenever the matcher's
//! withheld prefix *changes*: it grows (a mapped prefix gains a key), clears (the
//! sequence completed, broke, or the idle flush resolved it), or is replaced.
//!
//! Observability: the registered handler appends a compact `"mode|keys|conts"`
//! string per event to a global Lua table; the test feeds keys, then reads the
//! table back with `nvim_exec_lua`. Each continuation renders as
//! `"<key>/<desc>/<kind>"`, so one assertion covers the key, its description, and
//! whether it completes a map or only leads to a deeper group. A *cleared* event
//! is `"n||"` (empty keys, no continuations).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{barrier, exec_lua, feed, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The handler recorder + the `leader = <Space>` shared by every test: each event
/// is flattened to `"mode|keys|c1,c2,…"` and pushed onto `_G.kp`. Set before any
/// map so `<leader>` bakes in as `<Space>` at set-time (vim's set-time expansion).
const RECORDER: &str = "_G.kp = {}\n\
     vim.g.mapleader = ' '\n\
     nx.on_key_pending(function(ctx)\n\
       local parts = {}\n\
       for _, c in ipairs(ctx.continuations) do\n\
         parts[#parts+1] = c.key .. '/' .. (c.desc or '') .. '/' .. c.kind\n\
       end\n\
       table.insert(_G.kp, ctx.mode .. '|' .. ctx.keys .. '|' .. table.concat(parts, ','))\n\
     end)\n";

/// `;;`-joined record of every `nx.on_key_pending` event so far.
async fn events(rpc: &Rpc) -> String {
    exec_lua(rpc, "return table.concat(_G.kp, ';;')")
        .await
        .as_str()
        .unwrap_or("<not-a-string>")
        .to_string()
}

/// The synthetic idle flush the TUI fires after `timeoutlen` of no input, resolving
/// a trailing live-prefix (design D4) — stands in for the wall-clock timer.
async fn flush(rpc: &Rpc) {
    rpc.request("nxvim_input_flush", vec![])
        .await
        .expect("input flush");
}

/// Start a UI-attached server. The `incoming` receiver must stay alive for the
/// test's duration — dropping it closes the client's read side and the next RPC
/// fails "connection closed" — so it is returned even though these tests assert on
/// the Lua-side event log (via `exec_lua`) rather than on `redraw` frames.
async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// A growing mapped prefix fires the event with its sorted continuations — each
/// carrying the mapping's `desc` and `kind = "map"`.
#[tokio::test]
async fn prefix_growth_lists_continuations() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>w', function() end, {{ desc = 'write' }})\n\
             nx.keymap.set('n', '<leader>q', function() end, {{ desc = 'quit' }})"
        ),
    )
    .await;
    feed(&rpc, "<Space>");
    // Sorted by key notation: 'q' before 'w'. Both complete a mapping (kind=map).
    assert_eq!(events(&rpc).await, "n|<Space>|q/quit/map,w/write/map");
}

/// Completing the mapping clears the prefix — one trailing *cleared* event
/// (empty keys, no continuations) so a which-key popup closes. Fed key-by-key
/// (separate `nvim_input` batches), the way a TUI sends interactive keystrokes: a
/// same-batch `<Space>w` would settle before the per-batch sample and elide the
/// growth, which is the intended "don't flash on a fast sequence" behavior.
#[tokio::test]
async fn completing_a_mapping_clears() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>w', function() end, {{ desc = 'write' }})"
        ),
    )
    .await;
    feed(&rpc, "<Space>");
    feed(&rpc, "w");
    assert_eq!(events(&rpc).await, "n|<Space>|w/write/map;;n||");
}

/// A continuation that only leads to longer mappings is a `kind = "group"` with no
/// desc; descending into it lists *its* continuations.
#[tokio::test]
async fn group_continuation_then_descend() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>gs', function() end, {{ desc = 'stage' }})\n\
             nx.keymap.set('n', '<leader>gc', function() end, {{ desc = 'commit' }})\n\
             nx.keymap.set('n', '<leader>w', function() end, {{ desc = 'write' }})"
        ),
    )
    .await;
    feed(&rpc, "<Space>");
    // 'g' is a group (no desc, kind=group); 'w' completes a map.
    assert_eq!(events(&rpc).await, "n|<Space>|g//group,w/write/map");
    feed(&rpc, "g");
    // Now under '<Space>g': both leaves, sorted (gc before gs).
    assert_eq!(
        events(&rpc).await,
        "n|<Space>|g//group,w/write/map;;n|<Space>g|c/commit/map,s/stage/map"
    );
}

/// Breaking the prefix (a key that extends no mapping) clears it — the withheld
/// keys replay raw and the context fires one *cleared* event.
#[tokio::test]
async fn breaking_the_prefix_clears() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>w', function() end, {{ desc = 'write' }})"
        ),
    )
    .await;
    feed(&rpc, "<Space>");
    feed(&rpc, "x");
    assert_eq!(events(&rpc).await, "n|<Space>|w/write/map;;n||");
}

/// The idle flush resolves a lone trailing prefix and fires the *cleared* event —
/// proof the popup closes on timeout with no following key (design D4).
#[tokio::test]
async fn idle_flush_clears() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>w', function() end, {{ desc = 'write' }})"
        ),
    )
    .await;
    feed(&rpc, "<Space>");
    assert_eq!(events(&rpc).await, "n|<Space>|w/write/map");
    flush(&rpc).await;
    assert_eq!(events(&rpc).await, "n|<Space>|w/write/map;;n||");
}

/// A key that withholds nothing (not a mapping prefix) fires *no* event — the
/// signal is fire-on-change, not per keystroke (ADR 0002 rule 4).
#[tokio::test]
async fn non_prefix_key_fires_nothing() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>w', function() end, {{ desc = 'write' }})"
        ),
    )
    .await;
    feed(&rpc, "jjj");
    assert_eq!(events(&rpc).await, "");
}

/// A continuation key with no `desc` renders an empty description but still
/// `kind = "map"` — the desc is optional, not the kind.
#[tokio::test]
async fn map_without_desc_has_empty_desc() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<leader>w', function() end)"
        ),
    )
    .await;
    feed(&rpc, "<Space>");
    assert_eq!(events(&rpc).await, "n|<Space>|w//map");
}

/// The built-in **native default** maps carry friendly descriptions too: typing `g`
/// withholds it (a prefix of the LSP `gd`/`gD`/`gr` defaults) and the pending event
/// lists those continuations with their shipped `desc`, exactly like a user map — so
/// which-key shows nice labels for the built-ins with no user config. (Sorted by key
/// notation: `D` < `d` < `r`.)
#[tokio::test]
async fn native_default_continuations_carry_descriptions() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "g");
    assert_eq!(
        events(&rpc).await,
        "n|g|D/Go to declaration/map,d/Go to definition/map,r/Find references/map"
    );
}

/// A withheld prefix inside a **grabbing widget** lists *that widget's* keys
/// (source C), not the editing buffer's — the oracle computes continuations from the
/// active widget bucket. With a panel open, its built-in `gg` (a two-key `panel`
/// map) withholds on `g` and the event reports `mode = "panel"` with the panel's
/// continuation and its description.
#[tokio::test]
async fn widget_prefix_lists_the_active_widgets_keys() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    // Open a panel (its `gg` -> first is a two-key default map in the `panel` bucket).
    exec_lua(&rpc, "vim.panel.open('P', { 'aaa', 'bbb', 'ccc' })").await;
    barrier(&rpc).await;
    feed(&rpc, "g"); // withholds the panel's `gg` prefix
    assert_eq!(events(&rpc).await, "panel|g|g/First line/map");
}

/// With no `nx.on_key_pending` listener registered the editor still maps + fires
/// normally — the gate adds no behavior of its own (and the server never walks the
/// trie for continuations).
#[tokio::test]
async fn no_listener_input_unaffected() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        "_G.fired = false\n\
         vim.g.mapleader = ' '\n\
         nx.keymap.set('n', '<leader>w', function() _G.fired = true end, { desc = 'write' })",
    )
    .await;
    feed(&rpc, "<Space>w");
    let fired = exec_lua(&rpc, "return _G.fired").await;
    assert_eq!(fired, Value::Boolean(true));
}
