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
