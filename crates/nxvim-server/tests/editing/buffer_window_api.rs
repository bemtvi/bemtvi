use crate::support::*;

// ----- Phase 6: buffer / window Lua API ----------------------------------
//
// These drive the *Lua* buffer API (`vim.api.nvim_buf_*` / `nvim_win_get_cursor`
// / `vim.bo`) through `nvim_exec_lua`, which reads the Rust→Lua mirror the server
// refreshes before the eval. The native RPC `nvim_buf_get_lines` (`lines`) reads
// the real editor directly, so it independently confirms the queued mutation
// reached the rope, not just the Lua-side write-through.

#[tokio::test]
async fn buf_set_lines_then_get_lines_round_trips_within_one_chunk() {
    let (rpc, _incoming) = start(None).await;
    // Write-through must agree with the eventual real apply: set then get in one
    // chunk reads the Lua mirror; `lines` then proves the rope caught up.
    let got = exec_lua(
        &rpc,
        r#"
        vim.api.nvim_buf_set_lines(0, 0, -1, false, {"alpha", "beta", "gamma"})
        return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), "\n")
        "#,
    )
    .await;
    assert_eq!(got.as_str(), Some("alpha\nbeta\ngamma"));
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta", "gamma"]);
}

#[tokio::test]
async fn buf_get_lines_honors_negative_and_ranged_indices() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"a", "b", "c", "d", "e"})"#,
    )
    .await;
    let last = exec_lua(
        &rpc,
        r#"return table.concat(vim.api.nvim_buf_get_lines(0, -2, -1, false), ",")"#,
    )
    .await;
    assert_eq!(last.as_str(), Some("e"), "(-2,-1) is the last line");
    let mid = exec_lua(
        &rpc,
        r#"return table.concat(vim.api.nvim_buf_get_lines(0, 1, 3, false), ",")"#,
    )
    .await;
    assert_eq!(mid.as_str(), Some("b,c"), "(1,3) is end-exclusive");
}

#[tokio::test]
async fn buf_set_lines_append_replace_all_and_delete() {
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"one", "two", "three"})"#,
    )
    .await;
    // Append after the last line.
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, -1, -1, false, {"four"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["one", "two", "three", "four"]);
    // Delete the first line (empty replacement).
    exec_lua(&rpc, r#"vim.api.nvim_buf_set_lines(0, 0, 1, false, {})"#).await;
    assert_eq!(lines(&rpc).await, vec!["two", "three", "four"]);
    // Replace everything.
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"only"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["only"]);
}

#[tokio::test]
async fn buf_set_lines_on_a_fresh_empty_buffer() {
    let (rpc, _incoming) = start(None).await;
    // A fresh [No Name] buffer is [""]. Inserting at (0,0) keeps the empty line…
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, 0, false, {"first"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["first", ""]);
    // …while (0,-1) replaces through the last real line (the phantom-newline guard).
    let (rpc, _incoming) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"first"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["first"]);
}

#[tokio::test]
async fn buf_set_lines_reflected_in_the_rendered_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iuntouched<Esc>");
    exec_lua(
        &rpc,
        r#"vim.api.nvim_buf_set_lines(0, 0, -1, false, {"scripted edit"})"#,
    )
    .await;
    assert_eq!(lines(&rpc).await, vec!["scripted edit"]);
}

#[tokio::test]
async fn win_get_cursor_reflects_the_real_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar baz<Esc>");
    // Land somewhere unambiguous: line 2, a few columns in.
    feed(&rpc, "gg0jll");
    let pos = exec_lua(
        &rpc,
        r#"local c = vim.api.nvim_win_get_cursor(0); return c[1] * 1000 + c[2]"#,
    )
    .await;
    // Row 2 (1-based), column 2 (0-based).
    assert_eq!(pos.as_u64(), Some(2 * 1000 + 2));
}

#[tokio::test]
async fn get_current_win_is_the_single_window_handle() {
    let (rpc, _incoming) = start(None).await;
    // Phase 5: window handles are the editor's real ids (the first window is 1),
    // and the current window is among those `nvim_list_wins` reports.
    let win = exec_lua(&rpc, r#"return vim.api.nvim_get_current_win()"#).await;
    assert_eq!(win.as_u64(), Some(1));
    let listed = exec_lua(
        &rpc,
        r#"local w = vim.api.nvim_list_wins(); return #w == 1 and w[1] or -1"#,
    )
    .await;
    assert_eq!(
        listed.as_u64(),
        Some(1),
        "the one window is listed by its id"
    );
}

#[tokio::test]
async fn buf_is_loaded_is_true_for_open_and_false_for_unknown() {
    let (rpc, _incoming) = start(None).await;
    let open = exec_lua(
        &rpc,
        r#"return vim.api.nvim_buf_is_loaded(vim.api.nvim_get_current_buf())"#,
    )
    .await;
    assert_eq!(open.as_bool(), Some(true));
    let unknown = exec_lua(&rpc, r#"return vim.api.nvim_buf_is_loaded(9999)"#).await;
    assert_eq!(unknown.as_bool(), Some(false));
}

#[tokio::test]
async fn bo_option_write_is_observable_and_filetype_still_resolves() {
    let (rpc, _incoming) = start(None).await;
    // A write to the per-buffer option store reads back.
    let stored = exec_lua(&rpc, r#"vim.bo.shiftwidth = 2; return vim.bo.shiftwidth"#).await;
    assert_eq!(stored.as_u64(), Some(2));
    // nvim_set_option_value lands in the same store.
    let via_api = exec_lua(
        &rpc,
        r#"vim.api.nvim_set_option_value("tabstop", 8, { buf = 0 }); return vim.bo.tabstop"#,
    )
    .await;
    assert_eq!(via_api.as_u64(), Some(8));
}

#[tokio::test]
async fn vim_o_global_option_reaches_core_search() {
    let (rpc, _incoming) = start(None).await;
    // A global search option set through vim.o must reach the core, not just a
    // Lua table: with ignorecase on, a lowercase pattern matches uppercase text.
    feed(&rpc, "iaXYZb<Esc>0");
    exec_lua(&rpc, r#"vim.o.ignorecase = true"#).await;
    feed(&rpc, "/xyz<CR>");
    // The match "XYZ" sits at byte column 1; the cursor jumps there only because
    // ignorecase reached the editor (off, "xyz" never matches and it stays at 0).
    assert_eq!(cursor(&rpc).await, (1, 1));
}

#[tokio::test]
async fn vim_o_global_read_reflects_set_ex_command() {
    let (rpc, _incoming) = start(None).await;
    // Reading vim.o reflects the core's value, including one set via the `:set`
    // ex path (the server-pushed mirror, not just a Lua write-through).
    feed(&rpc, ":set ignorecase<CR>");
    let via_o = exec_lua(&rpc, r#"return vim.o.ignorecase"#).await;
    assert_eq!(via_o.as_bool(), Some(true));
    // The abbreviation resolves to the same canonical option.
    let via_abbrev = exec_lua(&rpc, r#"return vim.o.ic"#).await;
    assert_eq!(via_abbrev.as_bool(), Some(true));
}

#[tokio::test]
async fn vim_o_window_option_routes_to_current_window() {
    let (rpc, _incoming) = start(None).await;
    // vim.o forwards a window-local option to the current window: the write must
    // reach the core, observed by reading it back through vim.wo in a fresh chunk
    // (which reads the server-refreshed window mirror).
    exec_lua(&rpc, r#"vim.o.number = false"#).await;
    let via_wo = exec_lua(&rpc, r#"return vim.wo.number"#).await;
    assert_eq!(via_wo.as_bool(), Some(false));
}

#[tokio::test]
async fn vim_o_buffer_option_reaches_core_indent() {
    let (rpc, _incoming) = start(None).await;
    // vim.o forwards a buffer-local option to the current buffer: tabstop set
    // through vim.o drives the width expandtab fills to.
    exec_lua(&rpc, r#"vim.o.tabstop = 2"#).await;
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "i<Tab>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["  x"]);
}

#[tokio::test]
async fn vim_o_unwired_option_round_trips_observably() {
    let (rpc, _incoming) = start(None).await;
    // An option the core does not yet honor stays observable: it round-trips
    // through the plain store, and the seeded defaults read back.
    let tgc = exec_lua(
        &rpc,
        r#"vim.o.termguicolors = true; return vim.o.termguicolors"#,
    )
    .await;
    assert_eq!(tgc.as_bool(), Some(true));
    let bg = exec_lua(&rpc, r#"return vim.o.background"#).await;
    assert_eq!(bg.as_str(), Some("dark"));
}

#[tokio::test]
async fn expandtab_inserts_spaces_to_the_next_tabstop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=4<CR>");
    feed(&rpc, "i<Tab>x<Esc>");
    // expandtab turns the Tab into spaces up to the next tabstop (4).
    assert_eq!(lines(&rpc).await, vec!["    x"]);
}

#[tokio::test]
async fn expandtab_aligns_a_partial_tab_to_the_next_stop() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=4<CR>");
    // From virtual column 2 ("ab"), a Tab fills only to column 4: two spaces.
    feed(&rpc, "iab<Tab>c<Esc>");
    assert_eq!(lines(&rpc).await, vec!["ab  c"]);
}

#[tokio::test]
async fn noexpandtab_inserts_a_literal_tab() {
    let (rpc, _incoming) = start(None).await;
    // The default (noexpandtab) keeps a real tab character.
    feed(&rpc, "i<Tab>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["\tx"]);
}

#[tokio::test]
async fn tabstop_drives_the_screen_column_of_a_tab() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set tabstop=2<CR>");
    feed(&rpc, "i<Tab>x<Esc>");
    let _ = lines(&rpc).await;
    let view = latest_view(&mut incoming).expect("a redraw view");
    // The literal tab now expands to 2 cells (not the default 4), so 'x' sits at
    // screen column 2 while still at byte column 1.
    assert_eq!(view_u64(&view, "cursor_col"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 2);
}

#[tokio::test]
async fn default_indent_chain_resolves_to_four() {
    let (rpc, _incoming) = start(None).await;
    // Defaults: tabstop=4, shiftwidth=0 (follow tabstop), softtabstop=-1 (follow
    // shiftwidth). So a Tab's width resolves down the chain to 4 with no explicit
    // tabstop/softtabstop set — here observed through expandtab's spaces.
    feed(&rpc, ":set expandtab<CR>");
    feed(&rpc, "i<Tab>z<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    z"]);
}

#[tokio::test]
async fn softtabstop_drives_tab_independent_of_tabstop() {
    let (rpc, _incoming) = start(None).await;
    // softtabstop is the width a <Tab> keypress moves, distinct from tabstop (the
    // display width of a real tab). With sts=4 the Tab fills 4 columns even though
    // a literal tab would be 8 wide.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab>q<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    q"]);
}

#[tokio::test]
async fn softtabstop_backspace_removes_a_whole_unit() {
    let (rpc, _incoming) = start(None).await;
    // With softtabstop, <BS> right after a <Tab> deletes the whole soft-tab of
    // spaces it inserted, not one space.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab><BS>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["x"]);
}

#[tokio::test]
async fn typed_spaces_backspace_one_at_a_time() {
    let (rpc, _incoming) = start(None).await;
    // Spaces the user typed (not a <Tab>) are deleted one at a time, even though
    // softtabstop is on — only Tab-inserted whitespace collapses as a unit.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i    <BS>x<Esc>"); // four typed spaces, then one <BS>
    assert_eq!(lines(&rpc).await, vec!["   x"]);
}

#[tokio::test]
async fn typing_after_a_tab_breaks_the_soft_tab() {
    let (rpc, _incoming) = start(None).await;
    // A keystroke between the <Tab> and the <BS> ends the soft-tab window, so the
    // backspace removes just that character, leaving the tab's spaces intact.
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    feed(&rpc, "i<Tab>a<BS>b<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    b"]);
}

#[tokio::test]
async fn consecutive_tabs_backspace_unit_by_unit() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set expandtab tabstop=8 softtabstop=4<CR>");
    // Two <Tab>s build two soft-tab units (8 spaces); one <BS> peels one unit,
    // leaving four spaces.
    feed(&rpc, "i<Tab><Tab><BS>z<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    z"]);
    // On a fresh line, two <BS>s peel both units back to nothing.
    feed(&rpc, "o<Tab><Tab><BS><BS>w<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    z", "w"]);
}

#[tokio::test]
async fn set_parses_a_numeric_option_assignment() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, ":set shiftwidth=3<CR>");
    let sw = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("shiftwidth", {})"#,
    )
    .await;
    assert_eq!(sw.as_u64(), Some(3));
}

#[tokio::test]
async fn buffer_local_options_are_independent_per_buffer() {
    let (rpc, _incoming) = start(None).await;
    // A second, non-current buffer.
    let other = rpc
        .request(
            "nvim_create_buf",
            vec![Value::Boolean(true), Value::Boolean(false)],
        )
        .await
        .expect("create_buf")
        .as_u64()
        .expect("buffer id");
    // Set tabstop on the background buffer only.
    exec_lua(
        &rpc,
        &format!(r#"vim.api.nvim_set_option_value("tabstop", 2, {{ buf = {other} }})"#),
    )
    .await;
    let other_ts = exec_lua(
        &rpc,
        &format!(r#"return vim.api.nvim_get_option_value("tabstop", {{ buf = {other} }})"#),
    )
    .await;
    let cur_ts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("tabstop", {})"#,
    )
    .await;
    assert_eq!(
        other_ts.as_u64(),
        Some(2),
        "background buffer took the value"
    );
    assert_eq!(cur_ts.as_u64(), Some(4), "current buffer kept the default");
}

#[tokio::test]
async fn get_option_value_reads_the_core_default() {
    let (rpc, _incoming) = start(None).await;
    // Never set, so the read reflects the core default, not nil.
    let ts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("tabstop", {})"#,
    )
    .await;
    assert_eq!(ts.as_u64(), Some(4));
    let et = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("expandtab", {})"#,
    )
    .await;
    assert_eq!(et.as_bool(), Some(false));
    // shiftwidth defaults to 0 ("follow tabstop") and softtabstop to -1 ("follow
    // shiftwidth"), the modern follow-chain.
    let sw = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("shiftwidth", {})"#,
    )
    .await;
    assert_eq!(sw.as_i64(), Some(0));
    let sts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("softtabstop", {})"#,
    )
    .await;
    assert_eq!(sts.as_i64(), Some(-1));
}

#[tokio::test]
async fn bo_write_drives_tab_insertion() {
    let (rpc, _incoming) = start(None).await;
    // Writing vim.bo must reach the core and change how Tab indents.
    exec_lua(&rpc, r#"vim.bo.expandtab = true; vim.bo.tabstop = 4"#).await;
    feed(&rpc, "i<Tab>x<Esc>");
    assert_eq!(lines(&rpc).await, vec!["    x"]);
}

#[tokio::test]
async fn set_ex_command_is_visible_through_get_option_value() {
    let (rpc, _incoming) = start(None).await;
    // A value set via the :set ex-command path is readable back through the Lua
    // option surface (the Rust->Lua option mirror), not just the value last
    // written from Lua.
    feed(&rpc, ":set tabstop=4<CR>");
    let ts = exec_lua(
        &rpc,
        r#"return vim.api.nvim_get_option_value("tabstop", {})"#,
    )
    .await;
    assert_eq!(ts.as_u64(), Some(4));
}

#[tokio::test]
async fn buf_set_lines_targets_a_non_current_buffer() {
    let (rpc, _incoming) = start(None).await;
    // Create a second buffer (stays non-current) and edit it by id from Lua.
    let other = rpc
        .request(
            "nvim_create_buf",
            vec![Value::Boolean(true), Value::Boolean(false)],
        )
        .await
        .expect("create_buf")
        .as_u64()
        .expect("buffer id");
    exec_lua(
        &rpc,
        &format!(r#"vim.api.nvim_buf_set_lines({other}, 0, -1, false, {{"in the background"}})"#),
    )
    .await;
    // The current buffer is untouched…
    assert_eq!(lines(&rpc).await, vec![""]);
    // …and the background buffer got the edit (native RPC read, by id).
    let got = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(other),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    let got: Vec<String> = match got {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    assert_eq!(got, vec!["in the background"]);
}

#[tokio::test]
async fn buf_set_lines_strict_indexing_raises_out_of_range() {
    let (rpc, _incoming) = start(None).await;
    // pcall captures the strict-indexing error; a clamped (non-strict) call would
    // silently succeed, so this guards the fail-loud contract.
    let ok = exec_lua(
        &rpc,
        r#"return pcall(vim.api.nvim_buf_set_lines, 0, 50, 50, true, {"x"})"#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false), "strict out-of-range must error");
}
