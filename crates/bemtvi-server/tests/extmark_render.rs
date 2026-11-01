//! Black-box tests for the two per-frame extmark fast paths.
//!
//! **The virtual-line gate.** `Buffer::virt_lines_by_line` is called from the view,
//! cursor and mouse walks — several times per frame — and used to filter every mark
//! on each call. It now answers "none" in O(1) from a count the store maintains. A
//! count that drifts *low* silently stops virtual lines from rendering, so every path
//! that can remove a `virt_lines` mark is exercised here through what actually
//! renders, not through the private counter.
//!
//! **The highlight index.** `extmark_intervals` used to re-scan every mark for each
//! visible row; it now queries a per-frame sorted index. Overlapping and nested
//! ranges are the cases a naive index gets wrong, so they are pinned against the
//! spans that actually reach the client.
//!
//! See `docs/plans/2026-08-07-incremental-buffer-mirror.md`.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{exec_lua, start_with_file, wait_redraw, window0_field};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// How many rendered rows carry no buffer-line number *above* the last numbered row
/// — i.e. how many virtual rows the frame drew (trailing numberless rows are `~`
/// filler). Virtual lines are the only thing that inserts a numberless row inside the
/// text region, so this rises exactly when they render.
fn virtual_rows_in(map: &[(Value, Value)]) -> usize {
    let numbers = window0_field(map, "numbers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match numbers.iter().rposition(|v| v.as_u64().is_some()) {
        Some(last) => numbers[..last].iter().filter(|v| v.is_nil()).count(),
        None => 0,
    }
}

/// Settle the pending work, then read the virtual-row count off the next frame.
async fn virtual_rows(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> usize {
    let _ = exec_lua(rpc, "return 1").await;
    let map = wait_redraw(inc, |m| window0_field(m, "numbers").is_some()).await;
    virtual_rows_in(&map)
}

/// Add a `virt_lines` mark on `row` in namespace `ns_name`, returning its id.
async fn virt_mark(rpc: &Rpc, ns_name: &str, row: usize) -> u64 {
    exec_lua(
        rpc,
        &format!(
            "_G.{ns_name} = btv.ns.create('{ns_name}')
             return btv.buf.set_extmark(0, _G.{ns_name}, {row}, 0, {{
               virt_lines = {{ {{ {{ '-- v --', 'Comment' }} }} }},
             }})"
        ),
    )
    .await
    .as_u64()
    .expect("an extmark id")
}

fn sample() -> String {
    (0..8).map(|i| format!("line {i}\n")).collect()
}

#[tokio::test]
async fn deleting_one_virtual_line_mark_leaves_the_other_rendering() {
    let (rpc, mut inc) = start_with_file(&sample()).await;
    let a = virt_mark(&rpc, "va", 1).await;
    virt_mark(&rpc, "vb", 3).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 2);
    exec_lua(&rpc, &format!("btv.buf.del_extmark(0, _G.va, {a})")).await;
    assert_eq!(
        virtual_rows(&rpc, &mut inc).await,
        1,
        "deleting one virt_lines mark must not switch the other off"
    );
}

#[tokio::test]
async fn replacing_a_virtual_line_mark_in_place_keeps_it_rendering() {
    // `set_extmark` with an explicit id overwrites, so the count must shed the mark
    // it replaced and take on the new one — not simply increment.
    let (rpc, mut inc) = start_with_file(&sample()).await;
    let id = virt_mark(&rpc, "va", 1).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
    exec_lua(
        &rpc,
        &format!(
            "btv.buf.set_extmark(0, _G.va, 2, 0, {{
               id = {id}, virt_lines = {{ {{ {{ '-- w --', 'Comment' }} }} }},
             }})"
        ),
    )
    .await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
}

#[tokio::test]
async fn replacing_a_virtual_line_mark_with_a_plain_one_stops_it_rendering() {
    let (rpc, mut inc) = start_with_file(&sample()).await;
    let id = virt_mark(&rpc, "va", 1).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
    exec_lua(
        &rpc,
        &format!("btv.buf.set_extmark(0, _G.va, 1, 0, {{ id = {id}, hl_group = 'Comment', end_row = 1, end_col = 3 }})"),
    )
    .await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 0);
}

#[tokio::test]
async fn clearing_one_namespace_leaves_another_namespaces_virtual_line() {
    let (rpc, mut inc) = start_with_file(&sample()).await;
    virt_mark(&rpc, "va", 1).await;
    virt_mark(&rpc, "vb", 3).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 2);
    exec_lua(&rpc, "btv.buf.clear_namespace(0, _G.va, 0, -1)").await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
}

#[tokio::test]
async fn clearing_a_range_that_spares_the_mark_keeps_it_rendering() {
    let (rpc, mut inc) = start_with_file(&sample()).await;
    virt_mark(&rpc, "va", 5).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
    // Clear a line range the mark does not sit in.
    exec_lua(&rpc, "btv.buf.clear_namespace(0, _G.va, 0, 3)").await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
}

#[tokio::test]
async fn an_undo_keeps_virtual_lines_consistent() {
    // Undo swaps the whole extmark store (and moves the ephemeral namespaces across),
    // which is the one path that transfers marks between two stores.
    let (rpc, mut inc) = start_with_file(&sample()).await;
    virt_mark(&rpc, "va", 1).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
    exec_lua(&rpc, "btv.cmd('normal! x')").await;
    exec_lua(&rpc, "btv.cmd('undo')").await;
    // A destructive rope replacement drops extmarks (matching neovim), so what
    // matters is that the count agrees with the store rather than stranding a
    // phantom: no virtual row, and no crash.
    let rows = virtual_rows(&rpc, &mut inc).await;
    assert!(rows <= 1, "unexpected virtual row count after undo: {rows}");
    // The store must still accept and render a fresh mark afterwards.
    virt_mark(&rpc, "vc", 2).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, rows + 1);
}

#[tokio::test]
async fn nested_and_overlapping_highlight_marks_render_identically() {
    // The per-frame index finds a line's marks by a sorted range query. A mark that
    // starts well before a line but reaches into it (nesting) is what a naive
    // "contiguous window" query drops, so pin it through the rendered spans.
    let (rpc, mut inc) = start_with_file("aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\n").await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('hl')
         -- A long range spanning all three lines, plus a short one nested inside it
         -- on the last line, plus one overlapping the two.
         btv.buf.set_extmark(0, ns, 0, 0, { hl_group = 'ErrorMsg', end_row = 2, end_col = 10 })
         btv.buf.set_extmark(0, ns, 2, 2, { hl_group = 'WarningMsg', end_row = 2, end_col = 5 })
         btv.buf.set_extmark(0, ns, 1, 4, { hl_group = 'Search', end_row = 2, end_col = 3 })",
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let map = wait_redraw(&mut inc, |m| window0_field(m, "highlights").is_some()).await;
    let hl = window0_field(&map, "highlights")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // Every one of the three lines is covered by the spanning mark, so each row must
    // carry at least one span; the nested marks add more on rows 1 and 2.
    let per_row: Vec<usize> = hl
        .iter()
        .take(3)
        .map(|r| r.as_array().map_or(0, |a| a.len()))
        .collect();
    assert!(
        per_row.iter().all(|&n| n > 0),
        "every covered row must keep its spans, got {per_row:?}"
    );
    assert!(
        per_row[1] > 1 && per_row[2] > 1,
        "rows reached by a nested/overlapping mark must carry extra spans, got {per_row:?}"
    );
}

#[tokio::test]
async fn deleting_a_plain_mark_does_not_switch_off_virtual_lines() {
    // The count must drop only for marks that actually carried `virt_lines`. A
    // blanket decrement on every deletion under-counts, and the gate then reports
    // "no virtual lines" while one is still live — the silent-staleness failure.
    let (rpc, mut inc) = start_with_file(&sample()).await;
    virt_mark(&rpc, "va", 1).await;
    assert_eq!(virtual_rows(&rpc, &mut inc).await, 1);
    let plain = exec_lua(
        &rpc,
        "_G.p = btv.ns.create('plain')
         return btv.buf.set_extmark(0, _G.p, 4, 0, { hl_group = 'Comment', end_row = 4, end_col = 3 })",
    )
    .await
    .as_u64()
    .expect("an id");
    exec_lua(&rpc, &format!("btv.buf.del_extmark(0, _G.p, {plain})")).await;
    assert_eq!(
        virtual_rows(&rpc, &mut inc).await,
        1,
        "deleting a mark with no virt_lines must not switch the virtual line off"
    );
}

#[tokio::test]
async fn a_long_mark_is_found_past_a_shorter_one() {
    // The index walks back from the last mark starting before the line, pruning on a
    // running max of every earlier `end`. Using each mark's OWN end instead stops at
    // the first short mark and loses the long one behind it — so put a short mark
    // between a long one and the queried line, which is exactly that case.
    let (rpc, mut inc) = start_with_file(&sample()).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('hl')
         -- A: starts on line 0, reaches line 5.
         btv.buf.set_extmark(0, ns, 0, 0, { hl_group = 'ErrorMsg', end_row = 5, end_col = 5 })
         -- B: starts after A but ends back on line 1, well before line 5.
         btv.buf.set_extmark(0, ns, 1, 0, { hl_group = 'WarningMsg', end_row = 1, end_col = 3 })",
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let map = wait_redraw(&mut inc, |m| window0_field(m, "highlights").is_some()).await;
    let hl = window0_field(&map, "highlights")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let row5 = hl.get(5).and_then(Value::as_array).map_or(0, |a| a.len());
    assert!(
        row5 > 0,
        "line 5 must keep the span of the long mark reaching it past the short one"
    );
}

/// Time 300 keystrokes in a 5000-line buffer carrying `marks` extmarks, each built
/// with `opts` (the payload shape under test — different payloads drive completely
/// different per-frame projections).
async fn typing_cost(marks: usize, opts: &str) -> std::time::Duration {
    let text: String = (0..5_000)
        .map(|i| format!("line {i} with some text\n"))
        .collect();
    let (rpc, _inc) = start_with_file(&text).await;
    exec_lua(&rpc, "btv.on('CursorMoved', function() end)").await;
    if marks > 0 {
        exec_lua(
            &rpc,
            &format!(
                "_G.b = btv.ns.create('bench')
                 for i = 0, {} do btv.buf.set_extmark(0, _G.b, i, 0, {{ {} }}) end",
                marks - 1,
                opts
            ),
        )
        .await;
        assert_eq!(
            exec_lua(&rpc, "return #btv.buf.extmarks(0, _G.b, 0, -1, {})")
                .await
                .as_u64(),
            Some(marks as u64),
            "the marks under test were not created"
        );
    }
    bemtvi_test_harness::feed(&rpc, "2500GI");
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for _ in 0..300 {
        bemtvi_test_harness::feed(&rpc, "z");
    }
    let _ = exec_lua(&rpc, "return 1").await;
    started.elapsed()
}

/// A plain highlight range — drives `extmark_intervals` / `HlMarkIndex`.
const HL_MARK: &str = "hl_group = 'Comment', end_row = i, end_col = 4";
/// A gutter sign — drives `extmark_sign_cells`, the git-signs / diagnostics shape.
const SIGN_MARK: &str = "sign_text = '>>', sign_hl_group = 'Comment'";

#[tokio::test]
async fn marks_do_not_dominate_the_cost_of_typing() {
    // A RATIO rather than a wall-clock bound: both halves run on the same machine
    // under the same load, so this stays meaningful in a loaded `cargo test
    // --workspace` where an absolute threshold either flakes or has to be set so
    // loose it catches nothing.
    //
    // Measured ratios (mark-heavy / mark-free) as the work landed:
    //   whole-mark re-serialize per push  ~58x
    //   after the mirror gate + window     ~6.3x
    //   after the per-frame index + gate   ~2.2x   <- now
    // The ratio measured 2.2-2.3x across repeated runs, and reverting either Phase 4
    // fix pushes it to 4.0x+, so 3.2 leaves ~40% headroom while still failing on a
    // regression rather than sitting exactly on the boundary.
    let bare = typing_cost(0, "").await;
    let heavy = typing_cost(5_000, HL_MARK).await;
    let ratio = heavy.as_secs_f64() / bare.as_secs_f64();
    assert!(
        ratio < 3.2,
        "typing with 5000 extmarks cost {ratio:.1}x a mark-free buffer \
         ({heavy:?} vs {bare:?}) — extmarks are dominating a keystroke again"
    );
}

// ===== viewport pruning ======================================================
//
// `virt_text_for`, `extmark_sign_cells` and `line_bg_for` each bucket marks by their
// ANCHOR line, and now skip marks anchored outside the visible byte range so the
// per-mark rope lookup is paid per visible mark rather than per mark in the buffer.
// An off-by-one in that range silently hides a decoration on the first or last
// visible row, so both edges are pinned here — along with the fact that scrolling a
// previously-pruned mark into view still paints it.

/// The window's `signs` payload for each visible row, as `Some(glyph)` per row.
async fn signs_rows(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<Option<String>> {
    let _ = exec_lua(rpc, "return 1").await;
    let map = wait_redraw(inc, |m| window0_field(m, "diagnostics_signs").is_some()).await;
    window0_field(&map, "diagnostics_signs")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    r.as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn put_sign(rpc: &Rpc, row: usize) {
    exec_lua(
        rpc,
        &format!(
            "_G.s = _G.s or btv.ns.create('signs')
             btv.buf.set_extmark(0, _G.s, {row}, 0, {{ sign_text = '>>' }})"
        ),
    )
    .await;
}

#[tokio::test]
async fn a_sign_on_the_first_visible_row_renders() {
    let (rpc, mut inc) = start_with_file(&sample()).await;
    put_sign(&rpc, 0).await;
    let rows = signs_rows(&rpc, &mut inc).await;
    assert_eq!(rows.first().cloned().flatten().as_deref(), Some(">>"));
}

#[tokio::test]
async fn a_sign_on_the_last_visible_row_renders() {
    // The exclusive edge of the viewport byte range. A range that stops one line
    // early drops this sign and nothing else would notice.
    let text: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    let rows = signs_rows(&rpc, &mut inc).await;
    let last_text_row = rows.len() - 1;
    // Put the sign on whatever buffer line occupies the last visible row.
    put_sign(&rpc, last_text_row).await;
    let rows = signs_rows(&rpc, &mut inc).await;
    assert_eq!(
        rows[last_text_row].as_deref(),
        Some(">>"),
        "a sign on the last visible row must survive the viewport prune"
    );
}

#[tokio::test]
async fn a_sign_below_the_viewport_appears_after_scrolling_to_it() {
    let text: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    put_sign(&rpc, 300).await;
    let before = signs_rows(&rpc, &mut inc).await;
    assert!(
        before.iter().all(Option::is_none),
        "a sign 300 lines down must not paint in the first viewport"
    );
    exec_lua(&rpc, "btv.cmd('301')").await;
    let after = signs_rows(&rpc, &mut inc).await;
    assert!(
        after.iter().any(|r| r.as_deref() == Some(">>")),
        "scrolling the sign into view must paint it"
    );
}

#[tokio::test]
async fn a_line_highlight_on_the_last_visible_row_renders() {
    let text: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    let _ = exec_lua(&rpc, "return 1").await;
    let probe = wait_redraw(&mut inc, |m| window0_field(m, "numbers").is_some()).await;
    let n_rows = window0_field(&probe, "numbers")
        .and_then(Value::as_array)
        .map_or(0, |a| a.iter().filter(|v| v.as_u64().is_some()).count());
    let last = n_rows - 1;
    exec_lua(
        &rpc,
        &format!(
            // The group must resolve to a real style or the row is dropped before it
            // reaches the client, so define one rather than lean on the theme.
            "vim.api.nvim_set_hl(0, 'MyBg', {{ bg = 0x332211 }})
             local ns = btv.ns.create('lbg')
             btv.buf.set_extmark(0, ns, {last}, 0, {{ line_hl_group = 'MyBg' }})"
        ),
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let map = wait_redraw(&mut inc, |m| window0_field(m, "line_bg").is_some()).await;
    let painted = window0_field(&map, "line_bg")
        .and_then(Value::as_array)
        .map_or(0, |a| a.iter().filter(|v| !v.is_nil()).count());
    assert!(
        painted > 0,
        "a line_hl_group on the last visible row must survive the viewport prune"
    );
}

#[tokio::test]
async fn virtual_text_on_the_last_visible_row_renders() {
    let text: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    let _ = exec_lua(&rpc, "return 1").await;
    let probe = wait_redraw(&mut inc, |m| window0_field(m, "numbers").is_some()).await;
    let last = window0_field(&probe, "numbers")
        .and_then(Value::as_array)
        .map_or(0, |a| a.iter().filter(|v| v.as_u64().is_some()).count())
        - 1;
    exec_lua(
        &rpc,
        &format!(
            "local ns = btv.ns.create('vt')
             btv.buf.set_extmark(0, ns, {last}, 0, {{ virt_text = {{ {{ ' hint', 'Comment' }} }} }})"
        ),
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let map = wait_redraw(&mut inc, |m| window0_field(m, "virt_text").is_some()).await;
    let painted = window0_field(&map, "virt_text")
        .and_then(Value::as_array)
        .map_or(0, |a| a.iter().filter(|v| !v.is_nil()).count());
    assert!(
        painted > 0,
        "virt_text on the last visible row must survive the viewport prune"
    );
}

#[tokio::test]
async fn gutter_signs_do_not_dominate_the_cost_of_typing() {
    // A DIFFERENT payload from the test above, and that distinction is the point: a
    // `hl_group` mark never enters `extmark_sign_cells` / `virt_text_for` /
    // `line_bg_for`, so benchmarking only highlight marks measured the best case and
    // hid these paths entirely. Each of them bucketed EVERY mark in the buffer by
    // `byte_to_line` on every frame, which put a buffer full of git signs at ~13.8x a
    // mark-free one (6.5s vs 0.47s) while highlight marks sat at ~2.2x. Pruning to
    // the visible byte range brought it to ~2.0x.
    let bare = typing_cost(0, "").await;
    let heavy = typing_cost(5_000, SIGN_MARK).await;
    let ratio = heavy.as_secs_f64() / bare.as_secs_f64();
    assert!(
        ratio < 3.2,
        "typing with 5000 gutter signs cost {ratio:.1}x a mark-free buffer \
         ({heavy:?} vs {bare:?}) — the per-frame sign scan is unpruned again"
    );
}

// ===== the position index ====================================================
//
// Every per-frame layer finds its marks through the store's position index
// (`ExtmarkStore::starting_in` / `reaching`) rather than by walking every mark, so a
// frame's cost follows the marks on screen. See
// `docs/plans/2026-10-31-extmark-position-index.md`.

/// How long 300 cursor moves take near the top of a 20 000-line buffer whose last
/// `marks` lines each carry a mark made by `place` (a Lua statement using the line
/// index `i` and namespace `ns`): every mark is far below the viewport.
async fn moving_cost(marks: usize, place: &str) -> std::time::Duration {
    let lines = 20_000;
    let text: String = (0..lines)
        .map(|i| format!("line {i} with some text\n"))
        .collect();
    let (rpc, _inc) = start_with_file(&text).await;
    if marks > 0 {
        exec_lua(
            &rpc,
            &format!(
                "vim.api.nvim_set_hl(0, 'IxBg', {{ bg = 0x332211 }})
                 local ns = btv.ns.create('ix')
                 local diags = {{}}
                 for i = {}, {} do {place} end
                 if #diags > 0 then btv.diagnostic.set(ns, 0, diags) end",
                lines - marks,
                lines - 1
            ),
        )
        .await;
    }
    let _ = exec_lua(&rpc, "return 1").await;
    let started = std::time::Instant::now();
    for k in 0..300 {
        bemtvi_test_harness::feed(&rpc, if k % 20 < 10 { "j" } else { "k" });
    }
    let _ = exec_lua(&rpc, "return 1").await;
    started.elapsed()
}

#[tokio::test]
async fn marks_off_screen_cost_a_frame_nothing_in_any_layer() {
    // Ratios against a mark-free buffer, so a loaded `cargo test --workspace` doesn't
    // flake it. Every layer runs every frame whatever the marks carry, so one layer
    // walking every mark again raises every kind's ratio together: the mean is the
    // sharp signal. Measured: before the index ~3.5x per kind; with it, ~1.15x mean
    // (worst kind 1.3x); with one layer (signs) put back on the full walk, ~1.6x mean.
    let bare = moving_cost(0, "").await;
    let mut ratios = Vec::new();
    for (layer, place) in [
        (
            "highlight",
            "btv.buf.set_extmark(0, ns, i, 0, { hl_group = 'Comment', end_row = i, end_col = 2 })",
        ),
        (
            "virt_text",
            "btv.buf.set_extmark(0, ns, i, 0, { virt_text = { { ' x', 'Comment' } } })",
        ),
        (
            "line_hl_group",
            "btv.buf.set_extmark(0, ns, i, 0, { line_hl_group = 'IxBg' })",
        ),
        (
            "sign_text",
            "btv.buf.set_extmark(0, ns, i, 0, { sign_text = '>>' })",
        ),
        (
            "gutter_text",
            "btv.buf.set_extmark(0, ns, i, 0, { gutter_text = { { tostring(i) } } })",
        ),
        (
            "line_fill",
            "btv.buf.set_extmark(0, ns, i, 0, { line_fill = { text = '-' } })",
        ),
        (
            "diagnostic",
            "diags[#diags + 1] = { lnum = i, col = 0, end_col = 2, severity = 1, message = 'x' }",
        ),
    ] {
        let heavy = moving_cost(20_000, place).await;
        ratios.push((layer, heavy.as_secs_f64() / bare.as_secs_f64()));
    }
    let report: Vec<String> = ratios.iter().map(|(l, r)| format!("{l} {r:.2}x")).collect();
    let mean = ratios.iter().map(|(_, r)| r).sum::<f64>() / ratios.len() as f64;
    assert!(
        mean < 1.35 && ratios.iter().all(|(_, r)| *r < 1.8),
        "20 000 off-screen marks still cost a frame (mean {mean:.2}x; {}; bare {bare:?})",
        report.join(", ")
    );
}

#[tokio::test]
async fn a_range_from_far_above_the_viewport_still_paints() {
    // `reaching` must find a range that starts long before the viewport and ends in
    // it, past thousands of short marks that end before it.
    let text: String = (0..3_000).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('far')
         for i = 0, 2500 do btv.buf.set_extmark(0, ns, i, 0, { hl_group = 'Comment', end_row = i, end_col = 2 }) end
         btv.buf.set_extmark(0, ns, 1, 0, { hl_group = 'Comment', end_row = 2950, end_col = 0 })
         btv.cmd('2800')",
    )
    .await;
    let _ = exec_lua(&rpc, "return 1").await;
    let map = wait_redraw(&mut inc, |m| window0_field(m, "highlights").is_some()).await;
    let rows = window0_field(&map, "highlights")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let painted = rows
        .iter()
        .filter(|r| r.as_array().is_some_and(|a| !a.is_empty()))
        .count();
    assert!(
        painted >= rows.len().saturating_sub(2) && painted > 0,
        "the long range paints every row it covers: {painted} of {} rows",
        rows.len()
    );
}

#[tokio::test]
async fn marks_collapsed_by_a_deletion_still_render_on_their_rows() {
    // Marks of both gravities inside a deleted region collapse onto its start; the
    // index re-sorts that run. Signs on the lines around it must stay on their rows,
    // and editing on afterwards must keep finding them (a stale order would hide one).
    let (rpc, mut inc) = start_with_file(&sample()).await;
    exec_lua(
        &rpc,
        "local ns = btv.ns.create('grav')
         -- line 1 is `line 1 …`: a left-gravity mark at col 3 and a right-gravity one at col 1
         btv.buf.set_extmark(0, ns, 1, 3, { sign_text = 'L', right_gravity = false })
         btv.buf.set_extmark(0, ns, 1, 1, { sign_text = 'R', priority = 1 })
         btv.buf.set_extmark(0, ns, 0, 0, { sign_text = 'A' })
         btv.buf.set_extmark(0, ns, 2, 0, { sign_text = 'B' })",
    )
    .await;
    // Delete columns 0..5 of line 2 (1-based), collapsing both marks onto column 0.
    bemtvi_test_harness::feed(&rpc, "2G05x");
    let rows = signs_rows(&rpc, &mut inc).await;
    assert_eq!(rows[0].as_deref(), Some("A"));
    assert_eq!(
        rows[1].as_deref(),
        Some("L"),
        "the higher-priority collapsed mark wins: {rows:?}"
    );
    assert_eq!(rows[2].as_deref(), Some("B"));
    // Join line 2 onto line 1, then open a line above: every mark moves again.
    bemtvi_test_harness::feed(&rpc, "kJggOnew<Esc>");
    let rows = signs_rows(&rpc, &mut inc).await;
    assert_eq!(rows[0], None, "the opened line has no sign: {rows:?}");
    assert!(
        rows[1].is_some(),
        "line 1 (now joined) keeps a sign: {rows:?}"
    );
    assert_eq!(rows[2].as_deref(), Some("B"));
}

#[tokio::test]
async fn a_replacement_that_reorders_marks_keeps_them_findable() {
    // One edit replacing text that holds a right-gravity mark and a left-gravity one
    // sends the first past the replacement's end and leaves the second at its start,
    // so the earlier entry in the index now sits after the later one. Here the
    // replacement runs past the viewport: unless the index re-sorts that run, the
    // left-gravity sign on the last visible row is lost to the binary search.
    let text: String = (0..60).map(|i| format!("line {i}\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    let rows = signs_rows(&rpc, &mut inc).await;
    let last = rows.len() - 1;
    exec_lua(
        &rpc,
        &format!(
            "local ns = btv.ns.create('reorder')
             for i = 0, {last} - 1 do btv.buf.set_extmark(0, ns, i, 0, {{ sign_text = 'a' }}) end
             btv.buf.set_extmark(0, ns, {last}, 0, {{ sign_text = 'R' }})
             btv.buf.set_extmark(0, ns, {last}, 2, {{ sign_text = 'L', right_gravity = false }})
             for i = {last} + 1, 59 do btv.buf.set_extmark(0, ns, i, 0, {{ sign_text = 'z' }}) end"
        ),
    )
    .await;
    // Draw a frame first, so the index is built before the edit and has to be kept
    // sorted across it rather than rebuilt after.
    let _ = signs_rows(&rpc, &mut inc).await;
    // A separate chunk: the marks must exist before the edit moves them.
    exec_lua(
        &rpc,
        &format!("btv.buf.set_text(0, {last}, 0, {last}, 4, {{ 'one', 'two', 'three', 'four', 'five', 'six' }})"),
    )
    .await;
    let rows = signs_rows(&rpc, &mut inc).await;
    assert_eq!(
        rows[last].as_deref(),
        Some("L"),
        "the left-gravity sign stays on the last visible row: {rows:?}"
    );
    // The right-gravity one went to the replacement's end, five lines down, and the
    // marks after the replacement moved down with their lines.
    exec_lua(&rpc, &format!("btv.cmd('{}')", last + 10)).await;
    let rows = signs_rows(&rpc, &mut inc).await;
    let at = rows
        .iter()
        .position(|r| r.as_deref() == Some("L"))
        .expect("L still shown");
    assert_eq!(rows[at + 1..at + 5], [None, None, None, None], "{rows:?}");
    assert_eq!(rows[at + 5].as_deref(), Some("R"), "{rows:?}");
    assert_eq!(rows[at + 6].as_deref(), Some("z"), "{rows:?}");
}

/// One snapshot of namespace `fz`'s marks as the Lua mirror holds them, serialized
/// `id:row,col-end_row,end_col` in id order.
async fn fuzz_snapshot(rpc: &Rpc) -> String {
    exec_lua(
        rpc,
        "local out = {}
         for _, e in ipairs(btv.buf.extmarks(0, _G.fz.ns, 0, -1, { details = true })) do
           local d = e[4]
           out[#out + 1] = { e[1], string.format('%d:%d,%d-%s,%s', e[1], e[2], e[3],
             tostring(d.end_row), tostring(d.end_col)) }
         end
         table.sort(out, function(a, b) return a[1] < b[1] end)
         local s = {}
         for _, x in ipairs(out) do s[#s + 1] = x[2] end
         return table.concat(s, ' ')",
    )
    .await
    .as_str()
    .unwrap_or_default()
    .to_owned()
}

/// The next frame's `(buffer line, has a sign)` for every row showing a buffer line.
async fn fuzz_sign_rows(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<(u64, bool)> {
    let _ = exec_lua(rpc, "return 1").await;
    let map = wait_redraw(inc, |m| {
        window0_field(m, "diagnostics_signs").is_some() && window0_field(m, "numbers").is_some()
    })
    .await;
    let arr = |k: &str| {
        window0_field(&map, k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let signs = arr("diagnostics_signs");
    arr("numbers")
        .iter()
        .zip(signs.iter())
        .filter_map(|(n, s)| {
            let line = n.as_u64()?;
            let has = s
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .is_some();
            Some((line, has))
        })
        .collect()
}

#[tokio::test]
async fn the_position_index_agrees_with_the_marks_across_random_edits() {
    // A differential check of the index against the store it duplicates. The buffer
    // is taller than the window, which is scrolled into its middle, so every query is
    // a real sub-range. After each random edit the mirror's positions arrive through
    // the windowed refresh (which asks the index, `reaching`), and the signs through
    // `starting_in`; a structural change then forces a full re-read of every mark
    // straight from the store. The two reads must agree, and every visible line the
    // store puts a mark on (and only those) shows a sign. Marks mix both gravities,
    // points and ranges, and pile up as edits collapse them, so ties and re-sorted
    // runs are exercised. Most edits keep the line count, which bounds the refresh
    // window on both sides.
    let text: String = (0..90).map(|i| format!("ab{i}cdefgh\n")).collect();
    let (rpc, mut inc) = start_with_file(&text).await;
    exec_lua(
        &rpc,
        "_G.fz = { seed = 12345, ns = btv.ns.create('fz'), bump = btv.ns.create('bump') }
         function _G.fz.rand(n)
           _G.fz.seed = (_G.fz.seed * 1103515245 + 12345) % 2147483648
           return (_G.fz.seed // 65536) % n
         end
         local r = _G.fz.rand
         for _ = 1, 400 do
           local row = r(90)
           local col = r(8)
           local opts = { sign_text = 'x', right_gravity = r(2) == 0 }
           if r(2) == 0 then
             opts.end_row = math.min(89, row + r(3))
             opts.end_col = (opts.end_row == row) and math.min(8, col + r(4)) or r(8)
             opts.end_right_gravity = r(2) == 0
           end
           btv.buf.set_extmark(0, _G.fz.ns, row, col, opts)
         end
         btv.cmd('45')
         btv.cmd('normal! zz')",
    )
    .await;
    for step in 0..150 {
        // Draw a frame so the index is built and must be carried across the edit.
        let _ = fuzz_sign_rows(&rpc, &mut inc).await;
        let edit: String = exec_lua(
            &rpc,
            "local r = _G.fz.rand
             local lines = btv.buf.lines(0, 0, -1)
             local n = #lines
             local sr = 30 + r(30)
             local sc = r(#lines[sr + 1] + 1)
             local er, ec = sr, sc
             if r(4) ~= 0 then
               er = sr + r(2)
               ec = (er == sr) and math.min(#lines[er + 1], sc + r(5)) or r(#lines[er + 1] + 1)
             end
             local rep = ({ { '' }, { 'zz' }, { 'q', 'w' }, { 'yyy' }, { 'k' } })[r(5) + 1]
             -- Keep the line count near 90: a line-count change only every so often.
             local delta = (#rep - 1) - (er - sr)
             if (n > 92 and delta > 0) or (n < 88 and delta < 0) or r(3) ~= 0 then
               if delta ~= 0 then er = sr; ec = math.min(#lines[sr + 1], sc + r(3)); rep = { 'mm' } end
             end
             btv.buf.set_text(0, sr, sc, er, ec, rep)
             return string.format('%d,%d-%d,%d %s', sr, sc, er, ec, table.concat(rep, '|'))",
        )
        .await
        .as_str()
        .unwrap_or_default()
        .to_owned();
        let windowed = fuzz_snapshot(&rpc).await;
        let rows = fuzz_sign_rows(&rpc, &mut inc).await;
        exec_lua(&rpc, "btv.buf.set_extmark(0, _G.fz.bump, 0, 0, { id = 1 })").await;
        let full = fuzz_snapshot(&rpc).await;
        assert_eq!(
            windowed, full,
            "step {step} ({edit}): the index-scoped refresh disagrees with the store"
        );
        let marked: std::collections::BTreeSet<u64> = exec_lua(
            &rpc,
            "local s = {}
             for _, e in ipairs(btv.buf.extmarks(0, _G.fz.ns, 0, -1)) do s[#s + 1] = e[2] + 1 end
             return s",
        )
        .await
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_u64)
        .collect();
        assert!(rows.len() > 5, "the window shows lines: {rows:?}");
        for (line, has) in &rows {
            assert_eq!(
                *has,
                marked.contains(line),
                "step {step} ({edit}): line {line} sign {has}, rows {rows:?}"
            );
        }
    }
}
