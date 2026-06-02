//! Worker-level black-box tests: drive the public `run_worker` over an in-memory
//! pipe (exactly as the editor server drives the child process) and assert on
//! the `ts_*` notifications it sends back. These reach the worker's trust
//! boundary — a malformed `language` or edit delta arriving from the wire — that
//! the full-stack server tests can't, because the server never emits malformed
//! messages.
//!
//! The tests share process-global env (`NXVIM_DATA_DIR`) and a compiled grammar
//! fixture, so they serialize on a single lock.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_ts::run_worker;
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;

/// Serializes the env-mutating, subprocess-free worker tests.
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ----- grammar fixture (mirrors nxvim/tests/syntax.rs) ----------------------

/// Build (once) a `NXVIM_DATA_DIR` containing a compiled tree-sitter-rust
/// grammar + its highlights query, and point the worker's `data_dir()` at it.
fn fixture_data_dir() -> &'static Path {
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATA_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join("nxvim-ts-worker-fixture");
        let parser_dir = dir.join("parser");
        let query_dir = dir.join("queries").join("rust");
        std::fs::create_dir_all(&parser_dir).unwrap();
        std::fs::create_dir_all(&query_dir).unwrap();

        let src = grammar_src_dir().join("src");
        let out = parser_dir.join("rust.so");
        let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
        let status = std::process::Command::new(compiler)
            .args(["-shared", "-fPIC", "-O1"])
            .arg("-I")
            .arg(&src)
            .arg(src.join("parser.c"))
            .arg(src.join("scanner.c"))
            .arg("-o")
            .arg(&out)
            .status()
            .expect("run C compiler");
        assert!(status.success(), "compiling rust grammar fixture failed");

        std::fs::write(
            query_dir.join("highlights.scm"),
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        )
        .unwrap();

        std::env::set_var("NXVIM_DATA_DIR", &dir);
        dir
    })
}

fn grammar_src_dir() -> PathBuf {
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".cargo"));
    let registry = cargo_home.join("registry").join("src");
    for index in std::fs::read_dir(&registry).expect("read cargo registry src") {
        let candidate = index.unwrap().path().join("tree-sitter-rust-0.24.2");
        if candidate.is_dir() {
            return candidate;
        }
    }
    panic!("tree-sitter-rust-0.24.2 source not found under {registry:?}");
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .expect("HOME")
}

// ----- worker harness -------------------------------------------------------

/// Spawn `run_worker` over a duplex pipe and return a connected client.
fn spawn_worker() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (worker_end, client_end) = tokio::io::duplex(1 << 16);
    let (wr, ww) = tokio::io::split(worker_end);
    tokio::spawn(run_worker(wr, ww));
    let (cr, cw) = tokio::io::split(client_end);
    connect(cr, cw)
}

fn msg(pairs: Vec<(&str, Value)>) -> Vec<Value> {
    vec![Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )]
}

fn field<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Wait (bounded) for the next notification with `method` and return its map.
async fn next_notification(
    incoming: &mut UnboundedReceiver<Incoming>,
    method: &str,
) -> Vec<(Value, Value)> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), incoming.recv()).await {
            Ok(Some(Incoming::Notification { method: m, params })) if m == method => {
                if let Some(Value::Map(map)) = params.into_iter().next() {
                    return map;
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("worker closed the connection while waiting for {method}"),
            Err(_) => panic!("timed out waiting for the worker's {method} reply"),
        }
    }
}

// ----- S1: path-traversal allowlist -----------------------------------------

#[tokio::test]
async fn rejects_language_names_that_escape_the_data_dir() {
    let _guard = test_lock().lock().await;
    fixture_data_dir(); // sets NXVIM_DATA_DIR to a known-good dir
    let (rpc, mut incoming) = spawn_worker();

    // A language name with path components would, unvalidated, join into the
    // parser `.so` path and let the worker dlopen an arbitrary shared object.
    rpc.notify(
        "ts_open",
        msg(vec![
            ("buffer", Value::from(1u64)),
            ("tick", Value::from(1u64)),
            ("language", Value::from("../../../../etc/passwd")),
            ("text", Value::from("code")),
            ("first_line", Value::from(0u64)),
            ("last_line", Value::from(1u64)),
        ]),
    );

    let err = next_notification(&mut incoming, "ts_error").await;
    let message = field(&err, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        message.contains("invalid language name"),
        "a traversing language name must be rejected by the allowlist before any \
         filesystem access, got: {message:?}"
    );
}

// ----- S2: malformed edit deltas --------------------------------------------

#[tokio::test]
async fn malformed_edit_neither_crashes_nor_silences_the_buffer() {
    let _guard = test_lock().lock().await;
    fixture_data_dir();
    let (rpc, mut incoming) = spawn_worker();

    // Open a real rust buffer and drain its initial highlights.
    rpc.notify(
        "ts_open",
        msg(vec![
            ("buffer", Value::from(1u64)),
            ("tick", Value::from(1u64)),
            ("language", Value::from("rust")),
            ("text", Value::from("fn main() {}\n")),
            ("first_line", Value::from(0u64)),
            ("last_line", Value::from(1u64)),
        ]),
    );
    next_notification(&mut incoming, "ts_highlights").await;

    // A delta whose byte offsets run far past the text. Unvalidated, the shadow
    // `remove(..)` panics inside engine.edit; the worker's catch_unwind swallows
    // it, so the handler's follow-up send_highlights never runs and no reply
    // arrives — the buffer goes silently dark. Validated, the bad delta is
    // dropped and highlights for the (unchanged) buffer are still sent.
    let bad_edit = Value::Array(vec![
        Value::from(10_000_000u64), // start_byte
        Value::from(10_000_005u64), // old_end_byte
        Value::from(10_000_000u64), // new_end_byte
        Value::from(999u64),        // start_row
        Value::from(0u64),          // start_col
        Value::from(999u64),        // old_end_row
        Value::from(5u64),          // old_end_col
        Value::from(999u64),        // new_end_row
        Value::from(0u64),          // new_end_col
        Value::from(""),            // text
    ]);
    rpc.notify(
        "ts_edit",
        msg(vec![
            ("buffer", Value::from(1u64)),
            ("tick", Value::from(2u64)),
            ("edits", Value::Array(vec![bad_edit])),
            ("first_line", Value::from(0u64)),
            ("last_line", Value::from(1u64)),
        ]),
    );

    let after = next_notification(&mut incoming, "ts_highlights").await;
    assert_eq!(
        field(&after, "tick").and_then(Value::as_u64),
        Some(2),
        "the edit should still produce a highlights reply, not panic into silence"
    );
}
