//! Tier 1: reading a terminal's answer to "can you set the clipboard?".
//!
//! The reply decides whether the client tells the server it can carry `"+` / `"*`
//! over OSC 52 — the only clipboard an ssh session can reach. Getting it wrong in
//! either direction is a real failure: a false negative leaves the user with "no
//! clipboard provider" on a terminal that works, and a false positive makes every
//! yank *look* copied while the text goes nowhere. Black-box over the parsing the
//! probe uses, so no terminal is involved.

use bemtvi_tui::{da1_advertises_osc52, xtgettcap_advertises_osc52};

/// Encode `s` the way XTGETTCAP carries a capability value.
fn hex(s: &[u8]) -> String {
    s.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn da1_listing_52_is_clipboard_capable() {
    // xterm built with the clipboard extension reports 52 among its attributes.
    assert!(da1_advertises_osc52(b"\x1b[?63;1;2;4;6;9;15;22;52;29c"));
}

#[test]
fn da1_without_52_is_not() {
    // A terminal that supports plenty but says nothing about the clipboard.
    assert!(!da1_advertises_osc52(b"\x1b[?62;4;22c"));
}

#[test]
fn da1_does_not_match_52_inside_another_parameter() {
    // Substring matching would read `152` / `520` as clipboard support and emit an
    // escape the terminal never claimed to understand.
    assert!(!da1_advertises_osc52(b"\x1b[?152;520;5c"));
}

#[test]
fn da1_is_found_after_unrelated_output() {
    // The reply need not arrive alone: a slow terminal can prefix it with whatever
    // else it had to say.
    assert!(da1_advertises_osc52(b"junk\x1b[?62;52c"));
}

#[test]
fn a_truncated_da1_is_not_capable() {
    // No terminator ⇒ no complete answer; assuming support from half a reply is
    // exactly the false positive that copies into a void.
    assert!(!da1_advertises_osc52(b"\x1b[?62;52"));
    assert!(!da1_advertises_osc52(b""));
}

#[test]
fn xtgettcap_reporting_an_osc52_ms_is_capable() {
    // `Ms` (4d73 hex) = the sequence that sets the clipboard. This one *is* OSC 52.
    let reply = format!("\x1bP1+r4D73={}\x1b\\", hex(b"\x1b]52;%p1%s;%p2%s\x07"));
    assert!(xtgettcap_advertises_osc52(reply.as_bytes()));
}

#[test]
fn xtgettcap_reporting_some_other_ms_is_not() {
    // A terminal whose clipboard mechanism isn't OSC 52 must not be sent one —
    // bemtvi speaks no other clipboard escape.
    let reply = format!("\x1bP1+r4D73={}\x1b\\", hex(b"\x1b]9;clip\x07"));
    assert!(!xtgettcap_advertises_osc52(reply.as_bytes()));
}

#[test]
fn an_unsupported_xtgettcap_capability_is_not() {
    // `0+r` is the "I don't know that capability" answer.
    assert!(!xtgettcap_advertises_osc52(b"\x1bP0+r4D73\x1b\\"));
    // …as is a malformed value (odd-length hex) or no reply at all.
    assert!(!xtgettcap_advertises_osc52(b"\x1bP1+r4D73=1b5d3\x1b\\"));
    assert!(!xtgettcap_advertises_osc52(b""));
}

#[test]
fn another_csi_question_report_is_not_read_as_device_attributes() {
    // A mode report (`CSI ? 52 ; 1 $ y`) carries parameters too. Reading one as
    // device attributes would claim clipboard support the terminal never offered.
    assert!(!da1_advertises_osc52(b"\x1b[?52;1$y"));
    // …and a real DA1 arriving after one is still found.
    assert!(da1_advertises_osc52(b"\x1b[?2004;1$y\x1b[?62;52c"));
}

// ---------------------------------------------------------------------------
// Tier 2: what the client is willing to write back at the terminal
// ---------------------------------------------------------------------------
//
// `btv_ui_send` is the one notification whose payload the client writes to the
// terminal *verbatim* — it exists so a `"+` yank can reach the system clipboard
// over OSC 52. A verbatim write is a lot of trust to place in the far end of an
// RPC socket: escape sequences can rebind keys (so the next keystroke runs a
// command), switch to the alternate screen, request a reply the terminal types
// back as input, or read the clipboard *out* with OSC 52's `?` query. Over a
// daemon or an ssh split, the far end is not necessarily the machine the user is
// sitting at.
//
// So the client whitelists the single sequence family it actually needs, and
// drops everything else.

use bemtvi_tui::is_osc52;

/// Base64 of `s`, the way the server's `osc52_sequence` encodes a yank.
fn osc52(payload: &str) -> String {
    format!("\x1b]52;c;{payload}\x1b\\")
}

#[test]
fn the_sequence_the_server_actually_sends_is_allowed() {
    // `osc52_sequence("hello")` — the standard alphabet, with `=` padding.
    assert!(is_osc52(&osc52("aGVsbG8=")));
    // The full alphabet, including the two non-alphanumeric characters and `=`.
    assert!(is_osc52(&osc52("abcXYZ0189+/==")));
    // OSC 52's "clear the clipboard" — an empty payload, which is exactly what an
    // empty yank encodes to. Dropping it would silently break clearing.
    assert!(is_osc52(&osc52("")));

    // The exact byte strings the server-side suite pins for a real yank
    // (`bemtvi-server/tests/editing/clipboard.rs`). Copied verbatim on purpose:
    // the whitelist and the encoder are in different crates, so if either side's
    // shape drifts, one of the two suites has to notice.
    assert!(is_osc52("\x1b]52;c;aGVsbG8gd29ybGQK\x1b\\"));
    assert!(is_osc52("\x1b]52;c;aA==\x1b\\"));
}

#[test]
fn an_unrelated_escape_sequence_is_not_allowed() {
    // Alternate screen, cursor moves, colours: harmless-looking, but nothing the
    // server has any business writing through this channel.
    assert!(!is_osc52("\x1b[?1049h"));
    assert!(!is_osc52("\x1b[2J"));
    assert!(!is_osc52("\x1b[31m"));
    // A window-title set (OSC 0) — a different OSC entirely.
    assert!(!is_osc52("\x1b]0;pwned\x1b\\"));
    // Plain text is not an escape sequence either.
    assert!(!is_osc52("hello"));
    assert!(!is_osc52(""));
}

#[test]
fn a_key_rebinding_sequence_is_not_allowed() {
    // OSC 52's dangerous neighbours. Terminals that honour these turn a single
    // notification into command execution on the user's next keypress.
    assert!(!is_osc52("\x1b]52;c;aGk=\x07\x1bP+q636f6c6f7273\x1b\\")); // trailing junk
                                                                       // DECUDK — programmable function keys (a key that types `rm -rf ~`).
    assert!(!is_osc52("\x1bP1;1|11/726D202D7266207E\x1b\\"));
}

#[test]
fn the_clipboard_read_query_is_not_allowed() {
    // OSC 52 with `?` asks the terminal to send the clipboard's *contents* back as
    // input — the exfiltration direction. It is shaped almost exactly like the
    // write we do allow, and `?` is not in the base64 alphabet, which is what
    // separates them.
    assert!(!is_osc52("\x1b]52;c;?\x1b\\"));
    assert!(!is_osc52("\x1b]52;p;?\x1b\\"));
}

#[test]
fn a_payload_smuggling_a_second_escape_is_not_allowed() {
    // The reason the payload is checked against the base64 alphabet rather than
    // just "ends with ST": a payload containing ESC (or BEL) terminates the OSC
    // early on a real terminal, and everything after it is interpreted as a fresh
    // sequence. Neither byte is in the alphabet, so neither can appear.
    assert!(!is_osc52("\x1b]52;c;aGk=\x1b\\\x1b[?1049h\x1b\\"));
    assert!(!is_osc52("\x1b]52;c;aGk=\x07\x1b[2J\x1b\\"));
    assert!(!is_osc52(&osc52("aGk=\x1b[31m")));
    assert!(!is_osc52(&osc52("aG\nk=")));
}

#[test]
fn only_the_c_selection_is_allowed() {
    // bemtvi's `"+`/`"*` share one provider and always write selection `c`. A
    // sequence naming any other selection did not come from our encoder.
    assert!(!is_osc52("\x1b]52;p;aGk=\x1b\\"));
    assert!(!is_osc52("\x1b]52;s0;aGk=\x1b\\"));
    // …and one with no selection field at all.
    assert!(!is_osc52("\x1b]52;aGk=\x1b\\"));
}

#[test]
fn a_sequence_missing_its_terminator_is_not_allowed() {
    // An unterminated OSC leaves the terminal consuming whatever follows — the
    // user's own subsequent output — as part of the sequence.
    assert!(!is_osc52("\x1b]52;c;aGk="));
    assert!(!is_osc52("\x1b]52;c;aGk=\x07")); // BEL-terminated: not the form we emit
}
