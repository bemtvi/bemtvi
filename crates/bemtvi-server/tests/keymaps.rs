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

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{
    barrier, config_init, cursor, exec_lua, feed, lines, lua_bool, lua_u64, mode, redraw_after,
    start_with_config, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Send the synthetic idle flush (`bemtvi_input_flush`) the TUI fires after
/// `timeoutlen` with no further input — resolving any key the matcher withheld as
/// a live prefix. Stands in for the wall-clock timer the tests deliberately don't
/// wait on (design D4: timing is out of scope; the flush *mechanism* is what we
/// assert). Awaited so it has been processed before the following assertion.
async fn flush(rpc: &Rpc) {
    rpc.request("bemtvi_input_flush", vec![])
        .await
        .expect("input flush");
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

/// A function key (`<F5>`) fires its mapping. Regression: function keys weren't
/// modeled in the notation parser at all — `<F5>` parsed to nothing, so both the
/// `keymap.set` LHS and the fed input were empty, and the map never matched. (This
/// is what made the dap plugin's default `<F5>`/`<F10>`/`<F11>` bindings inert.)
#[tokio::test]
async fn function_key_map_fires() {
    let dir = temp_dir("keymap_fkey");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<F5>', function() print('F5_RAN') end)\n",
    )
    .await;

    let redraw = redraw_after(&rpc, &mut incoming, "<F5>").await;
    assert_eq!(
        message(&redraw),
        "F5_RAN",
        "the <F5> mapping's function ran"
    );
}

/// A `<C-S-c>` / `<A-S-c>` mapping fires when that exact notation is fed. Shift is a
/// distinct modifier flag once ctrl/alt is also held (`parse_special` sets
/// `key.shift`), so the LHS and the live input must round-trip through the same
/// `<C-S-c>` spelling. Paired regression: the clients' `notation` encoder used to drop
/// shift on a modified printable and send bare `<C-c>`, so these remaps never fired
/// (see `bemtvi-view`'s `shift_with_ctrl_or_alt_is_a_modifier_flag`).
#[tokio::test]
async fn ctrl_shift_and_alt_shift_letter_maps_fire() {
    let dir = temp_dir("keymap_shift_mod");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<C-S-c>', function() print('CTRL_SHIFT_C') end)\n\
         vim.keymap.set('n', '<A-S-c>', function() print('ALT_SHIFT_C') end)\n",
    )
    .await;

    let redraw = redraw_after(&rpc, &mut incoming, "<C-S-c>").await;
    assert_eq!(message(&redraw), "CTRL_SHIFT_C", "the <C-S-c> mapping ran");

    let redraw = redraw_after(&rpc, &mut incoming, "<A-S-c>").await;
    assert_eq!(message(&redraw), "ALT_SHIFT_C", "the <A-S-c> mapping ran");
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

/// A `noremap` string RHS whose keys include an ex command with a trailing `<CR>`
/// executes that command: `<Space>t` → `:tabnew<CR>` opens a second tab page.
#[tokio::test]
async fn string_map_ex_command_with_cr_executes() {
    let dir = temp_dir("keymap_str_ex");
    let (rpc, _incoming) =
        start_with_config(&dir, "vim.keymap.set('n', '<Space>t', ':tabnew<CR>')\n").await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    let before = bemtvi_test_harness::exec_lua(&rpc, "return #vim.api.nvim_list_tabpages()").await;
    assert_eq!(before.as_u64(), Some(1), "one tab page to start");

    // The mapped `:tabnew<CR>` must run: `<CR>` submits the command line (a `default`
    // cmdline map), it is not typed as text and left in the cmdline. Before the fix
    // the noremap RHS was fed straight to `editor.input`, so `<CR>` was inert and the
    // editor stayed in command mode showing `tabnew`.
    feed(&rpc, " t");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "the command line submitted and closed"
    );
    let after = bemtvi_test_harness::exec_lua(&rpc, "return #vim.api.nvim_list_tabpages()").await;
    assert_eq!(
        after.as_u64(),
        Some(2),
        "the mapped :tabnew<CR> executed and opened a second tab"
    );
}

/// The reported bug's exact shape: a `<leader>`-prefixed map whose RHS is an ex
/// command followed by more special keys (`:...<CR>` then normal-mode keys). The
/// `<CR>` runs the command and the trailing keys act — none are typed literally.
/// Here `<leader>d` → `:$<CR>gg` jumps to the last line then back to the first, so
/// the cursor ends at the top and the buffer is untouched.
#[tokio::test]
async fn leader_map_ex_then_normal_keys_all_apply() {
    let dir = temp_dir("keymap_leader_ex");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.g.mapleader = ' '\nvim.keymap.set('n', '<leader>d', ':$<CR>gg')\n",
    )
    .await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(lines(&rpc).await, vec!["line1", "line2", "line3"]);

    feed(&rpc, " d");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "back in normal mode, not stuck in cmdline"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        ":$<CR> jumped to the last line, then gg returned to the first"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "the RHS was motion/command only — nothing was typed into the buffer"
    );
}

/// A `noremap` string RHS still skips *user* maps (that is what `noremap` means),
/// even as it now fires built-in `default` maps. A user map `x` → `iZ<Esc>` must not
/// fire when a noremap RHS feeds `x`: `Q` → `xx` deletes two characters (built-in
/// `x`), it does not insert `Z`s.
#[tokio::test]
async fn noremap_rhs_still_skips_user_maps() {
    let dir = temp_dir("keymap_noremap_user");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'x', 'iZ<Esc>')\nvim.keymap.set('n', 'Q', 'xx')\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // Q feeds `xx` noremap: built-in delete-char twice, not the user `x` → iZ<Esc> map.
    feed(&rpc, "Q");
    assert_eq!(
        lines(&rpc).await,
        vec!["llo"],
        "noremap `xx` deleted two chars via the built-in, ignoring the user x map"
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

/// The withhold/replay engine plus the disambiguation oracle: with `gh` mapped,
/// the unmapped built-in `gg` still reaches the editor intact — the withheld `g`
/// is replayed and the second `g` released as a complete built-in, so go-to-top
/// fires on the keystroke alone. (This is the exact behavior the LSP backport
/// reuses for `gd` vs `gg`.) Before the unified disambiguation this needed a
/// trailing flush key (`"gg0"`); the oracle now resolves it with no following key.
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

    // `gg` → go to the first line, instantly: the second `g` completes a built-in
    // (the oracle releases it) instead of re-withholding as a live prefix of `gh`.
    feed(&rpc, "gg");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "gg reached the editor whole and went to the top — no flush key needed"
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

/// The idle flush still resolves a *genuinely* withheld prefix — one that is a
/// live prefix of a longer **mapping**, not a broken one. With `ggh` mapped, `gg`
/// is a real prefix of the `ggh` map (the disambiguation oracle never fires — no
/// mapping prefix is broken), so it is held with no movement, matching neovim's
/// `timeoutlen` wait. The TUI's idle flush (`bemtvi_input_flush`) then replays the
/// withheld `gg` raw — the shorter map having no completion — and the built-in
/// go-to-top fires, *without* the user pressing another key.
#[tokio::test]
async fn idle_flush_completes_a_withheld_prefix() {
    let dir = temp_dir("keymap_flush_gg");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'ggh', function() print('GGH') end)\n",
    )
    .await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `gg` is a live prefix of the `ggh` mapping, so it is held — go-to-top must
    // not fire yet (a following `h` would take the map). This is the one case the
    // oracle leaves alone: nothing is broken, so it waits like neovim's timeoutlen.
    feed(&rpc, "gg");
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "gg is still withheld as a live prefix of ggh; go-to-top hasn't fired"
    );

    // The idle flush replays the withheld gg raw (no `ggh` completion arrived);
    // core sees `gg` and jumps to line 1 — no following keystroke needed.
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

/// Under `:set notimeout`, the idle flush is a **no-op**: an ambiguous mapped
/// prefix is held *forever* for the next key, never resolved by an idle timeout.
/// This is what keeps a which-key popup up indefinitely after `<leader>`. Same
/// setup as `idle_flush_completes_a_withheld_prefix`, but with `notimeout` the
/// withheld `gg` stays pending across a flush (the server drops the flush), so
/// go-to-top does *not* fire — until a real key arrives.
#[tokio::test]
async fn notimeout_holds_a_withheld_prefix_across_the_flush() {
    let dir = temp_dir("keymap_notimeout");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.o.timeout = false\n\
         vim.keymap.set('n', 'ggh', function() print('GGH') end)\n",
    )
    .await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `gg` is withheld as a live prefix of `ggh`.
    feed(&rpc, "gg");
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "gg is withheld, go-to-top not fired"
    );

    // The idle flush must NOT resolve it under notimeout — the prefix waits forever.
    flush(&rpc).await;
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "notimeout: the flush left gg pending; go-to-top still hasn't fired"
    );

    // A real key still disambiguates: `h` completes the `ggh` map and fires its RHS
    // (prints GGH). The `gg` was consumed by the match (not replayed as go-to-top),
    // so the cursor stays put and the buffer is untouched — proving the prefix was
    // genuinely held across the flush, not silently dropped.
    let redraw = redraw_after(&rpc, &mut incoming, "h").await;
    assert_eq!(message(&redraw), "GGH", "the next key completed ggh");
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "ggh matched; gg never ran as a motion"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "the held prefix was a mapping match, not raw edits"
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

// ----- Phase 5: unified built-in disambiguation (the colliding-prefix fix) ----
//
// With a user map sharing a built-in's prefix, the matcher consults core's
// `command_status` oracle: a withheld run that already forms a complete built-in
// is released to the editor instead of re-held as a speculative mapping prefix.
// So every multi-key built-in fires *instantly* under a colliding user prefix —
// no idle flush, no following key. Each test below is red on pre-Phase-5 `main`
// (the built-in would lag until a flush). See the design doc, Phase 2.

/// `ggh` with `gh` mapped: the second `g` releases as a built-in (go-to-top)
/// before the `h` arrives, so `gg` then `h` (move left) runs and the `gh` map
/// does **not** fire. (Pre-fix this sent a lone `g` arming a dangling `gpending`,
/// then fired `gh` on `[g,h]` — visibly wrong; the `A!` RHS would append a `!`.)
#[tokio::test]
async fn ggh_resolves_gg_then_h_without_firing_the_gh_map() {
    let dir = temp_dir("keymap_ggh");
    let (rpc, _incoming) = start_with_config(&dir, "vim.keymap.set('n', 'gh', 'A!<Esc>')\n").await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `gg` → top (line 1, col 0); `h` → left (no-op at col 0). The `gh` map never
    // fires, so no `!` is appended anywhere.
    feed(&rpc, "ggh");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "gg jumped to the top, then h moved left — instantly, no flush"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3"],
        "the gh map did not fire — the buffer is untouched (no appended !)"
    );
}

/// Operators are instant under a colliding operator-prefix map: with `dh` mapped,
/// the doubled `dd` and the operator+motion `dw` both fire on the keystroke alone.
#[tokio::test]
async fn operator_dd_and_dw_fire_instantly_under_a_colliding_d_map() {
    let dir = temp_dir("keymap_dop");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'dh', function() print('DH') end)\n",
    )
    .await;

    // `dd` deletes the current line, with no following key to flush a held `d`.
    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>gg");
    assert_eq!(cursor(&rpc).await.0, 1, "back at the top");
    feed(&rpc, "dd");
    assert_eq!(
        lines(&rpc).await,
        vec!["line2", "line3"],
        "dd deleted line 1 instantly under the colliding dh map"
    );

    // `dw` deletes to the next word — the `w` is not a map prefix, so `d` replays
    // and `w` reaches the editor in the same feed. (`o` opens a clean line for it.)
    feed(&rpc, "ohello world<Esc>0");
    feed(&rpc, "dw");
    assert_eq!(
        lines(&rpc).await,
        vec!["line2", "world", "line3"],
        "dw deleted the first word instantly"
    );
}

/// A bare map on a key that is the **continuation of a multi-key built-in** must
/// not swallow that continuation — vim reads the second key of `g`/`z`/operator
/// commands (and `<C-w>`, covered in `dock.rs`) raw, never through mapping. This is
/// the *no-collision* case (distinct from the `gh`/`dh` collision tests above):
/// `h`/`e`/`t` are mapped bare, and `g`/`z`/`d` are **not** map prefixes, so the
/// prefix reaches the grammar raw and its continuation must follow it raw. This is
/// the bemtvi-tree report generalized — it binds `h`/`l` for fold navigation, which
/// previously ate the motion of `dh`/`ge` and the `<C-w>`/layer nav arg.
#[tokio::test]
async fn bare_maps_do_not_swallow_builtin_continuations() {
    let dir = temp_dir("keymap_continuation");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.fired = {}\n\
         vim.keymap.set('n', 'h', function() _G.fired.h = true end)\n\
         vim.keymap.set('n', 'j', function() _G.fired.j = true end)\n\
         vim.keymap.set('n', 't', function() _G.fired.t = true end)\n",
    )
    .await;

    // `dh` (operator + left motion): deletes the char left of the cursor; the bare
    // `h` map must not fire as the motion.
    feed(&rpc, "iabcdef<Esc>"); // cursor on the final 'f' (col 5)
    feed(&rpc, "dh");
    assert_eq!(
        lines(&rpc).await,
        vec!["abcdf"],
        "dh deleted the char left of the cursor — the bare h map did not eat the motion"
    );

    // `gj` (move down by display line): a two-key `g`-command whose second key is
    // the bare-mapped `j`. From the top line it must land on line 2, not fire the map.
    feed(&rpc, "ccline1<CR>line2<Esc>gg"); // two lines, cursor back at the top
    assert_eq!(cursor(&rpc).await.0, 1, "cursor at the top line");
    feed(&rpc, "gj");
    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "gj moved down a display line — the bare j map did not fire"
    );

    // `zt` (scroll cursor line to top): a `z`-command whose second key is the
    // bare-mapped `t`. It must run as the viewport command, not the map.
    feed(&rpc, "zt"); // a no-op on content here, but must not fire the t map

    let fired = lua_bool(
        &rpc,
        "return (_G.fired.h or _G.fired.j or _G.fired.t) == true",
    )
    .await;
    assert_eq!(
        fired,
        Some(false),
        "no bare map fired as a built-in continuation (dh / ge / zt all read raw)"
    );
}

/// Find-char and its repeat are instant under colliding `f`-maps: with `fh` and
/// `ff` mapped, `f{char}` jumps and `;` repeats, no flush — even though `f` is a
/// live prefix of both maps.
#[tokio::test]
async fn find_and_repeat_fire_instantly_under_colliding_f_maps() {
    let dir = temp_dir("keymap_find");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'fh', function() print('FH') end)\n\
         vim.keymap.set('n', 'ff', function() print('FF') end)\n",
    )
    .await;

    feed(&rpc, "ihello world<Esc>0");
    // `fo` → first `o` (col 4). The target char is delivered with no flush.
    feed(&rpc, "fo");
    assert_eq!(cursor(&rpc).await, (1, 4), "fo found the first o instantly");
    // `;` repeats the find → next `o` (col 7), instantly (`;` is no map prefix).
    feed(&rpc, ";");
    assert_eq!(cursor(&rpc).await, (1, 7), "; repeated the find instantly");
}

/// `r{char}` is instant under a colliding `rx` map: the replacement char is
/// delivered straight to the editor though `r` is a live prefix of `rx`.
#[tokio::test]
async fn replace_fires_instantly_under_a_colliding_r_map() {
    let dir = temp_dir("keymap_replace");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'rx', function() print('RX') end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    // `ra` replaces the `h` under the cursor with `a` — no flush needed.
    feed(&rpc, "ra");
    assert_eq!(
        lines(&rpc).await,
        vec!["aello"],
        "ra replaced the first char instantly under the colliding rx map"
    );
}

/// A text object is instant under a colliding object-prefix map: with `diz`
/// mapped (so `d` then `i` is a live mapping prefix), `diw` still deletes the
/// inner word on the keystroke alone.
#[tokio::test]
async fn text_object_diw_fires_instantly_under_a_colliding_object_map() {
    let dir = temp_dir("keymap_textobj");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'diz', function() print('DIZ') end)\n",
    )
    .await;

    feed(&rpc, "ihello world<Esc>0");
    // `diw` deletes the inner word "hello" → " world", no flush.
    feed(&rpc, "diw");
    assert_eq!(
        lines(&rpc).await,
        vec![" world"],
        "diw deleted the inner word instantly under the colliding diz map"
    );
}

/// Mode-awareness: the same `gh` map collides in **both** normal and visual, and
/// the oracle uses the mode-conditioned grammar — `gg` extends the selection to
/// its target instantly in charwise and linewise visual alike, with or without a
/// count (the count is not a map prefix, so it reaches the editor immediately
/// and its own pending count carries `3gg` to line 3).
#[tokio::test]
async fn visual_gg_variants_extend_instantly_under_a_colliding_g_map() {
    for (tag, seed, start_line, keys, want_line, want_mode) in [
        (
            "charwise",
            "iline1<CR>line2<CR>line3<Esc>",
            3,
            "vgg",
            1,
            "v",
        ),
        (
            "linewise",
            "iline1<CR>line2<CR>line3<Esc>",
            3,
            "Vgg",
            1,
            "V",
        ),
        ("counted", "ia<CR>b<CR>c<CR>d<CR>e<Esc>", 5, "v3gg", 3, "v"),
    ] {
        let dir = temp_dir("keymap_visual_gg");
        let (rpc, _incoming) = start_with_config(
            &dir,
            "vim.keymap.set({ 'n', 'v' }, 'gh', function() print('GH') end)\n",
        )
        .await;

        feed(&rpc, seed);
        assert_eq!(
            cursor(&rpc).await.0,
            start_line,
            "[{tag}] cursor starts on the last line"
        );

        // Enter visual, then `gg` resolves instantly — the second `g` releases as
        // a built-in under the visual `gh` collision, no idle flush.
        feed(&rpc, keys);
        assert_eq!(
            cursor(&rpc).await.0,
            want_line,
            "[{tag}] {keys} extended the selection instantly"
        );
        assert_eq!(mode(&rpc).await, want_mode, "[{tag}] visual mode kept");
    }
}

/// Mode-awareness for text objects: in visual `i`/`a` start an object, and with
/// a colliding visual object map (`iz`), `viwd` selects then deletes the inner
/// word instantly — the object resolves with no flush.
#[tokio::test]
async fn visual_text_object_fires_instantly_under_a_colliding_object_map() {
    let dir = temp_dir("keymap_visual_obj");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('v', 'iz', function() print('IZ') end)\n",
    )
    .await;

    feed(&rpc, "ihello world<Esc>0");
    // `v` enters visual; `iw` selects the inner word instantly (despite the `iz`
    // collision making `i` a live prefix); `d` deletes the selection.
    feed(&rpc, "viwd");
    assert_eq!(
        lines(&rpc).await,
        vec![" world"],
        "visual iw selected the inner word instantly, then d deleted it"
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

    // But the output was still logged: :messages lists both lines (in its read-only
    // scratch buffer, now the focused window).
    feed(&rpc, ":messages<CR>");
    let history = lines(&rpc).await;
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

// ----- Phase 4: <expr> maps -------------------------------------------------

/// An `<expr>` map's function RHS *returns the keys to feed* rather than acting.
/// Here `Q` returns a computed string (a real edit sequence) and it is fed; a plain
/// (non-expr) function map `N` with the same body has its return value **ignored**
/// — the contrast proves it's the `expr` flag, not the body, doing the work.
#[tokio::test]
async fn expr_map_feeds_its_returned_keys() {
    let dir = temp_dir("keymap_expr");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'Q', function() return 'iEXPR<Esc>' end, { expr = true })\n\
         vim.keymap.set('n', 'N', function() return 'iNOPE<Esc>' end)\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // N is a normal function map: its return value is ignored, nothing is inserted.
    feed(&rpc, "N");
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "a plain function map ignores its return value"
    );

    // Q is <expr>: the returned `iEXPR<Esc>` is fed, inserting EXPR at the cursor.
    feed(&rpc, "Q");
    assert_eq!(
        lines(&rpc).await,
        vec!["EXPRhello"],
        "the expr map fed its returned keys"
    );
}

/// An `<expr>` RHS that returns different keys depending on editor state — the
/// whole point of `<expr>`. `J` returns `gg` or `G` based on a Lua global a plain
/// map flips, so the computed motion follows the state at trigger time.
#[tokio::test]
async fn expr_map_computes_keys_from_state() {
    let dir = temp_dir("keymap_expr_dyn");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.g.go_top = true\n\
         vim.keymap.set('n', 'J', function()\n\
           return vim.g.go_top and 'gg' or 'G'\n\
         end, { expr = true })\n\
         vim.keymap.set('n', 'F', function() vim.g.go_top = false end)\n",
    )
    .await;

    feed(&rpc, "ia<CR>b<CR>c<Esc>"); // three lines; cursor on line 3
    assert_eq!(cursor(&rpc).await.0, 3);

    // go_top is true: J computes `gg` → jump to the top.
    feed(&rpc, "J");
    assert_eq!(cursor(&rpc).await.0, 1, "expr returned gg while go_top");

    // Flip the state, then J computes `G` → jump to the bottom.
    feed(&rpc, "F");
    feed(&rpc, "J");
    assert_eq!(cursor(&rpc).await.0, 3, "expr returned G after the flip");
}

/// The `<expr>` sandbox (textlock): an expr RHS must compute keys, not change the
/// editor. A function that calls `vim.cmd` raises under the lock, so the mapping
/// aborts — nothing is fed and the buffer is untouched — and the error surfaces.
#[tokio::test]
async fn expr_map_sandbox_blocks_editor_mutation() {
    let dir = temp_dir("keymap_expr_lock");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'S', function()\n\
           vim.cmd('normal! dd')\n\
           return 'x'\n\
         end, { expr = true })\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // S's RHS calls vim.cmd under the textlock: it raises, so neither the `dd` nor
    // the returned `x` takes effect — the line is intact.
    let redraw = redraw_after(&rpc, &mut incoming, "S").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "the sandboxed vim.cmd did not run, and no keys were fed"
    );
    assert!(
        message(&redraw).contains("E5555") || message(&redraw).contains("Error"),
        "the textlock violation surfaced an error, got {:?}",
        message(&redraw)
    );
}

/// The same sandbox through the **canonical** funnel. `btv.cmd` is the ex-command
/// entry the `btv.*` prime directive points at and `vim.cmd` is its alias, so a
/// textlock that only the alias honors has it backwards: the config written the
/// recommended way is the one that gets no error. The effect queue is discarded
/// either way, so the leak was a *silent* no-op — exactly what the fail-loud rule
/// exists to prevent.
#[tokio::test]
async fn expr_map_sandbox_blocks_the_canonical_ex_funnel_too() {
    let dir = temp_dir("keymap_expr_lock_btv");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'S', function()\n\
           btv.cmd('normal! dd')\n\
           return 'x'\n\
         end, { expr = true })\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    let redraw = redraw_after(&rpc, &mut incoming, "S").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "the sandboxed btv.cmd did not run, and no keys were fed"
    );
    assert!(
        message(&redraw).contains("E5555"),
        "btv.cmd under the textlock raises like vim.cmd, got {:?}",
        message(&redraw)
    );
}

/// The `<expr>` sandbox discards *every* effect queue, not just the handful the
/// Lua textlock already blocks. Feedkeys is not stopped by `btv._expr_lock` (it
/// only queues), so an expr RHS that calls it relies entirely on the server's
/// post-fire discard: only the *returned* keys may reach the editor. A leak here
/// applies the queued keys on the next effect drain — the `dd` below would eat
/// the whole line.
#[tokio::test]
async fn expr_map_discards_queued_effects() {
    let dir = temp_dir("keymap_expr_discard");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'Q', function()\n\
           btv._feedkeys('dd', false, false)\n\
           return 'x'\n\
         end, { expr = true })\n",
    )
    .await;

    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // Q's RHS queues `dd` via feedkeys and returns `x`: the returned key deletes
    // one char; the queued `dd` must be thrown away by the sandbox, not applied
    // by the next drain.
    feed(&rpc, "Q");
    assert_eq!(
        lines(&rpc).await,
        vec!["ello"],
        "only the expr's returned keys ran — the queued feedkeys leaked through"
    );

    // The discard is scoped to that fire: feedkeys queued outside an expr RHS
    // still applies (the sandbox didn't break the normal path).
    exec_lua(&rpc, "btv._feedkeys('x', false, false) return 0").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["llo"],
        "feedkeys outside the expr sandbox still runs"
    );
}

// ----- Disambiguation edge audits (Phase 3) ---------------------------------
//
// Phase 2 proved the oracle on direct input across the built-in families and both
// visual modes. Phase 3 audits the remaining release paths and mode corners: a
// remap RHS that resolves to a built-in, visual-line `V`, count + selection, the
// search-operator hand-off, and the inverse — a genuinely-ambiguous *mapped*
// prefix still defers to the idle flush (user maps keep winning).

/// The oracle is consulted on the **remap re-feed** path too: a remap RHS that
/// expands to a built-in fires it instantly. `Q` → `gg` (remap) is re-fed key by
/// key through the matcher, where `gh` makes the first `g` a live prefix; the
/// second `g` then releases as a complete built-in, so `Q` jumps to the top with
/// no flush — the same disambiguation as typing `gg` directly.
#[tokio::test]
async fn remap_to_a_builtin_is_instant() {
    let dir = temp_dir("keymap_remap_builtin");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gh', function() print('GH') end)\n\
         vim.keymap.set('n', 'Q', 'gg', { remap = true })\n",
    )
    .await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    feed(&rpc, "Q");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "Q remapped to gg, which resolved to the built-in go-to-top instantly"
    );
}

/// The search-operator hand-off (`d/{pattern}`) is instant under a colliding `d`
/// map: with `dh` mapped, `d` is a live prefix, but `/` breaks it — the operator
/// `d` replays and `/` opens the search prompt, so `d/world<CR>` deletes up to the
/// match with no flush. (The `/` is not a map prefix, so it reaches the editor in
/// the same feed — the operator + search-motion grammar resolves immediately.)
#[tokio::test]
async fn search_operator_handoff_is_instant_under_a_colliding_d_map() {
    let dir = temp_dir("keymap_dsearch");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'dh', function() print('DH') end)\n",
    )
    .await;

    feed(&rpc, "ihello world<Esc>gg");
    feed(&rpc, "d/world<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["world"],
        "d/world deleted up to the match instantly under the colliding dh map"
    );
    assert_eq!(cursor(&rpc).await, (1, 0));
}

/// The inverse, confirming user maps still win: with `ggh` *mapped*, `gg` is a
/// genuine live prefix of the mapping (not a broken one), so the oracle never
/// fires. Typing the full `ggh` fires the **map** — not the `gg` built-in then
/// `h`. (Compare `idle_flush_completes_a_withheld_prefix`, where the partial `gg`
/// waits for the flush and *then* resolves to the built-in.)
#[tokio::test]
async fn typing_a_full_mapped_prefix_fires_the_map_not_the_builtin() {
    let dir = temp_dir("keymap_ggh_mapped");
    let (rpc, _incoming) = start_with_config(&dir, "vim.keymap.set('n', 'ggh', 'A!<Esc>')\n").await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    // `ggh` is the complete mapping: it fires (appending `!` at the end of the
    // current line) instead of running `gg` (go-to-top) then `h`.
    feed(&rpc, "ggh");
    assert_eq!(
        lines(&rpc).await,
        vec!["line1", "line2", "line3!"],
        "the ggh map fired (appended !); the gg built-in did not run"
    );
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "the cursor stayed on line 3 — go-to-top never fired"
    );
}

// ----- g-prefix maps coexist with core g-motions (gd / gD / gr) --------------
//
// The LSP go-to keys (`gd`/`gD`/`gr`/`K`) are no longer Rust native defaults — the
// LSP plugin installs them buffer-local on `LspAttach` (see `prelude/lsp.lua`). What
// these tests still pin is the *keymap-grammar* behavior any `g`-prefixed map
// exercises, LSP or not: a user `g`-map fires, and seating it in the trie doesn't
// break core's `gg`/operator motions. A `g`-prefix map (the shape the LSP keys take
// once installed) stands in for that here without needing a live server.

/// A user `g`-prefixed map fires on its own keys and isn't swallowed by the built-in
/// `g` grammar — the matcher owns the `g` prefix and dispatches `gd`/`gD`/`gr` to the
/// mapped RHS.
#[tokio::test]
async fn g_prefix_user_maps_fire() {
    let dir = temp_dir("keymap_g_prefix_fire");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gd', function() print('MY_DEF') end)\n\
         vim.keymap.set('n', 'gD', function() print('MY_DECL') end)\n\
         vim.keymap.set('n', 'gr', function() print('MY_REFS') end)\n",
    )
    .await;

    assert_eq!(
        message(&redraw_after(&rpc, &mut incoming, "gd").await),
        "MY_DEF"
    );
    assert_eq!(
        message(&redraw_after(&rpc, &mut incoming, "gD").await),
        "MY_DECL"
    );
    assert_eq!(
        message(&redraw_after(&rpc, &mut incoming, "gr").await),
        "MY_REFS"
    );
}

/// Seating `g`-prefixed maps in the trie must not break core's `g`-motions: with
/// `gd`/`gD`/`gr` mapped, `gg` (go-to-top) still fires **instantly** on the
/// keystroke — the `command_status` oracle releases the second `g` as a built-in
/// under the `g`-prefix collision, with no idle flush or following key.
#[tokio::test]
async fn core_gg_is_instant_under_g_prefix_maps() {
    let dir = temp_dir("keymap_g_prefix_gg");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'gd', function() end)\n\
         vim.keymap.set('n', 'gD', function() end)\n\
         vim.keymap.set('n', 'gr', function() end)\n",
    )
    .await;

    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    assert_eq!(cursor(&rpc).await.0, 3, "cursor starts on the last line");

    feed(&rpc, "gg");
    assert_eq!(
        cursor(&rpc).await.0,
        1,
        "gg jumped to the top instantly despite the gd/gD/gr maps"
    );
}

/// The `gg`-then-operator sequences (`ggdG`, `dgg`) stay correct under `g`-prefix
/// maps: the oracle releases the built-in `gg` whole, so the operator that follows
/// binds to it, not to `gd`.
#[tokio::test]
async fn gg_operator_sequences_survive_g_prefix_maps() {
    let g_maps = "vim.keymap.set('n', 'gd', function() end)\n\
                  vim.keymap.set('n', 'gD', function() end)\n\
                  vim.keymap.set('n', 'gr', function() end)\n";

    // `ggdG`: to the top, then delete to the bottom — empties the buffer.
    let dir = temp_dir("keymap_g_prefix_gg_op");
    let (rpc, _incoming) = start_with_config(&dir, g_maps).await;
    feed(&rpc, "iline1<CR>line2<CR>line3<Esc>");
    feed(&rpc, "ggdG");
    assert_eq!(lines(&rpc).await, vec![""], "ggdG deleted every line");

    // `dgg`: delete from the last line up to the top — also empties it.
    let dir2 = temp_dir("keymap_g_prefix_dgg");
    let (rpc2, _i2) = start_with_config(&dir2, g_maps).await;
    feed(&rpc2, "iline1<CR>line2<CR>line3<Esc>");
    feed(&rpc2, "dgg");
    assert_eq!(lines(&rpc2).await, vec![""], "dgg deleted to the top");
}

// ===== count consumption by a Lua RHS ======================================

/// A count typed before a **Lua** mapping is the mapping's *argument*: it is
/// published as `v:count` / `v:count1` and then **consumed**, so whatever the
/// function executes starts from a clean pending command.
///
/// Regression: the pending state was only cleared *after* the RHS ran, so a
/// count the function itself fed concatenated onto the typed one — the common
/// `<C-o>` wrapper (`vim.cmd("normal! " .. vim.v.count1 .. "\15")` under
/// `3<C-o>`) fed `3` on top of the pending `3` and jumped back 33 places, i.e.
/// silently did nothing. A `normal!` with no count of its own inherited the
/// typed count instead of running once.
#[tokio::test]
async fn a_lua_rhs_consumes_the_typed_count_before_its_effects_run() {
    let dir = temp_dir("keymap_count_consumed");
    let (rpc, _incoming) = start_with_config(
        &dir,
        r#"
          vim.keymap.set("n", "<leader>j", function()
            _G.count1 = vim.v.count1
            vim.cmd("normal! " .. vim.v.count1 .. "j")
          end)
          vim.keymap.set("n", "<leader>k", function()
            vim.cmd("normal! k") -- no count of its own
          end)
        "#,
    )
    .await;

    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<CR>f<CR>g<Esc>gg"); // seven lines, cursor on 1
    assert_eq!(cursor(&rpc).await.0, 1);

    // The RHS sees the typed count …
    feed(&rpc, "3\\j");
    assert_eq!(
        lua_u64(&rpc, "return _G.count1").await,
        Some(3),
        "the Lua RHS reads the typed count as v:count1"
    );
    // … and its own `normal! 3j` moves exactly 3 lines (not 33 → clamped/no-op).
    assert_eq!(cursor(&rpc).await.0, 4, "normal! 3j moved three lines");

    // A `normal!` with no count of its own runs once — the typed count was
    // already consumed by the mapping, so it must not leak in.
    feed(&rpc, "3\\k");
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "normal! k moved one line, not three"
    );

    // And nothing leaks into the *next* command either.
    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await.0, 4, "the following j moved one line");
}

/// The counterpart: a **string** RHS still gets the count prefixed to it, as in
/// vim (`3x` with `x` mapped to `dd` deletes three lines). Only the Lua-function
/// arm consumes the count up front.
#[tokio::test]
async fn a_string_rhs_still_takes_the_typed_count() {
    let dir = temp_dir("keymap_count_string_rhs");
    let (rpc, _incoming) =
        start_with_config(&dir, "vim.keymap.set('n', 'X', 'dd', { remap = false })\n").await;

    feed(&rpc, "ia<CR>b<CR>c<CR>d<Esc>gg");
    feed(&rpc, "3X");
    assert_eq!(lines(&rpc).await, vec!["d"], "3X deleted three lines");
}

// ===== `<C-i>`/`<Tab>`-class notation aliases ==============================

/// The four Ctrl-chords that share a byte with a named terminal key — `<C-i>`
/// (0x09), `<C-m>` (0x0d), `<C-[>` (0x1b), `<C-h>` (0x08) — are canonicalized to
/// that named key when notation is parsed, as vim does. Either spelling on the
/// mapping LHS therefore matches the key the client actually sends.
///
/// Regression: they were distinct keys in the keymap layer, while the TUI can
/// only ever *produce* the named one (crossterm decodes 0x09 as `KeyCode::Tab`,
/// no CONTROL modifier). A `vim.keymap.set("n", "<C-i>", …)` was a mapping that
/// could never fire — silently dead, with the built-in acting in its place.
#[tokio::test]
async fn ctrl_chord_aliases_canonicalize_to_the_named_key() {
    // (LHS spelling, the key notation a client actually sends for it)
    for (lhs, sent) in [
        ("<C-i>", "<Tab>"),
        ("<Tab>", "<C-i>"),
        ("<C-m>", "<CR>"),
        ("<CR>", "<C-m>"),
        ("<C-[>", "<Esc>"),
        ("<Esc>", "<C-[>"),
        ("<C-h>", "<BS>"),
        ("<BS>", "<C-h>"),
    ] {
        let dir = temp_dir("keymap_ctrl_alias");
        let (rpc, _incoming) = start_with_config(
            &dir,
            &format!("_G.fired = 0\nvim.keymap.set('n', '{lhs}', function() _G.fired = _G.fired + 1 end)\n"),
        )
        .await;

        feed(&rpc, sent);
        assert_eq!(
            lua_u64(&rpc, "return _G.fired").await,
            Some(1),
            "a map on {lhs} fires for {sent} — they are one key with two names"
        );
    }
}

/// The kitty keyboard protocol's real payoff: `<S-CR>` / `<C-CR>` are their OWN keys
/// (`<Enter>` + a modifier the fold never touches), distinct from `<CR>` — and so
/// from a `<C-m>` map, which folds onto `<CR>` (`<C-m>` ≡ `<CR>`, as in vim/neovim).
/// A protocol-capable terminal delivers Shift/Ctrl+Enter as these, and the map must
/// not swallow them: pressing `<S-CR>`/`<C-CR>` inserts a line, it does not fire the
/// `<CR>` mapping. (Without the protocol the terminal can't send them at all — they
/// arrive as a plain `<CR>` and do fire it; enabling the protocol is what makes them
/// distinguishable, see the TUI client.)
#[tokio::test]
async fn shift_and_ctrl_enter_are_distinct_from_a_cr_map() {
    let dir = temp_dir("keymap_scr_distinct");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.fired = 0\nvim.keymap.set('i', '<C-m>', function() _G.fired = _G.fired + 1 end)\n",
    )
    .await;
    feed(&rpc, "i"); // insert mode, where the map lives

    // A plain `<CR>` fires the `<C-m>` map — they are one key (vim's ASCII aliasing).
    feed(&rpc, "<CR>");
    assert_eq!(
        lua_u64(&rpc, "return _G.fired").await,
        Some(1),
        "<CR> fires the <C-m> map (the two are folded together)"
    );

    // `<S-CR>` / `<C-CR>` carry a modifier on `<Enter>` the fold never collapses, so
    // they are distinct keys and do NOT fire the `<C-m>`/`<CR>` map.
    feed(&rpc, "<S-CR>");
    feed(&rpc, "<C-CR>");
    assert_eq!(
        lua_u64(&rpc, "return _G.fired").await,
        Some(1),
        "<S-CR>/<C-CR> are their own keys, not the <C-m>/<CR> mapping"
    );
}

/// Start a server with `init_lua` but attach a UI that declares the **kitty
/// keyboard protocol** active, so the four Ctrl-chords are NOT folded onto their
/// named twins — the modern-terminal path.
async fn start_with_config_kbd(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = bemtvi_test_harness::spawn(config_init(dir, init_lua));
    bemtvi_test_harness::attach_keyboard_protocol(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// With the kitty keyboard protocol on, the four Ctrl-chords are kept DISTINCT from
/// their named twins: a `<C-i>` map fires only for a `<C-i>` the terminal can now
/// deliver, and a plain `<Tab>` does *not* trigger it (nor vice versa). This is the
/// modern-terminal counterpart to `ctrl_chord_aliases_canonicalize_to_the_named_key`
/// (which covers the legacy fold when the protocol is off).
#[tokio::test]
async fn ctrl_chords_stay_distinct_under_the_keyboard_protocol() {
    // (LHS spelling, the twin key that must NOT trigger it, the key that must)
    for (lhs, other, same) in [
        ("<C-i>", "<Tab>", "<C-i>"),
        ("<Tab>", "<C-i>", "<Tab>"),
        ("<C-m>", "<CR>", "<C-m>"),
        ("<CR>", "<C-m>", "<CR>"),
        ("<C-[>", "<Esc>", "<C-[>"),
        ("<C-h>", "<BS>", "<C-h>"),
    ] {
        let dir = temp_dir("keymap_ctrl_distinct");
        let (rpc, _incoming) = start_with_config_kbd(
            &dir,
            &format!("_G.fired = 0\nvim.keymap.set('n', '{lhs}', function() _G.fired = _G.fired + 1 end)\n"),
        )
        .await;

        feed(&rpc, other);
        assert_eq!(
            lua_u64(&rpc, "return _G.fired").await,
            Some(0),
            "{other} must NOT fire the {lhs} map — the protocol keeps them apart"
        );
        feed(&rpc, same);
        assert_eq!(
            lua_u64(&rpc, "return _G.fired").await,
            Some(1),
            "{same} fires its own {lhs} map"
        );
    }
}

/// The user's real case: on a protocol-capable terminal, `<C-i>` and `<Tab>` can be
/// bound to *different* actions and each fires only for its own key.
#[tokio::test]
async fn ctrl_i_and_tab_bind_independently_under_the_protocol() {
    let dir = temp_dir("keymap_ci_tab_split");
    let (rpc, _incoming) = start_with_config_kbd(
        &dir,
        "_G.ci = 0\n_G.tab = 0\n\
         vim.keymap.set('n', '<C-i>', function() _G.ci = _G.ci + 1 end)\n\
         vim.keymap.set('n', '<Tab>', function() _G.tab = _G.tab + 1 end)\n",
    )
    .await;

    feed(&rpc, "<C-i>");
    feed(&rpc, "<Tab><Tab>");
    assert_eq!(
        lua_u64(&rpc, "return _G.ci").await,
        Some(1),
        "<C-i> map fired once"
    );
    assert_eq!(
        lua_u64(&rpc, "return _G.tab").await,
        Some(2),
        "<Tab> map fired twice, independently of <C-i>"
    );
}

/// The fold applies only to an *otherwise-unmodified* Ctrl-chord: `<C-S-i>` and
/// `<C-A-i>` carry a modifier a terminal can distinguish (kitty keyboard
/// protocol), so they stay their own keys rather than losing the extra modifier.
#[tokio::test]
async fn a_further_modified_ctrl_chord_does_not_fold() {
    let dir = temp_dir("keymap_ctrl_alias_mod");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.fired = 0\nvim.keymap.set('n', '<C-A-i>', function() _G.fired = _G.fired + 1 end)\n",
    )
    .await;

    feed(&rpc, "<Tab>");
    assert_eq!(
        lua_u64(&rpc, "return _G.fired").await,
        Some(0),
        "<Tab> did not fire the <C-A-i> map"
    );
    feed(&rpc, "<C-A-i>");
    assert_eq!(
        lua_u64(&rpc, "return _G.fired").await,
        Some(1),
        "<C-A-i> is still its own key"
    );
}

/// Typeahead must be parsed the way the client's own keys are. Under the protocol a
/// mapping's LHS `<C-h>` compiles to a distinct key, so folding a *fed* `<C-h>` onto
/// `<BS>` would make `btv._feedkeys` unable to reach a map the same session installed —
/// which is exactly how a plugin replays a key (a lazy `keys` trigger, a test's
/// `t:feed`).
#[tokio::test]
async fn fed_keys_are_parsed_like_typed_keys_under_the_protocol() {
    let dir = temp_dir("keymap_feed_protocol");
    let (rpc, _incoming) = start_with_config_kbd(
        &dir,
        "_G.ch = 0\n_G.bs = 0\n\
         vim.keymap.set('n', '<C-h>', function() _G.ch = _G.ch + 1 end)\n\
         vim.keymap.set('n', '<BS>', function() _G.bs = _G.bs + 1 end)\n",
    )
    .await;

    exec_lua(&rpc, "btv._feedkeys('<C-h>', true, false)").await;
    barrier(&rpc).await;
    assert_eq!(
        (
            lua_u64(&rpc, "return _G.ch").await,
            lua_u64(&rpc, "return _G.bs").await
        ),
        (Some(1), Some(0)),
        "a fed <C-h> reaches the <C-h> map, not the <BS> one"
    );

    exec_lua(&rpc, "btv._feedkeys('<BS>', true, false)").await;
    barrier(&rpc).await;
    assert_eq!(
        (
            lua_u64(&rpc, "return _G.ch").await,
            lua_u64(&rpc, "return _G.bs").await
        ),
        (Some(1), Some(1)),
        "and a fed <BS> reaches its own map"
    );
}

/// The legacy half: with no protocol declared, the fold applies on both sides, so a
/// fed `<C-h>` and a fed `<BS>` are the same key and hit the one map.
#[tokio::test]
async fn fed_ctrl_h_folds_onto_bs_without_the_protocol() {
    let dir = temp_dir("keymap_feed_legacy");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.bs = 0\nvim.keymap.set('n', '<BS>', function() _G.bs = _G.bs + 1 end)\n",
    )
    .await;

    exec_lua(&rpc, "btv._feedkeys('<C-h>', true, false)").await;
    barrier(&rpc).await;
    assert_eq!(
        lua_u64(&rpc, "return _G.bs").await,
        Some(1),
        "a legacy terminal cannot tell them apart, so the fed <C-h> IS <BS>"
    );
}

// ===== a string RHS that ends mid-command ==================================

/// A `noremap` string RHS whose keys stop at a **prefix** leaves the editor
/// mid-command, exactly as typing those keys would: the next key completes it.
/// Vim's `nnoremap X d` makes `Xj` a two-line delete.
///
/// Regression: the fire path cleared the pending command *unconditionally* once
/// the RHS had run — to consume the count/register typed ahead of the mapping —
/// which also wiped the operator/prefix stage the RHS had just built. `X` then
/// `j` deleted nothing at all, and the following key ran as a fresh command.
#[tokio::test]
async fn a_string_rhs_ending_in_an_operator_stays_pending() {
    let dir = temp_dir("keymap_rhs_operator_pending");
    let (rpc, _incoming) =
        start_with_config(&dir, "vim.keymap.set('n', 'X', 'd', { remap = false })\n").await;

    feed(&rpc, "ia<CR>b<CR>c<CR>d<Esc>gg");
    feed(&rpc, "X");
    feed(&rpc, "j");
    assert_eq!(
        lines(&rpc).await,
        vec!["c", "d"],
        "the operator the RHS armed took the next key as its motion"
    );
}

/// The same for a built-in *prefix* rather than an operator — and the case the
/// browser needs, where `<A-w>` stands in for a `<C-w>` the browser keeps for
/// itself. `<A-w>` must leave the window-command prefix pending so the next key
/// completes the split.
#[tokio::test]
async fn a_string_rhs_ending_in_the_window_prefix_stays_pending() {
    let dir = temp_dir("keymap_rhs_window_pending");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', '<A-w>', '<C-w>', { remap = false })\n",
    )
    .await;

    assert_eq!(lua_u64(&rpc, "return #btv.win.list()").await, Some(1));
    feed(&rpc, "<A-w>");
    feed(&rpc, "s");
    assert_eq!(
        lua_u64(&rpc, "return #btv.win.list()").await,
        Some(2),
        "<A-w> left <C-w> pending, so the following s split the window"
    );
}

/// …and the count typed ahead of such a mapping still prefixes the RHS's keys
/// rather than being consumed: `3X` with `X` mapped to `d` is `3d`, so the
/// following `j` deletes four lines.
#[tokio::test]
async fn a_count_ahead_of_a_prefix_rhs_prefixes_it() {
    let dir = temp_dir("keymap_rhs_operator_count");
    let (rpc, _incoming) =
        start_with_config(&dir, "vim.keymap.set('n', 'X', 'd', { remap = false })\n").await;

    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<CR>f<Esc>gg");
    feed(&rpc, "3X");
    feed(&rpc, "j");
    assert_eq!(
        lines(&rpc).await,
        vec!["e", "f"],
        "3X armed `3d`, and j completed it over four lines"
    );
}

/// The counterpart the unconditional clear existed for: a string RHS that
/// consumes nothing must not let the count typed ahead of it leak into the next
/// command. `:noh<CR>` takes no count, so the `j` after `3X` moves one line.
#[tokio::test]
async fn a_count_ahead_of_a_complete_rhs_does_not_leak() {
    let dir = temp_dir("keymap_rhs_count_no_leak");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.keymap.set('n', 'X', ':noh<CR>', { remap = false })\n",
    )
    .await;

    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<CR>f<Esc>gg");
    assert_eq!(cursor(&rpc).await.0, 1);
    feed(&rpc, "3X");
    feed(&rpc, "j");
    assert_eq!(
        cursor(&rpc).await.0,
        2,
        "the consumed count did not prefix the following j"
    );
}
