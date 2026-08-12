//! Tier-1 tests for the picker row fitter — the pure text layout every client runs
//! per visible row. Black-box, no server: a label plus the two-column `layout` the
//! server projects (`[head, match start, match end]`), asserted on the fitted string
//! and the remapped highlight spans.
//!
//! The shape under test is live_grep's: a `path:line:col: ` head followed by the
//! matched line. The head must survive a long body (it keeps at least 40% of the
//! row), the body must window around the match rather than showing the line's head,
//! and the match must come back as a highlight span pointing at the right chars.

use bemtvi_view::{elide_keep_tail, fit_row, row_head_col};

/// The chars of `out` covered by `spans` — what the client bolds.
fn highlighted(out: &str, spans: &[(u16, u16)]) -> String {
    let chars: Vec<char> = out.chars().collect();
    let mut s = String::new();
    for &(a, b) in spans {
        s.extend(&chars[a as usize..(b as usize).min(chars.len())]);
    }
    s
}

/// A live_grep-shaped row: `(label, layout)` for `path`, 1-based `line`, and a body
/// whose `needle` is at char offset `at`.
fn grep_row(path: &str, line: usize, body: &str, needle: &str) -> (String, (usize, usize, usize)) {
    let head = format!("{path}:{line}:1: ");
    let at = body.find(needle).expect("needle in body");
    let start = head.chars().count() + body[..at].chars().count();
    (
        format!("{head}{body}"),
        (head.chars().count(), start, start + needle.chars().count()),
    )
}

#[test]
fn no_layout_is_plain_tail_priority_elision() {
    // A row without a layout must fit exactly as it always did — the whole point of
    // the layout being opt-in per row.
    let label = "crates/bemtvi-server/src/redraw.rs";
    let spans = [(8u16, 13u16)];
    assert_eq!(
        fit_row(label, &spans, 20, None, 8),
        elide_keep_tail(label, &spans, 20)
    );
}

#[test]
fn a_long_line_never_squeezes_out_the_file_name() {
    // The bug: a hit on a long line filled the row with the line, and the row showed
    // the matched text and nothing else. The head column is floored at 40%, so the
    // file name and line number survive however long the body is.
    let body = format!("{} zqneedle tail", "x".repeat(300));
    let (label, layout) = grep_row(
        "crates/bemtvi-core/src/editor/menu.rs",
        1764,
        &body,
        "zqneedle",
    );
    let width = 100;
    let head_col = row_head_col(layout.0, width);
    assert_eq!(head_col, 40, "the head keeps 40% of a 100-col row");

    let (out, spans) = fit_row(&label, &[], width, Some(layout), head_col);
    assert_eq!(out.chars().count(), width, "the row fills its width");
    let head_out: String = out.chars().take(head_col).collect();
    assert!(
        head_out.starts_with('…') && head_out.trim_end().ends_with("menu.rs:1764:1:"),
        "the head elides to its tail, keeping the file name and the line: {head_out:?}"
    );
    assert!(
        out.contains("zqneedle"),
        "the body windows around the match instead of showing the line's head: {out:?}"
    );
    assert_eq!(
        highlighted(&out, &spans),
        "zqneedle",
        "the source's own match highlights: {out:?} {spans:?}"
    );
}

#[test]
fn a_short_head_takes_only_what_it_needs_and_aligns_the_bodies() {
    // Nothing is padded out to 40% for its own sake: when every head is short the
    // column shrinks to the widest one, and every body starts in that same column.
    let (a_label, a_layout) = grep_row("a.txt", 1, "alpha match here", "match");
    let (b_label, b_layout) = grep_row("bbbbbb.txt", 22, "beta match here", "match");
    let width = 60;
    let head_col = row_head_col(a_layout.0.max(b_layout.0), width);
    assert_eq!(head_col, b_layout.0, "the widest head sets the column");

    let (a_out, _) = fit_row(&a_label, &[], width, Some(a_layout), head_col);
    let (b_out, _) = fit_row(&b_label, &[], width, Some(b_layout), head_col);
    assert_eq!(
        a_out.char_indices().nth(head_col).map(|(i, _)| &a_out[i..]),
        Some("alpha match here"),
        "the shorter head is padded to the column: {a_out:?}"
    );
    assert_eq!(
        b_out.char_indices().nth(head_col).map(|(i, _)| &b_out[i..]),
        Some("beta match here"),
        "the widest head needs no padding: {b_out:?}"
    );
}

#[test]
fn the_body_window_slides_to_keep_the_match_visible() {
    // A match 200 columns in is off the right edge of any row; the body scrolls to it,
    // keeping a little leading context behind a `…` so the line still reads.
    let body = format!("{}zqneedle{}", "a".repeat(200), "b".repeat(200));
    let (label, layout) = grep_row("f.txt", 3, &body, "zqneedle");
    let width = 80;
    let head_col = row_head_col(layout.0, width);

    let (out, spans) = fit_row(&label, &[], width, Some(layout), head_col);
    assert_eq!(out.chars().count(), width);
    assert!(
        out.contains("…aaa"),
        "the elided body keeps leading context behind a `…`: {out:?}"
    );
    assert_eq!(highlighted(&out, &spans), "zqneedle");
    // The window keeps context on BOTH sides, not just the match flush left.
    let body_out: String = out.chars().skip(head_col).collect();
    let at = body_out.find("zqneedle").expect("match on screen");
    assert!(
        at > 1 && body_out[at..].len() > "zqneedle".len(),
        "the match sits inside the window, not at its edge: {body_out:?}"
    );
}

#[test]
fn a_hit_at_the_end_of_the_line_fills_the_column() {
    // Sliding the window back for leading context must not run past the line's end —
    // a match on the last chars still fills the body column.
    let body = format!("{}zqneedle", "a".repeat(300));
    let (label, layout) = grep_row("f.txt", 3, &body, "zqneedle");
    let width = 80;
    let head_col = row_head_col(layout.0, width);

    let (out, spans) = fit_row(&label, &[], width, Some(layout), head_col);
    assert_eq!(out.chars().count(), width, "no short row at the line's end");
    assert!(out.ends_with("zqneedle"), "the tail is on screen: {out:?}");
    assert_eq!(highlighted(&out, &spans), "zqneedle");
}

#[test]
fn a_row_that_fits_is_shown_whole() {
    let (label, layout) = grep_row("f.txt", 3, "short zqneedle line", "zqneedle");
    let head_col = row_head_col(layout.0, 100);
    let (out, spans) = fit_row(&label, &[], 100, Some(layout), head_col);
    assert_eq!(out.trim_end(), label, "nothing is elided: {out:?}");
    assert_eq!(highlighted(&out, &spans), "zqneedle");
}

#[test]
fn fuzzy_spans_survive_the_two_column_fit() {
    // A source can carry BOTH matcher spans (over the whole label) and its own match.
    // Spans straddling the head/body split come back as two ranges over the fitted
    // string, so the highlight still lands on the same characters.
    let (label, layout) = grep_row("dir/f.txt", 3, "zqneedle body", "zqneedle");
    let width = 40;
    let head_col = row_head_col(layout.0, width);
    // The `f.txt` of the head plus the first body char.
    let head_hit = (4u16, 9u16);
    let (out, spans) = fit_row(&label, &[head_hit], width, Some(layout), head_col);
    assert!(
        highlighted(&out, &spans).contains("f.txt"),
        "the fuzzy span still points at `f.txt`: {out:?} {spans:?}"
    );
    assert!(
        highlighted(&out, &spans).contains("zqneedle"),
        "and the declared match is highlighted too: {out:?} {spans:?}"
    );
}

#[test]
fn a_narrow_row_still_shows_the_match() {
    // Even squeezed to a few cells the fit is well-defined (no panic, no overflow):
    // the head takes its 40% floor, the body window keeps what's left.
    for width in 1..24 {
        let body = format!("{}zqneedle{}", "a".repeat(40), "b".repeat(40));
        let (label, layout) = grep_row("some/dir/file.txt", 12, &body, "zqneedle");
        let head_col = row_head_col(layout.0, width);
        let (out, spans) = fit_row(&label, &[], width, Some(layout), head_col);
        assert!(
            out.chars().count() <= width,
            "width {width} overflowed: {out:?}"
        );
        for &(a, b) in &spans {
            assert!(
                a <= b && b as usize <= out.chars().count(),
                "span {a}..{b} out of range for {out:?}"
            );
        }
    }
}

#[test]
fn a_plain_row_keeps_its_head_behind_a_trailing_ellipsis() {
    // A completion candidate too long for its column: the head — the part you scan and
    // the part the matcher hit — survives, and the cut is marked. Regression: a plain
    // (non-path) label came back whole and each client silently head-cut it, so a
    // truncated candidate looked like a genuinely shorter word butted against the kind.
    let label = "ab_this_is_a_really_long_completion_candidate_name";
    let (out, spans) = fit_row(label, &[(0, 6)], 20, None, 0);
    assert_eq!(out, "ab_this_is_a_really…");
    assert_eq!(
        out.chars().count(),
        20,
        "the fit never overflows its column"
    );
    assert_eq!(
        highlighted(&out, &spans),
        "ab_thi",
        "a surviving span still points at the same chars"
    );
    // A span wholly past the cut vanishes rather than pointing into the `…`.
    let (_, spans) = fit_row(label, &[(30, 40)], 20, None, 0);
    assert!(spans.is_empty(), "a dropped span is gone: {spans:?}");
    // A span straddling the cut is clipped to what is still shown.
    let (_, spans) = fit_row(label, &[(15, 40)], 20, None, 0);
    assert_eq!(spans, vec![(15, 19)]);
}

#[test]
fn a_plain_row_that_fits_is_never_marked() {
    let label = "abshort";
    assert_eq!(fit_row(label, &[], 20, None, 0).0, label);
    // Exactly filling the column is not an overflow — no `…` is spent on it.
    assert_eq!(fit_row(label, &[], 7, None, 0).0, label);
    assert_eq!(fit_row(label, &[], 6, None, 0).0, "absho…");
}

#[test]
fn a_path_row_still_keeps_its_tail() {
    // The head-priority cut is for plain labels only: a path row keeps the file name.
    let (out, _) = fit_row("crates/bemtvi-server/src/redraw.rs", &[], 20, None, 0);
    assert!(
        out.starts_with('…') && out.ends_with("redraw.rs"),
        "{out:?}"
    );
}
