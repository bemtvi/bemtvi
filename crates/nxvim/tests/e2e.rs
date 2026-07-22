//! Tier 3: drive the real `nxvim` binary in a pseudo-terminal and assert on the
//! terminal output a user would actually see. This is the only tier that proves
//! real crossterm decode, real terminal escapes, and process startup/args. Kept
//! thin: it is the slow/flaky surface, so the bulk of coverage lives in Tiers 1–2.
//!
//! Every test here is `#[ignore]`d: they need a real, well-behaved controlling
//! terminal and are flaky in headless/CI environments without one (concurrent PTY
//! sessions contend; output/exit timing is unreliable). Run them deliberately on a
//! real terminal with `cargo test -p nxvim --test e2e -- --ignored`. They are
//! otherwise hermetic — each spawns with a throwaway empty `NXVIM_CONFIG`
//! ([`empty_config_dir`]) so a run never depends on the developer's
//! `~/.config/nxvim` (only the `catppuccin` test additionally needs that plugin
//! installed, and skips when it is absent).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// A spawned `nxvim` process attached to a PTY, with a background thread feeding
/// all output into a `vt100` parser.
struct Session {
    writer: Box<dyn Write + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    /// The throwaway empty config dir injected for hermeticity (removed on drop);
    /// `None` when the test supplied its own `NXVIM_CONFIG`.
    _cfg_dir: Option<PathBuf>,
}

/// Create a fresh, empty config directory under the temp dir, unique per call, so a
/// spawned `nxvim` starts with no plugins / `init.lua` — hermetic, independent of
/// the developer's `~/.config/nxvim`.
fn empty_config_dir() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_e2e_cfg_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create empty config dir");
    dir
}

impl Session {
    fn spawn(args: &[&str], cols: u16, rows: u16) -> Session {
        Session::spawn_with_env(args, &[], cols, rows)
    }

    /// As [`Session::spawn`], plus extra environment variables for the child —
    /// used to point `nxvim` at a throwaway config dir / runtimepath / cache so
    /// the colorscheme e2e test stays hermetic.
    fn spawn_with_env(args: &[&str], env: &[(&str, &str)], cols: u16, rows: u16) -> Session {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_nxvim"));
        for a in args {
            cmd.arg(a);
        }
        // Hermetic by default: unless the test supplies its own `NXVIM_CONFIG`, point
        // the child at a fresh *empty* config dir so startup never reads the
        // developer's real `~/.config/nxvim`. Otherwise the test's outcome (and
        // whether it even starts promptly) would depend on whatever plugins / init.lua
        // happen to be installed locally — a test must not depend on the environment.
        let cfg_dir = (!env.iter().any(|(k, _)| *k == "NXVIM_CONFIG")).then(|| {
            let dir = empty_config_dir();
            cmd.env("NXVIM_CONFIG", &dir);
            dir
        });
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn nxvim");
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = pair.master.take_writer().expect("writer");

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let sink = parser.clone();
        // Continuously drain the PTY so the deadline logic in `wait_until` never
        // blocks on a read. The thread ends when the child closes the PTY.
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().process(&buf[..n]),
                }
            }
        });

        Session {
            writer,
            parser,
            _child: child,
            _master: pair.master,
            _cfg_dir: cfg_dir,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write");
        self.writer.flush().expect("flush");
    }

    /// Poll the parsed screen until `pred` holds or `timeout` elapses.
    fn wait_until(&self, timeout: Duration, pred: impl Fn(&vt100::Screen) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let guard = self.parser.lock().unwrap();
                if pred(guard.screen()) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                let guard = self.parser.lock().unwrap();
                return pred(guard.screen());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn screen_text(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Poll for the child to exit, returning its status (or `None` on timeout).
    fn wait_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self._child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Session {
    /// Kill the child on teardown so a failed assertion (which panics and
    /// unwinds) can't leave an orphaned `nxvim` process and a reader thread
    /// blocked forever on `read`. Killing the child closes the PTY, so the
    /// reader thread gets EOF and exits. Happy-path tests already `:q!` first,
    /// so this is a no-op for them.
    fn drop(&mut self) {
        let _ = self._child.kill();
        if let Some(dir) = &self._cfg_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn startup_shows_the_file_contents() {
    let path = std::env::temp_dir().join(format!("nxvim_e2e_startup_{}.txt", std::process::id()));
    std::fs::write(&path, "alpha\nbeta\n").unwrap();

    let mut s = Session::spawn(&[path.to_str().unwrap()], 80, 24);
    let ok = s.wait_until(Duration::from_secs(5), |scr| {
        let t = scr.contents();
        t.contains("alpha") && t.contains("beta")
    });
    assert!(ok, "screen never showed the file:\n{}", s.screen_text());

    s.send(b":q!\r");
    std::fs::remove_file(&path).ok();
}

#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn typing_appears_on_screen_and_mode_flips() {
    let mut s = Session::spawn(&[], 80, 24);
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| scr
            .contents()
            .contains("NORMAL")),
        "no NORMAL status at startup:\n{}",
        s.screen_text()
    );

    s.send(b"ihi");
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| {
            let t = scr.contents();
            t.contains("INSERT") && t.contains("hi")
        }),
        "after typing 'ihi':\n{}",
        s.screen_text()
    );

    s.send(b"\x1b"); // Esc
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| scr
            .contents()
            .contains("NORMAL")),
        "did not return to NORMAL:\n{}",
        s.screen_text()
    );

    s.send(b":q!\r");
}

#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn client_stays_responsive_while_the_editor_sleeps() {
    let mut s = Session::spawn(&[], 80, 24);
    assert!(s.wait_until(Duration::from_secs(5), |scr| scr
        .contents()
        .contains("NORMAL")));

    // Put the editor to sleep, then immediately type. This proves input sent
    // during a slow editor operation is not dropped — it is buffered and
    // applied once the editor wakes. (The stronger guarantee that the UI never
    // stalls while the editor is busy is covered deterministically in Tier 2,
    // tests/screen.rs; the PTY tier can't prove timing robustly.)
    s.send(b":sleep 800m\r");
    s.send(b"ihi\x1b");
    assert!(
        s.wait_until(Duration::from_secs(5), |scr| scr.contents().contains("hi")),
        "input typed during :sleep never applied:\n{}",
        s.screen_text()
    );

    s.send(b":q!\r");
}

/// Locate a catppuccin checkout to drive the real colorscheme through the full
/// stack. Prefers `$NXVIM_CATPPUCCIN`, else the place a user installs it
/// (`~/.config/nxvim/pack/plugins/start/catppuccin`). `None` (→ the test skips)
/// when it isn't on disk, since we deliberately don't vendor it into the repo.
fn catppuccin_dir() -> Option<PathBuf> {
    let candidate = std::env::var_os("NXVIM_CATPPUCCIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".config/nxvim/pack/plugins/start/catppuccin"))
        })?;
    candidate
        .join("lua/catppuccin/init.lua")
        .is_file()
        .then_some(candidate)
}

/// Tier 3, the visible payoff: the **real** catppuccin plugin, loaded from a
/// user config at startup, repaints the editor in 24-bit color — proven by the
/// truecolor escapes crossterm actually emits, decoded by `vt100`. Hermetic
/// except for the plugin checkout itself (skipped when absent; we don't vendor
/// it): a throwaway config dir, runtimepath, and compile cache are wired via env.
#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn catppuccin_repaints_the_editor_in_truecolor() {
    let Some(catppuccin) = catppuccin_dir() else {
        eprintln!(
            "skipping: no catppuccin checkout found \
             (set $NXVIM_CATPPUCCIN or install it under ~/.config/nxvim)"
        );
        return;
    };

    // A throwaway config (init.lua loads catppuccin) + a redirected compile
    // cache, so the test neither reads nor writes the user's real dirs.
    let base = std::env::temp_dir().join(format!("nxvim_e2e_cat_{}", std::process::id()));
    let config = base.join("config");
    let cache = base.join("cache");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        config.join("init.lua"),
        "require('catppuccin').setup({ flavour = 'mocha' })\n\
         vim.cmd.colorscheme('catppuccin')\n",
    )
    .unwrap();
    // A plain-text buffer: no grammar needed, so its glyphs paint in the theme's
    // `Normal` foreground rather than a treesitter capture color.
    let file = base.join("hello.txt");
    std::fs::write(&file, "hello\n").unwrap();

    let mut s = Session::spawn_with_env(
        &[file.to_str().unwrap()],
        &[
            ("NXVIM_CONFIG", config.to_str().unwrap()),
            ("NXVIM_RUNTIMEPATH", catppuccin.to_str().unwrap()),
            ("XDG_CACHE_HOME", cache.to_str().unwrap()),
        ],
        80,
        24,
    );

    // catppuccin-mocha: text foreground #cdd6f4, base background #1e1e2e.
    let text_fg = vt100::Color::Rgb(0xcd, 0xd6, 0xf4);
    let base_bg = vt100::Color::Rgb(0x1e, 0x1e, 0x2e);

    // The first text cell of "hello" must carry both — the colorscheme loaded at
    // startup and the client emitted real 24-bit escapes for it.
    let themed = |scr: &vt100::Screen| {
        (0..80).any(|col| {
            scr.cell(0, col).is_some_and(|c| {
                c.contents() == "h" && c.fgcolor() == text_fg && c.bgcolor() == base_bg
            })
        })
    };
    let ok = s.wait_until(Duration::from_secs(10), themed);
    assert!(
        ok,
        "catppuccin never themed the 'hello' text in truecolor:\n{}",
        s.screen_text()
    );

    s.send(b":q!\r");
    let _ = std::fs::remove_dir_all(&base);
}

/// The zero-config payoff: on a **truecolor** terminal, with an *empty* config,
/// nxvim defaults its bundled `nxvim` One Dark colorscheme in at startup — the
/// client detects 24-bit support (`COLORTERM=truecolor`), reports it on attach, and
/// the server auto-loads the scheme, so plain text paints in the theme's `Normal`.
/// Fully hermetic (the scheme is baked into the binary — no plugin checkout): the
/// truecolor twin of `catppuccin_repaints_the_editor_in_truecolor`, minus the user
/// config. `COLORTERM` is set explicitly so the outcome never depends on the
/// terminal the test itself runs under.
#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn truecolor_terminal_defaults_in_the_nxvim_colorscheme() {
    let base = std::env::temp_dir().join(format!("nxvim_e2e_tc_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    // A plain-text buffer (no grammar), so its glyphs paint in the theme's `Normal`
    // foreground rather than a treesitter capture color.
    let file = base.join("hello.txt");
    std::fs::write(&file, "hello\n").unwrap();

    // No NXVIM_CONFIG override ⇒ the harness points at a fresh *empty* config dir,
    // so nothing loads a colorscheme — the auto-default is the only thing that can.
    let mut s = Session::spawn_with_env(
        &[file.to_str().unwrap()],
        &[("COLORTERM", "truecolor")],
        80,
        24,
    );

    // nxvim One Dark: Normal foreground #abb2bf, background #282c34.
    let text_fg = vt100::Color::Rgb(0xab, 0xb2, 0xbf);
    let base_bg = vt100::Color::Rgb(0x28, 0x2c, 0x34);
    let themed = |scr: &vt100::Screen| {
        (0..80).any(|col| {
            scr.cell(0, col).is_some_and(|c| {
                c.contents() == "h" && c.fgcolor() == text_fg && c.bgcolor() == base_bg
            })
        })
    };
    let ok = s.wait_until(Duration::from_secs(10), themed);
    assert!(
        ok,
        "the bundled nxvim scheme never auto-loaded on a truecolor terminal:\n{}",
        s.screen_text()
    );

    s.send(b":q!\r");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn a_server_thread_panic_exits_nonzero() {
    // R9: a panic on the server thread must surface as a non-zero exit, not look
    // like a clean `:q` (the old `let _ = join()` discarded the payload and
    // exited 0). The debug-only `NXVIM_PANIC_TEST` hook forces that panic; with
    // it set the process must exit with the panic code 101.
    let mut s = Session::spawn_with_env(&[], &[("NXVIM_PANIC_TEST", "1")], 80, 24);
    let status = s
        .wait_exit(Duration::from_secs(10))
        .expect("nxvim should exit promptly after the server thread panics");
    assert!(!status.success(), "a server-thread panic must not exit 0");
    assert_eq!(
        status.exit_code(),
        101,
        "expected the panic exit code 101, got {}",
        status.exit_code()
    );
}

#[test]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn an_edit_host_connect_failure_exits_nonzero() {
    // A server-thread *error* (not just a panic) must surface as a non-zero exit.
    // The cheapest real trigger: a `nxvim://…` connect target with a malformed cert
    // hash — `connect_quic_reconnecting` fails immediately, the server thread returns
    // `Err`, and the process must not report success to the shell (the old code
    // printed "edit-host error" to stderr and then exited 0, indistinguishable from
    // a clean `:q` in scripts).
    let mut s = Session::spawn(&["nxvim://127.0.0.1:1/tok?cert=zz"], 80, 24);
    let status = s
        .wait_exit(Duration::from_secs(15))
        .expect("nxvim should exit promptly when the daemon connect fails");
    assert!(
        !status.success(),
        "an edit-host connect failure must not exit 0"
    );
}

#[test]
#[cfg(unix)]
#[ignore = "PTY/terminal e2e; needs a real controlling terminal. Run with --ignored. See module header."]
fn daemon_stderr_log_is_private_and_per_pid() {
    // The `--connect-daemon` edit-host redirects the daemon child's stderr to a log
    // under the temp dir. It must be a *private* (owner-only, `0600`) file at a
    // *per-pid* name — not the old fixed, world-readable `nxvim-daemon.log`, which let
    // another local user pre-plant a symlink there and have the daemon's stderr truncate
    // one of the victim's files (CWE-377), and exposed the daemon's diagnostics `0644`.
    use std::os::unix::fs::PermissionsExt;

    // A benign "daemon" that stays alive but never speaks the wire, so the edit-host
    // creates the stderr log (done at connect, before the handshake) and we can inspect
    // it without a real daemon.
    let mut s = Session::spawn_with_env(
        &["--connect-daemon"],
        &[("NXVIM_DAEMON_CMD", "sleep 30")],
        80,
        24,
    );
    let pid = s._child.process_id().expect("the spawned nxvim has a pid");
    let log = std::env::temp_dir().join(format!("nxvim-daemon-{pid}.log"));

    // Poll for the log to appear.
    let mut meta = None;
    for _ in 0..200 {
        if let Ok(m) = std::fs::metadata(&log) {
            meta = Some(m);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let meta = meta.unwrap_or_else(|| panic!("daemon stderr log {} never appeared", log.display()));

    let mode = meta.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "daemon stderr log must be owner-only 0600, got {mode:o}"
    );

    s.send(b":q!\r");
    std::fs::remove_file(&log).ok();
}
