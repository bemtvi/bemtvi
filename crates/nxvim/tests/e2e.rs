//! Tier 3: drive the real `nxvim` binary in a pseudo-terminal and assert on the
//! terminal output a user would actually see. This is the only tier that proves
//! real crossterm decode, real terminal escapes, and process startup/args. Kept
//! thin: it is the slow/flaky surface, so the bulk of coverage lives in Tiers 1–2.

use std::io::{Read, Write};
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
