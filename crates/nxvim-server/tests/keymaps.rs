//! Behavior tests for the key-mapping engine (`vim.keymap.set`), driven
//! black-box over RPC exactly like `editing.rs` / `autocmds.rs`. Phase 1 proves
//! the server-side withhold/replay matcher and the headline normal-mode surface:
//! a function or string RHS fires on its LHS, the matched keys don't *also* reach
//! the editor, a multi-key map's prefix is replayed intact when the sequence
//! turns out not to match, and a re-`set` of the same LHS wins (last-set-wins).
//!
//! Observability follows the autocmd tests: a function RHS that `print`s a marker
//! lands it on the message line; a string RHS / unmapped key is observed through
//! buffer contents and the cursor. Integration-test files don't share a module,
//! so the `start*/feed/...` helpers are copied from the established pattern.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also the runtimepath), and return a connected client.
async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

/// Type a string of vim key-notation (fire-and-forget notification).
fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Send the synthetic idle flush (`nxvim_input_flush`) the TUI fires after
/// `timeoutlen` with no further input — resolving any key the matcher withheld as
/// a live prefix. Stands in for the wall-clock timer the tests deliberately don't
/// wait on (design D4: timing is out of scope; the flush *mechanism* is what we
/// assert). Awaited so it has been processed before the following assertion.
async fn flush(rpc: &Rpc) {
    rpc.request("nxvim_input_flush", vec![])
        .await
        .expect("input flush");
}

/// Feed `keys`, then deterministically return the `redraw` map the server emitted
/// for that input (the serial-ordering trick from `autocmds.rs`).
async fn redraw_after(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    keys: &str,
) -> Vec<(Value, Value)> {
    while incoming.try_recv().is_ok() {} // drop notifications buffered earlier
    rpc.request("nvim_input", vec![Value::from(keys)])
        .await
        .expect("input");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    loop {
        match incoming.try_recv() {
            Ok(Incoming::Notification { method, params }) if method == "redraw" => {
                match params.into_iter().next() {
                    Some(Value::Map(map)) => return map,
                    _ => panic!("redraw without a map"),
                }
            }
            Ok(_) => continue,
            Err(_) => panic!("no redraw arrived for {keys:?}"),
        }
    }
}

fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The message line from a redraw map.
fn message(map: &[(Value, Value)]) -> String {
    field(map, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The open panel's lines from a redraw map (`:messages` / `:ls` content); empty
/// when no panel is open.
fn panel_lines(map: &[(Value, Value)]) -> Vec<String> {
    match field(map, "panel") {
        Some(Value::Map(p)) => match field(p, "lines") {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Fetch all buffer lines. Awaiting it also barriers: every message sent before
/// it has been processed by the server.
async fn lines(rpc: &Rpc) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// The (1-based line, 0-based col) cursor position.
async fn cursor(rpc: &Rpc) -> (usize, usize) {
    let result = rpc
        .request("nvim_win_get_cursor", vec![Value::from(0u64)])
        .await
        .expect("get_cursor");
    match result {
        Value::Array(a) => (
            a.first().and_then(Value::as_u64).unwrap_or(0) as usize,
            a.get(1).and_then(Value::as_u64).unwrap_or(0) as usize,
        ),
        _ => (0, 0),
    }
}

/// The editor's `mode()` short code (`"n"`, `"i"`, …). Awaiting it also barriers.
async fn mode(rpc: &Rpc) -> String {
    let result = rpc
        .request("nvim_get_mode", vec![])
        .await
        .expect("get_mode");
    match result {
        Value::Map(map) => field(&map, "mode")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_test_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A function-RHS map fires on its sequence, and the keys it consumed do **not**
/// also reach the editor (the `<Space>` and `x` would otherwise move/delete).
#[tokio::test]
async fn function_map_fires_and_withholds_its_keys() {
    let dir = temp_dir("keymap_fn");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<Space>x', function() print('MAPPED') end)\n",
    )
    .await;

    // Put known text in the buffer; `x` on it would delete a char if it leaked.
    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    let redraw = redraw_after(&rpc, &mut incoming, "<Space>x").await;
    assert_eq!(message(&redraw), "MAPPED", "the mapping's function ran");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "neither <Space> nor x reached the editor"
    );
}

/// A `noremap` string RHS is fed straight to the editor: `Y` → `y$` yanks to
/// end-of-line, observable by pasting it back.
#[tokio::test]
async fn string_map_is_fed_to_the_editor() {
    let dir = temp_dir("keymap_str");
    let (rpc, _incoming) = start_with_config(&dir, "vim.keymap.set('n', 'Y', 'y$')\n").await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // `Y` fires `y$` (yank "hello"); `P` pastes it before the cursor (col 0).
    feed(&rpc, "YP");
    assert_eq!(
        lines(&rpc).await,
        vec!["hellohello"],
        "Y mapped to y$ yanked the line, then P pasted it"
    );
}

/// A multi-key map fires only on the full sequence.
#[tokio::test]
async fn multikey_map_fires_on_full_sequence() {
    let dir = temp_dir("keymap_multi");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    let redraw = redraw_after(&rpc, &mut incoming, "gh").await;
    assert_eq!(message(&redraw), "GH", "gh fired its mapping");
    assert_eq!(lines(&rpc).await, vec!["hello"], "g/h did not reach editor");
}

/// The withhold/replay engine: with `gh` mapped, an unmapped `g`-sequence still
/// reaches the editor intact — the withheld `g` is replayed, so core's `gg`
/// (go-to-top) still happens. (This is the exact behavior the LSP backport reuses
/// for `gd` vs `gg`.) With no input timer a trailing live-prefix stays buffered
/// until the next key flushes it, so a final motion (`0`) is sent to flush it.
#[tokio::test]
async fn unmapped_prefix_sequence_reaches_the_editor() {
    let dir = temp_dir("keymap_replay");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n",
    )
    .await;

    // Three lines; cursor ends on the last after the insert.
    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(lines(&rpc).await, vec!["line1", "line2", "line3"]);
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `gg` → go to the first line. The trailing `g` is a live prefix of `gh`;
    // the `0` both proves the replay and flushes that buffered `g`.
    feed(&rpc, "gg0");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "the replayed g's executed gg (go-to-top), then 0 to column 0"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "the g-sequence was motion only; the buffer is untouched"
    );
}

/// Re-`set`ting the same `(mode, lhs)` replaces the prior mapping (last-set-wins).
/// (The *user > default* rung is exercised on the LSP backport, where defaults
/// first exist.)
#[tokio::test]
async fn last_set_mapping_wins() {
    let dir = temp_dir("keymap_last");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<Space>p', function() print('FIRST') end)\n\
         vim.keymap.set('n', '<Space>p', function() print('SECOND') end)\n",
    )
    .await;

    let redraw = redraw_after(&rpc, &mut incoming, "<Space>p").await;
    assert_eq!(
        message(&redraw),
        "SECOND",
        "the later mapping shadows the earlier"
    );
}

// ----- Phase 2: remap, <leader>, and the visual modes -----------------------

/// `<leader>` in the LHS is expanded from `vim.g.mapleader` at set-time. With the
/// leader a space, `<leader>w` fires on `<Space>w`.
#[tokio::test]
async fn leader_is_expanded_at_set_time() {
    let dir = temp_dir("keymap_leader");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.g.mapleader = ' '\n\
         vim.keymap.set('n', '<leader>w', function() print('LEAD') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]); // barrier: drains the insert redraws
    let redraw = redraw_after(&rpc, &mut incoming, "<Space>w").await;
    assert_eq!(message(&redraw), "LEAD", "<leader>w fired on <Space>w");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "the keys didn't reach core"
    );
}

/// A `remap` string RHS is re-fed *through the matcher*, so its keys trigger
/// further mappings: `a` → `b` (remap) reaches `b`'s function. (`noremap` would
/// instead feed a literal `b` to the editor and never see `b`'s map.)
#[tokio::test]
async fn remap_rhs_chains_through_another_mapping() {
    let dir = temp_dir("keymap_remap");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'a', 'b', { remap = true })\n\
         vim.keymap.set('n', 'b', function() print('VIA_B') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]); // barrier: drains the insert redraws
    let redraw = redraw_after(&rpc, &mut incoming, "a").await;
    assert_eq!(message(&redraw), "VIA_B", "a remapped to b reached b's fn");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "a never entered insert; b never reached the editor"
    );
}

/// `<leader>` is expanded in the string RHS too (not just the LHS), so a remap
/// RHS can name another `<leader>` mapping. Here `<leader>a` → `<leader>b`
/// (remap) reaches `<leader>b`'s function, with the leader a space.
#[tokio::test]
async fn leader_is_expanded_in_the_rhs() {
    let dir = temp_dir("keymap_leader_rhs");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.g.mapleader = ' '\n\
         vim.keymap.set('n', '<leader>a', '<leader>b', { remap = true })\n\
         vim.keymap.set('n', '<leader>b', function() print('VIA_LEADER_B') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]); // barrier: drains the insert redraws
    let redraw = redraw_after(&rpc, &mut incoming, "<Space>a").await;
    assert_eq!(
        message(&redraw),
        "VIA_LEADER_B",
        "<leader>a's RHS <leader>b expanded and chained"
    );
}

/// A self-referential `remap` map terminates at the depth cap instead of looping:
/// `x` → `x` (remap) exhausts its re-feed budget and then falls through to a
/// literal `x`, which deletes one char. The test completing at all proves it
/// didn't hang.
#[tokio::test]
async fn self_referential_remap_terminates() {
    let dir = temp_dir("keymap_cycle");
    let (rpc, _incoming) =
        start_with_config(&dir, "vim.keymap.set('n', 'x', 'x', { remap = true })\n").await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // `x` loops x→x until the budget runs out, then feeds one literal x: 'h' gone.
    feed(&rpc, "x");
    assert_eq!(
        lines(&rpc).await,
        vec!["ello"],
        "the cycle bottomed out in a single literal x (one char deleted)"
    );
}

/// A mode *list* maps in every listed mode: `{ 'n', 'v' }` fires both in Normal
/// and after entering Visual with `v`.
#[tokio::test]
async fn mode_list_maps_in_each_mode() {
    let dir = temp_dir("keymap_modelist");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set({ 'n', 'v' }, '<Space>m', function() print('MULTI') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]); // barrier: drains the insert redraws
    let normal = redraw_after(&rpc, &mut incoming, "<Space>m").await;
    assert_eq!(message(&normal), "MULTI", "fired in normal mode");

    // `v` enters Visual; the same map fires there too.
    let visual = redraw_after(&rpc, &mut incoming, "v<Space>m").await;
    assert_eq!(message(&visual), "MULTI", "fired in visual mode");
}

/// An `x`-mode map is Visual-only: it fires once Visual is entered, and a plain
/// Normal-mode press of the same key does not fire it.
#[tokio::test]
async fn visual_only_map_does_not_fire_in_normal() {
    let dir = temp_dir("keymap_xmode");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('x', 'U', function() print('XU') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]); // barrier: drains the insert redraws

    // In Normal, `U` is not an x-mode match — it must not fire the mapping.
    let normal = redraw_after(&rpc, &mut incoming, "U").await;
    assert_ne!(message(&normal), "XU", "x-mode map must not fire in normal");

    // Enter Visual with `v`, then `U` fires.
    let visual = redraw_after(&rpc, &mut incoming, "vU").await;
    assert_eq!(message(&visual), "XU", "x-mode map fired in visual");
}

// ----- Phase 3: insert/command mode, buffer-local maps, deletion ------------

/// An insert-mode map fires while inserting: `jk` → `<Esc>` leaves insert, and a
/// lone `j` still inserts a literal `j` (the withheld prefix is replayed when the
/// next key breaks the `jk` sequence). The matcher selects the Insert trie by the
/// editor's current mode.
#[tokio::test]
async fn insert_mode_map_fires_and_lone_prefix_inserts() {
    let dir = temp_dir("keymap_insert");
    let (rpc, _incoming) = start_with_config(&dir, "vim.keymap.set('i', 'jk', '<Esc>')\n").await;

    // Type some text, then `jk` to leave insert — the map fires in insert mode.
    feed(&rpc, "ihello");
    assert_eq!(mode(&rpc).await, "i", "i entered insert mode");
    feed(&rpc, "jk");
    assert_eq!(mode(&rpc).await, "n", "jk fired <Esc>, back to normal");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "neither j nor k was inserted"
    );

    // A lone `j` (no following `k`) still inserts: the withheld `j` is replayed
    // when the next key breaks `jk`. `<Esc>` both proves the replay and flushes
    // the trailing prefix (the D4 no-timer gap — a final key flushes `pending`).
    feed(&rpc, "oj<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello", "j"],
        "the lone j was inserted on its own line"
    );
}

/// A command-line map fires in command mode: with `'c'` mapping `jj` → `xy`,
/// typing `:` then `jj` edits the command line so it reads `xy` — the mapped keys
/// never reach the line themselves. Observed through the `cmdline` the redraw
/// carries (no need to *submit* the line, so ex-command semantics stay out of it).
#[tokio::test]
async fn command_mode_map_edits_the_command_line() {
    let dir = temp_dir("keymap_cmdline");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.keymap.set('c', 'jj', 'xy')\n").await;

    let redraw = redraw_after(&rpc, &mut incoming, ":jj").await;
    assert_eq!(
        field(&redraw, "command_mode").and_then(Value::as_bool),
        Some(true),
        ": entered command mode"
    );
    assert_eq!(
        field(&redraw, "cmdline").and_then(Value::as_str),
        Some("xy"),
        "jj fired its c-mode map, inserting xy into the command line"
    );
}

/// A buffer-local map fires only in the buffer it was set for: it works in
/// buffer 1, does nothing after `:enew` opens buffer 2, and works again once
/// buffer 1 is current. (The buffer-local > global rung of D6, here with no global
/// to fall back to.) The map *edits its buffer* (inserts a `Z`) rather than
/// printing, so each buffer's contents are an unambiguous, per-buffer witness —
/// the shared message line would carry a stale marker across the switches.
#[tokio::test]
async fn buffer_local_map_fires_only_in_its_buffer() {
    let dir = temp_dir("keymap_buflocal");
    // Bound to buffer 1 — the startup buffer's id. Inserts a `Z` at the cursor.
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<Space>b', 'iZ<Esc>', { buffer = 1 })\n",
    )
    .await;

    // Give buffer 1 real content (also makes it non-throwaway, so the later
    // `:enew` opens a *second* buffer instead of reusing this empty one).
    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // Buffer 1: `<Space>b` fires `iZ<Esc>`, inserting a Z at column 0.
    feed(&rpc, "<Space>b");
    assert_eq!(lines(&rpc).await, vec!["Zhello"], "fires in its own buffer");

    // `:enew` opens buffer 2; the buffer-1-local map is not in force there, so the
    // keys fall through (<Space>/b are normal-mode motions) and edit nothing.
    feed(&rpc, ":enew<CR>");
    feed(&rpc, "<Space>b");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "must not fire in another buffer"
    );

    // Back to buffer 1: the map is live again — a second Z lands at column 0.
    feed(&rpc, ":buffer 1<CR>");
    feed(&rpc, "<Space>b");
    assert_eq!(
        lines(&rpc).await,
        vec!["ZZhello"],
        "live again in its buffer"
    );
}

/// `vim.keymap.del` stops a map firing; and re-`set`ting the same map (an
/// augroup-`clear`-style re-source) leaves exactly one mapping, so it can't
/// double-fire. The function RHS appends a marker char so a double-fire would be
/// observable as two chars.
#[tokio::test]
async fn del_removes_a_map_and_resourcing_does_not_double_fire() {
    let dir = temp_dir("keymap_del");
    let (rpc, _incoming) = start_with_config(
        &dir,
        // Set the same map twice (the re-source case), then a third that we delete.
        "vim.keymap.set('n', '<Space>a', 'A')\n\
         vim.keymap.set('n', '<Space>a', 'A')\n\
         vim.keymap.set('n', '<Space>d', 'A')\n\
         vim.keymap.del('n', '<Space>d')\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // `<Space>a` is mapped to `A` (append). Despite being set twice, it fires once
    // — one `A` press worth of insert, appending one literal after the line.
    feed(&rpc, "<Space>aX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["helloX"],
        "double-set map fired once (A appended, X typed)"
    );

    // `<Space>d` was deleted: it no longer maps to `A`. The keys fall through —
    // `<Space>` moves right, `d` begins an operator — so nothing is inserted.
    feed(&rpc, "<Space>dd");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "the deleted map didn't fire; dd fell through and deleted the line"
    );
}

/// The lower-level `nvim_set_keymap` defaults to *remappable* (design D5 — the
/// `:map`-family default, opposite of `vim.keymap.set`'s `noremap` default), while
/// an explicit `{ noremap = true }` opts out. With a user map `p` → `iX<Esc>`:
/// `Q` (remappable) chains through `p` and inserts an `X`; `W` (noremap) feeds a
/// literal `p` to the editor (native paste), bypassing the map. Observed through
/// buffer contents — a per-buffer witness, unlike the shared message line.
#[tokio::test]
async fn nvim_set_keymap_defaults_to_remappable() {
    let dir = temp_dir("keymap_lowlevel");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'p', 'iX<Esc>')\n\
         vim.api.nvim_set_keymap('n', 'Q', 'p', {})\n\
         vim.api.nvim_set_keymap('n', 'W', 'p', { noremap = true })\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // Q is remappable (the nvim_set_keymap default): its RHS `p` re-feeds through
    // the matcher and triggers the user's `p` → `iX<Esc>` map, inserting an X.
    feed(&rpc, "Q");
    assert_eq!(
        lines(&rpc).await,
        vec!["Xhello"],
        "Q remapped through p, inserting X"
    );

    // W is noremap: its `p` is fed straight to the editor (native paste of the
    // empty unnamed register), bypassing the `p` map — no second X.
    feed(&rpc, "W");
    assert_eq!(
        lines(&rpc).await,
        vec!["Xhello"],
        "W (noremap) bypassed the p map"
    );
}

// ----- Phase 4: the `timeoutlen` idle flush (design D4) ----------------------

/// The blessed fix for the timer-less divergence: with `gh` mapped, pressing `gg`
/// withholds the second `g` as a live prefix of `gh`, so go-to-top doesn't fire on
/// the keystroke alone. The TUI's idle flush (`nxvim_input_flush`) resolves it —
/// the withheld `g` replays to the editor and `gg` jumps to the top — *without*
/// the user pressing another key, which is what the pre-Phase-4 engine required.
#[tokio::test]
async fn idle_flush_completes_a_withheld_prefix() {
    let dir = temp_dir("keymap_flush_gg");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n",
    )
    .await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `gg` alone leaves the second `g` withheld (a live prefix of `gh`), so the
    // cursor hasn't moved yet — exactly the trailing-prefix lag.
    feed(&rpc, "gg");
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "the second g is still withheld; go-to-top hasn't fired"
    );

    // The idle flush replays the withheld g; core sees `gg` and jumps to line 1 —
    // no following keystroke needed.
    flush(&rpc).await;
    assert_eq!(
        cursor(&rpc).await.0,
        1,
        "the idle flush completed gg → go-to-top"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "the g-sequence was motion only; the buffer is untouched"
    );
}

/// An ambiguous map (`j` is both a complete map *and* a prefix of `jk`) is held
/// rather than fired on the keystroke, since a following `k` would take the longer
/// map. The idle flush resolves the ambiguity in favor of the **shorter** map —
/// vim's `timeoutlen` behavior — firing `j`'s RHS without a next key.
#[tokio::test]
async fn idle_flush_resolves_ambiguous_shorter_map() {
    let dir = temp_dir("keymap_flush_ambig");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'j', function() print('SHORT') end)\n\
         vim.keymap.set('n', 'jk', function() print('LONG') end)\n",
    )
    .await;

    // `j` alone is ambiguous (it could continue to `jk`), so nothing fires yet.
    let redraw = redraw_after(&rpc, &mut incoming, "j").await;
    assert_eq!(message(&redraw), "", "j is held pending the ambiguity");

    // The idle flush fires the shorter map.
    while incoming.try_recv().is_ok() {}
    flush(&rpc).await;
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let mut fired = String::new();
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            if let Some(Value::Map(map)) = params.into_iter().next() {
                if !message(&map).is_empty() {
                    fired = message(&map);
                }
            }
        }
    }
    assert_eq!(fired, "SHORT", "the idle flush fired the shorter (j) map");
}

/// The flush is a no-op when nothing is withheld: the client arms it after every
/// keystroke and fires it unconditionally on idle, so a flush with an empty pending
/// buffer must not perturb editor state.
#[tokio::test]
async fn idle_flush_with_nothing_pending_is_a_noop() {
    let dir = temp_dir("keymap_flush_noop");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
    assert_eq!(cursor(&rpc).await, (1, 0));

    // No prefix is outstanding here (the `0` completed). Flushing changes nothing.
    flush(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "flush left the buffer alone"
    );
    assert_eq!(cursor(&rpc).await, (1, 0), "flush left the cursor alone");
}

// ----- Phase 4: <nowait> / <silent> / <unique> ------------------------------

/// `<nowait>` fires a complete map the instant it matches, even when it is a
/// prefix of a longer one — so an ambiguous short map resolves on the keystroke
/// alone, with no idle flush and no next key. Contrast `idle_flush_resolves_
/// ambiguous_shorter_map`, where the same `j`/`jk` pair *without* nowait holds `j`.
#[tokio::test]
async fn nowait_map_fires_immediately_despite_a_longer_map() {
    let dir = temp_dir("keymap_nowait");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'j', function() print('JNOW') end, { nowait = true })\n\
         vim.keymap.set('n', 'jk', function() print('JK') end)\n",
    )
    .await;

    let redraw = redraw_after(&rpc, &mut incoming, "j").await;
    assert_eq!(
        message(&redraw),
        "JNOW",
        "nowait fired j immediately, without waiting for a possible jk"
    );
}

/// `<silent>` runs the mapping but suppresses the message line it would leave: the
/// command line keeps whatever was there before, while the output still lands in
/// `:messages`. A non-silent twin shows its message, proving the suppression is the
/// flag's doing and not an empty effect.
#[tokio::test]
async fn silent_map_hides_its_message_but_keeps_the_history() {
    let dir = temp_dir("keymap_silent");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<Space>l', function() print('LOUD') end)\n\
         vim.keymap.set('n', '<Space>s', function() print('QUIET') end, { silent = true })\n",
    )
    .await;

    // The non-silent map shows its message on the command line.
    let loud = redraw_after(&rpc, &mut incoming, "<Space>l").await;
    assert_eq!(message(&loud), "LOUD");

    // The silent map fires (its print runs) but leaves the visible line as it was —
    // here still "LOUD" from the previous map, i.e. "QUIET" never reached it.
    let quiet = redraw_after(&rpc, &mut incoming, "<Space>s").await;
    assert_eq!(
        message(&quiet),
        "LOUD",
        "the silent map did not change the command line"
    );

    // But the output was still logged: :messages lists both lines.
    let msgs = redraw_after(&rpc, &mut incoming, ":messages<CR>").await;
    let history = panel_lines(&msgs);
    assert!(
        history.iter().any(|l| l.contains("QUIET")),
        "the silent map's output is still in :messages: {history:?}"
    );
    assert!(
        history.iter().any(|l| l.contains("LOUD")),
        "the loud map's output is in :messages too: {history:?}"
    );
}

/// `<unique>` refuses to overwrite an existing map: the set raises vim's E227 and
/// the original mapping stands. (The config captures the error via `pcall` and
/// stashes it behind another key so the black-box test can observe both effects.)
#[tokio::test]
async fn unique_map_errors_on_a_clash_and_keeps_the_original() {
    let dir = temp_dir("keymap_unique");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'U', function() print('ORIGINAL') end)\n\
         local ok, err = pcall(function()\n\
           vim.keymap.set('n', 'U', function() print('SHADOW') end, { unique = true })\n\
         end)\n\
         vim.keymap.set('n', 'E', function() print(ok and 'NO ERROR' or err) end)\n",
    )
    .await;

    // The unique set errored, so U still fires the original (no override).
    let orig = redraw_after(&rpc, &mut incoming, "U").await;
    assert_eq!(
        message(&orig),
        "ORIGINAL",
        "the unique clash did not overwrite the existing U map"
    );

    // And the captured error is vim's E227.
    let err = redraw_after(&rpc, &mut incoming, "E").await;
    assert!(
        message(&err).contains("E227"),
        "the unique clash raised E227, got {:?}",
        message(&err)
    );
}
