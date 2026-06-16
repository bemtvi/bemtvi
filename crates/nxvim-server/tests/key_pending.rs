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

/// Source-B recorder: each event flattened to `"mode|keys|label"`, for the built-in
/// command-grammar pending states (find-char, replace, marks) whose continuation set
/// is open and which carry a `ctx.label` instead of a key list. No leader needed —
/// these keys reach the editor directly.
const RECORDER_B: &str = "_G.kp = {}\n\
     nx.on_key_pending(function(ctx)\n\
       table.insert(_G.kp, ctx.mode .. '|' .. ctx.keys .. '|' .. (ctx.label or ''))\n\
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

// ----- source B: the built-in command grammar ------------------------------

/// `f` arms the find-char grammar — an *open* pending state (any char answers it),
/// so the event carries a `label` ("Find character") and no continuations, with the
/// keys typed so far as `keys`. Typing the target char completes the motion and
/// clears the context (one trailing `n||` — the popup closes).
#[tokio::test]
async fn find_char_fires_a_label_then_clears_on_the_target() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "ihello world<Esc>0"); // a line to search, cursor at col 0
    feed(&rpc, "f");
    assert_eq!(events(&rpc).await, "n|f|Find character");
    feed(&rpc, "w"); // jump to the 'w' — completes the find, clearing the state
    assert_eq!(events(&rpc).await, "n|f|Find character;;n||");
}

/// The find-char label composes with a pending operator: `d` (operator-pending, no
/// stage yet) fires *nothing* — only `f` arms the stage — and the event's `keys`
/// shows the whole `df` showcmd prefix, so a which-key reads "delete → to character".
#[tokio::test]
async fn find_char_under_an_operator_shows_the_operator_in_keys() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "d"); // operator-pending only — Stage::Start, so no source-B event
    assert_eq!(events(&rpc).await, "");
    feed(&rpc, "f"); // arms FindPending; keys carry the operator prefix
    assert_eq!(events(&rpc).await, "n|df|Find character");
}

/// A count and the operator both land in the showcmd-style `keys` ahead of the
/// trigger (`2df`), exactly like vim's showcmd.
#[tokio::test]
async fn find_char_keys_carry_count_and_operator() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "2df");
    assert_eq!(events(&rpc).await, "n|2df|Find character");
}

/// `r` (replace one char) is another open built-in state with its own label.
#[tokio::test]
async fn replace_char_fires_its_label() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "ihello<Esc>0");
    feed(&rpc, "r");
    assert_eq!(events(&rpc).await, "n|r|Replace character");
}

/// The A→B transition — the find-char swallow made legible. With `<leader>ff`/
/// `<leader>fg` mapped, `<Space>` withholds (source A). The idle flush replays it:
/// `<Space>` runs, `f` reaches the editor and arms find-char, so the *next* event is
/// the source-B "Find character" hint — which-key swaps the leader menu for it
/// instead of leaving the user staring at a closed popup with a swallowed key.
#[tokio::test]
async fn leader_group_timeout_becomes_a_find_char_hint() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER_B}\
             vim.g.mapleader = ' '\n\
             nx.keymap.set('n', '<leader>ff', function() end, {{ desc = 'find file' }})\n\
             nx.keymap.set('n', '<leader>fg', function() end, {{ desc = 'grep' }})"
        ),
    )
    .await;
    feed(&rpc, "ihello world<Esc>0"); // a line to search; setup fires no events
    feed(&rpc, "<Space>"); // source A: withheld leader prefix
    feed(&rpc, "f"); // descends to the f-group (still source A, withheld)
    assert_eq!(events(&rpc).await, "n|<Space>|;;n|<Space>f|");
    flush(&rpc).await; // replays <Space>f → <Space> runs, f arms find-char (source B)
    assert_eq!(
        events(&rpc).await,
        "n|<Space>|;;n|<Space>f|;;n|f|Find character"
    );
}

/// With no `nx.on_key_pending` listener, the built-in grammar runs untouched — the
/// server never asks the editor for its command-pending state.
#[tokio::test]
async fn source_b_no_listener_input_unaffected() {
    let (rpc, _incoming) = start().await;
    feed(&rpc, "ihello world<Esc>"); // no listener, no maps
    feed(&rpc, "0fw"); // find 'w' with no listener registered
    let col = exec_lua(&rpc, "return vim.fn.col('.')").await;
    assert_eq!(col.as_i64(), Some(7)); // landed on the 'w' of "world"
}
