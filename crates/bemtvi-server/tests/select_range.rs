//! Black-box tests for **Select mode** and its Lua primitive `btv.win.select_range`
//! (P6 of the snippet-engine primitives spec). Select mode highlights a byte range
//! like a charwise Visual selection, but the next printable / `<CR>` / `<BS>`
//! **replaces** it (delete the range + enter Insert with that input) — vim's
//! `v_CTRL-G`, the "type over the placeholder default" behavior a snippet engine
//! wants when it jumps onto `${1:default}`. `<Esc>` keeps the selected text and
//! parks the caret in Insert past it.
//!
//! These drive the primitive directly (a plugin would call `btv.win.select_range`
//! from its own tabstop-jump logic): seed a buffer, select a sub-line range from
//! `exec_lua`, then feed keys and assert the buffer / mode / cursor.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, exec_lua, feed, lines, mode, spawn, temp_dir, wait_redraw, window0_field,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir("select_range");
    let init = ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Seed a single-line buffer and leave the cursor at (0,0) in Normal mode.
async fn seed(rpc: &Rpc, text: &str) {
    feed(rpc, "i");
    feed(rpc, text);
    feed(rpc, "<Esc>gg0");
    // Round-trip so the insert has landed server-side before the next step.
    let _ = lines(rpc).await;
}

/// Enter Select mode over the 0-based, end-exclusive byte range on row 0.
async fn select(rpc: &Rpc, s_col: usize, e_col: usize) {
    exec_lua(
        rpc,
        &format!("btv.win.select_range(0, 0, {s_col}, 0, {e_col})"),
    )
    .await;
}

#[tokio::test]
async fn select_range_enters_select_mode_and_highlights_the_range() {
    let (rpc, mut inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    assert_eq!(
        mode(&rpc).await,
        "s",
        "select_range should enter Select mode"
    );

    // The rendered selection covers exactly the requested range on row 0. The
    // charwise Visual projection is inclusive of the last char, so bytes 6..11
    // paint as the half-open screen span [6, 11).
    let map = wait_redraw(&mut inc, |m| {
        window0_field(m, "selection")
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .is_some_and(|first| !matches!(first, Value::Nil))
    })
    .await;
    let selection = window0_field(&map, "selection")
        .and_then(Value::as_array)
        .expect("selection spans");
    let row0 = selection[0].as_array().expect("row 0 selection span");
    let (start, end) = (row0[0].as_u64().unwrap(), row0[1].as_u64().unwrap());
    assert_eq!((start, end), (6, 11), "the whole word should be selected");
}

#[tokio::test]
async fn a_printable_replaces_the_selection_and_enters_insert() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    feed(&rpc, "X");
    // The default is replaced (not inserted before it), and we are now in Insert.
    assert_eq!(lines(&rpc).await, vec!["hello X"]);
    assert_eq!(mode(&rpc).await, "i");

    // Further typing continues in Insert at the replacement site.
    feed(&rpc, "Y");
    assert_eq!(lines(&rpc).await, vec!["hello XY"]);
}

#[tokio::test]
async fn esc_defaults_to_keeping_the_default_and_dropping_to_normal() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    feed(&rpc, "<Esc>");
    // Default `on_escape = "normal"` (vim's `v_CTRL-G`): the text is kept and we drop
    // to Normal with the caret on the selection head (the last selected char, "d").
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
    assert_eq!(mode(&rpc).await, "n");

    feed(&rpc, "x"); // Normal-mode delete of the char under the cursor
    assert_eq!(lines(&rpc).await, vec!["hello worl"]);
}

#[tokio::test]
async fn on_escape_insert_keeps_the_default_and_parks_in_insert_past_it() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    // Opt into the snippet-friendly Escape: keep the default, park in Insert past it.
    exec_lua(
        &rpc,
        "btv.win.select_range(0, 0, 6, 0, 11, { on_escape = 'insert' })",
    )
    .await;
    assert_eq!(mode(&rpc).await, "s");

    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
    assert_eq!(mode(&rpc).await, "i");

    feed(&rpc, "!"); // appends right after the kept "world"
    assert_eq!(lines(&rpc).await, vec!["hello world!"]);
}

#[tokio::test]
async fn cr_replaces_the_selection_with_a_newline() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    feed(&rpc, "<CR>");
    assert_eq!(lines(&rpc).await, vec!["hello ", ""]);
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn backspace_deletes_the_selection_and_enters_insert() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    feed(&rpc, "<BS>");
    // The whole selection is removed and no character is typed in its place.
    assert_eq!(lines(&rpc).await, vec!["hello "]);
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn the_replace_is_one_undo_step() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    feed(&rpc, "X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello X"]);

    feed(&rpc, "u");
    // The delete-of-default + insert-of-X undo together, back to the original.
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn an_empty_range_degrades_to_insert_at_the_start() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    // An empty range (an empty tabstop): nothing to select, so the caret parks in
    // Insert at the start rather than entering Select.
    select(&rpc, 6, 6).await;

    assert_eq!(mode(&rpc).await, "i");
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, vec!["hello Zworld"]);
}

#[tokio::test]
async fn a_non_printable_key_leaves_select_for_normal() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    select(&rpc, 6, 11).await; // "world"

    // A printable char replaces (vim's rule — even `x`/`w` type over the selection),
    // so the fallback-to-Normal path is for *non-printable* keys. An arrow ends
    // Select without touching the buffer and is handled as an ordinary Normal motion.
    feed(&rpc, "<Right>");
    assert_eq!(mode(&rpc).await, "n");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);

    // Confirm we really are in plain Normal mode now: `x` deletes a single char.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["hello worl"]);
}

// ===== keyboard entry (gh / gH / <C-g>) =====================================

#[tokio::test]
async fn gh_enters_charwise_select_and_typing_replaces() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;

    // `gh` starts Select with a 1-wide selection at the cursor (like `v` but Select).
    feed(&rpc, "0"); // cursor on "h"
    feed(&rpc, "gh");
    assert_eq!(mode(&rpc).await, "s");
    // A printable replaces the selection and enters Insert. (Motions like `e` do NOT
    // extend a Select selection — they are printables, so they replace too, matching
    // vim; you extend a Select with shifted keys / arrows, or in Visual then toggle.)
    feed(&rpc, "Z");
    assert_eq!(lines(&rpc).await, vec!["Zello world"]);
    assert_eq!(mode(&rpc).await, "i");
}

#[tokio::test]
async fn gh_escape_defaults_to_normal() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;
    feed(&rpc, "0gh");
    assert_eq!(mode(&rpc).await, "s");
    // Keyboard-entered Select uses the vim-faithful default: <Esc> → Normal, text kept.
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "n");
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
}

#[tokio::test]
async fn capital_gh_enters_linewise_select_and_replaces_the_line() {
    let (rpc, _inc) = start().await;
    // Two lines so a linewise replace is visible.
    feed(&rpc, "iline one<CR>line two<Esc>gg");
    let _ = lines(&rpc).await;

    feed(&rpc, "gH");
    // Linewise Select reports vim's capital `S`.
    assert_eq!(mode(&rpc).await, "S");
    // Typing replaces the whole selected line with a fresh line (like `S`/`cc`).
    feed(&rpc, "new<Esc>");
    assert_eq!(lines(&rpc).await, vec!["new", "line two"]);
}

#[tokio::test]
async fn ctrl_g_toggles_visual_to_select_and_back() {
    let (rpc, _inc) = start().await;
    seed(&rpc, "hello world").await;

    feed(&rpc, "0v"); // Visual, cursor on "h"
    assert_eq!(mode(&rpc).await, "v");
    feed(&rpc, "<C-g>"); // toggle to Select
    assert_eq!(mode(&rpc).await, "s");
    feed(&rpc, "<C-g>"); // toggle back to Visual
    assert_eq!(mode(&rpc).await, "v");

    // From Visual-Line, <C-g> toggles to *linewise* Select (reported `S`).
    feed(&rpc, "<Esc>V");
    assert_eq!(mode(&rpc).await, "V");
    feed(&rpc, "<C-g>");
    assert_eq!(mode(&rpc).await, "S");
}
