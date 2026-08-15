//! `'report'` — the line-count / yank feedback vim prints on the message line
//! when one command changes (or yanks) more than `'report'` lines. Default `2`,
//! so `5dd` says `5 fewer lines` but `2dd` stays quiet.
//!
//! Driven black-box: feed keys, read the message off the resulting `redraw`.

use crate::support::*;

/// A fresh server on a temp file of `n` numbered lines (`line 1` … `line n`).
async fn start_n(tag: &str, n: usize) -> (Rpc, UnboundedReceiver<Incoming>) {
    let body: String = (1..=n).map(|i| format!("line {i}\n")).collect();
    let path = temp_path(tag).to_string_lossy().into_owned();
    std::fs::write(&path, body).expect("write temp file");
    start(Some(path)).await
}

// ===== delete ================================================================

#[tokio::test]
async fn deleting_more_lines_than_report_says_how_many_fewer() {
    let (rpc, mut i) = start_n("rep_dd5", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "5dd").await, "5 fewer lines");
}

#[tokio::test]
async fn deleting_at_most_report_lines_stays_silent() {
    // 'report' is 2 and vim reports only when the count is strictly greater, so a
    // 2-line delete says nothing.
    let (rpc, mut i) = start_n("rep_dd2", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "2dd").await, "");
}

#[tokio::test]
async fn report_zero_reports_a_single_deleted_line_in_the_singular() {
    // vim's asymmetric wording: "1 line less" singular, "N fewer lines" plural.
    let (rpc, mut i) = start_n("rep_zero", 10).await;
    feed(&rpc, ":set report=0<CR>");
    lines(&rpc).await; // land the option before the delete
    assert_eq!(message_after(&rpc, &mut i, "dd").await, "1 line less");
}

#[tokio::test]
async fn visual_line_delete_reports_the_selection_size() {
    let (rpc, mut i) = start_n("rep_vdel", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "V3jd").await, "4 fewer lines");
}

#[tokio::test]
async fn ex_delete_range_reports_the_deleted_lines() {
    let (rpc, mut i) = start_n("rep_exdel", 10).await;
    assert_eq!(
        message_after(&rpc, &mut i, ":2,7d<CR>").await,
        "6 fewer lines"
    );
}

#[tokio::test]
async fn charwise_delete_reports_only_the_lines_it_removed() {
    // A charwise selection spanning 5 lines but ending on line 5's first column
    // leaves that line behind: the buffer loses 4 lines, and `'report'` counts what
    // the buffer lost, not what the selection spanned.
    let (rpc, mut i) = start_n("rep_charwise", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "v4jd").await, "4 fewer lines");
}

#[tokio::test]
async fn change_does_not_report() {
    // vim computes the message for `c` and then paints `-- INSERT --` over it, so
    // the user never sees one. bemtvi has no insert-mode message line to clobber
    // it, so it must not echo one at all.
    let (rpc, mut i) = start_n("rep_change", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "5cc").await, "");
}

// ===== yank ==================================================================

#[tokio::test]
async fn yanking_more_lines_than_report_says_how_many() {
    let (rpc, mut i) = start_n("rep_yy", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "6yy").await, "6 lines yanked");
}

#[tokio::test]
async fn yanking_at_most_report_lines_stays_silent() {
    let (rpc, mut i) = start_n("rep_yy2", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "2yy").await, "");
}

#[tokio::test]
async fn a_named_register_is_part_of_the_yank_message() {
    let (rpc, mut i) = start_n("rep_yreg", 10).await;
    assert_eq!(
        message_after(&rpc, &mut i, "\"a6yy").await,
        "6 lines yanked into \"a"
    );
}

#[tokio::test]
async fn a_charwise_yank_inside_one_line_never_reports() {
    // vim counts a single-line charwise yank as zero lines, so even `:set report=0`
    // leaves it silent.
    let (rpc, mut i) = start_n("rep_ycharwise", 10).await;
    feed(&rpc, ":set report=0<CR>");
    lines(&rpc).await;
    assert_eq!(message_after(&rpc, &mut i, "y$").await, "");
}

#[tokio::test]
async fn a_multi_line_charwise_yank_reports_its_lines() {
    let (rpc, mut i) = start_n("rep_ymulti", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "y5j").await, "6 lines yanked");
}

// ===== put ===================================================================

#[tokio::test]
async fn pasting_more_lines_than_report_says_how_many_more() {
    let (rpc, mut i) = start_n("rep_put", 10).await;
    feed(&rpc, "4yy");
    lines(&rpc).await;
    assert_eq!(message_after(&rpc, &mut i, "p").await, "4 more lines");
}

#[tokio::test]
async fn a_counted_paste_reports_the_total_lines_added() {
    let (rpc, mut i) = start_n("rep_put_count", 10).await;
    feed(&rpc, "2yy");
    lines(&rpc).await;
    assert_eq!(message_after(&rpc, &mut i, "3p").await, "6 more lines");
}

#[tokio::test]
async fn a_single_line_paste_stays_silent() {
    let (rpc, mut i) = start_n("rep_put1", 10).await;
    feed(&rpc, "yy");
    lines(&rpc).await;
    assert_eq!(message_after(&rpc, &mut i, "p").await, "");
}

// ===== shift =================================================================

#[tokio::test]
async fn shifting_more_lines_than_report_names_the_operator() {
    let (rpc, mut i) = start_n("rep_shift", 10).await;
    assert_eq!(
        message_after(&rpc, &mut i, "5>>").await,
        "5 lines >ed 1 time"
    );
}

#[tokio::test]
async fn a_counted_visual_shift_reports_the_number_of_times() {
    let (rpc, mut i) = start_n("rep_shift_vis", 10).await;
    assert_eq!(
        message_after(&rpc, &mut i, "V4j3>").await,
        "5 lines >ed 3 times"
    );
}

#[tokio::test]
async fn shifting_at_most_report_lines_stays_silent() {
    let (rpc, mut i) = start_n("rep_shift_quiet", 10).await;
    assert_eq!(message_after(&rpc, &mut i, "2>>").await, "");
}

// ===== `:global` =============================================================

#[tokio::test]
async fn a_global_delete_reports_its_total_once() {
    // The per-line `:d` runs silent inside a `:g` pass; the whole run reports the
    // buffer's net change once, as vim does.
    let (rpc, mut i) = start_n("rep_gdel", 12).await;
    assert_eq!(
        message_after(&rpc, &mut i, ":g/1/d<CR>").await,
        "4 fewer lines"
    );
}

// ===== the option itself =====================================================

#[tokio::test]
async fn report_defaults_to_two_and_is_settable() {
    let (rpc, _i) = start_n("rep_opt", 10).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.report").await.as_i64(),
        Some(2)
    );
    feed(&rpc, ":set report=99<CR>");
    assert_eq!(
        exec_lua(&rpc, "return vim.o.report").await.as_i64(),
        Some(99)
    );
}

#[tokio::test]
async fn a_high_report_silences_everything() {
    let (rpc, mut i) = start_n("rep_high", 20).await;
    feed(&rpc, ":set report=99<CR>");
    lines(&rpc).await;
    assert_eq!(message_after(&rpc, &mut i, "10dd").await, "");
}
