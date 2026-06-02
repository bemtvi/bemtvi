//! Tier 3: drive the real `nxvim` binary in a pseudo-terminal and assert on the
//! terminal output a user would actually see. This is the only tier that proves
//! real crossterm decode, real terminal escapes, and process startup/args. Kept
//! thin: it is the slow/flaky surface, so the bulk of coverage lives in Tiers 1–2.

use std::io::{Read, Write};
use std::path::PathBuf;
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
    }
}

#[test]
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

#[test]
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
