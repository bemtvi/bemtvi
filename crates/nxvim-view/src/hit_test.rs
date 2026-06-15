//! Client-side hit-test for draggable resize handles.
//!
//! A client wants to show a resize cursor while the pointer hovers a divider it
//! can drag — a window-split separator, a status line with a window below it, or a
//! dock band edge. The authoritative hit-test lives in the server
//! (`Editor::resize_handle_at` + `dock_handle_at`), but querying it per pointer
//! move would mean a round-trip on every mouse motion. The geometry is already in
//! the [`View`] the client renders, so this mirrors the server's hit-test on that
//! client-side model — the same band math the renderer runs to place regions, run
//! backwards from a cell. Keep it in step with the core hit-test it mirrors.

use crate::view::{View, WindowRegion};

/// The cursor a client shows over a draggable separator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeCursor {
    /// A vertical divider — dragging it resizes widths (CSS `col-resize`, winit
    /// `EwResize`).
    Col,
    /// A horizontal divider — dragging it resizes heights (CSS `row-resize`, winit
    /// `NsResize`).
    Row,
}

/// The frame geometry the hit-test needs that the [`View`] doesn't itself carry:
/// the grid size and the chrome row counts the client derives when it lays the
/// frame out (the same values its renderer computes). `rows` is the windows-area
/// height — the grid height minus the command line.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub tabline_rows: u16,
    pub panel_rows: u16,
    pub global_status_rows: u16,
}

/// One cell of separator reservation toward the main area per open dock — `n + 1`
/// for an open dock (`n > 0`), `0` when closed. Mirrors `DockBands::reserved`.
fn reserved(n: u16) -> u16 {
    if n > 0 {
        n + 1
    } else {
        0
    }
}

/// Whether the screen cell `(row, col)` is over a draggable separator, and which
/// way it resizes — or `None` over an ordinary cell. Mirrors the server's
/// `Editor::resize_handle_at` (split dividers + status-line handles) and
/// `dock_handle_at` (dock band edges) on the client-side [`View`], so a client can
/// set a resize cursor on hover without a server round-trip.
pub fn resize_handle_at(view: &View, geo: Geometry, row: u16, col: u16) -> Option<ResizeCursor> {
    // A dock edge sits *between* regions, where no window split can be — so the two
    // hit-tests are disjoint and the order doesn't matter; check edges first.
    dock_edge_at(view, geo, row, col).or_else(|| split_handle_at(view, geo, row, col))
}

/// The middle band (left dock | main | right docks) vertical span `(top, height)`
/// — the rows the left/right dock edges run down, mirroring `region_geoms`.
fn middle_band(view: &View, geo: Geometry) -> (u16, u16) {
    let chrome = geo.tabline_rows + geo.panel_rows + geo.global_status_rows;
    let mid_y = reserved(view.dock_top) + geo.tabline_rows;
    let mid_h = geo
        .rows
        .saturating_sub(reserved(view.dock_top))
        .saturating_sub(reserved(view.dock_bottom))
        .saturating_sub(chrome)
        .max(1);
    (mid_y, mid_h)
}

/// A dock band edge — the separator between an open dock and the main area. Ports
/// `Editor::dock_handle_at`: left/right edges are vertical (resize width) and span
/// the middle band; top/bottom edges are horizontal (resize height) and span the
/// full width.
fn dock_edge_at(view: &View, geo: Geometry, row: u16, col: u16) -> Option<ResizeCursor> {
    let (dl, dr, dt, db) = (
        view.dock_left,
        view.dock_right,
        view.dock_top,
        view.dock_bottom,
    );
    let (mid_y, mid_h) = middle_band(view, geo);
    let in_mid = row >= mid_y && row < mid_y + mid_h;
    if dl > 0 && in_mid && col == dl {
        return Some(ResizeCursor::Col);
    }
    if dr > 0 && in_mid && col == geo.cols.saturating_sub(reserved(dr)) {
        return Some(ResizeCursor::Col);
    }
    if dt > 0 && col < geo.cols && row == dt {
        return Some(ResizeCursor::Row);
    }
    if db > 0 && col < geo.cols && row == geo.rows.saturating_sub(reserved(db)) {
        return Some(ResizeCursor::Row);
    }
    None
}

/// A split divider inside a region — a window separator, or the status row just
/// above a horizontal one (vim's status-line drag, where the window above is
/// grabbed). Ports `Editor::resize_handle_at`: separators are region-relative, so
/// each is offset by its region's absolute origin first.
fn split_handle_at(view: &View, geo: Geometry, row: u16, col: u16) -> Option<ResizeCursor> {
    for sep in &view.separators {
        let (ox, oy) = region_origin(view, geo, sep.region);
        if sep.vertical {
            let sx = ox + sep.x;
            if col == sx && row >= oy + sep.y && row < oy + sep.y + sep.length {
                return Some(ResizeCursor::Col);
            }
        } else {
            let sy = oy + sep.y;
            let in_x = col >= ox + sep.x && col < ox + sep.x + sep.length;
            // The separator row resizes the split; so does the status row one cell
            // above it (the status-line handle for the window directly above).
            if in_x && (row == sy || row + 1 == sy) {
                return Some(ResizeCursor::Row);
            }
        }
    }
    None
}

/// The absolute screen origin of a region's window-tree area (below its own
/// tabline row, where present), the cell a window/separator of that region offsets
/// against. Mirrors `Editor::region_geoms`.
fn region_origin(view: &View, geo: Geometry, region: WindowRegion) -> (u16, u16) {
    let (dl, dr) = (view.dock_left, view.dock_right);
    let reserved_left = reserved(dl);
    let (mid_y, mid_h) = middle_band(view, geo);
    let main_w = geo
        .cols
        .saturating_sub(reserved_left)
        .saturating_sub(reserved(dr))
        .max(1);
    // The bottom dock's band content starts past the middle band, the global status
    // line, and its own separator cell.
    let bottom_y = mid_y + mid_h + geo.global_status_rows + 1;
    // A dock reserves its band's first row for its own tabline when it has more than
    // one of its own tabs and the band is tall enough to spare a row.
    let dock_tl = |region, band: u16| -> u16 {
        let rt = match region {
            WindowRegion::DockLeft => &view.region_tablines.left,
            WindowRegion::DockRight => &view.region_tablines.right,
            WindowRegion::DockTop => &view.region_tablines.top,
            WindowRegion::DockBottom => &view.region_tablines.bottom,
            WindowRegion::Main => return 0,
        };
        u16::from(!rt.tabs.is_empty() && band > 1)
    };
    match region {
        WindowRegion::Main => (reserved_left, mid_y),
        WindowRegion::DockLeft => (0, mid_y + dock_tl(region, mid_h)),
        WindowRegion::DockRight => (reserved_left + main_w + 1, mid_y + dock_tl(region, mid_h)),
        WindowRegion::DockTop => (0, dock_tl(region, view.dock_top)),
        WindowRegion::DockBottom => (0, bottom_y + dock_tl(region, view.dock_bottom)),
    }
}
