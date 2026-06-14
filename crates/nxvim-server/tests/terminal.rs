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
use nxvim_test_harness::{command, cursor, feed, lines, mode, serial_lock, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
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

/// Re-entering terminal mode (`i`) keeps the cursor at the current position — like
/// `i` in a normal buffer — instead of snapping to the child's last input point.
#[tokio::test]
async fn reentering_keeps_cursor_at_navigated_position() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    feed(&rpc, "hello");
    wait_lines(&rpc, "the echoed 'hello'", |ls| has_line(ls, "hello")).await;

    // Leave to terminal-normal and move to the start of the input line.
    feed(&rpc, "<C-\\><C-n>0");
    assert_eq!(mode(&rpc).await, "n");
    let (_, ncol) = cursor(&rpc).await;
    assert_eq!(ncol, 0, "navigated to column 0");
    // Re-enter with `i`: the cursor stays at column 0 (where we navigated), not the
    // input end. `cat` gets no input, so nothing converges it away — deterministic.
    feed(&rpc, "i");
    assert_eq!(mode(&rpc).await, "t");
    let (_, ccol) = cursor(&rpc).await;
    assert_eq!(
        ccol, 0,
        "`i` enters at the cursor (col 0), not the input point"
    );
}

/// Triple-`<Esc>` is the discoverable escape hatch: three in a row leave to Normal,
/// while one or two stay in the job (forwarded to the child).
#[tokio::test]
async fn triple_esc_leaves_terminal_mode() {
    let _guard = serial_lock().lock().await;
    let (rpc, _incoming) = start().await;

    command(&rpc, "terminal cat").await;
    // One or two escapes do not leave — they go to the program.
    feed(&rpc, "<Esc><Esc>");
    assert_eq!(
        mode(&rpc).await,
        "t",
        "two escapes stay in the terminal job"
    );
    // The third in the run leaves to terminal-normal.
    feed(&rpc, "<Esc>");
    assert_eq!(
        mode(&rpc).await,
        "n",
        "the third escape leaves terminal mode"
    );
}
