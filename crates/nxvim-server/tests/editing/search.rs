use crate::support::*;

// ----- vim.fn.substitute (vim-regex compatibility) --------------------------

#[tokio::test]
async fn substitute_matches_vim_magic_semantics() {
    let (rpc, _incoming) = start(None).await;

    // Literal `\.` (magic: bare `.` is the wildcard, `\.` the literal), global.
    assert_eq!(substitute(&rpc, "a.b.c", r"\.", "/", "g").await, "a/b/c");
    // Bare `.` IS the wildcard in magic — first match only without `g`.
    assert_eq!(substitute(&rpc, "abc", ".", "X", "").await, "Xbc");
    // Escaped backslash → a literal backslash (Windows-path normalisation).
    assert_eq!(substitute(&rpc, r"a\b\c", r"\\", "/", "g").await, "a/b/c");
    // `\(\)` groups + `\+` one-or-more; `\1` in the replacement.
    assert_eq!(
        substitute(&rpc, "hello", r"\(l\+\)", r"[\1]", "").await,
        "he[ll]o"
    );
    // `&` is the whole match.
    assert_eq!(substitute(&rpc, "cat", "a", "[&]", "").await, "c[a]t");
    // `[^=]\+=` — a magic char class with `\+`.
    assert_eq!(substitute(&rpc, "VAR=val", r"[^=]\+=", "", "").await, "val");
    // POSIX class inside `[]`.
    assert_eq!(
        substitute(&rpc, "  hi  ", r"^[[:space:]]*", "", "").await,
        "hi  "
    );
}

#[tokio::test]
async fn substitute_handles_non_greedy_groups_and_anchors() {
    let (rpc, _incoming) = start(None).await;

    // The lspconfig `strip_archive_subpath` shape: `.\{-}` is non-greedy, so the
    // group stops at the FIRST `::`, not the last.
    assert_eq!(
        substitute(
            &rpc,
            "zipfile:///path/to/a::b::c",
            r"zipfile://\(.\{-}\)::.*$",
            r"\1",
            ""
        )
        .await,
        "/path/to/a"
    );
    // `$` anchors to the end; `^` to the start.
    assert_eq!(
        substitute(&rpc, "foobar", r"bar$", "BAZ", "").await,
        "fooBAZ"
    );
    assert_eq!(substitute(&rpc, "foofoo", r"^foo", "X", "g").await, "Xfoo");
}

#[tokio::test]
async fn substitute_very_magic_and_case_modifiers() {
    let (rpc, _incoming) = start(None).await;

    // `\v` very magic: bare `\d` class, `+`/`(` operators without backslashes.
    assert_eq!(
        substitute(&rpc, "a1b22c", r"\v\d+", "#", "g").await,
        "a#b#c"
    );
    assert_eq!(
        substitute(&rpc, "key: val", r"\v(\w+): (\w+)", r"\2=\1", "").await,
        "val=key"
    );
    // `\u&` upper-cases the first letter of each match (Title Case).
    assert_eq!(
        substitute(&rpc, "hello world", r"\w\+", r"\u&", "g").await,
        "Hello World"
    );
    // `\U…\E` upper-cases a span.
    assert_eq!(
        substitute(&rpc, "abc", r"\(b\)", r"\U\1\E", "").await,
        "aBc"
    );
    // The `i` flag folds case for matching.
    assert_eq!(substitute(&rpc, "FoO", "o", "0", "gi").await, "F00");
}

#[tokio::test]
async fn substitute_handles_zs_and_lookaround() {
    let (rpc, _incoming) = start(None).await;
    // `\zs` (match-start reset) — previously unrepresentable in RE2, now native:
    // `x\zsy` against "xy" matches just "y", so only the `y` is replaced.
    assert_eq!(substitute(&rpc, "xy", r"x\zsy", "z", "").await, "xz");
    // Look-around: `foo\(bar\)\@=` matches "foo" only when followed by "bar"
    // (the `\@=` look-ahead is consumed, not replaced).
    assert_eq!(
        substitute(&rpc, "foobar foobaz", r"foo\(bar\)\@=", "X", "g").await,
        "Xbar foobaz"
    );
}

// ----- vim.regex match-position anchors ( \zs / \ze ) -----------------------
//
// `vim.regex` is backed by the real vim regexp engine (`nxvim-regex`), so `\zs`
// (reset match start) and `\ze` (reset match end) report exactly the span vim
// does — the construct cmp-path relies on (`…/\zePAT*$`).

/// `\ze` resets the match END: `match_str` reports the span up to `\ze`, while the
/// text after it must still match. This is the exact construct cmp-path builds
/// (`…/\zePAT*$`), so getting it right is what unblocks cmp-path.
#[tokio::test]
async fn regex_ze_sets_match_end() {
    let (rpc, _incoming) = start(None).await;
    // `foo/\zebar` against "foo/bar": the whole pattern matches "foo/bar", but the
    // reported match is just "foo/" (offsets 0..4) — `bar` is matched-but-excluded.
    let span = exec_lua(
        &rpc,
        r#"local s, e = vim.regex([==[foo/\zebar]==]):match_str("foo/bar")
           return { s, e }"#,
    )
    .await;
    let span = span.as_array().expect("a {start, end} array");
    assert_eq!(span[0].as_i64(), Some(0), "match starts at 0");
    assert_eq!(
        span[1].as_i64(),
        Some(4),
        "\\ze ends the match before 'bar'"
    );
}

/// `\zs` resets the match START: everything before it must match but is excluded
/// from the reported span.
#[tokio::test]
async fn regex_zs_sets_match_start() {
    let (rpc, _incoming) = start(None).await;
    // `foo\zsbar` against "foobar": matches the whole, reports just "bar" (3..6).
    let span = exec_lua(
        &rpc,
        r#"local s, e = vim.regex([==[foo\zsbar]==]):match_str("foobar")
           return { s, e }"#,
    )
    .await;
    let span = span.as_array().expect("a {start, end} array");
    assert_eq!(span[0].as_i64(), Some(3), "\\zs starts the match at 'bar'");
    assert_eq!(span[1].as_i64(), Some(6), "match runs to the end");
}

/// A non-matching pattern still returns nil (not the zone group span), and `\zs`
/// combined with `\ze` reports exactly the span between them.
#[tokio::test]
async fn regex_zs_ze_zone_and_no_match() {
    let (rpc, _incoming) = start(None).await;
    let out = exec_lua(
        &rpc,
        r#"local both_s, both_e = vim.regex([==[a\zsbc\zed]==]):match_str("abcd")
           local nomatch = vim.regex([==[x\zsy]==]):match_str("ab")
           return { both_s, both_e, nomatch == nil }"#,
    )
    .await;
    let out = out.as_array().expect("a result array");
    assert_eq!(out[0].as_i64(), Some(1), "zone starts after 'a'");
    assert_eq!(out[1].as_i64(), Some(3), "zone ends before 'd'");
    assert_eq!(out[2].as_bool(), Some(true), "no overall match -> nil");
}

/// cmp-path's actual PATH_REGEX — built with `\%(…\)` groups, `\|` alternation and
/// a `\ze` boundary — must compile (it was rejected outright before `\ze` support)
/// and locate a path: `match_str` returns a real byte offset, not nil.
#[tokio::test]
async fn regex_cmp_path_pattern_compiles_and_matches() {
    let (rpc, _incoming) = start(None).await;
    let out = exec_lua(
        &rpc,
        r#"local NAME = [==[\%([^/\\:\*?<>'"`\|]\)]==]
           local pat = ([==[\%(\%(/PAT*[^/\\:\*?<>'"`\| .~]\)\|\%(/\.\.\)\)*/\zePAT*$]==]):gsub('PAT', NAME)
           local ok, re = pcall(vim.regex, pat)
           if not ok then return "compile-error: " .. tostring(re) end
           -- cmp-path feeds the text left of the cursor; `s` is the boundary it uses.
           local s = re:match_str("cd /usr/local/bi")
           return s"#,
    )
    .await;
    // The pattern matches at the first `/` of the path run (leftmost start). The
    // point is it compiled and matched at all — what cmp-path needs.
    assert_eq!(
        out.as_i64(),
        Some(3),
        "cmp-path PATH_REGEX compiles and match_str finds the path boundary"
    );
}

// ----- search ( `/`, `?`, `n`, `N` ) ----------------------------------------

#[tokio::test]
async fn search_forward_jumps_to_next_match() {
    let (rpc, _incoming) = search_fixture().await;
    // From the "foo" under the cursor on line 1, `/foo` finds the next one.
    feed(&rpc, "/foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 4));
    // And again moves to the third.
    feed(&rpc, "/foo<CR>");
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn search_forward_wraps_to_top() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, "G$"); // last line, last "foo"
    let _ = lines(&rpc).await; // barrier: flush the navigation redraw before capturing
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 0));
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("search hit BOTTOM, continuing at TOP")
    );
}

#[tokio::test]
async fn search_backward_jumps_to_previous_match() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "G"); // line 3
    feed(&rpc, "?foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 4));
}

#[tokio::test]
async fn n_and_capital_n_repeat_the_search() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>"); // -> (2,4)
    feed(&rpc, "n"); // same direction -> (3,4)
    assert_eq!(cursor(&rpc).await, (3, 4));
    feed(&rpc, "N"); // opposite direction -> back to (2,4)
    assert_eq!(cursor(&rpc).await, (2, 4));
}

#[tokio::test]
async fn greedy_pattern_steps_to_the_next_match_not_into_itself() {
    // A greedy pattern matches one whole span per line ("foo bar" -> "foo",
    // "baz foo" -> "baz foo"). Navigation must step between those distinct
    // matches, not crawl one grapheme deeper into the match under the cursor:
    // searching from the start of line 1's match lands on line 2, and `n` then
    // moves to line 3 — never to (1,1) or (2,1) inside the current match.
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, r"/.+o<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
    feed(&rpc, "n");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn search_vim_engine_word_boundary() {
    // `:set regexsyntax=vim` makes `/` speak vim's magic dialect, so `\<`/`\>`
    // word boundaries work — matching the standalone "foo", never the one inside
    // "foobar". (Under the PCRE default `\<foo\>` is an invalid pattern.)
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo foobar foo<Esc>gg0");
    feed(&rpc, ":set regexsyntax=vim<CR>");
    feed(&rpc, "/\\<foo\\><CR>");
    assert_eq!(
        cursor(&rpc).await,
        (1, 11),
        "\\<foo\\> skips the 'foo' inside 'foobar'"
    );
}

#[tokio::test]
async fn n_honors_a_count() {
    let (rpc, _incoming) = search_fixture().await;
    // First match is (2,4); `2n` skips ahead two: (3,4) then wrap to (1,0).
    feed(&rpc, "/foo<CR>");
    feed(&rpc, "2n");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn empty_pattern_repeats_the_last_search() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>"); // -> (2,4)
    feed(&rpc, "/<CR>"); // empty -> repeat forward -> (3,4)
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn missing_pattern_reports_e486_and_keeps_the_cursor() {
    let (rpc, mut incoming) = search_fixture().await;
    let map = redraw_after(&rpc, &mut incoming, "/zzz<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "cursor must not move on a miss");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: zzz")
    );
}

#[tokio::test]
async fn escape_cancels_the_search_prompt() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<Esc>");
    assert_eq!(cursor(&rpc).await, (1, 0), "Esc leaves the cursor put");
    // Back in normal mode: a plain motion works again.
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn command_line_shows_the_search_prefix_while_typing() {
    let (rpc, mut incoming) = search_fixture().await;
    let map = redraw_after(&rpc, &mut incoming, "/fo").await;
    assert_eq!(
        field(&map, "command_mode").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("fo"));
    assert_eq!(
        field(&map, "cmdline_prefix").and_then(Value::as_str),
        Some("/")
    );
}

// ----- search options & history (phase 2) -----------------------------------

#[tokio::test]
async fn search_is_case_sensitive_by_default() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>bar<CR>foo<Esc>gg");
    let _ = lines(&rpc).await;
    let map = redraw_after(&rpc, &mut incoming, "/FOO<CR>").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "no case-insensitive match by default"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E486: Pattern not found: FOO")
    );
}

#[tokio::test]
async fn ignorecase_matches_across_case() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>bar<CR>foo<Esc>gg");
    feed(&rpc, ":set ignorecase<CR>");
    feed(&rpc, "/FOO<CR>"); // folds to the "foo" on line 3
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn smartcase_makes_uppercase_patterns_sensitive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>foo<CR>Foo bar<Esc>gg");
    feed(&rpc, ":set ignorecase smartcase<CR>");
    // Lowercase pattern: case-insensitive, so the next line's "foo" matches.
    feed(&rpc, "/foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
    // Uppercase pattern: smartcase forces a case-sensitive match, skipping the
    // lowercase line to the capitalized "Foo" on line 3.
    feed(&rpc, "gg/Foo<CR>");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn counted_search_finds_the_nth_match() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "2/foo<CR>"); // 1st is (2,4), 2nd is (3,4)
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn nowrapscan_forward_reports_e385() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, ":set nowrapscan<CR>");
    feed(&rpc, "G$"); // past the last "foo"
    let _ = lines(&rpc).await;
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (3, 6), "cursor must not move");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E385: search hit BOTTOM without match for: foo")
    );
}

#[tokio::test]
async fn nowrapscan_backward_reports_e384() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, ":set nowrapscan<CR>");
    let _ = lines(&rpc).await;
    // Cursor is at the top, so nothing lies before it.
    let map = redraw_after(&rpc, &mut incoming, "?foo<CR>").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "cursor must not move");
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E384: search hit TOP without match for: foo")
    );
}

#[tokio::test]
async fn search_history_recalls_previous_patterns() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>");
    feed(&rpc, "/qux<CR>");
    let _ = lines(&rpc).await; // barrier before capturing
                               // Open a search prompt and walk back: newest ("qux") then older ("foo").
    let map = redraw_after(&rpc, &mut incoming, "/<Up><Up>").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("foo"));
    assert_eq!(
        field(&map, "cmdline_prefix").and_then(Value::as_str),
        Some("/")
    );
}

#[tokio::test]
async fn command_history_recalls_previous_commands() {
    // `:<Up>` walks back through previously-submitted ex commands (newest first),
    // replacing the typed line — the ex-command analogue of search history.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set number<CR>");
    feed(&rpc, ":set nonumber<CR>");
    let _ = lines(&rpc).await; // barrier before capturing
    let map = redraw_after(&rpc, &mut incoming, ":<Up>").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("set nonumber")
    );
    assert_eq!(
        field(&map, "cmdline_prefix").and_then(Value::as_str),
        Some(":")
    );
    // A second <Up> (still in the open prompt) reaches the older command.
    let map = redraw_after(&rpc, &mut incoming, "<Up>").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("set number")
    );
}

#[tokio::test]
async fn cmdline_left_arrow_inserts_mid_line() {
    // <Left> backs the command cursor over one char; typing then inserts there
    // rather than at the end. ":abc" + <Left> + "X" → "abXc".
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":abc<Left>X").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("abXc"));
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(3)
    );
}

#[tokio::test]
async fn cmdline_backspace_and_delete_act_at_the_cursor() {
    let (rpc, mut incoming) = start(None).await;
    // <Left> puts the cursor between b and c; <BS> removes the char before it (b).
    let map = redraw_after(&rpc, &mut incoming, ":abc<Left><BS>").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("ac"));
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(1)
    );
    // Fresh line: Home then <Del> removes the char under the cursor (the first).
    let map = redraw_after(&rpc, &mut incoming, "<Esc>:abc<Home><Del>").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("bc"));
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(0)
    );
}

#[tokio::test]
async fn cmdline_home_and_end_jump_to_the_ends() {
    let (rpc, mut incoming) = start(None).await;
    // Home sends the cursor to the start; inserting prepends.
    let map = redraw_after(&rpc, &mut incoming, ":abc<Home>X").await;
    assert_eq!(field(&map, "cmdline").and_then(Value::as_str), Some("Xabc"));
    // End jumps back to the tail; inserting appends.
    let map = redraw_after(&rpc, &mut incoming, "<End>Y").await;
    assert_eq!(
        field(&map, "cmdline").and_then(Value::as_str),
        Some("XabcY")
    );
    assert_eq!(
        field(&map, "cmdline_cursor").and_then(Value::as_u64),
        Some(5)
    );
}

#[tokio::test]
async fn cmdline_mid_line_edit_changes_the_executed_command() {
    // The point of in-line editing: fix a command before running it. Backing up
    // and inserting the missing space turns ":setnumber" into ":set number",
    // which enables the number option observably.
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(
        &rpc,
        &mut incoming,
        ":setnumber<Left><Left><Left><Left><Left><Left><Space><CR>",
    )
    .await;
    assert!(
        field(&map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "inserting a space mid-line should run :set number, enabling the option"
    );
}

#[tokio::test]
async fn command_history_up_arrow_reruns_last_command() {
    // The workflow that matters: open `:`, press <Up> to recall the last command,
    // <CR> to rerun it. Here recalling and submitting `:set number` re-enables the
    // number option observably in the redraw.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set number<CR>");
    feed(&rpc, ":set nonumber<CR>");
    let _ = lines(&rpc).await; // barrier
    let map = redraw_after(&rpc, &mut incoming, ":<Up><Up><CR>").await;
    assert!(
        field(&map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "rerunning :set number from history should re-enable the number option"
    );
}

// ----- search highlighting (phase 3: hlsearch / incsearch) ------------------

/// Per visible row, the search-match spans `[start, end)` (the `Search`
/// hlsearch highlight); an empty inner vec for rows with no match.
fn view_search(view: &[(Value, Value)]) -> Vec<Vec<(u64, u64)>> {
    view_get(view, "search")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| match v.as_array() {
                                    Some(p) if p.len() == 2 => Some((
                                        p[0].as_u64().unwrap_or(0),
                                        p[1].as_u64().unwrap_or(0),
                                    )),
                                    _ => None,
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn hlsearch_highlights_every_match_of_the_pattern() {
    let (rpc, mut incoming) = search_fixture().await;
    let map = redraw_after(&rpc, &mut incoming, "/foo<CR>").await;
    let search = view_search(&map);
    // "foo bar" / "baz foo" / "qux foo" → one "foo" match per line.
    assert_eq!(search.first().cloned().unwrap_or_default(), vec![(0, 3)]);
    assert_eq!(search.get(1).cloned().unwrap_or_default(), vec![(4, 7)]);
    assert_eq!(search.get(2).cloned().unwrap_or_default(), vec![(4, 7)]);
    // Rows past the end of the buffer carry no matches.
    assert!(search.iter().skip(3).all(Vec::is_empty));
}

#[tokio::test]
async fn nohlsearch_clears_the_match_highlight() {
    let (rpc, mut incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>");
    let _ = lines(&rpc).await; // barrier: flush the search redraw
    let map = redraw_after(&rpc, &mut incoming, ":noh<CR>").await;
    let search = view_search(&map);
    assert!(
        search.iter().all(Vec::is_empty),
        ":noh clears every match highlight, got {search:?}"
    );
}

#[tokio::test]
async fn incsearch_previews_the_next_match_while_typing() {
    let (rpc, mut incoming) = search_fixture().await;
    // Typing the pattern (no <CR>) hops the cursor to the next match live...
    let map = redraw_after(&rpc, &mut incoming, "/foo").await;
    assert_eq!(cursor(&rpc).await, (2, 4), "incsearch previews the match");
    // ...and the matches are already highlighted while still in the prompt.
    let search = view_search(&map);
    assert_eq!(search.get(1).cloned().unwrap_or_default(), vec![(4, 7)]);
}

#[tokio::test]
async fn escape_restores_the_origin_after_an_incsearch_preview() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo"); // preview hops the cursor to the line-2 match
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "<Esc>"); // ...and <Esc> rewinds to where the search began
    assert_eq!(cursor(&rpc).await, (1, 0), "Esc restores the search origin");
}

// ----- regex patterns (phase 4) ---------------------------------------------

#[tokio::test]
async fn dot_matches_any_character() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iac<CR>axc<Esc>gg");
    // `.` is a wildcard, so "axc" matches and the two-char "ac" does not.
    feed(&rpc, "/a.c<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn escaped_metacharacter_matches_literally() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaxc<CR>a.c<Esc>gg");
    // `\.` is a literal dot, so it skips "axc" for the line that really has one.
    feed(&rpc, "/a\\.c<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn anchor_caret_matches_line_start() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ixfoo<CR>foo bar<Esc>gg");
    // `^foo` ignores the "foo" embedded after x on line 1, taking line 2's start.
    feed(&rpc, "/^foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn anchor_dollar_matches_line_end() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ibar foo<CR>foo bar<Esc>gg");
    // `foo$` matches the trailing "foo" on line 1, not the one starting line 2.
    feed(&rpc, "/foo$<CR>");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn char_class_matches_a_digit() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<CR>a1c<Esc>gg");
    feed(&rpc, "/[0-9]<CR>");
    assert_eq!(cursor(&rpc).await, (2, 1));
}

#[tokio::test]
async fn quantifier_plus_requires_one_or_more() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iac<CR>abbbc<Esc>gg");
    // Canonical regex: bare `+` is the operator, so "ac" is skipped for "abbbc".
    feed(&rpc, "/ab+c<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn alternation_matches_either_branch() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifish<CR>dog<Esc>gg");
    // Canonical regex: bare `|` alternates (vim would need `\|`).
    feed(&rpc, "/cat|dog<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn word_boundary_matches_whole_word_only() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "icategory<CR>a cat<Esc>gg");
    // `\b` rejects the "cat" inside "category" for the standalone word.
    feed(&rpc, "/\\bcat\\b<CR>");
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn bare_plus_is_an_operator_not_a_literal() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia+b<CR>aaa<Esc>gg");
    // Canonical regex: `a+` matches one-or-more "a" (the "aaa" line), unlike vim
    // where a bare `+` is the literal character.
    feed(&rpc, "/a+<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn escaped_plus_matches_a_literal_plus() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>a+b<Esc>gg");
    // Escape with `\` to match the literal `+`, landing on the "a+b" line.
    feed(&rpc, "/a\\+b<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn inline_flag_forces_case_insensitive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ixxx<CR>FOO<Esc>gg");
    // Search is case-sensitive by default, but `(?i)` folds case for this pattern.
    feed(&rpc, "/(?i)foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn inline_flag_forces_case_sensitive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iFoo<CR>foo<Esc>gg");
    feed(&rpc, ":set ignorecase<CR>");
    // `ignorecase` would land on line 1's "Foo", but `(?-i)` overrides it.
    feed(&rpc, "/(?-i)foo<CR>");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn invalid_pattern_reports_e383_and_keeps_the_cursor() {
    let (rpc, mut incoming) = search_fixture().await;
    // An unbalanced group is a compile error (the escaped `\(` would be a literal).
    let map = redraw_after(&rpc, &mut incoming, "/a(b<CR>").await;
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "a pattern that does not compile must not move the cursor"
    );
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E383: Invalid search string: a(b")
    );
}

// ----- `*`/`#`, operator motion, offsets (phase 5) --------------------------

#[tokio::test]
async fn star_searches_word_under_cursor_forward() {
    let (rpc, _incoming) = search_fixture().await;
    // Cursor on "foo" (1,0); `*` jumps to the next whole-word "foo", then again.
    feed(&rpc, "*");
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "*");
    assert_eq!(cursor(&rpc).await, (3, 4));
}

#[tokio::test]
async fn hash_searches_word_under_cursor_backward() {
    let (rpc, _incoming) = search_fixture().await;
    feed(&rpc, "/foo<CR>"); // land on the start of line 2's "foo" (2,4)
    feed(&rpc, "#"); // `#` searches the word backward → line 1's "foo"
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn star_matches_whole_word_only() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foobar<CR>foo<Esc>gg");
    // `*` on "foo" skips "foobar" (not a whole word) for the standalone "foo".
    feed(&rpc, "*");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn g_star_matches_a_partial_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foobar<Esc>gg");
    // `g*` drops the word boundaries, so "foo" matches inside "foobar".
    feed(&rpc, "g*");
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn d_slash_deletes_up_to_the_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    // `d/world` deletes from the cursor up to (not including) the match.
    feed(&rpc, "d/world<CR>");
    assert_eq!(lines(&rpc).await, vec!["world"]);
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn c_slash_changes_up_to_the_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    feed(&rpc, "c/world<CR>"); // delete up to "world", land in insert mode
    feed(&rpc, "say <Esc>");
    assert_eq!(lines(&rpc).await, vec!["say world"]);
}

#[tokio::test]
async fn escape_during_an_operator_search_aborts_the_operator() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    feed(&rpc, "d/wor<Esc>"); // abandon the search → no delete
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
    assert_eq!(cursor(&rpc).await, (1, 0));
    // Back in normal mode: a plain edit still works.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["ello world"]);
}

#[tokio::test]
async fn search_offset_e_lands_on_the_match_end() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>gg");
    // `/world/e` puts the cursor on the last char of the match ("d", col 10).
    feed(&rpc, "/world/e<CR>");
    assert_eq!(cursor(&rpc).await, (1, 10));
}

#[tokio::test]
async fn search_offset_e_makes_an_operator_inclusive() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world foo<Esc>gg");
    // `d/world/e` deletes through the end of the match, leaving the rest.
    feed(&rpc, "d/world/e<CR>");
    assert_eq!(lines(&rpc).await, vec![" foo"]);
}

#[tokio::test]
async fn search_line_offset_moves_whole_lines() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb foo<CR>ccc<Esc>gg");
    // `/foo/+1` finds "foo" on line 2 and drops the cursor one line below.
    feed(&rpc, "/foo/+1<CR>");
    assert_eq!(cursor(&rpc).await, (3, 0));
}

#[tokio::test]
async fn search_from_visual_mode_stays_visual_and_extends_the_selection() {
    let (rpc, _incoming) = search_fixture().await;
    // Enter charwise visual at (1,0), then `/foo` searches forward to the next
    // "foo" (line 2, col 4). vim keeps the selection live: the mode stays Visual
    // and the moving end extends to the match, anchored at (1,0).
    feed(&rpc, "v/foo<CR>");
    assert_eq!(mode(&rpc).await, "v", "still in visual mode after a search");
    assert_eq!(cursor(&rpc).await, (2, 4));
    // The selection [(1,0)..=(2,4)] is real: `d` deletes through the match.
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["oo", "qux foo"]);
}

#[tokio::test]
async fn search_from_visual_line_mode_stays_visual_line() {
    let (rpc, _incoming) = search_fixture().await;
    // `V` (linewise) then `/qux` extends the line selection down to line 3.
    feed(&rpc, "V/qux<CR>");
    assert_eq!(mode(&rpc).await, "V", "still in visual-line mode");
    assert_eq!(cursor(&rpc).await, (3, 0));
    // The whole [1..=3] line range deletes.
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec![""]);
}
