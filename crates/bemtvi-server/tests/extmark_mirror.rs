//! Black-box tests for the **incremental** extmark mirror. The full `ExtmarkMirror`
//! list (positions *and* decorations) is re-serialized only when the store's
//! structural generation moves — a mark set, deleted, or cleared. An edit refreshes
//! positions alone, and only for marks inside the byte window the edit could have
//! moved *in row/column terms*: sliding a byte anchor is not the same as changing a
//! row/col, so typing on one line leaves every later mark's `(row, col)` identical.
//!
//! That window is the risky part — a mark wrongly excluded goes silently stale — so
//! these tests pin each way a mark can be affected. See
//! `docs/plans/2026-08-07-incremental-buffer-mirror.md`.

use bemtvi_rpc::Rpc;
use bemtvi_test_harness::{exec_lua, feed, start_with_file as open};

fn sample(n: usize) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

/// Create a namespace and return the Lua expression naming it.
async fn ns(rpc: &Rpc) {
    exec_lua(rpc, "_G.ns = btv.ns.create('test')").await;
}

/// Set one mark, returning its id.
async fn mark(rpc: &Rpc, row: usize, col: usize, extra: &str) -> u64 {
    let v = exec_lua(
        rpc,
        &format!("return btv.buf.set_extmark(0, _G.ns, {row}, {col}, {{ {extra} }})"),
    )
    .await;
    v.as_u64().expect("set_extmark returned no id")
}

/// The mirror's `(row, col)` for mark `id`.
async fn pos(rpc: &Rpc, id: u64) -> (i64, i64) {
    let v = exec_lua(
        rpc,
        &format!(
            "for _, e in ipairs(btv.buf.extmarks(0, _G.ns, 0, -1, {{}})) do
               if e[1] == {id} then return e[2] .. ',' .. e[3] end
             end
             return 'missing'"
        ),
    )
    .await;
    let s = v.as_str().unwrap_or("missing").to_string();
    let (r, c) = s
        .split_once(',')
        .unwrap_or_else(|| panic!("mark {id}: {s}"));
    (r.parse().unwrap(), c.parse().unwrap())
}

/// A detail field of mark `id`, as a string (`"nil"` when absent).
async fn detail(rpc: &Rpc, id: u64, path: &str) -> String {
    let v = exec_lua(
        rpc,
        &format!(
            "for _, e in ipairs(btv.buf.extmarks(0, _G.ns, 0, -1, {{ details = true }})) do
               if e[1] == {id} then local d = e[4] return tostring(d and d.{path}) end
             end
             return 'missing'"
        ),
    )
    .await;
    v.as_str().unwrap_or("missing").to_string()
}

/// Every mark's `id:row,col`, in mirror order.
async fn snapshot(rpc: &Rpc) -> String {
    let v = exec_lua(
        rpc,
        "local o = {}
         for _, e in ipairs(btv.buf.extmarks(0, _G.ns, 0, -1, {})) do
           o[#o + 1] = e[1] .. ':' .. e[2] .. ',' .. e[3]
         end
         return table.concat(o, ' ')",
    )
    .await;
    v.as_str().unwrap_or("").to_string()
}

/// The incrementally-refreshed mirror must equal what a full re-serialize produces.
/// Setting and immediately deleting a throwaway mark moves the store's structural
/// generation, so the next push rebuilds every mark from the authoritative core store
/// — comparing the two readings catches a stale incremental refresh without the test
/// having to predict extmark gravity semantics for itself.
async fn assert_matches_full_rebuild(rpc: &Rpc) {
    let incremental = snapshot(rpc).await;
    exec_lua(
        rpc,
        "local throwaway = btv.buf.set_extmark(0, _G.ns, 0, 0, {})
         btv.buf.del_extmark(0, _G.ns, throwaway)",
    )
    .await;
    let full = snapshot(rpc).await;
    assert_eq!(
        incremental, full,
        "the incremental mirror diverged from a full rebuild"
    );
}

#[tokio::test]
async fn a_mark_past_a_same_line_edit_keeps_its_position() {
    // Typing on line 5 slides the byte anchor of the mark on line 100, but not its
    // row or column — the window must exclude it, and it must still read correctly.
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(&rpc, 100, 3, "").await;
    feed(&rpc, "6GIxyz<Esc>");
    assert_eq!(pos(&rpc, m).await, (100, 3));
}

#[tokio::test]
async fn a_mark_past_an_inserted_line_shifts_down() {
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(&rpc, 100, 3, "").await;
    feed(&rpc, "6Gonew<Esc>");
    assert_eq!(pos(&rpc, m).await, (101, 3));
}

#[tokio::test]
async fn a_mark_past_deleted_lines_shifts_up() {
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(&rpc, 100, 3, "").await;
    feed(&rpc, "6G3dd");
    assert_eq!(pos(&rpc, m).await, (97, 3));
}

#[tokio::test]
async fn a_mark_on_the_edited_line_moves_its_column() {
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(&rpc, 5, 3, "").await;
    feed(&rpc, "6GIxyz<Esc>");
    assert_eq!(pos(&rpc, m).await, (5, 6));
}

#[tokio::test]
async fn a_range_mark_ending_on_the_edited_line_updates_its_end() {
    // The start is on an untouched earlier line but the END is inside the edit's
    // window, so the mark must still be refreshed — the window test is per-edge.
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(&rpc, 4, 0, "end_row = 5, end_col = 4").await;
    feed(&rpc, "6GIxyz<Esc>");
    assert_eq!(detail(&rpc, m, "end_col").await, "7");
    assert_eq!(pos(&rpc, m).await, (4, 0));
}

#[tokio::test]
async fn decorations_survive_an_edit() {
    // The whole point of the structural gate: an edit must not re-serialize (or lose)
    // the payload fields, which `ExtmarkStore::shift` never touches.
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(
        &rpc,
        100,
        0,
        "hl_group = 'ErrorMsg', priority = 42, sign_text = '>>'",
    )
    .await;
    feed(&rpc, "6GIxyz<Esc>");
    assert_eq!(detail(&rpc, m, "hl_group").await, "ErrorMsg");
    assert_eq!(detail(&rpc, m, "priority").await, "42");
    assert_eq!(detail(&rpc, m, "sign_text").await, ">>");
}

#[tokio::test]
async fn a_mark_added_after_an_edit_is_visible() {
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let a = mark(&rpc, 10, 0, "").await;
    feed(&rpc, "6GIxyz<Esc>");
    let b = mark(&rpc, 20, 1, "").await;
    assert_eq!(pos(&rpc, a).await, (10, 0));
    assert_eq!(pos(&rpc, b).await, (20, 1));
}

#[tokio::test]
async fn a_deleted_mark_disappears() {
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let m = mark(&rpc, 10, 0, "").await;
    exec_lua(&rpc, &format!("btv.buf.del_extmark(0, _G.ns, {m})")).await;
    feed(&rpc, "6GIxyz<Esc>");
    let v = exec_lua(&rpc, "return #btv.buf.extmarks(0, _G.ns, 0, -1, {})").await;
    assert_eq!(v.as_u64(), Some(0));
}

#[tokio::test]
async fn many_edits_keep_every_mark_exact() {
    // A sustained run: after 60 mixed edits every mark must still report the row the
    // text actually puts it on. Marks are one per line, so the expected row is
    // derivable — this is what catches a window that drifts.
    let (rpc, _inc) = open(&sample(300)).await;
    ns(&rpc).await;
    exec_lua(
        &rpc,
        "_G.ids = {}
         for i = 0, 299 do _G.ids[i] = btv.buf.set_extmark(0, _G.ns, i, 0, {}) end",
    )
    .await;
    // 20 same-line edits (no row change), then 10 line insertions, then 10 deletions.
    feed(&rpc, "100G");
    for _ in 0..20 {
        feed(&rpc, "Ix<Esc>");
    }
    for _ in 0..10 {
        feed(&rpc, "50Goadded<Esc>");
    }
    for _ in 0..10 {
        feed(&rpc, "20Gdd");
    }
    // Net: +10 lines above 100 from the inserts, -10 from the deletes → mark 200 sits
    // back at row 200. Assert against the buffer itself rather than arithmetic.
    let ok = exec_lua(
        &rpc,
        "local bad = {}
         for i = 0, 299 do
           local want = nil
           for _, e in ipairs(btv.buf.extmarks(0, _G.ns, 0, -1, {})) do
             if e[1] == _G.ids[i] then want = e[2] end
           end
           if want == nil then bad[#bad+1] = i end
         end
         return #bad",
    )
    .await;
    assert_eq!(ok.as_u64(), Some(0), "some marks vanished from the mirror");
    // The mark originally on line 250 is below every edit: +10 inserted, -10 deleted.
    let m250 = exec_lua(&rpc, "return _G.ids[250]").await.as_u64().unwrap();
    assert_eq!(pos(&rpc, m250).await.0, 250);
}

#[tokio::test]
async fn typing_in_a_mark_heavy_buffer_does_not_scale_with_the_marks() {
    // The regression guard. Re-serializing every mark on every push cost two rope
    // lookups per mark per keystroke: this run took ~29s that way and ~3s once the
    // structural gate and the dirty window landed. The bound has ~3x margin against
    // the old behavior returning. (The ~2.5s that remains over a mark-free buffer is
    // core `ExtmarkStore::shift` and the redraw projection, not the mirror — measured
    // by ablation, and a separate concern.)
    let (rpc, _inc) = open(&sample(5_000)).await;
    ns(&rpc).await;
    exec_lua(&rpc, "btv.on('CursorMoved', function() end)").await;
    exec_lua(
        &rpc,
        "for i = 0, 4999 do
           btv.buf.set_extmark(0, _G.ns, i, 0, { hl_group = 'Comment', end_row = i, end_col = 4 })
         end",
    )
    .await;
    let n = exec_lua(&rpc, "return #btv.buf.extmarks(0, _G.ns, 0, -1, {})").await;
    assert_eq!(
        n.as_u64(),
        Some(5_000),
        "the marks under test were not created"
    );

    feed(&rpc, "2500GI");
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for _ in 0..300 {
        feed(&rpc, "z");
    }
    let _ = exec_lua(&rpc, "return 1").await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "300 keystrokes with 5000 extmarks took {elapsed:?} — the mirror is \
         re-serializing marks per keystroke again",
    );
}

#[tokio::test]
async fn several_keys_in_one_message_do_not_scale_with_the_marks() {
    // Every key in one input message lands in one mirror batch, so a client that
    // coalesces fast typing (or a mapping, or `.`) hands the mirror several edits at
    // once. Those batches used to refresh every mark's position: ~67ms per two-key
    // message with 5000 marks, against ~3ms for a single key. Folding the batch into
    // one row span keeps it bounded like a single edit. The bound has ~3x margin
    // against the old cost.
    let (rpc, _inc) = open(&sample(5_000)).await;
    ns(&rpc).await;
    exec_lua(
        &rpc,
        "for i = 0, 4999 do
           btv.buf.set_extmark(0, _G.ns, i, 0, { hl_group = 'Comment', end_row = i, end_col = 4 })
         end",
    )
    .await;

    feed(&rpc, "2500GI");
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for _ in 0..450 {
        feed(&rpc, "ab");
    }
    let _ = exec_lua(&rpc, "return 1").await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "450 two-key messages with 5000 extmarks took {elapsed:?}: a multi-edit \
         batch is refreshing every mark again",
    );
}

#[tokio::test]
async fn a_batch_that_adds_a_line_moves_the_marks_on_and_below_it() {
    // Three edits in one message: `a`, a line break, `b`. The mark after the break
    // point rides onto the new line, and every later mark shifts down.
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let on = mark(&rpc, 5, 3, "").await;
    let before = mark(&rpc, 5, 0, "").await;
    let below = mark(&rpc, 100, 2, "").await;
    feed(&rpc, "6G0llia<CR>b<Esc>");
    let text = exec_lua(&rpc, "return table.concat(btv.buf.lines(0, 5, 7), '|')").await;
    assert_eq!(text.as_str(), Some("lia|bne 5"));
    assert_eq!(pos(&rpc, before).await, (5, 0));
    assert_eq!(pos(&rpc, on).await, (6, 2));
    assert_eq!(pos(&rpc, below).await, (101, 2));
    assert_matches_full_rebuild(&rpc).await;
}

#[tokio::test]
async fn a_batch_over_two_rows_moves_the_columns_on_both() {
    // Edits on rows 5 and 9 in one batch, no row change: the marks on both rows
    // move their columns, and the marks around and past them stay put. Queued Lua
    // edits drain together; typed keys would not, because leaving Insert pushes
    // the mirror in between.
    let (rpc, _inc) = open(&sample(200)).await;
    ns(&rpc).await;
    let first = mark(&rpc, 5, 3, "").await;
    let between = mark(&rpc, 7, 3, "").await;
    let last = mark(&rpc, 9, 3, "").await;
    let past = mark(&rpc, 10, 3, "").await;
    let spanning = mark(&rpc, 2, 0, "end_row = 9, end_col = 4").await;
    exec_lua(
        &rpc,
        "btv.buf.set_text(0, 5, 0, 5, 0, { 'x' }) btv.buf.set_text(0, 9, 0, 9, 0, { 'yy' })",
    )
    .await;
    assert_eq!(pos(&rpc, first).await, (5, 4));
    assert_eq!(pos(&rpc, between).await, (7, 3));
    assert_eq!(pos(&rpc, last).await, (9, 5));
    assert_eq!(pos(&rpc, past).await, (10, 3));
    assert_eq!(detail(&rpc, spanning, "end_col").await, "6");
    assert_matches_full_rebuild(&rpc).await;
}

#[tokio::test]
async fn word_sized_undo_steps_stay_cheap_in_a_mark_heavy_buffer() {
    // Typing commits an undo node at each step boundary, and a node snapshots the
    // extmark store, O(marks) per commit. `z<BS>` flips between typing and
    // deleting on every key, so this run commits 300 nodes over 5000 marks. The
    // run takes ~1s, of which the commits are ~0.5s. The bound catches a commit
    // growing to ~30ms: a snapshot that stopped being cheap.
    let (rpc, _inc) = open(&sample(5_000)).await;
    ns(&rpc).await;
    exec_lua(
        &rpc,
        "for i = 0, 4999 do
           btv.buf.set_extmark(0, _G.ns, i, 0, { hl_group = 'Comment', end_row = i, end_col = 4 })
         end",
    )
    .await;

    feed(&rpc, "2500GI");
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for _ in 0..150 {
        feed(&rpc, "z<BS>");
    }
    let _ = exec_lua(&rpc, "return 1").await;
    let elapsed = started.elapsed();
    let seq = exec_lua(&rpc, "return btv.undotree.get().seq_last").await;
    assert!(
        seq.as_u64().unwrap_or(0) >= 300,
        "the run did not break into undo steps: seq_last {seq:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "300 keystrokes (300 undo steps) with 5000 extmarks took {elapsed:?}",
    );
}

#[tokio::test]
async fn a_row_preserving_batch_that_shrinks_the_buffer_does_not_crash() {
    // The exact shape that crashed `lsp_features` (a formatter applying several edits
    // in one tick). Queued buffer ops drain together, so their byte offsets are each
    // expressed against a different intermediate buffer: here a late edit sits at a
    // large offset, then a row-preserving replacement shrinks the buffer well below
    // it. Deriving a window across the batch resolves that stale offset against the
    // final rope and panics the server — `index is out of bounds` in `line_start`.
    let (rpc, _inc) = open(&sample(300)).await;
    ns(&rpc).await;
    mark(&rpc, 100, 0, "").await;
    mark(&rpc, 250, 0, "").await;
    exec_lua(
        &rpc,
        "btv.buf.set_text(0, 290, 0, 290, 4, { 'Q' })
         local blanks = {}
         for i = 1, 270 do blanks[i] = '' end
         btv.buf.set_lines(0, 10, 280, false, blanks)",
    )
    .await;
    // Reaching here at all means the server survived; the mirror must also be right.
    assert_eq!(
        exec_lua(&rpc, "return btv.buf.line_count(0)")
            .await
            .as_u64(),
        Some(300)
    );
    assert_matches_full_rebuild(&rpc).await;
}

#[tokio::test]
async fn a_batch_whose_row_changes_cancel_out_still_moves_the_marks_between() {
    // One edit adds a line high up and another removes one far below, in a single
    // tick: the NET row change is zero, but every mark between them really did shift
    // down by one. Bounding a window on the net delta would report them unmoved.
    let (rpc, _inc) = open(&sample(300)).await;
    ns(&rpc).await;
    let between = mark(&rpc, 100, 0, "").await;
    mark(&rpc, 250, 0, "").await;
    exec_lua(&rpc, "btv.cmd('10copy 10') btv.cmd('200delete')").await;
    assert_eq!(pos(&rpc, between).await, (101, 0));
    assert_matches_full_rebuild(&rpc).await;
}

/// An extmark placed on a freshly-loaded buffer survives undoing the first edit
/// made after it. Undo restores the snapshot the node it returns to was committed
/// with, and the root node is snapshotted at buffer *load* — before any
/// `BufReadPost` handler decorates it — so every mark a plugin placed there was
/// wiped by the first `u`.
#[tokio::test]
async fn an_extmark_placed_after_load_survives_the_first_undo() {
    let (rpc, _incoming) = open("alpha\nbravo\ncharlie\n").await;
    // Decorate line 2, the way a `BufReadPost` handler would.
    exec_lua(
        &rpc,
        r#"_G.ns = vim.api.nvim_create_namespace("undo_demo")
           vim.api.nvim_buf_set_extmark(0, _G.ns, 1, 0, {
             virt_text = { { " <- note", "Comment" } }, virt_text_pos = "eol",
           })"#,
    )
    .await;
    let before = exec_lua(
        &rpc,
        "return #vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {})",
    )
    .await;
    assert_eq!(before.as_i64(), Some(1), "the mark is placed");

    // The first edit, then undo it.
    feed(&rpc, "ggO<Esc>");
    let moved = exec_lua(
        &rpc,
        "local m = vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {}) return m[1] and m[1][2]",
    )
    .await;
    assert_eq!(moved.as_i64(), Some(2), "the mark rode the insert down");
    feed(&rpc, "u");
    let after = exec_lua(
        &rpc,
        "local m = vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {}) return m[1] and m[1][2]",
    )
    .await;
    assert_eq!(
        after.as_i64(),
        Some(1),
        "the mark rode back to the line it was on, rather than being wiped"
    );
}

/// Undo restores text and where marks sit, not which marks exist: the set of marks is
/// the plugins' to manage. A mark a plugin deleted after an edit stays deleted when
/// the edit is undone — undo used to swap in the whole mark store it snapshotted, so
/// it came back (a cleared diagnostic, a dismissed intro screen).
#[tokio::test]
async fn an_extmark_deleted_after_an_edit_stays_deleted_through_undo() {
    let (rpc, _incoming) = open("alpha\nbravo\ncharlie\n").await;
    exec_lua(
        &rpc,
        r#"_G.ns = vim.api.nvim_create_namespace("undo_deleted")
           vim.api.nvim_buf_set_extmark(0, _G.ns, 1, 0, { virt_text = { { "note" } } })"#,
    )
    .await;
    // The plugin clears its mark as the user types — inside the very change that
    // undo then reverts.
    exec_lua(
        &rpc,
        "btv.on('TextChangedI', function() vim.api.nvim_buf_clear_namespace(0, _G.ns, 0, -1) end)",
    )
    .await;
    feed(&rpc, "ggOtyped<Esc>");
    let count = exec_lua(
        &rpc,
        "return #vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {})",
    )
    .await;
    assert_eq!(
        count.as_i64(),
        Some(0),
        "precondition: typing cleared the mark"
    );
    feed(&rpc, "u");
    let count = exec_lua(
        &rpc,
        "return #vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {})",
    )
    .await;
    assert_eq!(
        count.as_i64(),
        Some(0),
        "undo must not resurrect a deleted mark"
    );
    // Redo neither.
    feed(&rpc, "<C-r>");
    let count = exec_lua(
        &rpc,
        "return #vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {})",
    )
    .await;
    assert_eq!(count.as_i64(), Some(0), "redo must not resurrect it either");
}

/// The other half: a mark a plugin placed after an edit survives undoing that edit
/// (undo used to restore the snapshot from before it existed, wiping it).
#[tokio::test]
async fn an_extmark_added_after_an_edit_survives_undo() {
    let (rpc, _incoming) = open("alpha\nbravo\ncharlie\n").await;
    feed(&rpc, "ggO<Esc>");
    exec_lua(
        &rpc,
        r#"_G.ns = vim.api.nvim_create_namespace("undo_added")
           vim.api.nvim_buf_set_extmark(0, _G.ns, 3, 0, { virt_text = { { "note" } } })"#,
    )
    .await;
    feed(&rpc, "u");
    let lines = exec_lua(&rpc, "return btv.buf.line_count(0)").await;
    assert_eq!(lines.as_u64(), Some(3), "the edit is undone");
    let count = exec_lua(
        &rpc,
        "return #vim.api.nvim_buf_get_extmarks(0, _G.ns, 0, -1, {})",
    )
    .await;
    assert_eq!(
        count.as_i64(),
        Some(1),
        "undo must not wipe a mark placed after the edit"
    );
}

/// `clear_namespace(buf, ns, start, -1)` clears from `start` to the END — it must keep
/// the marks above `start`, in the same chunk (the mirror the read answers from) and
/// after the server has applied the clear.
#[tokio::test]
async fn clear_namespace_to_the_end_keeps_marks_above_the_start() {
    let (rpc, _inc) = open(&sample(20)).await;
    ns(&rpc).await;
    mark(&rpc, 0, 0, "").await;
    mark(&rpc, 8, 0, "").await;
    let same_chunk = exec_lua(
        &rpc,
        "btv.buf.clear_namespace(0, _G.ns, 5, -1)\n\
         return #btv.buf.extmarks(0, _G.ns, 0, -1, {})",
    )
    .await;
    assert_eq!(
        same_chunk.as_i64(),
        Some(1),
        "the row-0 mark survives in the mirror"
    );
    let later = exec_lua(&rpc, "return #btv.buf.extmarks(0, _G.ns, 0, -1, {})").await;
    assert_eq!(
        later.as_i64(),
        Some(1),
        "and after the server applied the clear"
    );
}

// ===== structural changes, per namespace =====================================
//
// A set / del / clear re-sends the namespaces it changed and leaves every other
// namespace's rows alone. See `docs/plans/2026-10-31-incremental-extmark-mirror.md`.
// These pin that the mirror still equals the core store after every way a
// namespace can change, so narrowing the push can't leave a row stale.

/// Every mirrored mark in the current buffer across all namespaces, with the
/// payload fields the mirror carries, one `ns/id@row,col-end_row,end_col[...]`
/// token per mark, sorted. Namespaces are named rather than numbered, so the
/// expected strings don't depend on allocation order.
async fn all_marks(rpc: &Rpc) -> String {
    let v = exec_lua(
        rpc,
        "local names = {}
         for name, id in pairs(btv._namespaces) do names[id] = name end
         local o = {}
         for _, e in ipairs(btv.buf.extmarks(0, -1, 0, -1, { details = true })) do
           local d = e[4]
           local f = {}
           for _, k in ipairs({ 'hl_group', 'sign_text', 'line_hl_group' }) do
             if d[k] ~= nil then f[#f + 1] = k .. '=' .. tostring(d[k]) end
           end
           if d.gutter_text then f[#f + 1] = 'gutter=' .. d.gutter_text[1][1] end
           o[#o + 1] = string.format('%s/%d@%d,%d-%s,%s[%s]', names[d.ns_id] or d.ns_id,
             e[1], e[2], e[3], tostring(d.end_row), tostring(d.end_col), table.concat(f, ' '))
         end
         table.sort(o)
         return table.concat(o, ' ')",
    )
    .await;
    v.as_str().unwrap_or("").to_string()
}

/// The mirror must equal the core store. Setting and deleting a throwaway mark in
/// every namespace the mirror knows forces each one to be re-read from the core, so
/// a row the incremental push left stale (or a namespace it failed to drop) shows up
/// as a difference between the two readings. The throwaway takes an id no test uses,
/// so it can't replace a decor provider's own mark 1.
async fn assert_mirror_is_the_core(rpc: &Rpc) {
    let incremental = all_marks(rpc).await;
    exec_lua(
        rpc,
        "local b = btv._resolve_bufnr(0)
         for nsid in pairs(btv._extmarks[b] or {}) do
           btv.buf.set_extmark(0, nsid, 0, 0, { id = 999999 })
           btv.buf.del_extmark(0, nsid, 999999)
         end",
    )
    .await;
    let reread = all_marks(rpc).await;
    assert_eq!(
        incremental, reread,
        "the mirror diverged from the core store"
    );
}

#[tokio::test]
async fn sets_deletes_and_clears_across_namespaces_keep_the_mirror_exact() {
    let (rpc, _inc) = open(&sample(20)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b, _G.c = btv.ns.create('a'), btv.ns.create('b'), btv.ns.create('c')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1, hl_group = 'Comment', end_row = 1, end_col = 3 })
         btv.buf.set_extmark(0, a, 2, 0, { id = 2, sign_text = '>>' })
         btv.buf.set_extmark(0, b, 3, 1, { id = 1, line_hl_group = 'Visual' })
         btv.buf.set_extmark(0, b, 4, 0, { id = 2, gutter_text = { { 'g4' } } })
         btv.buf.set_extmark(0, c, 5, 0, { id = 1 })
         btv.buf.set_extmark(0, c, 9, 0, { id = 2 })",
    )
    .await;
    assert_mirror_is_the_core(&rpc).await;
    // One change per namespace, each in its own chunk so each lands as its own push.
    exec_lua(&rpc, "btv.buf.del_extmark(0, b, 1)").await;
    exec_lua(&rpc, "btv.buf.clear_namespace(0, c, 8, -1)").await;
    exec_lua(
        &rpc,
        "btv.buf.set_extmark(0, a, 6, 2, { id = 2, hl_group = 'Error', end_row = 6, end_col = 4 })",
    )
    .await;
    assert_eq!(
        all_marks(&rpc).await,
        "a/1@1,0-1,3[hl_group=Comment] a/2@6,2-6,4[hl_group=Error] \
         b/2@4,0-nil,nil[gutter=g4] c/1@5,0-nil,nil[]"
    );
    assert_mirror_is_the_core(&rpc).await;
    // A text edit after the structural changes moves every namespace's marks.
    feed(&rpc, "ggOnew<Esc>");
    assert_eq!(
        all_marks(&rpc).await,
        "a/1@2,0-2,3[hl_group=Comment] a/2@7,2-7,4[hl_group=Error] \
         b/2@5,0-nil,nil[gutter=g4] c/1@6,0-nil,nil[]"
    );
    assert_mirror_is_the_core(&rpc).await;
}

#[tokio::test]
async fn a_reload_drops_every_namespace() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 1, 0, {})
         btv.buf.set_extmark(0, b, 2, 0, { sign_text = 'x' })",
    )
    .await;
    feed(&rpc, "ggx");
    bemtvi_test_harness::command(&rpc, "e!").await;
    assert_eq!(all_marks(&rpc).await, "", "`:e!` clears every mark");
    exec_lua(&rpc, "btv.buf.set_extmark(0, b, 3, 0, { id = 7 })").await;
    assert_eq!(all_marks(&rpc).await, "b/7@3,0-nil,nil[]");
    assert_mirror_is_the_core(&rpc).await;
}

#[tokio::test]
async fn undo_and_redo_keep_marks_added_and_deleted_after_the_history_point() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1 })
         btv.buf.set_extmark(0, a, 2, 0, { id = 2, hl_group = 'Comment', end_row = 2, end_col = 4 })
         btv.buf.set_extmark(0, b, 5, 0, { id = 1, sign_text = 's' })",
    )
    .await;
    feed(&rpc, "ggOnew<Esc>");
    exec_lua(
        &rpc,
        "btv.buf.del_extmark(0, a, 1)
         btv.buf.set_extmark(0, b, 8, 0, { id = 2 })",
    )
    .await;
    feed(&rpc, "u");
    assert_eq!(
        all_marks(&rpc).await,
        "a/2@2,0-2,4[hl_group=Comment] b/1@5,0-nil,nil[sign_text=s] b/2@7,4-nil,nil[]",
        "undo moves the marks back, keeps the one added (at its live byte, which the \
         undone line no longer pushes down) and doesn't bring back the one deleted"
    );
    assert_mirror_is_the_core(&rpc).await;
    feed(&rpc, "<C-r>");
    assert_eq!(
        all_marks(&rpc).await,
        "a/2@3,0-3,4[hl_group=Comment] b/1@6,0-nil,nil[sign_text=s] b/2@8,0-nil,nil[]",
    );
    assert_mirror_is_the_core(&rpc).await;
}

#[tokio::test]
async fn a_decor_provider_namespace_stays_exact_across_undo() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a = btv.ns.create('a')
         btv.buf.set_extmark(0, a, 4, 0, { id = 1 })
         btv.decor.provider { name = 'probe', on_range = function(ctx, publish)
           publish({ { ctx.top, 0, end_col = 1, hl = 'Comment' } })
         end }
         btv.decor.invalidate()",
    )
    .await;
    let published = "return #btv.buf.extmarks(0, -1, 0, -1, {}) == 2";
    assert!(
        bemtvi_test_harness::poll_true(&rpc, published).await,
        "the provider never published: {}",
        all_marks(&rpc).await
    );
    feed(&rpc, "ggOnew<Esc>");
    assert!(bemtvi_test_harness::poll_true(&rpc, published).await);
    feed(&rpc, "u");
    assert!(bemtvi_test_harness::poll_true(&rpc, published).await);
    assert_mirror_is_the_core(&rpc).await;
}

#[tokio::test]
async fn multi_cursor_undo_keeps_the_mirror_exact() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a = btv.ns.create('a')
         btv.buf.set_extmark(0, a, 1, 2, { id = 1 })
         btv.buf.set_extmark(0, a, 3, 0, { id = 2, hl_group = 'Comment', end_row = 3, end_col = 4 })",
    )
    .await;
    // Cursors on rows 0, 1 and 2, then type on all of them at once.
    feed(&rpc, "gg<A-c>jcjc<Esc>Ix<Esc>");
    assert_eq!(
        all_marks(&rpc).await,
        "a/1@1,3-nil,nil[] a/2@3,0-3,4[hl_group=Comment]",
        "the mark on a cursor row moves right by the typed character"
    );
    feed(&rpc, "<Esc>u");
    assert_eq!(
        all_marks(&rpc).await,
        "a/1@1,2-nil,nil[] a/2@3,0-3,4[hl_group=Comment]",
    );
    assert_mirror_is_the_core(&rpc).await;
}

#[tokio::test]
async fn a_namespace_cleared_to_empty_leaves_the_others() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1 })
         btv.buf.set_extmark(0, a, 2, 0, { id = 2 })
         btv.buf.set_extmark(0, b, 3, 0, { id = 1 })",
    )
    .await;
    exec_lua(&rpc, "btv.buf.clear_namespace(0, a, 0, -1)").await;
    assert_eq!(all_marks(&rpc).await, "b/1@3,0-nil,nil[]");
    // The emptied namespace takes marks again.
    exec_lua(&rpc, "btv.buf.set_extmark(0, a, 4, 0, { id = 3 })").await;
    assert_eq!(all_marks(&rpc).await, "a/3@4,0-nil,nil[] b/1@3,0-nil,nil[]");
    assert_mirror_is_the_core(&rpc).await;
}

/// A `set_extmark` that raises must leave no row behind for a later chunk to read.
/// An unknown `virt_text_pos` is rejected by the native converter, which ran after
/// the mirror write-through, and when nothing else in the buffer changed no push ever
/// overwrote the row: a mark the core never held stayed readable.
#[tokio::test]
async fn a_set_extmark_that_raises_leaves_no_row() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a = btv.ns.create('a')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1 })",
    )
    .await;
    let raised = exec_lua(
        &rpc,
        "local ok = pcall(btv.buf.set_extmark, 0, a, 2, 0,
           { id = 2, virt_text = { { 'v' } }, virt_text_pos = 'bogus' })
         return ok",
    )
    .await;
    assert_eq!(
        raised.as_bool(),
        Some(false),
        "an unknown virt_text_pos raises"
    );
    assert_eq!(all_marks(&rpc).await, "a/1@1,0-nil,nil[]");
}

/// The cost guard: re-setting a few marks in one namespace must not pay for every
/// mark in the buffer's other namespaces. A `CursorMoved` handler that republishes
/// 30 marks around the cursor (a cursor-word highlight) is timed with and without
/// 20 000 static marks in another namespace beside it.
async fn republish_cost(static_marks: usize) -> std::time::Duration {
    let (rpc, _inc) = open(&sample(20_000)).await;
    exec_lua(
        &rpc,
        &format!(
            "_G.s, _G.d = btv.ns.create('static'), btv.ns.create('dynamic')
             for i = 0, {} do btv.buf.set_extmark(0, s, i, 0, {{ hl_group = 'Comment', end_row = i, end_col = 4 }}) end
             btv.on('CursorMoved', function()
               local row = btv.cursor.get(0)[1] - 1
               btv.buf.clear_namespace(0, d, 0, -1)
               for i = 0, 29 do
                 btv.buf.set_extmark(0, d, row + i, 0, {{ hl_group = 'Search', end_row = row + i, end_col = 2 }})
               end
             end)",
            static_marks as i64 - 1
        ),
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return #btv.buf.extmarks(0, s, 0, -1, {})")
            .await
            .as_u64(),
        Some(static_marks as u64),
        "the static marks were not created"
    );
    feed(&rpc, "100G");
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for k in 0..100 {
        feed(&rpc, if k % 20 < 10 { "j" } else { "k" });
    }
    let _ = exec_lua(&rpc, "return 1").await;
    let elapsed = started.elapsed();
    let n = exec_lua(&rpc, "return #btv.buf.extmarks(0, d, 0, -1, {})").await;
    assert_eq!(n.as_u64(), Some(30), "the handler did not republish");
    elapsed
}

#[tokio::test]
async fn republishing_one_namespace_does_not_pay_for_the_others() {
    // A RATIO, so a loaded `cargo test --workspace` doesn't flake it. Measured:
    // re-serializing every namespace per push ~75x (34s for the 100 moves); per
    // namespace, with the frame still re-sorting one store-wide position index after
    // each structural change, ~3.1x; with an index per namespace, ~1.0x. 2.0 leaves
    // room for a loaded machine and still fails either regression.
    let bare = republish_cost(0).await;
    let heavy = republish_cost(20_000).await;
    let ratio = heavy.as_secs_f64() / bare.as_secs_f64();
    assert!(
        ratio < 2.0,
        "republishing 30 marks beside 20000 static ones cost {ratio:.1}x the same run \
         with none ({heavy:?} vs {bare:?}): the mirror is re-serializing every \
         namespace on a structural change, or the frame is re-sorting every mark"
    );
}

/// A namespace gone from the store while the buffer keeps other marks: `:e!` clears
/// every namespace, and a mark set later in the same chunk lands before the next push.
/// The cleared namespace must be dropped from the mirror, not left holding its rows.
#[tokio::test]
async fn a_reload_and_a_new_mark_in_one_chunk_drop_the_old_namespaces() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1 })
         btv.buf.set_extmark(0, b, 2, 0, { id = 1 })",
    )
    .await;
    feed(&rpc, "ggx");
    exec_lua(
        &rpc,
        "btv.cmd('e!')
         btv.buf.set_extmark(0, b, 3, 0, { id = 7 })",
    )
    .await;
    assert_eq!(all_marks(&rpc).await, "b/7@3,0-nil,nil[]");
    assert_mirror_is_the_core(&rpc).await;
}

/// A push the Lua side failed to apply must not count as delivered. The server records
/// which namespaces Lua holds, so a lost patch would leave its rows missing until that
/// namespace happened to change again; the next push has to repair it.
#[tokio::test]
async fn a_push_that_fails_in_lua_is_repaired_by_the_next() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1 })",
    )
    .await;
    // The next push raises once, then the real receiver is back.
    exec_lua(
        &rpc,
        "local real = btv._set_extmark_mirror
         btv._set_extmark_mirror = function()
           btv._set_extmark_mirror = real
           error('injected mirror failure')
         end
         btv.buf.set_extmark(0, b, 2, 0, { id = 1 })
         -- Drop the write-through row, so only a push can bring it back.
         btv._extmarks[btv._resolve_bufnr(0)][b] = nil",
    )
    .await;
    // This chunk's push is the one that raises.
    exec_lua(&rpc, "return 1").await;
    assert_eq!(
        all_marks(&rpc).await,
        "a/1@1,0-nil,nil[] b/1@2,0-nil,nil[]",
        "the push after the failed one re-sends what Lua missed"
    );
}

/// The same, when the lost push was dropping a namespace: the repair must remove it,
/// not just re-send the namespaces the store still holds.
#[tokio::test]
async fn a_failed_push_that_dropped_a_namespace_is_repaired() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         btv.buf.set_extmark(0, a, 1, 0, { id = 1 })
         btv.buf.set_extmark(0, b, 2, 0, { id = 1 })",
    )
    .await;
    feed(&rpc, "ggx");
    exec_lua(
        &rpc,
        "local real = btv._set_extmark_mirror
         btv._set_extmark_mirror = function()
           btv._set_extmark_mirror = real
           error('injected mirror failure')
         end
         btv.cmd('e!')
         btv.buf.set_extmark(0, b, 3, 0, { id = 7 })",
    )
    .await;
    exec_lua(&rpc, "return 1").await;
    assert_eq!(all_marks(&rpc).await, "b/7@3,0-nil,nil[]");
}

/// `clear_namespace` and `del_extmark` that raise must leave the mirror's rows alone:
/// the core cleared nothing, and with the push per namespace nothing else in the
/// buffer would re-send them. Both used to drop the rows before the native bridge
/// rejected the arguments.
#[tokio::test]
async fn a_clear_or_delete_that_raises_leaves_the_rows() {
    let (rpc, _inc) = open(&sample(10)).await;
    exec_lua(
        &rpc,
        "_G.a, _G.b = btv.ns.create('a'), btv.ns.create('b')
         for i = 1, 4 do btv.buf.set_extmark(0, a, i, 0, { id = i }) end",
    )
    .await;
    let raised = exec_lua(
        &rpc,
        "local c = pcall(btv.buf.clear_namespace, 0, a, 2.5, -1)
         local cc = pcall(btv.buf.clear_namespace, 0, -1, 2.5, -1)
         local d = pcall(btv.buf.del_extmark, 0, a, 1.5)
         return not c and not cc and not d",
    )
    .await;
    assert_eq!(
        raised.as_bool(),
        Some(true),
        "fractional rows and ids raise"
    );
    // A structural change elsewhere, which used to rebuild the whole buffer.
    exec_lua(&rpc, "btv.buf.set_extmark(0, b, 0, 0, { id = 1 })").await;
    assert_eq!(
        all_marks(&rpc).await,
        "a/1@1,0-nil,nil[] a/2@2,0-nil,nil[] a/3@3,0-nil,nil[] a/4@4,0-nil,nil[] \
         b/1@0,0-nil,nil[]"
    );
}

/// The editor's own namespaces sit at the top of the `u32` range, where the Lua
/// allocator never reaches. Naming one from Lua would reach into editor state (the
/// multi-cursor heads, the paint layer) and leave a mirror row no push ever corrects,
/// so the bridges refuse it, loud.
#[tokio::test]
async fn the_editors_own_namespaces_are_refused() {
    let (rpc, _inc) = open(&sample(10)).await;
    let errs = exec_lua(
        &rpc,
        "local reserved = 4294967295 - 8
         local out = {}
         for _, f in ipairs({
           function() btv.buf.set_extmark(0, reserved, 0, 0, {}) end,
           function() btv.buf.del_extmark(0, 4294967295, 1) end,
           function() btv.buf.clear_namespace(0, 4294967294, 0, -1) end,
         }) do
           local ok, e = pcall(f)
           out[#out + 1] = ok and 'ok' or (tostring(e):match('reserved') and 'reserved' or tostring(e))
         end
         return table.concat(out, ' ')",
    )
    .await;
    assert_eq!(errs.as_str(), Some("reserved reserved reserved"));
    assert_eq!(all_marks(&rpc).await, "");
}

/// `clear_namespace(buf, -1, …)` clears every namespace a plugin can touch. The
/// mirror also lists the editor's own highlight namespaces on the buffers it paints
/// (the quickfix list's error highlights), which the bridge refuses, so the loop must
/// pass over them rather than raise halfway through.
#[tokio::test]
async fn clearing_every_namespace_skips_the_editors_own() {
    let (rpc, _inc) = open(&sample(5)).await;
    exec_lua(
        &rpc,
        r#"vim.fn.setqflist({}, " ", { lines = { "a.c:1:E boom" }, efm = "%f:%l:%t %m" })"#,
    )
    .await;
    feed(&rpc, ":copen<CR>");
    let v = exec_lua(
        &rpc,
        "local b = btv._resolve_bufnr(0)
         local ns = btv.ns.create('mine')
         btv.buf.set_extmark(b, ns, 0, 0, {})
         local internal = 0
         for nsid in pairs(btv._extmarks[b] or {}) do
           if nsid >= 4294967295 - 8 then internal = internal + 1 end
         end
         local ok, e = pcall(btv.buf.clear_namespace, b, -1, 0, -1)
         return { internal, ok and 'ok' or tostring(e), #btv.buf.extmarks(b, ns, 0, -1, {}) }",
    )
    .await;
    let v = v.as_array().expect("a result table");
    assert!(
        v[0].as_u64().unwrap_or(0) > 0,
        "precondition: the quickfix buffer mirrors an editor namespace: {v:?}"
    );
    assert_eq!(v[1].as_str(), Some("ok"), "{v:?}");
    assert_eq!(
        v[2].as_u64(),
        Some(0),
        "the plugin's own mark is cleared: {v:?}"
    );
}
