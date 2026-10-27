//! `'list'` / `'listchars'`: invisible characters drawn as visible glyphs. The core
//! substitutes the glyphs into the painted rows (keeping every replaced character's
//! width, so cursor and span columns don't move) and the server colors them
//! `Whitespace` (`NonText` for `eol`). These pin the painted rows, the colors and
//! the cursor column; the option round-trip lives in `tests/options.rs`.

use crate::support::*;

/// Load `text` into the scratch buffer, run each `:set` in `sets`, and return the
/// redraw that follows.
async fn render(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    text: &[&str],
    sets: &[&str],
) -> Vec<(Value, Value)> {
    let quoted: Vec<String> = text.iter().map(|l| format!("{l:?}")).collect();
    exec_lua(
        rpc,
        &format!(
            "btv.buf.set_lines(0, 0, -1, false, {{{}}})",
            quoted.join(", ")
        ),
    )
    .await;
    exec_lua(rpc, "btv.win.set_cursor(0, 1, 0)").await;
    // `<` would start key notation and `|` would end the `:set`.
    let keys: String = sets
        .iter()
        .map(|s| format!(":set {}<CR>", s.replace('<', "<lt>").replace('|', "\\|")))
        .collect();
    redraw_after(rpc, incoming, &keys).await
}

/// The `[start, end)` columns of the spans in `group` on display row `row`.
fn spans_of(view: &[(Value, Value)], row: usize, group: &str) -> Vec<(u64, u64)> {
    view_get(view, "highlights")
        .and_then(Value::as_array)
        .and_then(|rows| rows.get(row))
        .and_then(Value::as_array)
        .map(|spans| {
            spans
                .iter()
                .filter_map(|s| {
                    let s = s.as_array()?;
                    (s.get(2)?.as_str()? == group).then_some(())?;
                    Some((s.first()?.as_u64()?, s.get(1)?.as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn list_is_off_by_default_and_draws_the_default_listchars_when_on() {
    let (rpc, mut incoming) = start(None).await;
    let text = ["\tab\tc  "];

    // Off: the row is the raw buffer text (the client expands the tabs).
    let view = render(&rpc, &mut incoming, &text, &["tabstop=4"]).await;
    assert_eq!(view_lines(&view)[0], "\tab\tc  ");

    // On, with the default `tab:→ ,space:·,trail:•,nbsp:␣`: each tab is `→` padded
    // with blanks to its full width (4 cells at col 0, 2 at col 6), trailing spaces `•`.
    let view = redraw_after(&rpc, &mut incoming, ":set list<CR>").await;
    assert_eq!(view_lines(&view)[0], "→   ab→ c••");
}

/// The default must show something on an ordinary file: one indented with spaces,
/// with no tabs and no trailing blanks. neovim's default draws only tabs, trailing
/// blanks and nbsps, so `:set list` changed nothing on such a file.
#[tokio::test]
async fn the_default_listchars_draws_plain_spaces() {
    let (rpc, mut incoming) = start(None).await;
    let view = render(&rpc, &mut incoming, &["    let y = 2;"], &["list"]).await;
    assert_eq!(view_lines(&view)[0], "····let·y·=·2;");
}

#[tokio::test]
async fn list_keeps_the_cursor_column() {
    let (rpc, mut incoming) = start(None).await;
    let _ = render(&rpc, &mut incoming, &["\t\tx"], &["tabstop=4"]).await;
    let off = redraw_after(&rpc, &mut incoming, "$").await;
    let on = redraw_after(&rpc, &mut incoming, ":set list<CR>").await;
    assert_eq!(view_lines(&on)[0], "→   →   x");
    assert_eq!(view_u64(&off, "cursor_screen_col"), 8);
    assert_eq!(
        view_u64(&on, "cursor_screen_col"),
        8,
        "the glyphs cover exactly the cells the tabs did"
    );
}

#[tokio::test]
async fn each_listchars_key_draws_its_glyph() {
    let (rpc, mut incoming) = start(None).await;
    // a(0) ' '(1) b(2) nbsp(3) c(4) tab(5..8) d(8): the tab is 3 cells wide.
    let view = render(
        &rpc,
        &mut incoming,
        &["a b\u{a0}c\td"],
        &[
            "tabstop=4",
            "list",
            "listchars=tab:<->,space:.,nbsp:_,eol:$",
        ],
    )
    .await;
    assert_eq!(view_lines(&view)[0], "a.b_c<->d$");

    // A two-char `tab:` repeats its second char; a one-cell tab with a third char
    // is just that third char.
    let view = render(&rpc, &mut incoming, &["abc\tx\ty"], &["listchars=tab:>-"]).await;
    assert_eq!(view_lines(&view)[0], "abc>x>--y");
    let view = render(&rpc, &mut incoming, &["abc\tx"], &["listchars=tab:<-|"]).await;
    assert_eq!(view_lines(&view)[0], "abc|x");
}

#[tokio::test]
async fn lead_trail_and_multispace_take_precedence_over_space() {
    let (rpc, mut incoming) = start(None).await;
    let view = render(
        &rpc,
        &mut incoming,
        &["   x  y z   "],
        &["list", "listchars=space:.,lead:L,trail:T,multispace:ab"],
    )
    .await;
    assert_eq!(
        view_lines(&view)[0],
        "LLLxaby.zTTT",
        "lead run → lead, inner run of 2 → multispace, lone space → space, trailing → trail"
    );

    // `leadmultispace` cycles across a leading run of two or more; a single leading
    // space isn't a run, so it falls back to `space`.
    let view = render(
        &rpc,
        &mut incoming,
        &["     x y", " z"],
        &["listchars=leadmultispace:|-,space:."],
    )
    .await;
    assert_eq!(view_lines(&view)[0], "|-|-|x.y");
    assert_eq!(view_lines(&view)[1], ".z");

    // A line that is blank throughout has no leading run: it is all trailing.
    let view = render(&rpc, &mut incoming, &["   "], &["listchars=lead:L,trail:-"]).await;
    assert_eq!(view_lines(&view)[0], "---");
}

#[tokio::test]
async fn wrapped_rows_size_tabs_from_the_full_line_and_end_with_eol() {
    let (rpc, mut incoming) = start(None).await;
    // 100 cells of text then a tab at column 100 (4 cells to the next stop of 8),
    // in a window narrower than that, so the tab lands on a continuation row.
    let line = format!("{}\tb", "a".repeat(100));
    let view = render(
        &rpc,
        &mut incoming,
        &[line.as_str()],
        &["tabstop=8", "wrap", "list", "listchars=tab:>-,eol:$"],
    )
    .await;
    let rows = view_lines(&view);
    let numbers = view_numbers(&view);
    assert_eq!(numbers[0], Some(1));
    assert_eq!(
        numbers[1],
        Some(1),
        "line 1 wraps onto a second row: {rows:?}"
    );
    assert!(
        !rows[0].contains('$'),
        "eol is only on the line's last row: {rows:?}"
    );
    assert!(
        rows[1].ends_with("a>---b$"),
        "the tab is sized at its full-line column (100 → 104): {rows:?}"
    );
}

#[tokio::test]
async fn nowrap_rows_keep_the_glyphs_under_leftcol() {
    let (rpc, mut incoming) = start(None).await;
    let line = format!("{}\tb  ", "a".repeat(200));
    let view = render(
        &rpc,
        &mut incoming,
        &[line.as_str()],
        &[
            "tabstop=8",
            "nowrap",
            "list",
            "listchars=tab:>-,trail:~,eol:$",
        ],
    )
    .await;
    // The row is cut to what the window can reach, so neither the tab nor the
    // trailing blanks nor `eol` are on it yet.
    assert!(!view_lines(&view)[0].contains(['>', '~', '$']));
    // `$` scrolls the line to its end: the whole tail is drawn.
    let view = redraw_after(&rpc, &mut incoming, "$").await;
    assert!(
        view_lines(&view)[0].ends_with("a>-------b~~$"),
        "{:?}",
        view_lines(&view)[0]
    );
}

#[tokio::test]
async fn list_glyphs_are_colored_whitespace_and_eol_nontext() {
    let (rpc, mut incoming) = start(None).await;
    let view = render(
        &rpc,
        &mut incoming,
        &["a\tb c "],
        &[
            "tabstop=4",
            "list",
            "listchars=tab:>-,space:.,trail:-,eol:$",
        ],
    )
    .await;
    // a(0) tab(1..4) b(4) ' '(5) c(6) ' '(7, trailing) $(8)
    assert_eq!(view_lines(&view)[0], "a>--b.c-$");
    assert_eq!(
        spans_of(&view, 0, "Whitespace"),
        vec![(1, 4), (5, 6), (7, 8)],
        "the tab, the space and the trailing space are Whitespace"
    );
    assert_eq!(
        spans_of(&view, 0, "NonText"),
        vec![(8, 9)],
        "eol is NonText"
    );

    // With `list` off nothing is colored.
    let view = redraw_after(&rpc, &mut incoming, ":set nolist<CR>").await;
    assert!(spans_of(&view, 0, "Whitespace").is_empty());
    assert!(spans_of(&view, 0, "NonText").is_empty());
}

#[tokio::test]
async fn a_bad_listchars_value_fails_loud_and_keeps_the_old_one() {
    let (rpc, mut incoming) = start(None).await;
    let _ = render(
        &rpc,
        &mut incoming,
        &["a b"],
        &["list", "listchars=space:."],
    )
    .await;
    for bad in ["tab:>", "eol:ab", "space:\u{4e2d}", "bogus:x", "nocolon"] {
        let view = redraw_after(&rpc, &mut incoming, &format!(":set listchars={bad}<CR>")).await;
        assert_eq!(
            message(&view),
            format!("E474: Invalid argument: listchars={bad}")
        );
        assert_eq!(
            view_lines(&view)[0],
            "a.b",
            "{bad} left the old value in place"
        );
    }
    // The validated-but-not-drawn keys set cleanly (a vim config's value works).
    let view = redraw_after(
        &rpc,
        &mut incoming,
        ":set listchars=space:.,extends:>,precedes:<,conceal:*<CR>",
    )
    .await;
    assert_eq!(view_lines(&view)[0], "a.b");
}

#[tokio::test]
async fn vim_opt_listchars_takes_a_map() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(
        &rpc,
        "vim.opt.list = true; vim.opt.listchars = { tab = '» ', trail = '·' }",
    )
    .await;
    let view = render(&rpc, &mut incoming, &["\tx  "], &["tabstop=4"]).await;
    assert_eq!(view_lines(&view)[0], "»   x··");
}
