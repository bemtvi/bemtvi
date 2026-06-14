use crate::support::*;

#[tokio::test]
async fn yank_and_paste_duplicates_a_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "yyp");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn named_register_round_trips() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Yank the first line into register `a`, then paste it below the last line.
    feed(&rpc, "gg\"ayy");
    feed(&rpc, "G\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta", "alpha"]);
}

#[tokio::test]
async fn uppercase_register_appends() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // `"ayy` then `"Ayy` accumulates both lines in register `a`.
    feed(&rpc, "gg\"ayy");
    feed(&rpc, "j\"Ayy");
    feed(&rpc, "G\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta", "alpha", "beta"]);
}

#[tokio::test]
async fn delete_ring_shifts_through_numbered_registers() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>");
    // Two linewise deletes fill the ring: `"1` = "two", `"2` = "one".
    feed(&rpc, "ggdd");
    feed(&rpc, "dd");
    // Buffer is just ["three"]; paste `"1` then `"2` back in.
    feed(&rpc, "\"1p\"2p");
    assert_eq!(lines(&rpc).await, vec!["three", "two", "one"]);
}

#[tokio::test]
async fn small_delete_uses_the_dash_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<Esc>");
    // A within-line `x` is a *small* delete → the `"-` register, not the ring.
    feed(&rpc, "0x");
    feed(&rpc, "$\"-p");
    assert_eq!(lines(&rpc).await, vec!["elloh"]);
}

#[tokio::test]
async fn yank_register_zero_survives_an_intervening_delete() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Yank fills `"0`; a later delete fills the ring/unnamed but never `"0`.
    feed(&rpc, "ggyy");
    feed(&rpc, "jdd");
    feed(&rpc, "\"0p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn black_hole_register_leaves_unnamed_intact() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    feed(&rpc, "ggyy");
    // `"_dd` discards "beta" without clobbering the unnamed register…
    feed(&rpc, "j\"_dd");
    // …so a plain paste still yields the yanked "alpha".
    feed(&rpc, "p");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha"]);
}

#[tokio::test]
async fn register_carries_through_a_count_either_order() {
    // `"a3dd`: register before count.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>ofour<Esc>");
    feed(&rpc, "gg\"a3dd");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["four", "one", "two", "three"]);

    // `3"add`: count before register — same three-line capture.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>othree<Esc>ofour<Esc>");
    feed(&rpc, "gg3\"add");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["four", "one", "two", "three"]);
}

#[tokio::test]
async fn paste_from_the_search_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello bar<Esc>");
    // The search sets `"/`; paste it onto a fresh line.
    feed(&rpc, "/bar<CR>");
    feed(&rpc, "o<Esc>\"/p");
    assert_eq!(lines(&rpc).await, vec!["hello bar", "bar"]);
}

#[tokio::test]
async fn paste_from_the_filename_register() {
    let path = temp_path("regname");
    std::fs::write(&path, "content\n").unwrap();
    let name = path.to_string_lossy().into_owned();
    let (rpc, _incoming) = start(Some(name.clone())).await;
    // `"%` is the current file name; paste it onto a new last line.
    feed(&rpc, "Go<Esc>\"%p");
    assert_eq!(lines(&rpc).await, vec!["content", name.as_str()]);
}

#[tokio::test]
async fn registers_command_lists_populated_registers() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "\"ayy");
    let map = latest_after(&rpc, &mut incoming, ":registers<CR>").await;

    assert_eq!(panel_title(&map), "Registers");
    let lines = panel_lines(&map);
    assert_eq!(lines.first().map(String::as_str), Some("Type Name Content"));
    // The linewise yank into `a` shows the `l` type and a trailing `^J`.
    assert!(
        lines
            .iter()
            .any(|l| l.contains("\"a") && l.contains("alpha^J")),
        "registers were: {lines:?}"
    );
}

#[tokio::test]
async fn registers_command_filters_by_argument() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "\"ayy\"byy");
    let map = latest_after(&rpc, &mut incoming, ":reg a<CR>").await;

    let lines = panel_lines(&map);
    assert!(lines.iter().any(|l| l.contains("\"a")), "want a: {lines:?}");
    assert!(
        !lines.iter().any(|l| l.contains("\"b")),
        "b should be filtered out: {lines:?}"
    );
}

#[tokio::test]
async fn read_only_register_refuses_a_delete() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<Esc>otwo<Esc>");
    // `"/dd` targets the read-only search register — vim beeps and changes
    // nothing, so the buffer is untouched.
    feed(&rpc, "gg\"/dd");
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
}

// ---- Phase 4: the Lua register surface (setreg / getreg / getregtype) + :put ----

#[tokio::test]
async fn setreg_then_paste_round_trips() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // `setreg` fills register `a` from Lua; `"ap` pastes it back (charwise:
    // inserted after the cursor, which rests on the final `a` after `<Esc>`).
    feed(&rpc, ":lua vim.fn.setreg('a', 'hi')<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["alphahi"]);
}

#[tokio::test]
async fn setreg_linewise_option_pastes_as_a_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // The `l` flag makes the register linewise, so `"ap` opens a new line below.
    feed(&rpc, ":lua vim.fn.setreg('a', 'beta', 'l')<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn setreg_list_value_is_linewise() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // A list value is linewise, one item per line.
    feed(&rpc, ":lua vim.fn.setreg('a', {'one', 'two'})<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["alpha", "one", "two"]);
}

#[tokio::test]
async fn setreg_append_flag_concatenates() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix<Esc>");
    // The `a` flag appends to the register's current contents.
    feed(&rpc, ":lua vim.fn.setreg('a', 'foo')<CR>");
    feed(&rpc, ":lua vim.fn.setreg('a', 'bar', 'a')<CR>");
    feed(&rpc, "\"ap");
    assert_eq!(lines(&rpc).await, vec!["xfoobar"]);
}

#[tokio::test]
async fn put_inserts_register_below_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Yank "alpha" into `a`, move to the top, then `:put a` drops it below line 1
    // as a whole line — even though the cursor sits mid-line.
    feed(&rpc, "gg\"ayy");
    feed(&rpc, ":put a<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha", "beta"]);
}

#[tokio::test]
async fn put_bang_inserts_above_the_current_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    feed(&rpc, "gg\"ayy");
    // `:put!` inserts above the addressed line instead of below.
    feed(&rpc, "G:put! a<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "alpha", "beta"]);
}

#[tokio::test]
async fn put_of_a_charwise_register_is_still_linewise() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // A charwise register set from Lua; `:put` inserts it as a whole line, not
    // spliced into the current line.
    feed(&rpc, ":lua vim.fn.setreg('a', 'beta')<CR>");
    feed(&rpc, ":put a<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
}

// ---- The `".` last-insert register (dot-repeat plan, Phase 3) ----------------

#[tokio::test]
async fn last_insert_register_pastes_the_typed_text() {
    let (rpc, _incoming) = start(None).await;
    // `iabc<Esc>` records "abc" in `".`; `".p` pastes it after the cursor.
    feed(&rpc, "iabc<Esc>");
    feed(&rpc, "\".p"); // paste `".` after 'c' -> "abcabc"
    assert_eq!(lines(&rpc).await, vec!["abcabc"]);
}

#[tokio::test]
async fn last_insert_register_is_read_only() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>");
    // `".dd` targets the read-only last-insert register for a delete: vim aborts
    // the whole command (no bell), so the line is untouched.
    feed(&rpc, "\".dd");
    assert_eq!(lines(&rpc).await, vec!["abc"]);
}

// ---- Insert-mode `<C-r>{register}` (insert a register's contents) -----------

#[tokio::test]
async fn ctrl_r_inserts_named_register_in_insert_mode() {
    let (rpc, _incoming) = start(None).await;
    // Yank "alpha" charwise into register `a`, then on a fresh line type "x" and
    // pull the register in after it with `<C-r>a`.
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "0\"ayiw");
    feed(&rpc, "ox<C-r>a<Esc>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "xalpha"]);
}

#[tokio::test]
async fn ctrl_r_with_linewise_register_inserts_trailing_newline() {
    let (rpc, _incoming) = start(None).await;
    // A linewise register (`"ayy`) carries a trailing newline, so `<C-r>a` inserts
    // it verbatim — splitting the line after the register's contents, like vim.
    feed(&rpc, "ialpha<Esc>");
    feed(&rpc, "\"ayy");
    feed(&rpc, "ox<C-r>a<Esc>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "xalpha", ""]);
}

#[tokio::test]
async fn ctrl_r_inserts_unnamed_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    // `yiw` fills the unnamed register with "foo"; `<C-r>"` re-inserts it.
    feed(&rpc, "yiw");
    feed(&rpc, "A bar <C-r>\"<Esc>");
    assert_eq!(lines(&rpc).await, vec!["foo bar foo"]);
}

#[tokio::test]
async fn ctrl_r_lands_cursor_after_inserted_text() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab<Esc>");
    feed(&rpc, "yiw"); // unnamed = "ab"
                       // Insert at the start, pull in the register, then keep typing: the new text
                       // must land *after* the inserted register contents, not inside it.
    feed(&rpc, "I<C-r>\"X<Esc>");
    assert_eq!(lines(&rpc).await, vec!["abXab"]);
}

#[tokio::test]
async fn ctrl_r_then_esc_inserts_nothing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    feed(&rpc, "yiw");
    // `<C-r>` then `<Esc>` aborts the register pick, leaving the typed text intact
    // and no register content inserted.
    feed(&rpc, "ox<C-r><Esc>");
    assert_eq!(lines(&rpc).await, vec!["foo", "x"]);
}

#[tokio::test]
async fn ctrl_r_inserts_register_in_command_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    feed(&rpc, "0yiw"); // unnamed = "hello"
                        // In the `:` command line, `<C-r>"` pulls the unnamed register in, building
                        // `:s/hello/HI/` so the substitution runs against the yanked word.
    feed(&rpc, ":s/<C-r>\"/HI/<CR>");
    assert_eq!(lines(&rpc).await, vec!["HI world"]);
}

#[tokio::test]
async fn ctrl_r_in_search_line_finds_yanked_word() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>");
    feed(&rpc, "0yiw"); // unnamed = "foo"
                        // Search for the yanked word via `<C-r>"` in the `/` line; the cursor jumps
                        // to the next match (the second "foo").
    feed(&rpc, "/<C-r>\"<CR>");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

#[tokio::test]
async fn ctrl_r_ctrl_w_inserts_word_under_cursor_in_insert_mode() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoobar baz<Esc>");
    // Cursor back on "foobar"; `i<C-r><C-w>` pulls the word under the cursor in at
    // the insert point (no register involved).
    feed(&rpc, "0i<C-r><C-w><Esc>");
    assert_eq!(lines(&rpc).await, vec!["foobarfoobar baz"]);
}

#[tokio::test]
async fn ctrl_r_ctrl_w_inserts_word_under_cursor_in_command_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>");
    feed(&rpc, "0"); // cursor on "foo"
                     // `<C-r><C-w>` in the `:s` line pulls in the word under the buffer cursor,
                     // building `:s/foo/X/`.
    feed(&rpc, ":s/<C-r><C-w>/X/<CR>");
    assert_eq!(lines(&rpc).await, vec!["X bar"]);
}

#[tokio::test]
async fn ctrl_r_ctrl_w_in_search_finds_word_under_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>");
    feed(&rpc, "0"); // cursor on the first "foo"
                     // `/<C-r><C-w><CR>` searches for the word under the cursor — like `*`,
                     // landing on the next "foo".
    feed(&rpc, "/<C-r><C-w><CR>");
    assert_eq!(cursor(&rpc).await, (1, 8));
}

#[tokio::test]
async fn ctrl_r_insert_is_dot_repeatable() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>");
    feed(&rpc, "yiw"); // unnamed = "foo"
                       // `o<C-r>"<Esc>` opens a line holding "foo"; `.` repeats the whole insert
                       // session — register pull included — building another "foo" line.
    feed(&rpc, "o<C-r>\"<Esc>");
    feed(&rpc, ".");
    assert_eq!(lines(&rpc).await, vec!["foo", "foo", "foo"]);
}
