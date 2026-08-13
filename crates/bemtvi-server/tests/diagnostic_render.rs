//! Black-box tests for the **viewport-scoped** diagnostic render projections.
//!
//! The underline, gutter-sign and inline-virtual-text surfaces each used to rebuild
//! the buffer's whole merged diagnostic list per window per frame, and then bucket
//! every entry by `byte_to_line` — a rope lookup per diagnostic, three times a frame
//! — when only the ~50 visible rows can contribute anything. They now prune to the
//! viewport's byte range with an integer compare first. See
//! `docs/plans/2026-08-08-per-keystroke-costs-round-2.md`.
//!
//! The failure mode of a range prune is an off-by-one at either edge silently
//! *hiding* a diagnostic, so both edges are pinned here — against what actually
//! reaches the client, not against the pruning helper. The multi-line case is the
//! one a naive "anchor line in the viewport" prune gets wrong: a diagnostic that
//! starts above the viewport and reaches into it must still underline the rows it
//! covers.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{exec_lua, feed, lines, start_with_file, wait_redraw, window0_field};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

fn sample(n: usize) -> String {
    (0..n)
        .map(|i| format!("line {i} with some text\n"))
        .collect()
}

/// Settle pending work and take the next frame that carries a rendered window.
async fn frame(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<(Value, Value)> {
    let _ = exec_lua(rpc, "return 1").await;
    wait_redraw(inc, |m| window0_field(m, "numbers").is_some()).await
}

/// The 1-based buffer line each rendered row shows (`None` for `~` filler).
fn numbers(map: &[(Value, Value)]) -> Vec<Option<u64>> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::as_u64).collect())
        .unwrap_or_default()
}

/// The first and last **0-based buffer lines** the frame actually shows.
fn visible_range(map: &[(Value, Value)]) -> (usize, usize) {
    let nums: Vec<u64> = numbers(map).into_iter().flatten().collect();
    assert!(!nums.is_empty(), "the frame rendered no buffer lines");
    (
        (*nums.first().unwrap() - 1) as usize,
        (*nums.last().unwrap() - 1) as usize,
    )
}

/// Per rendered row, the diagnostic underline spans as `(start_col, end_col, severity)`.
fn diag_spans(map: &[(Value, Value)]) -> Vec<Vec<(u64, u64, u64)>> {
    window0_field(map, "diagnostics")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| {
                                    let s = v.as_array()?;
                                    Some((
                                        s.first()?.as_u64()?,
                                        s.get(1)?.as_u64()?,
                                        s.get(2)?.as_u64()?,
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn diag_pairs(map: &[(Value, Value)], key: &str) -> Vec<Option<(String, u64)>> {
    window0_field(map, key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|row| {
                    let s = row.as_array()?;
                    Some((s.first()?.as_str()?.to_string(), s.get(1)?.as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The rendered row index showing 0-based buffer line `line`.
fn row_of(map: &[(Value, Value)], line: usize) -> usize {
    numbers(map)
        .iter()
        .position(|n| *n == Some(line as u64 + 1))
        .unwrap_or_else(|| panic!("buffer line {line} is not on screen"))
}

/// Set one client diagnostic covering `[col, end_col)` from `lnum` to `end_lnum`.
async fn set_diag(rpc: &Rpc, lnum: usize, col: usize, end_lnum: usize, end_col: usize) {
    exec_lua(
        rpc,
        &format!(
            r#"
            vim.diagnostic.config({{ underline = true, virtual_text = true, signs = true }})
            vim.diagnostic.set(1, 0, {{
              {{ lnum = {lnum}, col = {col}, end_lnum = {end_lnum}, end_col = {end_col},
                 severity = 1, message = "boom" }},
            }})
            "#
        ),
    )
    .await;
}

/// Assert the three render surfaces all carry the diagnostic on `line`'s row.
fn assert_painted(map: &[(Value, Value)], line: usize, what: &str) {
    let row = row_of(map, line);
    let spans = diag_spans(map);
    assert!(
        !spans.get(row).cloned().unwrap_or_default().is_empty(),
        "{what}: no underline on buffer line {line} (row {row}): {spans:?}",
    );
    let signs = diag_pairs(map, "diagnostics_signs");
    assert!(
        signs.get(row).cloned().flatten().is_some(),
        "{what}: no gutter sign on buffer line {line} (row {row}): {signs:?}",
    );
    let virt = diag_pairs(map, "diagnostics_virt");
    assert!(
        virt.get(row).cloned().flatten().is_some(),
        "{what}: no virtual text on buffer line {line} (row {row}): {virt:?}",
    );
}

#[tokio::test]
async fn a_diagnostic_on_the_first_visible_row_paints() {
    // The low edge of the prune. Scroll so the viewport starts well into the buffer,
    // learn exactly which line that is, and put the diagnostic on it.
    let (rpc, mut inc) = start_with_file(&sample(400)).await;
    feed(&rpc, "200Gzt");
    let _ = lines(&rpc).await;
    let (first, _) = visible_range(&frame(&rpc, &mut inc).await);

    set_diag(&rpc, first, 0, first, 4).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(
        visible_range(&map).0,
        first,
        "the viewport moved between the two frames",
    );
    assert_painted(&map, first, "first visible row");
}

#[tokio::test]
async fn a_diagnostic_on_the_last_visible_row_paints() {
    // The high edge — the off-by-one that a `<` where `<=` belongs would eat.
    let (rpc, mut inc) = start_with_file(&sample(400)).await;
    feed(&rpc, "200Gzt");
    let _ = lines(&rpc).await;
    let (_, last) = visible_range(&frame(&rpc, &mut inc).await);

    set_diag(&rpc, last, 0, last, 4).await;
    let map = frame(&rpc, &mut inc).await;
    assert_eq!(
        visible_range(&map).1,
        last,
        "the viewport moved between the two frames",
    );
    assert_painted(&map, last, "last visible row");
}

#[tokio::test]
async fn a_multi_line_diagnostic_entering_from_above_still_underlines() {
    // Starts three lines above the viewport and ends two inside it. Its *anchor* is
    // off-screen, so a prune that keys on the start line alone drops it — but the
    // rows it covers inside the viewport must still be underlined.
    let (rpc, mut inc) = start_with_file(&sample(400)).await;
    feed(&rpc, "200Gzt");
    let _ = lines(&rpc).await;
    let (first, _) = visible_range(&frame(&rpc, &mut inc).await);
    assert!(first >= 3, "need room above the viewport");

    set_diag(&rpc, first - 3, 0, first + 1, 4).await;
    let map = frame(&rpc, &mut inc).await;
    let spans = diag_spans(&map);
    for line in [first, first + 1] {
        let row = row_of(&map, line);
        assert!(
            !spans.get(row).cloned().unwrap_or_default().is_empty(),
            "a diagnostic reaching in from above must underline line {line} (row {row}): {spans:?}",
        );
    }
}

#[tokio::test]
async fn a_multi_line_diagnostic_leaving_below_still_underlines() {
    // The mirror case: anchored on the last visible line and running off the bottom.
    let (rpc, mut inc) = start_with_file(&sample(400)).await;
    feed(&rpc, "200Gzt");
    let _ = lines(&rpc).await;
    let (_, last) = visible_range(&frame(&rpc, &mut inc).await);

    set_diag(&rpc, last, 0, last + 5, 4).await;
    let map = frame(&rpc, &mut inc).await;
    assert_painted(&map, last, "last visible row, running off the bottom");
}

#[tokio::test]
async fn a_diagnostic_scrolled_into_view_paints() {
    // Set while off-screen, then scrolled to: the prune is per-frame, so the frame
    // that first shows the line must find it. A prune cached across frames fails here.
    let (rpc, mut inc) = start_with_file(&sample(400)).await;
    let _ = lines(&rpc).await;
    set_diag(&rpc, 300, 0, 300, 4).await;

    let map = frame(&rpc, &mut inc).await;
    assert!(
        !numbers(&map).contains(&Some(301)),
        "line 300 must start off-screen for this test to mean anything",
    );

    feed(&rpc, "301Gzt");
    let _ = lines(&rpc).await;
    let map = frame(&rpc, &mut inc).await;
    assert_painted(&map, 300, "scrolled into view");
}

#[tokio::test]
async fn the_statusline_count_sees_every_diagnostic_not_just_the_visible_ones() {
    // `diag_counts_for` feeds the statusline segment and counts the WHOLE buffer;
    // it must not be pruned along with the render surfaces. Two diagnostics, one of
    // them far off-screen.
    let (rpc, mut inc) = start_with_file(&sample(400)).await;
    let _ = lines(&rpc).await;
    exec_lua(
        &rpc,
        r#"
        vim.diagnostic.set(1, 0, {
          { lnum = 0,   col = 0, end_lnum = 0,   end_col = 4, severity = 1, message = "near" },
          { lnum = 350, col = 0, end_lnum = 350, end_col = 4, severity = 1, message = "far" },
          { lnum = 351, col = 0, end_lnum = 351, end_col = 4, severity = 2, message = "warn" },
        })
        "#,
    )
    .await;
    let _ = frame(&rpc, &mut inc).await;
    assert_eq!(
        exec_lua(&rpc, "return #vim.diagnostic.get(0)")
            .await
            .as_u64(),
        Some(3),
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return #vim.diagnostic.get(0, { severity = vim.diagnostic.severity.ERROR })"
        )
        .await
        .as_u64(),
        Some(2),
        "the off-screen error still counts",
    );
}

// ------------------------------------------------------------------ perf guard

async fn time_typing(rpc: &Rpc, keys: usize) -> std::time::Duration {
    feed(rpc, "2500GI");
    let _ = lines(rpc).await;
    let started = std::time::Instant::now();
    for _ in 0..keys {
        feed(rpc, "z");
    }
    let _ = lines(rpc).await;
    started.elapsed()
}

#[tokio::test]
async fn typing_does_not_scale_with_the_diagnostic_count() {
    // The regression guard. Rebuilding the merged list and bucketing every entry by
    // `byte_to_line` on each of three surfaces, per frame, made 300 keystrokes with
    // 5000 diagnostics take 17.5 s against a 0.49 s baseline (36x), scaling cleanly
    // with the count. A **ratio** rather than a wall-clock bound so both halves see
    // the same machine load under a loaded `cargo test --workspace`.
    let (plain, _i1) = start_with_file(&sample(5_000)).await;
    exec_lua(&plain, "btv.on('CursorMoved', function() end)").await;
    let baseline = time_typing(&plain, 300).await;

    let (heavy, _i2) = start_with_file(&sample(5_000)).await;
    exec_lua(&heavy, "btv.on('CursorMoved', function() end)").await;
    exec_lua(
        &heavy,
        r#"
        vim.diagnostic.config({ underline = true, virtual_text = true, signs = true })
        local ds = {}
        for i = 1, 5000 do
          ds[i] = { lnum = i - 1, col = 0, end_lnum = i - 1, end_col = 3,
                    severity = 1, message = "diag " .. i }
        end
        vim.diagnostic.set(1, 0, ds)
        "#,
    )
    .await;
    assert_eq!(
        exec_lua(&heavy, "return #vim.diagnostic.get(0)")
            .await
            .as_u64(),
        Some(5_000),
        "the benchmark must not pass by having set no diagnostics",
    );
    let loaded = time_typing(&heavy, 300).await;

    let ratio = loaded.as_secs_f64() / baseline.as_secs_f64().max(0.001);
    assert!(
        ratio < 3.0,
        "typing with 5000 diagnostics cost {ratio:.1}x a diagnostic-free buffer \
         ({loaded:?} vs {baseline:?}) — the render projections are walking the whole \
         merged list per frame again",
    );
}

// ================================================ an end-exclusive span ends where it says
//
// A multi-line diagnostic's `end` is EXCLUSIVE: `[2,0)-[4,0)` reaches up to row 4, not
// onto it. The row clip mapped both anchors of a zero-width intersection to 0 and then
// widened `(0, 0)` into a 1-cell span, so the row *after* the diagnostic grew a phantom
// squiggle at column 0 — a red mark on a line the server never flagged.

#[tokio::test]
async fn a_multiline_span_does_not_squiggle_the_row_it_stops_before() {
    let (rpc, mut inc) = start_with_file(&sample(20)).await;
    // Rows 2 and 3 are covered; row 4 is where the span ENDS, so it must be clean.
    set_diag(&rpc, 2, 0, 4, 0).await;
    let map = frame(&rpc, &mut inc).await;

    let spans = diag_spans(&map);
    for line in [2usize, 3] {
        let row = row_of(&map, line);
        assert!(
            !spans.get(row).cloned().unwrap_or_default().is_empty(),
            "line {line} is inside the span and must be underlined: {spans:?}"
        );
    }
    let end_row = row_of(&map, 4);
    assert!(
        spans.get(end_row).cloned().unwrap_or_default().is_empty(),
        "line 4 is the span's EXCLUSIVE end — it must carry no underline, but got \
         {:?} (a zero-width intersection widened into a phantom 1-cell squiggle)",
        spans.get(end_row)
    );
}

#[tokio::test]
async fn a_zero_width_diagnostic_still_paints_on_its_own_row() {
    // The counterpart: a genuinely zero-width diagnostic resting at its own start is
    // a real mark (a "missing semicolon here" caret), and must survive the guard that
    // suppresses the phantom one.
    let (rpc, mut inc) = start_with_file(&sample(20)).await;
    set_diag(&rpc, 3, 0, 3, 0).await;
    let map = frame(&rpc, &mut inc).await;

    let row = row_of(&map, 3);
    assert!(
        !diag_spans(&map)
            .get(row)
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "a zero-width diagnostic on its own row is a real mark and must paint"
    );
}

// ========================================== a position the LSP wire cannot carry is refused
//
// `vim.diagnostic.set` takes i64 positions from Lua; the LSP wire carries u32. An
// `as u32` truncation maps a huge line number onto a small one and plants a real
// squiggle on the WRONG line. The whole set is rejected instead, leaving the previous
// one in force — a plugin bug must not silently relabel the buffer.

#[tokio::test]
async fn a_diagnostic_past_the_wire_range_is_rejected_without_disturbing_the_old_set() {
    let (rpc, mut inc) = start_with_file(&sample(20)).await;
    set_diag(&rpc, 3, 0, 3, 4).await;
    let map = frame(&rpc, &mut inc).await;
    let good_row = row_of(&map, 3);
    assert!(
        !diag_spans(&map)
            .get(good_row)
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "the first set is in force"
    );

    // 2^32 + 5 truncates to 5 in u32 — a line that exists in this buffer, so a
    // truncating implementation paints a convincing squiggle in the wrong place.
    exec_lua(
        &rpc,
        r#"vim.diagnostic.set(1, 0, {
             { lnum = 4294967301, col = 0, end_lnum = 4294967301, end_col = 1,
               severity = 1, message = "out of range" },
           })"#,
    )
    .await;
    let map = frame(&rpc, &mut inc).await;
    let spans = diag_spans(&map);

    let bad_row = row_of(&map, 5);
    assert!(
        spans.get(bad_row).cloned().unwrap_or_default().is_empty(),
        "line 5 is where 2^32+5 TRUNCATES to — it must not be flagged: {spans:?}"
    );
    assert!(
        !spans.get(good_row).cloned().unwrap_or_default().is_empty(),
        "the rejected set must leave the previous one intact, not half-apply: {spans:?}"
    );
}

// ====================================== anchors are trusted only while they are OURS
//
// Diagnostics are projected from extmark anchors so they ride edits, and the anchors
// are addressed BY POSITION in the merged list — so they are only meaningful while the
// list is the one they were placed from. The bookkeeping used to check the mark COUNT
// alone, which an undo can satisfy by accident: undoing past a re-publish restores the
// extmark store as it was under the PREVIOUS set, which may hold exactly as many
// marks. The squiggles then render at the old set's positions.
//
// The namespace's mutation generation is recorded alongside the count, and the
// placement itself bumps it — so a resurrected store carries an older generation and
// the projection falls back to the published ranges.
//
// SCOPE: this pins the user-visible property (an undo never relocates a diagnostic),
// not the generation check itself. Reverting to the count-only test leaves it green:
// the anchors are refreshed on the tick after the undo, before the frame projects, so
// the window in which a resurrected store could be trusted is not reachable by
// driving the editor from outside. The guard is defence for a narrower ordering than
// a black-box test can arrange.

#[tokio::test]
async fn an_undo_that_resurrects_older_anchors_does_not_relocate_the_diagnostics() {
    let (rpc, mut inc) = start_with_file(&sample(20)).await;

    // The first set anchors on line 2…
    set_diag(&rpc, 2, 0, 2, 4).await;
    let map = frame(&rpc, &mut inc).await;
    let old_row = row_of(&map, 2);
    assert!(
        !diag_spans(&map)
            .get(old_row)
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "the first set paints on line 2"
    );

    // …an edit gives the undo somewhere to land, between the two placements…
    feed(&rpc, "10Gx");
    let _ = lines(&rpc).await;

    // …and the second set, the same SIZE (one diagnostic) but a different place,
    // replaces the anchors.
    set_diag(&rpc, 7, 0, 7, 4).await;
    let map = frame(&rpc, &mut inc).await;
    let new_row = row_of(&map, 7);
    assert!(
        !diag_spans(&map)
            .get(new_row)
            .cloned()
            .unwrap_or_default()
            .is_empty(),
        "the second set paints on line 7"
    );

    // Undo past the edit: the extmark store reverts to the snapshot taken while the
    // FIRST set's anchors were in place — one mark, exactly as many as the live list
    // has, so a count-only check is satisfied by a store that is not ours.
    feed(&rpc, "u");
    let map = frame(&rpc, &mut inc).await;
    let spans = diag_spans(&map);

    let old_row = row_of(&map, 2);
    let new_row = row_of(&map, 7);
    assert!(
        spans.get(old_row).cloned().unwrap_or_default().is_empty(),
        "line 2 belongs to a set that is no longer published — trusting the \
         resurrected anchors puts the squiggle back there: {spans:?}"
    );
    assert!(
        !spans.get(new_row).cloned().unwrap_or_default().is_empty(),
        "the live set's own published range must still paint on line 7: {spans:?}"
    );
}
