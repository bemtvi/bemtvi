//! Helix editing model — Phase 3: immediate-apply verbs.
//!
//! In Helix a verb acts on the current selection *now* — there is no
//! operator-pending wait. `d`/`c`/`y` operate on the `anchor..head` range, `>`/`<`/
//! `=` on the lines it touches, `~` switches case. `d` collapses the selection,
//! `y`/`~` keep it, and `c` opens Insert that resumes Helix normal on `<Esc>`.
//! These tests drive the opt-in mode (`:helix`) and assert on buffer contents, the
//! register, the rendered selection span, and the reported `mode()`.

use crate::support::*;

/// `d` deletes the current selection immediately (no motion wait) and collapses.
#[tokio::test]
async fn delete_removes_the_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    // `w` selects "hello " (the word + trailing space, cols 0..=5); `d` deletes it.
    feed(&rpc, "wd");
    assert_eq!(
        lines(&rpc).await,
        vec!["world"],
        "the selection was deleted"
    );
    assert_eq!(
        mode(&rpc).await,
        "hn",
        "stayed in Helix normal after delete"
    );
}

/// `c` changes the selection: delete then Insert; `<Esc>` resumes Helix normal.
#[tokio::test]
async fn change_deletes_then_inserts_and_resumes_helix() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    feed(&rpc, "wc");
    assert_eq!(mode(&rpc).await, "i", "change dropped into Insert");
    feed(&rpc, "X<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["Xworld"],
        "typed text replaced the selection"
    );
    assert_eq!(
        mode(&rpc).await,
        "hn",
        "<Esc> from the change resumed Helix normal"
    );
}

/// `y` yanks the selection into the unnamed register and *keeps* the selection —
/// the Helix behavior, unlike vim's visual `y` which drops back to a point.
#[tokio::test]
async fn yank_fills_register_and_keeps_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "wy").await;
    let reg = exec_lua(&rpc, "return vim.fn.getreg('\"')").await;
    assert_eq!(
        reg.as_str(),
        Some("hello "),
        "the selection went to the register"
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "the selection is kept after the yank",
    );
    assert_eq!(mode(&rpc).await, "hn", "still in Helix normal");
}

/// `~` switches the case of every character in the selection, keeping it selected.
#[tokio::test]
async fn switch_case_toggles_the_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "w~").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["HELLO world"],
        "case flipped across the selection only (the trailing space is unaffected)",
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "the selection is kept after the case switch",
    );
}

/// `>` indents the lines the selection touches (whole-line, like vim's `>`).
#[tokio::test]
async fn indent_shifts_the_selected_lines() {
    let (rpc, _i) = start(None).await;
    feed(
        &rpc,
        "ihello world<Esc>:set shiftwidth=4 expandtab<CR>0:helix<CR>",
    );
    feed(&rpc, ">");
    assert_eq!(
        lines(&rpc).await,
        vec!["    hello world"],
        "the line gained one shiftwidth of indent",
    );
    assert_eq!(
        mode(&rpc).await,
        "hn",
        "stayed in Helix normal after the shift"
    );
}

/// A verb on a plain (collapsed) 1-wide selection acts on the single char — `d`
/// with no prior selecting motion deletes the char under the cursor, like `x`.
#[tokio::test]
async fn delete_on_a_point_selection_removes_one_char() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello<Esc>0:helix<CR>");
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["ello"],
        "the char under the cursor was deleted"
    );
}

/// `r{char}` overwrites every character in the selection with `{char}`, keeping the
/// selection painted (Helix's `replace`).
#[tokio::test]
async fn replace_char_overwrites_the_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    // `w` selects "hello " (cols 0..=5); `r-` overwrites all six chars with '-'.
    let map = redraw_after(&rpc, &mut incoming, "wr-").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["------world"],
        "the whole selection (including the trailing space) became dashes",
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "the selection is kept after replace",
    );
    assert_eq!(mode(&rpc).await, "hn", "stayed in Helix normal");
}

/// `r` preserves newlines — a selection spanning lines keeps its line breaks, only
/// the real characters are overwritten.
#[tokio::test]
async fn replace_char_preserves_newlines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iab<CR>cd<Esc>:helix<CR>");
    // `%` selects the whole file; `r.` replaces every non-newline char.
    feed(&rpc, "%r.");
    assert_eq!(
        lines(&rpc).await,
        vec!["..", ".."],
        "each character became a dot, the newline survived",
    );
}

/// `R` replaces the selection with the unnamed register's contents, leaving the
/// spliced text selected (Helix's `replace_with_yanked`).
#[tokio::test]
async fn replace_with_yanked_splices_the_register() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    exec_lua(&rpc, "vim.fn.setreg('\"', 'ZZ')").await;
    // `w` selects "hello "; `R` swaps it for the register text "ZZ".
    let map = redraw_after(&rpc, &mut incoming, "wR").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["ZZworld"],
        "the selection was replaced by the register contents",
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 2)),
        "the spliced text is left selected",
    );
    assert_eq!(mode(&rpc).await, "hn", "stayed in Helix normal");
}

/// `J` joins the lines the selection spans into one, space-separated.
#[tokio::test]
async fn join_merges_the_selected_lines() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>:helix<CR>");
    // `%` selects all three lines; `J` joins them into "a b c".
    feed(&rpc, "%J");
    assert_eq!(
        lines(&rpc).await,
        vec!["a b c"],
        "the three lines were joined with single spaces",
    );
    assert_eq!(mode(&rpc).await, "hn", "stayed in Helix normal");
}

/// `J` on a single-line (point) selection joins it with the line below, like vim's
/// `J` — no selection span is required.
#[tokio::test]
async fn join_on_a_point_selection_pulls_up_the_next_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>:helix<CR>kJ");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar"],
        "the line below was joined onto the current one",
    );
}

/// `"{reg}` selects the register the next verb uses: `"ay` yanks into register `a`.
/// The selection is one-shot — a following plain `d` writes the unnamed register,
/// leaving `a` intact.
#[tokio::test]
async fn register_select_yanks_into_a_named_register_then_clears() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    // `w` selects "hello "; `"ay` yanks it into register a.
    feed(&rpc, "w\"ay");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getreg('a')").await.as_str(),
        Some("hello "),
        "the selection went to register a",
    );

    // A following plain `d` (no `\"a`) must go to the unnamed register, not a.
    feed(&rpc, "wd");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getreg('a')").await.as_str(),
        Some("hello "),
        "register a is untouched — the `\"a` selection was one-shot",
    );
}

/// `"{reg}p` pastes from the named register.
#[tokio::test]
async fn register_select_pastes_from_a_named_register() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello<Esc>0:helix<CR>");
    exec_lua(&rpc, "vim.fn.setreg('a', 'ZZ')").await;

    // Point selection on 'h'; `"ap` pastes register a after it.
    feed(&rpc, "\"ap");
    assert_eq!(
        lines(&rpc).await,
        vec!["hZZello"],
        "register a was pasted after the cursor char",
    );
}
