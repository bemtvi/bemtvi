//! Tier-1 tests for the input notation encoder — the client side of the
//! `nx_input` contract. Black-box, no server: we assert the exact notation
//! strings, which must be ones the server's `parse_keys` tokenizes back to the
//! same key (its `<...>` scan ends at the *first* `>`, so a literal `>` can
//! never appear inside a modifier form).

use nxvim_view::{notation, Key};

#[test]
fn modified_gt_uses_its_named_escape() {
    // A literal '>' inside `<...>` terminates the form early on the server
    // (`<C->>` scans as inner `"C-"` and falls apart into five literal keys),
    // so a modified '>' must use the `gt` named escape the server's
    // `parse_special` resolves back to '>'.
    assert_eq!(notation(true, false, false, Key::Char('>')), "<C-gt>");
    assert_eq!(notation(false, true, false, Key::Char('>')), "<A-gt>");
    assert_eq!(notation(true, true, false, Key::Char('>')), "<C-A-gt>");
}

#[test]
fn bare_gt_is_literal() {
    // Unmodified '>' opens no form; it is sent as itself.
    assert_eq!(notation(false, false, false, Key::Char('>')), ">");
}

#[test]
fn bare_lt_is_escaped() {
    // A bare '<' would open a spurious `<...>` form; `<lt>` is its escape.
    assert_eq!(notation(false, false, false, Key::Char('<')), "<lt>");
}

#[test]
fn modified_lt_stays_inline() {
    // Inside a modifier form a literal '<' is unambiguous (the scan only ends
    // at '>'), and this is the form core's `key_to_notation` emits too.
    assert_eq!(notation(true, false, false, Key::Char('<')), "<C-<>");
}
