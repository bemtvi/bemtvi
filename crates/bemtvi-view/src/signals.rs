//! Dying properly when the process is **killed**, not just when it exits.
//!
//! Shared by every UI client, because both of them have the same hole: their whole
//! cleanup story is RAII guards and ordinary returns, and a `Drop` never runs for a
//! signal whose default action is "terminate". So `kill <pid>` (SIGTERM), a closed
//! terminal window (SIGHUP) or a segfault in one of the vendored C engines used to
//! end the process on the spot — losing everything the exit sequence does, and, for
//! the TUI, leaving the user's tty configured as the editor had it:
//!
//! - **raw mode still on** — a termios setting that lives on the *tty*, not in the
//!   process, so it outlives it: the shell gets no echo, no line editing, and Enter
//!   doesn't submit (output stair-steps without `ONLCR`);
//! - **mouse reporting still on** — every pointer move sprays escape codes;
//! - alternate screen, bracketed paste, kitty keyboard flags and the insert-mode bar
//!   cursor all still in effect.
//!
//! That shell is unusable without knowing to blind-type `stty sane` / `reset`.
//! [`install`] closes both holes. SIGKILL (`kill -9`) is uncatchable by definition
//! and stays unrecoverable.
//!
//! # Two paths out
//!
//! **Graceful** — SIGTERM (a plain `kill`), SIGHUP (the terminal window closed) and
//! SIGINT. These mean "please stop", so the client stops the way `:qall!` does: the
//! handler only *records* the request and calls [`Config::on_shutdown`], and the
//! client's own event loop drives the real quit — `QuitPre`/`ExitPre`/`VimLeavePre`/
//! `VimLeave` autocmds fire, shada is written (so marks, registers, histories and the
//! exit cursor survive), the guards restore the terminal on the way out, and only
//! then does [`exit_as_signal_if_killed`] re-raise so the shell still reports
//! "Terminated".
//!
//! **Hard** — SIGQUIT (whose whole point is "die now, dump core") and the fault
//! signals, where the process is already broken and running Lua would be reckless.
//! Also the fallback for the graceful path: a *second* signal, or a graceful attempt
//! that hasn't finished within [`GRACE`](unix::GRACE) (`BEMTVI_EXIT_GRACE_MS`),
//! because the reason a user reaches for `kill` is usually that the client is already
//! wedged. The hard path performs [`Config`]'s terminal restore from the signal
//! handler itself and then lets the signal run its course — chaining to a handler
//! that was already installed, otherwise re-raising under the default disposition —
//! so the exit status (and any core dump) is exactly what it would have been. A
//! client with nothing to restore (the GUI: a window, not a tty) skips the fault
//! signals entirely rather than displacing the runtime's own crash reporting for
//! nothing.
//!
//! **Everything the handler runs must be async-signal-safe**, since it can interrupt
//! the process anywhere — including inside `malloc` or with stdout's lock held. Hence
//! the shape of [`Config`]: the caller renders its escape sequence *up front* (the
//! TUI's comes from the same crossterm commands its RAII guards emit) and the
//! original termios is captured at install time, so the handler itself only does
//! atomic loads, `write(2)` and `tcsetattr(3)` — never `crossterm::execute!` /
//! `disable_raw_mode`, whose Rust `io` lock and internal `Mutex` would deadlock if
//! the signal landed while they were held. The graceful path needs none of that care:
//! the handler hands off to an ordinary thread through a self-pipe, and everything
//! real happens back on the client's event loop.

/// How a client wants to be shut down — see the module docs.
pub struct Config {
    /// Escape sequence written to stdout on the hard path, pre-rendered because the
    /// signal handler cannot format one. Empty for a client with no terminal state
    /// (the GUI), which also opts it out of handling the fault signals.
    pub restore_sequence: Vec<u8>,
    /// Whether to put the controlling tty back into the mode it was in at install
    /// time. `true` for a client that enables raw mode, `false` for the GUI.
    pub restore_termios: bool,
    /// Called once, from a plain background thread, when a signal has asked for a
    /// graceful shutdown: the client's cue to quit its session properly. It must not
    /// block — the shutdown it kicks off is on a clock ([`GRACE`](unix::GRACE)).
    pub on_shutdown: Box<dyn Fn() + Send + 'static>,
}

/// Install the fatal-signal handlers described by `config`.
///
/// Call **before** the terminal is put into raw mode / the alternate screen, so the
/// termios captured here is the user's original cooked-mode one. Only the first call
/// does anything. A no-op on non-unix, where there is no signal to catch this way.
pub fn install(config: Config) {
    #[cfg(unix)]
    unix::install(config);
    #[cfg(not(unix))]
    drop(config);
}

/// Whether a graceful shutdown has already been asked for — for the window between
/// the signal arriving and the client being in a position to hear about it (an attach
/// in flight, a session swap).
pub fn shutdown_requested() -> bool {
    #[cfg(unix)]
    {
        unix::shutdown_requested()
    }
    #[cfg(not(unix))]
    false
}

/// Finish a signal-initiated exit: if the process is only shutting down because it
/// was killed, die *of that signal* now that the graceful work is done.
///
/// Call at the very end, after the terminal is restored and the server threads have
/// been joined — a caught-and-cleaned-up SIGTERM must still look like a SIGTERM to
/// whoever sent it (a shell, a script, a supervisor), not like a clean exit 0.
/// Returns normally when the process is exiting for any other reason.
pub fn exit_as_signal_if_killed() {
    #[cfg(unix)]
    unix::exit_as_signal_if_killed();
}

#[cfg(unix)]
pub mod unix {
    use std::cell::UnsafeCell;
    use std::mem::MaybeUninit;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::Config;

    /// The signals whose default action terminates the process and that a client can
    /// meaningfully catch. SIGKILL/SIGSTOP are uncatchable and absent by necessity;
    /// the hardware/abort faults (`SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE`, `SIGABRT`)
    /// are here because this process links real C engines (PUC Lua, tree-sitter, the
    /// vendored vim regex) and a crash in one must not take the user's terminal down
    /// with it — re-raising under `SIG_DFL` still dumps core. They are installed only
    /// for a client that has terminal state to restore; see [`install`].
    const FATAL: [libc::c_int; 9] = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGILL,
        libc::SIGABRT,
        libc::SIGFPE,
        libc::SIGBUS,
        libc::SIGSEGV,
        libc::SIGTERM,
    ];

    /// The "please stop" subset of [`FATAL`], which gets the graceful shutdown (see
    /// the module docs). SIGQUIT is deliberately *not* here — "quit and dump core" is
    /// what it means — and neither is any fault, where the process is already broken.
    const GRACEFUL: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGHUP, libc::SIGINT];

    /// How long the graceful shutdown gets before the watchdog stops waiting and takes
    /// the hard path. A `kill` has to be honoured promptly — and the usual reason for
    /// one is an editor that is already stuck, in which case nothing will come of the
    /// polite request. Overridable with `BEMTVI_EXIT_GRACE_MS` for a session with slow
    /// `VimLeave` work (or to tighten it).
    pub const GRACE: Duration = Duration::from_secs(5);

    /// Set once a graceful shutdown has been requested, so a *second* signal skips
    /// straight to the hard path (`kill` twice to stop waiting).
    static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
    /// The signal that asked us to shut down, re-raised at the very end so the exit
    /// looks like what it is. `0` until one arrives.
    static EXIT_SIGNAL: AtomicI32 = AtomicI32::new(0);
    /// The write end of the self-pipe the handler pokes to wake the watchdog thread.
    static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

    /// The pre-rendered restore sequence (leaked at install; read by the handler).
    static RESTORE_PTR: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
    static RESTORE_LEN: AtomicUsize = AtomicUsize::new(0);

    /// The tty whose termios we saved, or `-1` when there is none to restore.
    static TTY_FD: AtomicI32 = AtomicI32::new(-1);
    static HAVE_TERMIOS: AtomicBool = AtomicBool::new(false);

    /// The dispositions we replaced, one per [`FATAL`] entry, so the handler can hand
    /// the signal on to whoever had it (see [`chain_to_previous`]).
    struct PrevSlot(UnsafeCell<[MaybeUninit<libc::sigaction>; FATAL.len()]>);
    // SAFETY: same publication discipline as `SAVED` — written during `install`
    // before the handler that reads it can be reached.
    unsafe impl Sync for PrevSlot {}
    static PREV: PrevSlot = PrevSlot(UnsafeCell::new(
        [const { MaybeUninit::uninit() }; FATAL.len()],
    ));

    /// The terminal's attributes as they were *before* raw mode. A plain `static`
    /// cell rather than a `Mutex`/`OnceLock`: the handler must read it without
    /// taking a lock. Written once in [`install`] before `HAVE_TERMIOS` is set
    /// (release), read only after that flag reads true (acquire).
    struct TermiosSlot(UnsafeCell<MaybeUninit<libc::termios>>);
    // SAFETY: written once during `install` before any handler can observe it (the
    // `HAVE_TERMIOS` release/acquire pair orders the publication), read-only after.
    unsafe impl Sync for TermiosSlot {}
    static SAVED: TermiosSlot = TermiosSlot(UnsafeCell::new(MaybeUninit::uninit()));

    pub(super) fn install(config: Config) {
        // Once only: a second pass would record *our own* handler as the previous
        // disposition and chain to itself forever.
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| install_once(config));
    }

    pub(super) fn shutdown_requested() -> bool {
        SHUTDOWN_REQUESTED.load(Ordering::Acquire)
    }

    /// Re-raise the signal that started this shutdown, now that the graceful work is
    /// done — see [`super::exit_as_signal_if_killed`]. The terminal has already been
    /// restored by the ordinary exit path, so this only has to arrange the death.
    pub(super) fn exit_as_signal_if_killed() {
        let sig = EXIT_SIGNAL.load(Ordering::Acquire);
        if sig == 0 {
            return;
        }
        // SAFETY: restoring the default disposition and re-raising a catchable signal.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }

    fn install_once(config: Config) {
        let Config {
            restore_sequence,
            restore_termios,
            on_shutdown,
        } = config;
        // Whether the hard path has anything to do beyond dying. A client with no
        // terminal state (the GUI) gets the graceful signals only: displacing the
        // runtime's crash reporting for the faults would cost its diagnostics and buy
        // nothing.
        let restores_terminal = !restore_sequence.is_empty() || restore_termios;
        publish_restore_sequence(restore_sequence);
        if restore_termios {
            save_termios();
        }
        start_watchdog(on_shutdown);

        // `sigaction`, not `signal(2)`, for the two flags that matter here:
        //
        // - `SA_ONSTACK` runs the handler on the alternate signal stack the Rust
        //   runtime installs per thread. Without it a *stack-overflow* SIGSEGV — the
        //   one fault with no stack left to run a handler on — would fault again and
        //   die with the terminal still broken.
        // - `SA_SIGINFO` gives the three-argument form, which is what a previously
        //   installed handler (Rust's own SIGSEGV/SIGBUS reporter) expects to be
        //   passed when we chain to it.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
        // SAFETY: `sa_mask` is a valid out-pointer into our own stack value.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };

        for (i, sig) in FATAL.into_iter().enumerate() {
            if !restores_terminal && !GRACEFUL.contains(&sig) {
                continue;
            }
            let mut prev = MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: valid handler + out-pointer for a catchable signal.
            if unsafe { libc::sigaction(sig, &action, prev.as_mut_ptr()) } == 0 {
                // SAFETY: exclusive during install; `i` is in range by construction.
                unsafe { (*PREV.0.get())[i] = prev };
            }
        }
    }

    /// Open the self-pipe and park a thread on it: the handler can only poke a pipe,
    /// so all the *waiting* a graceful shutdown needs happens here, on a plain OS
    /// thread that is deliberately independent of the tokio runtime and the server —
    /// the parts most likely to be wedged when someone reaches for `kill`.
    fn start_watchdog(on_shutdown: Box<dyn Fn() + Send + 'static>) {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` is a valid two-element out-array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return; // no pipe ⇒ no graceful path; the handler falls back to hard exit
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        WAKE_FD.store(write_fd, Ordering::Release);
        std::thread::Builder::new()
            .name("bemtvi-exit-watchdog".into())
            .spawn(move || {
                if !wait_for_wake(read_fd) {
                    return;
                }
                // Hand the request to the client, which quits its session properly.
                on_shutdown();
                // …but only wait so long for that to take effect. Whatever state the
                // rest of the process is in, the terminal gets restored and the signal
                // gets honoured.
                std::thread::sleep(grace());
                restore();
                hard_exit(EXIT_SIGNAL.load(Ordering::Acquire));
            })
            .ok();
    }

    /// Block until the handler pokes the pipe. `false` if the pipe died instead.
    fn wait_for_wake(fd: libc::c_int) -> bool {
        let mut byte = [0u8; 1];
        loop {
            // SAFETY: reading one byte into a local buffer from our own pipe.
            let n = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
            if n > 0 {
                return true;
            }
            if n < 0 && is_eintr() {
                continue;
            }
            return false;
        }
    }

    /// [`GRACE`], or the `BEMTVI_EXIT_GRACE_MS` override when it parses.
    fn grace() -> Duration {
        std::env::var("BEMTVI_EXIT_GRACE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(GRACE, Duration::from_millis)
    }

    /// Leak the caller's escape sequence so the handler can `write(2)` it straight
    /// from a raw pointer — the allocation happens here, at install time, because it
    /// could not happen inside a signal handler.
    fn publish_restore_sequence(sequence: Vec<u8>) {
        let seq: &'static mut [u8] = Vec::leak(sequence);
        RESTORE_LEN.store(seq.len(), Ordering::Relaxed);
        RESTORE_PTR.store(seq.as_mut_ptr(), Ordering::Release);
    }

    /// Capture the current terminal attributes so the handler can put the tty back
    /// into cooked mode. Uses the same fd crossterm's `enable_raw_mode` does —
    /// stdin when it is a tty, otherwise `/dev/tty` — or gives up when neither is a
    /// terminal (output redirected to a file: nothing to restore).
    fn save_termios() {
        let fd = tty_fd();
        if fd < 0 {
            return;
        }
        let mut termios = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `fd` is an open terminal; `termios` is a valid out-pointer.
        if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } != 0 {
            return;
        }
        // SAFETY: exclusive during install, before `HAVE_TERMIOS` publishes it.
        unsafe { *SAVED.0.get() = termios };
        TTY_FD.store(fd, Ordering::Relaxed);
        HAVE_TERMIOS.store(true, Ordering::Release);
    }

    /// A descriptor for the controlling terminal, kept open for the rest of the
    /// process (deliberately never closed — the handler may need it at any moment).
    fn tty_fd() -> libc::c_int {
        // SAFETY: plain libc calls on a constant fd / a static C string.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) == 1 {
                return libc::STDIN_FILENO;
            }
            libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR)
        }
    }

    /// The signal handler. Either records a graceful shutdown request and **returns**
    /// — leaving the editor running just long enough to quit itself properly — or,
    /// for a signal that can't wait, takes the hard path right here.
    extern "C" fn handler(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        // First "please stop" signal: hand it to the watchdog thread and get out of
        // the way. The `swap` makes the *second* one fall through to the hard path,
        // which is how a user insists (and how they escape a shutdown that hangs).
        if GRACEFUL.contains(&sig) && !SHUTDOWN_REQUESTED.swap(true, Ordering::AcqRel) {
            EXIT_SIGNAL.store(sig, Ordering::Release);
            let fd = WAKE_FD.load(Ordering::Acquire);
            if fd >= 0 {
                write_all(fd, [b'x'].as_ptr(), 1);
                return;
            }
            // No pipe (its creation failed): nothing will ever drive the graceful
            // path, so don't pretend — fall through and die now.
        }
        hard_exit_chaining(sig, info, ctx)
    }

    /// Restore the terminal and let the signal do exactly what it would have done
    /// without us.
    ///
    /// That means handing off rather than exiting: to the handler that was already
    /// installed if there was one (the Rust runtime's SIGSEGV/SIGBUS reporter — its
    /// "thread has overflowed its stack" message must not be swallowed), otherwise
    /// re-raising under `SIG_DFL` so the parent shell still sees "terminated by
    /// SIGTERM" and a fault still dumps core.
    fn hard_exit_chaining(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        ctx: *mut libc::c_void,
    ) -> ! {
        restore();
        chain_to_previous(sig, info, ctx);
        hard_exit(sig)
    }

    /// Die of `sig` under its default disposition, from wherever we are.
    fn hard_exit(sig: libc::c_int) -> ! {
        // A signal of 0 means "no signal recorded" — nothing to re-raise, so take the
        // one death that always works.
        if sig == 0 {
            // SAFETY: `abort` is async-signal-safe.
            unsafe { libc::abort() }
        }
        // SAFETY: all async-signal-safe, and permitted from a handler.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            // Inside a handler `sig` is blocked, so a plain `raise` would only mark it
            // pending and *return* — falling through to the `abort` below, which would
            // report the death as SIGABRT instead of the signal the user actually
            // sent. Unblock it first so the re-raise lands immediately, under the
            // default disposition: the shell sees "Terminated" for a `kill`, and a
            // fault still dumps core. (Harmless when called off the handler, where the
            // signal was never blocked to begin with.)
            let mut just_this: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut just_this);
            libc::sigaddset(&mut just_this, sig);
            libc::sigprocmask(libc::SIG_UNBLOCK, &just_this, ptr::null_mut());
            libc::raise(sig);
            // Unreachable unless the signal was somehow ignored; never return from
            // here — for a fault, returning would just re-run the faulting
            // instruction forever.
            libc::abort()
        }
    }

    /// Invoke the disposition we displaced, if it was a real handler. Returns
    /// normally when there was nothing to chain to (or it chose to return), leaving
    /// the caller to re-raise.
    fn chain_to_previous(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        let Some(i) = FATAL.iter().position(|&s| s == sig) else {
            return;
        };
        // SAFETY: `PREV[i]` was initialized by `install` — this handler can only run
        // for a signal whose `sigaction` succeeded — and is not written again.
        let prev = unsafe { (*PREV.0.get())[i].assume_init() };
        let f = prev.sa_sigaction;
        if f == libc::SIG_DFL || f == libc::SIG_IGN {
            return;
        }
        // SAFETY: `sa_sigaction` holds a function pointer of the form its own
        // `SA_SIGINFO` flag selects; we call it with the arguments it was given.
        unsafe {
            if prev.sa_flags & libc::SA_SIGINFO != 0 {
                let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                    std::mem::transmute(f);
                f(sig, info, ctx);
            } else {
                let f: extern "C" fn(libc::c_int) = std::mem::transmute(f);
                f(sig);
            }
        }
    }

    /// Async-signal-safe terminal restore: raw `write(2)` of the pre-rendered escape
    /// sequence, then the saved termios back onto the tty.
    fn restore() {
        let ptr = RESTORE_PTR.load(Ordering::Acquire);
        let len = RESTORE_LEN.load(Ordering::Relaxed);
        if !ptr.is_null() {
            write_all(libc::STDOUT_FILENO, ptr, len);
        }
        if HAVE_TERMIOS.load(Ordering::Acquire) {
            let fd = TTY_FD.load(Ordering::Relaxed);
            // SAFETY: `SAVED` was fully initialized before `HAVE_TERMIOS` was set.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, (*SAVED.0.get()).as_ptr()) };
        }
    }

    /// `write(2)` the whole buffer, resuming after a short write or an `EINTR` (both
    /// are ordinary on a tty). Bounded by `len`, so a persistently failing fd can't
    /// spin: any other error ends it.
    fn write_all(fd: libc::c_int, buf: *const u8, len: usize) {
        let mut written = 0usize;
        while written < len {
            // SAFETY: `buf[written..len]` is in bounds of the leaked sequence.
            let n = unsafe { libc::write(fd, buf.add(written).cast(), len - written) };
            if n > 0 {
                written += n as usize;
            } else if n < 0 && is_eintr() {
                continue;
            } else {
                return;
            }
        }
    }

    /// Whether the last syscall failed with `EINTR`. `last_os_error` is a bare read
    /// of the thread-local `errno` (no allocation, no locking), so it is safe here.
    fn is_eintr() -> bool {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
    }
}
