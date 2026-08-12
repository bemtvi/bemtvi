use crate::support::*;

// ----- line-number column ---------------------------------------------------

/// Read a top-level bool field out of a redraw map.

#[tokio::test]
async fn number_column_is_on_by_default() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    assert!(field_bool(&map, "number"), "number on by default");
    assert!(
        field_bool(&map, "relativenumber"),
        "relativenumber on by default"
    );
    // Small buffer → 4-cell gutter (vim's numberwidth minimum).
    assert_eq!(field(&map, "number_width").and_then(Value::as_u64), Some(4));
}

#[tokio::test]
async fn numbers_track_buffer_lines_and_filler_rows() {
    let path = write_n_lines("nums", 2);
    let (rpc, mut incoming) = start(Some(path)).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;

    let nums = numbers(&map);
    // Two real lines numbered 1, 2; everything below is a `~` filler (None).
    assert_eq!(nums[0], Some(1));
    assert_eq!(nums[1], Some(2));
    assert!(
        nums[2..].iter().all(|n| n.is_none()),
        "fillers carry no number"
    );
}

#[tokio::test]
async fn set_nonumber_disables_the_gutter() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set nonumber norelativenumber<CR>").await;

    assert!(!field_bool(&map, "number"));
    assert!(!field_bool(&map, "relativenumber"));
    assert_eq!(
        field(&map, "number_width").and_then(Value::as_u64),
        Some(0),
        "no number option → zero-width gutter"
    );
}

#[tokio::test]
async fn numberwidth_sets_the_minimum_gutter_width() {
    let (rpc, mut incoming) = start(None).await;

    // `:set numberwidth=6` widens the small-buffer gutter from the 4 default to 6.
    let map = redraw_after(&rpc, &mut incoming, ":set numberwidth=6<CR>").await;
    assert_eq!(field(&map, "number_width").and_then(Value::as_u64), Some(6));

    // The `nuw` abbreviation works and is a *minimum*: a 2-cell request still keeps
    // the digits+1 the line count needs (here 2 → "9 " is 2, so it lands at 2).
    let map = redraw_after(&rpc, &mut incoming, ":set nuw=2<CR>").await;
    assert_eq!(field(&map, "number_width").and_then(Value::as_u64), Some(2));

    // `:set nuw=0` is rejected (vim's E487 floor of 1) and leaves the value at 2.
    let map = redraw_after(&rpc, &mut incoming, ":set nuw=0<CR>").await;
    assert_eq!(field(&map, "number_width").and_then(Value::as_u64), Some(2));
}

#[tokio::test]
async fn numberwidth_round_trips_through_vim_o() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(&rpc, "vim.o.numberwidth = 8").await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        field(&map, "number_width").and_then(Value::as_u64),
        Some(8),
        "vim.o.numberwidth reaches the gutter"
    );
    let got = exec_lua(&rpc, "return vim.o.numberwidth").await;
    assert_eq!(got.as_u64(), Some(8), "vim.o.numberwidth reads back");
}

// ----- sign column ----------------------------------------------------------

/// The redraw's sign-column width in cells (`sign_width`, focused window).
fn sign_width(map: &[(Value, Value)]) -> u64 {
    field(map, "sign_width")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[tokio::test]
async fn signcolumn_width_follows_the_policy() {
    let (rpc, mut incoming) = start(None).await;

    // Default `auto` on a clean buffer (no diagnostics) reserves nothing.
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(sign_width(&map), 0, "auto collapses with no signs");

    // `yes` always reserves one 2-cell column even with no signs; `yes:2` reserves
    // two (4 cells). Each sign column is 2 cells.
    let map = redraw_after(&rpc, &mut incoming, ":set signcolumn=yes<CR>").await;
    assert_eq!(sign_width(&map), 2, "yes reserves one column");
    let map = redraw_after(&rpc, &mut incoming, ":set scl=yes:2<CR>").await;
    assert_eq!(sign_width(&map), 4, "yes:2 reserves two columns");

    // `no` collapses the column unconditionally.
    let map = redraw_after(&rpc, &mut incoming, ":set scl=no<CR>").await;
    assert_eq!(sign_width(&map), 0, "no hides the column");
}

#[tokio::test]
async fn signcolumn_rejects_bad_values_and_keeps_the_old_one() {
    let (rpc, mut incoming) = start(None).await;

    // Land on a known-good value first (yes:2 → 4 cells).
    let map = redraw_after(&rpc, &mut incoming, ":set scl=yes:2<CR>").await;
    assert_eq!(sign_width(&map), 4);

    // A bogus value and the not-yet-supported `number` mode are both rejected
    // (E474); the option must be left untouched, not silently reset.
    let map = redraw_after(&rpc, &mut incoming, ":set scl=bogus<CR>").await;
    assert_eq!(
        sign_width(&map),
        4,
        "a bad value leaves signcolumn unchanged"
    );
    let map = redraw_after(&rpc, &mut incoming, ":set scl=number<CR>").await;
    assert_eq!(
        sign_width(&map),
        4,
        "`number` mode is rejected, not applied"
    );
}

#[tokio::test]
async fn signcolumn_round_trips_through_vim_o() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(&rpc, r#"vim.o.signcolumn = "yes:3""#).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        sign_width(&map),
        6,
        "vim.o.signcolumn=yes:3 reserves three columns"
    );
}

#[tokio::test]
async fn set_toggles_and_abbreviations_work() {
    let (rpc, mut incoming) = start(None).await;

    // `nu!` toggles `number` off; `rnu` abbreviation stays on.
    let map = redraw_after(&rpc, &mut incoming, ":set nu!<CR>").await;
    assert!(!field_bool(&map, "number"), "nu! toggled number off");
    assert!(
        field_bool(&map, "relativenumber"),
        "relativenumber untouched"
    );

    // `invnumber` toggles it back on.
    let map = redraw_after(&rpc, &mut incoming, ":set invnumber<CR>").await;
    assert!(field_bool(&map, "number"), "invnumber toggled number on");
}

// ----- cursorline -----------------------------------------------------------

#[tokio::test]
async fn cursorline_is_off_by_default() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        !field_bool(&map, "cursorline"),
        "cursorline off by default (vim's default)"
    );
}

#[tokio::test]
async fn set_cursorline_projects_the_flag_and_chrome_style() {
    let (rpc, mut incoming) = start(None).await;

    // `:set cursorline` flips the per-window flag the clients read to paint the
    // cursor row; `:set nocursorline` clears it; the `cul` abbreviation works too.
    let map = redraw_after(&rpc, &mut incoming, ":set cursorline<CR>").await;
    assert!(field_bool(&map, "cursorline"), "set cursorline turns it on");

    let map = redraw_after(&rpc, &mut incoming, ":set nocursorline<CR>").await;
    assert!(
        !field_bool(&map, "cursorline"),
        "set nocursorline turns it off"
    );

    let map = redraw_after(&rpc, &mut incoming, ":set cul<CR>").await;
    assert!(field_bool(&map, "cursorline"), "the cul abbreviation works");

    // The `cul!` toggle flips it back off.
    let map = redraw_after(&rpc, &mut incoming, ":set cul!<CR>").await;
    assert!(!field_bool(&map, "cursorline"), "cul! toggled it off");
}

#[tokio::test]
async fn cursorline_round_trips_through_vim_wo() {
    let (rpc, mut incoming) = start(None).await;
    exec_lua(&rpc, "vim.wo.cursorline = true").await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert!(
        field_bool(&map, "cursorline"),
        "vim.wo.cursorline reaches the window"
    );
    let got = exec_lua(&rpc, "return vim.wo.cursorline").await;
    assert_eq!(
        got.as_bool(),
        Some(true),
        "vim.wo.cursorline reads back the live value"
    );
}
