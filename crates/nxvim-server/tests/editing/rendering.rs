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

/// With `nowrap` (nxvim's only text-window mode today), a cursor driven past the
/// window's text width scrolls the viewport horizontally (`leftcol`) to keep the
/// cursor on screen, and scrolls all the way back at column 0 — vim's `w_leftcol`.
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

/// The shipped `examples/horizontal-scroll/` config sources cleanly and actually
/// configures the editor (not just "loads"): its `:set sidescrolloff=8` takes
/// effect, observable through `:set siso?`.
#[tokio::test]
async fn horizontal_scroll_example_config_runs() {
    let dir = temp_dir("hscroll-ex");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/horizontal-scroll/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;

    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(!msg.contains("Error"), "example left an error: {msg:?}");

    // The example's `vim.cmd("set sidescrolloff=8")` reached the core.
    let map = redraw_after(&rpc, &mut incoming, ":set siso?<CR>").await;
    assert_eq!(view_str(&map, "message"), "sidescrolloff=8");
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

    // Viewport height 24 → half page = 12.
    assert_eq!(scroll_u64(&map, "from_top"), 0);
    assert_eq!(scroll_u64(&map, "to_top"), 12);
    assert_eq!(scroll_u64(&map, "from_cursor"), 0);
    assert_eq!(scroll_u64(&map, "to_cursor"), 12);
    assert_eq!(scroll_u64(&map, "base_line"), 0);
    assert_eq!(scroll_u64(&map, "duration_ms"), 96); // 12 * 8, within [80,160]
                                                     // Window = |to-from| + height = 12 + 24.
    assert_eq!(scroll_lines_len(&map), 36);
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

/// The shipped `examples/smooth-scroll/init.lua` loads cleanly and its
/// `vim.o.scrollanimduration = 220` reaches the core — a guard so the example
/// can't silently rot.
#[tokio::test]
async fn smooth_scroll_example_config_loads_and_applies() {
    let dir = temp_dir("smooth-scroll-example");
    let init = include_str!("../../../../examples/smooth-scroll/init.lua");
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    assert_eq!(
        exec_lua(&rpc, "return vim.o.scrollanimduration")
            .await
            .as_u64(),
        Some(220),
        "the example's vim.o.scrollanimduration must reach the core"
    );
    // The :ScrollReport command the example registers exists.
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_commands({})['ScrollReport'] ~= nil"
        )
        .await
        .as_bool(),
        Some(true)
    );
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
    assert_eq!(scroll_u64(&map, "base_line"), 10); // min(from, to)
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
    assert_eq!(scroll_u64(&map, "base_line"), 0);
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
    assert_eq!(scroll_u64(&map, "base_line"), 28);
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
    assert_eq!(scroll_u64(&map, "base_line"), 0);
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
    rpc.request("nvim_command", vec![Value::from("sleep 150m")])
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
