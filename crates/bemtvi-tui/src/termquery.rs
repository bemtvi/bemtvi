//! Asking the terminal what it can do, over stdio, without eating the user's
//! keystrokes.
//!
//! Every capability probe has the same shape: write an escape sequence, then read
//! whatever the terminal answers. The trap is the "then read" — a terminal that
//! doesn't implement the query answers *nothing*, and a blocking read parked on
//! stdin swallows the first keys the user types (and, on its way out, can drop the
//! tty back to cooked mode; see the war story in [`crate::images`]). So the wait is
//! always a `poll(2)`: it can wait *without consuming* stdin and leaves nothing
//! behind on timeout, so a mute terminal costs one bounded pause and a fully
//! working keyboard.
//!
//! **Everything is asked ONCE, in one write, and read back in one pass.** Asking
//! serially is what makes a multiplexer feel broken: each unanswered question costs
//! its whole timeout, and the timeouts add up in front of the first frame (under
//! tmux, one `XTGETTCAP` nobody answers used to cost half a second on its own, and
//! a second graphics probe another two). Terminals answer queries in the order they
//! arrive, so a single Device Status Report sent **last** is a sentinel: once its
//! `CSI 0 n` comes back, every question ahead of it has either been answered or
//! never will be, and the read stops. One round trip on any terminal that answers
//! at all; one bounded wait on a terminal that answers nothing.
//!
//! Raw mode must already be on when this runs (`ratatui::init`), or the line
//! discipline holds the reply back — and it must run *before* the input
//! `EventStream` exists, or the two race for the same bytes.

use std::time::Duration;

/// What the terminal said it can do — the answers to one round of questions.
///
/// A field is only true when the terminal *said so*: silence means "no", because
/// every one of these drives a decision that is worse to get wrong optimistically
/// than to leave off (see [`crate::osc52_enabled`] on the vanishing yank).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermCaps {
    /// The kitty keyboard protocol: the terminal answered the progressive-
    /// enhancement flags query, so it can deliver `<C-i>`/`<C-m>`/`<S-CR>`/… as
    /// distinct keys.
    pub kitty_keyboard: bool,
    /// OSC 52 clipboard writes — the only clipboard an ssh session can reach.
    pub osc52: bool,
    /// Sixel graphics (device attribute `4`).
    pub sixel: bool,
    /// The kitty graphics protocol.
    pub kitty_graphics: bool,
    /// One cell's size in pixels, `(width, height)`, from `CSI 16 t`. Terminal
    /// graphics need it to convert an image's pixel size into cells.
    pub cell_size: Option<(u16, u16)>,
    /// A terminal **multiplexer** is between us and the real terminal — tmux or
    /// screen, which answer our queries themselves and can only speak for what
    /// *they* implement, not for the emulator the user is actually looking at.
    ///
    /// It is not a capability but the reason a capability answer can be a floor
    /// rather than the truth; [`crate::osc52_enabled`] is where that matters.
    pub multiplexer: bool,
}

/// How long to wait for the sentinel before giving up on the whole round. Only a
/// terminal that answers *nothing* pays this in full; anything that answers the
/// Device Status Report ends the wait as soon as its reply lands. Generous, because
/// the round trip can cross a couple of ssh hops.
#[cfg(unix)]
const PROBE_WAIT: Duration = Duration::from_millis(1000);

/// Ask the terminal everything, in one round trip (see the module docs).
///
/// The questions, in the order they go out — the sentinel last, so its answer
/// means "that's all there is":
///
/// ```text
/// CSI ? u                     kitty keyboard protocol flags
/// APC _G i=31 … ST            kitty graphics
/// DCS + q 4D73 ST             XTGETTCAP `Ms` (an OSC 52 sequence?)
/// CSI > q                     XTVERSION — who is answering all this?
/// CSI 16 t                    cell size in pixels
/// CSI c                       primary device attributes (sixel, OSC 52)
/// CSI 5 n                     device status report — the sentinel
/// ```
#[cfg(unix)]
pub(crate) fn probe() -> TermCaps {
    let mut query: Vec<u8> = Vec::new();
    query.extend_from_slice(b"\x1b[?u");
    if !graphics_query_suppressed() {
        query.extend_from_slice(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    }
    // Terminal.app echoes sequences it doesn't understand straight back, which
    // would land in the input stream as garbage — never XTGETTCAP it (neovim
    // carves out the same exception).
    if !is_apple_terminal() {
        query.extend_from_slice(b"\x1bP+q4D73\x1b\\");
        query.extend_from_slice(b"\x1b[>q");
    }
    query.extend_from_slice(b"\x1b[16t");
    query.extend_from_slice(b"\x1b[c");
    query.extend_from_slice(b"\x1b[5n");

    let mut caps = match ask(&query, PROBE_WAIT) {
        Some(reply) => parse_term_caps(&reply),
        None => TermCaps::default(),
    };
    // `TERM` is the fallback for a multiplexer that doesn't answer XTVERSION (GNU
    // screen predates it), and it survives the ssh hop that `$TMUX` does not.
    caps.multiplexer |= std::env::var("TERM").is_ok_and(|t| term_names_a_multiplexer(&t));
    caps
}

/// Non-unix: no `poll(2)` to probe with, so ask nothing over stdio and let the
/// callers fall back to their own platform paths (crossterm's console-API keyboard
/// probe, halfblocks images) rather than emit escapes we can't safely read back.
#[cfg(not(unix))]
pub(crate) fn probe() -> TermCaps {
    TermCaps::default()
}

/// Whether a terminal's *graphics* answers should be ignored in favour of what its
/// env says. WezTerm and Konsole both advertise protocols that read worse than
/// their env hint (WezTerm's own iTerm2 support beats the sixel it advertises;
/// Konsole's sixel is buggy), so ratatui-image blacklists both — mirror that here,
/// both to skip asking and to discard the device-attribute answer that arrives
/// anyway (one `CSI c` answers the clipboard question too, so it is always sent).
pub(crate) fn graphics_query_suppressed() -> bool {
    let set = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty());
    set("WEZTERM_EXECUTABLE") || set("KONSOLE_VERSION")
}

#[cfg(unix)]
fn is_apple_terminal() -> bool {
    std::env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal")
}

/// Write `query` to the terminal and read back everything it answers, stopping at
/// the Device Status Report that terminates the round — or `None` when it says
/// nothing within `wait` (it answers no queries at all; the caller treats every
/// capability as unsupported).
///
/// Reading through the sentinel is what keeps stdin clean: a fragment left behind
/// would be delivered to the input parser as garbage keystrokes.
#[cfg(unix)]
fn ask(query: &[u8], wait: Duration) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    use std::time::Instant;

    let mut out = std::io::stdout();
    out.write_all(query).and_then(|()| out.flush()).ok()?;

    let deadline = Instant::now() + wait;
    let mut stdin = std::io::stdin();
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if !stdin_readable_within(remain) {
            return (!reply.is_empty()).then_some(reply);
        }
        // Only consume input that looks like a probe reply. Every answer to our
        // queries starts with ESC; anything else readable here is a keystroke the
        // user typed while we waited, and consuming it would swallow the key —
        // the same failure as the parked read `crate::images` warns about (the
        // wait can be a full second on a terminal that answers nothing). Peek
        // the first byte without removing it; when the peek can't be performed
        // (no data yet, EOF, a platform that rejects MSG_PEEK on stdin) fall
        // back to reading blindly. Escaped keys (an arrow, Alt+letter) can still
        // be eaten, but plain typed text — the common case — is left for the
        // `EventStream`, and the replies that sit behind it are sequences
        // crossterm drops rather than re-delivers as keys.
        if !stdin_starts_with_escape() {
            return (!reply.is_empty()).then_some(reply);
        }
        match stdin.read(&mut buf) {
            // EOF / error: nothing more is coming.
            Ok(0) | Err(_) => return (!reply.is_empty()).then_some(reply),
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
        if has_status_report(&reply) {
            return Some(reply);
        }
    }
}

/// Whether the next byte on stdin is ESC — i.e. the readable input is (part of)
/// a probe reply rather than a keystroke — peeking without consuming it. Returns
/// `true` (read anyway) when the peek can't be performed: a poll/recv race with
/// no data yet, EOF, or a platform that rejects `MSG_PEEK` on stdin. This must
/// never block: `stdin_readable_within` just reported the byte as ready.
#[cfg(unix)]
fn stdin_starts_with_escape() -> bool {
    use std::os::unix::io::AsRawFd;
    let mut byte = 0u8;
    let n = unsafe {
        libc::recv(
            std::io::stdin().as_raw_fd(),
            &mut byte as *mut u8 as *mut libc::c_void,
            1,
            libc::MSG_PEEK,
        )
    };
    match n {
        1 => byte == 0x1b,
        _ => true,
    }
}

/// `poll(2)` stdin for readability within `timeout` — waits without consuming a
/// byte, and holds no resources once it returns (unlike a parked reader thread).
#[cfg(unix)]
pub(crate) fn stdin_readable_within(timeout: Duration) -> bool {
    use std::os::unix::io::AsRawFd;

    let mut pfd = libc::pollfd {
        fd: std::io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remain = deadline.saturating_duration_since(std::time::Instant::now());
        let ms = i32::try_from(remain.as_millis()).unwrap_or(i32::MAX);
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            // Interrupted by a signal: retry with the remaining budget (a zero
            // budget makes `poll` return 0 immediately, ending the loop).
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            n => return n > 0 && (pfd.revents & libc::POLLIN) != 0,
        }
    }
}

/// Whether a Device Status Report answer — the sentinel that closes a probe round
/// — is present. `CSI 0 n` is "terminal ok"; `CSI 3 n` is "terminal not ok", which
/// still means the terminal got that far and has nothing left to say.
pub fn has_status_report(reply: &[u8]) -> bool {
    find(reply, b"\x1b[0n").is_some() || find(reply, b"\x1b[3n").is_some()
}

/// Read one probe round's answers into [`TermCaps`]. Pure — the replies are just
/// bytes, so this is where the capability decisions are tested, no terminal
/// involved.
///
/// Nothing here anchors at the start of the reply: the answers arrive in query
/// order but a terminal is free to interleave other output, so each capability is
/// scanned for independently.
pub fn parse_term_caps(reply: &[u8]) -> TermCaps {
    TermCaps {
        kitty_keyboard: kitty_keyboard_flags_reported(reply),
        osc52: crate::da1_advertises_osc52(reply) || crate::xtgettcap_advertises_osc52(reply),
        sixel: crate::da1_advertises_sixel(reply),
        kitty_graphics: kitty_graphics_ok(reply),
        cell_size: cell_size(reply),
        multiplexer: reports_multiplexer(reply),
    }
}

/// Whether the terminal named itself a multiplexer in its XTVERSION answer,
/// `DCS > | <name> ST` — tmux replies `tmux 3.7b`, and every emulator that
/// implements the query replies with its own name.
///
/// Asking beats reading `TERM`: `TERM` is a *terminfo entry name* the user (or a
/// login script, or an ssh hop) can set to anything, while this is the thing
/// answering our queries saying what it is. `TERM` stays as the fallback for a
/// multiplexer too old to answer — see [`term_names_a_multiplexer`].
fn reports_multiplexer(reply: &[u8]) -> bool {
    let Some(at) = find(reply, b"\x1bP>|") else {
        return false;
    };
    let body = &reply[at + 4..];
    let end = body.iter().position(|&b| b == 0x1b).unwrap_or(body.len());
    let name = String::from_utf8_lossy(&body[..end]).to_ascii_lowercase();
    name.starts_with("tmux") || name.starts_with("screen")
}

/// Whether a `TERM` value names a multiplexer's terminfo entry (`tmux`,
/// `tmux-256color`, `screen`, `screen.xterm-256color`, …). The fallback for a
/// multiplexer that answers no XTVERSION; see [`reports_multiplexer`].
pub fn term_names_a_multiplexer(term: &str) -> bool {
    let term = term.to_ascii_lowercase();
    ["tmux", "screen"].iter().any(|m| {
        term == *m || term.starts_with(&format!("{m}-")) || term.starts_with(&format!("{m}."))
    })
}

/// Whether the terminal answered the progressive-enhancement flags query at all —
/// the reply is `CSI ? <flags> u`, and only a terminal that implements the kitty
/// keyboard protocol sends one. The flag *value* doesn't matter: it reports which
/// enhancements are currently on, not which are supported, and a terminal that
/// answers can be pushed the ones we want.
fn kitty_keyboard_flags_reported(reply: &[u8]) -> bool {
    csi_question_params(reply, b'u').is_some()
}

/// Whether the kitty graphics query came back `OK`. The answer is
/// `APC _G i=31 ; OK ST`; anything else (an error code, silence) is "no".
fn kitty_graphics_ok(reply: &[u8]) -> bool {
    let Some(at) = find(reply, b"\x1b_Gi=31;") else {
        return false;
    };
    reply[at + 8..].starts_with(b"OK")
}

/// One cell's pixel size from a `CSI 16 t` answer, `CSI 6 ; <height> ; <width> t`
/// — returned as `(width, height)`, the order terminal-graphics code wants. A zero
/// in either axis is no answer at all.
fn cell_size(reply: &[u8]) -> Option<(u16, u16)> {
    let mut rest = reply;
    while let Some(at) = find(rest, b"\x1b[6;") {
        let params = &rest[at + 4..];
        let end = params
            .iter()
            .position(|&b| !b.is_ascii_digit() && b != b';')
            .unwrap_or(params.len());
        if params.get(end) == Some(&b't') {
            let mut it = params[..end].split(|&b| b == b';');
            let h = it.next().and_then(parse_u16);
            let w = it.next().and_then(parse_u16);
            if it.next().is_none() {
                if let (Some(h), Some(w)) = (h, w) {
                    if w > 0 && h > 0 {
                        return Some((w, h));
                    }
                }
            }
        }
        rest = &params[end..];
    }
    None
}

fn parse_u16(bytes: &[u8]) -> Option<u16> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// The parameters of the first `CSI ? … <final>` report in `reply`, or `None` when
/// there is no complete one. Shared by every `CSI ?` answer we read (device
/// attributes, keyboard flags) so a truncated reply is never mistaken for a
/// complete one, and so the digits of some *other* `CSI ?` report can't be read as
/// this one's.
pub(crate) fn csi_question_params(reply: &[u8], final_byte: u8) -> Option<Vec<&[u8]>> {
    let mut rest = reply;
    while let Some(at) = find(rest, b"\x1b[?") {
        let params = &rest[at + 3..];
        // The parameters run up to the sequence's final byte. Stopping at the first
        // byte that can't be one keeps this from reading some other report's numbers.
        let end = params
            .iter()
            .position(|&b| !b.is_ascii_digit() && b != b';')
            .unwrap_or(params.len());
        if params.get(end) == Some(&final_byte) {
            return Some(params[..end].split(|&b| b == b';').collect());
        }
        // Not the report we want (or truncated: no final byte at all) — keep
        // looking. The slice always shrinks, since it starts past the introducer
        // just examined.
        rest = &params[end..];
    }
    None
}

/// The index of the first occurrence of `needle` in `haystack`.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .filter(|_| !needle.is_empty())
}
