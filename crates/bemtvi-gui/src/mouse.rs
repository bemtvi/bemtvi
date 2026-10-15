//! Mouse gestures → server RPC, the GUI analogue of the TUI's mouse handling.
//!
//! mitsuami reports the pointer in points over the editor's surface, with the
//! modifiers held; the client scales it to pixels and tracks the latest one (a
//! wheel event carries no position) and feeds them here. These helpers are the *pure* part — pixel→cell conversion math, the
//! `btv_input_mouse` modifier string, the overlay hit-test rectangles, and the
//! wheel-notch accumulator — kept toolkit-light and unit-tested in
//! `tests/mouse.rs`. The actual `rpc.notify` wiring lives in `lib`.
//!
//! Screen cells are **absolute** (row 0 is the top of the window, tabline
//! included): the server owns the hit-test from a global cell back to a window +
//! buffer position (`grid` is always 0 — bemtvi is single-grid), and subtracts the
//! tabline itself, exactly as it does for the TUI's raw terminal cells.

use mitsuami::prelude::{Modifiers, MouseButton};

/// The `btv_input_mouse` button name for a pointer button, or `None` for one the
/// server has no gesture for (back/forward/other). `"left"` carries the
/// focus-follows-click + selection machinery; `"right"` is the `'mousemodel'`
/// branch (extend / popup-setpos); `"middle"` pastes the `"*` register. Mirrors
/// the TUI, which forwards the same three names.
pub fn button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Primary => Some("left"),
        MouseButton::Secondary => Some("right"),
        MouseButton::Middle => Some("middle"),
        _ => None,
    }
}

/// The `btv_input_mouse` modifier string for the live modifier state — the shared
/// [`bemtvi_view::mouse_modifier`] over mitsuami's flags. (Shift is a real chord
/// here, unlike typed text where the platform folds it into the character — a mouse
/// event carries no character to fold it into.)
pub fn mouse_modifier(mods: Modifiers) -> String {
    bemtvi_view::mouse_modifier(mods.control, mods.shift, mods.alt)
}

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
/// already applied — see [`ScrollDelta`](mitsuami::prelude::ScrollDelta). `None`
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
