//! Tier-1 tests for the client-side resize-handle hit-test — the pure geometry a
//! GUI/web client runs on hover to pick a resize cursor. Black-box, no server: we
//! build a [`View`] + [`Geometry`] by hand and assert the cell→cursor mapping,
//! the client-side mirror of the server's `resize_handle_at`/`dock_handle_at`.

use bemtvi_view::{
    resize_handle_at, text_pointer_at, Geometry, ResizeCursor, Separator, View, WinRect,
    WindowRegion, WindowView,
};

/// An 80×24 grid with no chrome: 80 columns, 23 windows-area rows (24 − cmdline).
fn geo() -> Geometry {
    Geometry {
        cols: 80,
        rows: 23,
        tabline_rows: 0,
        global_status_rows: 0,
    }
}

#[test]
fn nothing_over_plain_cells() {
    let view = View::default();
    assert_eq!(resize_handle_at(&view, geo(), 5, 5), None);
    assert_eq!(resize_handle_at(&view, geo(), 0, 0), None);
}

#[test]
fn vertical_split_separator_is_a_col_cursor() {
    // A vsplit at column 40 spanning the whole windows area (main region origin 0,0).
    let view = View {
        separators: vec![Separator {
            vertical: true,
            x: 40,
            y: 0,
            length: 23,
            region: WindowRegion::Main,
        }],
        ..Default::default()
    };
    assert_eq!(
        resize_handle_at(&view, geo(), 5, 40),
        Some(ResizeCursor::Col)
    );
    // One cell either side is plain text.
    assert_eq!(resize_handle_at(&view, geo(), 5, 39), None);
    assert_eq!(resize_handle_at(&view, geo(), 5, 41), None);
}

#[test]
fn horizontal_separator_and_status_row_above_are_row_cursors() {
    // A horizontal divider at row 11; the status row just above it (row 10) is a
    // drag handle too (vim's status-line drag).
    let view = View {
        separators: vec![Separator {
            vertical: false,
            x: 0,
            y: 11,
            length: 80,
            region: WindowRegion::Main,
        }],
        ..Default::default()
    };
    assert_eq!(
        resize_handle_at(&view, geo(), 11, 20),
        Some(ResizeCursor::Row)
    );
    assert_eq!(
        resize_handle_at(&view, geo(), 10, 20),
        Some(ResizeCursor::Row)
    );
    assert_eq!(resize_handle_at(&view, geo(), 12, 20), None);
}

/// A pair of vertically stacked windows around a horizontal divider at row 11,
/// each `status_visible` per the argument — the `laststatus` shapes: `true` for
/// per-window status rows (laststatus=2), `false` for hidden ones (0 / 3, where
/// the freed row is ordinary text).
fn stacked_view(status_visible: bool) -> View {
    let win = |y: u16| WindowView {
        rect: Some(WinRect {
            x: 0,
            y,
            width: 80,
            height: 11,
        }),
        status_visible,
        ..Default::default()
    };
    View {
        windows: vec![win(0), win(12)],
        separators: vec![Separator {
            vertical: false,
            x: 0,
            y: 11,
            length: 80,
            region: WindowRegion::Main,
        }],
        ..Default::default()
    }
}

#[test]
fn text_row_above_separator_is_no_handle_when_status_is_hidden() {
    // laststatus=0/3: the upper window has no status row, so the row above the
    // divider is ordinary text — the server's authoritative hit-test only grabs
    // a *status* row there, and the mirror must agree (no bogus resize cursor
    // over the last text line of every upper split).
    let view = stacked_view(false);
    assert_eq!(resize_handle_at(&view, geo(), 10, 20), None);
    // The divider itself still resizes.
    assert_eq!(
        resize_handle_at(&view, geo(), 11, 20),
        Some(ResizeCursor::Row)
    );
}

#[test]
fn status_row_above_separator_is_a_handle_when_status_is_shown() {
    // laststatus=2: the upper window's bottom rect row is its status line, the
    // classic vim status-line drag handle.
    let view = stacked_view(true);
    assert_eq!(
        resize_handle_at(&view, geo(), 10, 20),
        Some(ResizeCursor::Row)
    );
}

#[test]
fn left_dock_edge_is_a_col_cursor_within_the_middle_band() {
    // A left dock of width 20: its edge separator sits at column 20.
    let view = View {
        dock_left: 20,
        ..Default::default()
    };
    assert_eq!(
        resize_handle_at(&view, geo(), 5, 20),
        Some(ResizeCursor::Col)
    );
    assert_eq!(resize_handle_at(&view, geo(), 5, 19), None);
    assert_eq!(resize_handle_at(&view, geo(), 5, 21), None);
}

#[test]
fn right_dock_edge_is_a_col_cursor() {
    // A right dock of width 20 occupies the right-most columns; its edge sits at
    // col 80 − (20 + 1) = 59.
    let view = View {
        dock_right: 20,
        ..Default::default()
    };
    assert_eq!(
        resize_handle_at(&view, geo(), 5, 59),
        Some(ResizeCursor::Col)
    );
    assert_eq!(resize_handle_at(&view, geo(), 5, 58), None);
}

#[test]
fn top_dock_edge_is_a_row_cursor() {
    let view = View {
        dock_top: 5,
        ..Default::default()
    };
    assert_eq!(
        resize_handle_at(&view, geo(), 5, 40),
        Some(ResizeCursor::Row)
    );
    assert_eq!(resize_handle_at(&view, geo(), 4, 40), None);
}

#[test]
fn bottom_dock_edge_is_a_row_cursor() {
    // A bottom dock of height 6: its edge sits at row 23 − (6 + 1) = 16.
    let view = View {
        dock_bottom: 6,
        ..Default::default()
    };
    assert_eq!(
        resize_handle_at(&view, geo(), 16, 40),
        Some(ResizeCursor::Row)
    );
    assert_eq!(resize_handle_at(&view, geo(), 15, 40), None);
}

#[test]
fn closed_docks_have_no_edge() {
    let view = View::default(); // every dock 0 (closed)
                                // The columns/rows a width-20 / height-6 dock edge would occupy are plain now.
    assert_eq!(resize_handle_at(&view, geo(), 5, 20), None);
    assert_eq!(resize_handle_at(&view, geo(), 16, 40), None);
}

/// A window at `rect` in the main region, taking the I-beam when `text`.
fn text_win(rect: WinRect, text: bool) -> WindowView {
    WindowView {
        rect: Some(rect),
        region: WindowRegion::Main,
        text_pointer: text,
        status_visible: true,
        numbers: (1..=usize::from(rect.height)).map(Some).collect(),
        ..Default::default()
    }
}

#[test]
fn the_i_beam_is_over_the_text_only() {
    // A full-area window with a 4-cell number gutter and a status row at the bottom.
    let mut win = text_win(
        WinRect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        },
        true,
    );
    win.number = true;
    win.number_width = 4;
    let view = View {
        windows: vec![win],
        ..Default::default()
    };
    assert!(text_pointer_at(&view, geo(), 3, 10));
    assert!(text_pointer_at(&view, geo(), 0, 4), "the first text column");
    assert!(!text_pointer_at(&view, geo(), 3, 3), "the number gutter");
    assert!(!text_pointer_at(&view, geo(), 22, 10), "the status row");
}

#[test]
fn a_read_only_window_or_a_float_over_the_text_keeps_the_arrow() {
    let rect = |x, width| WinRect {
        x,
        y: 0,
        width,
        height: 23,
    };
    let mut float = text_win(
        WinRect {
            x: 5,
            y: 2,
            width: 20,
            height: 5,
        },
        false,
    );
    float.floating = true;
    let view = View {
        windows: vec![
            text_win(rect(0, 40), true),
            text_win(rect(41, 39), false),
            float,
        ],
        ..Default::default()
    };
    assert!(text_pointer_at(&view, geo(), 10, 10));
    assert!(
        !text_pointer_at(&view, geo(), 10, 50),
        "the read-only split"
    );
    assert!(!text_pointer_at(&view, geo(), 3, 10), "the float on top");
}

#[test]
fn the_rows_past_the_buffer_keep_the_arrow() {
    // Two buffer lines, a virtual line under the first, then `~` fillers.
    let mut win = text_win(
        WinRect {
            x: 0,
            y: 0,
            width: 80,
            height: 23,
        },
        true,
    );
    win.numbers = vec![Some(1), None, Some(2), None, None];
    win.virt_lines = vec![None, Some(Vec::new()), None, None, None];
    let view = View {
        windows: vec![win],
        ..Default::default()
    };
    assert!(text_pointer_at(&view, geo(), 0, 10));
    assert!(text_pointer_at(&view, geo(), 2, 10), "a line's row");
    assert!(!text_pointer_at(&view, geo(), 1, 10), "a virtual line");
    assert!(!text_pointer_at(&view, geo(), 3, 10), "a `~` filler");
    assert!(!text_pointer_at(&view, geo(), 10, 10), "past the last row");
}
