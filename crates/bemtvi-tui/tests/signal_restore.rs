//! Tier 1: a **killed** client still restores the terminal.
//!
//! `kill <pid>` (SIGTERM), a closed terminal window (SIGHUP) or a crash in one of
//! the vendored C engines runs no destructor: the RAII guards and
//! `ratatui::restore()` only fire on an unwinding exit. Without a signal handler the
//! tty keeps the editor's settings after the process is gone — raw mode above all,
//! which is a *termios* setting on the terminal itself and so outlives the process
//! (the shell then gets no echo and no line editing), plus mouse reporting spraying
//! escape codes on every pointer move, the alternate screen, and bracketed paste.
//! That is unrecoverable for a user who doesn't know to blind-type `reset`.
//!
//! Black-box and hermetic: each test spawns a **real pty** and re-executes this same
//! test binary inside it as a child that installs the handler, puts the terminal into
//! the state the editor leaves it in, and then dies the way the scenario says. The
//! parent inspects the pty from the outside — its termios (master and slave share
//! one) and the bytes the child emitted — which is exactly what a user's shell is
//! left holding.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// Names the child's scenario, and by its presence marks the process as the child.
const CHILD_ENV: &str = "BEMTVI_TUI_SIGNAL_RESTORE_CHILD";
/// The child announces "the terminal is now in the editor's state" with this marker.
const READY: &[u8] = b"<<READY>>";
/// The `double` child announces that the first signal left it running.
const ALIVE: &[u8] = b"<<ALIVE>>";
/// Exit code the child uses if the death it asked for left it alive.
const SURVIVED: u32 = 97;

/// The child half, run inside a pty by the tests below. `#[ignore]`d so a normal
/// `cargo test` run never executes it — the parent invokes it by name with
/// `--ignored`.
#[test]
#[ignore = "spawned inside a pty by the tests in this file"]
fn signal_restore_child() {
    let scenario = std::env::var(CHILD_ENV).unwrap_or_else(|_| {
        panic!("this test is a helper: it must be spawned with {CHILD_ENV} set")
    });

    // What the editor does at startup, in the same order (`bemtvi_tui::run`).
    bemtvi_tui::install_signal_restore();
    crossterm::terminal::enable_raw_mode().expect("enable raw mode in the pty");
    let mut out = std::io::stdout();
    crossterm::execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        SetCursorStyle::SteadyBar,
    )
    .expect("put the terminal into the editor's state");
    out.write_all(READY).expect("write ready marker");
    out.flush().expect("flush");

    // Hold here until the parent has inspected the tty — otherwise the restore could
    // land before it looks, and the precondition check would read the *restored*
    // state and pass vacuously.
    let mut go = [0u8; 1];
    std::io::stdin()
        .read_exact(&mut go)
        .expect("parent's go-ahead");

    match scenario.as_str() {
        // `kill <pid>` with nothing driving the graceful path (no event loop here, as
        // in a client too wedged to answer): the watchdog has to give up waiting and
        // restore the terminal itself.
        "watchdog" => {
            raise(libc::SIGTERM);
            park();
        }
        // `kill` twice — the way a user insists. The first signal must *not* kill us
        // (that's what makes a graceful shutdown possible at all); the second must,
        // without waiting out the grace period.
        "double" => {
            raise(libc::SIGTERM);
            std::thread::sleep(Duration::from_millis(300));
            out.write_all(ALIVE).expect("write alive marker");
            out.flush().expect("flush");
            raise(libc::SIGTERM);
            park();
        }
        // A hard crash with a handler already installed (the Rust runtime's
        // stack-overflow reporter), which our handler must chain to.
        "overflow" => {
            std::hint::black_box(overflow_the_stack(0));
        }
        other => panic!("unknown child scenario {other:?}"),
    }

    // Only reached if the death did not take: a fatal signal must always land in the
    // end. A distinct exit code so the parent can say exactly that.
    std::process::exit(SURVIVED as i32);
}

/// SAFETY wrapper: send ourselves `sig`, exactly as an outside `kill` would.
fn raise(sig: libc::c_int) {
    // SAFETY: sending ourselves a signal.
    unsafe { libc::raise(sig) };
}

/// Stay alive (and out of the way) while something else decides our fate. Bounded, so
/// a broken build fails the test instead of hanging it.
fn park() {
    std::thread::sleep(Duration::from_secs(30));
}

/// Recurse until the guard page is hit. `black_box` on a stack-resident buffer keeps
/// the recursion from being optimized into a loop.
#[inline(never)]
#[allow(unconditional_recursion, reason = "overflowing the stack is the point")]
fn overflow_the_stack(depth: u64) -> u64 {
    let mut frame = [depth; 256];
    std::hint::black_box(&mut frame);
    frame[0] + overflow_the_stack(depth + 1)
}

/// The graceful path can only be *offered*: nothing guarantees the client is in a
/// state to take it — the usual reason for a `kill` is that it is wedged. The
/// watchdog is the backstop, and it must leave the terminal exactly as the in-handler
/// restore would have.
#[test]
fn terminal_is_restored_when_the_graceful_path_never_answers() {
    // A short grace so the test doesn't sit out the production one.
    let killed = run_child("watchdog", &[("BEMTVI_EXIT_GRACE_MS", "500")]);

    assert_ne!(
        killed.status.exit_code(),
        SURVIVED,
        "the watchdog never stopped waiting — a `kill` must be honoured"
    );
    // Not just "died" — died *of SIGTERM*. Restoring must not change what the shell
    // reports (an `abort()` fallback would say "Aborted" for a plain `kill`).
    assert_eq!(
        killed.status.signal(),
        Some(signal_name(libc::SIGTERM).as_str()),
        "a killed client must still report the signal that killed it"
    );
    killed.assert_terminal_usable();

    for (what, seq) in [
        (
            "disable mouse reporting",
            command_bytes(DisableMouseCapture),
        ),
        (
            "disable bracketed paste",
            command_bytes(DisableBracketedPaste),
        ),
        (
            "leave the alternate screen",
            command_bytes(LeaveAlternateScreen),
        ),
        (
            "reset the cursor shape",
            command_bytes(SetCursorStyle::DefaultUserShape),
        ),
    ] {
        assert!(
            contains(&killed.output, &seq),
            "a killed client must {what}: {seq:?} missing from {:?}",
            String::from_utf8_lossy(&killed.output)
        );
    }
}

/// The first "please stop" signal must leave the process *running* — that is the
/// whole basis of the graceful shutdown, which needs the editor alive long enough to
/// quit itself. A second one is how the user says they meant it, and must not wait
/// out the grace period.
#[test]
fn a_second_signal_kills_immediately_without_waiting_out_the_grace() {
    // A grace period long enough that only the second signal can end this.
    let started = Instant::now();
    let killed = run_child("double", &[("BEMTVI_EXIT_GRACE_MS", "600000")]);
    let elapsed = started.elapsed();

    assert!(
        contains(&killed.output, ALIVE),
        "the first signal killed the client outright, leaving no room for a graceful \
         shutdown: {:?}",
        String::from_utf8_lossy(&killed.output)
    );
    assert!(
        elapsed < Duration::from_secs(60),
        "the second signal waited for the grace period instead of killing at once \
         ({elapsed:?})"
    );
    assert_ne!(
        killed.status.exit_code(),
        SURVIVED,
        "the client outlived two signals"
    );
    killed.assert_terminal_usable();
}

/// A crash the *Rust runtime* already handles must still reach its reporter: the
/// handler restores the terminal and then chains to the disposition it displaced,
/// rather than turning "thread has overflowed its stack" into a silent death.
#[test]
fn a_crash_restores_the_terminal_and_still_reports_itself() {
    let killed = run_child("overflow", &[]);

    assert_ne!(
        killed.status.exit_code(),
        SURVIVED,
        "the handler swallowed the fault instead of passing it on"
    );
    killed.assert_terminal_usable();
    assert!(
        contains(&killed.output, b"has overflowed its stack"),
        "the runtime's stack-overflow report was lost: {:?}",
        String::from_utf8_lossy(&killed.output)
    );
}

/// The aftermath of one child scenario: the pty it died on (kept open so its
/// attributes stay readable), everything it wrote, and how it exited.
struct Killed {
    _master: Box<dyn MasterPty + Send>,
    fd: RawFd,
    output: Vec<u8>,
    status: portable_pty::ExitStatus,
}

impl Killed {
    /// The whole point: a shell inheriting this tty must be usable again.
    fn assert_terminal_usable(&self) {
        assert!(
            echo_on(self.fd),
            "the tty was left in raw mode after the process died — a shell on it gets \
             no echo and no line editing"
        );
        assert!(
            canonical_on(self.fd),
            "the tty was left in non-canonical mode after the process died"
        );
    }
}

/// Run one child scenario to its death in a fresh pty, checking on the way that the
/// terminal really was in the editor's raw-mode state first.
fn run_child(scenario: &str, env: &[(&str, &str)]) -> Killed {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let fd = pair.master.as_raw_fd().expect("pty master fd");

    // Cooked mode is the baseline the child must be put back into.
    assert!(echo_on(fd), "a fresh pty starts in cooked mode");

    let mut cmd = CommandBuilder::new(std::env::current_exe().expect("test binary path"));
    cmd.args([
        "--exact",
        "signal_restore_child",
        "--ignored",
        "--nocapture",
    ]);
    cmd.env(CHILD_ENV, scenario);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = pair.slave.spawn_command(cmd).expect("spawn the pty child");
    drop(pair.slave);

    let output = Arc::new(Mutex::new(Vec::new()));
    let saw_ready = Arc::new(AtomicBool::new(false));
    {
        let (sink, ready) = (output.clone(), saw_ready.clone());
        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut sink = sink.lock().unwrap();
                sink.extend_from_slice(&buf[..n]);
                if contains(&sink, READY) {
                    ready.store(true, Ordering::SeqCst);
                }
            }
        });
    }

    // The child only reaches its ready marker after raw mode is really on, so this
    // also proves the precondition: the bug's starting state is genuine.
    wait_for(Duration::from_secs(60), || saw_ready.load(Ordering::SeqCst))
        .expect("child never signalled that the terminal was set up");
    assert!(
        !echo_on(fd),
        "precondition: the child must have put the pty into raw mode"
    );

    // Release the child, which then kills itself.
    let mut writer = pair.master.take_writer().expect("pty writer");
    writer.write_all(b"g").expect("send the go-ahead");
    writer.flush().expect("flush the go-ahead");

    let status = wait_with_timeout(&mut child, Duration::from_secs(60)).expect("child never died");
    // The reader thread races the child's death; give the tail of its output a moment
    // to land before snapshotting (the restore sequence is the last thing written).
    let _ = wait_for(Duration::from_secs(5), || {
        contains(
            &output.lock().unwrap(),
            &command_bytes(LeaveAlternateScreen),
        )
    });
    let output = output.lock().unwrap().clone();

    Killed {
        _master: pair.master,
        fd,
        output,
        status,
    }
}

/// The exact bytes crossterm emits for `command`, so the assertions compare against
/// the real encoding rather than a hand-copied escape sequence.
fn command_bytes(command: impl crossterm::Command) -> Vec<u8> {
    let mut buf = Vec::new();
    crossterm::execute!(buf, command).unwrap();
    buf
}

/// The platform's name for `sig`, in the same form portable-pty reports it
/// (`strsignal`), so the comparison doesn't hard-code a locale's wording.
fn signal_name(sig: libc::c_int) -> String {
    // SAFETY: `strsignal` returns a static string for a known signal number.
    unsafe { std::ffi::CStr::from_ptr(libc::strsignal(sig)) }
        .to_string_lossy()
        .into_owned()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The pty's current attributes. On Linux the master and the slave share one
/// termios, so the parent can read what the child left behind on the tty.
fn termios_of(fd: RawFd) -> libc::termios {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is the open pty master; `termios` is a valid out-pointer.
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    assert_eq!(rc, 0, "tcgetattr on the pty master");
    // SAFETY: initialized by the successful `tcgetattr` above.
    unsafe { termios.assume_init() }
}

fn echo_on(fd: RawFd) -> bool {
    termios_of(fd).c_lflag & libc::ECHO != 0
}

fn canonical_on(fd: RawFd) -> bool {
    termios_of(fd).c_lflag & libc::ICANON != 0
}

fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> Result<(), ()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(())
}

fn wait_with_timeout(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    timeout: Duration,
) -> Option<portable_pty::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("waiting for the pty child: {e}"),
        }
    }
    let _ = child.kill();
    None
}
