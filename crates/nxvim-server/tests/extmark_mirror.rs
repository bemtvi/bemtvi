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

use nxvim_rpc::Rpc;
use nxvim_test_harness::{exec_lua, feed, start_with_file as open};

fn sample(n: usize) -> String {
    (0..n).map(|i| format!("line {i}\n")).collect()
}

/// Create a namespace and return the Lua expression naming it.
async fn ns(rpc: &Rpc) {
    exec_lua(rpc, "_G.ns = nx.ns.create('test')").await;
}

/// Set one mark, returning its id.
async fn mark(rpc: &Rpc, row: usize, col: usize, extra: &str) -> u64 {
    let v = exec_lua(
        rpc,
        &format!("return nx.buf.set_extmark(0, _G.ns, {row}, {col}, {{ {extra} }})"),
    )
    .await;
    v.as_u64().expect("set_extmark returned no id")
}

/// The mirror's `(row, col)` for mark `id`.
async fn pos(rpc: &Rpc, id: u64) -> (i64, i64) {
    let v = exec_lua(
        rpc,
        &format!(
            "for _, e in ipairs(nx.buf.extmarks(0, _G.ns, 0, -1, {{}})) do
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
            "for _, e in ipairs(nx.buf.extmarks(0, _G.ns, 0, -1, {{ details = true }})) do
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
         for _, e in ipairs(nx.buf.extmarks(0, _G.ns, 0, -1, {})) do
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
        "local throwaway = nx.buf.set_extmark(0, _G.ns, 0, 0, {})
         nx.buf.del_extmark(0, _G.ns, throwaway)",
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
    exec_lua(&rpc, &format!("nx.buf.del_extmark(0, _G.ns, {m})")).await;
    feed(&rpc, "6GIxyz<Esc>");
    let v = exec_lua(&rpc, "return #nx.buf.extmarks(0, _G.ns, 0, -1, {})").await;
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
         for i = 0, 299 do _G.ids[i] = nx.buf.set_extmark(0, _G.ns, i, 0, {}) end",
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
           for _, e in ipairs(nx.buf.extmarks(0, _G.ns, 0, -1, {})) do
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
    exec_lua(&rpc, "nx.on('CursorMoved', function() end)").await;
    exec_lua(
        &rpc,
        "for i = 0, 4999 do
           nx.buf.set_extmark(0, _G.ns, i, 0, { hl_group = 'Comment', end_row = i, end_col = 4 })
         end",
    )
    .await;
    let n = exec_lua(&rpc, "return #nx.buf.extmarks(0, _G.ns, 0, -1, {})").await;
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
        "nx.buf.set_text(0, 290, 0, 290, 4, { 'Q' })
         local blanks = {}
         for i = 1, 270 do blanks[i] = '' end
         nx.buf.set_lines(0, 10, 280, false, blanks)",
    )
    .await;
    // Reaching here at all means the server survived; the mirror must also be right.
    assert_eq!(
        exec_lua(&rpc, "return nx.buf.line_count(0)").await.as_u64(),
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
    exec_lua(&rpc, "nx.cmd('10copy 10') nx.cmd('200delete')").await;
    assert_eq!(pos(&rpc, between).await, (101, 0));
    assert_matches_full_rebuild(&rpc).await;
}
