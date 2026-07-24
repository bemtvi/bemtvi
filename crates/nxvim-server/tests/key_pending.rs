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

/// Availability-aware recorder: like `RECORDER` but each continuation renders as
/// `"<key>/<desc>/<kind>/<available>"`, for the timeout-transition test where a
/// `g`-prefix map is kept visible but flagged unavailable.
const RECORDER_AVAIL: &str = "_G.kp = {}\n\
     vim.g.mapleader = ' '\n\
     nx.on_key_pending(function(ctx)\n\
       local parts = {}\n\
       for _, c in ipairs(ctx.continuations) do\n\
         parts[#parts+1] = c.key .. '/' .. (c.desc or '') .. '/' .. c.kind .. '/' .. tostring(c.available)\n\
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

/// Three `g`-prefixed user maps with which-key-style descriptions — the shape the
/// LSP keys take once `prelude/lsp.lua` installs them on `LspAttach` (they are no
/// longer native defaults). Used to exercise the `g`-prefix merge / timeout behavior
/// without a live server.
const G_MAPS: &str = "nx.keymap.set('n', 'gd', function() end, { desc = 'Go to definition' })\n\
     nx.keymap.set('n', 'gD', function() end, { desc = 'Go to declaration' })\n\
     nx.keymap.set('n', 'gr', function() end, { desc = 'Find references' })\n";

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

/// A BUFFER-LOCAL mapped prefix on a focused `nx.view` buffer surfaces through the
/// pending oracle — the keys-helper / which-key path for a plugin view (e.g.
/// nxvim-diff's panes, which bind `co`/`ct`/`cb`/`cp`/… buffer-local on each read-only
/// pane). The invariant: a view's buffer-local maps reach the oracle so a which-key
/// popup can list them. `c` is also the built-in change operator, so the event MERGES
/// the operator's motion grammar with the buffer-local `c…` maps — the maps must be in
/// there (as their second key `o`/`t`, kind `map`, carrying the mapping's `desc`).
#[tokio::test]
async fn buffer_local_prefix_on_focused_view_fires_pending() {
    let (rpc, _incoming) = start().await;
    // Mount a view and bind its buffer-local c-prefix maps once its buffer exists.
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             vw = nx.view.create{{ name = 'ours' }}\n\
             vw:set_lines{{ 'alpha', 'beta' }}\n\
             vw:mount{{ split = 'vsplit' }}"
        ),
    )
    .await;
    barrier(&rpc).await;
    exec_lua(
        &rpc,
        "local buf = vw:bufnr()\n\
         nx.keymap.set('n', 'co', function() end, { buffer = buf, desc = 'choose ours' })\n\
         nx.keymap.set('n', 'ct', function() end, { buffer = buf, desc = 'choose theirs' })\n\
         vw:focus()",
    )
    .await;
    barrier(&rpc).await;
    feed(&rpc, "c");
    let ev = events(&rpc).await;
    assert!(
        ev.starts_with("n|c|"),
        "pressing `c` must fire a pending event: {ev:?}"
    );
    assert!(
        ev.contains("o/choose ours/map"),
        "the view's buffer-local `co` map must surface in the pending continuations: {ev:?}"
    );
    assert!(
        ev.contains("t/choose theirs/map"),
        "the view's buffer-local `ct` map must surface in the pending continuations: {ev:?}"
    );
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
/// (separate `nx_input` batches), the way a TUI sends interactive keystrokes: a
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

/// A `g`-prefixed map carries its `desc` into the pending event: typing `g`
/// withholds it (a prefix of the `gd`/`gD`/`gr` maps) and the event lists those
/// continuations with their descriptions. **Phase 2:** `g` is *also* a built-in
/// command prefix, so the editor's own `gg`/`gt`/`gT`/`g;`/`g,`/`g*`/`g#` (and the
/// `` g` ``/`g'` mark-jump groups) are merged in — the matcher can't see them (the
/// key never reached the grammar) — and the union is re-sorted by key notation. So
/// which-key shows the mapped `g` keys *and* the built-in motions under one `g`.
#[tokio::test]
async fn g_prefix_merges_builtin_motions_with_user_maps() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, &format!("{RECORDER}{G_MAPS}")).await;
    feed(&rpc, "g");
    assert_eq!(
        events(&rpc).await,
        "n|g|\
         #/Search word backward (partial)/map,\
         '/Jump to mark line (no jumplist)/group,\
         */Search word forward (partial)/map,\
         ,/Newer change position/map,\
         ;/Older change position/map,\
         D/Go to declaration/map,\
         T/Previous tab/map,\
         `/Jump to mark (no jumplist)/group,\
         c/Toggle comment/map,\
         d/Go to definition/map,\
         g/Go to first line/map,\
         j/Down one display line/map,\
         k/Up one display line/map,\
         r/Find references/map,\
         t/Next tab/map"
    );
}

/// After the leader timeout commits `g` to the built-in grammar, the `g`-prefixed
/// maps (`gd`/`gD`/`gr`) can no longer fire — but they stay *listed*, flagged
/// `available = false`, so a which-key can keep them visible (or drop them) by
/// choice rather than having them vanish too fast to read. The built-in motions
/// remain `available = true`. Frame 1 (withheld `g`) lists everything available; the
/// idle flush replays `g` into `GPending` and Frame 2 re-flags the maps unavailable.
#[tokio::test]
async fn timed_out_g_maps_stay_listed_as_unavailable() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, &format!("{RECORDER_AVAIL}{G_MAPS}")).await;

    feed(&rpc, "g"); // Frame 1: withheld `g` — maps + built-ins, all available
    let frame1 = events(&rpc).await;
    assert!(
        frame1.contains("D/Go to declaration/map/true"),
        "frame 1 lists the map as available: {frame1}"
    );

    flush(&rpc).await; // timeout: `g` → built-in GPending (Frame 2)
    let ev = events(&rpc).await;
    let frame2 = ev.rsplit(";;").next().unwrap();
    // The maps are kept, now unavailable; the built-in motions are available.
    assert!(
        frame2.contains("D/Go to declaration/map/false"),
        "map kept but unavailable: {frame2}"
    );
    assert!(
        frame2.contains("d/Go to definition/map/false")
            && frame2.contains("r/Find references/map/false"),
        "all g-maps unavailable: {frame2}"
    );
    assert!(
        frame2.contains("g/Go to first line/map/true"),
        "built-in motion still available: {frame2}"
    );
}

/// A built-in key the user has *also* mapped does not double up — the mapped entry
/// (source A) wins, since it is what actually fires. With `gg` user-mapped, the
/// merge drops the built-in `g` row and keeps the user's `desc`.
#[tokio::test]
async fn user_map_on_a_builtin_key_wins_over_the_merge() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!("{RECORDER}nx.keymap.set('n', 'gg', function() end, {{ desc = 'top!' }})"),
    )
    .await;
    feed(&rpc, "g");
    // Exactly one `g/...` row, carrying the user's desc — not the built-in
    // "Go to first line".
    let ev = events(&rpc).await;
    assert!(ev.contains("g/top!/map"), "user desc kept: {ev}");
    assert!(
        !ev.contains("g/Go to first line/map"),
        "no built-in dup: {ev}"
    );
}

/// A withheld prefix inside a **grabbing widget** lists *that widget's* keys
/// (source C), not the editing buffer's — the oracle computes continuations from the
/// active widget bucket. With a `nx.ui.select` menu open, its built-in `gg` (a two-key
/// `select` map) withholds on `g` and the event reports `mode = "select"` with the
/// widget's continuation and its description.
#[tokio::test]
async fn widget_prefix_lists_the_active_widgets_keys() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    // Open a select menu (its `gg` -> first is a two-key default map in the `select` bucket).
    exec_lua(&rpc, "nx.ui.select({ 'aaa', 'bbb', 'ccc' }, {})").await;
    barrier(&rpc).await;
    feed(&rpc, "g"); // withholds the select widget's `gg` prefix
    assert_eq!(events(&rpc).await, "select|g|g/First item/map");
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

/// The find-char label composes with a pending operator: `d` (operator-pending)
/// fires the operator's name (**"Delete"**) with the operator in `keys`; then `f`
/// arms find-char and the event's `keys` shows the whole `df` showcmd prefix, so a
/// which-key reads "Delete → Find character".
#[tokio::test]
async fn find_char_under_an_operator_shows_the_operator_in_keys() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "d"); // operator-pending: the operator name labels it
    assert_eq!(events(&rpc).await, "n|d|Delete");
    feed(&rpc, "f"); // arms FindPending; keys carry the operator prefix
    assert_eq!(events(&rpc).await, "n|d|Delete;;n|df|Find character");
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

// ----- source B Phase 2: enumerated built-in continuations ------------------

/// A lone `z` is a *finite* built-in prefix — no map withholds it, so it reaches the
/// editor's `ZPending` stage and the event carries the enumerated viewport commands
/// (`zt`/`zz`/`zb`, `z<CR>`/`z.`/`z-`) as continuations, sorted by key notation, each
/// `kind = "map"`. (Uses the source-A `RECORDER`, which flattens continuations.)
#[tokio::test]
async fn z_prefix_lists_view_continuations() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "z");
    assert_eq!(
        events(&rpc).await,
        "n|z|\
         -/Bottom, first non-blank/map,\
         ./Center, first non-blank/map,\
         <CR>/Top, first non-blank/map,\
         b/Scroll line to bottom/map,\
         t/Scroll line to top/map,\
         z/Scroll line to center/map"
    );
}

/// A lone `<C-w>` reaches `WindowPending` (no map withholds it) and lists the window
/// commands. The set is large, so this asserts the key markers rather than the whole
/// sorted string: the prefix is `<C-w>`, a representative command is present, and the
/// doubled `<C-w>` is a *group* (the dock layer-switch prefix it leads into).
#[tokio::test]
async fn window_prefix_lists_window_continuations() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "<C-w>");
    let ev = events(&rpc).await;
    assert!(ev.starts_with("n|<C-w>|"), "prefix is <C-w>: {ev}");
    assert!(ev.contains("s/Split horizontal/map"), "split listed: {ev}");
    assert!(ev.contains("v/Split vertical/map"), "vsplit listed: {ev}");
    assert!(ev.contains("c/Close window/map"), "close listed: {ev}");
    // The second <C-w> leads deeper into the dock layer prefix → a group.
    assert!(ev.contains("<C-w>/Dock layer/group"), "layer group: {ev}");
}

/// Descending into `<C-w><C-w>` (the dock layer prefix) lists the layer-cross
/// directions — the lowercase keys cross focus, the capitals move the buffer.
#[tokio::test]
async fn window_layer_prefix_lists_cross_directions() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "<C-w>");
    feed(&rpc, "<C-w>");
    let ev = events(&rpc).await;
    // The latest event is the deeper `<C-w><C-w>` context.
    assert!(
        ev.ends_with(
            "n|<C-w><C-w>|\
         H/Move buffer to left dock/map,\
         J/Move buffer to bottom dock/map,\
         K/Move buffer to top dock/map,\
         L/Move buffer to right dock/map,\
         h/Cross to left dock/map,\
         j/Cross to bottom dock/map,\
         k/Cross to top dock/map,\
         l/Cross to right dock/map"
        ),
        "layer cross directions: {ev}"
    );
}

/// A pure operator-pending state (`d`/`c`/`y`/`=`) carries the operator's **name**
/// as its label (`d` → "Delete"), with the operator in `keys`, and clears when the
/// operator completes (here `dw`, one trailing `n||`).
#[tokio::test]
async fn operator_pending_labels_with_the_operator_name() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "d"); // delete-operator pending
    assert_eq!(events(&rpc).await, "n|d|Delete");
    feed(&rpc, "w"); // dw completes (stays in normal) — the pending state clears
    assert_eq!(events(&rpc).await, "n|d|Delete;;n||");
}

/// Operator-pending enumerates the operator-range alphabet (Phase 3): the motions
/// that complete the range plus the introducer groups (find / text-object / `g` /
/// mark / search). `c` (change) lists the same alphabet, and the doubled operator
/// (`cc`) shows as "current line(s)". Uses `RECORDER` to assert the continuations.
#[tokio::test]
async fn operator_pending_lists_the_motion_alphabet() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "c"); // change-operator pending
    let ev = events(&rpc).await;
    assert!(ev.starts_with("n|c|"), "keys are the operator: {ev}");
    assert!(ev.contains("w/to next word/map"), "word motion: {ev}");
    assert!(ev.contains("$/to end of line/map"), "eol motion: {ev}");
    assert!(
        ev.contains("c/current line(s)/map"),
        "doubled op (cc): {ev}"
    );
    // The introducers are groups (they arm a further stage).
    assert!(
        ev.contains("i/inner object →/group"),
        "text-object group: {ev}"
    );
    assert!(ev.contains("f/find char →/group"), "find group: {ev}");
}

/// `i`/`a` (operator-pending or visual) enumerate the text-object kinds (Phase 3) —
/// word/paragraph, the bracket pairs, the quotes — from `ObjectKind::from_key`.
#[tokio::test]
async fn text_object_introducer_lists_object_kinds() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "di"); // delete + inner-object introducer
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    assert!(frame.starts_with("n|di|"), "keys are `di`: {frame}");
    assert!(frame.contains("w/word/map"), "word object: {frame}");
    assert!(
        frame.contains("(/() parentheses/map"),
        "paren object: {frame}"
    );
    assert!(
        frame.contains("p/paragraph/map"),
        "paragraph object: {frame}"
    );
}

/// The text-object introducer also lists the **tree-sitter** objects Phase 1 added
/// (`f`/`a`/`c`/`t` → function/argument/comment/type), so a which-key popup shows the
/// full object menu. Like the bracket/quote objects, they are listed as the object
/// *alphabet* — always offered, not gated on whether a grammar is loaded (`di(` is
/// shown with no paren at the cursor too).
#[tokio::test]
async fn text_object_introducer_lists_treesitter_kinds() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "di"); // delete + inner-object introducer
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    assert!(frame.starts_with("n|di|"), "keys are `di`: {frame}");
    for (key, kind) in [
        ("f", "function"),
        ("a", "argument"),
        ("c", "comment"),
        ("t", "type"),
    ] {
        assert!(
            frame.contains(&format!("{key}/{kind} (tree-sitter)/map")),
            "tree-sitter {kind} object listed: {frame}"
        );
    }
}

/// The same tree-sitter objects appear when the introducer is armed from **visual**
/// mode (`vi`), not just operator-pending — the which-key menu is identical, since
/// both arm `Stage::TextObjectPending`.
#[tokio::test]
async fn visual_text_object_introducer_lists_treesitter_kinds() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "vi"); // visual + inner-object introducer
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    // In visual mode the introducer's own key (`i`) is the shown prefix — there is
    // no pending operator ahead of it as in `di`.
    assert!(
        frame.starts_with("v|i|"),
        "keys are `i` in visual mode: {frame}"
    );
    assert!(
        frame.contains("f/function (tree-sitter)/map"),
        "function object: {frame}"
    );
    assert!(
        frame.contains("t/type (tree-sitter)/map"),
        "type object: {frame}"
    );
}

/// A user-registered object (`nx.textobject.map`) shows in the which-key object menu
/// under its introducer, keyed by its object key with the capture as the description;
/// a registration overriding a built-in key replaces that row (no duplicate).
#[tokio::test]
async fn text_object_menu_lists_user_registered_objects() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    exec_lua(
        &rpc,
        "nx.textobject.map('il', '@loop.inner')\n\
         nx.textobject.map('if', '@function.inside')", // override the built-in `if`
    )
    .await;
    feed(&rpc, "ihello<Esc>0");
    feed(&rpc, "di");
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    // New key `l` appears with its capture as the description.
    assert!(
        frame.contains("l/@loop.inner/map"),
        "registered loop object: {frame}"
    );
    // The overridden `if` shows the user's capture, and only once (built-in replaced).
    assert!(
        frame.contains("f/@function.inside/map"),
        "overridden function object: {frame}"
    );
    assert!(
        !frame.contains("f/function (tree-sitter)/map"),
        "the built-in `f` row is replaced by the override, not duplicated: {frame}"
    );
}

/// `"` (register-pending) lists the registers that actually hold text (Phase 3),
/// keyed to a one-line content preview — not the bare a–z alphabet. After yanking a
/// line into register `a`, pressing `"` surfaces `a` with its contents.
#[tokio::test]
async fn register_pending_lists_stored_registers() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "\"ayy"); // yank the line into register a
    feed(&rpc, "\""); // register-pending: lists what's stored
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    assert!(
        frame.starts_with("n|\"|"),
        "keys are the register prefix: {frame}"
    );
    assert!(
        frame.contains("a/hello world"),
        "register a with its contents previewed: {frame}"
    );
}

/// `"` also surfaces the readable read-only specials — `%` (filename), `/` (last
/// search), `:` (last command), `.` (last insert) — alongside the stored writable
/// cells, since each is a valid paste source you can type at the prompt. Each
/// appears only once it actually resolves to text: a fresh buffer named `f.txt`
/// gives `%`, a search gives `/`, an ex command gives `:`, an insert gives `.`.
#[tokio::test]
async fn register_pending_lists_readonly_specials() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    // Name the buffer (so `%` resolves), run a search (`/`), an ex command (`:`),
    // and leave some inserted text (`.`).
    feed(&rpc, ":e f.txt<CR>"); // new-file buffer name → register %
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "/world<CR>"); // last search → register /
    feed(&rpc, ":noh<CR>"); // last ex command → register :
    feed(&rpc, "\""); // register-pending: lists stored + specials
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    assert!(
        frame.starts_with("n|\"|"),
        "keys are the register prefix: {frame}"
    );
    assert!(frame.contains("%/f.txt/"), "filename register %: {frame}");
    assert!(
        frame.contains("//world/"),
        "last-search register /: {frame}"
    );
    assert!(frame.contains(":/noh/"), "last-command register :: {frame}");
    assert!(
        frame.contains("./hello world/"),
        "last-insert register .: {frame}"
    );
}

/// Once a register is *selected* (`"a`), the grammar is back at a clean boundary but
/// with the register armed — which-key keeps the popup open showing **which** register
/// (in `keys`) and the actions that consume it (paste / delete-char complete; the
/// operators are groups awaiting a motion). The label is "Use register".
#[tokio::test]
async fn selected_register_shows_actions() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "\"a"); // select register a — the action menu, not a closed popup
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    assert!(
        frame.starts_with("n|\"a|"),
        "keys carry the register: {frame}"
    );
    assert!(frame.contains("p/paste after/map"), "paste action: {frame}");
    assert!(
        frame.contains("x/delete char into/map"),
        "delete-char: {frame}"
    );
    assert!(
        frame.contains("d/delete →/group"),
        "delete operator group: {frame}"
    );
    assert!(
        frame.contains("y/yank →/group"),
        "yank operator group: {frame}"
    );
}

/// The selected-register state labels as "Use register" (the register name is in
/// `keys`), so a which-key titles it `"a — Use register`.
#[tokio::test]
async fn selected_register_label() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER_B).await;
    feed(&rpc, "\"a");
    assert_eq!(events(&rpc).await, "n|\"a|Use register");
}

/// `` ` `` (mark-jump-pending) lists the marks that are actually set (Phase 3), keyed
/// to the mark's line text — not the whole letter alphabet. After `ma`, pressing
/// `` ` `` surfaces `a`.
#[tokio::test]
async fn mark_jump_pending_lists_set_marks() {
    let (rpc, _incoming) = start().await;
    exec_lua(&rpc, RECORDER).await;
    feed(&rpc, "ihello world<Esc>0");
    feed(&rpc, "ma"); // set mark a on the line
    feed(&rpc, "`"); // mark-jump-pending: lists set marks
    let ev = events(&rpc).await;
    let frame = ev.rsplit(";;").next().unwrap();
    assert!(
        frame.starts_with("n|`|"),
        "keys are the mark prefix: {frame}"
    );
    // The row leads with the mark's position, then a preview of its line.
    assert!(
        frame.contains("a/1:0  hello world"),
        "mark a with position + line preview: {frame}"
    );
}

/// A withheld prefix containing a *modified* `>` must render as its named escape
/// (`<C-gt>`): the inline spelling `<C->>` closes at the FIRST `>` when re-parsed
/// (`parse_keys` tokenizes `<C->` → junk), so it can never round-trip through a
/// which-key that re-feeds or displays the pending keys.
#[tokio::test]
async fn pending_keys_render_modified_gt_as_named_escape() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "{RECORDER}\
             nx.keymap.set('n', '<C-gt>d', function() end, {{ desc = 'gt-d' }})\n\
             nx.keymap.set('n', '<C-gt>x', function() end, {{ desc = 'gt-x' }})"
        ),
    )
    .await;
    feed(&rpc, "<C-gt>");
    barrier(&rpc).await;
    let got = events(&rpc).await;
    assert!(
        got.contains("|<C-gt>|"),
        "the pending prefix renders as the named escape <C-gt>, got {got:?}"
    );
    assert!(
        !got.contains("<C->>"),
        "must never emit <C->> (it re-parses as literal chars), got {got:?}"
    );
}

// ----- source B: Helix native sub-grammars ---------------------------------
//
// Helix's multi-key states (`m` match mode, `z` view, `f`/`t`/`F`/`T` find, `r`
// replace, `"` register) are driven by `handle_helix`'s own pending fields, outside
// the vim `PendingCommand`, so `Editor::command_pending` can't see them. The server
// asks `Editor::helix_command_pending` instead when the mode is Helix, so a
// which-key still lights up for the native Helix grammar. These assert the projected
// continuations / labels, mode `hn`. A `flush` after the probe key resolves the
// matcher's trailing single-key batch and lets the async `on_key_pending` handler
// settle before the event log is read.

/// Recorder + content + enter Helix, returning after the mode-change event is dropped.
async fn start_helix(recorder: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start().await;
    exec_lua(&rpc, recorder).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    exec_lua(&rpc, "_G.kp = {}").await;
    (rpc, incoming)
}

/// Feed a native Helix sub-grammar prefix and return the settled event log.
async fn helix_pending_after(rpc: &Rpc, keys: &str) -> String {
    feed(rpc, keys);
    flush(rpc).await;
    events(rpc).await
}

/// `m` (match mode) enumerates its sub-keys: `mm` goto-match completes (a map);
/// `mi`/`ma`/`ms`/`md`/`mr` only arm a further stage (groups), sorted by key.
#[tokio::test]
async fn helix_match_mode_lists_its_continuations() {
    let (rpc, _i) = start_helix(RECORDER).await;
    assert_eq!(
        helix_pending_after(&rpc, "m").await,
        "hn|m|\
         a/Around text object/group,\
         d/Surround delete/group,\
         i/Inside text object/group,\
         m/Goto matching bracket/map,\
         r/Surround replace/group,\
         s/Surround add/group"
    );
}

/// `mi` lists the text-object alphabet — the vim objects AND the tree-sitter captures
/// (`f`/`a`/`c`/`t`), the same set `resolve_text_object` accepts.
#[tokio::test]
async fn helix_match_inside_lists_the_object_alphabet() {
    let (rpc, _i) = start_helix(RECORDER).await;
    let ev = helix_pending_after(&rpc, "mi").await;
    assert!(ev.contains("hn|mi|"), "keys carry the `mi` prefix: {ev:?}");
    for row in [
        "w/Word/map",
        "p/Paragraph/map",
        "f/Function/map",
        "a/Argument/map",
        "c/Comment/map",
        "t/Class/map",
        "(/Parentheses/map",
    ] {
        assert!(ev.contains(row), "object menu must list {row:?}: {ev:?}");
    }
}

/// `z` (view menu) lists the Helix `zt`/`zz`/`zb` viewport keys — not the wider vim
/// `z` alphabet.
#[tokio::test]
async fn helix_view_menu_lists_zt_zz_zb() {
    let (rpc, _i) = start_helix(RECORDER).await;
    assert_eq!(
        helix_pending_after(&rpc, "z").await,
        "hn|z|\
         b/Cursor line to bottom/map,\
         t/Cursor line to top/map,\
         z/Cursor line to center/map"
    );
}

/// A pending Helix `f` (find) is an any-character leaf: it carries a `label`, no
/// continuations (source-B recorder shows the label).
#[tokio::test]
async fn helix_find_fires_a_label() {
    let (rpc, _i) = start_helix(RECORDER_B).await;
    assert_eq!(helix_pending_after(&rpc, "f").await, "hn|f|Find character");
}

/// A pending Helix `r` (replace-char) carries its label likewise.
#[tokio::test]
async fn helix_replace_fires_a_label() {
    let (rpc, _i) = start_helix(RECORDER_B).await;
    assert_eq!(
        helix_pending_after(&rpc, "r").await,
        "hn|r|Replace character"
    );
}

/// A pending Helix `"` (register select) lists the live registers, reusing the same
/// projection vim's `"` does.
#[tokio::test]
async fn helix_register_lists_live_registers() {
    let (rpc, _i) = start_helix(RECORDER).await;
    exec_lua(&rpc, "vim.fn.setreg('a', 'hi')").await;
    exec_lua(&rpc, "_G.kp = {}").await;
    let ev = helix_pending_after(&rpc, "\"").await;
    assert!(
        ev.contains("hn|\"|"),
        "keys carry the `\"` prefix in Helix normal: {ev:?}"
    );
    assert!(
        ev.contains("a/hi/map"),
        "the `a` register (contents `hi`) must be listed: {ev:?}"
    );
}

/// The Helix `g` goto menu (plugin-mapped, source A) is NOT polluted with the vim
/// `g`-grammar rows: the built-in merge is skipped in Helix, so `gj`/`g#`/tab keys —
/// which don't mean the vim thing in Helix — never appear.
#[tokio::test]
async fn helix_goto_menu_has_no_stray_vim_rows() {
    let (rpc, _i) = start_helix(RECORDER).await;
    let ev = helix_pending_after(&rpc, "g").await;
    assert!(
        ev.contains("g/Go to file start/map"),
        "the Helix goto menu is present: {ev:?}"
    );
    for stray in [
        "Down one display line",
        "Up one display line",
        "Next tab",
        "Search word forward",
    ] {
        assert!(
            !ev.contains(stray),
            "vim `g`-grammar row {stray:?} must NOT leak into Helix: {ev:?}"
        );
    }
}
