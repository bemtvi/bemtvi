//! Mouse math, the pure part of the GUI's mouse handling: pixel → cell, and the wheel's
//! notch accumulator. Screen cells are **absolute** (row 0 is the top of the window,
//! tabline included): the server owns the hit-test from a global cell back to a window +
//! buffer position (`grid` is always 0 — bemtvi is single-grid), and subtracts the
//! tabline itself, exactly as it does for the TUI's raw terminal cells.

/// The cell `(col, row)` a physical-pixel position falls in, for cell size
/// `(cell_w, cell_h)` in physical pixels. A negative coordinate (the pointer left
/// the window to the top/left) clamps to `0`. Pure so the conversion is testable
/// without a GPU.
pub fn cell_at(x: f64, y: f64, cell_w: f32, cell_h: f32) -> (u16, u16) {
    let col = (x.max(0.0) as f32 / cell_w.max(1.0)).floor() as u16;
    let row = (y.max(0.0) as f32 / cell_h.max(1.0)).floor() as u16;
    (col, row)
}

/// Add `amount` notches to the running `accum` and return the whole notches to
/// emit now, leaving the fractional remainder in `accum`. A wheel mouse sends one
/// line per detent (`amount` ≈ ±1.0, emitted immediately); a trackpad sends many
/// fractional pixels-as-lines that accumulate until they cross a whole line, so a
/// slow drag still scrolls smoothly one row at a time rather than not at all.
/// Truncation is toward zero, so direction never flips from rounding.
pub fn drain_notches(amount: f32, accum: &mut f32) -> i32 {
    *accum += amount;
    let whole = accum.trunc();
    *accum -= whole;
    whole as i32
}

/// The `btv_input_mouse` wheel **action** for a vertical notch count: `"down"`
/// when positive, `"up"` when negative. mitsuami's positive scroll goes towards
/// the end of the content (later lines), with the user's scrolling direction
/// already applied — see mitsuami's `ScrollDelta`. `None`
/// for zero.
pub fn vertical_action(notches: i32) -> Option<&'static str> {
    match notches.signum() {
        1 => Some("down"),
        -1 => Some("up"),
        _ => None,
    }
}

/// The `btv_input_mouse` wheel **action** for a horizontal notch count: `"right"`
/// when positive, `"left"` when negative (mitsuami's positive x goes towards the
/// content's right end). `None` for zero.
pub fn horizontal_action(notches: i32) -> Option<&'static str> {
    match notches.signum() {
        1 => Some("right"),
        -1 => Some("left"),
        _ => None,
    }
}
