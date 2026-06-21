use crate::support::*;

#[tokio::test]
async fn screen_column_accounts_for_wide_characters() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>"); // each CJK char is 3 bytes wide, 2 cells wide
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");
    // Cursor rests on the last char 本: byte column 3, screen column 2.
    assert_eq!(view_u64(&view, "cursor_col"), 3);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 2);
}

#[tokio::test]
async fn screen_column_expands_tabs_to_the_next_tabstop() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i<Tab>x<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    // Cursor on 'x' at byte column 1; the leading tab puts it at the next tabstop
    // (the default tabstop is 4), screen col 4.
    assert_eq!(view_u64(&view, "cursor_col"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 4);
}

/// With `nowrap` (the default; `:set wrap` opts into soft-wrap), a cursor driven
/// past the window's text width scrolls the viewport horizontally (`leftcol`) to
/// keep the cursor on screen, and scrolls back at column 0 — vim's `w_leftcol`.
#[tokio::test]
async fn nowrap_scrolls_horizontally_to_keep_cursor_visible() {
    let (rpc, mut incoming) = start(None).await;
    // A line far wider than the 80-column window.
    feed(&rpc, "i");
    feed(&rpc, &"abcdefghij".repeat(20)); // 200 columns

    // At column 0 the window is not horizontally scrolled.
    let at_start = redraw_after(&rpc, &mut incoming, "<Esc>0").await;
    assert_eq!(view_u64(&at_start, "leftcol"), 0);
    let text_width = 80 - view_u64(&at_start, "number_width");

    // Jumping to end-of-line scrolls the viewport right to keep the cursor visible.
    let at_end = redraw_after(&rpc, &mut incoming, "$").await;
    let leftcol = view_u64(&at_end, "leftcol");
    let csc = view_u64(&at_end, "cursor_screen_col");
    assert!(leftcol > 0, "leftcol must advance for an off-screen cursor");
    assert!(
        csc >= leftcol && csc - leftcol < text_width,
        "cursor (screen col {csc}) must be visible within [{leftcol}, {leftcol}+{text_width})"
    );

    // Returning to column 0 scrolls the viewport all the way back.
    let back = redraw_after(&rpc, &mut incoming, "0").await;
    assert_eq!(view_u64(&back, "leftcol"), 0);
}

/// `sidescrolloff` keeps a margin of columns between the cursor and the window
/// edge while horizontally scrolling, mirroring vim's option.
#[tokio::test]
async fn sidescrolloff_keeps_a_margin_to_the_right_of_the_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i");
    feed(&rpc, &"abcdefghij".repeat(20)); // 200 columns
    feed(&rpc, "<Esc>:set sidescrolloff=8<CR>0");

    // Land the cursor mid-line, well past the right edge, scrolling right.
    let map = redraw_after(&rpc, &mut incoming, "120l").await;
    let leftcol = view_u64(&map, "leftcol");
    let csc = view_u64(&map, "cursor_screen_col");
    let text_width = 80 - view_u64(&map, "number_width");
    assert!(
        csc >= leftcol && csc - leftcol < text_width,
        "cursor must be visible"
    );
    // There is text beyond the cursor, so the 8-column right margin is preserved.
    let right_margin = text_width - (csc - leftcol) - 1;
    assert_eq!(
        right_margin, 8,
        "sidescrolloff keeps 8 columns right of the cursor"
    );
}

/// The horizontal-scroll options are queryable via `:set ss?` and settable via
/// `:set ss=N`, like any number option.
#[tokio::test]
async fn set_sidescroll_query_echoes_the_value() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set sidescroll?<CR>").await;
    assert_eq!(view_str(&map, "message"), "sidescroll=1");
    let map = redraw_after(&rpc, &mut incoming, ":set sidescroll=5<CR>:set ss?<CR>").await;
    assert_eq!(view_str(&map, "message"), "sidescroll=5");
}

#[tokio::test]
async fn charwise_visual_highlights_the_selected_columns() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Back to column 0, then select three characters inclusively (h, e, l).
    feed(&rpc, "0vll");
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Cursor rests on the third char, which is included → columns [0, 3).
    assert_eq!(sel.first().copied().flatten(), Some((0, 3)));
    // No other visible row is selected.
    assert!(sel.iter().skip(1).all(Option::is_none));
}

#[tokio::test]
async fn charwise_visual_highlight_accounts_for_a_tab_and_wide_char() {
    let (rpc, mut incoming) = start(None).await;
    // "\ta你b": a tab, then 'a', a double-width '你', and 'b'. The highlight span
    // is in *screen* columns, so it must expand the tab (col-dependent width) and
    // count '你' as two cells — exactly the projection path the single-pass
    // virtcol mapper drives. With the default tabstop 4 the cells are:
    //   \t → 0..4,  a → 4,  你 → 5..7,  b → 7.
    feed(&rpc, "i<Tab>a你b<Esc>");
    // Select the whole line charwise: column 0 (the tab), then step over a, 你, b.
    feed(&rpc, "0vlll");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // [0, 8): tab(4) + a(1) + 你(2) + b(1), inclusive of the trailing 'b'. A bare
    // byte→column mapping (no tab expansion / wide-char width) would give end 6
    // (the byte length); a stale forward-walk cursor would mis-place it.
    assert_eq!(sel.first().copied().flatten(), Some((0, 8)));
}

#[tokio::test]
async fn charwise_visual_spanning_lines_marks_the_newline_cell() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>");
    // Top of buffer, column 0, then select down onto the second line's 'b'.
    feed(&rpc, "gg0vj");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // First line is fully selected plus one trailing cell for the newline.
    assert_eq!(sel.first().copied().flatten(), Some((0, 4)));
    // Second line is selected up to and including the char under the cursor.
    assert_eq!(sel.get(1).copied().flatten(), Some((0, 1)));
}

#[tokio::test]
async fn linewise_visual_highlights_the_whole_line_width() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    feed(&rpc, "V");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Linewise selection fills the line to the text edge: the viewport (attached
    // at 80) minus the default 4-cell number gutter, so the highlight stops at
    // the text area and never bleeds into the gutter.
    assert_eq!(sel.first().copied().flatten(), Some((0, 76)));
}

#[tokio::test]
async fn linewise_visual_fills_full_width_without_a_gutter() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    // With no number column the whole viewport width is text again.
    feed(&rpc, ":set nonumber norelativenumber<CR>");
    feed(&rpc, "V");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    assert_eq!(sel.first().copied().flatten(), Some((0, 80)));
}

#[tokio::test]
async fn charwise_visual_selecting_backwards_orders_the_span() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    // Cursor rests on 'o' (col 4); select leftwards back to 'l' (col 2).
    feed(&rpc, "vhh");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Anchor 'o' and cursor 'l' are both inclusive → columns [2, 5).
    assert_eq!(sel.first().copied().flatten(), Some((2, 5)));
}

#[tokio::test]
async fn incsearch_from_visual_extends_the_selection_live_while_typing() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg0");
    // Open a charwise selection at (1,0), then start typing a search for "bar"
    // *without* committing. vim keeps the selection live: the incsearch preview
    // hops the moving end to the match (line 2), and the highlight follows.
    feed(&rpc, "v/bar");
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    // Line 1 fully selected plus the newline cell; line 2 up to the preview cursor.
    assert_eq!(sel.first().copied().flatten(), Some((0, 4)));
    assert_eq!(sel.get(1).copied().flatten(), Some((0, 1)));
}

#[tokio::test]
async fn escaping_a_visual_search_restores_the_original_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg0");
    // Select two chars, start a search, then cancel it: the selection rewinds to
    // exactly what it was before the `/` — same mode, same extent.
    feed(&rpc, "vl/bar<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    assert_eq!(mode(&rpc).await, "v");
    let sel = view_selection(&view);
    assert_eq!(sel.first().copied().flatten(), Some((0, 2)));
    assert!(sel.iter().skip(1).all(Option::is_none));
}

#[tokio::test]
async fn leaving_visual_mode_clears_the_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "0vll<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");

    let sel = view_selection(&view);
    assert!(sel.iter().all(Option::is_none));
}

#[tokio::test]
async fn horizontal_motion_steps_over_multibyte_chars() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "in\u{e9}on<Esc>"); // "néon": n é(2 bytes) o n
    feed(&rpc, "0");
    assert_eq!(cursor(&rpc).await, (1, 0)); // 'n'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 1)); // 'é'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 3)); // 'o' — skipped é's second byte
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 4)); // last 'n'
    feed(&rpc, "l");
    assert_eq!(cursor(&rpc).await, (1, 4)); // stays put at end of line
    feed(&rpc, "hh");
    assert_eq!(cursor(&rpc).await, (1, 1)); // back across 'o' and onto 'é'
}

#[tokio::test]
async fn x_deletes_a_whole_grapheme_cluster() {
    let (rpc, _incoming) = start(None).await;
    // 'e' + combining acute accent (one grapheme, three bytes) followed by 'x'.
    feed(&rpc, "ie\u{0301}x<Esc>");
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn x_deletes_a_wide_char_and_leaves_the_rest() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i日本<Esc>");
    feed(&rpc, "0x");
    assert_eq!(lines(&rpc).await, vec!["本"]);
}

#[tokio::test]
async fn charwise_paste_keeps_a_combining_grapheme_intact() {
    // "éx" is e + combining acute, then x. Yank the é cluster, then paste it
    // after the cursor: it must land whole after é, never split between the
    // base and its combining mark.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ie\u{0301}x<Esc>");
    feed(&rpc, "0ylp");
    assert_eq!(lines(&rpc).await, vec!["e\u{0301}e\u{0301}x"]);
}

#[tokio::test]
async fn r_replaces_a_whole_grapheme_cluster() {
    // `r` removes its range directly (it does not go through the grapheme-aware
    // snap_range that `x` uses), so grapheme-stepping the advance is what keeps
    // the combining mark from being orphaned onto the replacement character.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ie\u{0301}x<Esc>"); // "éx" as e + combining acute + x
    feed(&rpc, "0rz"); // replace the first grapheme (é) with 'z'
    assert_eq!(lines(&rpc).await, vec!["zx"]);
}

#[tokio::test]
async fn r_replaces_with_a_keymap_prefix_char_instantly() {
    // `rg` must replace the char under the cursor with a literal `g` *now*, even
    // though `g` is a live prefix of the native `gd`/`gD`/`gr` LSP mappings. The
    // replacement char is an argument read literally, so it bypasses the keymap
    // matcher; without the bypass the matcher withholds the `g` waiting to
    // disambiguate `gd`/`gr`, and the replace appears to hang.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    feed(&rpc, "0rg");
    assert_eq!(lines(&rpc).await, vec!["gbc"]);
}

#[tokio::test]
async fn counted_replace_with_a_keymap_prefix_char() {
    // The count before `r` lives in pending and is untouched by the literal-arg
    // bypass, so `3rg` still replaces three chars with `g` even though `g` is a
    // native-map prefix.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcd<Esc>");
    feed(&rpc, "03rg");
    assert_eq!(lines(&rpc).await, vec!["gggd"]);
}

#[tokio::test]
async fn find_target_is_a_literal_keymap_prefix_char() {
    // `fg` likewise reads its target char literally: it must jump to the `g`
    // without the matcher withholding it as a `gd`/`gr` prefix.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabgde<Esc>");
    feed(&rpc, "0fg");
    assert_eq!(cursor(&rpc).await, (1, 2));
}

#[tokio::test]
async fn insert_backspace_deletes_a_precomposed_char() {
    let (rpc, _incoming) = start(None).await;
    // Type "aé" (é precomposed, 2 bytes) then backspace once: the whole 'é' goes.
    feed(&rpc, "ia\u{e9}");
    feed(&rpc, "<BS>");
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn insert_backspace_deletes_a_combining_grapheme() {
    let (rpc, _incoming) = start(None).await;
    // Type "a" then "e" + combining acute (one grapheme). Backspace must remove
    // the WHOLE cluster (base + mark), not just the combining mark.
    feed(&rpc, "iae\u{0301}");
    feed(&rpc, "<BS>");
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["a"]);
}

#[tokio::test]
async fn dw_deletes_a_multibyte_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ih\u{e9}llo w\u{f6}rld<Esc>"); // "héllo wörld"
    feed(&rpc, "0dw");
    assert_eq!(lines(&rpc).await, vec!["w\u{f6}rld"]);
}

#[tokio::test]
async fn b_and_e_handle_multibyte_words() {
    // "foo wörld": w is byte 4, ö spans bytes 5..7, d is byte 9.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo w\u{f6}rld<Esc>");
    // `b` lands on a word boundary, never inside ö's continuation byte.
    feed(&rpc, "$b");
    assert_eq!(cursor(&rpc).await, (1, 4)); // start of "wörld"
    feed(&rpc, "b");
    assert_eq!(cursor(&rpc).await, (1, 0)); // start of "foo"

    // `e` lands on the last char of each word, stepping over the wide cluster.
    feed(&rpc, "e");
    assert_eq!(cursor(&rpc).await, (1, 2)); // last 'o' of "foo"
    feed(&rpc, "e");
    assert_eq!(cursor(&rpc).await, (1, 9)); // 'd' at the end of "wörld"
}

#[tokio::test]
async fn vertical_motion_keeps_screen_column_across_wide_chars() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i日本x<Esc>"); // screen columns: 日@0, 本@2, x@4
    feed(&rpc, "oabcdef<Esc>"); // an ASCII line below it
    feed(&rpc, "gg"); // line 1, on 日
    feed(&rpc, "l"); // → 本, byte col 3, screen col 2
    assert_eq!(cursor(&rpc).await, (1, 3));
    feed(&rpc, "j"); // down: screen col 2 → byte col 2 ('c')
    assert_eq!(cursor(&rpc).await, (2, 2));
    feed(&rpc, "k"); // back up: screen col 2 → byte col 3 (本)
    assert_eq!(cursor(&rpc).await, (1, 3));
}

#[tokio::test]
async fn vertical_motion_keeps_screen_column_across_a_tab() {
    // A leading tab expands to the default tabstop (4), so 'x' sits at screen
    // column 4 even though it is byte 1. Vertical motion must map that screen
    // column onto the ASCII line below (where byte == screen column).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "i<Tab>x<Esc>"); // line 1: "\tx"
    feed(&rpc, "oabcdefghij<Esc>"); // line 2: ASCII
    feed(&rpc, "ggl"); // line 1, onto 'x' at byte 1 / screen col 4
    assert_eq!(cursor(&rpc).await, (1, 1));
    feed(&rpc, "j"); // down: screen col 4 → byte 4 ('e')
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "k"); // back up: screen col 4 → byte 1 ('x')
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn dl_deletes_a_trailing_multibyte_grapheme() {
    // `dl` on the last char must delete that whole grapheme (like `x`) and keep
    // the line's newline. This relies on `l` advancing its motion target to
    // end-of-line (s.len()) so the exclusive operator range covers the last
    // character; clamping `l` short of EOL would make `dl` a no-op here.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "in\u{e9}on<Esc>"); // "néon"
    feed(&rpc, "$dl"); // on last 'n' -> delete it
    assert_eq!(lines(&rpc).await, vec!["n\u{e9}o"]);
    feed(&rpc, "$dl"); // on 'o' -> delete it
    assert_eq!(lines(&rpc).await, vec!["n\u{e9}"]);
    feed(&rpc, "$dl"); // on 'é' -> delete the whole 2-byte cluster
    assert_eq!(lines(&rpc).await, vec!["n"]);
}

#[tokio::test]
async fn redraw_has_no_scroll_for_plain_motion() {
    let path = write_n_lines("noscroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "j").await;

    assert!(
        scroll(&map).is_none(),
        "a plain `j` must carry no scroll gesture"
    );
    assert_eq!(lines_len(&map), 24, "viewport stays one screen tall");
}

#[tokio::test]
async fn ctrl_d_emits_half_page_scroll() {
    let path = write_n_lines("cd", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;

    // Viewport height 24 → half page = 12. (The buffer-line names are derived from
    // the screen-row band; with no virt_lines they equal the buffer-line offsets.)
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 12);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8, within [80,160]
                                                     // Band screen rows = |to-from| + height = 12 + 24.
    assert_eq!(scroll_lines_len(&map), 36);
}

#[tokio::test]
async fn virt_lines_scroll_animates_and_interleaves_the_virtual_row() {
    // A scroll whose range contains a `virt_lines` extmark used to fall back to an
    // instant snap (no gesture) — the band was buffer-line based and couldn't place
    // the extra screen rows. The band is now screen-row based, so the virtual rows
    // ride the slide: the redraw carries a gesture, and the band interleaves the
    // virtual row (a `None`-numbered band row, making the band one row taller).
    let path = write_n_lines("vlscroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // A whole virtual row below line 4 (index 3), inside the half-page <C-d> range.
    exec_lua(
        &rpc,
        r#"local ns = vim.api.nvim_create_namespace("vl")
           vim.api.nvim_buf_set_extmark(0, ns, 3, 0, {
               virt_lines = { { { "-- virtual --", "Comment" } } },
           })"#,
    )
    .await;
    let _ = lines(&rpc).await; // barrier: flush the decor redraw

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;

    // Previously this snapped (scroll == None); now it animates.
    assert!(
        scroll(&map).is_some(),
        "a virt_lines-containing scroll now animates instead of snapping"
    );
    // The band interleaves the virtual row: line 4 (index 3) sits at band row 3, its
    // `virt_lines`-below virtual row at band row 4 (a `None` number), then line 5.
    let numbers = scroll_numbers(&map);
    assert_eq!(numbers[3], Some(4), "line 4 is band row 3");
    assert_eq!(
        numbers[4], None,
        "the interleaved virtual row carries no number"
    );
    assert_eq!(numbers[5], Some(5), "line 5 follows the virtual row");
    // The extra screen row makes the band one taller than the plain |12| + 24 = 36.
    assert_eq!(
        scroll_lines_len(&map),
        37,
        "the band over-scans the virtual row"
    );
}

#[tokio::test]
async fn set_wrap_lays_a_long_line_across_display_rows() {
    // `:set wrap` lays a line wider than the text area across several display rows
    // instead of panning horizontally — the row model emits one `RowKind::Line` row
    // per soft-wrap segment, and the cursor lands on the wrapped row + row-local col.
    let (rpc, mut incoming) = start(None).await;
    // Full 80-cell text area (drop the number/relative-number gutter).
    feed(&rpc, ":set nonumber<CR>:set norelativenumber<CR>");
    feed(&rpc, "i");
    feed(&rpc, &"a".repeat(200)); // 200 columns
    let map = redraw_after(&rpc, &mut incoming, "<Esc>:set wrap<CR>").await;

    let lines = view_lines(&map);
    assert_eq!(lines[0].chars().count(), 80, "row 0 is the first 80 cells");
    assert_eq!(lines[1].chars().count(), 80, "row 1 is the next 80 cells");
    assert_eq!(
        lines[2],
        "a".repeat(40),
        "row 2 holds the 40-cell remainder"
    );
    // The cursor (last 'a', byte 199) sits on the third display row at row-local
    // column 39 (199 - 160), with no horizontal scroll.
    assert_eq!(
        view_u64(&map, "cursor_row"),
        2,
        "cursor on the third display row"
    );
    assert_eq!(
        view_u64(&map, "cursor_screen_col"),
        39,
        "row-local cursor column"
    );
    assert_eq!(view_u64(&map, "leftcol"), 0, "wrap pins leftcol at 0");
}

#[tokio::test]
async fn wrapped_content_scrolls_and_rides_the_band() {
    // The proof of the row-model refactor: word-wrap needs *no* scroll-path changes.
    // The band is screen-row based, so a wrapped buffer scrolls and animates with the
    // wrap rows interleaved — a buffer line occupies several consecutive band rows.
    let body: String = (1..=60)
        .map(|i| format!("{i}{}\n", "x".repeat(200)))
        .collect();
    let path = write_temp("wrapscroll", "txt", &body);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    let _ = lines(&rpc).await; // barrier

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;
    assert!(
        scroll(&map).is_some(),
        "wrapped content still animates — no virt_lines-style snap"
    );
    // A wrapped line occupies several band rows, so its 1-based number repeats
    // consecutively in the band's `numbers` (the wrap rows ride the band).
    let numbers = scroll_numbers(&map);
    assert!(
        numbers.windows(2).any(|w| w[0].is_some() && w[0] == w[1]),
        "a wrapped line spans consecutive band rows: {numbers:?}"
    );
}

#[tokio::test]
async fn gj_gk_step_display_rows_within_a_wrapped_line() {
    // `gj`/`gk` move by *display* row, so within a soft-wrapped line they step its
    // continuation rows (the cursor stays on the same buffer line, its column
    // advancing by the row width) rather than jumping a whole buffer line like j/k.
    // `incoming` is bound (not dropped) to keep the RPC connection alive, though
    // these cursor-only assertions never read a redraw off it.
    let (rpc, _incoming) = start(None).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    feed(&rpc, "i");
    feed(&rpc, &"a".repeat(200)); // one buffer line, wraps to 80 + 80 + 40
    feed(&rpc, "<Esc>gg0"); // line 1, column 0 (first display row)
    let _ = lines(&rpc).await;

    feed(&rpc, "gj");
    assert_eq!(
        cursor(&rpc).await,
        (1, 80),
        "gj → second display row, same buffer line"
    );
    feed(&rpc, "gj");
    assert_eq!(cursor(&rpc).await, (1, 160), "gj → third display row");
    feed(&rpc, "gk");
    assert_eq!(cursor(&rpc).await, (1, 80), "gk → back up one display row");
}

#[tokio::test]
async fn gj_crosses_lines_and_falls_back_to_j_without_wrap() {
    let body = format!("{}\nsecond\n", "a".repeat(200));
    let path = write_temp("gjcross", "txt", &body);
    let (rpc, _incoming) = start(Some(path)).await; // bound to keep the RPC alive
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    let _ = lines(&rpc).await;

    // From the last display row of line 1, gj crosses to the next buffer line.
    feed(&rpc, "gg0gjgj"); // line 1, third display row (col 160)
    feed(&rpc, "gj");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "gj from the last display row crosses to the next line"
    );

    // With `nowrap`, gj is plain j: one buffer line.
    feed(&rpc, ":set nowrap<CR>gg0");
    feed(&rpc, "gj");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "nowrap: gj == j (one buffer line down)"
    );
}

#[tokio::test]
async fn overlays_clip_and_rebase_to_the_wrap_segment() {
    // The proof of the segment-aware projection fix: a per-row overlay computed in
    // full-line columns (here an inline `virt_text` extmark) is clipped to the wrap
    // segment its anchor falls in and rebased to that row's local columns — so on a
    // wrapped line it lands on the right continuation row, at the right column, not
    // drifted by the segment offset. Uses an extmark (hermetic — no LSP/treesitter).
    let (rpc, mut incoming) = start(None).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    feed(&rpc, "i");
    feed(&rpc, &"a".repeat(200)); // one line, wraps to 80 + 80 + 40
    feed(&rpc, "<Esc>");
    // Inline virt_text anchored at column 100 — inside the SECOND display row's
    // segment [80, 160), so at row-local column 20 on row 1.
    exec_lua(
        &rpc,
        r#"local ns = vim.api.nvim_create_namespace("vt")
           vim.api.nvim_buf_set_extmark(0, ns, 0, 100, {
               virt_text = { { "HINT", "Comment" } },
               virt_text_pos = "inline",
           })"#,
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    // Per row, the inline virt_text anchor columns ([pos, col, hl_mode, chunks];
    // pos 1 = inline).
    let cols = |row: usize| -> Vec<u64> {
        view_get(&map, "virt_text")
            .and_then(Value::as_array)
            .and_then(|rows| rows.get(row))
            .and_then(Value::as_array)
            .map(|places| {
                places
                    .iter()
                    .filter_map(|p| {
                        let p = p.as_array()?;
                        (p[0].as_u64()? == 1).then(|| p[1].as_u64())?
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        cols(0).is_empty(),
        "the anchor is not on the first display row"
    );
    assert_eq!(
        cols(1),
        vec![20],
        "inline virt_text lands on the continuation row, rebased to column 20"
    );
}

#[tokio::test]
async fn showbreak_prefixes_continuation_rows_and_reduces_wrap_width() {
    // `:set showbreak=>>` draws a marker at the start of every soft-wrap continuation
    // row; the marker consumes leading cells, so a continuation wraps into
    // `width - marker_width` text cells. The first display row has no marker. The
    // cursor lands past the marker on a continuation row.
    let (rpc, mut incoming) = start(None).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    feed(&rpc, ":set showbreak=>><CR>");
    feed(&rpc, "i");
    feed(&rpc, &"a".repeat(200)); // wraps to 80, then 78 + 78 + ... at width-2
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    let lines = view_lines(&map);
    assert_eq!(
        lines[0].chars().count(),
        80,
        "first row: full width, no marker"
    );
    assert!(
        !lines[0].starts_with(">>"),
        "the first row carries no marker"
    );
    assert!(
        lines[1].starts_with(">>"),
        "continuation row starts with the marker"
    );
    assert_eq!(
        lines[1].chars().count(),
        80,
        "marker (2) + 78 text cells fills the row"
    );
    assert_eq!(
        &lines[1][2..],
        &"a".repeat(78),
        "the marker precedes the wrapped text"
    );

    // gj from the first row's start lands on the second display row, just past the
    // marker (screen column 2), still on the same buffer line.
    feed(&rpc, "gg0");
    let map = redraw_after(&rpc, &mut incoming, "gj").await;
    assert_eq!(
        view_u64(&map, "cursor_row"),
        1,
        "cursor on the second display row"
    );
    assert_eq!(
        view_u64(&map, "cursor_screen_col"),
        2,
        "cursor sits past the 2-cell marker"
    );
}

#[tokio::test]
async fn showbreak_and_breakindent_order_and_briopt_sbr() {
    // With both `breakindent` and `showbreak`, vim's default draws the breakindent
    // first and the marker right before the wrapped text (`    >>text`), so the text
    // sits one marker-width past the indent. `:set breakindentopt=sbr` draws the
    // marker first and absorbs its width into the indent, so the text aligns exactly
    // under the line's indent (`>>  text`).
    let (rpc, mut incoming) = start(None).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    feed(&rpc, ":set breakindent<CR>:set showbreak=>><CR>");
    feed(&rpc, "i");
    feed(&rpc, "    "); // 4-space indent
    feed(&rpc, &"a".repeat(200));
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    // Default order: 4 indent spaces, then the marker, then text (text at column 6).
    let lines = view_lines(&map);
    assert_eq!(
        &lines[1][..6],
        "    >>",
        "indent then marker, before the text"
    );
    assert_eq!(
        &lines[1][6..],
        &"a".repeat(74),
        "text wraps into width - 6 cells"
    );

    // `breakindentopt=sbr`: marker first, indent reduced by its width — text aligns
    // under the line's own indent (column 4).
    let map = redraw_after(&rpc, &mut incoming, ":set breakindentopt=sbr<CR>").await;
    let lines = view_lines(&map);
    assert_eq!(
        &lines[1][..4],
        ">>  ",
        "marker then reduced indent, text at column 4"
    );
    assert_eq!(
        &lines[1][4..],
        &"a".repeat(76),
        "text wraps into width - 4 cells"
    );
}

#[tokio::test]
async fn breakindent_indents_continuation_rows_to_match_the_line() {
    // `:set breakindent` indents continuation rows to match the wrapped line's own
    // leading whitespace, so the wrapped text reads as a hanging block.
    let (rpc, mut incoming) = start(None).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    feed(&rpc, ":set breakindent<CR>");
    feed(&rpc, "i");
    feed(&rpc, "    "); // 4-space indent
    feed(&rpc, &"a".repeat(200));
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    let lines = view_lines(&map);
    // First row: 4 indent + 76 a's = 80. Continuation rows: 4 spaces of breakindent
    // then 76 a's (wrapped into width - 4 cells).
    assert_eq!(lines[0].chars().count(), 80);
    assert!(
        lines[1].starts_with("    "),
        "continuation indented 4 cells"
    );
    assert_eq!(&lines[1][..4], "    ");
    assert_eq!(
        &lines[1][4..],
        &"a".repeat(76),
        "wrapped text aligns under the line's indent"
    );
}

#[tokio::test]
async fn wrap_continuation_rows_are_flagged_for_a_blank_gutter() {
    // A wrapped line's number shows on its first display row only; the client blanks
    // the gutter on the continuation rows. The server marks them with a per-row
    // `continuation` flag (the wire signal), while `numbers` still repeats the line
    // number on every row (so highlights / diagnostics keep their row→line mapping
    // and a continuation stays distinct from a `~` filler).
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set nonumber<CR>:set norelativenumber<CR>");
    feed(&rpc, "ishort<CR>");
    feed(&rpc, &"a".repeat(200)); // a line that wraps to 80 + 80 + 40
    let map = redraw_after(&rpc, &mut incoming, "<Esc>:set wrap<CR>").await;

    let cont = view_continuation(&map);
    let nums = view_numbers(&map);
    // Row 0: "short" (line 1, first row). Rows 1..=3: line 2's first + two
    // continuations. The first row of each line is not a continuation; the wrap
    // rows are.
    assert!(!cont[0], "line 1's only row is not a continuation");
    assert!(!cont[1], "line 2's first display row is not a continuation");
    assert!(cont[2], "line 2's second display row is a continuation");
    assert!(cont[3], "line 2's third display row is a continuation");
    // `numbers` still carries the line number on every row of line 2 (1-based 2).
    assert_eq!(nums[1], Some(2));
    assert_eq!(
        nums[2],
        Some(2),
        "continuation keeps its line number on the wire"
    );
    assert_eq!(nums[3], Some(2));
    // Past the buffer: `~` filler rows are neither numbered nor continuations.
    let last = cont.len() - 1;
    assert!(!cont[last], "a `~` filler is not a continuation");
    assert_eq!(nums[last], None, "a `~` filler carries no number");
}

#[tokio::test]
async fn g0_g_caret_g_dollar_move_within_the_display_row() {
    // `g0`/`g^`/`g$` are the within-row siblings of `gj`/`gk`: they move to the
    // first column / first non-blank / last column of the cursor's *display* row,
    // bounded by its soft-wrap segment rather than the whole buffer line.
    let (rpc, _incoming) = start(None).await; // bound to keep the RPC alive
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>",
    );
    feed(&rpc, "i");
    // "  " + 198 'a's: wraps to [0,80) [80,160) [160,200). The second segment is
    // all 'a's; the first has two leading blanks.
    feed(&rpc, &format!("{}{}", "  ", "a".repeat(198)));
    feed(&rpc, "<Esc>");
    let _ = lines(&rpc).await;

    // Land mid-second-segment, then snap to its edges.
    feed(&rpc, "gg0gj10l"); // second display row (col 80) + 10 → col 90
    assert_eq!(cursor(&rpc).await, (1, 90), "precondition: mid second row");
    feed(&rpc, "g0");
    assert_eq!(cursor(&rpc).await, (1, 80), "g0 → first column of the row");
    feed(&rpc, "g$");
    assert_eq!(cursor(&rpc).await, (1, 159), "g$ → last column of the row");

    // On the first display row, g^ skips the two leading blanks; g0 does not.
    feed(&rpc, "gg5l"); // first display row
    feed(&rpc, "g0");
    assert_eq!(cursor(&rpc).await, (1, 0), "g0 → column 0 (with blanks)");
    feed(&rpc, "g^");
    assert_eq!(
        cursor(&rpc).await,
        (1, 2),
        "g^ → first non-blank of the row"
    );

    // With `nowrap`, g0/g$/g^ collapse to plain 0/$/^ over the whole line.
    feed(&rpc, ":set nowrap<CR>gg");
    feed(&rpc, "g$");
    assert_eq!(cursor(&rpc).await, (1, 199), "nowrap g$ == $ (line end)");
    feed(&rpc, "g0");
    assert_eq!(cursor(&rpc).await, (1, 0), "nowrap g0 == 0");
}

#[tokio::test]
async fn scroll_band_carries_diagnostic_arrays() {
    // Diagnostic underlines and signs ride the scroll band now (they were settle-only):
    // `project_band` mirrors `window_value`, so the band carries a per-row
    // `diagnostics` and `diagnostics_signs` array aligned with its rows. Without a
    // language server the spans are empty, but the arrays must be present and
    // row-aligned so the client paints them frame by frame as they slide.
    let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    let path = write_temp("scrolldiag", "txt", &body);
    let (rpc, mut incoming) = start(Some(path)).await;
    let _ = lines(&rpc).await; // barrier
    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;

    let band_len = scroll_lines_len(&map);
    assert!(band_len > 0, "the band over-scans at least the viewport");
    assert_eq!(
        scroll_array_len(&map, "diagnostics"),
        Some(band_len),
        "diagnostic underlines ride the band, aligned with its rows"
    );
    assert_eq!(
        scroll_array_len(&map, "diagnostics_signs"),
        Some(band_len),
        "diagnostic signs ride the band, aligned with its rows"
    );
}

#[tokio::test]
async fn client_set_diagnostics_paint_without_a_server() {
    // `vim.diagnostic.set` — a pure-Lua plugin's own diagnostics, with no LSP
    // server attached — must reach the three render surfaces (underline span,
    // gutter sign, inline virtual text), not just the `vim.diagnostic.get`
    // read-back. The `col`/`end_col` are native byte columns (encoding-free).
    let path = write_temp("clientdiag", "txt", "hello world\nsecond line\n");
    let (rpc, mut incoming) = start(Some(path)).await;
    let _ = lines(&rpc).await; // barrier

    exec_lua(
        &rpc,
        r#"
        vim.diagnostic.config({ underline = true, virtual_text = true, signs = true })
        vim.diagnostic.set(1, 0, {
          { lnum = 0, col = 0, end_lnum = 0, end_col = 5, severity = 1, message = "boom" },
        })
        "#,
    )
    .await;

    let view = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    // Underline: row 0 carries one [0,5) span at severity 1 (error).
    let diags = view_diag_spans(&view);
    assert_eq!(
        diags.first().cloned().unwrap_or_default(),
        vec![(0, 5, 1)],
        "client-set underline paints on row 0: {diags:?}"
    );
    // Gutter sign: row 0 carries the error glyph.
    let signs = view_diag_signs(&view);
    assert_eq!(
        signs.first().cloned().flatten(),
        Some(("E".to_string(), 1)),
        "client-set sign paints on row 0: {signs:?}"
    );
    // Inline virtual text: row 0 carries the message at severity 1.
    let virt = view_diag_virt(&view);
    assert!(
        virt.first()
            .cloned()
            .flatten()
            .is_some_and(|(t, sev)| t.contains("boom") && sev == 1),
        "client-set virtual text paints on row 0: {virt:?}"
    );

    // `vim.diagnostic.reset` must unpaint them again (the reverse sync clears the
    // server's render store), not just empty the Lua read-back.
    exec_lua(&rpc, "vim.diagnostic.reset(1, 0)").await;
    let view = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        view_diag_spans(&view).iter().all(Vec::is_empty),
        "reset clears the underline: {:?}",
        view_diag_spans(&view)
    );
    assert!(
        view_diag_signs(&view).iter().all(Option::is_none),
        "reset clears the sign: {:?}",
        view_diag_signs(&view)
    );
}

#[tokio::test]
async fn multiple_diagnostics_on_one_line_pick_most_severe_and_keep_span_order() {
    // Guards the per-frame diagnostics index against the old per-row scan: when
    // several diagnostics start on the same line, the sign / virtual-text slot
    // must go to the most severe one (ties → leftmost column), regardless of the
    // order they were set in — and every underline span on the row must still be
    // emitted, in the order the merged list yields them. A lower-severity entry
    // is set FIRST so a naive "first match wins" bucket would pick the wrong one.
    let path = write_temp("multidiag", "txt", "hello world\nsecond line\n");
    let (rpc, mut incoming) = start(Some(path)).await;
    let _ = lines(&rpc).await; // barrier

    exec_lua(
        &rpc,
        r#"
        vim.diagnostic.config({ underline = true, virtual_text = true, signs = true })
        vim.diagnostic.set(1, 0, {
          -- severity 2 (warn) set first, spanning cols 6..11 ("world")
          { lnum = 0, col = 6, end_lnum = 0, end_col = 11, severity = 2, message = "warn here" },
          -- severity 1 (error) set second, spanning cols 0..5 ("hello")
          { lnum = 0, col = 0, end_lnum = 0, end_col = 5, severity = 1, message = "boom" },
        })
        "#,
    )
    .await;

    let view = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    // Underline: both spans paint on row 0, in merged (set) order — warn span
    // first (it was set first), then the error span.
    let diags = view_diag_spans(&view);
    assert_eq!(
        diags.first().cloned().unwrap_or_default(),
        vec![(6, 11, 2), (0, 5, 1)],
        "both diagnostics underline row 0 in merged order: {diags:?}"
    );
    // Sign + virtual text: the most severe (error, severity 1) wins the single
    // slot even though the warn was set first.
    assert_eq!(
        view_diag_signs(&view).first().cloned().flatten(),
        Some(("E".to_string(), 1)),
        "the error sign wins the row's one sign slot: {:?}",
        view_diag_signs(&view)
    );
    assert!(
        view_diag_virt(&view)
            .first()
            .cloned()
            .flatten()
            .is_some_and(|(t, sev)| t.contains("boom") && sev == 1),
        "the error message wins the row's inline slot: {:?}",
        view_diag_virt(&view)
    );
}

#[tokio::test]
async fn scroll_band_carries_search_highlights() {
    // hlsearch matches must ride the scroll band, not vanish until the slide
    // settles: the band's `search` spans cover its rows so the client paints them
    // frame by frame. Every line holds "needle" at columns 0..6.
    let body: String = (1..=100).map(|i| format!("needle {i}\n")).collect();
    let path = write_temp("scrollhl", "txt", &body);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Activate hlsearch, then scroll a half page so a band is projected.
    feed(&rpc, "/needle<CR>");
    let _ = lines(&rpc).await; // barrier: flush the search redraw
    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;

    let search = scroll_search(&map);
    let band_len = scroll_lines_len(&map);
    assert_eq!(
        search.len(),
        band_len,
        "search spans align with the band rows"
    );
    // Every band row over real buffer lines carries the one "needle" match (0..6);
    // rows past the buffer carry none.
    assert_eq!(
        search.first().cloned().unwrap_or_default(),
        vec![(0, 6)],
        "the first band row keeps its hlsearch match while sliding"
    );
    assert!(
        search.iter().take(100).all(|row| row == &vec![(0, 6)]),
        "every band row over the buffer keeps its match: {search:?}"
    );
}

#[tokio::test]
async fn visual_scroll_band_carries_the_maximal_selection_extent() {
    // A scroll in visual mode slides the selection with the text. The band must
    // carry the selection over the *maximal* extent the slide touches — anchor →
    // the scroll endpoint furthest from the anchor — so the client can grow it
    // (scrolling away from the anchor) *and* shrink it (scrolling back) by clipping
    // to the interpolated cursor. Projecting only the destination cursor's extent
    // would collapse the band to the *small* end while shrinking, and the rows the
    // cursor sweeps back across would flash instead of sliding.
    let path = write_n_lines("vsel", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Linewise visual from line 0 (anchor), then <C-d>: cursor + viewport 0 -> 12.
    feed(&rpc, "V");
    let down = scroll_after(&rpc, &mut incoming, "<C-d>").await;
    assert_eq!(scroll_u64(&down, "from_cursor"), 0);
    assert_eq!(scroll_u64(&down, "to_cursor"), 12);
    // Anchor (line 0) is above the cursor, so the selection extends downward.
    assert_eq!(scroll_sel_extends_down(&down), Some(true));
    // Band (base_line 0) selects lines 0..=12 — the full extent — and nothing past.
    let sel = scroll_selection(&down);
    assert!(sel[0].is_some(), "line 0 (the anchor) is selected");
    assert!(sel[12].is_some(), "line 12 (the cursor) is selected");
    assert!(sel[13].is_none(), "line 13 past the cursor is not selected");

    // Now <C-u> shrinks the selection back toward the anchor: cursor 12 -> 0. The
    // furthest endpoint from the anchor is the *source* cursor (12), so the band
    // still carries lines 0..=12 — the rows the cursor sweeps back across stay in
    // the band to be revealed/hidden per frame, rather than collapsing to line 0.
    let up = scroll_after(&rpc, &mut incoming, "<C-u>").await;
    assert_eq!(scroll_u64(&up, "from_cursor"), 12);
    assert_eq!(scroll_u64(&up, "to_cursor"), 0);
    assert_eq!(scroll_sel_extends_down(&up), Some(true));
    let sel = scroll_selection(&up);
    assert!(
        sel[12].is_some(),
        "the band keeps the maximal (source) extent while shrinking, not just the destination line 0"
    );
}

#[tokio::test]
async fn search_next_animates_the_jump_to_an_offscreen_match() {
    // `n` is a navigation, so a jump to an off-screen match scrolls the viewport
    // and animates — like the explicit scrolls. (The committed `/pattern<CR>` runs
    // in command mode and is left crisp; only `n`/`N` from normal mode animate.)
    let body: String = (0..100)
        .map(|i| if i == 4 || i == 89 { "needle\n" } else { "x\n" })
        .collect();
    let path = write_temp("nsearch", "txt", &body);
    let (rpc, mut incoming) = start(Some(path)).await;

    // First match is on line 5 (on screen); the committed search itself isn't animated.
    let _ = redraw_after(&rpc, &mut incoming, "/needle<CR>").await;
    // `n` jumps to the far match on line 90 — off screen, so the slide animates.
    let map = scroll_after(&rpc, &mut incoming, "n").await;
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        89,
        "n lands on line 90 (index 89)"
    );
}

#[tokio::test]
async fn jumplist_navigation_animates_the_scroll() {
    // `<C-o>`/`<C-i>` walk the jumplist; landing off screen scrolls and animates.
    let path = write_n_lines("jumpscroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `G` jumps to the last line (recording line 1) and scrolls there.
    let _ = scroll_after(&rpc, &mut incoming, "G").await;
    // `<C-o>` returns to the pre-jump line 1 — a jump back up the file, animated.
    let back = scroll_after(&rpc, &mut incoming, "<C-o>").await;
    assert_eq!(scroll_u64(&back, "to_cursor"), 0, "<C-o> returns to line 1");
    // `<C-i>` walks forward again to the last line, animated.
    let fwd = scroll_after(&rpc, &mut incoming, "<C-i>").await;
    assert_eq!(
        scroll_u64(&fwd, "to_cursor"),
        99,
        "<C-i> returns to the last line"
    );
}

#[tokio::test]
async fn zz_centers_the_cursor_line() {
    // `zz` parks the cursor's line in the middle of the window without moving the
    // cursor. Window height 24 → the line sits `height/2 == 12` rows from the top.
    let path = write_n_lines("zz", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `50G` lands the cursor on line 50 (index 49) at the bottom of the screen.
    let _ = scroll_after(&rpc, &mut incoming, "50G").await;
    let map = scroll_after(&rpc, &mut incoming, "zz").await;

    assert_eq!(scroll_u64(&map, "to_top"), 37, "line 49 centered: 49 - 12");
    assert_eq!(scroll_u64(&map, "from_cursor"), 49);
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        49,
        "zz leaves the cursor put"
    );
    assert_eq!(cursor(&rpc).await, (50, 0), "cursor stays on line 50");
}

#[tokio::test]
async fn zt_puts_the_cursor_line_at_the_top() {
    // `zt` scrolls so the cursor's line becomes the top visible row.
    let path = write_n_lines("zt", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let _ = scroll_after(&rpc, &mut incoming, "50G").await;
    let map = scroll_after(&rpc, &mut incoming, "zt").await;

    assert_eq!(scroll_u64(&map, "to_top"), 49, "line 49 to the top row");
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        49,
        "zt leaves the cursor put"
    );
    // The settled viewport shows line 50 on the first row.
    let settled = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(first_visible_line(&settled), "line50");
}

#[tokio::test]
async fn zb_puts_the_cursor_line_at_the_bottom() {
    // `zb` scrolls so the cursor's line becomes the bottom visible row.
    let path = write_n_lines("zb", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Center first (top 37) so there is room to scroll the line down to the bottom.
    let _ = scroll_after(&rpc, &mut incoming, "50G").await;
    let _ = scroll_after(&rpc, &mut incoming, "zz").await;
    let map = scroll_after(&rpc, &mut incoming, "zb").await;

    // Bottom row = top + height - 1, so top = 49 + 1 - 24 = 26.
    assert_eq!(scroll_u64(&map, "to_top"), 26, "line 49 to the bottom row");
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        49,
        "zb leaves the cursor put"
    );
}

#[tokio::test]
async fn z_enter_tops_the_line_and_moves_to_first_non_blank() {
    // `z<CR>` is `zt` that also drops the cursor on the line's first non-blank,
    // unlike `zt`/`zz`/`zb` which keep the column. Build a buffer whose line 50 is
    // indented so the two behaviors are distinguishable.
    let body: String = (1..=100)
        .map(|i| {
            if i == 50 {
                "    indented\n".to_string()
            } else {
                format!("line{i}\n")
            }
        })
        .collect();
    let path = write_temp("zcr", "txt", &body);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Sit the cursor on the indented line at column 0, then `z<CR>`.
    let _ = scroll_after(&rpc, &mut incoming, "50G0").await;
    let map = scroll_after(&rpc, &mut incoming, "z<CR>").await;

    assert_eq!(
        scroll_u64(&map, "to_top"),
        49,
        "z<CR> tops the line like zt"
    );
    assert_eq!(
        cursor(&rpc).await,
        (50, 4),
        "z<CR> moves to the first non-blank (column 4)"
    );
}

#[tokio::test]
async fn z_dot_centers_the_line_and_moves_to_first_non_blank() {
    // `z.` is `zz` plus the first-non-blank jump.
    let body: String = (1..=100)
        .map(|i| {
            if i == 50 {
                "  two\n".to_string()
            } else {
                format!("line{i}\n")
            }
        })
        .collect();
    let path = write_temp("zdot", "txt", &body);
    let (rpc, mut incoming) = start(Some(path)).await;

    let _ = scroll_after(&rpc, &mut incoming, "50G0").await;
    let map = scroll_after(&rpc, &mut incoming, "z.").await;

    assert_eq!(scroll_u64(&map, "to_top"), 37, "z. centers like zz");
    assert_eq!(
        cursor(&rpc).await,
        (50, 2),
        "z. moves to the first non-blank (column 2)"
    );
}

#[tokio::test]
async fn counted_zt_targets_the_given_line() {
    // `{count}zt` moves the cursor to line {count} (1-based) and tops it — vim's
    // `{count}z<CR>`/`zt`. From the file head, `30zt` jumps to line 30 and scrolls.
    let path = write_n_lines("countzt", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "30zt").await;

    assert_eq!(scroll_u64(&map, "to_cursor"), 29, "cursor jumps to line 30");
    assert_eq!(scroll_u64(&map, "to_top"), 29, "line 30 to the top row");
    assert_eq!(cursor(&rpc).await, (30, 0));
}

#[tokio::test]
async fn zz_in_visual_mode_keeps_the_selection() {
    // The z-family works in visual mode: it repositions the view and leaves the
    // selection (and visual mode) intact.
    let path = write_n_lines("zzvis", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "V"); // linewise visual from line 1
    let _ = scroll_after(&rpc, &mut incoming, "50G").await; // extend selection to line 50
    let map = scroll_after(&rpc, &mut incoming, "zz").await;

    assert_eq!(scroll_u64(&map, "to_top"), 37, "zz centers in visual too");
    assert_eq!(
        mode(&rpc).await,
        "V",
        "still in linewise visual mode after zz"
    );
}

#[tokio::test]
async fn changelist_navigation_animates_the_scroll() {
    // `g;` steps to an older change; when it's off screen the viewport slides.
    let path = write_n_lines("changescroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // A change low in the file, then one near the bottom (leaving the cursor there).
    feed(&rpc, "5Gx");
    feed(&rpc, "90Gx");
    // `g;` jumps back to the older change on line 5 — off screen now, so it animates.
    let map = scroll_after(&rpc, &mut incoming, "g;").await;
    assert_eq!(
        scroll_u64(&map, "to_cursor"),
        4,
        "g; lands on the line-5 change"
    );
}

#[tokio::test]
async fn page_down_acts_like_ctrl_d() {
    let path = write_n_lines("pgdn", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "<PageDown>").await;

    // Identical to <C-d>: viewport height 24 → half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 12);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
}

#[tokio::test]
async fn page_up_acts_like_ctrl_u() {
    let path = write_n_lines("pgup", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Scroll down a full page first so there's room to scroll back up.
    let _ = redraw_after(&rpc, &mut incoming, "<C-f>").await; // top 0 -> 22
    let map = scroll_after(&rpc, &mut incoming, "<PageUp>").await; // top 22 -> 10

    // Identical to <C-u>: half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 22);
    assert_eq!(scroll_u64(&map, "to_top"), 10);
    assert_eq!(scroll_u64(&map, "from_cursor"), 22);
    assert_eq!(scroll_u64(&map, "to_cursor"), 10);
}

#[tokio::test]
async fn ctrl_f_emits_full_page_scroll() {
    let path = write_n_lines("cf", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "<C-f>").await;

    // Full page = height - 2 = 22.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 22);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160); // 22*8=176, clamped to 160
    assert_eq!(scroll_lines_len(&map), 46); // 22 + 24
}

#[tokio::test]
async fn noscrollanim_snaps_without_a_scroll_gesture() {
    let path = write_n_lines("noscrollanim", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Turn the animation off — the viewport must still scroll (to_top = 12), but
    // the redraw carries no `scroll` descriptor, so every client snaps there.
    feed(&rpc, ":set noscrollanim<CR>");
    let map = redraw_after(&rpc, &mut incoming, "<C-d>").await;

    assert!(
        scroll(&map).is_none(),
        "noscrollanim must emit no scroll gesture"
    );
    assert_eq!(
        first_visible_line(&map),
        "line13",
        "the viewport still scrolled half a page (top 0 -> 12)"
    );
}

#[tokio::test]
async fn window_local_scrollanim_off_snaps_only_that_window() {
    // `'scrollanim'` is a per-window override of the global: `vim.wo.scrollanim = false`
    // makes the focused window's `<C-d>` snap (no gesture) even though the global is on
    // — the seam the side-by-side diff uses so a synced scroll doesn't desync (only the
    // focused pane can animate, so a mirrored pane jumping while it slides looks wrong).
    let path = write_n_lines("wsca", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    exec_lua(&rpc, "vim.wo.scrollanim = false").await;
    let _ = lines(&rpc).await; // barrier: the window option lands before the scroll

    // The override reads back through the window mirror (`vim.wo` is the resolved value).
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.scrollanim").await.as_bool(),
        Some(false),
        "the window-local override reads back"
    );

    let map = redraw_after(&rpc, &mut incoming, "<C-d>").await;
    assert!(
        scroll(&map).is_none(),
        "a window with scrollanim off snaps even though the global is on"
    );
}

#[tokio::test]
async fn window_local_scrollanim_on_overrides_global_off() {
    // The override cuts both ways: with the global off, `vim.wo.scrollanim = true` forces
    // the focused window to slide — proving it's a true per-window override, not just an
    // off-switch.
    let path = write_n_lines("wsca2", 100);
    let (rpc, mut incoming) = start(Some(path)).await;
    feed(&rpc, ":set noscrollanim<CR>");
    exec_lua(&rpc, "vim.wo.scrollanim = true").await;
    let _ = lines(&rpc).await; // barrier

    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;
    assert!(
        scroll(&map).is_some(),
        "a window with scrollanim on slides even though the global is off"
    );
}

#[tokio::test]
async fn scrollanimduration_caps_the_slide() {
    let path = write_n_lines("scad-cap", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // <C-f> travels 22 lines → the default would be 22*8=176 clamped to 160; with
    // a 100ms ceiling it clamps to 100 instead.
    feed(&rpc, ":set scrollanimduration=100<CR>");
    let map = scroll_after(&rpc, &mut incoming, "<C-f>").await;

    assert_eq!(scroll_u64(&map, "to_top"), 22, "still scrolls a full page");
    assert_eq!(scroll_u64(&map, "duration_ms"), 100);
}

#[tokio::test]
async fn scrollanimduration_below_the_floor_collapses_to_a_fixed_slide() {
    let path = write_n_lines("scad-floor", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // With a ceiling under the usual 80ms floor, the floor drops to the ceiling so
    // even a short <C-d> (12*8=96) settles at the fixed cap rather than an empty
    // [80, 50] range.
    feed(&rpc, ":set scrollanimduration=50<CR>");
    let map = scroll_after(&rpc, &mut incoming, "<C-d>").await;

    assert_eq!(scroll_u64(&map, "duration_ms"), 50);
}

#[tokio::test]
async fn scrollanimduration_zero_disables_animation() {
    let path = write_n_lines("scad-zero", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // A zero ceiling is equivalent to `noscrollanim`: the viewport jumps with no
    // `scroll` descriptor.
    feed(&rpc, ":set scrollanimduration=0<CR>");
    let map = redraw_after(&rpc, &mut incoming, "<C-d>").await;

    assert!(
        scroll(&map).is_none(),
        "a zero duration ceiling emits no scroll gesture"
    );
    assert_eq!(first_visible_line(&map), "line13");
}

#[tokio::test]
async fn scrollanim_options_round_trip_through_vim_o() {
    let path = write_n_lines("scad-lua", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Defaults read back through the Lua option bridge.
    assert_eq!(
        exec_lua(&rpc, "return vim.o.scrollanim").await.as_bool(),
        Some(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.o.scrollanimduration")
            .await
            .as_u64(),
        Some(160)
    );

    // Writing the option through `vim.o` reaches the core and turns the slide off.
    exec_lua(&rpc, "vim.o.scrollanim = false").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.scrollanim").await.as_bool(),
        Some(false)
    );
    let map = redraw_after(&rpc, &mut incoming, "<C-d>").await;
    assert!(
        scroll(&map).is_none(),
        "vim.o.scrollanim = false must suppress the scroll gesture"
    );
}

#[tokio::test]
async fn ctrl_u_at_top_is_not_a_scroll() {
    let path = write_n_lines("cu", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Already at the top: top can't move up, so no slide.
    let map = redraw_after(&rpc, &mut incoming, "<C-u>").await;

    assert!(
        scroll(&map).is_none(),
        "no viewport movement → no scroll gesture"
    );
}

#[tokio::test]
async fn scroll_window_pads_past_end_of_buffer() {
    let path = write_n_lines("eof", 30);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = scroll_after(&rpc, &mut incoming, "<C-f>").await;

    assert_eq!(scroll_u64(&map, "to_top"), 22);
    assert_eq!(scroll_lines_len(&map), 46); // window length is fixed regardless of EOF
                                            // The 30-line buffer fills rows 0..30; the rest are "~".
    let s = scroll(&map).unwrap();
    let lines = s
        .iter()
        .find(|(k, _)| k.as_str() == Some("lines"))
        .unwrap()
        .1
        .as_array()
        .unwrap();
    assert_eq!(lines.last().and_then(Value::as_str), Some("~"));
}

#[tokio::test]
async fn ctrl_u_mid_buffer_scrolls_up() {
    let path = write_n_lines("cu_mid", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Scroll down a full page first so there's room to scroll back up.
    let _ = redraw_after(&rpc, &mut incoming, "<C-f>").await; // top 0 -> 22
    let map = scroll_after(&rpc, &mut incoming, "<C-u>").await; // top 22 -> 10

    assert_eq!(scroll_u64(&map, "from_top"), 22);
    assert_eq!(scroll_u64(&map, "to_top"), 10);
    assert_eq!(scroll_u64(&map, "from_cursor"), 22);
    assert_eq!(scroll_u64(&map, "to_cursor"), 10);
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8
    assert_eq!(scroll_lines_len(&map), 36); // |22 - 10| + 24
}

#[tokio::test]
async fn ctrl_e_scrolls_one_line_keeping_the_cursor_line() {
    let path = write_n_lines("ce", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "10G"); // cursor mid-screen (line 10), top still 0
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    // The window scrolled down one line, but the cursor held its buffer line —
    // the defining difference from <C-d>, which drags the cursor with the view.
    assert_eq!(first_visible_line(&map), "line2", "top moved down one line");
    assert_eq!(cursor(&rpc).await, (10, 0), "cursor stayed on its line");
}

#[tokio::test]
async fn ctrl_e_pulls_the_cursor_when_it_would_scroll_off_top() {
    let path = write_n_lines("ce_pull", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "gg"); // cursor on line 1 — the top visible row
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    // Scrolling down one line would push line 1 off the top, so the cursor is
    // pulled to the new top line (scrolloff is 0).
    assert_eq!(first_visible_line(&map), "line2");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "cursor pulled to the new top line"
    );
}

#[tokio::test]
async fn ctrl_y_scrolls_one_line_up_keeping_the_cursor_line() {
    let path = write_n_lines("cy", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "<C-f>"); // top 0 -> 22, cursor lands on line 23 (top row)
    let map = redraw_after(&rpc, &mut incoming, "<C-y>").await;

    // View scrolls back up one line; the cursor (now one row down) holds line 23.
    assert_eq!(first_visible_line(&map), "line22", "top moved up one line");
    assert_eq!(cursor(&rpc).await, (23, 0), "cursor stayed on its line");
}

#[tokio::test]
async fn ctrl_y_at_the_top_does_nothing() {
    let path = write_n_lines("cy_top", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    feed(&rpc, "gg");
    let map = redraw_after(&rpc, &mut incoming, "<C-y>").await;

    assert!(scroll(&map).is_none(), "no viewport movement at the top");
    assert_eq!(first_visible_line(&map), "line1");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn count_motion_emits_scroll() {
    let path = write_n_lines("count_j", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `30j` lands the cursor on line 30; ensure_visible drags top to 30+1-24 = 7.
    let map = scroll_after(&rpc, &mut incoming, "30j").await;

    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 7);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 30);
    assert_eq!(scroll_u64(&map, "duration_ms"), 80); // 7*8=56, clamped up to 80
    assert_eq!(scroll_lines_len(&map), 31); // |7 - 0| + 24
}

#[tokio::test]
async fn g_to_last_line_emits_capped_scroll() {
    let path = write_n_lines("big_g", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // `G` jumps to line 99; top settles at 99+1-24 = 76. The raw travel is 76
    // lines, but it's capped to two screens (2*24 = 48) so the slide stays bounded.
    let map = scroll_after(&rpc, &mut incoming, "G").await;

    assert_eq!(scroll_u64(&map, "from_top"), 28); // 76 - 48 (cap)
    assert_eq!(scroll_u64(&map, "to_top"), 76);
    assert_eq!(scroll_u64(&map, "from_cursor"), 51); // 99 - 48 (cap)
    assert_eq!(scroll_u64(&map, "to_cursor"), 99);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160); // 48*8=384, clamped to 160
    assert_eq!(scroll_lines_len(&map), 72); // 48 + 24
}

#[tokio::test]
async fn gg_back_to_top_emits_capped_scroll() {
    let path = write_n_lines("gg", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let _ = redraw_after(&rpc, &mut incoming, "G").await; // jump to the bottom first
    let map = scroll_after(&rpc, &mut incoming, "gg").await; // ...then back to the top

    assert_eq!(scroll_u64(&map, "from_top"), 48); // 0 + 48 (cap)
    assert_eq!(scroll_u64(&map, "to_top"), 0);
    assert_eq!(scroll_u64(&map, "from_cursor"), 48);
    assert_eq!(scroll_u64(&map, "to_cursor"), 0);
    assert_eq!(scroll_u64(&map, "duration_ms"), 160);
    assert_eq!(scroll_lines_len(&map), 72);
}

#[tokio::test]
async fn single_line_edge_scroll_is_not_animated() {
    let path = write_n_lines("edge", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    // Move to the last visible row (line 23) without scrolling, then step one
    // line further: the viewport nudges by exactly one line, which must stay
    // crisp rather than animate — otherwise held `j`/`k` would feel laggy.
    let _ = redraw_after(&rpc, &mut incoming, "23j").await;
    let map = redraw_after(&rpc, &mut incoming, "j").await;

    assert!(
        scroll(&map).is_none(),
        "a one-line viewport shift must carry no scroll gesture"
    );
}

#[tokio::test]
async fn sleep_blocks_the_editor_for_the_requested_duration() {
    let (rpc, _incoming) = start(None).await;
    // The command is acknowledged promptly; the server then sleeps. The next
    // request can only be handled once the sleep finishes, so its round-trip
    // time is a reliable *lower bound* on the sleep (lower bounds never flake).
    rpc.request("nx_command", vec![Value::from("sleep 150m")])
        .await
        .expect("sleep command");
    let begin = std::time::Instant::now();
    let _ = lines(&rpc).await;
    assert!(
        begin.elapsed() >= std::time::Duration::from_millis(120),
        "follow-up returned too soon: {:?}",
        begin.elapsed()
    );
}

/// `'fillchars'` `eob` is the char drawn on screen rows past the end of the buffer
/// (vim's `~`). By default those filler rows show `~`; setting `eob` to a space
/// (`:set fillchars=eob:\ `) blanks them — the marker the user asked to hide.
#[tokio::test]
async fn fillchars_eob_blanks_the_end_of_buffer_tilde() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    let _ = lines(&rpc).await; // barrier so the redraw is buffered
    let view = latest_view(&mut incoming).expect("a redraw view");
    // One short buffer line; every screen row past it is a `~` filler by default.
    let rows = view_lines(&view);
    assert!(
        rows.iter().any(|r| r == "~"),
        "default fillchars draws `~` end-of-buffer markers, got {rows:?}"
    );

    // `:set fillchars=eob:\ ` (an escaped space) replaces the filler char with a
    // space: no more `~`, and the filler rows render blank instead.
    let view = redraw_after(&rpc, &mut incoming, ":set fillchars=eob:\\ <CR>").await;
    let rows = view_lines(&view);
    assert!(
        !rows.iter().any(|r| r == "~"),
        "fillchars eob:<space> removes the `~` markers, got {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r == " "),
        "blanked filler rows render as a single space, got {rows:?}"
    );
}

/// A different `eob` char (not just a blank) is honored too, and a bad `'fillchars'`
/// value fails loud (E474) rather than silently sticking the window on junk.
#[tokio::test]
async fn fillchars_eob_custom_char_and_invalid_value() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihi<Esc>");

    let view = redraw_after(&rpc, &mut incoming, ":set fillchars=eob:%<CR>").await;
    let rows = view_lines(&view);
    assert!(
        rows.iter().any(|r| r == "%") && !rows.iter().any(|r| r == "~"),
        "fillchars eob:% draws `%` fillers, not `~`, got {rows:?}"
    );

    // An unknown key is rejected loudly; the window keeps its prior value (`%`).
    let view = redraw_after(&rpc, &mut incoming, ":set fillchars=bogus:x<CR>").await;
    assert_eq!(message(&view), "E474: Invalid argument: fillchars=bogus:x");
    let rows = view_lines(&view);
    assert!(
        rows.iter().any(|r| r == "%"),
        "a rejected fillchars value leaves the prior `%` filler intact, got {rows:?}"
    );
}

#[tokio::test]
async fn wo_wrap_funnel_actually_wraps_not_just_stores() {
    // `vim.wo.wrap = true` (the `nx.wo` funnel a plugin / component uses, e.g.
    // `ctx.wo.wrap` in the plugin-manager UI) must reach the *core* and soft-wrap
    // the line, not merely store `true` in the Lua-side mirror. The regression: a
    // window option missing from the wired set (`WIN_OPT_CANON`) fell through to the
    // observable-only `nx._wo_store`, so the read-back said `true` while the display
    // never wrapped — exactly the "loads ≠ works" trap a value-only assert misses.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set nonumber<CR>:set norelativenumber<CR>");
    feed(&rpc, "i");
    feed(&rpc, &"a".repeat(200)); // 200 columns, wider than the 80-cell text area
    feed(&rpc, "<Esc>");
    // Set wrap via the funnel (NOT `:set wrap`), then force a fresh redraw.
    exec_lua(&rpc, "vim.wo.wrap = true").await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    // The read-back reflects the core (the mirror now carries `wrap`)...
    assert_eq!(
        exec_lua(&rpc, "return vim.wo.wrap").await,
        Value::Boolean(true),
        "vim.wo.wrap reads back true"
    );
    // ...and, the real point, the line is laid across display rows.
    let lines = view_lines(&map);
    assert_eq!(
        lines[0].chars().count(),
        80,
        "the long line wraps at the text-area width (was 200 — panned, unwrapped — before the fix)"
    );
    assert_eq!(lines[1].chars().count(), 80, "second wrap segment");
    assert_eq!(lines[2], "a".repeat(40), "remainder on the third row");
}
