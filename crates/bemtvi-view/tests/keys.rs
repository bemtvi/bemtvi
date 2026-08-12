//! Tier-1 tests for the input notation encoder — the client side of the
//! `btv_input` contract. Black-box, no server: we assert the exact notation
//! strings, which must be ones the server's `parse_keys` tokenizes back to the
//! same key (its `<...>` scan ends at the *first* `>`, so a literal `>` can
//! never appear inside a modifier form).

use bemtvi_view::{notation, Key};

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

#[test]
fn shift_with_ctrl_or_alt_is_a_modifier_flag() {
    // With ctrl/alt also held, shift is a *distinct* modifier the kitty keyboard
    // protocol reports separately — it is NOT folded into the character. It must
    // be carried as the explicit `S-` flag so the server matches a `<C-S-c>` /
    // `<A-S-c>` mapping (neovim's model). Dropping it here sent bare `<C-c>` and
    // the remap could never fire.
    assert_eq!(notation(true, false, true, Key::Char('c')), "<C-S-c>");
    assert_eq!(notation(false, true, true, Key::Char('c')), "<A-S-c>");
    assert_eq!(notation(true, true, true, Key::Char('c')), "<C-A-S-c>");
}

#[test]
fn shifted_modified_letter_is_lowercased() {
    // The platform upcases Shift+c to `C`; a *modified* key does not distinguish
    // letter case (neovim's model — `parse_special` lowercases too), so the
    // uppercase form normalizes to `<C-S-c>`, matching the same mapping.
    assert_eq!(notation(true, false, true, Key::Char('C')), "<C-S-c>");
    assert_eq!(notation(false, true, true, Key::Char('C')), "<A-S-c>");
}

#[test]
fn bare_shifted_char_stays_baked() {
    // WITHOUT ctrl/alt, shift is folded into the character by the platform
    // (`Shift+a` → `A`), so it carries no `S-` and is sent literally.
    assert_eq!(notation(false, false, true, Key::Char('A')), "A");
}
