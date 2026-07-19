use crate::support::*;

#[tokio::test]
async fn ex_range_absolute_line_jumps() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":3<CR>");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn ex_range_dollar_jumps_to_last_line() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":$<CR>");
    assert_eq!(cursor(&rpc).await, (5, 0));
}

#[tokio::test]
async fn ex_range_dot_offset_moves_relative() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":3<CR>"); // on line 3
    feed(&rpc, ":.+2<CR>"); // +2 -> line 5
    assert_eq!(cursor(&rpc).await, (5, 0));
    feed(&rpc, ":.-1<CR>"); // -1 -> line 4 (indented)
    assert_eq!(cursor(&rpc).await, (4, 4), "lands on first non-blank");
}

#[tokio::test]
async fn ex_range_bare_offset_is_relative_to_cursor() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":2<CR>"); // on line 2
    feed(&rpc, ":+2<CR>"); // a leading +/- offset is relative to the cursor
    assert_eq!(cursor(&rpc).await, (4, 4));
}

#[tokio::test]
async fn ex_range_pair_moves_to_last_address() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":2,4<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (4, 4),
        "a pair moves to the last address"
    );
}

#[tokio::test]
async fn ex_range_percent_moves_to_last_line() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":%<CR>");
    assert_eq!(cursor(&rpc).await, (5, 0));
}

#[tokio::test]
async fn ex_range_out_of_buffer_clamps() {
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":999<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (5, 0),
        "an over-large line clamps to last"
    );
}

#[tokio::test]
async fn ex_range_reversed_errors_loudly() {
    let (rpc, mut incoming) = range_fixture().await;
    // vim would prompt to swap; we can't prompt, so fail loud rather than
    // silently swap (the no-silent-errors rule).
    let map = redraw_after(&rpc, &mut incoming, ":3,1<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E493"),
        "expected E493 backwards-range error, got {msg:?}"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "cursor stays put on a bad range"
    );
}

#[tokio::test]
async fn ex_range_unknown_mark_errors_loudly() {
    let (rpc, mut incoming) = range_fixture().await;
    // An *unset* mark address must fail loud, not resolve to a bogus line.
    let map = redraw_after(&rpc, &mut incoming, ":'a<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E20"),
        "expected E20 mark-not-set error, got {msg:?}"
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "cursor stays put on a bad range"
    );
}

#[tokio::test]
async fn ex_range_visual_marks_delete_selection() {
    let (rpc, _incoming) = range_fixture().await;
    // Select lines 2–3 linewise, leave visual (stamping `'<` / `'>`), then the
    // canonical `:'<,'>d` deletes exactly those lines.
    feed(&rpc, "ggj");
    feed(&rpc, "Vj");
    feed(&rpc, "<Esc>");
    feed(&rpc, ":'<,'>d<CR>");
    assert_eq!(lines(&rpc).await, vec!["one", "    four", "five"]);
}

#[tokio::test]
async fn ex_range_buffer_local_marks_address_lines() {
    let (rpc, _incoming) = range_fixture().await;
    // Mark `a` on line 2 and `b` on line 4; `:'a,'bd` deletes that inclusive span.
    feed(&rpc, "ggjma");
    feed(&rpc, "jjmb");
    feed(&rpc, ":'a,'bd<CR>");
    assert_eq!(lines(&rpc).await, vec!["one", "five"]);
}

#[tokio::test]
async fn colon_in_visual_prefills_the_selection_range() {
    let (rpc, mut incoming) = range_fixture().await;
    // Select lines 2–3, then `:` — vim stamps `'<`/`'>` and prefills `'<,'>` so the
    // command line already reads ":'<,'>" with the cursor at the end.
    feed(&rpc, "ggj");
    feed(&rpc, "Vj");
    let map = redraw_after(&rpc, &mut incoming, ":").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("'<,'>"),
        "`:` in visual mode should prefill the selection range"
    );
    // Typing just `d` then <CR> deletes the selected lines — the real user flow.
    feed(&rpc, "d<CR>");
    assert_eq!(lines(&rpc).await, vec!["one", "    four", "five"]);
}

// ---- Phase 1: the :substitute command -----------------------------------
//
// Pattern + replacement are canonical regex (the dialect `/` search uses):
// `(\w+)` captures, `$1` back-refs, `\r` -> newline in the replacement.

#[tokio::test]
async fn substitute_replaces_first_match_on_current_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>");
    feed(&rpc, ":s/foo/baz<CR>"); // trailing delimiter optional
    assert_eq!(lines(&rpc).await, vec!["baz bar foo"], "first match only");
}

#[tokio::test]
async fn substitute_g_flag_replaces_every_match_on_the_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>");
    feed(&rpc, ":s/foo/baz/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["baz bar baz"]);
}

#[tokio::test]
async fn substitute_percent_range_spans_the_whole_buffer() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "bar"]);
}

#[tokio::test]
async fn substitute_line_range_limits_the_edit() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":1,2s/foo/bar<CR>");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "foo"]);
}

#[tokio::test]
async fn substitute_expands_capture_groups() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Canonical-regex groups `(\w+)`, PCRE-style `$1`/`$2` back-refs (the
    // documented divergence from vim's `\(\)` / `\1`).
    feed(&rpc, ":s/(\\w+) (\\w+)/$2 $1/<CR>");
    assert_eq!(lines(&rpc).await, vec!["world hello"]);
}

#[tokio::test]
async fn substitute_empty_replacement_deletes_the_match() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoobar<Esc>");
    feed(&rpc, ":s/o//g<CR>");
    assert_eq!(lines(&rpc).await, vec!["fbar"]);
}

#[tokio::test]
async fn substitute_carriage_return_splits_the_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia, b, c<Esc>");
    feed(&rpc, ":s/, /\\r/g<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "b", "c"],
        "\\r in the replacement splits one line into three"
    );
}

#[tokio::test]
async fn substitute_case_override_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iFOO foo<Esc>");
    feed(&rpc, ":s/foo/x/gi<CR>"); // i: ignore case -> both match
    assert_eq!(lines(&rpc).await, vec!["x x"]);
}

#[tokio::test]
async fn substitute_n_flag_counts_without_changing_the_buffer() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/x/gn<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo foo foo"],
        "n flag makes no edits"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("3 matches on 1 line")
    );
}

#[tokio::test]
async fn substitute_unknown_flag_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/bar/z<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo"], "no edit on a bad flag");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E488"),
        "expected trailing-chars error, got {msg:?}"
    );
}

#[tokio::test]
async fn substitute_no_match_reports_e486_and_keeps_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>gg0");
    let before = cursor(&rpc).await;
    let map = redraw_after(&rpc, &mut incoming, ":s/zzz/x/<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar"],
        "buffer untouched on a miss"
    );
    assert_eq!(cursor(&rpc).await, before, "cursor stays put on a miss");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: zzz")
    );
}

#[tokio::test]
async fn substitute_reports_count_message() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<CR>foo foo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":%s/foo/bar/g<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("6 substitutions on 3 lines")
    );
}

#[tokio::test]
async fn substitute_is_a_single_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "bar"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "foo", "foo"],
        "one u undoes the whole :%s"
    );
}

#[tokio::test]
async fn substitute_cursor_lands_on_last_changed_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>foo<Esc>gg");
    feed(&rpc, ":%s/foo/baz<CR>");
    // Cursor on the last line a substitution happened (line 3), first non-blank.
    assert_eq!(cursor(&rpc).await, (3, 0));
}

// ---- Phase 2: pattern reuse, repeat, count, delimiters ------------------
//
// Bare `:s` / `:&` / `:&&` repeat the last substitute; `~` recalls the last
// replacement; alternate delimiters and a trailing count round out the parser.

#[tokio::test]
async fn substitute_bare_s_repeats_last_resetting_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar/g<CR>"); // line 1: both replaced
    feed(&rpc, "2G");
    feed(&rpc, ":s<CR>"); // repeat on line 2 — flags reset, so first match only
    assert_eq!(lines(&rpc).await, vec!["bar bar", "bar foo"]);
}

#[tokio::test]
async fn substitute_bare_s_accepts_new_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar<CR>"); // line 1: first match only (no g)
    feed(&rpc, "2G");
    feed(&rpc, ":s g<CR>"); // repeat with a fresh g flag -> every match
    assert_eq!(lines(&rpc).await, vec!["bar foo", "bar bar"]);
}

#[tokio::test]
async fn substitute_ampersand_repeats_resetting_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar/g<CR>");
    feed(&rpc, "2G");
    feed(&rpc, ":&<CR>"); // `:&` repeats with flags reset, like bare `:s`
    assert_eq!(lines(&rpc).await, vec!["bar bar", "bar foo"]);
}

#[tokio::test]
async fn substitute_double_ampersand_keeps_flags() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo<CR>foo foo<Esc>");
    feed(&rpc, ":1s/foo/bar/g<CR>");
    feed(&rpc, "2G");
    feed(&rpc, ":&&<CR>"); // `:&&` keeps the previous flags (g)
    assert_eq!(lines(&rpc).await, vec!["bar bar", "bar bar"]);
}

#[tokio::test]
async fn substitute_bare_s_without_previous_errors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo"], "nothing to repeat");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E33"), "expected E33, got {msg:?}");
}

#[tokio::test]
async fn substitute_trailing_count_applies_to_n_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>gg");
    feed(&rpc, ":s/foo/bar/ 2<CR>"); // current line + 1 more
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "foo"]);
}

#[tokio::test]
async fn substitute_accepts_alternate_delimiters() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "i/usr/bin<Esc>");
    feed(&rpc, ":s#/usr#/opt#<CR>"); // `#` delimiter so `/` is literal in the pattern
    assert_eq!(lines(&rpc).await, vec!["/opt/bin"]);
    feed(&rpc, ":s,/bin,/sbin,<CR>"); // `,` delimiter
    assert_eq!(lines(&rpc).await, vec!["/opt/sbin"]);
}

#[tokio::test]
async fn substitute_tilde_recalls_previous_replacement() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo baz<Esc>");
    feed(&rpc, ":s/foo/bar/<CR>"); // -> "bar baz", remembers replacement "bar"
    feed(&rpc, ":s/baz/~/<CR>"); // `~` expands to the previous replacement "bar"
    assert_eq!(lines(&rpc).await, vec!["bar bar"]);
}

#[tokio::test]
async fn substitute_tilde_without_previous_errors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/~/<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo"], "no previous replacement");
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E33"), "expected E33, got {msg:?}");
}

// ----- Phase 3: the `c` (confirm) flag -----

#[tokio::test]
async fn substitute_confirm_prompts_then_y_replaces_n_skips() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>"); // opens a confirm prompt on the 1st match
    feed(&rpc, "y"); // replace #1
    feed(&rpc, "n"); // skip    #2
    feed(&rpc, "y"); // replace #3
    assert_eq!(lines(&rpc).await, vec!["bar foo bar"]);
}

#[tokio::test]
async fn substitute_confirm_shows_the_replace_prompt() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/bar/c<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo"],
        "nothing changes until the prompt is answered"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("replace with bar (y/n/a/l/q/^E/^Y)?")
    );
}

#[tokio::test]
async fn substitute_confirm_a_replaces_all_remaining() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "a"); // this match and every remaining one, no more prompts
    assert_eq!(lines(&rpc).await, vec!["bar bar bar"]);
}

#[tokio::test]
async fn substitute_confirm_q_quits_without_touching_the_rest() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "y"); // replace #1
    feed(&rpc, "q"); // quit before #2
    assert_eq!(lines(&rpc).await, vec!["bar foo foo"]);
}

#[tokio::test]
async fn substitute_confirm_esc_quits_like_q() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "y"); // replace #1
    feed(&rpc, "<Esc>"); // quit before #2
    assert_eq!(lines(&rpc).await, vec!["bar foo foo"]);
}

#[tokio::test]
async fn substitute_confirm_l_replaces_current_then_stops() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "n"); // skip #1
    feed(&rpc, "l"); // replace #2 and stop (last)
    assert_eq!(lines(&rpc).await, vec!["foo bar foo"]);
}

#[tokio::test]
async fn substitute_confirm_spans_a_range_with_y_and_n() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/c<CR>"); // one match per line (no g)
    feed(&rpc, "y"); // line 1
    feed(&rpc, "n"); // line 2
    feed(&rpc, "y"); // line 3
    assert_eq!(lines(&rpc).await, vec!["bar", "foo", "bar"]);
}

#[tokio::test]
async fn substitute_confirm_reports_count_when_done() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>");
    feed(&rpc, ":%s/foo/bar/c<CR>");
    feed(&rpc, "y");
    feed(&rpc, "y");
    let map = redraw_after(&rpc, &mut incoming, "y").await;
    assert_eq!(lines(&rpc).await, vec!["bar", "bar", "bar"]);
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("3 substitutions on 3 lines")
    );
}

#[tokio::test]
async fn substitute_confirm_is_a_single_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foo foo<Esc>");
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "y");
    feed(&rpc, "y");
    feed(&rpc, "y");
    assert_eq!(lines(&rpc).await, vec!["bar bar bar"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo foo foo"],
        "one u undoes it all"
    );
}

#[tokio::test]
async fn substitute_confirm_carriage_return_split_then_continue() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia, b, c<Esc>");
    feed(&rpc, ":s/, /\\r/gc<CR>"); // each ", " can split the line in two
    feed(&rpc, "y"); // split after "a" -> "a" / "b, c"
    feed(&rpc, "y"); // the walk continues onto the pushed-down tail
    assert_eq!(lines(&rpc).await, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn substitute_confirm_n_flag_overrides_c_and_only_counts() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo foo<Esc>");
    // `n` wins over `c`: a counting pass, no prompt, no edit.
    let map = redraw_after(&rpc, &mut incoming, ":s/foo/bar/gnc<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["foo foo"], "n makes no edits");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("2 matches on 1 line")
    );
}

#[tokio::test]
async fn substitute_confirm_ctrl_e_scrolls_without_consuming_the_match() {
    let path = write_n_lines("conf_ce", 100); // lines "line1".."line100"
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, "gg");
    feed(&rpc, ":%s/line/LINE/c<CR>"); // prompt opens on line 1's match
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    // The peek scrolled the window down a line but kept the prompt up and made
    // no edit — `^E` is not an answer. (nxvim keeps the cursor on screen every
    // frame, so the view-cursor rides along; the pending match lives in the
    // confirm state, not the cursor, so the answer still lands on it.)
    assert_eq!(first_visible_line(&map), "line2", "view scrolled one line");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("replace with LINE (y/n/a/l/q/^E/^Y)?"),
        "prompt still up after the scroll"
    );
    assert_eq!(
        lines(&rpc).await[0],
        "line1",
        "no substitution happened on the scroll key"
    );

    // The still-pending match answers to `y` as if the scroll never happened.
    feed(&rpc, "y");
    feed(&rpc, "q"); // stop after the first
    let after = lines(&rpc).await;
    assert_eq!(
        after[0], "LINE1",
        "y substituted the originally-prompted match"
    );
    assert_eq!(after[1], "line2", "and only that one");
}

#[tokio::test]
async fn substitute_confirm_cursor_lands_on_last_changed_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>gg0");
    feed(&rpc, ":%s/foo/bar/c<CR>");
    feed(&rpc, "y"); // line 1
    feed(&rpc, "n"); // skip line 2
    feed(&rpc, "y"); // line 3 — the last change
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "cursor on the last changed line"
    );
}

#[tokio::test]
async fn substitute_confirm_all_skipped_pushes_no_undo() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ofoo foo<Esc>"); // line 2 is "foo foo"; line 1 stays ""
    feed(&rpc, ":s/foo/bar/gc<CR>");
    feed(&rpc, "n"); // skip #1
    feed(&rpc, "n"); // skip #2 -> prompt ends, nothing changed
    assert_eq!(lines(&rpc).await, vec!["", "foo foo"], "buffer untouched");
    // The skipped substitute pushed no undo entry, so `u` reverts the prior edit
    // (the `o` insert) — not a phantom no-op snapshot.
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "u undoes the insert, not the :s"
    );
}

// ---- `regexsyntax`: the real vim regex engine in :substitute -----------------
//
// `:set regexsyntax=vim` swaps the editor's `:s` pattern + replacement dialect
// from the default canonical-regex (`(\w+)` / `$1`) to vim's "magic" dialect
// (`\(\w\+\)` groups, `\1` / `&` back-refs, case modifiers, `\<`/`\>`), backed by
// the embedded `nxvim-regex` engine. `pcre` keeps the historical behavior.

#[tokio::test]
async fn substitute_vim_engine_uses_backslash_capture_refs() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, ":set regexsyntax=vim<CR>");
    // Vim magic: `\(\)` groups + `\1`/`\2` refs — the opposite of the PCRE default.
    feed(&rpc, ":s/\\(\\w\\+\\) \\(\\w\\+\\)/\\2 \\1/<CR>");
    assert_eq!(lines(&rpc).await, vec!["world hello"]);
}

#[tokio::test]
async fn substitute_vim_engine_ampersand_is_whole_match() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "icat<Esc>");
    feed(&rpc, ":set regexsyntax=vim<CR>");
    // `&` in a vim replacement is the whole match (a literal `&` under PCRE).
    feed(&rpc, ":s/a/[&]/<CR>");
    assert_eq!(lines(&rpc).await, vec!["c[a]t"]);
}

#[tokio::test]
async fn substitute_vim_engine_case_modifier_title_cases() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, ":set regexsyntax=vim<CR>");
    // `\u&` upper-cases the first letter of each match.
    feed(&rpc, ":s/\\w\\+/\\u&/g<CR>");
    assert_eq!(lines(&rpc).await, vec!["Hello World"]);
}

#[tokio::test]
async fn substitute_vim_engine_non_greedy_group() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia::b::c<Esc>");
    feed(&rpc, ":set regexsyntax=vim<CR>");
    // `\{-}` is vim's non-greedy `*` — stops at the FIRST `::`, not the last.
    feed(&rpc, ":s/\\(.\\{-}\\)::.*/\\1/<CR>");
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn substitute_pcre_default_keeps_dollar_refs() {
    // Regression guard: with the option unset the canonical-regex default still
    // uses `(\w+)` / `$1`, exactly as before this engine existed.
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, ":s/(\\w+) (\\w+)/$2 $1/<CR>");
    assert_eq!(lines(&rpc).await, vec!["world hello"]);
}

#[tokio::test]
async fn regexsyntax_query_defaults_to_pcre() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set regexsyntax?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("regexsyntax=pcre")
    );
}

#[tokio::test]
async fn regexsyntax_rejects_unknown_value() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set regexsyntax=perl<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E474"),
        "expected E474 invalid-argument, got {msg:?}"
    );
}

#[tokio::test]
async fn regexsyntax_is_buffer_local() {
    // `:set regexsyntax` sets a BUFFER-LOCAL override (like `:set tabstop`): one
    // buffer can use vim's dialect while another keeps the pcre default.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "isome text<Esc>"); // make buffer 1 non-throwaway so :enew opens a new one
    feed(&rpc, ":set regexsyntax=vim<CR>"); // buffer 1 -> vim
    feed(&rpc, ":enew<CR>"); // buffer 2 -> follows the global (pcre)
    let map = redraw_after(&rpc, &mut incoming, ":set regexsyntax?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("regexsyntax=pcre"),
        "a fresh buffer follows the global pcre default"
    );
    feed(&rpc, ":bp<CR>"); // back to buffer 1
    let map = redraw_after(&rpc, &mut incoming, ":set regexsyntax?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("regexsyntax=vim"),
        "buffer 1 kept its own vim override"
    );
}

#[tokio::test]
async fn regexsyntax_settable_via_vim_bo() {
    // `vim.bo.regexsyntax` pins the dialect on the current buffer (e.g. from a
    // FileType autocmd). With "vim", `\<foo\>` word boundaries work in `/` search.
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo foobar foo<Esc>gg0");
    exec_lua(&rpc, "vim.bo.regexsyntax = 'vim'").await;
    feed(&rpc, "/\\<foo\\><CR>");
    assert_eq!(
        cursor(&rpc).await,
        (1, 11),
        "vim.bo override makes `/` skip the foo inside foobar"
    );
}

#[tokio::test]
async fn regexsyntax_global_default_via_vim_o() {
    // `vim.o.regexsyntax` sets the GLOBAL default; a buffer with no local override
    // follows it, so `:s` speaks vim's dialect.
    let (rpc, _i) = start(None).await;
    exec_lua(&rpc, "vim.o.regexsyntax = 'vim'").await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, ":s/\\(\\w\\+\\) \\(\\w\\+\\)/\\2 \\1/<CR>");
    assert_eq!(lines(&rpc).await, vec!["world hello"]);
}

// ===== oversized addresses / counts must clamp, never overflow ===============

#[tokio::test]
async fn huge_ex_line_address_clamps_to_the_last_line() {
    // A line address wider than i64 used to overflow the accumulator (a panic in
    // debug builds); vim clamps any out-of-range address to the last line.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>gg");
    feed(&rpc, ":99999999999999999999<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "an oversized address lands on the last line"
    );
}

#[tokio::test]
async fn huge_ex_address_offset_clamps_to_the_last_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>gg");
    feed(&rpc, ":.+99999999999999999999<CR>");
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "an oversized +offset lands on the last line"
    );
}

#[tokio::test]
async fn huge_substitute_count_means_to_end_of_file() {
    // `:s/pat/rep/ N` substitutes over N lines from the range's end; an N near
    // usize::MAX used to overflow `hi + c - 1` before the clamp (debug panic).
    // vim treats an oversized count as "through the last line".
    let (rpc, _incoming) = start(None).await;
    // From line 2, so `range.hi (1) + usize::MAX` genuinely overflows pre-fix.
    feed(&rpc, "ifoo<CR>foo<CR>foo<Esc>2gg");
    feed(&rpc, ":s/foo/bar/ 18446744073709551615<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "bar", "bar"],
        "the count clamps to the end of the file"
    );
}

// ----- `|` command chaining (vim's `:bar`) -------------------------------------

#[tokio::test]
async fn bar_chains_ex_commands_on_one_line() {
    // `:cmd1|cmd2` runs both, left to right — the shape behind idioms like
    // `:%bd|e#` and `:w|q`.
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":1d|1d<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["three", "    four", "five"],
        "both chained deletes run"
    );
}

#[tokio::test]
async fn bar_chain_aborts_after_a_failing_command() {
    // vim abandons the rest of the line once a command errors, so a chain can't
    // apply its tail to a state the failed command never established.
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":nosuchcommand|1d<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "two", "three", "    four", "five"],
        "the delete after a failed command must not run"
    );
}

#[tokio::test]
async fn escaped_bar_is_not_a_command_separator() {
    // Outside a pattern, a backslash still escapes the bar (vim's rule), so `\|` stays
    // in the argument. If it split, the tail would run as a command and report E492.
    let (rpc, mut incoming) = range_fixture().await;
    feed(&rpc, ":echom 'a\\|b'<CR>");
    let map = redraw_after(&rpc, &mut incoming, "").await;
    let msg = field(&map, "message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        !msg.contains("E492"),
        "an escaped bar must not split the command line, got {msg:?}"
    );
    assert!(
        msg.contains('|'),
        "the whole argument should reach `:echom`, got {msg:?}"
    );
}

#[tokio::test]
async fn bare_bar_is_alternation_inside_a_substitute_pattern() {
    // nxvim's regex flavor is PCRE, so a *bare* `|` is alternation (vim's `\|`) and a
    // `\|` is a literal bar — the reverse of vim. Command-line bar splitting must
    // therefore not touch a bar inside `:s`'s delimited sections, or the pattern
    // loses half of itself and the tail runs as a bogus command.
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":%s/two|three/X/<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["one", "X", "X", "    four", "five"],
        "a bare bar in the pattern is alternation, not a command separator"
    );
}

#[tokio::test]
async fn substitute_still_chains_after_its_final_delimiter() {
    // The flip side: a bar *after* the substitute's last delimiter is a real
    // separator, so `:s/../../|cmd` still chains.
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":%s/two/X/|1d<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["X", "three", "    four", "five"],
        "the substitute runs, then the chained delete"
    );
}

#[tokio::test]
async fn bare_bar_survives_a_vimgrep_pattern() {
    // `:vimgrep /{pat}/ {file}` opens a delimited pattern section too, so the same
    // rule applies: a bare `|` there is alternation, not a separator. If it split,
    // the tail would run as a command and report E492.
    let dir = temp_dir("vimgrep_bar");
    let file = dir.join("hay.txt");
    std::fs::write(&file, "two\nthree\nfour\n").expect("write");
    let (rpc, mut incoming) = start(Some(file.display().to_string())).await;

    feed(
        &rpc,
        &format!(":vimgrep /two|three/j {}<CR>", file.display()),
    );
    let map = redraw_after(&rpc, &mut incoming, "").await;
    let msg = field(&map, "message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    assert!(
        msg.contains("2 matches"),
        "the alternation should match both `two` and `three` (a split pattern reports \
         E682 on the truncated `/two`), got {msg:?}"
    );
}

#[tokio::test]
async fn normal_takes_a_literal_bar_argument() {
    // `:normal` is one of vim's bar-swallowing commands: the whole rest of the line
    // is keystrokes, so the bar is typed rather than starting a new command.
    let (rpc, _i) = range_fixture().await;
    feed(&rpc, ":1<CR>");
    feed(&rpc, ":normal A|x<CR>");
    assert_eq!(
        lines(&rpc).await,
        vec!["one|x", "two", "three", "    four", "five"],
        "`:normal` types the bar instead of chaining"
    );
}
