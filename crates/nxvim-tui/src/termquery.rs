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
//! Raw mode must already be on when these run (`ratatui::init`), or the line
//! discipline holds the reply back — and they must run *before* the input
//! `EventStream` exists, or the two race for the same bytes.

use std::time::Duration;

/// How long a reply may pause mid-sequence before we call it finished. Terminals
/// answer in one write; this only has to outlast the kernel splitting it.
#[cfg(unix)]
const QUIET: Duration = Duration::from_millis(50);

/// Write `query` to the terminal and return everything it answers, or `None` when
/// it says nothing within `wait` (it doesn't implement the query — the caller
/// treats that as "unsupported").
///
/// Reads until the reply goes quiet for [`QUIET`], so the *whole* answer is
/// consumed: a fragment left on stdin would be delivered to the input parser as
/// garbage keystrokes, or mistaken for the reply to the next probe.
#[cfg(unix)]
pub(crate) fn ask(query: &[u8], wait: Duration) -> Option<Vec<u8>> {
    use std::io::{Read, Write};

    let mut out = std::io::stdout();
    out.write_all(query).and_then(|()| out.flush()).ok()?;
    // Generous first wait: a slow hop (ssh) can sit on the reply for a while.
    if !stdin_readable_within(wait) {
        return None;
    }
    let mut stdin = std::io::stdin();
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        match stdin.read(&mut buf) {
            // EOF / error: nothing more is coming.
            Ok(0) | Err(_) => return (!reply.is_empty()).then_some(reply),
            Ok(n) => reply.extend_from_slice(&buf[..n]),
        }
        if !stdin_readable_within(QUIET) {
            return Some(reply);
        }
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
