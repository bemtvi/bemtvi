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

use nxvim_rpc::{Incoming, Rpc};
use nxvim_test_harness::{exec_lua, feed, lines, start_with_file, wait_redraw, window0_field};
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
    exec_lua(&plain, "nx.on('CursorMoved', function() end)").await;
    let baseline = time_typing(&plain, 300).await;

    let (heavy, _i2) = start_with_file(&sample(5_000)).await;
    exec_lua(&heavy, "nx.on('CursorMoved', function() end)").await;
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
