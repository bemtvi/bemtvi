//! Tier 3: killing the GUI client still runs the editor's exit sequence.
//!
//! The GUI has no terminal to hand back — a window is not a tty — but it has just as
//! much to lose from being torn down mid-tick: `VimLeave` never fires, plugins never
//! persist their state, and the server's clean-exit shada flush (marks, registers,
//! histories, the exit cursor) never happens. So `kill` is routed the same way the
//! window's close button already is: through the server, as a quit.
//!
//! Proven by spawning the real `nxvim-gui` binary with a throwaway config whose
//! `VimLeave` autocmd writes the buffer to a marker path — the file exists only if
//! the sequence really ran before the process died. `#[ignore]`d: it needs a display
//! server and brings up a real GPU window, so run it deliberately with
//! `cargo test -p nxvim-gui --test signal_exit -- --ignored`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A throwaway config dir with autocmds that leave a trace on the way in and out, so
/// the test can tell when the editor is up and whether it left cleanly.
fn scratch_config() -> (PathBuf, PathBuf, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("nxvim_gui_signal_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch config dir");
    let entered = dir.join("entered");
    let left = dir.join("left");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "nx.autocmd.create(\"VimEnter\", {{\n  callback = function()\n    \
             nx.cmd(\"write! {}\")\n  end,\n}})\n\
             nx.autocmd.create(\"VimLeave\", {{\n  callback = function()\n    \
             nx.cmd(\"write! {}\")\n  end,\n}})\n",
            entered.display(),
            left.display()
        ),
    )
    .expect("write init.lua");
    (dir, entered, left)
}

fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait().expect("wait for nxvim-gui") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
}

fn has_display() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

#[test]
#[ignore = "spawns a real GPU window; needs a display server. Run with --ignored."]
fn killing_the_gui_runs_the_exit_sequence() {
    if !has_display() {
        eprintln!("skip: no WAYLAND_DISPLAY / DISPLAY to open a window on");
        return;
    }
    let (dir, entered, left) = scratch_config();

    let mut child = Command::new(env!("CARGO_BIN_EXE_nxvim-gui"))
        .env("NXVIM_CONFIG", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn nxvim-gui");

    // Up and running (a GPU window can take a while to come up on a cold cache).
    assert!(
        wait_for(Duration::from_secs(90), || entered.exists()),
        "nxvim-gui never reached VimEnter"
    );
    // The signal is delivered to the UI thread through the event-loop proxy, so give
    // that loop a moment to actually be running before killing.
    std::thread::sleep(Duration::from_secs(2));

    let pid = child.id() as i32;
    // SAFETY: plain `kill(2)` on a child we spawned — what the user typed.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0, "kill");

    let status = wait_for_exit(&mut child, Duration::from_secs(30)).unwrap_or_else(|| {
        let _ = child.kill();
        panic!("nxvim-gui did not exit after SIGTERM");
    });

    assert!(
        left.exists(),
        "VimLeave never ran on the way out ({})",
        left.display()
    );
    // A caught-and-cleaned-up SIGTERM must still look like a SIGTERM from outside.
    assert_eq!(
        signal_of(&status),
        Some(libc::SIGTERM),
        "a killed GUI must still report the signal that killed it"
    );
    cleanup(&dir);
}

fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}
