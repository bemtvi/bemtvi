use crate::support::*;

// ----- text objects --------------------------------------------------------

#[tokio::test]
async fn diw_deletes_the_word_under_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    // Cursor onto the middle word, delete it (leaving both surrounding spaces).
    feed(&rpc, "0wdiw");
    assert_eq!(lines(&rpc).await, vec!["foo  baz"]);
}

#[tokio::test]
async fn daw_deletes_the_word_and_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    feed(&rpc, "0wdaw");
    assert_eq!(lines(&rpc).await, vec!["foo baz"]);
}

#[tokio::test]
async fn daw_on_last_word_takes_leading_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>");
    // On the final word there is no trailing space, so the leading one goes.
    feed(&rpc, "$daw");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn ciw_changes_the_word_under_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    feed(&rpc, "0ciwqux<Esc>");
    assert_eq!(lines(&rpc).await, vec!["qux bar baz"]);
}

#[tokio::test]
async fn diw_on_whitespace_deletes_the_blank_run() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo   bar<Esc>");
    // Cursor into the run of spaces; `iw` is that whole run.
    feed(&rpc, "0llldiw");
    assert_eq!(lines(&rpc).await, vec!["foobar"]);
}

#[tokio::test]
async fn diw_on_punctuation_stops_at_the_class_boundary() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo...bar<Esc>");
    // On the dots, `iw` is just the punctuation run.
    feed(&rpc, "0llldiw");
    assert_eq!(lines(&rpc).await, vec!["foobar"]);
}

#[tokio::test]
async fn di_word_big_spans_punctuation() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo.bar baz<Esc>");
    // WORD ignores the `.` boundary, so `iW` is the whole "foo.bar".
    feed(&rpc, "0diW");
    assert_eq!(lines(&rpc).await, vec![" baz"]);
}

#[tokio::test]
async fn d2aw_deletes_two_words() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>");
    feed(&rpc, "0d2aw");
    assert_eq!(lines(&rpc).await, vec!["baz"]);
}

#[tokio::test]
async fn viw_selects_the_word() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Cursor in the middle of "hello", select the inner word.
    feed(&rpc, "0llviw");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    let sel = view_selection(&view);
    // "hello" spans columns [0, 5).
    assert_eq!(sel.first().copied().flatten(), Some((0, 5)));
}

#[tokio::test]
async fn di_paren_deletes_inside_the_parens() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    // Cursor inside the parens (onto 'b'), then delete the inner text.
    feed(&rpc, "0lllldi(");
    assert_eq!(lines(&rpc).await, vec!["foo()baz"]);
}

#[tokio::test]
async fn da_paren_deletes_the_parens_too() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    feed(&rpc, "0llllda(");
    assert_eq!(lines(&rpc).await, vec!["foobaz"]);
}

#[tokio::test]
async fn di_paren_works_with_the_cursor_on_the_close_bracket() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    // Move onto the closing paren (column 7), then delete inside.
    feed(&rpc, "0llllllldi(");
    assert_eq!(lines(&rpc).await, vec!["foo()baz"]);
}

#[tokio::test]
async fn ci_brace_changes_innermost_nested_pair() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i{a{b}c}<Esc>");
    // Cursor onto the inner 'b' (column 3); change the innermost braces.
    feed(&rpc, "0lllci{X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["{a{X}c}"]);
}

#[tokio::test]
async fn dib_is_an_alias_for_di_paren() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(bar)baz<Esc>");
    feed(&rpc, "0lllldib");
    assert_eq!(lines(&rpc).await, vec!["foo()baz"]);
}

#[tokio::test]
async fn di_brace_big_is_an_alias() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i{bar}<Esc>");
    feed(&rpc, "0diB");
    assert_eq!(lines(&rpc).await, vec!["{}"]);
}

#[tokio::test]
async fn da_angle_deletes_the_bracketed_text() {
    let (rpc, _incoming) = start(None).await;
    // `<lt>`/`<gt>` insert literal angle brackets (a bare `<x>` would parse as a
    // key). Buffer becomes "a<b>c".
    feed(&rpc, "ia<lt>b<gt>c<Esc>");
    // Cursor onto the '<' (column 1), then delete the angle-bracketed text.
    feed(&rpc, "0lda<");
    assert_eq!(lines(&rpc).await, vec!["ac"]);
}

#[tokio::test]
async fn di_bracket_spanning_lines() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix[a<CR>b]y<Esc>");
    // Cursor inside the brackets on the first line ('a', column 2).
    feed(&rpc, "gg0lldi[");
    // Charwise delete of "a\nb" joins the two lines around the brackets.
    assert_eq!(lines(&rpc).await, vec!["x[]y"]);
}

#[tokio::test]
async fn vi_paren_selects_inside() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i(abc)<Esc>");
    feed(&rpc, "0vi(");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    let sel = view_selection(&view);
    // "abc" sits at columns [1, 4).
    assert_eq!(sel.first().copied().flatten(), Some((1, 4)));
}

#[tokio::test]
async fn i_in_normal_mode_still_enters_insert() {
    let (rpc, _incoming) = start(None).await;
    // No operator and not visual: `i` must remain plain insert.
    feed(&rpc, "ifoo<Esc>");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn a_in_normal_mode_still_appends() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    // `a` after the 'f' appends, inserting between f and oo.
    feed(&rpc, "0aX<Esc>");
    assert_eq!(lines(&rpc).await, vec!["fXoo"]);
}

#[tokio::test]
async fn unknown_text_object_cancels_the_operator() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>");
    // `diz` is not a text object; it should cancel and leave the line intact.
    feed(&rpc, "0diz");
    assert_eq!(lines(&rpc).await, vec!["foo bar"]);
}

// ----- quote text objects --------------------------------------------------

#[tokio::test]
async fn di_quote_deletes_inside_the_quotes() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\" ok<Esc>");
    // Cursor inside the quotes (onto 'h', column 5).
    feed(&rpc, "0llllldi\"");
    assert_eq!(lines(&rpc).await, vec!["say \"\" ok"]);
}

#[tokio::test]
async fn da_quote_deletes_quotes_and_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\" ok<Esc>");
    feed(&rpc, "0llllllda\"");
    assert_eq!(lines(&rpc).await, vec!["say ok"]);
}

#[tokio::test]
async fn ci_quote_changes_inside_the_quotes() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\" ok<Esc>");
    feed(&rpc, "0llllllci\"X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["say \"X\" ok"]);
}

#[tokio::test]
async fn di_quote_seeks_forward_on_the_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "isay \"hi\"<Esc>");
    // Cursor before the quotes; vim seeks forward to the next pair on the line.
    feed(&rpc, "0di\"");
    assert_eq!(lines(&rpc).await, vec!["say \"\""]);
}

#[tokio::test]
async fn da_quote_takes_leading_space_when_no_trailing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix \"hi\"<Esc>");
    // No trailing whitespace after the closing quote, so the leading space goes.
    feed(&rpc, "0lllda\"");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn di_single_quote() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix'a'y<Esc>");
    // Cursor on 'a' (column 2).
    feed(&rpc, "0lldi'");
    assert_eq!(lines(&rpc).await, vec!["x''y"]);
}

#[tokio::test]
async fn da_backtick_deletes_quoted_span() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix`a`y<Esc>");
    feed(&rpc, "0llda`");
    assert_eq!(lines(&rpc).await, vec!["xy"]);
}

#[tokio::test]
async fn vi_quote_selects_inside() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i\"abc\"<Esc>");
    feed(&rpc, "0lvi\"");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    let sel = view_selection(&view);
    // "abc" sits at columns [1, 4).
    assert_eq!(sel.first().copied().flatten(), Some((1, 4)));
}

#[tokio::test]
async fn di_quote_without_a_pair_does_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ino quotes here<Esc>");
    feed(&rpc, "0di\"");
    assert_eq!(lines(&rpc).await, vec!["no quotes here"]);
}

#[tokio::test]
async fn di_quote_treats_escaped_quote_as_one_string_from_the_left() {
    let (rpc, _incoming) = start(None).await;
    // Buffer: "trib\"uto" — one string with an escaped quote in the middle.
    feed(&rpc, "i\"trib\\\"uto\"<Esc>");
    // Cursor in the "trib" half (column 2).
    feed(&rpc, "0lldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn di_quote_treats_escaped_quote_as_one_string_from_the_right() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i\"trib\\\"uto\"<Esc>");
    // Cursor in the "uto" half (column 8), past the escaped quote.
    feed(&rpc, "08ldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn da_quote_with_escaped_quote_deletes_the_whole_string() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix \"a\\\"b\"<Esc>");
    // Cursor inside; the escaped quote is not a delimiter.
    feed(&rpc, "0llllda\"");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn di_quote_escaped_backslash_keeps_the_closing_quote() {
    let (rpc, _incoming) = start(None).await;
    // Buffer: "a\\" — an escaped backslash, then a real closing quote.
    feed(&rpc, "i\"a\\\\\"<Esc>");
    feed(&rpc, "0ldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\""]);
}

#[tokio::test]
async fn di_quote_with_dangling_quote_works_on_the_left_side() {
    let (rpc, _incoming) = start(None).await;
    // Three unescaped quotes: "trib"uto" — a shared middle quote.
    feed(&rpc, "i\"trib\"uto\"<Esc>");
    // Cursor in the "trib" half (column 2).
    feed(&rpc, "0lldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"\"uto\""]);
}

#[tokio::test]
async fn di_quote_with_dangling_quote_works_on_the_right_side() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i\"trib\"uto\"<Esc>");
    // Cursor in the "uto" half (column 7), past the shared middle quote.
    feed(&rpc, "0llllllldi\"");
    assert_eq!(lines(&rpc).await, vec!["\"trib\"\""]);
}

#[tokio::test]
async fn ci_quote_between_two_strings_takes_the_gap() {
    let (rpc, _incoming) = start(None).await;
    // vim's `current_quote`: off a quote, the string runs from the nearest quote
    // before the cursor to the next one — between two strings, the gap itself.
    feed(&rpc, "i\"a\" \"b\"<Esc>");
    // Cursor on the space between the strings (column 3).
    feed(&rpc, "0lllci\"X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["\"a\"X\"b\""]);
}

/// The same with words in the gap, every flavour (expected from neovim).
#[tokio::test]
async fn quote_objects_between_two_strings_match_vim() {
    let (rpc, _incoming) = start(None).await;
    let text = ["say \"one\" and \"two\" end"];
    for col in [9, 10, 12] {
        assert_eq!(
            edit_probe(&rpc, &text, (1, col), "di\"").await,
            vec!["say \"one\"\"two\" end"],
            "di\" at {col}"
        );
        assert_eq!(
            edit_probe(&rpc, &text, (1, col), "da\"").await,
            vec!["say \"onetwo\" end"],
            "da\" at {col}"
        );
    }
    // Before the first string it still seeks forward; on a quote it pairs.
    assert_eq!(
        edit_probe(&rpc, &text, (1, 0), "di\"").await,
        vec!["say \"\" and \"two\" end"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 8), "di\"").await,
        vec!["say \"\" and \"two\" end"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 14), "di\"").await,
        vec!["say \"one\" and \"\" end"]
    );
}

// ----- paragraph & sentence text objects -----------------------------------

#[tokio::test]
async fn dap_deletes_the_paragraph_and_trailing_blank_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR><CR>three<Esc>");
    feed(&rpc, "ggdap");
    assert_eq!(lines(&rpc).await, vec!["three"]);
}

#[tokio::test]
async fn dip_deletes_just_the_paragraph() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR><CR>three<Esc>");
    feed(&rpc, "ggdip");
    assert_eq!(lines(&rpc).await, vec!["", "three"]);
}

#[tokio::test]
async fn dip_on_a_blank_line_deletes_the_blank_run() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR><CR><CR>two<Esc>");
    // Onto the middle blank line, delete the run of blank lines.
    feed(&rpc, "ggjdip");
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
}

#[tokio::test]
async fn vap_then_delete_matches_dap() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR><CR>three<Esc>");
    feed(&rpc, "ggvapd");
    assert_eq!(lines(&rpc).await, vec!["three"]);
}

#[tokio::test]
async fn das_deletes_a_sentence_with_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iHello world. Foo bar. Baz qux.<Esc>");
    feed(&rpc, "0das");
    assert_eq!(lines(&rpc).await, vec!["Foo bar. Baz qux."]);
}

#[tokio::test]
async fn dis_deletes_a_sentence_without_trailing_space() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iHello world. Foo bar.<Esc>");
    feed(&rpc, "0dis");
    assert_eq!(lines(&rpc).await, vec![" Foo bar."]);
}

#[tokio::test]
async fn das_on_a_middle_sentence() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iHello world. Foo bar. Baz qux.<Esc>");
    // Cursor onto the second sentence (column 13, 'F').
    feed(&rpc, "013ldas");
    assert_eq!(lines(&rpc).await, vec!["Hello world. Baz qux."]);
}

#[tokio::test]
async fn das_handles_a_terminator_before_a_closing_quote() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iSay \"Hi.\" Go.<Esc>");
    feed(&rpc, "0das");
    assert_eq!(lines(&rpc).await, vec!["Go."]);
}

#[tokio::test]
async fn cis_changes_the_current_sentence() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iOne. Two.<Esc>");
    feed(&rpc, "0cisHi<Esc>");
    assert_eq!(lines(&rpc).await, vec!["Hi Two."]);
}

// ----- linewise promotion of block objects ---------------------------------

#[tokio::test]
async fn di_paren_promotes_to_linewise_for_whole_line_content() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>    bar,<CR>    baz,<CR>)<Esc>");
    // Cursor on a content line, then delete the inner block.
    feed(&rpc, "ggjdi(");
    // The content lines go; the bracket lines stay (linewise).
    assert_eq!(lines(&rpc).await, vec!["foo(", ")"]);
}

#[tokio::test]
async fn di_brace_promotes_to_linewise_from_the_close_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifn() {<CR>    body();<CR>}<Esc>");
    // Cursor on the closing-brace line still finds the block.
    feed(&rpc, "di{");
    assert_eq!(lines(&rpc).await, vec!["fn() {", "}"]);
}

#[tokio::test]
async fn ci_brace_linewise_opens_a_line_for_insert() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifn() {<CR>    old();<CR>}<Esc>");
    feed(&rpc, "ggjci{new();<Esc>");
    assert_eq!(lines(&rpc).await, vec!["fn() {", "new();", "}"]);
}

#[tokio::test]
async fn da_paren_stays_charwise_for_whole_line_content() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>    bar,<CR>)<Esc>");
    // `a(` includes the brackets and is charwise: everything collapses.
    feed(&rpc, "ggjda(");
    assert_eq!(lines(&rpc).await, vec!["foo"]);
}

#[tokio::test]
async fn vi_paren_stays_charwise_in_visual_mode() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>    bar,<CR>)<Esc>");
    // In visual mode the block object is charwise (no linewise promotion), so
    // deleting joins the bracket lines.
    feed(&rpc, "ggjvi(d");
    assert_eq!(lines(&rpc).await, vec!["foo()"]);
}

#[tokio::test]
async fn di_paren_linewise_with_no_content_lines_is_a_noop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo(<CR>)<Esc>");
    feed(&rpc, "ggdi(");
    assert_eq!(lines(&rpc).await, vec!["foo(", ")"]);
}

// ----- f/t/F/T find-char motions -------------------------------------------

#[tokio::test]
async fn f_moves_onto_the_target_char() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0fo");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn f_with_a_count_finds_the_nth_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // 'l' is at columns 2, 3, 9; the 3rd is column 9.
    feed(&rpc, "03fl");
    assert_eq!(cursor(&rpc).await, (1, 9));
}

#[tokio::test]
async fn t_stops_before_the_target_char() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0to");
    assert_eq!(cursor(&rpc).await, (1, 3));
}

#[tokio::test]
async fn cap_f_searches_backward() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Cursor rests on the final 'd' (column 10); back to the 'o' at column 7.
    feed(&rpc, "Fo");
    assert_eq!(cursor(&rpc).await, (1, 7));
}

#[tokio::test]
async fn cap_t_stops_after_the_backward_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "To");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

#[tokio::test]
async fn f_does_nothing_when_the_char_is_absent() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0fz");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn dfx_deletes_through_the_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0dfo");
    assert_eq!(lines(&rpc).await, vec![" world"]);
}

#[tokio::test]
async fn dtx_deletes_up_to_the_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0dto");
    assert_eq!(lines(&rpc).await, vec!["o world"]);
}

#[tokio::test]
async fn d_cap_f_deletes_backward_excluding_the_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Cursor on 'd' (col 10); dFo deletes "orl" (cols 7..10), keeping 'd'.
    feed(&rpc, "dFo");
    assert_eq!(lines(&rpc).await, vec!["hello wd"]);
}

#[tokio::test]
async fn semicolon_repeats_the_find() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0fo;");
    // 'o' at col 4, then the next 'o' at col 7.
    assert_eq!(cursor(&rpc).await, (1, 7));
}

#[tokio::test]
async fn comma_repeats_the_find_reversed() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // fo -> col 4, ; -> col 7, , reverses back to col 4.
    feed(&rpc, "0fo;,");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn semicolon_after_t_skips_the_adjacent_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia-b-c-d<Esc>");
    // t- lands at col 0 (before the '-' at col 1); ; must advance, not stick.
    feed(&rpc, "0t-;");
    assert_eq!(cursor(&rpc).await, (1, 2));
}

#[tokio::test]
async fn v_f_then_delete_includes_the_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0vfod");
    assert_eq!(lines(&rpc).await, vec![" world"]);
}

// ----- vim ground truth (nvim --clean) for object edges and counts ---------

#[tokio::test]
async fn daw_on_the_last_word_of_a_line_takes_leading_space_not_the_newline() {
    let (rpc, _incoming) = start(None).await;
    let text = ["one two three", "four five"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 12), "daw").await,
        vec!["one two", "four five"]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.get('-')").await,
        Value::from(" three")
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 4), "d2aw").await,
        vec!["one", "four five"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 3), "d2aw").await,
        vec!["one", "four five"]
    );
    // Indentation is not "leading white space" for `aw`.
    assert_eq!(
        edit_probe(&rpc, &["  foo"], (1, 2), "daw").await,
        vec!["  "]
    );
    // `das` on the last sentence of a line likewise stays on its line.
    assert_eq!(
        edit_probe(
            &rpc,
            &["Hello world.  This is it.", "Next."],
            (1, 16),
            "das"
        )
        .await,
        vec!["Hello world.", "Next."]
    );
}

#[tokio::test]
async fn is_on_the_gap_between_sentences_selects_the_gap() {
    let (rpc, _incoming) = start(None).await;
    let text = ["Hello world.  This is it.  Another one!"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 12), "dis").await,
        vec!["Hello world.This is it.  Another one!"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 12), "das").await,
        vec!["Hello world.  Another one!"]
    );
}

#[tokio::test]
async fn paragraph_counts_take_whole_blocks() {
    let (rpc, _incoming) = start(None).await;
    let text = ["a1", "a2", "", "", "b1", "b2", "", "c1"];
    assert_eq!(edit_probe(&rpc, &text, (1, 0), "d2ap").await, vec!["c1"]);
    assert_eq!(
        edit_probe(&rpc, &text, (3, 0), "d2ap").await,
        vec!["a1", "a2"]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 0), "d3ip").await,
        vec!["", "c1"]
    );
}

#[tokio::test]
async fn bracket_object_count_selects_the_outer_level() {
    let (rpc, _incoming) = start(None).await;
    let text = ["x = (a, (b, c), d)"];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 9), "d2i(").await,
        vec!["x = ()"]
    );
    assert_eq!(edit_probe(&rpc, &text, (1, 9), "d2a(").await, vec!["x = "]);
    // Asking for more levels than exist is a no-op, as in vim.
    assert_eq!(
        edit_probe(&rpc, &text, (1, 6), "d2i(").await,
        vec!["x = (a, (b, c), d)"]
    );
}

#[tokio::test]
async fn ci_paren_on_empty_parens_inserts_between_them() {
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        edit_probe(&rpc, &["f()"], (1, 1), "ci(x<Esc>").await,
        vec!["f(x)"]
    );
    assert_eq!(
        edit_probe(&rpc, &["f()"], (1, 2), "ci)x<Esc>").await,
        vec!["f(x)"]
    );
    assert_eq!(
        edit_probe(&rpc, &["s = \"\""], (1, 4), "ci\"x<Esc>").await,
        vec!["s = \"x\""]
    );
}

#[tokio::test]
async fn repeating_an_object_in_visual_mode_grows_the_selection() {
    let (rpc, _incoming) = start(None).await;
    let words = ["one two three"];
    assert_eq!(
        edit_probe(&rpc, &words, (1, 0), "vawawd").await,
        vec!["three"]
    );
    assert_eq!(
        edit_probe(&rpc, &words, (1, 0), "viwiwiwd").await,
        vec![" three"]
    );
    let parens = ["x = (a, (b, c), d)"];
    assert_eq!(
        edit_probe(&rpc, &parens, (1, 9), "va(a(d").await,
        vec!["x = "]
    );
    assert_eq!(
        edit_probe(&rpc, &parens, (1, 9), "vi(i(d").await,
        vec!["x = ()"]
    );
    let paras = ["a1", "a2", "", "", "b1", "b2", "", "c1"];
    assert_eq!(
        edit_probe(&rpc, &paras, (1, 0), "vipipd").await,
        vec!["b1", "b2", "", "c1"]
    );
    assert_eq!(edit_probe(&rpc, &paras, (1, 0), "vapapd").await, vec!["c1"]);
}

#[tokio::test]
async fn as_takes_the_line_break_before_the_next_sentence() {
    let (rpc, _incoming) = start(None).await;
    // The white space after a sentence runs on to the next sentence's start, line
    // break and indent included; ending in column 0 from the indent, `das` is
    // linewise (vim's exclusive-motion rule).
    assert_eq!(
        edit_probe(&rpc, &["Foo bar.", "Baz qux."], (1, 0), "das").await,
        vec!["Baz qux."]
    );
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.gettype('\"')").await,
        Value::from("V")
    );
    assert_eq!(
        edit_probe(&rpc, &["  Foo bar.", "Baz qux."], (1, 3), "das").await,
        vec!["Baz qux."]
    );
    assert_eq!(
        edit_probe(
            &rpc,
            &["Hello world.  This is it.", "  Next."],
            (1, 16),
            "das"
        )
        .await,
        vec!["Hello world.  Next."]
    );
    assert_eq!(
        edit_probe(&rpc, &["One. Two.", "Three."], (1, 5), "das").await,
        vec!["One.", "Three."]
    );
    edit_probe(&rpc, &["a b", "c d", "", "e"], (1, 0), "yas").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.reg.get('\"')").await,
        Value::from("a b\nc d\n")
    );
    // Leading blanks reach back over a line break only when blanks precede it.
    assert_eq!(
        edit_probe(&rpc, &["Foo bar.  ", "Baz qux."], (2, 0), "das").await,
        vec!["Foo bar."]
    );
    // The linewise yank still leaves the cursor at the sentence's start.
    edit_probe(&rpc, &["Foo bar.", "Baz qux."], (1, 5), "yas").await;
    assert_eq!(cursor(&rpc).await, (1, 0));
    // Visual `as` stops short of the line break.
    assert_eq!(
        edit_probe(&rpc, &["Foo bar.", "Baz qux."], (1, 0), "vasd").await,
        vec!["", "Baz qux."]
    );
}

#[tokio::test]
async fn a_counted_inner_quote_takes_the_quotes_but_no_white_space() {
    // vim's `2i"`: the quotes are included, the white space `a"` adds is not.
    let (rpc, _incoming) = start(None).await;
    let text = ["x = \"ab\" + \"cd\""];
    assert_eq!(
        edit_probe(&rpc, &text, (1, 5), "2di\"").await,
        vec!["x =  + \"cd\""]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 5), "2ci\"Q<Esc>").await,
        vec!["x = Q + \"cd\""]
    );
    assert_eq!(
        edit_probe(&rpc, &text, (1, 5), "v2i\"d").await,
        vec!["x =  + \"cd\""]
    );
}
