//! Perf guards for the **computed** fold sources.
//!
//! `refresh_folds` runs from the per-keystroke input hook and its cache key carries
//! the buffer's `changedtick`, so every edit rebuilds the whole fold structure. That
//! is unavoidable in shape — a fold is a property of the whole buffer — but it must
//! not cost a *rope lookup* or a *Lua call* per line. It did both:
//! `foldmethod=indent` and `=marker` read every line through indexed access, and a
//! generic Lua `'foldexpr'` was evaluated once per line per keystroke.
//!
//! See `docs/plans/2026-08-08-per-keystroke-costs-round-2.md`. Fold *correctness*
//! lives in `editing/folds.rs`; this file only pins the cost.

use bemtvi_rpc::Rpc;
use bemtvi_test_harness::{command, exec_lua, feed, lines, start_with_file as open};

const LINES: usize = 5_000;
const KEYS: usize = 300;

/// A buffer with real indent structure and a marker pair every few lines, so both
/// computed sources have something to chew on.
fn sample(n: usize) -> String {
    (0..n)
        .map(|i| match i % 6 {
            0 => format!("block {i} {{{{{{\n"),
            1 | 2 => format!("    inner {i}\n"),
            3 => format!("        deeper {i}\n"),
            4 => format!("    inner {i}\n"),
            _ => format!("end {i} }}}}}}\n"),
        })
        .collect()
}

async fn time_typing(rpc: &Rpc) -> std::time::Duration {
    feed(rpc, "2500GA");
    let _ = lines(rpc).await;
    let started = std::time::Instant::now();
    for _ in 0..KEYS {
        feed(rpc, "z");
    }
    let _ = lines(rpc).await;
    started.elapsed()
}

/// Time `KEYS` keystrokes with `foldmethod` set (empty ⇒ leave it at `manual`).
async fn typing_under(foldmethod: &str) -> std::time::Duration {
    let (rpc, _i) = open(&sample(LINES)).await;
    exec_lua(&rpc, "btv.on('CursorMoved', function() end)").await;
    if !foldmethod.is_empty() {
        command(&rpc, &format!("set foldmethod={foldmethod}")).await;
    }
    time_typing(&rpc).await
}

#[tokio::test]
async fn typing_does_not_scale_with_indent_folds() {
    // 13.0 s against a 0.49 s `manual` baseline before (27x): every keystroke read
    // all 5000 lines back through indexed rope access to re-derive their levels.
    let baseline = typing_under("manual").await;
    let folded = typing_under("indent").await;
    let ratio = folded.as_secs_f64() / baseline.as_secs_f64().max(0.001);
    assert!(
        ratio < 3.0,
        "typing under foldmethod=indent cost {ratio:.1}x a manual-fold buffer \
         ({folded:?} vs {baseline:?}) — the indent scan is walking the buffer the \
         expensive way again",
    );
}

#[tokio::test]
async fn typing_does_not_scale_with_marker_folds() {
    let baseline = typing_under("manual").await;
    let folded = typing_under("marker").await;
    let ratio = folded.as_secs_f64() / baseline.as_secs_f64().max(0.001);
    assert!(
        ratio < 3.0,
        "typing under foldmethod=marker cost {ratio:.1}x a manual-fold buffer \
         ({folded:?} vs {baseline:?}) — the marker scan is walking the buffer the \
         expensive way again",
    );
}

#[tokio::test]
async fn a_lua_foldexpr_is_not_evaluated_for_every_line_on_every_keystroke() {
    // The sharpest form of the bug: a generic `'foldexpr'` was called once per
    // buffer line per keystroke — 1 500 000 Lua calls for this run — when an edit
    // changes the fold value of the rows it touched and nothing else.
    let (rpc, _i) = open(&sample(LINES)).await;
    exec_lua(&rpc, "btv.on('CursorMoved', function() end)").await;
    exec_lua(
        &rpc,
        "_G.calls = 0
         function _G.probe_fold(l)
           _G.calls = _G.calls + 1
           return 0
         end",
    )
    .await;
    command(&rpc, "set foldexpr=v:lua.probe_fold(vim.v.lnum)").await;
    command(&rpc, "set foldmethod=expr").await;

    feed(&rpc, "2500GA");
    let _ = lines(&rpc).await;
    let before = exec_lua(&rpc, "return _G.calls").await.as_u64().unwrap();
    assert!(
        before >= LINES as u64,
        "the foldexpr must have run over the buffer at least once ({before} calls) — \
         otherwise this test proves nothing",
    );

    for _ in 0..KEYS {
        feed(&rpc, "z");
    }
    let _ = lines(&rpc).await;
    let after = exec_lua(&rpc, "return _G.calls").await.as_u64().unwrap();

    let per_key = (after - before) as f64 / KEYS as f64;
    assert!(
        per_key < 50.0,
        "a keystroke evaluated the foldexpr {per_key:.0} times ({} calls over {KEYS} \
         keystrokes in a {LINES}-line buffer) — it is being re-run for the whole buffer",
        after - before,
    );
}
