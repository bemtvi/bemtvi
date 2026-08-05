//! Black-box tests for `vim.diagnostic.config({ update_in_insert = … })`: *when* a
//! diagnostic update that lands while you are typing reaches the screen.
//!
//! A source republishes per keystroke — a language server after every `didChange`, a
//! linter plugin calling `vim.diagnostic.set` — so applying each one makes the
//! squiggles, gutter signs and inline messages churn under the cursor over errors
//! that exist only because the line isn't finished. nxvim's default is a debounce:
//! updates are held while you type and applied once typing goes quiet (3s), with
//! neovim's two settings still reachable — `true` (apply at once) and `false` (hold
//! until `InsertLeave`).
//!
//! Driven through the client-set surface (`nx.diagnostic.set`), which shares the
//! pause gate, the debounce and every render surface with the server-pushed set, so
//! the mechanism is covered without standing up a language server. The LSP-publish
//! half of the same gate is covered in `crates/nxvim/tests/lsp_features.rs`.
//!
//! Timing note: every test that asserts "still held" configures a *long* interval
//! (a minute), so a loaded machine can't let the debounce elapse mid-assertion; the
//! one test that asserts the debounce *fires* configures a short one and polls
//! without an upper bound on how late it may be.

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, barrier, drain_to_latest_redraw, exec_lua, feed, mode, spawn, window0_field,
    write_n_lines,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = spawn(ServerInit::default());
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Per-row underline-span count from the focused window's `diagnostics` payload —
/// what the client actually paints. Empty when the key is absent.
fn diag_span_counts(map: &[(Value, Value)]) -> Vec<usize> {
    window0_field(map, "diagnostics")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|r| r.as_array().map_or(0, Vec::len))
                .collect()
        })
        .unwrap_or_default()
}

/// The 0-based buffer rows currently carrying an underline span. Every caller has
/// already round-tripped the state-changing request, so the *latest* frame is the
/// settled one (CLAUDE.md's take-latest rule); the retry is only for a channel that
/// hasn't been fed a frame yet.
async fn painted_rows(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<usize> {
    for _ in 0..50 {
        barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |_| true) {
            return diag_span_counts(&map)
                .into_iter()
                .enumerate()
                .filter(|&(_, n)| n > 0)
                .map(|(row, _)| row)
                .collect();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no redraw arrived");
}

/// Poll the painted rows until they equal `want` (or give up after ~10s, returning
/// whatever was last seen). Used where a *timer* — not a request — is what applies
/// the change, so there is no round-trip to synchronize on.
async fn await_painted_rows(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &[usize],
) -> Vec<usize> {
    let mut last = Vec::new();
    for _ in 0..200 {
        last = painted_rows(rpc, incoming).await;
        if last == want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    last
}

/// Open a 7-line file and put one client-set diagnostic on 0-based line 1.
async fn open_with_a_diagnostic(rpc: &Rpc) {
    let path = write_n_lines("diagnostic_insert", 7);
    feed(rpc, &format!(":e {path}<CR>"));
    exec_lua(
        rpc,
        r#"nx.diagnostic.set(nx.ns.create("insert_pause"), 0, {
             { lnum = 1, col = 0, end_lnum = 1, end_col = 4,
               severity = nx.diagnostic.severity.ERROR, message = "before" },
           })"#,
    )
    .await;
}

/// Replace the set with a single diagnostic on 0-based line 3.
async fn move_the_diagnostic(rpc: &Rpc) {
    exec_lua(
        rpc,
        r#"nx.diagnostic.set(nx.ns.create("insert_pause"), 0, {
             { lnum = 3, col = 0, end_lnum = 3, end_col = 4,
               severity = nx.diagnostic.severity.ERROR, message = "after" },
           })"#,
    )
    .await;
}

/// The headline behavior: an update landing while you type doesn't move anything,
/// and lands by itself once typing has been quiet for the interval — no need to
/// leave insert mode.
#[tokio::test]
async fn a_held_update_applies_when_typing_goes_quiet() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 150 })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    move_the_diagnostic(&rpc).await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![1],
        "the update must not repaint the instant it lands"
    );

    assert_eq!(
        await_painted_rows(&rpc, &mut incoming, &[3]).await,
        vec![3],
        "the debounce applies it once typing goes quiet"
    );
    assert_eq!(
        mode(&rpc).await,
        "i",
        "and it did so *in* insert mode — no `<Esc>` was needed"
    );
}

/// Leaving insert short-circuits the wait: you don't sit through the interval after
/// pressing `<Esc>`.
#[tokio::test]
async fn insert_leave_applies_a_held_update_without_waiting() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    move_the_diagnostic(&rpc).await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![1],
        "held — a minute of quiet is nowhere near up"
    );

    feed(&rpc, "<Esc>");
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![3],
        "`InsertLeave` applies the held update at once"
    );
}

/// `update_in_insert = false` is neovim's setting: nothing lands before
/// `InsertLeave`, however long you pause.
#[tokio::test]
async fn update_in_insert_false_holds_until_insert_leave() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = false })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    move_the_diagnostic(&rpc).await;
    // Sit idle well past any debounce interval: with the timer disabled, nothing
    // may apply on its own.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![1],
        "no timer is armed at all, so idling changes nothing"
    );

    feed(&rpc, "<Esc>");
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![3],
        "`InsertLeave` applies the held update"
    );
}

/// `update_in_insert = true` is neovim's other setting: every update repaints the
/// moment it lands, insert mode or not.
#[tokio::test]
async fn update_in_insert_true_applies_immediately() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = true })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    move_the_diagnostic(&rpc).await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![3],
        "with `update_in_insert = true`, a mid-insert update repaints at once"
    );
}

/// A *clear* is held the same way — an empty update is a pending clear, not
/// "nothing was held".
#[tokio::test]
async fn a_clear_during_insert_is_held_too() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    exec_lua(
        &rpc,
        r#"nx.diagnostic.reset(nx.ns.create("insert_pause"), 0)"#,
    )
    .await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![1],
        "a clear landing mid-insert must not repaint either"
    );

    feed(&rpc, "<Esc>");
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        Vec::<usize>::new(),
        "`InsertLeave` applies the held clear"
    );
}

/// Only the *last* held update survives: a source republishing per keystroke leaves
/// one set to apply, not a backlog to replay.
#[tokio::test]
async fn only_the_newest_held_update_is_applied() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    move_the_diagnostic(&rpc).await;
    exec_lua(
        &rpc,
        r#"nx.diagnostic.set(nx.ns.create("insert_pause"), 0, {
             { lnum = 5, col = 0, end_lnum = 5, end_col = 4,
               severity = nx.diagnostic.severity.ERROR, message = "newest" },
           })"#,
    )
    .await;

    feed(&rpc, "<Esc>");
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![5],
        "the newest held update wins; the ones it superseded are gone"
    );
}

/// Re-configuring mid-insert takes effect on what is *already* held, rather than
/// applying only from the next update on.
#[tokio::test]
async fn switching_to_immediate_flushes_what_was_held() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;
    open_with_a_diagnostic(&rpc).await;

    feed(&rpc, "iabc");
    move_the_diagnostic(&rpc).await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![1],
        "held under the long interval"
    );

    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = true })").await;
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![3],
        "switching to immediate flushes the held update"
    );
}

/// A diagnostic is anchored to the text, not to the coordinates it was published
/// at: inserting lines *above* it carries it down, so the squiggle stays on the
/// line it flagged instead of sitting on whatever text later moved into that row.
///
/// This is what makes holding an update honest — the set on screen keeps meaning
/// what it meant, for the whole time it is held.
#[tokio::test]
async fn a_held_diagnostic_follows_the_text_it_flags() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;
    open_with_a_diagnostic(&rpc).await;
    assert_eq!(painted_rows(&rpc, &mut incoming).await, vec![1]);

    // Open two lines above the flagged one, typing into the first. The diagnostic
    // was published for row 1; its text is now on row 3.
    feed(&rpc, "ggOfirst<CR>second");
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![3],
        "the diagnostic rides the two inserted lines down"
    );

    // Undo puts the text back where it was, and the diagnostic with it.
    feed(&rpc, "<Esc>u");
    assert_eq!(
        painted_rows(&rpc, &mut incoming).await,
        vec![1],
        "and back up again when the insertion is undone"
    );
}

/// Within a line, a diagnostic keeps its grip on the *text*: typing in front of it
/// pushes the span right rather than leaving the squiggle over whatever character
/// happens to land under those columns.
#[tokio::test]
async fn a_held_diagnostic_follows_the_text_within_its_line() {
    let (rpc, mut incoming) = start().await;
    exec_lua(&rpc, "vim.diagnostic.config({ update_in_insert = 60000 })").await;
    let path = write_n_lines("diagnostic_insert", 7);
    feed(&rpc, &format!(":e {path}<CR>"));
    // `write_n_lines` writes `line 1`, `line 2`, … — flag the `2` on 0-based line 1.
    exec_lua(
        &rpc,
        r#"nx.diagnostic.set(nx.ns.create("insert_pause"), 0, {
             { lnum = 1, col = 5, end_lnum = 1, end_col = 6,
               severity = nx.diagnostic.severity.ERROR, message = "the 2" },
           })"#,
    )
    .await;
    let cols = |map: &[(Value, Value)]| -> Vec<(u64, u64)> {
        window0_field(map, "diagnostics")
            .and_then(Value::as_array)
            .and_then(|rows| rows.get(1).cloned())
            .and_then(|row| row.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|s| {
                let s = s.as_array()?;
                Some((s.first()?.as_u64()?, s.get(1)?.as_u64()?))
            })
            .collect()
    };

    barrier(&rpc).await;
    let before = cols(&drain_to_latest_redraw(&mut incoming, |_| true).expect("redraw"));
    assert_eq!(before, vec![(5, 6)], "flagged the `2` at column 5");

    // Insert three characters at the start of that line.
    feed(&rpc, "1Gjiabc<Esc>");
    barrier(&rpc).await;
    let after = cols(&drain_to_latest_redraw(&mut incoming, |_| true).expect("redraw"));
    assert_eq!(
        after,
        vec![(8, 9)],
        "the squiggle moved with the text it flags, not stayed at column 5"
    );
}

/// The config round-trips: the default is the 3s debounce, and all three forms are
/// echoed back as given.
#[tokio::test]
async fn update_in_insert_defaults_to_a_3s_debounce_and_round_trips() {
    let (rpc, _incoming) = start().await;
    let before = exec_lua(
        &rpc,
        "return tostring(vim.diagnostic.config().update_in_insert)",
    )
    .await;
    assert_eq!(
        before.as_str(),
        Some("3000"),
        "the default is a 3-second debounce"
    );

    for form in ["true", "false", "500"] {
        exec_lua(
            &rpc,
            &format!("vim.diagnostic.config({{ update_in_insert = {form} }})"),
        )
        .await;
        let got = exec_lua(
            &rpc,
            "return tostring(vim.diagnostic.config().update_in_insert)",
        )
        .await;
        assert_eq!(got.as_str(), Some(form), "`{form}` is echoed back as given");
    }
}
