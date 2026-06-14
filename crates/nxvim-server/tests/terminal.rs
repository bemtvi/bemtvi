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
use nxvim_test_harness::{command, feed, lines, mode, serial_lock, start_attached};
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
