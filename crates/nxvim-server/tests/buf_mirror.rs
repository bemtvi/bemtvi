//! Black-box tests for the **incremental** Rust→Lua buffer mirror: an edit ships the
//! rows that changed (a line delta the Lua side splices into the array it already
//! holds) instead of re-serializing the whole buffer, which used to make one
//! keystroke cost O(buffer). See `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
//!
//! Every test checks two things, and both matter:
//!
//!  * **Correctness** — the mirror (`nx.buf.lines`, read out of `nx._bufs`) matches
//!    the authoritative core read (`nvim_buf_get_lines`, which never touches the
//!    mirror). A wrong splice shows up as a divergence.
//!  * **That the delta path actually ran** — a splice mutates the array in place, so
//!    it stays the SAME Lua table; a full push installs a new one. Comparing table
//!    identity across an edit tells the two apart, so a test can't quietly pass by
//!    falling back to the full push it was written to replace.

use nxvim_rpc::Rpc;
use nxvim_test_harness::{command, exec_lua, feed, lines, start_with_file as open};
use rmpv::Value;

/// The mirror's own view of the current buffer — what `nx.buf.lines` reads out of
/// `nx._bufs`, which is the array a delta splices into.
async fn mirror(rpc: &Rpc) -> Vec<String> {
    match exec_lua(rpc, "return nx.buf.lines(0, 0, -1, false)").await {
        Value::Array(a) => a
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect(),
        other => panic!("expected the mirror's line array, got {other:?}"),
    }
}

/// Register a handler on a per-keystroke event, so the mirror is pushed on every key
/// the way a loaded config makes it — not only when a chunk reaches Lua.
async fn push_every_key(rpc: &Rpc) {
    exec_lua(rpc, "nx.on('CursorMoved', function() end)").await;
}

/// Pin the identity of the current buffer's mirror array, for [`spliced`].
async fn pin(rpc: &Rpc) {
    exec_lua(rpc, "_G.__pinned = nx._bufs[nx._resolve_bufnr(0)].lines").await;
}

/// Whether the mirror is still the same Lua table pinned by [`pin`] — true when the
/// server spliced a delta into it, false when it re-pushed the buffer whole.
async fn spliced(rpc: &Rpc) -> bool {
    let v = exec_lua(
        rpc,
        "return nx._bufs[nx._resolve_bufnr(0)].lines == _G.__pinned",
    )
    .await;
    matches!(v, Value::Boolean(true))
}

/// The mirror agrees with the core, and (when `by_delta`) got there by splicing.
async fn assert_mirrors(rpc: &Rpc, by_delta: bool) {
    let core = lines(rpc).await;
    let mirrored = mirror(rpc).await;
    assert_eq!(mirrored, core, "the mirror diverged from the buffer");
    assert_eq!(
        spliced(rpc).await,
        by_delta,
        "expected the mirror to be updated {} (delta = {by_delta})",
        if by_delta { "incrementally" } else { "in full" },
    );
}

/// A buffer with enough lines that a whole-buffer re-serialize would be obvious.
fn sample(n: usize) -> String {
    (0..n)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat()
}

#[tokio::test]
async fn typing_inside_a_line_splices_only_that_line() {
    let (rpc, _inc) = open(&sample(200)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    // Insert at the start of line 100, one key at a time — the hot path.
    feed(&rpc, "100GIxy<Esc>");
    assert_mirrors(&rpc, true).await;
    assert_eq!(lines(&rpc).await[99], "xyline 99");
}

#[tokio::test]
async fn opening_a_line_splices_a_growing_span() {
    let (rpc, _inc) = open(&sample(50)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "10Gonew one<CR>new two<Esc>");
    assert_mirrors(&rpc, true).await;
    let core = lines(&rpc).await;
    assert_eq!(core.len(), 52);
    assert_eq!(&core[10..12], ["new one", "new two"]);
}

#[tokio::test]
async fn deleting_lines_splices_a_shrinking_span() {
    let (rpc, _inc) = open(&sample(50)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "20G3dd");
    assert_mirrors(&rpc, true).await;
    let core = lines(&rpc).await;
    assert_eq!(core.len(), 47);
    assert_eq!(core[19], "line 22");
}

#[tokio::test]
async fn joining_lines_splices() {
    let (rpc, _inc) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "5GJ");
    assert_mirrors(&rpc, true).await;
    assert_eq!(lines(&rpc).await[4], "line 4 line 5");
}

#[tokio::test]
async fn deleting_the_first_line_splices_the_head() {
    let (rpc, _inc) = open(&sample(30)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "ggdd");
    assert_mirrors(&rpc, true).await;
    assert_eq!(lines(&rpc).await[0], "line 1");
}

#[tokio::test]
async fn appending_past_the_last_line_splices_the_tail() {
    let (rpc, _inc) = open(&sample(30)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "Gotail<Esc>");
    assert_mirrors(&rpc, true).await;
    let core = lines(&rpc).await;
    assert_eq!(core.len(), 31);
    assert_eq!(core[30], "tail");
}

#[tokio::test]
async fn a_substitution_across_the_buffer_folds_into_one_span() {
    // Many edits land in a single batch — the fold's multi-edit path.
    let (rpc, _inc) = open(&sample(60)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    command(&rpc, "%s/line/row/").await;
    assert_mirrors(&rpc, true).await;
    let core = lines(&rpc).await;
    assert_eq!(core[0], "row 0");
    assert_eq!(core[59], "row 59");
}

#[tokio::test]
async fn undo_pushes_the_buffer_in_full() {
    // Undo replaces the whole rope, so every row anchor is void: the batch is a
    // `resync` and the mirror must be re-pushed whole, not spliced.
    let (rpc, _inc) = open(&sample(40)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "5Gdd");
    assert_mirrors(&rpc, true).await;
    pin(&rpc).await;
    feed(&rpc, "u");
    assert_mirrors(&rpc, false).await;
    assert_eq!(lines(&rpc).await.len(), 40);
}

#[tokio::test]
async fn redo_pushes_the_buffer_in_full() {
    let (rpc, _inc) = open(&sample(40)).await;
    push_every_key(&rpc).await;
    feed(&rpc, "5Gdd");
    feed(&rpc, "u");
    pin(&rpc).await;
    feed(&rpc, "<C-r>");
    assert_mirrors(&rpc, false).await;
    assert_eq!(lines(&rpc).await.len(), 39);
}

#[tokio::test]
async fn a_queued_set_lines_lands_in_the_mirror() {
    // `nx.buf.set_lines` does NOT write through to the mirror — it queues a buffer op
    // that lands through the core and is journaled like any other edit. If it ever
    // did write through, the delta would be applied on top of it and corrupt the
    // array, so this pins the contract.
    let (rpc, _inc) = open(&sample(30)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    exec_lua(&rpc, "nx.buf.set_lines(0, 5, 8, false, { 'a', 'b' })").await;
    assert_mirrors(&rpc, true).await;
    let core = lines(&rpc).await;
    assert_eq!(core.len(), 29);
    assert_eq!(&core[5..7], ["a", "b"]);
}

#[tokio::test]
async fn a_freshly_created_buffer_is_pushed_in_full_then_spliced() {
    let (rpc, _inc) = open(&sample(10)).await;
    push_every_key(&rpc).await;
    command(&rpc, "enew").await;
    // Nothing was mirrored for this buffer before, so its first content must arrive
    // whole; only the edit after that can be a delta.
    pin(&rpc).await;
    feed(&rpc, "ifirst<CR>second<Esc>");
    assert_mirrors(&rpc, true).await;
    assert_eq!(lines(&rpc).await, ["first", "second"]);
}

#[tokio::test]
async fn edits_in_a_background_buffer_stay_mirrored() {
    let (rpc, _inc) = open(&sample(20)).await;
    push_every_key(&rpc).await;
    command(&rpc, "enew").await;
    feed(&rpc, "iscratch<Esc>");
    // Back to the first buffer, whose mirror was last pushed before the switch.
    command(&rpc, "bprevious").await;
    pin(&rpc).await;
    feed(&rpc, "ggIx<Esc>");
    assert_mirrors(&rpc, true).await;
    assert_eq!(lines(&rpc).await[0], "xline 0");
}

#[tokio::test]
async fn replacing_the_whole_buffer_keeps_the_mirror_exact() {
    let (rpc, _inc) = open(&sample(80)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    command(&rpc, "%d").await;
    assert_mirrors(&rpc, true).await;
    assert_eq!(lines(&rpc).await, [""]);
}

#[tokio::test]
async fn a_long_typing_run_never_drifts() {
    // The regression this whole change exists for: sustained per-keystroke edits.
    // Every key splices, and after 200 of them the mirror must still be exact.
    let (rpc, _inc) = open(&sample(300)).await;
    push_every_key(&rpc).await;
    pin(&rpc).await;
    feed(&rpc, "150GI");
    for _ in 0..200 {
        feed(&rpc, "z");
    }
    feed(&rpc, "<Esc>");
    assert_mirrors(&rpc, true).await;
    assert_eq!(
        lines(&rpc).await[149],
        format!("{}line 149", "z".repeat(200))
    );
}

#[tokio::test]
async fn typing_in_a_large_buffer_does_not_scale_with_the_buffer() {
    // The regression guard. Re-serializing the whole buffer on every edit made one
    // keystroke cost O(lines): this exact run took ~47s that way and ~0.8s with the
    // delta, so the bound below has ~3x margin against the old behavior returning and
    // ~19x headroom over the current one (it runs in a debug build, under a loaded
    // `cargo test --workspace`). A failure here means an edit went back to touching
    // the whole buffer — see `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
    let content: String = (0..20_000)
        .map(|i| format!("line {i} with some text\n"))
        .collect();
    let (rpc, _inc) = open(&content).await;
    push_every_key(&rpc).await;
    feed(&rpc, "10000GI");
    // Round-trip so the setup has landed before the clock starts.
    let _ = lines(&rpc).await;

    let started = std::time::Instant::now();
    for _ in 0..300 {
        feed(&rpc, "z");
    }
    let _ = lines(&rpc).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "300 keystrokes in a 20k-line buffer took {elapsed:?} — the mirror is scaling \
         with the buffer again",
    );
    assert_eq!(
        lines(&rpc).await[9_999],
        format!("{}line 9999 with some text", "z".repeat(300))
    );
}
