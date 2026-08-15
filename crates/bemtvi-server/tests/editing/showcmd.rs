//! `'showcmd'` — the partly-typed command vim shows in the last line's right
//! corner, and the selection size it shows there instead while Visual is up.
//!
//! Driven black-box: feed keys, read the `showcmd` field off the resulting
//! `redraw` map (the client paints that string right-aligned on the command row).

use crate::support::*;

/// The `'showcmd'` string carried by a redraw.
fn showcmd(map: &[(Value, Value)]) -> String {
    field_str(map, "showcmd")
}

/// A fresh server on a temp file of `n` lines of `abcdefgh`.
async fn start_n(tag: &str, n: usize) -> (Rpc, UnboundedReceiver<Incoming>) {
    let body: String = std::iter::repeat_n("abcdefgh\n", n).collect();
    let path = temp_path(tag).to_string_lossy().into_owned();
    std::fs::write(&path, body).expect("write temp file");
    start(Some(path)).await
}

// ===== the partly-typed command ==============================================

#[tokio::test]
async fn a_typed_count_shows_in_the_corner() {
    // The literal report: typing a number left no trace on screen.
    let (rpc, mut i) = start_n("sc_count", 20).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "2").await), "2");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "3").await), "23");
}

#[tokio::test]
async fn an_operator_waiting_for_its_motion_shows_with_its_count() {
    let (rpc, mut i) = start_n("sc_op", 20).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "2d").await), "2d");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "3").await), "2d3");
}

#[tokio::test]
async fn an_armed_register_is_part_of_the_run() {
    let (rpc, mut i) = start_n("sc_reg", 20).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "\"a").await), "\"a");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "3y").await), "\"a3y");
}

#[tokio::test]
async fn an_argument_stage_shows_the_key_that_armed_it() {
    let (rpc, mut i) = start_n("sc_stage", 20).await;
    // `f` waits for the character to find; `z` and `<C-w>` for their sub-command.
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "f").await), "f");
    feed(&rpc, "x");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "2z").await), "2z");
    feed(&rpc, "<Esc>");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "<C-w>").await), "<C-w>");
}

#[tokio::test]
async fn the_corner_clears_when_the_command_completes() {
    let (rpc, mut i) = start_n("sc_clear", 20).await;
    feed(&rpc, "2");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "j").await), "");
}

#[tokio::test]
async fn a_half_typed_mapped_prefix_shows_too() {
    // The prefix is withheld by the server's keymap matcher and never reaches the
    // editor, so only the server can put it in the corner.
    let (rpc, mut i) = start_n("sc_map", 20).await;
    exec_lua(
        &rpc,
        "btv.keymap.set('n', '<Space>fs', function() end)\nreturn 1",
    )
    .await;
    assert_eq!(
        showcmd(&redraw_after(&rpc, &mut i, "<Space>f").await),
        "<Space>f"
    );
}

#[tokio::test]
async fn a_long_run_keeps_its_tail_within_vims_ten_columns() {
    let (rpc, mut i) = start_n("sc_trunc", 20).await;
    let map = redraw_after(&rpc, &mut i, "123456789012").await;
    assert_eq!(showcmd(&map), "3456789012");
}

// ===== the selection size ====================================================

#[tokio::test]
async fn a_linewise_selection_shows_its_line_count() {
    let (rpc, mut i) = start_n("sc_vline", 20).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "V").await), "1");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "3j").await), "4");
}

#[tokio::test]
async fn a_charwise_selection_inside_one_line_shows_its_character_count() {
    let (rpc, mut i) = start_n("sc_vchar", 20).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "v").await), "1");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "3l").await), "4");
}

#[tokio::test]
async fn a_charwise_selection_across_lines_shows_its_line_count() {
    let (rpc, mut i) = start_n("sc_vspan", 20).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "vj").await), "2");
}

#[tokio::test]
async fn a_multibyte_selection_shows_characters_and_bytes() {
    // vim's `chars-bytes` spelling, used only when a selected character is more
    // than one byte.
    let path = temp_path("sc_utf8").to_string_lossy().into_owned();
    std::fs::write(&path, "héllo\n").expect("write temp file");
    let (rpc, mut i) = start(Some(path)).await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "vl").await), "2-3");
}

#[tokio::test]
async fn leaving_visual_clears_the_size() {
    let (rpc, mut i) = start_n("sc_vleave", 20).await;
    feed(&rpc, "V3j");
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "<Esc>").await), "");
}

// ===== the option ============================================================

#[tokio::test]
async fn noshowcmd_leaves_the_corner_empty() {
    let (rpc, mut i) = start_n("sc_off", 20).await;
    feed(&rpc, ":set noshowcmd<CR>");
    lines(&rpc).await; // land the option before the count
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "2d").await), "");
    // Including the matcher's withheld prefix, which the server appends.
    exec_lua(
        &rpc,
        "btv.keymap.set('n', '<Space>fs', function() end)\nreturn 1",
    )
    .await;
    assert_eq!(showcmd(&redraw_after(&rpc, &mut i, "<Space>f").await), "");
}

#[tokio::test]
async fn showcmd_defaults_on_and_is_settable_from_lua() {
    let (rpc, _i) = start_n("sc_opt", 20).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.showcmd").await.as_bool(),
        Some(true)
    );
    feed(&rpc, ":set noshowcmd<CR>");
    assert_eq!(
        exec_lua(&rpc, "return vim.o.showcmd").await.as_bool(),
        Some(false)
    );
}
