//! Behavior tests for manual code folds (Phase 1): `zf`/`zF` create folds and the
//! `z` family opens/closes them, with the rendered view collapsing a closed fold
//! into one placeholder row. Black-box: drive vim keys over the RPC and assert on
//! the redraw `lines` / `numbers` arrays (the rendered screen), since folds change
//! what is *shown*, not the buffer (`nvim_buf_get_lines` still returns every line).

use crate::support::*;

/// Insert six numbered lines `L1..L6` and park the cursor at the top.
async fn six_lines() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    feed(&rpc, "iL1<CR>L2<CR>L3<CR>L4<CR>L5<CR>L6<Esc>gg");
    assert_eq!(
        lines(&rpc).await,
        vec!["L1", "L2", "L3", "L4", "L5", "L6"],
        "fixture buffer"
    );
    (rpc, incoming)
}

/// The visible buffer-line numbers of the focused window (dropping `~` fillers).
fn visible_numbers(map: &[(Value, Value)]) -> Vec<u64> {
    view_numbers(map).into_iter().flatten().collect()
}

/// The visible buffer-line numbers of the window carrying `focused: true` in the
/// `windows` array. Unlike [`visible_numbers`] (which falls back to `windows[0]`,
/// the first window in layout order) this follows focus — needed once a `:split`
/// makes "first" and "focused" diverge.
fn focused_numbers(map: &[(Value, Value)]) -> Vec<u64> {
    let windows = map_get(map, "windows")
        .and_then(Value::as_array)
        .expect("windows array");
    let win = windows
        .iter()
        .filter_map(|w| match w {
            Value::Map(m) => Some(m),
            _ => None,
        })
        .find(|m| {
            map_get(m, "focused")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .expect("a focused window");
    map_get(win, "numbers")
        .and_then(Value::as_array)
        .expect("numbers array")
        .iter()
        .filter_map(Value::as_u64)
        .collect()
}

#[tokio::test]
async fn zf_creates_closed_fold_that_collapses_the_range() {
    let (rpc, mut incoming) = six_lines().await;

    // `2G` to line 2, then `zf2j` folds lines 2..4 (it is created closed). The
    // cursor parks on the fold's first line (line 2).
    let map = redraw_after(&rpc, &mut incoming, "2Gzf2j").await;

    // The fold collapses lines 2-4 into one placeholder row: visible buffer lines
    // are now 1, 2 (the fold's first line), 5, 6 — lines 3 and 4 are hidden.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 5, 6],
        "lines 3-4 are hidden inside the closed fold"
    );

    let rendered = view_lines(&map);
    let fold_row = &rendered[1];
    assert!(
        fold_row.contains("3 lines") && fold_row.contains("L2"),
        "fold placeholder shows the line count and first line, got {fold_row:?}"
    );
    assert!(
        !rendered.iter().any(|l| l == "L3" || l == "L4"),
        "folded lines must not be rendered, got {rendered:?}"
    );

    // The buffer itself is untouched — folding only hides lines on screen.
    assert_eq!(
        lines(&rpc).await,
        vec!["L1", "L2", "L3", "L4", "L5", "L6"],
        "fold does not modify the buffer"
    );
}

#[tokio::test]
async fn zo_reopens_a_closed_fold() {
    let (rpc, mut incoming) = six_lines().await;
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await;

    // The cursor is on the closed fold's first line; `zo` opens it.
    let map = redraw_after(&rpc, &mut incoming, "zo").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 3, 4, 5, 6],
        "every line is visible again after zo"
    );
    let rendered = view_lines(&map);
    assert!(
        rendered.iter().any(|l| l == "L3") && rendered.iter().any(|l| l == "L4"),
        "the previously folded lines are shown, got {rendered:?}"
    );
}

#[tokio::test]
async fn za_toggles_the_fold_under_the_cursor() {
    let (rpc, mut incoming) = six_lines().await;
    // Create the fold open (via the operator that closes it), then toggle twice.
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await; // closed
    let opened = redraw_after(&rpc, &mut incoming, "za").await; // -> open
    assert_eq!(
        visible_numbers(&opened),
        vec![1, 2, 3, 4, 5, 6],
        "za opened"
    );
    let closed = redraw_after(&rpc, &mut incoming, "za").await; // -> closed
    assert_eq!(visible_numbers(&closed), vec![1, 2, 5, 6], "za re-closed");
}

#[tokio::test]
async fn zr_and_zm_open_and_close_every_fold() {
    let (rpc, mut incoming) = six_lines().await;
    // Two sibling folds: lines 1-2 (0-based 0-1) and lines 4-5 (0-based 3-4).
    feed(&rpc, "ggzfj"); // fold lines 1-2
    feed(&rpc, "4Gzfj"); // fold lines 4-5
                         // Both closed: visible lines 1, 3, 4, 6.
    let closed = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        visible_numbers(&closed),
        vec![1, 3, 4, 6],
        "both folds closed"
    );

    let all_open = redraw_after(&rpc, &mut incoming, "zR").await;
    assert_eq!(
        visible_numbers(&all_open),
        vec![1, 2, 3, 4, 5, 6],
        "zR opens every fold"
    );

    let all_closed = redraw_after(&rpc, &mut incoming, "zM").await;
    assert_eq!(
        visible_numbers(&all_closed),
        vec![1, 3, 4, 6],
        "zM closes every fold"
    );
}

#[tokio::test]
async fn foldenable_off_shows_every_line() {
    let (rpc, mut incoming) = six_lines().await;
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await; // closed fold over 2-4

    // `zn` clears 'foldenable' — the fold still exists but nothing collapses.
    let shown = redraw_after(&rpc, &mut incoming, "zn").await;
    assert_eq!(
        visible_numbers(&shown),
        vec![1, 2, 3, 4, 5, 6],
        "nofoldenable shows every line"
    );

    // `zN` restores it — the fold collapses again.
    let folded = redraw_after(&rpc, &mut incoming, "zN").await;
    assert_eq!(
        visible_numbers(&folded),
        vec![1, 2, 5, 6],
        "foldenable re-collapses the fold"
    );
}

#[tokio::test]
async fn split_inherits_a_clone_of_the_parents_folds() {
    let (rpc, mut incoming) = six_lines().await;
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await; // closed fold over 2-4

    // `:split` copies the parent's fold state to the new (focused) window, as vim
    // does — the split opens showing the same collapsed view.
    let split = redraw_after(&rpc, &mut incoming, ":split<CR>").await;
    assert_eq!(
        focused_numbers(&split),
        vec![1, 2, 5, 6],
        "the split inherits the closed fold"
    );

    // It is a *clone*: opening the fold in the split leaves the original window's
    // copy closed.
    let opened = redraw_after(&rpc, &mut incoming, "zo").await;
    assert_eq!(
        focused_numbers(&opened),
        vec![1, 2, 3, 4, 5, 6],
        "zo opens the split's copy"
    );
    let original = redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    assert_eq!(
        focused_numbers(&original),
        vec![1, 2, 5, 6],
        "the original window's fold stays closed"
    );
}

#[tokio::test]
async fn creating_a_fold_after_zn_re_enables_folding() {
    // Repro: `zn` turns folding off (everything shows), then making a fold must
    // work again — in vim `zf`/`zF` set 'foldenable' rather than staying dead.
    let (rpc, mut incoming) = six_lines().await;
    let off = redraw_after(&rpc, &mut incoming, "zn").await;
    assert_eq!(
        visible_numbers(&off),
        vec![1, 2, 3, 4, 5, 6],
        "zn shows every line"
    );

    // Creating a fold re-enables 'foldenable', so the new fold collapses.
    let after = redraw_after(&rpc, &mut incoming, "2Gzf2j").await;
    assert_eq!(
        visible_numbers(&after),
        vec![1, 2, 5, 6],
        "zf after zn re-enables folding and collapses the new fold"
    );
}

#[tokio::test]
async fn closing_a_fold_after_zn_re_enables_folding() {
    // `zc` (and the other close commands) likewise set 'foldenable' in vim.
    let (rpc, mut incoming) = six_lines().await;
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await; // fold lines 2-4 (closed)
    redraw_after(&rpc, &mut incoming, "zn").await; // folding off
                                                   // Cursor is on line 2 (the fold's first line); `zc` re-enables folding.
    let on = redraw_after(&rpc, &mut incoming, "zc").await;
    assert_eq!(
        visible_numbers(&on),
        vec![1, 2, 5, 6],
        "zc after zn re-enables folding"
    );
}

#[tokio::test]
async fn zd_deletes_the_fold_under_the_cursor() {
    let (rpc, mut incoming) = six_lines().await;
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await; // closed fold over 2-4
                                                       // `zd` removes the fold (cursor is on its first line); all lines return.
    let map = redraw_after(&rpc, &mut incoming, "zd").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 3, 4, 5, 6],
        "zd deletes the fold"
    );
}

#[tokio::test]
async fn j_steps_over_a_closed_fold() {
    let (rpc, mut incoming) = six_lines().await;
    // Fold lines 2-4 (closed); the cursor parks on line 2 (the fold header).
    redraw_after(&rpc, &mut incoming, "2Gzf2j").await;
    assert_eq!(cursor(&rpc).await.0, 2, "cursor on the fold header");

    // `j` from the fold header lands on the next visible line (line 5), counting
    // the whole collapsed range as a single step rather than entering its interior.
    feed(&rpc, "j");
    assert_eq!(cursor(&rpc).await.0, 5, "j skips over the closed fold");

    // `k` steps back onto the fold header (one visible line up).
    feed(&rpc, "k");
    assert_eq!(cursor(&rpc).await.0, 2, "k lands back on the fold header");
}

#[tokio::test]
async fn g_into_a_closed_fold_snaps_to_its_header() {
    let (rpc, mut incoming) = six_lines().await;
    // Fold the last three lines (4-6); cursor parks on line 4.
    redraw_after(&rpc, &mut incoming, "4Gzf2j").await;
    feed(&rpc, "gg"); // back to the top
    assert_eq!(cursor(&rpc).await.0, 1, "gg to the first line");

    // `G` targets the last line (6), which is hidden inside the closed fold — it
    // snaps to the fold's visible header line (4).
    feed(&rpc, "G");
    assert_eq!(
        cursor(&rpc).await.0,
        4,
        "G into a closed fold snaps to its header"
    );
}

#[tokio::test]
async fn foldcolumn_shows_markers() {
    let (rpc, mut incoming) = six_lines().await;
    // Turn on a 1-cell fold column, then create a closed fold over lines 2-4.
    let map = redraw_after(&rpc, &mut incoming, ":set foldcolumn=1<CR>2Gzf2j").await;

    let col = view_get(&map, "foldcolumn")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        })
        .expect("foldcolumn array present");
    assert_eq!(
        view_u64(&map, "foldcolumn_width"),
        1,
        "fold column is one cell wide"
    );
    // Row 0 is line 1 (no fold) → blank; row 1 is the closed fold placeholder → `+`;
    // rows 2/3 are lines 5/6 (no fold) → blank.
    assert_eq!(col[0], " ", "line 1 has no fold marker");
    assert_eq!(col[1], "+", "the closed fold shows a + marker");
    assert_eq!(col[2], " ", "line 5 has no fold marker");

    // Opening the fold flips the marker to `-` on its first line and `│` within.
    let opened = redraw_after(&rpc, &mut incoming, "zo").await;
    let ocol: Vec<String> = view_get(&opened, "foldcolumn")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap();
    assert_eq!(ocol[1], "-", "open fold's first line shows -");
    assert_eq!(ocol[2], "│", "open fold's interior shows │");
}

#[tokio::test]
async fn zj_zk_navigate_between_folds() {
    let (rpc, _incoming) = six_lines().await;
    // Two folds: lines 2-3 and lines 5-6.
    feed(&rpc, "2Gzfj");
    feed(&rpc, "5Gzfj");
    feed(&rpc, "gg"); // top, line 1

    feed(&rpc, "zj"); // to the start of the next fold (line 2)
    assert_eq!(cursor(&rpc).await.0, 2, "zj jumps to the next fold start");
    feed(&rpc, "zj"); // to the next fold's start (line 5)
    assert_eq!(cursor(&rpc).await.0, 5, "zj jumps to the following fold");
    feed(&rpc, "zk"); // back to the previous fold (line 2/3)
    assert!(
        matches!(cursor(&rpc).await.0, 2 | 3),
        "zk jumps to the previous fold"
    );
}

#[tokio::test]
async fn capital_zf_folds_count_lines() {
    let (rpc, mut incoming) = six_lines().await;
    // `3zF` from line 2 folds 3 lines (2-4), created closed.
    let map = redraw_after(&rpc, &mut incoming, "2G3zF").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 5, 6],
        "3zF folds three lines from the cursor"
    );
}

// ===== Phase 6: operator-over-fold semantics ===============================
//
// A linewise operator (or a linewise-visual selection) over a *closed* fold acts
// on the whole fold range, not just its header line (vim's fold rule). Driven over
// manual folds; the buffer is what changes, so these assert on `nvim_buf_get_lines`.

#[tokio::test]
async fn dd_over_a_closed_fold_deletes_the_whole_fold() {
    let (rpc, _incoming) = six_lines().await;
    // Fold lines 2-4 closed, cursor parked on the header (line 2).
    feed(&rpc, "2Gzf2j");
    // `dd` on the closed fold deletes all three folded lines, not just line 2.
    feed(&rpc, "dd");
    assert_eq!(
        lines(&rpc).await,
        vec!["L1", "L5", "L6"],
        "dd over a closed fold removes the whole fold range"
    );
}

#[tokio::test]
async fn yy_over_a_closed_fold_yanks_the_whole_fold() {
    let (rpc, _incoming) = six_lines().await;
    feed(&rpc, "2Gzf2j");
    // Yank the closed fold, jump to the last line, and paste below it.
    feed(&rpc, "yyGp");
    assert_eq!(
        lines(&rpc).await,
        vec!["L1", "L2", "L3", "L4", "L5", "L6", "L2", "L3", "L4"],
        "yy over a closed fold yanks all three folded lines"
    );
}

#[tokio::test]
async fn dj_over_a_closed_fold_deletes_through_the_whole_fold() {
    let (rpc, _incoming) = six_lines().await;
    // Fold lines 2-4 closed, then park the cursor on line 1 (above the fold).
    feed(&rpc, "2Gzf2jgg");
    // `dj` deletes line 1 plus the next *display* line — which is the whole closed
    // fold (the operator-pending motion path expands the range to the fold's end).
    feed(&rpc, "dj");
    assert_eq!(
        lines(&rpc).await,
        vec!["L5", "L6"],
        "dj over a closed fold deletes through the fold's full range"
    );
}

#[tokio::test]
async fn visual_line_delete_over_a_closed_fold_takes_the_whole_fold() {
    let (rpc, _incoming) = six_lines().await;
    feed(&rpc, "2Gzf2j");
    // Select the fold's header line linewise and delete — the closed fold expands
    // the selection to its full range.
    feed(&rpc, "Vd");
    assert_eq!(
        lines(&rpc).await,
        vec!["L1", "L5", "L6"],
        "a linewise-visual delete over a closed fold takes the whole fold"
    );
}

// ===== Phase 3: foldmethod=indent ==========================================
//
// Computed folds: the structure is derived from each line's leading indent
// (`indent / shiftwidth`, default sw=4), recomputed on edit and option change.
// `'foldlevel'` governs which levels display closed.

/// Insert `src` (each entry already carrying its literal indentation) into a
/// fresh buffer and return to the top in Normal mode.
async fn indent_fixture(src: &[&str]) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start(None).await;
    feed(&rpc, &format!("i{}<Esc>gg", src.join("<CR>")));
    let want: Vec<String> = src.iter().map(|s| s.to_string()).collect();
    assert_eq!(lines(&rpc).await, want, "indent fixture");
    (rpc, incoming)
}

#[tokio::test]
async fn vim_bo_foldmethod_sets_the_fold_source() {
    // The `vim.bo`/`btv.bo` option path reaches the live fold engine (not just a
    // Lua-only shadow), so a config can enable folds without `:set`.
    let (rpc, mut incoming) = indent_fixture(&["a", "    b", "    c"]).await;
    exec_lua(&rpc, "vim.bo.foldmethod = 'indent'").await;
    // The next tick recomputes folds from the new method; foldlevel 0 closes the
    // indented block (lines 2-3) into one placeholder under line 1.
    let map = redraw_after(&rpc, &mut incoming, "gg").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2],
        "btv.bo.foldmethod=indent folds the indented block, got {:?}",
        visible_numbers(&map)
    );
}

#[tokio::test]
async fn indent_folds_collapse_nested_blocks() {
    // levels: 0, 1, 2, 2, 1, 0  → a level-1 fold over the whole body (lines 2-5)
    // with a level-2 fold nested over the doubly-indented `x`/`y`.
    let (rpc, mut incoming) =
        indent_fixture(&["func()", "    a", "        x", "        y", "    b", "qux"]).await;
    let map = redraw_after(&rpc, &mut incoming, ":set foldmethod=indent<CR>").await;
    // foldlevel defaults to 0, so every computed fold closes; the outermost level-1
    // fold collapses the entire indented body into one row.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 6],
        "the whole indented body folds into the placeholder at line 2"
    );
    let rendered = view_lines(&map);
    assert!(
        rendered[1].contains("4 lines") && rendered[1].contains("a"),
        "placeholder shows the level-1 fold's span + first line, got {:?}",
        rendered[1]
    );
}

#[tokio::test]
async fn indent_foldlevel_one_shows_outer_level_only() {
    let (rpc, mut incoming) =
        indent_fixture(&["func()", "    a", "        x", "        y", "    b", "qux"]).await;
    feed(&rpc, ":set foldmethod=indent<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set foldlevel=1<CR>").await;
    // Level-1 fold open; only the deeper level-2 (`x`/`y`) block stays closed.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 3, 5, 6],
        "only the doubly-indented block is folded at foldlevel=1"
    );
    let rendered = view_lines(&map);
    assert!(
        rendered[2].contains("2 lines") && rendered[2].contains("x"),
        "the level-2 fold collapses x/y, got {:?}",
        rendered[2]
    );
}

#[tokio::test]
async fn editing_reflows_indent_folds() {
    let (rpc, mut incoming) = indent_fixture(&["func()", "    a", "    b", "qux"]).await;
    feed(&rpc, ":set foldmethod=indent<CR>");
    // The body (a/b) folds to two lines initially.
    let before = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        visible_numbers(&before),
        vec![1, 2, 4],
        "a/b fold to one row"
    );
    assert!(view_lines(&before)[1].contains("2 lines"));

    // Open the fold, append a third indented line after `b`, then re-close: the
    // fold structure must have reflowed to span all three indented lines.
    feed(&rpc, "zR"); // open every fold
    feed(&rpc, "3Go    c<Esc>"); // add "    c" below `b`
    let after = redraw_after(&rpc, &mut incoming, ":set foldlevel=0<CR>").await;
    assert_eq!(
        visible_numbers(&after),
        vec![1, 2, 5],
        "the fold grew to hide the new line too"
    );
    assert!(
        view_lines(&after)[1].contains("3 lines"),
        "fold reflowed to three lines, got {:?}",
        view_lines(&after)[1]
    );
}

#[tokio::test]
async fn unsupported_foldmethod_fails_loud() {
    let (rpc, mut incoming) = indent_fixture(&["a", "b"]).await;
    // A real vim method bemtvi hasn't implemented yet (`syntax`/`diff`): a named,
    // non-silent error (no silent no-op that leaves folding looking broken).
    let syntax = redraw_after(&rpc, &mut incoming, ":set foldmethod=syntax<CR>").await;
    assert!(
        message(&syntax).contains("not supported"),
        "syntax foldmethod should fail loud, got {:?}",
        message(&syntax)
    );
    // A value vim doesn't define at all is E474.
    let bogus = redraw_after(&rpc, &mut incoming, ":set foldmethod=bogus<CR>").await;
    assert!(
        message(&bogus).contains("E474"),
        "an unknown foldmethod is E474, got {:?}",
        message(&bogus)
    );
}

// ===== Phase 4a: foldmethod=expr (tree-sitter foldexpr) ====================
//
// The dispatch surface, exercised hermetically (no grammar): foldmethod=expr is
// accepted, the tree-sitter foldexpr is recognized, and an expr source with no
// grammar loaded is inert (no crash). The generic (non-native) foldexpr is
// evaluated for real in the Phase 5 section below.
// The actual tree-sitter folding over a real parse lives in the `#[ignore]`d
// `treesitter_folds` e2e test (needs network + a C compiler, like the other ts
// e2e tests).

#[tokio::test]
async fn foldmethod_expr_is_accepted_and_queryable() {
    let (rpc, mut incoming) = indent_fixture(&["a", "b"]).await;
    // `expr` is a supported method now — setting it must not error.
    let set = redraw_after(&rpc, &mut incoming, ":set foldmethod=expr<CR>").await;
    assert_eq!(message(&set), "", "setting foldmethod=expr is silent");
    // The tree-sitter foldexpr is recognized (no Phase-5 warning).
    let fde = redraw_after(
        &rpc,
        &mut incoming,
        ":set foldexpr=v:lua.vim.treesitter.foldexpr()<CR>",
    )
    .await;
    assert_eq!(
        message(&fde),
        "",
        "the tree-sitter foldexpr is native, no warning"
    );
    // `?` round-trips both values.
    let q = redraw_after(&rpc, &mut incoming, ":set foldmethod?<CR>").await;
    assert!(
        message(&q).contains("foldmethod=expr"),
        "got {:?}",
        message(&q)
    );
}

// ===== Phase 5: generic Lua foldexpr =======================================
//
// A non-native `'foldexpr'` is evaluated per line by the server (vim's fold-expr
// model, with `v:lnum` bound) and the per-line values are pushed into the core
// fold engine, which resolves them to the same fold structure the other sources
// build. Driven over a scratch buffer — no grammar required.

#[tokio::test]
async fn generic_foldexpr_folds_returned_levels() {
    let (rpc, mut incoming) = six_lines().await;
    // A foldexpr that puts lines 2..4 at fold level 1 (everything else at level 0).
    exec_lua(
        &rpc,
        "function MyFold()\n\
         local l = vim.v.lnum\n\
         if l >= 2 and l <= 4 then return 1 else return 0 end\n\
         end\n\
         return true",
    )
    .await;
    feed(&rpc, ":set foldmethod=expr<CR>");
    // Setting a generic foldexpr is silent now — it's evaluated, not warned about.
    let map = redraw_after(&rpc, &mut incoming, ":set foldexpr=v:lua.MyFold()<CR>").await;
    assert_eq!(
        message(&map),
        "",
        "a generic foldexpr is evaluated, not warned, got {:?}",
        message(&map)
    );
    // foldlevel defaults to 0, so the level-1 fold over lines 2..4 closes: lines 3-4
    // hide behind the placeholder on line 2.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 5, 6],
        "the foldexpr folds lines 2-4, hiding 3-4"
    );
    let rendered = view_lines(&map);
    assert!(
        rendered[1].contains("3 lines"),
        "the fold placeholder shows the span, got {:?}",
        rendered[1]
    );
}

#[tokio::test]
async fn generic_foldexpr_reflows_on_edit() {
    // A content-driven foldexpr: an indented line is fold level 1, a flush-left line
    // level 0 — so the fold tracks the indentation, not absolute line numbers.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihead<CR>  a<CR>  b<CR>c<CR>tail<Esc>gg");
    exec_lua(
        &rpc,
        "function MyFold()\n\
         local l = vim.v.lnum\n\
         local s = vim.api.nvim_buf_get_lines(0, l - 1, l, false)[1] or ''\n\
         if s:match('^%s') then return 1 else return 0 end\n\
         end\n\
         return true",
    )
    .await;
    feed(&rpc, ":set foldmethod=expr<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set foldexpr=v:lua.MyFold()<CR>").await;
    // Lines 2-3 (`  a`/`  b`) are indented and fold into one placeholder; line 4
    // (`c`, flush-left) and `tail` stay visible.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 4, 5],
        "the indented block 2-3 folds, got {:?}",
        visible_numbers(&map)
    );
    // Indent the visible line 4 (`c` → `  c`): the foldexpr re-evaluates on the edit
    // and the fold grows to span 2-4, hiding the now-indented line.
    let after = redraw_after(&rpc, &mut incoming, "4GI  <Esc>").await;
    assert_eq!(
        visible_numbers(&after),
        vec![1, 2, 5],
        "the edit reflowed the fold to span 2-4, got {:?}",
        visible_numbers(&after)
    );
}

#[tokio::test]
async fn generic_foldexpr_eval_error_is_loud() {
    let (rpc, mut incoming) = six_lines().await;
    feed(&rpc, ":set foldmethod=expr<CR>");
    // A foldexpr that calls an undefined function fails to evaluate — the error is
    // surfaced on the message line (no silent no-op) and the buffer stays unfolded.
    let map = redraw_after(&rpc, &mut incoming, ":set foldexpr=v:lua.NoSuchFold()<CR>").await;
    assert!(
        message(&map).contains("foldexpr"),
        "a broken foldexpr surfaces its error, got {:?}",
        message(&map)
    );
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 3, 4, 5, 6],
        "a broken foldexpr folds nothing, all lines shown"
    );
}

#[tokio::test]
async fn treesitter_foldexpr_lua_marker() {
    let (rpc, _incoming) = start(None).await;
    // The native marker exists under `btv.*` and is aliased as `vim.*` — both
    // resolve to the one function `'foldexpr'` references.
    let same = exec_lua(
        &rpc,
        "return btv.treesitter.foldexpr == vim.treesitter.foldexpr",
    )
    .await;
    assert_eq!(
        same.as_bool(),
        Some(true),
        "vim.treesitter.foldexpr aliases btv.treesitter.foldexpr"
    );
    // Calling it directly is a loud usage error (it's a native marker consumed by
    // the fold engine; per-line Lua foldexpr evaluation is Phase 5).
    let errored = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.treesitter.foldexpr, 1)\n\
         return (not ok) and tostring(err):find('native marker') ~= nil",
    )
    .await;
    assert_eq!(
        errored.as_bool(),
        Some(true),
        "calling the marker fails loud, naming it a native marker"
    );
}

#[tokio::test]
async fn lsp_foldexpr_lua_marker() {
    let (rpc, _incoming) = start(None).await;
    // The LSP fold marker exists under `btv.*` and is aliased as `vim.*` — both
    // resolve to the one function `'foldexpr'` references for the LSP fold source.
    let same = exec_lua(&rpc, "return btv.lsp.foldexpr == vim.lsp.foldexpr").await;
    assert_eq!(
        same.as_bool(),
        Some(true),
        "vim.lsp.foldexpr aliases btv.lsp.foldexpr"
    );
    // Calling it directly is a loud usage error (it's a native marker the server
    // consumes via textDocument/foldingRange, never evaluated per line).
    let errored = exec_lua(
        &rpc,
        "local ok, err = pcall(btv.lsp.foldexpr, 1)\n\
         return (not ok) and tostring(err):find('native marker') ~= nil",
    )
    .await;
    assert_eq!(
        errored.as_bool(),
        Some(true),
        "calling the LSP marker fails loud, naming it a native marker"
    );
}

#[tokio::test]
async fn expr_folds_inert_without_a_grammar() {
    // With the tree-sitter foldexpr set but no grammar for the buffer (plain
    // scratch text), there are simply no folds — every line stays visible, and
    // nothing crashes.
    let (rpc, mut incoming) = indent_fixture(&["alpha", "beta", "gamma"]).await;
    feed(&rpc, ":set foldexpr=v:lua.vim.treesitter.foldexpr()<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set foldmethod=expr<CR>").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 3],
        "no grammar ⇒ no tree-sitter folds, all lines shown"
    );
}

// ===== Phase 6: foldmethod=marker ==========================================
//
// The fifth fold source: folds bounded by literal markers in the text
// (`'foldmarker'`, default `{{{`/`}}}`). A start marker opens a fold at its line
// (the line shown when closed); the matching end marker's line is the fold's last
// line. Markers nest by counting, and a number after a marker (`{{{2`) sets an
// absolute level. Computed like indent/expr — recomputed on edit/option change,
// `'foldlevel'` governs which levels display closed.

#[tokio::test]
async fn marker_folds_collapse_a_marked_block() {
    // A plain `{{{`/`}}}` pair folds the lines between (inclusive of both markers).
    let (rpc, mut incoming) =
        indent_fixture(&["head {{{", "body1", "body2", "tail }}}", "after"]).await;
    let map = redraw_after(&rpc, &mut incoming, ":set foldmethod=marker<CR>").await;
    // foldlevel defaults to 0, so the fold closes: lines 1-4 collapse to the
    // placeholder on line 1, leaving line 5 visible.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 5],
        "the marked block (lines 1-4) folds into the placeholder at line 1, got {:?}",
        visible_numbers(&map)
    );
    let rendered = view_lines(&map);
    assert!(
        rendered[0].contains("4 lines") && rendered[0].contains("head"),
        "placeholder shows the span + first line, got {:?}",
        rendered[0]
    );
    // The buffer is untouched — the markers stay in the text.
    assert_eq!(
        lines(&rpc).await,
        vec!["head {{{", "body1", "body2", "tail }}}", "after"],
        "marker folds don't modify the buffer"
    );
}

#[tokio::test]
async fn marker_folds_nest_by_counting() {
    // Nested markers: the outer pair spans lines 1-6, the inner pair lines 2-5.
    let (rpc, mut incoming) = indent_fixture(&[
        "outer {{{",
        "inner {{{",
        "deep1",
        "deep2",
        "endin }}}",
        "endout }}}",
        "after",
    ])
    .await;
    feed(&rpc, ":set foldmethod=marker<CR>");
    // foldlevel=1 opens the level-1 outer fold; only the nested level-2 fold
    // (lines 2-5) stays closed.
    let map = redraw_after(&rpc, &mut incoming, ":set foldlevel=1<CR>").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 6, 7],
        "only the inner (level-2) marked block folds at foldlevel=1, got {:?}",
        visible_numbers(&map)
    );
    let rendered = view_lines(&map);
    assert!(
        rendered[1].contains("4 lines") && rendered[1].contains("inner"),
        "the inner fold collapses lines 2-5, got {:?}",
        rendered[1]
    );
}

#[tokio::test]
async fn numbered_marker_sets_absolute_level() {
    // `{{{1` opens a level-1 fold regardless of nesting; a later `{{{2` nests one
    // deeper. The `}}}` end markers close them.
    let (rpc, mut incoming) =
        indent_fixture(&["a {{{1", "b", "c {{{2", "d", "e }}}", "f }}}", "g"]).await;
    feed(&rpc, ":set foldmethod=marker<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set foldlevel=1<CR>").await;
    // Level-1 fold (lines 1-6) open; the level-2 fold (lines 3-5) stays closed —
    // hiding lines 4-5 while line 6 (still in the open outer fold) stays visible.
    assert_eq!(
        visible_numbers(&map),
        vec![1, 2, 3, 6, 7],
        "the level-2 absolute fold collapses lines 3-5, got {:?}",
        visible_numbers(&map)
    );
}

#[tokio::test]
async fn custom_foldmarker_changes_the_delimiters() {
    // `'foldmarker'` overrides the markers; the default `{{{`/`}}}` then no longer
    // fold, and the custom pair does.
    let (rpc, mut incoming) =
        indent_fixture(&["head #region", "body1", "body2", "tail #endregion", "after"]).await;
    feed(&rpc, ":set foldmarker=#region,#endregion<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set foldmethod=marker<CR>").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 5],
        "the custom #region/#endregion pair folds lines 1-4, got {:?}",
        visible_numbers(&map)
    );
}

#[tokio::test]
async fn editing_reflows_marker_folds() {
    // Adding a line inside the marked block grows the fold on the next recompute.
    let (rpc, mut incoming) = indent_fixture(&["head {{{", "body", "tail }}}", "after"]).await;
    feed(&rpc, ":set foldmethod=marker<CR>");
    let before = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        visible_numbers(&before),
        vec![1, 4],
        "block folds to one row"
    );
    assert!(view_lines(&before)[0].contains("3 lines"));

    // Open, insert a line inside the markers, re-close.
    feed(&rpc, "zR");
    feed(&rpc, "2Gonew body<Esc>"); // add a line after `body`, still inside the markers
    let after = redraw_after(&rpc, &mut incoming, ":set foldlevel=0<CR>").await;
    assert_eq!(
        visible_numbers(&after),
        vec![1, 5],
        "the fold grew to hide the inserted line, got {:?}",
        visible_numbers(&after)
    );
    assert!(
        view_lines(&after)[0].contains("4 lines"),
        "fold reflowed to four lines, got {:?}",
        view_lines(&after)[0]
    );
}

#[tokio::test]
async fn invalid_foldmarker_fails_loud() {
    let (rpc, mut incoming) = indent_fixture(&["a", "b"]).await;
    // A `'foldmarker'` without the required start,end pair is rejected (E474),
    // never silently leaving the markers unset.
    let map = redraw_after(&rpc, &mut incoming, ":set foldmarker=oops<CR>").await;
    assert!(
        message(&map).contains("E474"),
        "a foldmarker without a comma pair is E474, got {:?}",
        message(&map)
    );
}

#[tokio::test]
async fn btv_bo_foldmarker_reaches_the_live_engine() {
    // The `btv.bo`/`vim.bo` foldmarker write reaches the fold engine (not just a
    // Lua shadow), so a config can pick custom markers without `:set`.
    let (rpc, mut incoming) =
        indent_fixture(&["head <!--", "body1", "body2", "tail -->", "after"]).await;
    exec_lua(&rpc, "btv.bo.foldmarker = '<!--,-->'").await;
    exec_lua(&rpc, "btv.bo.foldmethod = 'marker'").await;
    let map = redraw_after(&rpc, &mut incoming, "gg").await;
    assert_eq!(
        visible_numbers(&map),
        vec![1, 5],
        "the custom <!--/--> pair folds lines 1-4, got {:?}",
        visible_numbers(&map)
    );
}
