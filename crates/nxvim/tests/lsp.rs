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
use nxvim_tui::{paint, View};
use ratatui::style::{Color, Modifier};
use rmpv::Value;
use serde_json::Value as Json;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;

const COLS: u16 = 80;
const ROWS: u16 = 24;
/// The hybrid number-column width the editor ships with — diagnostic/text screen
/// columns are offset by this much once the gutter is split off (matches
/// `screen.rs`).
const GUTTER: u16 = 4;

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
    // Redirect the LSP log to a temp file (never the real state dir) at DEBUG, so
    // tests are hermetic and the log captures the sync traffic.
    let log = lsp_log_path(tag);
    let _ = std::fs::remove_file(&log);
    std::env::set_var("NXVIM_LSP_LOG_FILE", &log);
    std::env::set_var("NXVIM_LSP_LOG_LEVEL", "debug");
    record
}

/// The temp LSP-log path this test's mock writes to (mirrors `configure_mock`).
fn lsp_log_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nxvim-lsp-log-{}-{tag}.log", std::process::id()))
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

/// Drain buffered notifications, returning the most recent `redraw` params.
fn drain_latest_redraw(incoming: &mut UnboundedReceiver<Incoming>) -> Option<Vec<Value>> {
    let mut latest = None;
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            latest = Some(params);
        }
    }
    latest
}

/// The bottom panel's `(title, lines)` from a redraw, if one is open.
fn panel_of(params: &[Value]) -> Option<(String, Vec<String>)> {
    let Value::Map(map) = params.first()? else {
        return None;
    };
    let Value::Map(panel) = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("panel"))?
        .1
        .clone()
    else {
        return None;
    };
    let get = |key| {
        panel
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let title = get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let lines = get("lines")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some((title, lines))
}

/// Poll (bounded) until a panel is open, returning its `(title, lines)`.
async fn wait_for_panel(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> (String, Vec<String>) {
    for _ in 0..40 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(panel) = drain_latest_redraw(incoming).and_then(|p| panel_of(&p)) {
            return panel;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no panel opened within timeout");
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

/// The redraw map's top-level value for `key`, if present.
fn redraw_get<'a>(params: &'a [Value], key: &str) -> Option<&'a Value> {
    let Value::Map(map) = params.first()? else {
        return None;
    };
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// Decode the `diagnostics` redraw key into per-row `(start, end, severity)`
/// screen-column spans (dropping the trailing style-id, which is `Nil` with no
/// colorscheme loaded).
fn diagnostics_of(params: &[Value]) -> Vec<Vec<(u64, u64, u64)>> {
    redraw_get(params, "diagnostics")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|s| {
                                    let t = s.as_array()?;
                                    Some((t[0].as_u64()?, t[1].as_u64()?, t[2].as_u64()?))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The redraw's message-line text.
fn message_of(params: &[Value]) -> String {
    redraw_get(params, "message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Poll (bounded) until a redraw whose `diagnostics` has at least one non-empty
/// row arrives, returning that redraw's params.
async fn wait_for_diagnostics(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<Value> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if diagnostics_of(&params).iter().any(|row| !row.is_empty()) {
                return params;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("diagnostics never appeared in a redraw");
}

/// Poll (bounded) until a redraw whose message line equals `want` arrives,
/// returning that redraw's params.
async fn wait_for_message(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &str,
) -> Vec<Value> {
    for _ in 0..40 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if message_of(&params) == want {
                return params;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("message line never became {want:?}");
}

/// A `file://` URI for an absolute path string, matching how the editor forms
/// one from a buffer path (the mock returns these in scripted goto/references
/// results, and the server resolves them back to a path).
fn file_uri(path: &str) -> String {
    format!("file://{path}")
}

/// An LSP `Location` (zero-width range at `line:character`) for the mock script's
/// `definition` / `references` / … fields.
fn location(path: &str, line: u32, character: u32) -> Json {
    serde_json::json!({
        "uri": file_uri(path),
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character },
        }
    })
}

/// The current cursor as `(1-based line, 0-based column)`, like neovim.
async fn cursor(rpc: &Rpc) -> (u64, u64) {
    match rpc
        .request("nvim_win_get_cursor", vec![])
        .await
        .expect("cursor")
    {
        Value::Array(a) => (a[0].as_u64().unwrap(), a[1].as_u64().unwrap()),
        other => panic!("unexpected cursor value: {other:?}"),
    }
}

/// Poll (bounded) until the cursor reaches `want`, driving the loop so async LSP
/// replies are processed. Panics with the last position seen if it never does.
async fn wait_for_cursor(rpc: &Rpc, want: (u64, u64)) {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if cursor(rpc).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "cursor never reached {want:?}; last was {:?}",
        cursor(rpc).await
    );
}

/// A single LSP diagnostic as the mock script's `diagnostics` array expects it.
fn diag(line: u32, start: u32, end: u32, severity: u32, message: &str) -> Json {
    serde_json::json!({
        "range": {
            "start": { "line": line, "character": start },
            "end": { "line": line, "character": end },
        },
        "severity": severity,
        "message": message,
    })
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

    // The LSP log got its START banner and (at DEBUG) the outgoing didOpen.
    let log = std::fs::read_to_string(lsp_log_path("init")).unwrap_or_default();
    assert!(
        log.contains("[START]") && log.contains("LSP logging initiated"),
        "the lsp log should carry a START banner, got:\n{log}"
    );
    assert!(
        log.contains("didOpen"),
        "the lsp log should record the outgoing didOpen at DEBUG, got:\n{log}"
    );
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

    // Edit then write: the buffer's write counter advances on a successful :w.
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
async fn undo_back_to_the_saved_state_does_not_fire_did_save() {
    // didSave is a real save hook (the buffer's write counter), not a
    // `modified`-flag heuristic: undoing back to the on-disk content clears
    // `modified` without any `:w`, and must NOT be mistaken for a save. Only a
    // real write does.
    let _guard = test_lock().lock().await;
    let record = configure_mock("nosave", serde_json::json!({}));
    let file = temp_file("nosave", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Edit, then undo straight back to the saved content (modified clears, no :w).
    feed(&rpc, "ohello<Esc>u");
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !has_method(&record_lines(&record), "textDocument/didSave"),
        "undo-to-clean must not fire didSave; saw {:?}",
        record_lines(&record)
    );

    // A genuine write now does fire it — proving the hook works, not just stays quiet.
    feed(&rpc, "ohello<Esc>");
    rpc.request("nvim_command", vec![Value::from("w")])
        .await
        .expect("write");
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didSave")).await;
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

#[tokio::test]
async fn lsp_info_reports_the_running_server() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "info",
        serde_json::json!({ "position_encoding": "utf-8", "sync_kind": "incremental" }),
    );
    let file = temp_file("info", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    // Attach first (didOpen) so the server is initialized and the buffer attached.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    rpc.request("nvim_command", vec![Value::from("LspInfo")])
        .await
        .expect("LspInfo");
    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;

    assert_eq!(title, "LSP info");
    let body = lines.join("\n");
    assert!(body.contains("rust"), "names the rust server:\n{body}");
    assert!(
        body.contains("utf-8"),
        "shows the negotiated encoding:\n{body}"
    );
    assert!(body.contains("incremental"), "shows the sync kind:\n{body}");
    assert!(body.contains("attached"), "the buffer is attached:\n{body}");
}

// ----- Phase 2: diagnostics --------------------------------------------------

#[tokio::test]
async fn diagnostics_are_projected_with_screen_columns() {
    // The headline conversion guard: a leading tab (expands to 8 cells) then a
    // 2-byte `é` (1 cell), with a diagnostic over "diag" — utf-8 chars/bytes
    // 3..7. It must surface on *screen* columns 9..13, proving both byte->screen
    // (`virtcol` over the tab- and wide-aware line) and the LSP char->byte step.
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-cols",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 3, 7, 1, "bad diag")],
        }),
    );
    let file = temp_file("diag-cols", "rs", "\tédiag\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    let params = wait_for_diagnostics(&rpc, &mut incoming).await;
    let rows = diagnostics_of(&params);
    assert_eq!(
        rows[0],
        vec![(9, 13, 1)],
        "the diagnostic spans screen columns 9..13 at severity 1 (error)"
    );
}

#[tokio::test]
async fn the_diagnostic_under_the_cursor_shows_on_the_message_line() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-msg",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "use of bad")],
        }),
    );
    let file = temp_file("diag-msg", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // The cursor opens at column 0 ('l'), off the diagnostic (chars 4..7) — `w`
    // moves it onto "bad", and the message line picks up its text.
    feed(&rpc, "w");
    let params = wait_for_message(&rpc, &mut incoming, "use of bad").await;
    assert_eq!(message_of(&params), "use of bad");

    // Moving off the diagnostic clears the message again (it never went to
    // `:messages`, so nothing lingers).
    feed(&rpc, "$");
    let params = wait_for_message(&rpc, &mut incoming, "").await;
    assert_eq!(message_of(&params), "");
}

#[tokio::test]
async fn lsp_diagnostics_panel_lists_and_jumps() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-panel",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(1, 4, 5, 1, "x is bad")],
        }),
    );
    let file = temp_file("diag-panel", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    rpc.request("nvim_command", vec![Value::from("LspDiagnostics")])
        .await
        .expect("LspDiagnostics");
    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP diagnostics");
    assert_eq!(lines.len(), 1, "one diagnostic, one row: {lines:?}");
    assert!(
        lines[0].contains("2:5"),
        "the row names the 1-based line:col, got {:?}",
        lines[0]
    );
    assert!(
        lines[0].contains("x is bad"),
        "and the message: {:?}",
        lines[0]
    );

    // `<CR>` on the entry closes the panel and jumps to the diagnostic — line 2
    // (1-based), byte column 4.
    feed(&rpc, "<CR>");
    barrier(&rpc).await;
    let cursor = rpc
        .request("nvim_win_get_cursor", vec![])
        .await
        .expect("cursor");
    assert_eq!(
        cursor,
        Value::Array(vec![Value::from(2u64), Value::from(4u64)]),
        "the cursor jumped to the diagnostic's line and column"
    );
}

#[tokio::test]
async fn a_diagnostic_cell_is_painted_with_an_underline() {
    // Tier 2: the real client paint. A diagnostic cell carries the UNDERLINED
    // modifier and the error severity's `sp` underline color, while an adjacent
    // non-diagnostic cell carries neither — proving the span boundaries survive
    // all the way to the rendered grid.
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-paint",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "bad")],
        }),
    );
    let file = temp_file("diag-paint", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    let params = wait_for_diagnostics(&rpc, &mut incoming).await;
    let buf = paint(&View::from_redraw(&params), COLS, ROWS);

    // "bad" sits at byte/screen columns 4..7 on line 0; the painted cells are
    // offset by the number-column gutter.
    let on = GUTTER + 4; // first cell of "bad"
    let off = GUTTER + 7; // the space just after "bad"
    assert_eq!(buf.cell((on, 0)).unwrap().symbol(), "b");
    assert!(
        buf.cell((on, 0))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED),
        "the diagnostic cell is underlined"
    );
    assert_eq!(
        buf.cell((on, 0)).unwrap().style().underline_color,
        Some(Color::Red),
        "with the error severity's built-in underline color"
    );
    assert!(
        !buf.cell((off, 0))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::UNDERLINED),
        "the cell just past the diagnostic is not underlined"
    );
}

// ----- Phase 3: go-to definition & references --------------------------------

#[tokio::test]
async fn gd_jumps_to_a_definition_in_the_same_file() {
    let _guard = test_lock().lock().await;
    // `target` is defined on line 0; the call site is on line 1. `gd` from the
    // call site jumps to the definition's (line, col).
    let file = temp_file("gd-same", "rs", "fn target() {}\nfn main() { target() }\n");
    let record = configure_mock(
        "gd-same",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Move to the call site (line 1) and request go-to-definition via the `gd`
    // built-in keymap.
    feed(&rpc, "jgd");
    // The reply lands the cursor at the definition: 1-based line 1, byte col 3.
    wait_for_cursor(&rpc, (1, 3)).await;

    // The keymap actually issued the LSP request (not swallowed as editor motion).
    assert!(
        has_method(&record_lines(&record), "textDocument/definition"),
        "gd should send a textDocument/definition request"
    );
}

#[tokio::test]
async fn gd_switches_buffers_for_a_cross_file_definition() {
    let _guard = test_lock().lock().await;
    // The definition lives in a *different* file, on a line with a 2-byte `é`
    // before the target column — and the server negotiated utf-16. So the jump
    // must (a) open/switch to the file, then (b) read its just-loaded line to
    // convert the utf-16 character into a byte column. The `=` is utf-16 unit 9
    // ("let café " is 9 units, é counting as one) but byte 10 (é is two bytes):
    // landing on byte col 10 proves the cross-file char→byte conversion.
    let other = temp_file("gd-other", "rs", "let café = 1\n");
    let main = temp_file("gd-main", "rs", "fn main() {}\n");
    let record = configure_mock(
        "gd-cross",
        serde_json::json!({
            "position_encoding": "utf-16",
            "definition": location(&other, 0, 9),
        }),
    );
    let (rpc, _incoming) = start(Some(main)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gd");
    // Switched to the other file, cursor on the `=`: 1-based line 1, byte col 10.
    wait_for_cursor(&rpc, (1, 10)).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["let café = 1".to_string()],
        "the jump switched to the definition's buffer"
    );
}

#[tokio::test]
async fn gr_lists_references_in_the_panel_and_jumps() {
    let _guard = test_lock().lock().await;
    // Two references to `x`, on lines 1 and 2 (col 8 in each). `gr` lists them in
    // a select panel; `<CR>` on the first jumps to it.
    let file = temp_file("gr", "rs", "let x = 1\nlet y = x\nlet z = x\n");
    let record = configure_mock(
        "gr",
        serde_json::json!({ "references": [location(&file, 1, 8), location(&file, 2, 8)] }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gr");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references");
    assert_eq!(
        panel_lines.len(),
        2,
        "one row per reference: {panel_lines:?}"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/references"),
        "gr should send a textDocument/references request"
    );

    // `<CR>` on the first row jumps to it: 1-based line 2, byte col 8.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 8)).await;
}

#[tokio::test]
async fn panelopen_reopens_the_references_panel_and_still_jumps() {
    let _guard = test_lock().lock().await;
    // Regression: navigating from the references list with `<CR>` closes the
    // panel; reopening it with `:panelopen` must keep its jump targets, so a
    // second `<CR>` still navigates (previously the targets were lost on the
    // first jump and the reopened list was inert).
    let file = temp_file("gr-reopen", "rs", "let x = 1\nlet y = x\nlet z = x\n");
    let record = configure_mock(
        "gr-reopen",
        serde_json::json!({ "references": [location(&file, 1, 8), location(&file, 2, 8)] }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gr");
    let (title, _lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references");

    // First navigation: `<CR>` on row 0 jumps to reference 1 (line 2, col 8) and
    // closes the panel.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 8)).await;

    // Reopen the dismissed list and navigate again — to a *different* row, so the
    // jump is observable: row 1 is reference 2 (line 3, col 8).
    rpc.request("nvim_command", vec![Value::from("panelopen")])
        .await
        .expect("panelopen");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references", "the references panel came back");
    assert_eq!(panel_lines.len(), 2, "with its content: {panel_lines:?}");

    feed(&rpc, "j<CR>");
    wait_for_cursor(&rpc, (3, 8)).await;
}

#[tokio::test]
async fn an_empty_definition_reply_reports_no_definition() {
    let _guard = test_lock().lock().await;
    // The server returns nothing for `gd`: a brief message, no jump, no panel.
    let file = temp_file("gd-empty", "rs", "fn main() {}\n");
    let record = configure_mock("gd-empty", serde_json::json!({}));
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gd");
    let params = wait_for_message(&rpc, &mut incoming, "No definition found").await;
    assert_eq!(message_of(&params), "No definition found");
    // The cursor never left the origin.
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn a_definition_reply_is_dropped_if_the_cursor_moved() {
    // Stale-reply drop (Decision 3): fire `gd`, move the cursor before the async
    // reply lands, and the jump must be discarded — the move wins, not the
    // now-irrelevant definition. `gdj` does exactly this in one input batch: the
    // request is issued at (0,0), then `j` moves to (1,0) before the reply (which
    // the select! loop only processes after the batch) is handled.
    let _guard = test_lock().lock().await;
    let file = temp_file(
        "gd-stale",
        "rs",
        "fn target() {}\nfn main() {}\nlet z = 1\n",
    );
    let record = configure_mock(
        "gd-stale",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gdj");
    // Give the (now-stale) reply ample time to arrive and be dropped.
    for _ in 0..8 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "the cursor stayed where `j` moved it; the stale definition reply did not jump"
    );
    // The request was genuinely sent (so we really exercised the drop, not a
    // never-issued request).
    assert!(has_method(
        &record_lines(&record),
        "textDocument/definition"
    ));
}

#[tokio::test]
async fn k_shows_hover_docs_in_the_panel() {
    let _guard = test_lock().lock().await;
    // The mock returns markdown hover contents; `K` opens the panel with the
    // markup rendered as plain lines (the trailing blank line is trimmed).
    let file = temp_file("hover", "rs", "fn target() {}\n");
    let record = configure_mock(
        "hover",
        serde_json::json!({
            "hover": {
                "contents": {
                    "kind": "markdown",
                    "value": "fn target()\n\nThe target function\n",
                }
            }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "K");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP hover");
    assert_eq!(
        panel_lines,
        vec![
            "fn target()".to_string(),
            String::new(),
            "The target function".to_string(),
        ],
        "the hover markup is rendered as plain lines, trailing blank trimmed"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/hover"),
        "K should send a textDocument/hover request"
    );
}

#[tokio::test]
async fn a_long_hover_line_wraps_in_the_panel() {
    let _guard = test_lock().lock().await;
    // A hover line longer than the panel width must wrap across rows, not clip.
    // The panel spans the full terminal width (COLS), so a 100-char unbroken run
    // hard-breaks into an 80-cell row and a 20-cell row.
    let file = temp_file("hover-wrap", "rs", "fn main() {}\n");
    let long = "a".repeat(100);
    let record = configure_mock(
        "hover-wrap",
        serde_json::json!({
            "hover": { "contents": { "kind": "markdown", "value": long } }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "K");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP hover");
    assert_eq!(
        panel_lines,
        vec!["a".repeat(COLS as usize), "a".repeat(100 - COLS as usize)],
        "the long line wrapped to the panel width instead of being clipped"
    );
}

#[tokio::test]
async fn an_empty_hover_reply_reports_no_information() {
    let _guard = test_lock().lock().await;
    // The server has nothing to say at the cursor: a brief message, no panel.
    let file = temp_file("hover-empty", "rs", "fn main() {}\n");
    let record = configure_mock("hover-empty", serde_json::json!({}));
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "K");
    let params = wait_for_message(&rpc, &mut incoming, "No hover information").await;
    assert_eq!(message_of(&params), "No hover information");
    assert!(panel_of(&params).is_none(), "an empty hover opens no panel");
}

#[tokio::test]
async fn ctrl_k_shows_signature_help_with_the_active_parameter() {
    let _guard = test_lock().lock().await;
    // The mock returns a two-parameter signature with the second parameter active;
    // `<C-k>` in insert mode renders the active signature on the message line with
    // its active parameter highlighted in brackets.
    let file = temp_file("sighelp", "rs", "fn add(a: i32, b: i32) -> i32 { a }\n");
    let record = configure_mock(
        "sighelp",
        serde_json::json!({
            "signature_help": {
                "signatures": [
                    {
                        "label": "fn add(a: i32, b: i32) -> i32",
                        "parameters": [ { "label": "a: i32" }, { "label": "b: i32" } ],
                    }
                ],
                "activeSignature": 0,
                "activeParameter": 1,
            }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Enter insert mode, then trigger signature help with `<C-k>` (which must not
    // insert a literal `k`).
    feed(&rpc, "i<C-k>");
    let params = wait_for_message(
        &rpc,
        &mut incoming,
        "fn add(a: i32, b: i32) -> i32    [b: i32]",
    )
    .await;
    assert_eq!(
        message_of(&params),
        "fn add(a: i32, b: i32) -> i32    [b: i32]"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/signatureHelp"),
        "<C-k> should send a textDocument/signatureHelp request"
    );

    // The buffer is unchanged: `<C-k>` was consumed as a mapping, not typed.
    assert_eq!(
        lines(&rpc).await,
        vec!["fn add(a: i32, b: i32) -> i32 { a }".to_string()],
        "<C-k> did not insert a literal k"
    );
}

// ----- Phase 5: completion (the popup menu) ----------------------------------

/// The `pmenu` redraw key as `(labels, selected)`, or `None` when no popup is
/// open (the key is `Nil`). `selected` is `-1` until the user navigates.
fn pmenu_of(params: &[Value]) -> Option<(Vec<String>, i64)> {
    let Value::Map(map) = params.first()? else {
        return None;
    };
    let pmenu = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("pmenu"))?
        .1
        .clone();
    let Value::Map(pm) = pmenu else {
        return None; // Nil ⇒ no popup
    };
    let get = |key: &str| {
        pm.iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v)
    };
    let labels = get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|it| it.as_array()?.first()?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let selected = get("selected").and_then(Value::as_i64).unwrap_or(-1);
    Some((labels, selected))
}

/// Drain every buffered `redraw` (oldest first), returning their params — so a
/// test can assert the popup never closed *between* keystrokes, not just at the
/// end.
fn drain_all_redraws(incoming: &mut UnboundedReceiver<Incoming>) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    while let Ok(Incoming::Notification { method, params }) = incoming.try_recv() {
        if method == "redraw" {
            out.push(params);
        }
    }
    out
}

/// Poll until a redraw whose `pmenu` satisfies `pred` arrives, returning it.
async fn wait_for_pmenu_where(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    pred: impl Fn(&(Vec<String>, i64)) -> bool,
) -> Vec<Value> {
    for _ in 0..60 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if pmenu_of(&params).as_ref().is_some_and(&pred) {
                return params;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the completion popup never reached the expected state");
}

/// Poll until the completion popup is open, returning that redraw's params.
async fn wait_for_pmenu(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) -> Vec<Value> {
    wait_for_pmenu_where(rpc, incoming, |_| true).await
}

/// Poll until the popup's item labels equal `want`, asserting it stays open the
/// whole time (no drained redraw shows `pmenu: Nil`) — i.e. it refreshes in
/// place rather than closing and reopening.
async fn wait_for_pmenu_items(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    want: &[&str],
) {
    for _ in 0..60 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        for params in drain_all_redraws(incoming) {
            let Some((labels, _)) = pmenu_of(&params) else {
                panic!("the completion popup closed during a live refresh");
            };
            if labels.iter().map(String::as_str).eq(want.iter().copied()) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the popup items never became {want:?}");
}

/// Poll until a redraw shows the popup closed (`pmenu: Nil`).
async fn wait_for_pmenu_closed(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) {
    for _ in 0..40 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if pmenu_of(&params).is_none() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the completion popup never closed");
}

/// How many recorded messages carried `method`.
fn count_method(recs: &[Json], method: &str) -> usize {
    recs.iter().filter(|r| r["method"] == method).count()
}

/// A bare `CompletionItem` (just a label).
fn citem(label: &str) -> Json {
    serde_json::json!({ "label": label })
}

/// A `CompletionItem[]` response (a `CompletionResponse::Array`, always complete).
fn completion_items(labels: &[&str]) -> Json {
    serde_json::json!(labels.iter().map(|l| citem(l)).collect::<Vec<_>>())
}

/// A `CompletionList` response with the given `isIncomplete` flag.
fn completion_list(incomplete: bool, labels: &[&str]) -> Json {
    serde_json::json!({
        "isIncomplete": incomplete,
        "items": labels.iter().map(|l| citem(l)).collect::<Vec<_>>(),
    })
}

#[tokio::test]
async fn completion_orders_by_importance_and_filters_the_prefix() {
    let _guard = test_lock().lock().await;
    // The headline: with `use nv` typed, the menu shows the items matching `nv`
    // (`nva`, `nvb`) — ahead of and to the exclusion of `self`/`pub` — even though
    // the server returned them in a deliberately unhelpful order. A complete list
    // means the narrowing is a client-side refilter: exactly one request is sent.
    let record = configure_mock(
        "compl-order",
        serde_json::json!({ "completion": completion_items(&["pub", "self", "nvb", "nva"]) }),
    );
    let file = temp_file("compl-order", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, type `use `, trigger: the popup opens with everything (empty
    // prefix), ordered by the server priority (here, the label).
    feed(&rpc, "ouse <C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert_eq!(
        pmenu_of(&params).unwrap().0,
        vec!["nva", "nvb", "pub", "self"],
        "an empty prefix shows every candidate"
    );

    // Type `n` then `v`: the menu stays open and narrows in place to the `nv`
    // matches, in importance order — `self`/`pub` gone.
    feed(&rpc, "n");
    wait_for_pmenu_items(&rpc, &mut incoming, &["nva", "nvb"]).await;
    feed(&rpc, "v");
    wait_for_pmenu_items(&rpc, &mut incoming, &["nva", "nvb"]).await;

    // A complete list ⇒ the narrowing filtered the cache; no extra request fired.
    assert_eq!(
        count_method(&record_lines(&record), "textDocument/completion"),
        1,
        "a complete list is filtered client-side, not re-requested"
    );
}

#[tokio::test]
async fn completion_ranking_honors_sort_text_over_the_label() {
    let _guard = test_lock().lock().await;
    // Two prefix matches whose `sortText` order reverses their alphabetical order
    // (`config` sorts after `connect`), plus a subsequence-only item. Importance
    // wins: `sortText` orders the prefix matches, and the subsequence ranks below.
    let record = configure_mock(
        "compl-sort",
        serde_json::json!({
            "completion": [
                { "label": "config", "sortText": "2" },
                { "label": "connect", "sortText": "1" },
                { "label": "disconnect" },
            ]
        }),
    );
    let file = temp_file("compl-sort", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "ocon<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    // `connect` (sortText 1) before `config` (sortText 2); `disconnect` (only a
    // subsequence of `con`) last.
    wait_for_pmenu_items(&rpc, &mut incoming, &["connect", "config", "disconnect"]).await;
}

#[tokio::test]
async fn an_incomplete_list_re_requests_and_the_menu_stays_open() {
    let _guard = test_lock().lock().await;
    // `isIncomplete:true` ⇒ each narrowing keystroke fires a fresh request whose
    // result replaces the list, rather than filtering the cache. The popup stays
    // open across the round-trip (never goes `Nil`).
    let record = configure_mock(
        "compl-live",
        serde_json::json!({
            "completion_sequence": [
                completion_list(true, &["nano", "never", "nvidia", "nvim"]),
                completion_list(true, &["nvidia", "nvim"]),
            ]
        }),
    );
    let file = temp_file("compl-live", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Type `n`, trigger → the broad list (filtered to the `n` matches).
    feed(&rpc, "on<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    wait_for_pmenu_items(&rpc, &mut incoming, &["nano", "never", "nvidia", "nvim"]).await;
    assert_eq!(
        count_method(&record_lines(&record), "textDocument/completion"),
        1
    );

    // Type `v`: a *second* request lands the narrowed list, and the menu stayed
    // open throughout (the helper fails if any redraw showed it closed).
    feed(&rpc, "v");
    wait_for_pmenu_items(&rpc, &mut incoming, &["nvidia", "nvim"]).await;
    let recs = wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/completion") >= 2
    })
    .await;
    assert_eq!(
        count_method(&recs, "textDocument/completion"),
        2,
        "an incomplete list re-requests on the narrowing keystroke"
    );
}

#[tokio::test]
async fn accepting_a_completion_inserts_the_item_and_additional_edits() {
    let _guard = test_lock().lock().await;
    // Accept replaces the typed word with the item (not appends) and applies its
    // `additionalTextEdits` (an inserted `use` line) — all one undo step.
    let record = configure_mock(
        "compl-accept",
        serde_json::json!({
            "completion": [{
                "label": "println",
                "insertText": "println",
                "additionalTextEdits": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 0 },
                    },
                    "newText": "use std::io;\n",
                }],
            }]
        }),
    );
    let file = temp_file("compl-accept", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, type the prefix `pr`, trigger, select the item, accept.
    feed(&rpc, "opr<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n><CR>");
    barrier(&rpc).await;

    // The word became `println` (replaced, not `prprintln`), and the import line
    // was inserted at the top.
    assert_eq!(
        lines(&rpc).await,
        vec![
            "use std::io;".to_string(),
            "fn main() {}".to_string(),
            "println".to_string(),
        ],
        "accept replaced the prefix and applied the additional edit"
    );
    wait_for_pmenu_closed(&rpc, &mut incoming).await;

    // A single undo restores both the insertion and the import.
    feed(&rpc, "<Esc>u");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string()],
        "one undo reverts the whole accept"
    );
}

#[tokio::test]
async fn navigating_and_dismissing_the_completion_menu() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "compl-nav",
        serde_json::json!({ "completion": completion_items(&["alpha", "beta"]) }),
    );
    let file = temp_file("compl-nav", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, trigger: both items, nothing selected yet.
    feed(&rpc, "o<C-x><C-o>");
    let params = wait_for_pmenu(&rpc, &mut incoming).await;
    assert_eq!(pmenu_of(&params).unwrap().0, vec!["alpha", "beta"]);
    assert_eq!(pmenu_of(&params).unwrap().1, -1, "nothing selected yet");

    // `<C-n>` highlights the first item.
    feed(&rpc, "<C-n>");
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == 0).await;
    assert_eq!(pmenu_of(&params).unwrap().1, 0);

    // `<C-e>` dismisses without inserting; the buffer keeps only the empty line
    // `o` opened (no stray `e` from the control key).
    feed(&rpc, "<C-e>");
    wait_for_pmenu_closed(&rpc, &mut incoming).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), String::new()],
        "<C-e> inserted nothing"
    );

    // Re-open, then `<Esc>` dismisses the menu AND leaves insert mode, still
    // inserting no literal character.
    feed(&rpc, "<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<Esc>");
    wait_for_pmenu_closed(&rpc, &mut incoming).await;
    let mode = match rpc.request("nvim_get_mode", vec![]).await.unwrap() {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some("mode"))
            .and_then(|(_, v)| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    };
    assert_eq!(mode, "n", "<Esc> returned to normal mode");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), String::new()],
        "<Esc> inserted nothing"
    );
}

#[tokio::test]
async fn the_completion_popup_paints_as_a_bordered_overlay() {
    let _guard = test_lock().lock().await;
    // Tier 2: the real client paint. The popup is a bordered box anchored one row
    // below the cursor at the word-start column (past the gutter); the selected
    // row is reverse-highlighted, and its cells belong to the menu, not the text.
    let record = configure_mock(
        "compl-paint",
        serde_json::json!({ "completion": completion_items(&["alpha", "beta"]) }),
    );
    let file = temp_file("compl-paint", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open an empty line (cursor at column 0), trigger, select the first item.
    feed(&rpc, "o<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n>");
    let params = wait_for_pmenu_where(&rpc, &mut incoming, |(_, sel)| *sel == 0).await;
    let buf = paint(&View::from_redraw(&params), COLS, ROWS);

    // The box's top-left border sits at the word start: gutter (4) + col 0, one
    // row below the cursor (cursor row 1 ⇒ border row 2).
    assert_eq!(
        buf.cell((GUTTER, 2)).unwrap().symbol(),
        "┌",
        "the popup is a bordered box anchored under the word"
    );
    // Inside the border: the selected item `alpha` (reversed), then `beta`.
    let item_col = GUTTER + 1;
    assert_eq!(buf.cell((item_col, 3)).unwrap().symbol(), "a");
    assert!(
        buf.cell((item_col, 3))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "the selected row is reverse-highlighted"
    );
    assert_eq!(buf.cell((item_col, 4)).unwrap().symbol(), "b");
    assert!(
        !buf.cell((item_col, 4))
            .unwrap()
            .style()
            .add_modifier
            .contains(Modifier::REVERSED),
        "an unselected row is not highlighted"
    );
}

#[tokio::test]
async fn accepting_a_utf16_text_edit_lands_at_the_right_byte() {
    let _guard = test_lock().lock().await;
    // The completion analogue of the cross-file `é` test: a line with a leading
    // 2-byte `é` and a utf-16 server. The item's `textEdit` range is in utf-16
    // units (char 1..2 = the `x` after `é`); accepting must convert it to byte
    // 2..3, so `x` → `xyz` lands as `éxyz`, not corrupting the `é`.
    let record = configure_mock(
        "compl-utf16",
        serde_json::json!({
            "position_encoding": "utf-16",
            "completion": [{
                "label": "xyz",
                "textEdit": {
                    "range": {
                        "start": { "line": 0, "character": 1 },
                        "end": { "line": 0, "character": 2 },
                    },
                    "newText": "xyz",
                },
            }],
        }),
    );
    let file = temp_file("compl-utf16", "rs", "éx\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Append (insert at end of line, after `x`), trigger, select, accept.
    feed(&rpc, "A<C-x><C-o>");
    wait_for_pmenu(&rpc, &mut incoming).await;
    feed(&rpc, "<C-n><CR>");
    barrier(&rpc).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["éxyz".to_string()],
        "the utf-16 edit range converted to the right byte offset (after é)"
    );
}

#[tokio::test]
async fn completion_never_blocks_the_editor() {
    let _guard = test_lock().lock().await;
    // Resilience: a trigger whose server offers nothing (a null reply) opens no
    // menu and the editor keeps editing — text typed right after the trigger
    // lands normally and completion inserts nothing.
    let record = configure_mock("compl-resil", serde_json::json!({}));
    let file = temp_file("compl-resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Open a line, type `foo`, trigger (null reply ⇒ no menu), keep typing `bar`.
    feed(&rpc, "ofoo<C-x><C-o>bar<Esc>");
    // The request was genuinely sent (so the resilience path was exercised), and
    // its null reply opened no menu.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/completion")).await;
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), "foobar".to_string()],
        "the editor stays fully editable; completion inserted nothing"
    );
}
