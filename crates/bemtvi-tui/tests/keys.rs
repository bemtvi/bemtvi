//! Tier 1: the crossterm `KeyEvent` -> vim key-notation translation, tested as
//! the public function the client uses. Black-box, no process, no timing.

use bemtvi_tui::encode_key;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn note(code: KeyCode, mods: KeyModifiers) -> Option<String> {
    encode_key(KeyEvent::new(code, mods))
}

#[test]
fn plain_char_is_itself() {
    assert_eq!(
        note(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(),
        Some("a")
    );
}

#[test]
fn special_keys_use_angle_notation() {
    assert_eq!(
        note(KeyCode::Esc, KeyModifiers::NONE).as_deref(),
        Some("<Esc>")
    );
    assert_eq!(
        note(KeyCode::Enter, KeyModifiers::NONE).as_deref(),
        Some("<CR>")
    );
    assert_eq!(
        note(KeyCode::Backspace, KeyModifiers::NONE).as_deref(),
        Some("<BS>")
    );
    assert_eq!(
        note(KeyCode::Tab, KeyModifiers::NONE).as_deref(),
        Some("<Tab>")
    );
}

#[test]
fn ctrl_and_alt_get_prefixed() {
    assert_eq!(
        note(KeyCode::Char('w'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-w>")
    );
    assert_eq!(
        note(KeyCode::Char('x'), KeyModifiers::ALT).as_deref(),
        Some("<A-x>")
    );
}

#[test]
fn literal_less_than_is_escaped() {
    assert_eq!(
        note(KeyCode::Char('<'), KeyModifiers::NONE).as_deref(),
        Some("<lt>")
    );
}

#[test]
fn navigation_keys_use_angle_notation() {
    assert_eq!(
        note(KeyCode::Left, KeyModifiers::NONE).as_deref(),
        Some("<Left>")
    );
    assert_eq!(
        note(KeyCode::Right, KeyModifiers::NONE).as_deref(),
        Some("<Right>")
    );
    assert_eq!(
        note(KeyCode::Up, KeyModifiers::NONE).as_deref(),
        Some("<Up>")
    );
    assert_eq!(
        note(KeyCode::Down, KeyModifiers::NONE).as_deref(),
        Some("<Down>")
    );
    assert_eq!(
        note(KeyCode::Home, KeyModifiers::NONE).as_deref(),
        Some("<Home>")
    );
    assert_eq!(
        note(KeyCode::End, KeyModifiers::NONE).as_deref(),
        Some("<End>")
    );
    assert_eq!(
        note(KeyCode::PageUp, KeyModifiers::NONE).as_deref(),
        Some("<PageUp>")
    );
    assert_eq!(
        note(KeyCode::PageDown, KeyModifiers::NONE).as_deref(),
        Some("<PageDown>")
    );
    assert_eq!(
        note(KeyCode::Delete, KeyModifiers::NONE).as_deref(),
        Some("<Del>")
    );
}

#[test]
fn unmapped_keys_return_none() {
    // A key code with no vim-notation mapping is dropped (the caller ignores it).
    // (`Insert` has no notation here; F-keys used to be here too — see below.)
    assert_eq!(note(KeyCode::Insert, KeyModifiers::NONE), None);
}

#[test]
fn function_keys_map_to_angle_notation() {
    // Regression: crossterm reports F-keys as `KeyCode::F(n)`, but encode_key had no
    // arm for them, so they hit the catch-all and were dropped — `<F5>` (and the dap
    // plugin's default <F5>/<F10>/<F11> bindings) never reached the server in the TUI.
    assert_eq!(
        note(KeyCode::F(5), KeyModifiers::NONE).as_deref(),
        Some("<F5>")
    );
    assert_eq!(
        note(KeyCode::F(1), KeyModifiers::NONE).as_deref(),
        Some("<F1>")
    );
    assert_eq!(
        note(KeyCode::F(12), KeyModifiers::NONE).as_deref(),
        Some("<F12>")
    );
    // Modifiers wrap the named key (`<S-F5>`, `<C-F6>`).
    assert_eq!(
        note(KeyCode::F(5), KeyModifiers::SHIFT).as_deref(),
        Some("<S-F5>")
    );
    assert_eq!(
        note(KeyCode::F(6), KeyModifiers::CONTROL).as_deref(),
        Some("<C-F6>")
    );
}

#[test]
fn shift_tab_is_back_tab_notation() {
    // crossterm reports Shift+Tab as the dedicated `BackTab` code (no SHIFT
    // modifier on most terminals); it must reach the server as `<S-Tab>`, the
    // notation the cmdline wildmenu / snippet tabstop nav bind. Previously it hit
    // the catch-all and was dropped, so Shift+Tab did nothing.
    assert_eq!(
        note(KeyCode::BackTab, KeyModifiers::NONE).as_deref(),
        Some("<S-Tab>")
    );
    // Some terminals (kitty/CSI-u protocols) instead send Tab + SHIFT — same key.
    assert_eq!(
        note(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(),
        Some("<S-Tab>")
    );
    assert_eq!(
        note(KeyCode::Tab, KeyModifiers::SHIFT).as_deref(),
        Some("<S-Tab>")
    );
}

#[test]
fn legacy_c0_control_bytes_map_to_vim_notation() {
    // A legacy terminal (no kitty keyboard protocol — bemtvi doesn't enable it)
    // sends the C0 bytes 0x1C..=0x1F for CTRL-\, CTRL-], CTRL-^ and CTRL-_.
    // crossterm decodes those bytes as Ctrl+'4'..'7' (see its unix parse.rs), so
    // without remapping, <C-]> (help tag jump), <C-^> (alternate file) and the
    // others never reach the server and the keypress does nothing.
    assert_eq!(
        note(KeyCode::Char('4'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-\\>")
    );
    assert_eq!(
        note(KeyCode::Char('5'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-]>")
    );
    assert_eq!(
        note(KeyCode::Char('6'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-^>")
    );
    assert_eq!(
        note(KeyCode::Char('7'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-_>")
    );
}

#[test]
fn legacy_c0_control_bytes_keep_the_alt_modifier() {
    // Alt+Ctrl+] arrives as an ESC-prefixed 0x1D, which crossterm decodes as
    // Alt+Ctrl+'5' (the same C0 mapping as above, plus ALT for the ESC prefix).
    // The remap must keep the Alt modifier and still canonicalize the C0 byte —
    // not fall through to `<C-A-5>`, which matches no `<C-A-]>` mapping.
    assert_eq!(
        note(
            KeyCode::Char('5'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )
        .as_deref(),
        Some("<C-A-]>")
    );
    assert_eq!(
        note(
            KeyCode::Char('4'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )
        .as_deref(),
        Some("<C-A-\\>")
    );
}

#[test]
fn combined_ctrl_alt_prefixes_both() {
    assert_eq!(
        note(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )
        .as_deref(),
        Some("<C-A-a>")
    );
}

// ---------------------------------------------- the C0 fallback is protocol-gated
//
// The `Char('4'..'7') + CONTROL` remap above exists for a LEGACY terminal, where
// those events really are the 0x1C..0x1F control bytes. Under the kitty keyboard
// protocol the same crossterm event means the literal key: the terminal reports
// Ctrl+4 as `CSI 52;5u` and Ctrl+\ as its own CSI-u sequence, so remapping there
// folds four real keys onto `<C-\>`/`<C-]>`/`<C-^>`/`<C-_>` — destroying exactly
// the distinguishability the protocol is enabled to buy.

fn note_kitty(code: KeyCode, mods: KeyModifiers) -> Option<String> {
    bemtvi_tui::encode_key_with(KeyEvent::new(code, mods), true)
}

#[test]
fn under_the_kitty_protocol_ctrl_digits_stay_digits() {
    for (ch, legacy) in [
        ('4', "<C-\\>"),
        ('5', "<C-]>"),
        ('6', "<C-^>"),
        ('7', "<C-_>"),
    ] {
        // Legacy: the C0 remap applies.
        assert_eq!(
            note(KeyCode::Char(ch), KeyModifiers::CONTROL).as_deref(),
            Some(legacy),
            "without the protocol, Ctrl+{ch} is the C0 byte"
        );
        // Kitty on: the same event is the real Ctrl+digit.
        assert_eq!(
            note_kitty(KeyCode::Char(ch), KeyModifiers::CONTROL).as_deref(),
            Some(format!("<C-{ch}>").as_str()),
            "under the protocol, Ctrl+{ch} is the digit key, not the C0 byte"
        );
    }
}

#[test]
fn the_default_entry_point_still_assumes_the_legacy_encoding() {
    // `encode_key` is the protocol-less spelling of `encode_key_with`, so the
    // clients that never negotiated the protocol keep the legacy behaviour.
    assert_eq!(
        note(KeyCode::Char('4'), KeyModifiers::CONTROL),
        bemtvi_tui::encode_key_with(
            KeyEvent::new(KeyCode::Char('4'), KeyModifiers::CONTROL),
            false
        )
    );
}

// ------------------------------------------- xterm reports Ctrl chords as C0 chars
//
// The other CSI-u shape: an xterm-family terminal reports a Ctrl chord by its
// CONTROL codepoint (`CSI 28;5u` for Ctrl+\) rather than the base key's printable
// one. crossterm decodes that as `Char('\x1c') + CONTROL` — a shape the legacy C0
// decode never produces — and the notation encoder had no mapping for it, so it
// emitted a raw control byte inside `<C-…>` that matched no mapping at all: a
// `<C-\>` mapping silently died on those terminals.

#[test]
fn a_control_codepoint_is_folded_back_to_its_key() {
    for (byte, want) in [
        ('\u{1c}', "<C-\\>"),
        ('\u{1d}', "<C-]>"),
        ('\u{1e}', "<C-^>"),
        ('\u{1f}', "<C-_>"),
    ] {
        assert_eq!(
            note(KeyCode::Char(byte), KeyModifiers::CONTROL).as_deref(),
            Some(want),
            "the control codepoint must name the key it folds from"
        );
    }
}

#[test]
fn a_control_letter_codepoint_folds_back_to_its_letter() {
    // 0x01..0x1A are Ctrl+A..Ctrl+Z; neovim's model lowercases the letter.
    assert_eq!(
        note(KeyCode::Char('\u{1}'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-a>")
    );
    assert_eq!(
        note(KeyCode::Char('\u{17}'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-w>"),
        "<C-w> is the window prefix — it must survive an xterm-style report"
    );
}

#[test]
fn nul_folds_onto_ctrl_space() {
    // An XKB-off xterm's keysym for Ctrl+Space / Ctrl+@ is NUL. Folding it onto
    // `<C-Space>` keeps the default completion trigger alive there.
    assert_eq!(
        note(KeyCode::Char('\0'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-space>")
    );
}

#[test]
fn control_codepoint_folding_holds_under_the_kitty_protocol_too() {
    // A terminal only emits a control codepoint for the chord that produces it, so
    // no distinct key is folded onto another — the fold is correct either way.
    assert_eq!(
        note_kitty(KeyCode::Char('\u{1c}'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-\\>")
    );
}

#[test]
fn the_named_control_keys_are_not_folded() {
    // Tab / CR / Esc have their own `KeyCode`s — crossterm maps their codepoints
    // first, so they must never reach the fold and come back as `<C-i>` &c.
    assert_eq!(
        note(KeyCode::Tab, KeyModifiers::NONE).as_deref(),
        Some("<Tab>")
    );
    assert_eq!(
        note(KeyCode::Enter, KeyModifiers::NONE).as_deref(),
        Some("<CR>")
    );
}
