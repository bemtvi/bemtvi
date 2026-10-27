//! Tier 1: the GUI's own column math for runs of text it lays end to end — status
//! rows, tablines, colored segments — measured in display cells, the way the shaper
//! lays the glyphs down, so a wide CJK/emoji char takes two cells and whatever
//! follows it starts after both. Black-box, no window, no GPU.

use bemtvi_gpu::{run_columns, segment_columns};

#[test]
fn a_status_row_lays_a_wide_file_name_out_in_two_cells_a_char() {
    // `%f` is `日本.txt` (two 2-cell chars), then ` [+]` in its own colour.
    let segs = vec![
        (" ".to_string(), None),
        ("日本.txt".to_string(), None),
        (" [+]".to_string(), None),
    ];
    assert_eq!(
        segment_columns(&segs, 3),
        vec![(3, 1), (4, 8), (12, 4)],
        "the segment after a wide name must start past all of its cells"
    );
}

#[test]
fn a_run_measures_each_text_in_cells() {
    assert_eq!(
        run_columns(["ab", "にほ", "", "c"], 0),
        vec![(0, 2), (2, 4), (6, 0), (6, 1)]
    );
}
