//! Black-box tests for `:terminal` (Phase 3 — the native PTY transport).
//!
//! A real server over RPC spawns a real local PTY child; the test drives it with
//! `nvim_input` and asserts on the buffer the child's output is mirrored into. PTY
//! output is asynchronous (a reader thread streams it back and the server settles
//! off-tick), so assertions poll the buffer until the expected text lands rather
//! than reading once. Hermetic: only POSIX `cat` is spawned (present on macOS/Linux).

use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, command, cursor, feed, lines, mode, serial_lock, spawn, start_attached, TestClock,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// The current buffer's reported name (`nvim_buf_get_name`).
async fn buf_name(rpc: &Rpc) -> String {
    rpc.request("nvim_buf_get_name", vec![Value::from(0u64)])
        .await
        .expect("get_name")
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Poll the current buffer until `pred` holds, or panic after ~10s. PTY output is
/// async, so the expected text may take a few ticks to stream in and settle.
async fn wait_lines(rpc: &Rpc, what: &str, pred: impl Fn(&[String]) -> bool) -> Vec<String> {
    for _ in 0..200 {
        let ls = lines(rpc).await;
        if pred(&ls) {
            return ls;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "timed out waiting for {what}; last buffer lines: {:?}",
        lines(rpc).await
    );
}

fn has_line(ls: &[String], text: &str) -> bool {
    ls.iter().any(|l| l.trim_end() == text)
}

/// `:terminal cat` runs a real child: typed input is echoed back into the buffer,
/// and `<C-d>` (EOF) ends it with the `[Process exited 0]` notice, dropping us back
/// to Normal mode.
#[tokio::test]
async fn terminal_echoes_input_and_reports_exit() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    // `:terminal` enters terminal-job mode; keystrokes go to the child.
    assert_eq!(
        mode(&rpc).await,
        "t",
        "should be in terminal mode after :terminal"
    );

    // `cat` echoes each line back: typing "hello<CR>" makes "hello" appear.
    feed(&rpc, "hello<CR>");
    wait_lines(&rpc, "cat to echo 'hello'", |ls| has_line(ls, "hello")).await;

    // `<C-d>` at the start of a line sends EOF; `cat` exits 0.
    feed(&rpc, "<C-d>");
    wait_lines(&rpc, "the process-exit notice", |ls| {
        ls.iter().any(|l| l.contains("[Process exited 0]"))
    })
    .await;
    // A dead terminal drops back to Normal mode.
    assert_eq!(
        mode(&rpc).await,
        "n",
        "should leave terminal mode when the child exits"
    );
}

/// `<C-\><C-n>` leaves terminal-job mode for terminal-normal (the buffer reads as
/// ordinary text for scrolling / yanking) without killing the child.
#[tokio::test]
async fn ctrl_backslash_ctrl_n_leaves_terminal_mode() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hi<CR>");
    wait_lines(&rpc, "cat to echo 'hi'", |ls| has_line(ls, "hi")).await;

    feed(&rpc, "<C-\\><C-n>");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "<C-\\><C-n> should return to normal mode"
    );

    // The echoed text is still there to navigate.
    assert!(
        has_line(&lines(&rpc).await, "hi"),
        "terminal output stays readable in normal mode"
    );

    // `i` re-enters terminal-job mode.
    feed(&rpc, "i");
    assert_eq!(mode(&rpc).await, "t", "`i` re-enters the terminal job");
}

/// The cursor sits at the child's next-write position (one past the last typed
/// char), not on top of it — terminal mode allows the cursor past end-of-line.
#[tokio::test]
async fn cursor_sits_after_the_last_typed_char() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello");
    // The PTY echoes typed chars immediately, so "hello" lands on row 1.
    let ls = wait_lines(&rpc, "the echoed 'hello'", |ls| has_line(ls, "hello")).await;
    let row = ls.iter().position(|l| l.trim_end() == "hello").unwrap();
    let (cline, ccol) = cursor(&rpc).await;
    assert_eq!(cline, row + 1, "cursor on the typed line");
    assert_eq!(
        ccol, 5,
        "cursor is after 'hello' (col 5), not on the 'o' (col 4)"
    );
}

/// `<C-4>` is how macOS / xterm deliver Ctrl-\ (crossterm decodes byte 0x1c as
/// Ctrl+'4'), so `<C-4><C-n>` must also leave terminal mode.
#[tokio::test]
async fn ctrl_4_is_accepted_as_ctrl_backslash() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "<C-4><C-n>");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "<C-4><C-n> (macOS Ctrl-\\) should leave terminal mode"
    );
}

/// Plain `<C-r>` reaches the child (shell reverse-search), while `<C-\><C-r>{reg}`
/// pastes a register into the terminal — the analogue of insert mode's `<C-r>{reg}`.
#[tokio::test]
async fn ctrl_backslash_ctrl_r_pastes_a_register() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    // Yank "hello" into register a (in the startup scratch buffer).
    feed(&rpc, "ihello<Esc>");
    feed(&rpc, "\"ayy");

    // Open a terminal and paste register a into it via the `<C-\><C-r>` chord.
    command(&rpc, "terminal cat").await;
    feed(&rpc, "<C-\\><C-r>a");
    wait_lines(&rpc, "register paste to reach cat", |ls| {
        has_line(ls, "hello")
    })
    .await;
}

/// Re-entering terminal mode (`i`) snaps the cursor back to the child's input
/// position, not wherever normal-mode navigation parked it.
#[tokio::test]
async fn reentering_snaps_cursor_to_the_input_position() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello");
    wait_lines(&rpc, "the echoed 'hello'", |ls| has_line(ls, "hello")).await;

    // Leave to terminal-normal and navigate up/to the start.
    feed(&rpc, "<C-\\><C-n>gg0");
    assert_eq!(mode(&rpc).await, "n");
    // Re-enter: the cursor jumps back to the input position (after "hello").
    feed(&rpc, "i");
    assert_eq!(mode(&rpc).await, "t");
    let (_cline, ccol) = cursor(&rpc).await;
    assert_eq!(
        ccol, 5,
        "re-entry snaps to the input position (col 5), not col 0"
    );
}

/// A terminal buffer's name is its window title: seeded from the spawned command,
/// then updated to the child's OSC title (`\e]2;…\a`) — what a desktop terminal
/// shows in its title bar.
#[tokio::test]
async fn terminal_buffer_name_tracks_the_window_title() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    assert_eq!(buf_name(&rpc).await, "cat", "name seeded from the command");

    // `cat` echoes its input, so feeding an OSC title sequence makes the emulator see
    // it (as if the child had emitted it): ESC ] 2 ; my-title BEL.
    feed(&rpc, "<Esc>]2;my-title<C-g><CR>");
    for _ in 0..200 {
        if buf_name(&rpc).await == "my-title" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        buf_name(&rpc).await,
        "my-title",
        "name follows the child's OSC window title"
    );
}

/// A live terminal buffer is read-only in terminal-normal mode: editing commands
/// (`x`, `dd`, …) are refused so they can't corrupt the mirrored screen. Once the
/// child exits, the buffer becomes an ordinary editable scratch buffer.
#[tokio::test]
async fn live_terminal_buffer_is_read_only() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello<CR>");
    wait_lines(&rpc, "cat to echo 'hello'", |ls| has_line(ls, "hello")).await;

    // Into terminal-normal, to the top, and try to delete — must be refused.
    feed(&rpc, "<C-\\><C-n>gg");
    let before = lines(&rpc).await;
    feed(&rpc, "ddx");
    assert_eq!(
        lines(&rpc).await,
        before,
        "edits must be refused while the terminal is live"
    );

    // End the child (EOF); the buffer becomes editable.
    feed(&rpc, "i<C-d>");
    wait_lines(&rpc, "the process-exit notice", |ls| {
        ls.iter().any(|l| l.contains("[Process exited 0]"))
    })
    .await;
    let before_dead = lines(&rpc).await;
    feed(&rpc, "ggdd");
    assert_ne!(
        lines(&rpc).await,
        before_dead,
        "a dead terminal buffer is editable"
    );
}

/// Start a clocked terminal session so the triple-`<Esc>` chord window is driven
/// deterministically (no wall-clock timing in the test).
async fn start_clocked() -> (Rpc, TestClock, UnboundedReceiver<Incoming>) {
    let clock = TestClock::new();
    let init = ServerInit {
        mouse_clock: Some(clock.handle()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, clock, incoming)
}

/// Triple-`<Esc>` in quick succession (each press within the chord window) leaves
/// terminal mode; the first two are still forwarded to the child.
#[tokio::test]
async fn triple_esc_quick_succession_leaves() {
    let _guard = serial_lock().lock().await;
    let (rpc, clock, _incoming) = start_clocked().await;

    clock.set_ms(1000);
    command(&rpc, "terminal cat").await;
    clock.set_ms(1000);
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "t", "one escape stays");
    clock.set_ms(1100);
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "t", "two escapes stay");
    clock.set_ms(1200);
    feed(&rpc, "<Esc>");
    assert_eq!(mode(&rpc).await, "n", "three quick escapes leave");
}

/// Escapes spaced further apart than the chord window never leave — so a TUI program
/// inside the terminal still gets each lone `<Esc>`.
#[tokio::test]
async fn slow_escapes_stay_in_terminal_mode() {
    let _guard = serial_lock().lock().await;
    let (rpc, clock, _incoming) = start_clocked().await;

    clock.set_ms(0);
    command(&rpc, "terminal cat").await;
    for t in [1000u64, 2000, 3000] {
        clock.set_ms(t);
        feed(&rpc, "<Esc>");
        assert_eq!(
            mode(&rpc).await,
            "t",
            "a spaced-out escape stays in the job"
        );
    }
}
