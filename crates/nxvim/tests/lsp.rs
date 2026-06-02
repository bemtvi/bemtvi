//! LSP Phase 1 (lifecycle + document sync), end to end through the real stack:
//! the in-process server spawns the **real** `nxvim` binary as a scripted mock
//! language server (`--__lsp-mock`, selected via `NXVIM_LSP_CMD`), which speaks
//! real LSP over stdio and records every message it receives to a file. LSP
//! replies are asynchronous, so these tests **poll** the record file / buffer
//! state (bounded wait) until the expected message arrives, exactly like the
//! syntax tests poll redraws.
//!
//! These tests spawn subprocesses and share process-global env, so they
//! serialize on a single lock.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use rmpv::Value;
use serde_json::Value as Json;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Serializes the subprocess-spawning tests (shared env + server lifecycle).
fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ----- mock configuration ---------------------------------------------------

/// Write a mock script with a unique record file and point `NXVIM_LSP_CMD` at
/// `nxvim --__lsp-mock <script>`, so every configured filetype launches the mock
/// (the LSP analogue of `NXVIM_TS_WORKER`). `extra` merges script fields
/// (`position_encoding`, `sync_kind`, `exit_after_initialize`). Returns the
/// record file path. Also points the *syntax* worker env at the real binary so a
/// `.rs` buffer doesn't try to re-spawn this test executable as a ts worker.
fn configure_mock(tag: &str, extra: Json) -> PathBuf {
    let dir = std::env::temp_dir();
    let record = dir.join(format!("nxvim-lsp-rec-{}-{tag}.jsonl", std::process::id()));
    let script_path = dir.join(format!(
        "nxvim-lsp-script-{}-{tag}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&record);

    let mut script = serde_json::json!({ "record": record.to_str().unwrap() });
    if let Json::Object(fields) = extra {
        for (k, v) in fields {
            script[k] = v;
        }
    }
    std::fs::write(&script_path, serde_json::to_string(&script).unwrap()).unwrap();

    let bin = env!("CARGO_BIN_EXE_nxvim");
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{bin} --__lsp-mock {}", script_path.display()),
    );
    std::env::set_var("NXVIM_TS_WORKER", bin);
    record
}

/// Parse the record file into the `{method, params}` objects the mock appended.
fn record_lines(path: &Path) -> Vec<Json> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The first recorded message with the given method, if any.
fn find<'a>(recs: &'a [Json], method: &str) -> Option<&'a Json> {
    recs.iter().find(|r| r["method"] == method)
}

/// True once a message with `method` has been recorded.
fn has_method(recs: &[Json], method: &str) -> bool {
    recs.iter().any(|r| r["method"] == method)
}

// ----- server harness -------------------------------------------------------

/// Start a server (LSP + syntax enabled) editing `file`, attach a UI. Mirrors the
/// syntax tests' harness.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (server_end, client_end) = tokio::io::duplex(1 << 16);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(
            server_end,
            ServerInit {
                file,
                ..Default::default()
            },
        ));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![
            Value::from(COLS as u64),
            Value::from((ROWS - 2) as u64),
            Value::Map(vec![]),
        ],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

fn feed(rpc: &Rpc, keys: &str) {
    rpc.notify("nvim_input", vec![Value::from(keys)]);
}

/// Force the server to process pending input and emit a redraw (which drives LSP
/// document sync), so the mock has a chance to receive the next message.
async fn barrier(rpc: &Rpc) {
    rpc.request(
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    .expect("barrier");
}

async fn lines(rpc: &Rpc) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0u64),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .expect("get_lines");
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Poll (bounded) until the record file satisfies `pred`, returning the parsed
/// records. Panics with the records seen if it never does.
async fn wait_for_record(rpc: &Rpc, record: &Path, pred: impl Fn(&[Json]) -> bool) -> Vec<Json> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        let recs = record_lines(record);
        if pred(&recs) {
            return recs;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "record never satisfied predicate; saw: {:?}",
        record_lines(record)
    );
}

/// Write `content` to a fresh temp file with `ext` and return its path string.
fn temp_file(tag: &str, ext: &str, content: &str) -> String {
    let path = std::env::temp_dir().join(format!("nxvim-lsp-{}-{tag}.{ext}", std::process::id()));
    std::fs::write(&path, content).unwrap();
    path.display().to_string()
}

/// Concatenate the `text` of every `content_changes` entry across all recorded
/// `didChange` notifications — the full text the deltas inserted.
fn did_change_text(recs: &[Json]) -> String {
    recs.iter()
        .filter(|r| r["method"] == "textDocument/didChange")
        .flat_map(|r| {
            r["params"]["contentChanges"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .collect()
}

// ----- tests ----------------------------------------------------------------

#[tokio::test]
async fn opening_a_rust_buffer_initializes_and_did_opens() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("init", serde_json::json!({}));
    let content = "fn main() {}\n";
    let file = temp_file("init", "rs", content);
    let (rpc, _incoming) = start(Some(file)).await;

    // The handshake then the first didOpen flow asynchronously: poll until both
    // are recorded.
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // initialize advertised utf-8 (preferred) among the position encodings.
    let init = find(&recs, "initialize").expect("an initialize request");
    let encodings = &init["params"]["capabilities"]["general"]["positionEncodings"];
    assert_eq!(
        encodings[0].as_str(),
        Some("utf-8"),
        "utf-8 should be advertised first, got {encodings:?}"
    );

    // didOpen carries the buffer text and the rust languageId, at version 1.
    let open = find(&recs, "textDocument/didOpen").unwrap();
    let doc = &open["params"]["textDocument"];
    assert_eq!(doc["text"].as_str(), Some(content));
    assert_eq!(doc["languageId"].as_str(), Some("rust"));
    assert_eq!(doc["version"].as_i64(), Some(1));
}

#[tokio::test]
async fn typing_sends_an_incremental_did_change_with_a_version_bump() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("change", serde_json::json!({ "sync_kind": "incremental" }));
    let file = temp_file("change", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Wait for the document to open first.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Insert "hello" at the top in one input batch.
    feed(&rpc, "ggihello<Esc>");
    let recs = wait_for_record(&rpc, &record, |r| did_change_text(r).contains("hello")).await;

    // The change(s) carry the inserted text, and the version bumped past the
    // didOpen's version 1.
    assert!(did_change_text(&recs).contains("hello"));
    let change = find(&recs, "textDocument/didChange").unwrap();
    assert!(
        change["params"]["textDocument"]["version"]
            .as_i64()
            .unwrap()
            >= 2,
        "the document version should bump past 1, got {:?}",
        change["params"]["textDocument"]["version"]
    );
    // Incremental sync sends ranges, not just full text.
    assert!(
        change["params"]["contentChanges"][0]["range"].is_object(),
        "incremental changes carry a range"
    );
}

#[tokio::test]
async fn a_non_ascii_prefix_yields_the_right_utf8_position() {
    // Regression guard for Decision 4: positions are byte/encoding units, not
    // char counts. The line starts with a 2-byte `é`, so an edit appended after
    // it must land at character 2 under the negotiated utf-8 encoding (a char
    // count would wrongly say 1).
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "utf8",
        serde_json::json!({ "position_encoding": "utf-8", "sync_kind": "incremental" }),
    );
    let file = temp_file("utf8", "rs", "é\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Append `x` at end of the line (byte column 2, right after `é`).
    feed(&rpc, "Ax<Esc>");
    let recs = wait_for_record(&rpc, &record, |r| did_change_text(r).contains('x')).await;

    let change = recs
        .iter()
        .find(|r| {
            r["method"] == "textDocument/didChange"
                && r["params"]["contentChanges"][0]["text"] == "x"
        })
        .expect("a didChange inserting x");
    let start = &change["params"]["contentChanges"][0]["range"]["start"];
    assert_eq!(start["line"].as_i64(), Some(0));
    assert_eq!(
        start["character"].as_i64(),
        Some(2),
        "the insert is at byte/utf-8 column 2 (after the 2-byte é), not char count 1"
    );
}

#[tokio::test]
async fn writing_then_deleting_sends_did_save_and_did_close() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("save", serde_json::json!({}));
    let file = temp_file("save", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Edit then write: the cleared `modified` with no further text change is a :w.
    feed(&rpc, "ohello<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
        .await
        .expect("write");
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didSave")).await;

    // Delete the buffer: didClose.
    rpc.request("nvim_command", vec![Value::from("bd")])
        .await
        .expect("bdelete");
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didClose")).await;
    assert!(has_method(&recs, "textDocument/didSave"));
    assert!(has_method(&recs, "textDocument/didClose"));
}

#[tokio::test]
async fn a_plain_text_buffer_starts_no_server() {
    let _guard = test_lock().lock().await;
    // The mock is configured, but a `.txt` filetype maps to no server.
    let record = configure_mock("plain", serde_json::json!({}));
    let file = temp_file("plain", "txt", "just text\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Give any (erroneous) server time to start and receive a message.
    feed(&rpc, "ihello<Esc>");
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        record_lines(&record).is_empty(),
        "a .txt buffer must never start a language server, got {:?}",
        record_lines(&record)
    );
}

#[tokio::test]
async fn the_editor_survives_a_server_that_exits_after_initialize() {
    // Resilience: the mock replies to initialize then exits, every time. The
    // manager respawns it (then the breaker gives up), but the editor must stay
    // fully responsive throughout — the LSP analogue of the syntax crash test.
    let _guard = test_lock().lock().await;
    configure_mock(
        "resil",
        serde_json::json!({ "exit_after_initialize": true }),
    );
    let file = temp_file("resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    // Hammer the editor with edits while the server crash-loops.
    feed(&rpc, "ggdGiline one<CR>line two<CR>line three<Esc>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string()
        ],
        "the editor must apply every keystroke regardless of the dying server"
    );
}
