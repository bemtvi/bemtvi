//! Mouse gestures → server RPC, the GUI analogue of the TUI's mouse handling.
//!
//! winit reports the pointer in physical pixels and never bundles the modifiers
//! or position into the button/wheel events, so the [`crate::App`] event loop
//! tracks the latest cursor pixel and the live [`ModifiersState`] and feeds them
//! here. These helpers are the *pure* part — pixel→cell conversion math, the
//! `nvim_input_mouse` modifier string, the overlay hit-test rectangles, and the
//! wheel-notch accumulator — kept toolkit-light and unit-tested in
//! `tests/mouse.rs`. The actual `rpc.notify` wiring lives in `lib`.
//!
//! Screen cells are **absolute** (row 0 is the top of the window, tabline
//! included): the server owns the hit-test from a global cell back to a window +
//! buffer position (`grid` is always 0 — nxvim is single-grid), and subtracts the
//! tabline itself, exactly as it does for the TUI's raw terminal cells.

use winit::event::MouseButton;
use winit::keyboard::ModifiersState;

/// The `nvim_input_mouse` button name for a winit button, or `None` for one the
/// server has no gesture for (back/forward/other). `"left"` carries the
/// focus-follows-click + selection machinery; `"right"` is the `'mousemodel'`
/// branch (extend / popup-setpos); `"middle"` pastes the `"*` register. Mirrors
/// the TUI, which forwards the same three names.
pub fn button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Left => Some("left"),
        MouseButton::Right => Some("right"),
        MouseButton::Middle => Some("middle"),
        _ => None,
    }
}

/// The `nvim_input_mouse` modifier string for the live modifier state — e.g.
/// Ctrl+Shift → `"CS"`. The server's parser accepts the chars in any order with
/// the `-` separator optional, so concatenation is enough. Mirrors the TUI's
/// `mouse_modifier`; drives shift-click (extend the selection) and Ctrl/Alt
/// gestures. (Shift is a real chord here, unlike key input where winit folds it
/// into the character — a mouse event carries no character to fold it into.)
pub fn mouse_modifier(mods: ModifiersState) -> String {
    let mut s = String::new();
    if mods.control_key() {
        s.push('C');
    }
    if mods.shift_key() {
        s.push('S');
    }
    if mods.alt_key() {
        s.push('A');
    }
    s
}

/// Whether `(col, row)` falls inside the `w`×`h` rect anchored at `(x, y)` — the
/// hit-test shared by the completion popup and its doc preview. Mirrors the TUI's
/// `within`.
pub fn within(col: u16, row: u16, x: u16, y: u16, w: u16, h: u16) -> bool {
    col >= x && col < x + w && row >= y && row < y + h
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

/// The `nvim_input_mouse` wheel **action** for a vertical notch count: `"up"`
/// when positive, `"down"` when negative. winit's positive delta means the
/// content moves down (revealing earlier lines), i.e. a scroll *up* — see
/// [`MouseScrollDelta`](winit::event::MouseScrollDelta). `None` for zero.
pub fn vertical_action(notches: i32) -> Option<&'static str> {
    match notches.signum() {
        1 => Some("up"),
        -1 => Some("down"),
        _ => None,
    }
}

/// The `nvim_input_mouse` wheel **action** for a horizontal notch count: `"left"`
/// when positive, `"right"` when negative (winit's positive-x reveals content to
/// the left). `None` for zero.
pub fn horizontal_action(notches: i32) -> Option<&'static str> {
    match notches.signum() {
        1 => Some("left"),
        -1 => Some("right"),
        _ => None,
    }
}
