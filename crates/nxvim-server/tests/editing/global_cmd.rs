use crate::support::*;

// ----- :global / :vglobal (and the :delete / :print they drive) -----

#[tokio::test]
async fn ex_delete_removes_the_range_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<CR>ddd<Esc>");
    feed(&rpc, ":2,3d<CR>");
    assert_eq!(lines(&rpc).await, vec!["aaa", "ddd"]);
}

#[tokio::test]
async fn ex_delete_bare_removes_the_current_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg");
    feed(&rpc, ":d<CR>"); // current line (1) only
    assert_eq!(lines(&rpc).await, vec!["bbb", "ccc"]);
}

#[tokio::test]
async fn global_deletes_every_matching_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ikeep<CR>drop me<CR>keep<CR>drop<CR>keep<Esc>");
    feed(&rpc, ":g/drop/d<CR>");
    assert_eq!(lines(&rpc).await, vec!["keep", "keep", "keep"]);
}

#[tokio::test]
async fn vglobal_deletes_every_non_matching_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ikeep<CR>drop me<CR>keep<CR>drop<CR>x<Esc>");
    feed(&rpc, ":v/drop/d<CR>"); // delete lines NOT matching "drop"
    assert_eq!(lines(&rpc).await, vec!["drop me", "drop"]);
}

#[tokio::test]
async fn global_bang_is_vglobal() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ikeep<CR>drop<CR>keep<Esc>");
    feed(&rpc, ":g!/drop/d<CR>"); // == :v/drop/d
    assert_eq!(lines(&rpc).await, vec!["drop"]);
}

#[tokio::test]
async fn global_runs_substitute_on_matching_lines_only() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo 1<CR>bar 1<CR>foo 1<Esc>");
    feed(&rpc, ":g/foo/s/1/X/<CR>"); // substitute only on the "foo" lines
    assert_eq!(
        lines(&rpc).await,
        vec!["foo X", "bar 1", "foo X"],
        "the bar line is skipped even though it also contains 1"
    );
}

#[tokio::test]
async fn global_default_range_is_the_whole_file() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "idrop<CR>a<CR>drop<Esc>G"); // cursor on the last line
    feed(&rpc, ":g/drop/d<CR>"); // no range → whole file, not the current line
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn global_range_limits_the_scan() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ix<CR>x<CR>x<CR>x<Esc>");
    feed(&rpc, ":2,3g/x/d<CR>"); // only lines 2..3 are eligible
    assert_eq!(lines(&rpc).await, vec!["x", "x"]);
}

#[tokio::test]
async fn global_is_a_single_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ix<CR>y<CR>x<CR>y<CR>x<Esc>");
    feed(&rpc, ":g/x/d<CR>");
    assert_eq!(lines(&rpc).await, vec!["y", "y"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["x", "y", "x", "y", "x"],
        "one u restores every :g/x/d deletion"
    );
}

#[tokio::test]
async fn global_empty_pattern_reuses_last_search() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>foo<Esc>");
    feed(&rpc, "/foo<CR>"); // sets the last search pattern
    feed(&rpc, ":g//d<CR>"); // empty pattern → reuse "foo"
    assert_eq!(lines(&rpc).await, vec!["bar"]);
}

#[tokio::test]
async fn global_prints_matching_lines_when_no_command() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<CR>beta<CR>also alpha<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":g/alpha/<CR>").await; // default cmd = print
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha", "beta", "also alpha"],
        "print changes nothing"
    );
    // The last printed line shows on the message line.
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("also alpha")
    );
}

#[tokio::test]
async fn global_nested_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ix<CR>y<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":g/x/g/y/d<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["x", "y"], "nothing deleted");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E147"), "expected E147, got {msg:?}");
}

#[tokio::test]
async fn global_no_match_reports_e486() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":g/zzz/d<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo", "bar"], "buffer untouched");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: zzz")
    );
}

// ----- the `:bar` exception list accepts the dispatcher's abbreviations -----

/// Baseline: with the full `:global` spelling, a bar inside the argument chains
/// sub-commands per matched line — it is NOT a command separator.
#[tokio::test]
async fn global_full_name_keeps_the_bar() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>");
    // For each of a,b: append `-m`, then prepend `+`. A mis-split would run the
    // trailing `:s/^/+/` once, on the cursor line, instead of per matched line.
    feed(&rpc, ":global/[ab]/s/$/-m/|s/^/+/<CR>");
    assert_eq!(lines(&rpc).await, vec!["+a-m", "+b-m", "c"]);
}

/// `ex_takes_bar` must accept the same abbreviations the dispatcher does: `:glo`
/// is `:global`, so its bar belongs to the sub-command chain exactly as the full
/// spelling's does. (The list once held only `g`/`global`, so every intermediate
/// spelling mis-split at the bar — the pattern was truncated and the tail ran as
/// its own ex-command.)
#[tokio::test]
async fn global_abbreviation_keeps_the_bar() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>");
    feed(&rpc, ":glo/[ab]/s/$/-m/|s/^/+/<CR>");
    assert_eq!(lines(&rpc).await, vec!["+a-m", "+b-m", "c"]);
}

/// Same drift, `:normal` family: `:norma iX|Y` types `X|Y` — the bar is part of
/// the keystroke string for every accepted spelling, not just `norm`/`normal`.
#[tokio::test]
async fn normal_abbreviation_keeps_the_bar() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iab<Esc>0");
    feed(&rpc, ":norma iX|Y<CR>");
    assert_eq!(lines(&rpc).await, vec!["X|Yab"]);
}
