//! Visual-mode `r{char}` — replace every character of the selection.
//!
//! Vim's `r` means two different things by mode. In Normal it replaces the char
//! under the cursor (`3r-` replaces three). In Visual it replaces the *whole
//! selection*, one `c` per selected character, then leaves Visual. bemtvi used to
//! run the Normal-mode branch in both, so `v$r-` replaced a single char and left
//! the selection's remainder untouched.
//!
//! The invariant that makes this more than a spelling difference: `r` never
//! changes line structure. Newlines inside a selection are NOT replaced, so a
//! three-line selection stays three lines — which is exactly what a `remove` +
//! `insert` of the flat range would get wrong. Every test here asserts the line
//! *count* as well as the content.

use crate::support::*;

/// A buffer seeded with `content`, no file behind it.
async fn buf(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    // `i<text><Esc>` rather than a file, so the fixture stays hermetic. The literal
    // newlines are fed as `<CR>` so insert-mode sees real line breaks.
    let typed: String = content.replace('\n', "<CR>");
    feed(&rpc, &format!("i{typed}<Esc>gg0"));
    (rpc, incoming)
}

#[tokio::test]
async fn charwise_visual_r_replaces_every_selected_char() {
    let (rpc, _i) = buf("hello world").await;
    // `v` + `4l` selects "hello" (5 chars, inclusive of the char under the cursor).
    feed(&rpc, "v4lr-");
    assert_eq!(
        lines(&rpc).await,
        vec!["----- world"],
        "all five selected chars become '-', the rest is untouched"
    );
}

#[tokio::test]
async fn visual_r_leaves_visual_mode() {
    let (rpc, _i) = buf("hello world").await;
    feed(&rpc, "v4lr-");
    assert_eq!(mode(&rpc).await, "n", "vim's visual `r` returns to Normal");
}

#[tokio::test]
async fn visual_r_puts_the_cursor_on_the_first_replaced_char() {
    let (rpc, _i) = buf("hello world").await;
    // Select forward from col 2 ("llo"), so the low end is *not* where the cursor
    // sits when `r` runs — the cursor must settle back on the selection's start.
    feed(&rpc, "llv2lr-");
    assert_eq!(lines(&rpc).await, vec!["he--- world"]);
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "cursor on the first replaced char"
    );
}

#[tokio::test]
async fn a_backward_selection_also_settles_on_the_low_end() {
    let (rpc, _i) = buf("hello world").await;
    // Start at col 5 and select *backwards* to col 1: vim places the cursor at the
    // selection's low end either way, not where the head happened to be.
    feed(&rpc, "5lv4hr-");
    assert_eq!(lines(&rpc).await, vec!["h-----world"]);
    assert_eq!(cursor(&rpc).await, (1, 1), "the low end, not the head");
}

#[tokio::test]
async fn a_multi_line_charwise_selection_keeps_its_newlines() {
    let (rpc, _i) = buf("abc\ndef\nghi").await;
    // From (1,1) through (3,1): the selection spans two newlines. `r` replaces the
    // *characters*, never the line breaks — three lines in, three lines out.
    feed(&rpc, "lvjjr-");
    assert_eq!(
        lines(&rpc).await,
        vec!["a--", "---", "--i"],
        "each line's selected span is replaced; the line structure survives"
    );
}

#[tokio::test]
async fn linewise_visual_r_replaces_whole_lines_without_joining_them() {
    let (rpc, _i) = buf("abc\ndef\nghi").await;
    feed(&rpc, "jVr-");
    assert_eq!(
        lines(&rpc).await,
        vec!["abc", "---", "ghi"],
        "V selects the whole line; its newline is still not replaced"
    );
}

#[tokio::test]
async fn linewise_visual_r_spans_every_selected_line() {
    let (rpc, _i) = buf("abc\ndeff\nghi").await;
    feed(&rpc, "Vjr*");
    assert_eq!(
        lines(&rpc).await,
        vec!["***", "****", "ghi"],
        "each line is replaced to its own length, not the first line's"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "linewise settles at the first line, col 0"
    );
}

#[tokio::test]
async fn an_empty_line_inside_a_linewise_selection_stays_empty() {
    let (rpc, _i) = buf("ab\n\ncd").await;
    feed(&rpc, "VGr-");
    assert_eq!(
        lines(&rpc).await,
        vec!["--", "", "--"],
        "a zero-length line has no chars to replace and must not gain one"
    );
}

#[tokio::test]
async fn the_replacement_is_counted_in_chars_not_bytes() {
    // "héllo" is 6 bytes but 5 chars. Replacing the selection must yield 5 `-`,
    // and a byte-counted implementation would emit 6.
    let (rpc, _i) = buf("héllo").await;
    feed(&rpc, "v$r-");
    assert_eq!(lines(&rpc).await, vec!["-----"]);
}

#[tokio::test]
async fn a_multibyte_replacement_char_is_written_once_per_selected_char() {
    // Three 1-byte chars replaced by a 2-byte one: the count follows the *chars*
    // replaced, not the bytes either side occupies.
    let (rpc, _i) = buf("abc").await;
    feed(&rpc, "v$ré");
    assert_eq!(lines(&rpc).await, vec!["ééé"]);
}

#[tokio::test]
async fn visual_r_is_one_undo_step() {
    let (rpc, _i) = buf("abc\ndef").await;
    // `vj` is charwise from (1,0) to (2,0) — all of line 1 plus line 2's first
    // char, which is what vim replaces here.
    feed(&rpc, "vjr-");
    assert_eq!(lines(&rpc).await, vec!["---", "-ef"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["abc", "def"],
        "a single `u` restores the whole selection, not one line of it"
    );
}

#[tokio::test]
async fn visual_r_marks_the_changed_region() {
    let (rpc, _i) = buf("hello world").await;
    feed(&rpc, "llv2lr-");
    // `` `[ `` / `` `] `` bracket the change, like every other operator.
    feed(&rpc, "`[");
    assert_eq!(cursor(&rpc).await, (1, 2), "`[ at the change start");
    feed(&rpc, "`]");
    assert_eq!(cursor(&rpc).await, (1, 4), "`] at the change end");
}

#[tokio::test]
async fn normal_mode_r_is_unaffected() {
    // The counterpart guard: routing Visual `r` away must not disturb the Normal
    // one, count and all.
    let (rpc, _i) = buf("hello").await;
    feed(&rpc, "3r-");
    assert_eq!(lines(&rpc).await, vec!["---lo"]);
}

#[tokio::test]
async fn visual_r_on_a_nomodifiable_buffer_is_refused() {
    let (rpc, mut incoming) = buf("hello").await;
    feed_sync(&rpc, ":set nomodifiable<CR>").await;
    let msg = message_after(&rpc, &mut incoming, "v$r-").await;
    assert!(
        msg.contains("E21"),
        "visual `r` must honour 'modifiable' like every other operator, got {msg:?}"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "the refused edit must leave the text alone"
    );
}
