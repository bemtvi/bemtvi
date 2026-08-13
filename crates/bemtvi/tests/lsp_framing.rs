//! What a language server's *stdout framing* and *stderr volume* may cost the
//! editor. Both channels are attacker-influenced in the sense that matters here:
//! a server is an external program, and a corrupt (or merely buggy — a server
//! that prints to stdout mid-stream) frame header must not be able to take the
//! editor down with it.
//!
//! Two concrete unbounded-allocation paths, both reachable without the server
//! being malicious at all:
//!
//! 1. **`Content-Length`.** `async-lsp` 0.2.4's `Message::read` does
//!    `vec![0u8; content_len]` straight from the parsed header, with no cap. An
//!    announced 999999999999 is a ~1 TB allocation request: the allocator fails
//!    and Rust *aborts the process* — the whole editor, unsaved buffers and all.
//!    The `FramingGuard` between the child's stdout and the main loop rejects the
//!    announcement first, so the connection dies instead of the editor.
//! 2. **stderr.** The drain reads to the next `\n`; a server that writes megabytes
//!    without one grows the buffer to match. Each read is capped instead, and the
//!    over-long line is logged truncated.
//!
//! Wired like `lsp_stderr.rs`: the scripted mock (`bemtvi --__lsp-mock`) stands in
//! for the server via `$BEMTVI_LSP_CMD` (process-global env ⇒ `serial_lock`), and
//! the manager's log file — pointed at the test's temp dir — is where the
//! rejection and the truncation are observable.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, serial_lock, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

/// Write a mock LSP script and point `$BEMTVI_LSP_CMD` at the binary's
/// `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer (filetype `rust`) and attach.
async fn open_rust(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Configure + enable the mock server for `rust` buffers.
async fn enable_mock(rpc: &Rpc) {
    exec_lua(
        rpc,
        r#"
        btv.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;
}

/// Poll the manager's log file until it contains `want`, then return it. Returns
/// `None` if it never does — the caller decides how loudly that fails.
async fn await_log(path: &Path, want: &str) -> Option<String> {
    for _ in 0..200 {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.contains(want) {
                return Some(text);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

/// The editor is still answering RPC — i.e. it survived whatever the server did.
/// (A `vec![0u8; huge]` abort would have taken this test binary with it, so
/// reaching the assertion at all is half the proof; this makes the other half —
/// a *live* editor rather than a wedged one — explicit.)
async fn still_alive(rpc: &Rpc) {
    let answer = exec_lua(rpc, "return 6 * 7").await;
    assert_eq!(
        answer,
        Value::from(42),
        "the editor must keep serving Lua after the server's stream failed"
    );
}

/// A server announcing an enormous `Content-Length` must lose its *connection*,
/// not the editor's *process*. Pre-guard, `async-lsp` allocated the announced
/// 999999999999 bytes the moment it finished parsing the header — the allocator
/// fails and the process aborts, which in this harness (the server runs
/// in-process) kills the test binary outright.
#[tokio::test]
async fn an_enormous_content_length_fails_the_connection_not_the_editor() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_framing_huge");
    let log = dir.join("lsp.log");
    std::env::set_var("BEMTVI_LSP_LOG_FILE", &log);
    arm_mock(&dir, r#"{ "bogus_content_length": "999999999999" }"#);

    let (rpc, _incoming) = open_rust(&dir).await;
    enable_mock(&rpc).await;

    let text = await_log(&log, "Content-Length exceeds")
        .await
        .unwrap_or_else(|| {
            panic!(
                "the framing guard should have rejected the announcement; log was {:?}",
                std::fs::read_to_string(&log).unwrap_or_default()
            )
        });
    assert!(
        text.contains("language server stream failed"),
        "the rejection must be reported as the reason the loop ended, not swallowed: {text}"
    );
    still_alive(&rpc).await;

    std::env::remove_var("BEMTVI_LSP_CMD");
    std::env::remove_var("BEMTVI_LSP_LOG_FILE");
}

/// The same announcement with a leading `+`. `usize::from_str` — which is exactly
/// how `async-lsp` parses the header value — accepts `+999999999999`, so a guard
/// that only recognises bare digits reads the line as unparseable, declines to
/// bound it, and hands `async-lsp` the very allocation it exists to prevent. The
/// guard must accept every form the parser behind it does.
#[tokio::test]
async fn a_plus_prefixed_content_length_does_not_slip_past_the_guard() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_framing_plus");
    let log = dir.join("lsp.log");
    std::env::set_var("BEMTVI_LSP_LOG_FILE", &log);
    arm_mock(&dir, r#"{ "bogus_content_length": "+999999999999" }"#);

    let (rpc, _incoming) = open_rust(&dir).await;
    enable_mock(&rpc).await;

    let text = await_log(&log, "Content-Length exceeds")
        .await
        .unwrap_or_else(|| {
            panic!(
                "`+`-prefixed lengths parse the same as bare ones — the guard must \
                 bound them too; log was {:?}",
                std::fs::read_to_string(&log).unwrap_or_default()
            )
        });
    assert!(
        text.contains("language server stream failed"),
        "the rejection must be reported as the reason the loop ended: {text}"
    );
    still_alive(&rpc).await;

    std::env::remove_var("BEMTVI_LSP_CMD");
    std::env::remove_var("BEMTVI_LSP_LOG_FILE");
}

/// A server spewing a single 256 KiB line with no newline in it must not make the
/// drain's buffer grow to 256 KiB (nor to whatever the server chooses next). Each
/// read is capped, so the line reaches the log in bounded pieces, the first marked
/// truncated — and the drain keeps up, which the server proves by staying alive to
/// answer `initialize` afterwards (it exits on a failed stderr write).
#[tokio::test]
async fn an_unterminated_stderr_line_is_logged_truncated_not_buffered_whole() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_framing_stderr");
    let log = dir.join("lsp.log");
    std::env::set_var("BEMTVI_LSP_LOG_FILE", &log);
    // 256 KiB in one line — four times the default pipe capacity, so the mock
    // blocks (and would die) unless the client keeps draining.
    arm_mock(&dir, r#"{ "stderr_long_line": 262144 }"#);

    let (rpc, _incoming) = open_rust(&dir).await;
    enable_mock(&rpc).await;

    let text = await_log(&log, "truncated").await.unwrap_or_else(|| {
        panic!(
            "an over-long stderr line should be logged truncated, not buffered whole; \
             log was {:?}",
            std::fs::read_to_string(&log).unwrap_or_default()
        )
    });
    // The cap is what bounds memory: no single logged line may carry the whole
    // 256 KiB the server wrote.
    let longest = text.lines().map(str::len).max().unwrap_or(0);
    assert!(
        longest < 32 * 1024,
        "the drain buffered {longest} bytes into one log line — the per-read cap is \
         what keeps a server's stderr from sizing the editor's memory"
    );
    still_alive(&rpc).await;

    std::env::remove_var("BEMTVI_LSP_CMD");
    std::env::remove_var("BEMTVI_LSP_LOG_FILE");
}
