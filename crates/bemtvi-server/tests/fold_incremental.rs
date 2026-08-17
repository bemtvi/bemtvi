//! Correctness tests for the **incremental** computed-fold inputs.
//!
//! Each computed fold source derives one value per line from that line's text
//! alone, so the editor caches those values per buffer and splices only the rows an
//! edit touched instead of re-deriving the whole buffer on every keystroke (see
//! `docs/plans/2026-08-08-per-keystroke-costs-round-2.md`). A splice can be wrong in
//! ways a from-scratch derivation cannot, so every test here is an **oracle
//! comparison**: reach some text by *editing*, then fold the identical text opened
//! *fresh*, and require the two fold structures to be indistinguishable.
//!
//! That oracle is what makes these tests worth having. Asserting a hand-written
//! expected fold shape would pass just as happily against a cache that is subtly
//! stale in a way the author did not think to check; comparing against the
//! correct-by-construction path cannot.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_test_harness::{
    command, exec_lua, feed, lines, start_with_file, wait_redraw, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// The 1-based buffer lines the frame actually shows. With `foldlevel=0` every
/// computed fold is closed, so this *is* the fold structure: a closed fold shows its
/// header line and hides the rest.
fn visible(map: &[(Value, Value)]) -> Vec<u64> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

async fn frame(rpc: &Rpc, inc: &mut UnboundedReceiver<Incoming>) -> Vec<(Value, Value)> {
    let _ = exec_lua(rpc, "return 1").await;
    wait_redraw(inc, |m| window0_field(m, "numbers").is_some()).await
}

/// Open `text`, apply `setup` (the fold configuration), feed `keys`, and report the
/// buffer's final lines alongside the fold structure that resulted.
async fn edit_then_fold(text: &str, setup: &[&str], keys: &str) -> (Vec<String>, Vec<u64>) {
    let (rpc, mut inc) = start_with_file(text).await;
    for cmd in setup {
        command(&rpc, cmd).await;
    }
    if !keys.is_empty() {
        feed(&rpc, keys);
        let _ = lines(&rpc).await;
    }
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let text = lines(&rpc).await;
    (text, visible(&frame(&rpc, &mut inc).await))
}

/// The same fold configuration applied to `text` from scratch — the oracle.
async fn fold_fresh(text: &str, setup: &[&str]) -> Vec<u64> {
    edit_then_fold(text, setup, "").await.1
}

/// Reach some text by editing, then require the folds to match that text folded fresh.
async fn assert_matches_fresh(text: &str, setup: &[&str], keys: &str, what: &str) {
    let (final_text, edited) = edit_then_fold(text, setup, keys).await;
    let joined = format!("{}\n", final_text.join("\n"));
    let fresh = fold_fresh(&joined, setup).await;
    assert_eq!(
        edited, fresh,
        "{what}: folds after editing disagree with the same text folded fresh\n\
         text: {final_text:?}",
    );
}

fn indented() -> String {
    "func()\n    a\n    b\n        deep\n    c\nqux\ntail\n".to_string()
}

fn marked() -> String {
    "head\nblock {{{\n  one\n  two\nend }}}\nbetween\nsecond {{{\n  three\nclose }}}\ntail\n"
        .to_string()
}

const INDENT: &[&str] = &["set foldmethod=indent"];
const MARKER: &[&str] = &["set foldmethod=marker"];

// ------------------------------------------------------------------- indent

#[tokio::test]
async fn indent_folds_survive_typing_into_a_line() {
    assert_matches_fresh(&indented(), INDENT, "2GAxyz<Esc>", "typing into a line").await;
}

#[tokio::test]
async fn indent_folds_survive_changing_a_lines_indent() {
    // The edited row's own level changes, which is the splice's whole point.
    assert_matches_fresh(&indented(), INDENT, "5GI    <Esc>", "deepening a line").await;
    assert_matches_fresh(&indented(), INDENT, "4G^hhhhhhhhD", "flattening a line").await;
}

#[tokio::test]
async fn indent_folds_survive_inserting_lines() {
    // Row count grows: every cached row after the insert must shift, not be reused
    // in place.
    assert_matches_fresh(
        &indented(),
        INDENT,
        "2Go        new one<CR>    new two<Esc>",
        "inserting two lines",
    )
    .await;
}

#[tokio::test]
async fn indent_folds_survive_deleting_lines() {
    assert_matches_fresh(&indented(), INDENT, "3G2dd", "deleting two lines").await;
    assert_matches_fresh(&indented(), INDENT, "1GdG", "deleting everything").await;
}

#[tokio::test]
async fn indent_folds_survive_joining_lines() {
    assert_matches_fresh(&indented(), INDENT, "2GJ", "joining lines").await;
}

#[tokio::test]
async fn indent_folds_survive_undo() {
    // Undo replaces the whole rope, so the cached rows are meaningless — the fold
    // journal reports a resync and the inputs must be rebuilt, not spliced.
    assert_matches_fresh(&indented(), INDENT, "2Go        new<Esc>u", "undo").await;
    assert_matches_fresh(
        &indented(),
        INDENT,
        "2Go        new<Esc>u<C-r>",
        "undo then redo",
    )
    .await;
}

#[tokio::test]
async fn indent_folds_survive_a_multi_edit_batch() {
    // A single Lua chunk queuing several `set_lines` lands as one batch of edits,
    // which is the shape a naive fold of the journal gets wrong (and which the
    // buffer mirror's own plan found crashing the server).
    let (rpc, mut inc) = start_with_file(&indented()).await;
    command(&rpc, "set foldmethod=indent").await;
    exec_lua(
        &rpc,
        "btv.buf.set_lines(0, 1, 2, false, { '    a', '    a2' })
         btv.buf.set_lines(0, 5, 6, false, { '    c', '    c2', '    c3' })",
    )
    .await;
    let _ = lines(&rpc).await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let text = lines(&rpc).await;
    let edited = visible(&frame(&rpc, &mut inc).await);
    let fresh = fold_fresh(&format!("{}\n", text.join("\n")), INDENT).await;
    assert_eq!(edited, fresh, "multi-edit batch, text: {text:?}");
}

#[tokio::test]
async fn changing_tabstop_re_derives_the_indent_levels() {
    // `'tabstop'` decides what a leading tab is worth, so it changes the cached
    // per-line columns on text that did not move at all — the case a cache keyed on
    // `changedtick` alone would serve stale.
    let text = "top\n\tone\n\ttwo\n\t\tdeep\nend\n";
    let (rpc, mut inc) = start_with_file(text).await;
    command(&rpc, "set shiftwidth=4").await;
    command(&rpc, "set tabstop=8").await;
    command(&rpc, "set foldmethod=indent").await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let wide = visible(&frame(&rpc, &mut inc).await);

    command(&rpc, "set tabstop=2").await;
    feed(&rpc, "gg");
    let narrow = visible(&frame(&rpc, &mut inc).await);
    assert_ne!(
        wide, narrow,
        "changing tabstop must re-derive the indent levels ({wide:?})",
    );

    // …and it must match the same setting applied from scratch.
    let fresh = fold_fresh(
        text,
        &["set shiftwidth=4", "set tabstop=2", "set foldmethod=indent"],
    )
    .await;
    assert_eq!(narrow, fresh, "tabstop change vs a fresh fold");
}

#[tokio::test]
async fn indent_folds_survive_an_undo_and_an_edit_in_one_tick() {
    // The case the plain undo test cannot reach. `mark_resync` clears the fold
    // journal, so an undo on its own leaves nothing to splice and the rebuild happens
    // whether or not the resync flag is honoured. A bar-separated ex command runs
    // *both* in one tick, so the journal carries the post-undo edit while the cached
    // rows still describe the pre-undo text — and because this undo leaves the line
    // count unchanged, the length safety net cannot catch a bad splice either.
    let text = "func()\n    a\n    b\n    c\nqux\ntail\n";
    let (rpc, mut inc) = start_with_file(text).await;
    command(&rpc, "set foldmethod=indent").await;
    // Un-indent line 4, which *splits* the block's fold — a structural change, so a
    // stale cached row for it is visible rather than hidden inside an outer fold.
    command(&rpc, "4s/^    //").await;
    let _ = lines(&rpc).await;

    // Undo (line 4 is indented again) and edit line 2, in one tick.
    command(&rpc, "undo | 2s/a/A/").await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let text = lines(&rpc).await;
    let edited = visible(&frame(&rpc, &mut inc).await);
    let fresh = fold_fresh(&format!("{}\n", text.join("\n")), INDENT).await;
    assert_eq!(edited, fresh, "undo + edit in one tick, text: {text:?}");
}

// ------------------------------------------------------------------- marker

#[tokio::test]
async fn marker_folds_survive_typing_into_a_line() {
    assert_matches_fresh(&marked(), MARKER, "3GAxyz<Esc>", "typing into a line").await;
}

#[tokio::test]
async fn marker_folds_survive_adding_and_removing_a_marker() {
    // A marker line changes the running level for every line after it, so the rows
    // *downstream* of the edit must be re-derived from the cached tokens.
    assert_matches_fresh(&marked(), MARKER, "6Gcchere {{{<Esc>", "adding a marker").await;
    assert_matches_fresh(&marked(), MARKER, "2Gccplain<Esc>", "removing a marker").await;
}

#[tokio::test]
async fn marker_folds_survive_inserting_and_deleting_lines() {
    assert_matches_fresh(
        &marked(),
        MARKER,
        "1Goinner {{{<CR>body<CR>done }}}<Esc>",
        "inserting a marked block",
    )
    .await;
    assert_matches_fresh(&marked(), MARKER, "2G3dd", "deleting across a marker").await;
}

#[tokio::test]
async fn marker_folds_survive_undo() {
    assert_matches_fresh(&marked(), MARKER, "1Gonew {{{<Esc>u", "undo").await;
}

#[tokio::test]
async fn changing_foldmarker_re_derives_the_markers() {
    let text = "head\nblock <<<\n  one\nend >>>\ntail\n";
    let (rpc, mut inc) = start_with_file(text).await;
    command(&rpc, "set foldmethod=marker").await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let default_markers = visible(&frame(&rpc, &mut inc).await);
    assert_eq!(
        default_markers.len(),
        5,
        "`<<<`/`>>>` are not the default markers, so nothing folds yet",
    );

    command(&rpc, "set foldmarker=<<<,>>>").await;
    feed(&rpc, "gg");
    let custom = visible(&frame(&rpc, &mut inc).await);
    let fresh = fold_fresh(text, &["set foldmethod=marker", "set foldmarker=<<<,>>>"]).await;
    assert_eq!(
        custom, fresh,
        "changing foldmarker must re-derive the markers on unchanged text",
    );
    assert_ne!(
        custom, default_markers,
        "the new markers must actually fold"
    );
}

// --------------------------------------------------------------- Lua foldexpr

/// A foldexpr that folds by a marker word in the line's own text, so its value is a
/// genuine per-line function of content the test can move around.
/// A generic `'foldexpr'` is a sandbox expression over `line` and `lnum`. It
/// needs no buffer access — the row's own text is passed in — which is what lets
/// it be evaluated synchronously, in the frame the edit landed in.
const EXPR_SETUP: &str = r#"line:find('OPEN') and '>1' or line:find('SHUT') and '<1' or '='"#;

/// Set the focused buffer's `'foldexpr'`. Through `btv.bo` rather than `:set`,
/// since an expression contains spaces.
async fn set_foldexpr(rpc: &Rpc, expr: &str) {
    exec_lua(rpc, &format!("btv.bo.foldexpr = {expr:?} return true")).await;
}

async fn expr_fixture(text: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, inc) = start_with_file(text).await;
    set_foldexpr(&rpc, EXPR_SETUP).await;
    command(&rpc, "set foldmethod=expr").await;
    (rpc, inc)
}

/// The fold structure `text` produces under the foldexpr, computed from scratch.
async fn expr_fresh(text: &str) -> Vec<u64> {
    let (rpc, mut inc) = expr_fixture(text).await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    visible(&frame(&rpc, &mut inc).await)
}

async fn assert_expr_matches_fresh(text: &str, keys: &str, what: &str) {
    let (rpc, mut inc) = expr_fixture(text).await;
    feed(&rpc, keys);
    let _ = lines(&rpc).await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let final_text = lines(&rpc).await;
    let edited = visible(&frame(&rpc, &mut inc).await);
    let fresh = expr_fresh(&format!("{}\n", final_text.join("\n"))).await;
    assert_eq!(
        edited, fresh,
        "{what}: foldexpr folds after editing disagree with the same text folded \
         fresh\ntext: {final_text:?}",
    );
}

fn expr_text() -> String {
    "top\nOPEN here\nbody one\nbody two\nSHUT\nmiddle\nOPEN again\nmore\nSHUT\nend\n".to_string()
}

#[tokio::test]
async fn foldexpr_folds_survive_typing_into_a_line() {
    assert_expr_matches_fresh(&expr_text(), "3GAxyz<Esc>", "typing into a line").await;
}

#[tokio::test]
async fn foldexpr_folds_survive_a_line_becoming_a_fold_start() {
    // The edited row's *own* value flips, which is exactly the row the splice marks
    // for re-evaluation. If it were left with its old value the fold would not move.
    assert_expr_matches_fresh(&expr_text(), "6GccOPEN new<Esc>", "a line becoming OPEN").await;
    assert_expr_matches_fresh(
        &expr_text(),
        "2Gccplain now<Esc>",
        "a line stopping being OPEN",
    )
    .await;
}

#[tokio::test]
async fn foldexpr_folds_survive_inserting_lines() {
    assert_expr_matches_fresh(
        &expr_text(),
        "1GoOPEN inserted<CR>filler<CR>SHUT<Esc>",
        "inserting a folded block",
    )
    .await;
}

#[tokio::test]
async fn foldexpr_folds_survive_deleting_lines() {
    assert_expr_matches_fresh(&expr_text(), "2G3dd", "deleting across a fold start").await;
}

#[tokio::test]
async fn foldexpr_folds_survive_undo() {
    assert_expr_matches_fresh(&expr_text(), "1GoOPEN new<Esc>u", "undo").await;
}

#[tokio::test]
async fn changing_the_foldexpr_re_evaluates_every_line() {
    // A different expression is a different derivation: the cached values belong to
    // the old one and must be discarded even though no text changed.
    let (rpc, mut inc) = expr_fixture(&expr_text()).await;
    command(&rpc, "set foldlevel=0").await;
    feed(&rpc, "gg");
    let by_open = visible(&frame(&rpc, &mut inc).await);

    set_foldexpr(
        &rpc,
        r#"line:find('middle') and '>1' or line:find('end') and '<1' or '='"#,
    )
    .await;
    feed(&rpc, "gg");
    let by_middle = visible(&frame(&rpc, &mut inc).await);
    assert_ne!(
        by_open, by_middle,
        "swapping the foldexpr must re-evaluate the buffer, got {by_middle:?}",
    );
}
