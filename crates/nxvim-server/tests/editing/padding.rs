//! `'padding'` — the window-local per-side blank margin around a window's content
//! box. These assert the server projection (the redraw `padding` array and the
//! shrunk text body) and the set/round-trip plumbing; the mouse mapping under
//! padding lives in `tests/mouse.rs`.

use crate::support::*;

/// The focused window's `padding` array (`[top, right, bottom, left]`), defaulting
/// to all-zero when the key is absent.
fn padding(map: &[(Value, Value)]) -> Vec<u64> {
    field(map, "padding")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_else(|| vec![0, 0, 0, 0])
}

#[tokio::test]
async fn padding_defaults_to_zero() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(padding(&map), vec![0, 0, 0, 0], "no margin out of the box");
}

#[tokio::test]
async fn set_padding_projects_and_shrinks_the_text_body() {
    let (rpc, mut incoming) = start(None).await;
    // Baseline text-row count with no margin.
    let base = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let base_rows = lines_len(&base);

    let map = redraw_after(&rpc, &mut incoming, ":set padding=2<CR>").await;
    assert_eq!(padding(&map), vec![2, 2, 2, 2], "uniform 2 on every side");
    // Top+bottom margin of 2 each removes 4 screen rows from the text body.
    assert_eq!(
        lines_len(&map),
        base_rows - 4,
        "vertical padding shrinks the projected text height"
    );
}

#[tokio::test]
async fn padding_css_shorthand_forms() {
    let (rpc, mut incoming) = start(None).await;

    // `pad` abbreviation, uniform.
    let map = redraw_after(&rpc, &mut incoming, ":set pad=3<CR>").await;
    assert_eq!(padding(&map), vec![3, 3, 3, 3]);

    // Two values: vertical, horizontal. Commas avoid `:set`'s space tokenizer.
    let map = redraw_after(&rpc, &mut incoming, ":set padding=1,4<CR>").await;
    assert_eq!(padding(&map), vec![1, 4, 1, 4]);

    // Four values: top, right, bottom, left (CSS order).
    let map = redraw_after(&rpc, &mut incoming, ":set padding=1,2,3,4<CR>").await;
    assert_eq!(padding(&map), vec![1, 2, 3, 4]);

    // A space-separated value also works if the space is backslash-escaped, the
    // vim way (an unescaped space would start a second `:set` option). Quotes are
    // NOT special in `:set` — they'd be taken literally — so commas or `\ ` are the
    // two ways to pass a multi-value padding.
    let map = redraw_after(&rpc, &mut incoming, ":set padding=1\\ 2<CR>").await;
    assert_eq!(padding(&map), vec![1, 2, 1, 2]);

    // `&` resets to no margin.
    let map = redraw_after(&rpc, &mut incoming, ":set padding&<CR>").await;
    assert_eq!(padding(&map), vec![0, 0, 0, 0]);
}

#[tokio::test]
async fn padding_rejects_bad_value_and_keeps_the_old_one() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set padding=2<CR>").await;
    assert_eq!(padding(&map), vec![2, 2, 2, 2]);

    // A non-numeric token is invalid (E474) and leaves the previous value intact.
    let map = redraw_after(&rpc, &mut incoming, ":set padding=bogus<CR>").await;
    assert_eq!(
        padding(&map),
        vec![2, 2, 2, 2],
        "bad value rejected, kept old"
    );

    // Five tokens is an unsupported count — also rejected, value unchanged.
    let map = redraw_after(&rpc, &mut incoming, ":set padding=1,2,3,4,5<CR>").await;
    assert_eq!(padding(&map), vec![2, 2, 2, 2]);
}

/// The shipped `examples/padding/init.lua` actually applies its margin — guards
/// the example against bit-rot (see the project's example-config convention).
#[tokio::test]
async fn example_config_applies_padding() {
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/padding/init.lua"
    ))
    .expect("read examples/padding/init.lua");
    let dir = temp_dir("padding-example");
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        padding(&map),
        vec![2, 2, 2, 2],
        "the example's `vim.wo.padding = 2` reaches the window"
    );
}

#[tokio::test]
async fn padding_round_trips_through_vim_wo() {
    let (rpc, mut incoming) = start(None).await;

    // A bare number sets a uniform margin.
    exec_lua(&rpc, "vim.wo.padding = 2").await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        padding(&map),
        vec![2, 2, 2, 2],
        "vim.wo.padding reaches the window"
    );
    let got = exec_lua(&rpc, "return vim.wo.padding").await;
    assert_eq!(
        got.as_str(),
        Some("2"),
        "reads back as the canonical string"
    );

    // A string shorthand sets the per-side form.
    exec_lua(&rpc, "vim.wo.padding = '1 2 3 4'").await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(padding(&map), vec![1, 2, 3, 4]);
    let got = exec_lua(&rpc, "return vim.wo.padding").await;
    assert_eq!(got.as_str(), Some("1 2 3 4"));
}
