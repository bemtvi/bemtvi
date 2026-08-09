//! Tier 3: startup against a terminal that answers *some* of what nxvim asks it.
//!
//! The client opens by asking the terminal what it can do. A multiplexer answers
//! only part of that — tmux replies to the device attributes and the status report
//! itself and ignores `XTGETTCAP` and the kitty queries outright — and every
//! question nobody answers used to cost its own timeout, serially, in front of the
//! first frame: half a second for the clipboard capability, two more for the
//! graphics probe, which also left a thread parked in a blocking `stdin` read that
//! then ate the user's first keystroke. That is the "nxvim is slow to open inside
//! tmux, and slower still over a nested ssh" report, and it is what these tests
//! pin down.
//!
//! Unlike the rest of the PTY suite (`e2e.rs`, all `#[ignore]`d) these are *not*
//! ignored: the test owns the master side of the pty and plays the terminal itself,
//! so the answers — and therefore the timing — are deterministic and need no real
//! controlling terminal.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// The budget one startup's capability round may take. The point of asking
/// everything at once is that an unanswered question costs nothing, so this sits
/// far below the ~2.6 s of stacked timeouts the serial probes used to spend — and
/// far enough above a debug build's own startup that it can't flake.
const STARTUP_BUDGET: Duration = Duration::from_secs(5);

/// A terminal, played by the test: it answers what tmux answers and stays silent
/// on the rest.
struct FakeTerminal {
    /// Shared with the answering thread — a pty hands out exactly one writer, and
    /// both the terminal's replies and the test's keystrokes go down it.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    screen: Arc<Mutex<vt100::Parser>>,
    /// Capability queries that arrived *after* the terminal answered a status
    /// report — i.e. after the client had been told "that is everything".
    late_queries: Arc<Mutex<Vec<&'static str>>>,
    /// Every byte the client wrote. `vt100` renders the *display*, so an escape
    /// that isn't display state (an OSC 52 clipboard write) is only visible here.
    raw: Arc<Mutex<Vec<u8>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    cfg_dir: PathBuf,
}

/// A fresh empty config dir, so startup never reads the developer's own
/// `~/.config/nxvim` (hermetic, per the suite convention).
fn empty_config_dir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir =
        nxvim_test_harness::temp_root().join(format!("nxvim_term_cfg_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create empty config dir");
    dir
}

impl FakeTerminal {
    /// Spawn `nxvim` on a pty and answer its capability queries the way tmux does:
    /// device attributes and the status report, nothing else. The reply to the
    /// status report is what tells the client the round is over — a terminal that
    /// never sent one would (correctly) cost the client its whole timeout.
    fn spawn_tmux_like(file: &str) -> FakeTerminal {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_nxvim"));
        cmd.arg(file);
        let cfg_dir = empty_config_dir();
        cmd.env("NXVIM_CONFIG", &cfg_dir);
        // A multiplexer's `TERM`, so anything keying off it sees what the user's
        // session sees. No `TMUX`: over an ssh hop into a tmux pane it isn't
        // forwarded, which is exactly the shape that used to be slowest.
        cmd.env("TERM", "tmux-256color");
        cmd.env_remove("TMUX");
        // Leave capability *overrides* out of it — the probe under test is the
        // point, and an inherited `NXVIM_OSC52` / `NXVIM_KITTY_KEYBOARD` would skip it.
        cmd.env_remove("NXVIM_OSC52");
        cmd.env_remove("NXVIM_KITTY_KEYBOARD");
        // What an ssh session looks like: no display for a host clipboard tool to
        // talk to, so the server has to fall back to the terminal's own OSC 52.
        cmd.env("DISPLAY", "");
        cmd.env("WAYLAND_DISPLAY", "");

        let child = pair.slave.spawn_command(cmd).expect("spawn nxvim");
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let answers = writer.clone();

        let screen = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let sink = screen.clone();
        let late_queries = Arc::new(Mutex::new(Vec::new()));
        let late_sink = late_queries.clone();
        let raw = Arc::new(Mutex::new(Vec::new()));
        let raw_sink = raw.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut pending = Vec::new();
            let mut round_closed = false;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        sink.lock().unwrap().process(&buf[..n]);
                        raw_sink.lock().unwrap().extend_from_slice(&buf[..n]);
                        pending.extend_from_slice(&buf[..n]);
                    }
                }
                // Swallow tmux passthrough (`DCS tmux; … ST`) wholesale, the way a
                // pane with `allow-passthrough` off does — that default is why a
                // probe wrapped for the *outer* terminal gets no answer at all, and
                // a responder that peeked inside the wrapper would answer questions
                // real tmux drops.
                strip_tmux_passthrough(&mut pending);
                // Answer in arrival order, so the status report's reply really does
                // come back last — the property the client relies on to know that
                // every question ahead of it has been answered or never will be.
                let mut reply = Vec::new();
                for (query, answer) in [
                    // XTVERSION: tmux names itself, exactly as tmux 3.x does.
                    (&b"\x1b[>q"[..], &b"\x1bP>|tmux 3.7b\x1b\\"[..]),
                    // Device attributes describe *tmux*, and never mention 52.
                    (&b"\x1b[c"[..], &b"\x1b[?1;2;4c"[..]),
                    (&b"\x1b[5n"[..], &b"\x1b[0n"[..]),
                ] {
                    let mut from = 0;
                    while let Some(at) = find(&pending[from..], query) {
                        reply.extend_from_slice(answer);
                        from += at + query.len();
                    }
                }
                if round_closed {
                    late_sink.lock().unwrap().extend(
                        CAPABILITY_QUERIES
                            .iter()
                            .filter(|(_, q)| find(&pending, q).is_some())
                            .map(|(name, _)| *name),
                    );
                }
                round_closed |= !reply.is_empty();
                pending.clear();
                if !reply.is_empty() {
                    let mut out = answers.lock().unwrap();
                    let _ = out.write_all(&reply).and_then(|()| out.flush());
                }
            }
        });

        FakeTerminal {
            writer,
            screen,
            child,
            late_queries,
            raw,
            _master: pair.master,
            cfg_dir,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let mut out = self.writer.lock().unwrap();
        out.write_all(bytes).expect("write");
        out.flush().expect("flush");
    }

    /// Poll the parsed screen until `pred` holds, returning how long it took (or
    /// `None` on timeout).
    fn wait_until(
        &self,
        timeout: Duration,
        pred: impl Fn(&vt100::Screen) -> bool,
    ) -> Option<Duration> {
        let start = Instant::now();
        let deadline = start + timeout;
        loop {
            if pred(self.screen.lock().unwrap().screen()) {
                return Some(start.elapsed());
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Poll the *raw* output stream until it contains `needle`, or `timeout`
    /// elapses. For escapes that leave no mark on the screen (see [`Self::raw`]).
    fn wait_for_raw(&self, timeout: Duration, needle: &[u8]) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if find(&self.raw.lock().unwrap(), needle).is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn screen_text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }
}

impl Drop for FakeTerminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.cfg_dir);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The escape each capability question is written as — enough to recognise one
/// arriving, whether or not this terminal answers it. `CSI 16 t` is left out: it
/// shares its shape with the window-size reports a client may legitimately send.
const CAPABILITY_QUERIES: &[(&str, &[u8])] = &[
    ("device attributes", b"\x1b[c"),
    ("status report", b"\x1b[5n"),
    ("kitty keyboard flags", b"\x1b[?u"),
    ("kitty graphics", b"\x1b_Gi="),
    ("XTGETTCAP", b"\x1bP+q"),
];

/// Delete every `DCS tmux; … ST` passthrough sequence from `buf`, contents and all.
fn strip_tmux_passthrough(buf: &mut Vec<u8>) {
    const START: &[u8] = b"\x1bPtmux;";
    const END: &[u8] = b"\x1b\\";
    while let Some(at) = find(buf, START) {
        let Some(rel) = find(&buf[at + START.len()..], END) else {
            // Still arriving: drop the tail, it belongs to a sequence tmux eats.
            buf.truncate(at);
            return;
        };
        buf.drain(at..at + START.len() + rel + END.len());
    }
}

fn sample_file(name: &str, body: &str) -> PathBuf {
    let path = nxvim_test_harness::temp_root().join(format!("{}_{name}", std::process::id()));
    std::fs::write(&path, body).expect("write sample file");
    path
}

#[test]
fn the_terminal_is_asked_everything_once() {
    // The fix, stated as a property the terminal can see: every question goes out in
    // one burst ended by a status report, and once that report is answered the
    // client asks nothing more. Probing serially instead is what stacked the
    // timeouts — each unanswered question waited out its own before the next went
    // out — so "nothing after the sentinel" is the thing worth pinning, and it holds
    // regardless of how fast the machine running this is.
    let path = sample_file("asked_once.txt", "alpha\nbeta\n");
    let term = FakeTerminal::spawn_tmux_like(path.to_str().unwrap());
    let took = term
        .wait_until(STARTUP_BUDGET, |s| s.contents().contains("alpha"))
        .unwrap_or_else(|| {
            panic!(
                "no first frame within {STARTUP_BUDGET:?}; screen was:\n{}",
                term.screen_text()
            )
        });
    eprintln!("first frame after {took:?}");
    // Anything the editor emits later (a resize, an explicit clipboard write) is
    // fine; only re-asking what it already asked is the regression.
    let late = term.late_queries.lock().unwrap().clone();
    assert!(
        late.is_empty(),
        "the client kept probing after the terminal said it was done: {late:?} — \
         each of those waits out its own timeout in front of the first frame"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn no_keystroke_after_startup_is_swallowed() {
    // The graphics probe used to leave a thread parked in a blocking `stdin` read
    // once its own query went unanswered. It stays there until *something* arrives
    // — the user's typing — and whichever read it wins is consumed instead of
    // reaching the editor. Typing one key per write gives that race several turns:
    // a parked reader takes one of them, and a single lost key is the whole bug
    // (the `i` never happens, or a letter vanishes mid-word).
    let path = sample_file("keys.txt", "\n");
    let mut term = FakeTerminal::spawn_tmux_like(path.to_str().unwrap());
    assert!(
        term.wait_until(STARTUP_BUDGET, |s| s.contents().contains('~'))
            .is_some(),
        "editor never painted; screen was:\n{}",
        term.screen_text()
    );
    // Give any parked reader the chance to be sitting on the read before typing.
    std::thread::sleep(Duration::from_millis(300));
    for key in b"iabcdefg" {
        term.send(&[*key]);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(
        term.wait_until(STARTUP_BUDGET, |s| s.contents().contains("abcdefg"))
            .is_some(),
        "a keystroke was eaten before it reached the editor; screen was:\n{}",
        term.screen_text()
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_yank_inside_a_multiplexer_still_reaches_the_clipboard() {
    // tmux answers the clipboard questions itself and its answers describe *tmux*:
    // its device attributes never list 52 and it ignores XTGETTCAP. Reading that
    // silence as "this terminal can't do OSC 52" left `"+y` with no clipboard
    // provider in every tmux session — which is precisely the session OSC 52 is
    // for, since the remote host has no clipboard the user could paste from.
    // tmux forwards the escape outward, so the yank has to leave as one.
    let path = sample_file("yank.txt", "clip me\n");
    let mut term = FakeTerminal::spawn_tmux_like(path.to_str().unwrap());
    assert!(
        term.wait_until(STARTUP_BUDGET, |s| s.contents().contains("clip me"))
            .is_some(),
        "file never showed; screen was:\n{}",
        term.screen_text()
    );
    term.send(b"\"+yy");
    // `ESC ] 52 ; c ;` then base64("clip me\n").
    assert!(
        term.wait_for_raw(STARTUP_BUDGET, b"\x1b]52;c;Y2xpcCBtZQo=\x1b\\"),
        "the yank never left as an OSC 52 write; screen was:\n{}",
        term.screen_text()
    );
    let _ = std::fs::remove_file(&path);
}
