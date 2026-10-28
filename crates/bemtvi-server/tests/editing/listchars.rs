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

    // On, with the default `tab:→ ,space:·,trail:•,nbsp:◦,eol:¬`: each tab is `→`
    // padded with blanks to its full width (4 cells at col 0, 2 at col 6), trailing
    // spaces `•`, and the line end `¬`.
    let view = redraw_after(&rpc, &mut incoming, ":set list<CR>").await;
    assert_eq!(view_lines(&view)[0], "→   ab→ c••¬");
}

/// The default must show something on an ordinary file: one indented with spaces,
/// with no tabs and no trailing blanks. neovim's default draws only tabs, trailing
/// blanks and nbsps, so `:set list` changed nothing on such a file. Line ends show
/// too, so a blank line is visibly a line.
#[tokio::test]
async fn the_default_listchars_draws_plain_spaces_and_line_ends() {
    let (rpc, mut incoming) = start(None).await;
    let view = render(&rpc, &mut incoming, &["    let y = 2;", "", "x"], &["list"]).await;
    let rows = view_lines(&view);
    assert_eq!(rows[0], "····let·y·=·2;¬");
    assert_eq!(rows[1], "¬", "an empty line shows its line end");
    assert_eq!(rows[2], "x¬");
}

#[tokio::test]
async fn list_keeps_the_cursor_column() {
    let (rpc, mut incoming) = start(None).await;
    let _ = render(&rpc, &mut incoming, &["\t\tx"], &["tabstop=4"]).await;
    let off = redraw_after(&rpc, &mut incoming, "$").await;
    let on = redraw_after(&rpc, &mut incoming, ":set list<CR>").await;
    assert_eq!(view_lines(&on)[0], "→   →   x¬");
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

// ---- Phase 2: `eol` on a full wrapped row, `extends` / `precedes` ----------------

/// The focused window's text width, with the gutters turned off so it is the rect's.
async fn bare_text_width(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> usize {
    let view = redraw_after(
        rpc,
        incoming,
        ":set nonumber norelativenumber signcolumn=no foldcolumn=0<CR>",
    )
    .await;
    view_get(&view, "rect")
        .and_then(Value::as_map)
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("width")))
        .and_then(|(_, v)| v.as_u64())
        .expect("a window rect width") as usize
}

/// A wrapped line that exactly fills its last row has no cell left for `eol`, so the
/// glyph takes a row of its own, as in vim, instead of being cut at the edge.
#[tokio::test]
async fn eol_on_a_full_wrapped_row_gets_a_row_of_its_own() {
    let (rpc, mut incoming) = start(None).await;
    let w = bare_text_width(&rpc, &mut incoming).await;
    let full = "a".repeat(w);
    let view = render(
        &rpc,
        &mut incoming,
        &[full.as_str(), "b"],
        &["wrap", "list", "listchars=eol:¬"],
    )
    .await;
    let rows = view_lines(&view);
    assert_eq!(rows[0], full);
    assert_eq!(rows[1], "¬", "the line end gets the next row: {rows:?}");
    assert_eq!(
        view_numbers(&view)[1],
        Some(1),
        "that row still belongs to line 1"
    );
    assert!(view_continuation(&view)[1], "and is a continuation row");
    assert_eq!(rows[2], "b¬");
    assert_eq!(
        spans_of(&view, 1, "NonText"),
        vec![(0, 1)],
        "the glyph on its own row is still NonText"
    );

    // The row is real to the cursor math: line 2 now sits on screen row 2.
    let view = redraw_after(&rpc, &mut incoming, "j").await;
    assert_eq!(view_u64(&view, "cursor_row"), 2);
    // An Insert cursor at the end of line 1 lands on the line-end row.
    let view = redraw_after(&rpc, &mut incoming, "kA").await;
    assert_eq!(view_u64(&view, "cursor_row"), 1);
    assert_eq!(view_u64(&view, "cursor_screen_col"), 0);

    // Without `eol` the line fits its one row, as before.
    let view = redraw_after(&rpc, &mut incoming, "<Esc>:set listchars=space:·<CR>").await;
    let rows = view_lines(&view);
    assert_eq!(rows[1], "b", "{rows:?}");
}

/// Under `nowrap`, `extends` marks the last column when the line runs past the
/// right edge, and `precedes` the first column when `leftcol` hides text on the
/// left. Both replace the cell they land on, so no column moves.
#[tokio::test]
async fn extends_and_precedes_mark_the_edges_of_a_nowrap_row() {
    let (rpc, mut incoming) = start(None).await;
    let w = bare_text_width(&rpc, &mut incoming).await;
    let long = format!("{}Z", "a".repeat(w + 20));
    let view = render(
        &rpc,
        &mut incoming,
        &[long.as_str(), "short"],
        &["nowrap", "list", "listchars=extends:»,precedes:«"],
    )
    .await;
    let row: Vec<char> = view_lines(&view)[0].chars().collect();
    assert_eq!(row[w - 1], '»', "extends on the last visible column");
    assert_eq!(row[0], 'a', "nothing hidden on the left yet");
    assert_eq!(
        view_lines(&view)[1],
        "short",
        "a line that fits gets no marker"
    );
    assert_eq!(
        spans_of(&view, 0, "NonText"),
        vec![((w - 1) as u64, w as u64)]
    );

    // `$` scrolls right: now text is hidden on the left, none on the right.
    let view = redraw_after(&rpc, &mut incoming, "$").await;
    let leftcol = view_u64(&view, "leftcol") as usize;
    assert!(leftcol > 0);
    let row: Vec<char> = view_lines(&view)[0].chars().collect();
    assert_eq!(row[leftcol], '«', "precedes on the first visible column");
    assert_eq!(
        *row.last().unwrap(),
        'Z',
        "the line's end is on screen: no extends"
    );
    assert_eq!(view_u64(&view, "cursor_screen_col") as usize, w + 20);

    // A line exactly as wide as the window doesn't run past it.
    let exact = "e".repeat(w);
    let view = render(&rpc, &mut incoming, &[exact.as_str()], &[]).await;
    assert_eq!(view_lines(&view)[0], exact);
}

/// A marker that lands inside a multi-cell glyph (a tab's run) replaces only the
/// cell it is on.
#[tokio::test]
async fn extends_inside_a_tab_replaces_only_its_cell() {
    let (rpc, mut incoming) = start(None).await;
    let w = bare_text_width(&rpc, &mut incoming).await;
    // The tab starts two cells before the edge and spans it (tabstop 4, and `w - 2`
    // is a multiple of 2 but the tab stop after it is further out than `w`).
    let pad = ((w - 2) / 4) * 4 + 1; // a tab at column `pad` reaches the next stop
    let line = format!("{}\tafter", "x".repeat(pad));
    let view = render(
        &rpc,
        &mut incoming,
        &[line.as_str()],
        &["tabstop=4", "nowrap", "list", "listchars=tab:→─,extends:»"],
    )
    .await;
    let row: Vec<char> = view_lines(&view)[0].chars().collect();
    // The tab is `→──` (3 cells from `pad`); whichever of its cells is the last
    // visible column carries the marker, the rest of the run is unchanged.
    let tab_end = pad + 3;
    assert!(
        tab_end >= w,
        "the tab must span the edge (w={w}, pad={pad})"
    );
    assert_eq!(row[w - 1], '»');
    assert_eq!(row[pad], '→', "the tab's first cell is still its glyph");
}

/// No edge marker covers the cursor's own cell. vim draws one there, which hides the
/// character the cursor is on whenever it rides an edge: after `$` on a line whose
/// `eol` cell is past the edge, or stepping right along a line that continues.
#[tokio::test]
async fn edge_markers_never_cover_the_cursor() {
    let (rpc, mut incoming) = start(None).await;
    let w = bare_text_width(&rpc, &mut incoming).await;
    let long = format!("{}Z", "a".repeat(w + 20));
    let _ = render(
        &rpc,
        &mut incoming,
        &[long.as_str()],
        &["nowrap", "list", "listchars=eol:¬,extends:»,precedes:«"],
    )
    .await;

    // `$`: the cursor is on `Z` at the right edge, the `¬` cell is past it.
    let view = redraw_after(&rpc, &mut incoming, "$").await;
    let leftcol = view_u64(&view, "leftcol") as usize;
    let row: Vec<char> = view_lines(&view)[0].chars().collect();
    assert_eq!(
        view_u64(&view, "cursor_screen_col") as usize,
        leftcol + w - 1
    );
    assert_eq!(
        row[leftcol + w - 1],
        'Z',
        "the cursor's character, not a marker"
    );
    assert_eq!(row[leftcol], '«', "the other edge keeps its marker");
    assert!(spans_of(&view, 0, "NonText")
        .iter()
        .all(|&(s, _)| s as usize != leftcol + w - 1));

    // Back at the start, the cursor on the last visible column of a line that goes on.
    let view = redraw_after(&rpc, &mut incoming, &format!("0{w}|")).await;
    assert_eq!(view_u64(&view, "leftcol"), 0);
    let row: Vec<char> = view_lines(&view)[0].chars().collect();
    assert_eq!(row[w - 1], 'a', "the cursor's character, not a marker");
    // One column left of the cursor, the marker is back.
    let view = redraw_after(&rpc, &mut incoming, "h").await;
    let row: Vec<char> = view_lines(&view)[0].chars().collect();
    assert_eq!(row[w - 1], '»');
}
