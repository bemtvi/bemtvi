//! Tier-1 tests for the client-side resize-handle hit-test — the pure geometry a
//! GUI/web client runs on hover to pick a resize cursor. Black-box, no server: we
//! build a [`View`] + [`Geometry`] by hand and assert the cell→cursor mapping,
//! the client-side mirror of the server's `resize_handle_at`/`dock_handle_at`.

use nxvim_view::{
    resize_handle_at, Geometry, ResizeCursor, Separator, View, WinRect, WindowRegion, WindowView,
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
