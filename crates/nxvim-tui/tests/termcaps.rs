//! Tier 1: reading one round of terminal capability answers.
//!
//! Startup asks the terminal everything at once and reads the replies back in a
//! single pass, terminated by the Device Status Report it sends last (see
//! `termquery`). What that pass concludes decides whether modified keys reach the
//! server as distinct keys, whether `"+y` can cross an ssh hop, and which graphics
//! protocol image previews use — so the reply parsing is worth pinning down
//! directly. Black-box over the public parsers, so no terminal is involved.

use nxvim_tui::{has_status_report, parse_term_caps, term_names_a_multiplexer};

/// A tmux pane's answers: it replies to the device attributes and the status
/// report itself, ignores `XTGETTCAP` and the kitty queries entirely, and reports
/// its cell size.
const TMUX_REPLY: &[u8] = b"\x1bP>|tmux 3.7b\x1b\\\x1b[?1;2;4c\x1b[6;16;8t\x1b[0n";

/// A modern terminal that implements everything asked.
const RICH_REPLY: &[u8] = b"\x1b[?0u\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t\x1b[?62;4;52c\x1b[0n";

#[test]
fn a_terminal_that_answers_everything_is_read_as_capable() {
    let caps = parse_term_caps(RICH_REPLY);
    assert!(caps.kitty_keyboard, "answered the enhancement flags query");
    assert!(caps.osc52, "device attributes list 52");
    assert!(caps.sixel, "device attributes list 4");
    assert!(caps.kitty_graphics, "the graphics query came back OK");
    assert_eq!(caps.cell_size, Some((10, 20)), "CSI 6;<h>;<w>t is (w, h)");
}

#[test]
fn silence_is_read_as_unsupported() {
    // Nothing came back at all: every capability must read false rather than be
    // assumed. A wrong "yes" here is a yank that silently goes nowhere and a
    // keymap that waits for a key the terminal never sends.
    let caps = parse_term_caps(b"");
    assert_eq!(caps, Default::default());
}

#[test]
fn unanswered_questions_do_not_bleed_into_answered_ones() {
    // The tmux case, and the whole point of asking in one round: the questions it
    // ignores must simply come back false, without costing anything or corrupting
    // the answers that did arrive.
    let caps = parse_term_caps(TMUX_REPLY);
    assert!(!caps.kitty_keyboard);
    assert!(!caps.osc52);
    assert!(!caps.kitty_graphics);
    assert!(caps.sixel, "tmux does list 4 in its device attributes");
    assert_eq!(caps.cell_size, Some((8, 16)));
}

#[test]
fn xtgettcap_can_carry_the_clipboard_answer_alone() {
    // A terminal whose device attributes say nothing about the clipboard but whose
    // `Ms` capability *is* an OSC 52 sequence: `4d73` = "Ms", value `\x1b]52`.
    let reply = b"\x1b[?62;4c\x1bP1+r4d73=1b5d3532\x1b\\\x1b[0n";
    assert!(parse_term_caps(reply).osc52);
}

#[test]
fn a_graphics_query_that_failed_is_not_support() {
    // kitty answers `OK` on success and an error code otherwise; anything but OK
    // must not select the protocol, or every preview paints garbage.
    let reply = b"\x1b_Gi=31;ENOENT:whatever\x1b\\\x1b[0n";
    assert!(!parse_term_caps(reply).kitty_graphics);
}

#[test]
fn a_zero_cell_size_is_no_answer() {
    // A cell size of zero can't convert pixels into cells; treating it as an answer
    // would divide by it. Fall back to halfblocks instead.
    assert_eq!(parse_term_caps(b"\x1b[6;0;0t\x1b[0n").cell_size, None);
}

#[test]
fn the_status_report_terminates_a_round() {
    // The sentinel is what lets the read stop instead of waiting out the timeout on
    // every terminal that ignores some question. Both forms end the round: `0n` is
    // "ok", `3n` is "not ok" — either way the terminal has nothing left to say.
    assert!(has_status_report(TMUX_REPLY));
    assert!(has_status_report(b"\x1b[3n"));
    assert!(!has_status_report(b"\x1b[?1;2;4c"));
    // A cursor position report is not a status report — stopping on one would cut
    // the round short and lose the answers still in flight behind it.
    assert!(!has_status_report(b"\x1b[12;40R"));
}

#[test]
fn answers_are_found_after_unrelated_output() {
    // The replies need not arrive alone or in order: a terminal is free to write
    // whatever else it had to say around them.
    let reply = b"noise\x1b[6;20;10tmore\x1b[?0u\x1b[?62;52c\x1b[0n";
    let caps = parse_term_caps(reply);
    assert!(caps.kitty_keyboard);
    assert!(caps.osc52);
    assert_eq!(caps.cell_size, Some((10, 20)));
}

#[test]
fn tmux_names_itself_in_its_version_reply() {
    // What answered the queries decides how much its silence is worth: tmux can
    // only speak for tmux, so "did not mention the clipboard" is not "the terminal
    // can't do it" (see `osc52_enabled`).
    assert!(parse_term_caps(TMUX_REPLY).multiplexer);
}

#[test]
fn a_real_terminal_is_not_mistaken_for_a_multiplexer() {
    // Every emulator that implements XTVERSION answers with its own name; only a
    // multiplexer's silence needs the benefit of the doubt, so a name that isn't
    // one must not get it.
    for reply in [
        &b"\x1bP>|WezTerm 20240203\x1b\\\x1b[0n"[..],
        &b"\x1bP>|kitty(0.32.2)\x1b\\\x1b[0n"[..],
        &b"\x1bP>|foot(1.16.2)\x1b\\\x1b[0n"[..],
        // No XTVERSION answer at all.
        &b"\x1b[?62;4c\x1b[0n"[..],
    ] {
        assert!(!parse_term_caps(reply).multiplexer, "reply: {reply:?}");
    }
}

#[test]
fn term_is_the_fallback_for_a_multiplexer_that_answers_nothing() {
    // GNU screen predates XTVERSION, and `TERM` is what survives the ssh hop that
    // `$TMUX` does not — so the entry name still has to be read.
    for term in ["tmux", "tmux-256color", "screen", "screen.xterm-256color"] {
        assert!(term_names_a_multiplexer(term), "{term}");
    }
    // A real terminal's entry must not read as one — including names that merely
    // start with the same letters.
    for term in [
        "xterm-256color",
        "wezterm",
        "foot",
        "screenshot-term",
        "dumb",
    ] {
        assert!(!term_names_a_multiplexer(term), "{term}");
    }
}
