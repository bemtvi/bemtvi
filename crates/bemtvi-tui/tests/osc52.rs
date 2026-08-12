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
