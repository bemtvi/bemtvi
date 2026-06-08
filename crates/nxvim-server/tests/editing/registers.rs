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
async fn getreg_and_getregtype_read_the_core_register_file() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // A linewise yank into `a`; `getreg`/`getregtype` must read it from core.
    // Route the answer back through the buffer (trailing newline trimmed) so a
    // plain `lines()` assertion proves the read.
    feed(&rpc, "\"ayy");
    feed(
        &rpc,
        ":lua vim.api.nvim_buf_set_lines(0, 0, -1, false, \
         { vim.fn.getregtype('a') .. '|' .. (vim.fn.getreg('a'):gsub('%s+$', '')) })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["V|alpha"]);
}

#[tokio::test]
async fn getregtype_is_charwise_for_an_empty_register() {
    let (rpc, _incoming) = start(None).await;
    // An untouched register is charwise ("v"); its contents are "".
    feed(
        &rpc,
        ":lua vim.api.nvim_buf_set_lines(0, 0, -1, false, \
         { vim.fn.getregtype('z') .. '|' .. vim.fn.getreg('z') })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["v|"]);
}

#[tokio::test]
async fn getreg_reads_the_search_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello bar<Esc>");
    // The search sets `"/`; `getreg('/')` projects it like any read.
    feed(&rpc, "/bar<CR>");
    feed(
        &rpc,
        ":lua vim.api.nvim_buf_set_lines(0, 0, -1, false, { vim.fn.getreg('/') })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["bar"]);
}

#[tokio::test]
async fn setreg_rejects_a_read_only_register() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // Writing the read-only filename register `%` must raise, not silently
    // no-op: the pcall fails, so the buffer reads "ERR".
    feed(
        &rpc,
        ":lua local ok = pcall(vim.fn.setreg, '%', 'x'); \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, { ok and 'OK' or 'ERR' })<CR>",
    );
    assert_eq!(lines(&rpc).await, vec!["ERR"]);
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
