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

/// A temp config dir whose `init.lua` registers the mock language server through
/// the Phase 7a Lua start path — `vim.lsp.config('mock', …)` + `vim.lsp.enable` —
/// for the filetypes the tests open. There is no built-in auto-spawn anymore, so
/// every LSP test starts its server this way. The config's `cmd` is a placeholder:
/// the real command is injected per-test via `$NXVIM_LSP_CMD` (see
/// `configure_mock`), which the server honors over the config's `cmd`.
fn lsp_config_dir() -> PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("nxvim-lsp-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create lsp config dir");
        std::fs::write(
            dir.join("init.lua"),
            "vim.lsp.config('mock', { cmd = { 'mock' }, \
             filetypes = { 'rust', 'go', 'python', 'lua' } })\n\
             vim.lsp.enable('mock')\n",
        )
        .expect("write init.lua");
        dir
    })
    .clone()
}

/// Start a server (LSP via the injected `init.lua`, syntax enabled) editing
/// `file`, attach a UI. Mirrors the syntax tests' harness.
async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with_config_dir(file, lsp_config_dir()).await
}

/// Like [`start`], but with an explicit `config_dir` — for tests that need a
/// `vim.lsp.config`/`enable` arrangement other than the shared one (e.g. enabling
/// the server *interactively, after* the buffer is already open).
async fn start_with_config_dir(
    file: Option<String>,
    config_dir: PathBuf,
) -> (Rpc, UnboundedReceiver<Incoming>) {
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
                config_dir: Some(config_dir),
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

/// True when a redraw carries at least one treesitter highlight span — i.e. the
/// syntax worker has replied, so it is now caught up (non-pending) and will drain
/// the buffer's edit journal before the LSP sync on the next edit.
fn has_highlights(params: &[Value]) -> bool {
    redraw_get(params, "highlights")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.as_array().is_some_and(|spans| !spans.is_empty()))
        })
}

/// Poll until a redraw shows syntax highlights (the worker has caught up).
async fn wait_for_highlights(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if has_highlights(&params) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("syntax highlights never appeared (the worker never replied)");
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
async fn did_change_reaches_the_server_after_the_syntax_worker_drains() {
    // Regression: the syntax worker and the LSP client each drain the buffer's
    // edit journal, and the syntax sync runs first. Once the worker has caught up
    // (not mid-parse), it consumed the edits before the LSP sync could — leaving
    // the language server's document frozen at `didOpen` (every `didChange`
    // carried 0 changes), so completion and friends ran against stale text. With
    // independent journals, an edit must still reach the server after syntax has
    // drained. This deterministically reproduces it by settling syntax first.
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "sync-share",
        serde_json::json!({ "sync_kind": "incremental" }),
    );
    let file = temp_file("sync-share", "rs", "fn main() {}\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Let the syntax worker reply (highlights appear), so it is now non-pending
    // and drains the journal before the LSP sync on the next edit.
    wait_for_highlights(&rpc, &mut incoming).await;

    // Type a distinctive change; the server must still receive it as a real
    // content change (before the fix this `didChange` was empty and `ZZZ` never
    // arrived).
    feed(&rpc, "oZZZ<Esc>");
    // `did_change_text` concatenates every `didChange`'s content-change texts;
    // the inserted `ZZZ` shows up there only if the server actually received the
    // change (before the fix, the journal was drained by syntax, so this batch's
    // `didChange` was empty and `ZZZ` never arrived — this would time out).
    let recs = wait_for_record(&rpc, &record, |r| did_change_text(r).contains("ZZZ")).await;
    assert!(
        did_change_text(&recs).contains("ZZZ"),
        "the language server received the edit after syntax drained the journal"
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
    assert!(body.contains("mock"), "names the mock server:\n{body}");
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

// ----- Phase 6: formatting / rename / code actions --------------------------

/// An LSP `TextEdit` (`{range, newText}`) for the mock's `formatting`/`rename`/
/// `code_action` script fields. Positions are in the server's negotiated encoding.
fn text_edit(sl: u32, sc: u32, el: u32, ec: u32, new: &str) -> Json {
    serde_json::json!({
        "range": {
            "start": { "line": sl, "character": sc },
            "end": { "line": el, "character": ec },
        },
        "newText": new,
    })
}

/// A `WorkspaceEdit` with a `changes` map built from `(path, TextEdit[])` pairs
/// (used directly as the `rename` reply, or nested as a code action's `edit`).
fn ws_changes(entries: &[(&str, Vec<Json>)]) -> Json {
    let mut changes = serde_json::Map::new();
    for (path, edits) in entries {
        changes.insert(file_uri(path), Json::Array(edits.clone()));
    }
    let mut edit = serde_json::Map::new();
    edit.insert("changes".to_string(), Json::Object(changes));
    Json::Object(edit)
}

/// Run an ex-command over RPC (awaiting the dispatch, not the async LSP reply).
async fn cmd(rpc: &Rpc, command: &str) {
    rpc.request("nvim_command", vec![Value::from(command)])
        .await
        .expect("command");
}

/// Poll (bounded) until the current buffer's lines equal `want`.
async fn wait_for_lines(rpc: &Rpc, want: &[&str]) {
    for _ in 0..80 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if lines(rpc)
            .await
            .iter()
            .map(String::as_str)
            .eq(want.iter().copied())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("buffer never became {want:?}; saw {:?}", lines(rpc).await);
}

/// The current buffer id.
async fn current_buf(rpc: &Rpc) -> u64 {
    rpc.request("nvim_get_current_buf", vec![])
        .await
        .unwrap()
        .as_u64()
        .unwrap()
}

/// Make buffer `id` current (the `nvim_set_current_buf` entry point).
async fn set_buf(rpc: &Rpc, id: u64) {
    rpc.request("nvim_set_current_buf", vec![Value::from(id)])
        .await
        .unwrap();
}

/// Editable lines of buffer `id` (read by handle, without switching to it).
async fn lines_of_buf(rpc: &Rpc, id: u64) -> Vec<String> {
    let result = rpc
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(id),
                Value::from(0i64),
                Value::from(-1i64),
                Value::Boolean(false),
            ],
        )
        .await
        .unwrap();
    match result {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn lsp_format_rewrites_the_buffer_and_is_idempotent() {
    let _guard = test_lock().lock().await;
    // The mock returns a whole-line replacement; `:LspFormat` rewrites the buffer.
    // The edit replaces line 0 incl. its newline ((0,0)..(1,0)) with the canonical
    // text, so a re-run on the already-formatted line is a no-op (idempotent).
    let record = configure_mock(
        "fmt",
        serde_json::json!({ "formatting": [text_edit(0, 0, 1, 0, "let x = 1;\n")] }),
    );
    let file = temp_file("fmt", "rs", "let x=1;\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    wait_for_lines(&rpc, &["let x = 1;"]).await;

    // Re-format: the line is already canonical, so it stays unchanged.
    cmd(&rpc, "LspFormat").await;
    for _ in 0..6 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        lines(&rpc).await,
        vec!["let x = 1;".to_string()],
        "re-formatting already-formatted text is a no-op"
    );
}

#[tokio::test]
async fn a_formatting_reply_is_dropped_after_an_intervening_edit() {
    let _guard = test_lock().lock().await;
    // Content-version guard: the formatting reply is delayed; an edit lands before
    // it does, so applying the (now stale, whole-document) edit would corrupt the
    // buffer. The reply must be dropped, leaving the user's edit intact.
    let record = configure_mock(
        "fmt-stale",
        serde_json::json!({
            "formatting": [text_edit(0, 0, 1, 0, "FORMATTED\n")],
            "reply_delay_ms": 200,
        }),
    );
    let file = temp_file("fmt-stale", "rs", "original\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // Fire format (the mock sleeps before replying), then edit before it lands.
    cmd(&rpc, "LspFormat").await;
    feed(&rpc, "A!<Esc>");
    barrier(&rpc).await;
    // The request really went out (so the drop path is exercised, not a no-op),
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/formatting")).await;
    // and after the reply delay elapses the stale reply has been dropped.
    for _ in 0..10 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        lines(&rpc).await,
        vec!["original!".to_string()],
        "the late formatting reply was dropped; the user's edit stands"
    );
}

#[tokio::test]
async fn rename_applies_a_workspace_edit_across_open_buffers() {
    let _guard = test_lock().lock().await;
    // Rename returns a two-file WorkspaceEdit; both open buffers change, each is
    // independently undoable, and the active buffer's cursor survives.
    let file_a = temp_file("rename-a", "rs", "foo = foo\n");
    let file_b = temp_file("rename-b", "rs", "use foo\n");
    let record = configure_mock(
        "rename",
        serde_json::json!({
            "rename": ws_changes(&[
                (&file_a, vec![text_edit(0, 0, 0, 3, "xyz"), text_edit(0, 6, 0, 9, "xyz")]),
                (&file_b, vec![text_edit(0, 4, 0, 7, "xyz")]),
            ])
        }),
    );
    let (rpc, _incoming) = start(Some(file_a)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    let buf_a = current_buf(&rpc).await;

    // Open B (so the rename can reach it), wait for its didOpen, then back to A.
    cmd(&rpc, &format!("e {file_b}")).await;
    wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/didOpen") >= 2
    })
    .await;
    let buf_b = current_buf(&rpc).await;
    set_buf(&rpc, buf_a).await;

    // Cursor at the start of A; rename foo → xyz.
    feed(&rpc, "gg0");
    cmd(&rpc, "LspRename xyz").await;
    wait_for_lines(&rpc, &["xyz = xyz"]).await;

    // The other open buffer changed too (read by handle, no switch).
    assert_eq!(
        lines_of_buf(&rpc, buf_b).await,
        vec!["use xyz".to_string()],
        "the rename reached the other open buffer"
    );
    // The active buffer's cursor survived at a valid resting cell.
    assert_eq!(cursor(&rpc).await, (1, 0), "the active cursor survived");

    // Undo on A reverts only A; B is untouched (independent undo histories).
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["foo = foo".to_string()]);
    assert_eq!(
        lines_of_buf(&rpc, buf_b).await,
        vec!["use xyz".to_string()],
        "B is unaffected by A's undo"
    );
    // Switch to B and undo there.
    set_buf(&rpc, buf_b).await;
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["use foo".to_string()]);
}

#[tokio::test]
async fn a_code_action_lists_in_the_panel_and_applies_on_enter() {
    let _guard = test_lock().lock().await;
    // The code-action list opens in the panel; `<CR>` on a row applies that
    // action's eager edit (and no control key leaks a literal character).
    let file = temp_file("ca", "rs", "let x=1;\n");
    let edit = ws_changes(&[(&file, vec![text_edit(0, 0, 1, 0, "let x = 1;\n")])]);
    let record = configure_mock(
        "ca",
        serde_json::json!({
            "code_action": [{ "title": "Add spaces", "edit": edit }]
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspCodeAction").await;
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP code actions");
    assert_eq!(panel_lines, vec!["Add spaces".to_string()]);

    // `<CR>` applies the chosen action and closes the panel.
    feed(&rpc, "<CR>");
    wait_for_lines(&rpc, &["let x = 1;"]).await;
    assert!(
        !rpc.request("nxvim_panel_is_open", vec![])
            .await
            .unwrap()
            .as_bool()
            .unwrap(),
        "the code-action panel closed after applying"
    );
}

#[tokio::test]
async fn a_lazy_code_action_is_resolved_before_applying() {
    let _guard = test_lock().lock().await;
    // A lazy action arrives with no `edit` (only `data`); selecting it fires
    // `codeAction/resolve`, and the resolved edit (returned with the action) is
    // what gets applied.
    let file = temp_file("ca-resolve", "rs", "let x=1;\n");
    let resolved = serde_json::json!({
        "title": "Add spaces",
        "edit": ws_changes(&[(&file, vec![text_edit(0, 0, 1, 0, "let x = 1;\n")])]),
    });
    let record = configure_mock(
        "ca-resolve",
        serde_json::json!({
            // No `edit` here — only `data`, so the client must resolve it.
            "code_action": [{ "title": "Add spaces", "data": { "id": 1 } }],
            "code_action_resolve": resolved,
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspCodeAction").await;
    let (_title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(panel_lines, vec!["Add spaces".to_string()]);

    // `<CR>` resolves then applies; the resolve round-trip really happened.
    feed(&rpc, "<CR>");
    wait_for_lines(&rpc, &["let x = 1;"]).await;
    assert!(
        has_method(&record_lines(&record), "codeAction/resolve"),
        "the lazy action was resolved before applying"
    );
}

#[tokio::test]
async fn a_formatting_edit_lands_at_the_right_byte_with_utf16() {
    let _guard = test_lock().lock().await;
    // The edit analogue of the cross-file `é` test: a leading 2-byte `é` and a
    // utf-16 server. The edit's range is in utf-16 units (char 1..2 = the `x`
    // after `é`); applying it must convert to byte 2..3, so `x` → `X` lands as
    // `éX=1`, not corrupting the `é`.
    let record = configure_mock(
        "fmt-utf16",
        serde_json::json!({
            "position_encoding": "utf-16",
            "formatting": [text_edit(0, 1, 0, 2, "X")],
        }),
    );
    let file = temp_file("fmt-utf16", "rs", "éx=1\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    wait_for_lines(&rpc, &["éX=1"]).await;
}

#[tokio::test]
async fn format_and_rename_never_block_the_editor() {
    let _guard = test_lock().lock().await;
    // Resilience: format/rename requests whose server offers nothing (null replies)
    // leave the editor fully editable and the buffer unchanged.
    let record = configure_mock("edit-resil", serde_json::json!({}));
    let file = temp_file("edit-resil", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(&rpc, "LspFormat").await;
    cmd(&rpc, "LspRename bar").await;
    // Both requests were genuinely sent; their null replies applied nothing.
    wait_for_record(&rpc, &record, |r| {
        has_method(r, "textDocument/formatting") && has_method(r, "textDocument/rename")
    })
    .await;
    // The editor still edits.
    feed(&rpc, "ook<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn main() {}".to_string(), "ok".to_string()],
        "the editor stays fully editable; the null edit replies changed nothing"
    );
}

/// Restores an env var to its prior value on drop, so a test that sets a
/// process-global env var leaves it as it found it even on a panic.
struct EnvGuard(&'static str, Option<std::ffi::OsString>);
impl EnvGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvGuard(key, prev)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

#[tokio::test]
async fn the_workspace_root_is_configurable_via_env() {
    let _guard = test_lock().lock().await;
    // `$NXVIM_LSP_ROOT` overrides the workspace root the client sends as `rootUri`
    // (and uses as the server's working dir) — the knob for pointing the editor at
    // a real project root when testing against a live server.
    let root = std::env::temp_dir().join(format!("nxvim-lsp-root-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let record = configure_mock("ws-root", serde_json::json!({}));
    let _env = EnvGuard::set("NXVIM_LSP_ROOT", &root);
    let file = temp_file("ws-root", "rs", "fn main() {}\n");
    let (rpc, _incoming) = start(Some(file)).await;

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "initialize")).await;
    let root_uri = find(&recs, "initialize").unwrap()["params"]["rootUri"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        root_uri,
        file_uri(root.to_str().unwrap()),
        "rootUri honors NXVIM_LSP_ROOT"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn rename_matches_a_buffer_opened_through_a_symlink() {
    let _guard = test_lock().lock().await;
    // A server may canonicalize symlinks in the URI it returns (e.g. macOS
    // `/var` → `/private/var`), so it differs from the URI we sent at `didOpen`.
    // The apply must still match the open buffer — by canonicalized path. The
    // buffer is opened via a symlink; the rename is keyed by the real path.
    let real = temp_file("rename-sym-real", "rs", "foo = 1\n");
    let link = std::env::temp_dir().join(format!("nxvim-lsp-sym-{}.rs", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let record = configure_mock(
        "rename-sym",
        serde_json::json!({
            "rename": ws_changes(&[(&real, vec![text_edit(0, 0, 0, 3, "bar")])])
        }),
    );
    let (rpc, _incoming) = start(Some(link.to_str().unwrap().to_string())).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    feed(&rpc, "gg0");
    cmd(&rpc, "LspRename bar").await;
    wait_for_lines(&rpc, &["bar = 1"]).await;
    let _ = std::fs::remove_file(&link);
}

/// `vim.lsp.enable` called **interactively, after the buffer is already open**
/// must start the server for that buffer — not merely arm a `FileType` autocmd
/// for *future* buffers. Opening the file fires `FileType` once; a later
/// `:lua vim.lsp.enable(...)` used to no-op because the dispatcher caught nothing.
/// Mirrors neovim, whose `enable` processes already-loaded buffers on the spot.
#[tokio::test]
async fn enable_after_open_starts_the_server_for_the_current_buffer() {
    let _guard = test_lock().lock().await;
    let record = configure_mock("enable_late", serde_json::json!({}));
    let file = temp_file("enable_late", "rs", "fn main() {}\n");

    // A config dir that *defines* the mock but does not enable it, so opening the
    // rust buffer starts nothing — the enable is the interactive step below.
    let cfg = std::env::temp_dir().join(format!("nxvim-lsp-cfg-late-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).expect("create config dir");
    std::fs::write(
        cfg.join("init.lua"),
        "vim.lsp.config('mock', { cmd = { 'mock' }, filetypes = { 'rust' } })\n",
    )
    .expect("write init.lua");

    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg).await;

    // Nothing enabled yet: the open rust buffer must not have started a server.
    for _ in 0..4 {
        barrier(&rpc).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        record_lines(&record).is_empty(),
        "no server should start before enable, got {:?}",
        record_lines(&record)
    );

    // Enable interactively, after the buffer's FileType already fired. The server
    // must start for the current buffer and run the handshake + didOpen.
    cmd(&rpc, "lua vim.lsp.enable('mock')").await;
    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    let open = find(&recs, "textDocument/didOpen").expect("a didOpen after a late enable");
    assert_eq!(
        open["params"]["textDocument"]["languageId"].as_str(),
        Some("rust"),
        "the late enable opened the current rust buffer against the server"
    );
}

// ----- Phase 7b Slice 1: vim.lsp.buf.* --------------------------------------
//
// The Lua entry points route through the same native `request_lsp*` paths the
// built-in keymaps/ex-commands use. These tests prove that route end-to-end by
// driving each feature through a *Lua-set* keymap (a non-default key, so the
// trigger is unambiguously `vim.lsp.buf.*` and not nxvim's native `gd`/`K`
// defaults) — the `on_attach`-style call site real configs use.

#[tokio::test]
async fn vim_lsp_buf_definition_jumps_via_a_lua_set_keymap() {
    let _guard = test_lock().lock().await;
    // A user maps `<Space>d` to `vim.lsp.buf.definition` (the on_attach pattern).
    // Pressing it enqueues an `LspOp::BufRequest` the server applies on the same
    // tick, issuing the request and jumping the cursor to the reply's location.
    let file = temp_file("buf-def", "rs", "fn target() {}\nfn main() { target() }\n");
    let record = configure_mock(
        "buf-def",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let (rpc, _incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(
        &rpc,
        "lua vim.keymap.set('n', '<Space>d', vim.lsp.buf.definition)",
    )
    .await;

    // From the call site (line 1), the Lua-set key jumps to the definition.
    feed(&rpc, "j d");
    wait_for_cursor(&rpc, (1, 3)).await;
    assert!(
        has_method(&record_lines(&record), "textDocument/definition"),
        "vim.lsp.buf.definition should send a textDocument/definition request"
    );
}

#[tokio::test]
async fn vim_lsp_buf_references_opens_the_panel_via_lua() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.buf.references` routes to the references list — always a panel,
    // navigable with `<CR>`, exactly like the native `gr`.
    let file = temp_file("buf-ref", "rs", "let x = 1\nlet y = x\nlet z = x\n");
    let record = configure_mock(
        "buf-ref",
        serde_json::json!({ "references": [location(&file, 1, 8), location(&file, 2, 8)] }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(
        &rpc,
        "lua vim.keymap.set('n', '<Space>r', vim.lsp.buf.references)",
    )
    .await;

    feed(&rpc, " r");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP references");
    assert_eq!(
        panel_lines.len(),
        2,
        "one row per reference: {panel_lines:?}"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/references"),
        "vim.lsp.buf.references should send a textDocument/references request"
    );

    // `<CR>` on the first row jumps to it: 1-based line 2, byte col 8.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 8)).await;
}

#[tokio::test]
async fn vim_lsp_buf_hover_shows_text_via_lua() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.buf.hover` opens the same panel `K` does, with the markup rendered
    // as plain lines.
    let file = temp_file("buf-hover", "rs", "fn target() {}\n");
    let record = configure_mock(
        "buf-hover",
        serde_json::json!({
            "hover": {
                "contents": { "kind": "markdown", "value": "fn target()\n" }
            }
        }),
    );
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    cmd(
        &rpc,
        "lua vim.keymap.set('n', '<Space>h', vim.lsp.buf.hover)",
    )
    .await;

    feed(&rpc, " h");
    let (title, panel_lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP hover");
    assert_eq!(panel_lines, vec!["fn target()".to_string()]);
    assert!(
        has_method(&record_lines(&record), "textDocument/hover"),
        "vim.lsp.buf.hover should send a textDocument/hover request"
    );
}

#[tokio::test]
async fn vim_lsp_buf_rename_applies_a_cross_buffer_edit_via_lua() {
    let _guard = test_lock().lock().await;
    // `vim.lsp.buf.rename('xyz')` carries the new name (nxvim requires it — no
    // prompt UI) and applies the returned WorkspaceEdit across every open buffer,
    // the same path `:LspRename` drives.
    let file_a = temp_file("buf-rename-a", "rs", "foo = foo\n");
    let file_b = temp_file("buf-rename-b", "rs", "use foo\n");
    let record = configure_mock(
        "buf-rename",
        serde_json::json!({
            "rename": ws_changes(&[
                (&file_a, vec![text_edit(0, 0, 0, 3, "xyz"), text_edit(0, 6, 0, 9, "xyz")]),
                (&file_b, vec![text_edit(0, 4, 0, 7, "xyz")]),
            ])
        }),
    );
    let (rpc, _incoming) = start(Some(file_a)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    let buf_a = current_buf(&rpc).await;

    // Open B so the rename can reach it, then return to A.
    cmd(&rpc, &format!("e {file_b}")).await;
    wait_for_record(&rpc, &record, |r| {
        count_method(r, "textDocument/didOpen") >= 2
    })
    .await;
    let buf_b = current_buf(&rpc).await;
    set_buf(&rpc, buf_a).await;

    // Cursor at the start of A; rename foo → xyz through the Lua entry point.
    feed(&rpc, "gg0");
    cmd(&rpc, "lua vim.lsp.buf.rename('xyz')").await;
    wait_for_lines(&rpc, &["xyz = xyz"]).await;
    assert_eq!(
        lines_of_buf(&rpc, buf_b).await,
        vec!["use xyz".to_string()],
        "vim.lsp.buf.rename reached the other open buffer"
    );
    assert!(
        has_method(&record_lines(&record), "textDocument/rename"),
        "vim.lsp.buf.rename should send a textDocument/rename request"
    );
}

// ----- Phase 7b Slice 2: vim.diagnostic.* -----------------------------------
//
// `get` reads the Rust→Lua mirror through `nvim_exec_lua`; the actions
// (`goto_next`/`goto_prev`/`setloclist`/`config`) enqueue an LspOp the server
// applies, reusing the native cursor-move / panel / underline paths.

/// Evaluate a Lua chunk on the server and return its value over RPC.
async fn exec_lua(rpc: &Rpc, code: &str) -> Value {
    rpc.request(
        "nvim_exec_lua",
        vec![Value::from(code), Value::Array(vec![])],
    )
    .await
    .expect("nvim_exec_lua")
}

/// A field of an rmpv map by key (the diagnostic tables `vim.diagnostic.get`
/// returns serialize to maps).
fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val)
}

#[tokio::test]
async fn vim_diagnostic_get_returns_the_mirror_with_a_severity_filter() {
    let _guard = test_lock().lock().await;
    // Two diagnostics of different severities are published; `get(0)` reads them
    // back from the mirror with neovim's field shape (0-based lnum/col, severity
    // 1=ERROR…4=HINT), and `opts.severity` filters to one.
    let record = configure_mock(
        "diag-get",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(0, 4, 7, 1, "error one"),
                diag(2, 0, 3, 2, "warn two"),
            ],
        }),
    );
    let file = temp_file("diag-get", "rs", "let bad = 1\nfn ok() {}\nzzz = 2\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // Poll the mirror until both publishes have landed.
    let all = loop {
        let v = exec_lua(&rpc, "return vim.diagnostic.get(0)").await;
        if v.as_array().map(|a| a.len()).unwrap_or(0) == 2 {
            break v;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let arr = all.as_array().unwrap();

    // The first entry carries the error, indexed the neovim way.
    let e = &arr[0];
    assert_eq!(map_get(e, "lnum").and_then(Value::as_i64), Some(0));
    assert_eq!(map_get(e, "col").and_then(Value::as_i64), Some(4));
    assert_eq!(map_get(e, "end_col").and_then(Value::as_i64), Some(7));
    assert_eq!(map_get(e, "severity").and_then(Value::as_i64), Some(1));
    assert_eq!(
        map_get(e, "message").and_then(Value::as_str),
        Some("error one")
    );

    // The severity filter keeps only the matching diagnostic.
    let errors = exec_lua(&rpc, "return vim.diagnostic.get(0, { severity = 1 })").await;
    assert_eq!(
        errors.as_array().map(|a| a.len()),
        Some(1),
        "severity=1 keeps only the error: {errors:?}"
    );
    let warns = exec_lua(&rpc, "return vim.diagnostic.get(0, { severity = 2 })").await;
    assert_eq!(
        warns
            .as_array()
            .and_then(|a| a.first())
            .and_then(|d| map_get(d, "message"))
            .and_then(Value::as_str),
        Some("warn two"),
        "severity=2 keeps only the warning: {warns:?}"
    );
}

#[tokio::test]
async fn vim_diagnostic_goto_moves_across_diagnostics_and_wraps() {
    let _guard = test_lock().lock().await;
    // Diagnostics at (line 0, col 4) and (line 2, col 0). goto_next walks forward
    // and wraps past the last back to the first; goto_prev wraps before the first
    // to the last.
    let record = configure_mock(
        "diag-goto",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [
                diag(0, 4, 7, 1, "first"),
                diag(2, 0, 3, 1, "second"),
            ],
        }),
    );
    let file = temp_file("diag-goto", "rs", "let bad = 1\nfn ok() {}\nzzz = 2\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // From (0,0): forward to the first diagnostic, then the second.
    exec_lua(&rpc, "vim.diagnostic.goto_next()").await;
    wait_for_cursor(&rpc, (1, 4)).await;
    exec_lua(&rpc, "vim.diagnostic.goto_next()").await;
    wait_for_cursor(&rpc, (3, 0)).await;
    // Past the last: wrap to the first.
    exec_lua(&rpc, "vim.diagnostic.goto_next()").await;
    wait_for_cursor(&rpc, (1, 4)).await;
    // Before the first: wrap to the last.
    exec_lua(&rpc, "vim.diagnostic.goto_prev()").await;
    wait_for_cursor(&rpc, (3, 0)).await;
}

#[tokio::test]
async fn vim_diagnostic_setloclist_opens_the_navigable_panel() {
    let _guard = test_lock().lock().await;
    let record = configure_mock(
        "diag-loclist",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(1, 4, 5, 1, "x is bad")],
        }),
    );
    let file = temp_file("diag-loclist", "rs", "fn main() {}\nlet x = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    exec_lua(&rpc, "vim.diagnostic.setloclist()").await;
    let (title, lines) = wait_for_panel(&rpc, &mut incoming).await;
    assert_eq!(title, "LSP diagnostics");
    assert_eq!(lines.len(), 1, "one diagnostic, one row: {lines:?}");
    assert!(
        lines[0].contains("x is bad"),
        "row carries the message: {lines:?}"
    );

    // The panel is navigable: `<CR>` jumps to line 2 (1-based), byte col 4.
    feed(&rpc, "<CR>");
    wait_for_cursor(&rpc, (2, 4)).await;
}

#[tokio::test]
async fn vim_diagnostic_config_underline_false_hides_the_squiggles() {
    let _guard = test_lock().lock().await;
    // The one config key with a backing surface: `underline = false` removes the
    // diagnostic underline spans from the redraw (the message line/panel stay).
    let record = configure_mock(
        "diag-config",
        serde_json::json!({
            "position_encoding": "utf-8",
            "diagnostics": [diag(0, 4, 7, 1, "bad")],
        }),
    );
    let file = temp_file("diag-config", "rs", "let bad = 1\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    wait_for_diagnostics(&rpc, &mut incoming).await;

    // Disable the underline; the spans drain out of the redraw.
    exec_lua(&rpc, "vim.diagnostic.config({ underline = false })").await;
    let mut hidden = false;
    for _ in 0..80 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if diagnostics_of(&params).iter().all(|row| row.is_empty()) {
                hidden = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(hidden, "underline=false should clear the diagnostic spans");

    // Re-enabling brings them back.
    exec_lua(&rpc, "vim.diagnostic.config({ underline = true })").await;
    wait_for_diagnostics(&rpc, &mut incoming).await;
}

// ----- Phase 7b Slice 3: LspAttach / on_attach / server_capabilities --------
//
// When a buffer's first `didOpen` goes out under an initialized server, the
// server fires `LspAttach` with `data.client_id`; the default autocmd in the
// `nxvim.lsp.enable` augroup resolves the client and runs the config's
// `on_attach(client, bufnr)` — the call site that wires buffer-local LSP keymaps
// and reads `client.server_capabilities`.

/// A fresh config dir whose `init.lua` defines + enables the `mock` server with
/// the given `on_attach` body (a Lua chunk with `client`/`bufnr` in scope).
fn attach_config_dir(tag: &str, on_attach_body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-lsp-cfg-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "vim.lsp.config('mock', {{ cmd = {{ 'mock' }}, filetypes = {{ 'rust' }}, \
             on_attach = function(client, bufnr)\n{on_attach_body}\nend }})\n\
             vim.lsp.enable('mock')\n"
        ),
    )
    .expect("write init.lua");
    dir
}

#[tokio::test]
async fn on_attach_sets_a_buffer_local_keymap_that_drives_definition() {
    let _guard = test_lock().lock().await;
    // The config's `on_attach` maps a non-default key (`<Space>d`) to
    // `vim.lsp.buf.definition`, buffer-local. That the key works proves the server
    // fired `LspAttach` on attach and the default autocmd ran `on_attach`.
    let file = temp_file(
        "attach-def",
        "rs",
        "fn target() {}\nfn main() { target() }\n",
    );
    let record = configure_mock(
        "attach-def",
        serde_json::json!({ "definition": location(&file, 0, 3) }),
    );
    let cfg = attach_config_dir(
        "attach-def",
        "vim.keymap.set('n', '<Space>d', vim.lsp.buf.definition, { buffer = bufnr })",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    // From the call site (line 1), the `on_attach`-set key jumps to the definition
    // (the space in the feed string is the `<Space>` half of the `<Space>d` map).
    feed(&rpc, "j d");
    wait_for_cursor(&rpc, (1, 3)).await;
    assert!(
        has_method(&record_lines(&record), "textDocument/definition"),
        "the on_attach keymap should drive a textDocument/definition request"
    );
}

#[tokio::test]
async fn on_attach_reads_the_server_capabilities() {
    let _guard = test_lock().lock().await;
    // `client.server_capabilities` is readable inside `on_attach`: the mock
    // advertises the provider booleans, and the client carries its name/id. The
    // body stashes them in a global the test reads back over `nvim_exec_lua`.
    let file = temp_file("attach-caps", "rs", "fn main() {}\n");
    let record = configure_mock("attach-caps", serde_json::json!({}));
    let cfg = attach_config_dir(
        "attach-caps",
        "_G.nxvim_seen = { \
           hover = client.server_capabilities.hoverProvider, \
           definition = client.server_capabilities.definitionProvider, \
           name = client.name, \
           has_id = client.id ~= nil, \
         }",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    let seen = exec_lua(&rpc, "return _G.nxvim_seen").await;
    assert_eq!(
        map_get(&seen, "hover").and_then(Value::as_bool),
        Some(true),
        "server_capabilities.hoverProvider is readable in on_attach: {seen:?}"
    );
    assert_eq!(
        map_get(&seen, "definition").and_then(Value::as_bool),
        Some(true),
        "server_capabilities.definitionProvider is readable in on_attach: {seen:?}"
    );
    assert_eq!(
        map_get(&seen, "name").and_then(Value::as_str),
        Some("mock"),
        "the client carries its config name: {seen:?}"
    );
    assert_eq!(
        map_get(&seen, "has_id").and_then(Value::as_bool),
        Some(true),
        "the client carries a numeric id: {seen:?}"
    );
}

#[tokio::test]
async fn a_withheld_capability_reads_as_falsy_in_on_attach() {
    let _guard = test_lock().lock().await;
    // A server that doesn't advertise `hoverProvider` surfaces it as nil/false, so
    // an `on_attach` can gate a mapping on the capability. The script overrides the
    // mock's advertised `hoverProvider` to `false` (other providers stay on).
    let file = temp_file("attach-nohover", "rs", "fn main() {}\n");
    let record = configure_mock(
        "attach-nohover",
        serde_json::json!({
            "capabilities": {
                "definitionProvider": true,
                "hoverProvider": false,
            }
        }),
    );
    let cfg = attach_config_dir(
        "attach-nohover",
        "_G.nxvim_hover = client.server_capabilities.hoverProvider or false\n\
         _G.nxvim_def = client.server_capabilities.definitionProvider or false",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_hover").await.as_bool(),
        Some(false),
        "a withheld hoverProvider reads falsy in on_attach"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_def").await.as_bool(),
        Some(true),
        "an advertised definitionProvider still reads true"
    );
}

#[tokio::test]
async fn the_config_settings_init_options_and_capabilities_reach_the_server() {
    let _guard = test_lock().lock().await;
    // Phase 2: a config's `settings` / `init_options` / `capabilities` must reach
    // the server, not be dropped. The config sets a sentinel in each; the mock
    // records the handshake, so we can assert each sentinel arrived where it should.
    let record = configure_mock("cfgfwd", serde_json::json!({}));
    let content = "fn main() {}\n";
    let file = temp_file("cfgfwd", "rs", content);

    let dir = std::env::temp_dir().join(format!("nxvim-lsp-cfgfwd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("init.lua"),
        "vim.lsp.config('mock', { cmd = { 'mock' }, filetypes = { 'rust' }, \
         settings = { ['mock-ls'] = { sentinel = 'SETTING' } }, \
         init_options = { initSentinel = 'INIT' }, \
         capabilities = { experimental = { nxvimSentinel = true } } })\n\
         vim.lsp.enable('mock')\n",
    )
    .expect("write init.lua");

    let (rpc, _incoming) = start_with_config_dir(Some(file), dir.clone()).await;

    // Wait until both the handshake and the post-`initialized` configuration push
    // are recorded.
    let recs = wait_for_record(&rpc, &record, |r| {
        has_method(r, "workspace/didChangeConfiguration")
    })
    .await;

    let init = find(&recs, "initialize").expect("an initialize request");
    // init_options is forwarded verbatim as initializationOptions.
    assert_eq!(
        init["params"]["initializationOptions"]["initSentinel"].as_str(),
        Some("INIT"),
        "init_options should reach initialize as initializationOptions, got {:?}",
        init["params"]["initializationOptions"]
    );
    // The config's capabilities are deep-merged OVER nxvim's base ones: the
    // sentinel is present AND the base positionEncodings survive.
    assert_eq!(
        init["params"]["capabilities"]["experimental"]["nxvimSentinel"].as_bool(),
        Some(true),
        "config capabilities should merge into initialize, got {:?}",
        init["params"]["capabilities"]["experimental"]
    );
    assert_eq!(
        init["params"]["capabilities"]["general"]["positionEncodings"][0].as_str(),
        Some("utf-8"),
        "the base capabilities must survive the merge"
    );
    // settings arrive via workspace/didChangeConfiguration after `initialized`.
    let cfg = find(&recs, "workspace/didChangeConfiguration").unwrap();
    assert_eq!(
        cfg["params"]["settings"]["mock-ls"]["sentinel"].as_str(),
        Some("SETTING"),
        "settings should reach didChangeConfiguration, got {:?}",
        cfg["params"]["settings"]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Write an `init.lua` that registers the mock with an extra config fragment
/// (lifecycle hooks, etc.) spliced into the `vim.lsp.config('mock', { … })` table.
fn hook_config_dir(tag: &str, config_fragment: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-lsp-hook-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create config dir");
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "vim.lsp.config('mock', {{ cmd = {{ 'mock' }}, filetypes = {{ 'rust' }}, \
             {config_fragment} }})\nvim.lsp.enable('mock')\n"
        ),
    )
    .expect("write init.lua");
    dir
}

#[tokio::test]
async fn before_init_shapes_the_initialize_params() {
    let _guard = test_lock().lock().await;
    // Phase 3: `before_init(init_params, config)` runs just before the handshake
    // and can set `init_params.initializationOptions` — the rust_analyzer pattern
    // (copy settings → initializationOptions). The mock records what arrived.
    let record = configure_mock("beforeinit", serde_json::json!({}));
    let file = temp_file("beforeinit", "rs", "fn main() {}\n");
    let cfg = hook_config_dir(
        "beforeinit",
        "settings = { ['rust-analyzer'] = { cargo = { sentinel = 'BI' } } }, \
         before_init = function(init_params, config) \
           init_params.initializationOptions = config.settings['rust-analyzer'] \
         end",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "initialize")).await;
    let init = find(&recs, "initialize").expect("an initialize request");
    assert_eq!(
        init["params"]["initializationOptions"]["cargo"]["sentinel"].as_str(),
        Some("BI"),
        "before_init's initializationOptions should reach initialize, got {:?}",
        init["params"]["initializationOptions"]
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

#[tokio::test]
async fn on_init_runs_with_the_real_initialize_result() {
    let _guard = test_lock().lock().await;
    // Phase 3: `on_init(client, result)` fires after `initialize`, with the raw
    // result. The hook stashes a value derived from `result` so we can prove it ran
    // and saw real data, not a faked empty.
    let record = configure_mock("oninit", serde_json::json!({}));
    let file = temp_file("oninit", "rs", "fn main() {}\n");
    let cfg = hook_config_dir(
        "oninit",
        "on_init = function(client, result) \
           _G.nxvim_on_init = (result.capabilities ~= nil) and client.name or 'NO_RESULT' \
         end",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;

    // didOpen is sent after `initialized`, so once it's recorded on_init has run.
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_on_init").await.as_str(),
        Some("mock"),
        "on_init should run with the real initialize result and the client"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

#[tokio::test]
async fn on_exit_runs_with_the_exit_code() {
    let _guard = test_lock().lock().await;
    // Phase 3: `on_exit(code, signal, client)` fires when the server exits. The mock
    // exits cleanly right after `initialize`; the hook records the code/name.
    configure_mock(
        "onexit",
        serde_json::json!({ "exit_after_initialize": true }),
    );
    let file = temp_file("onexit", "rs", "fn main() {}\n");
    let cfg = hook_config_dir(
        "onexit",
        "on_exit = function(code, signal, client) \
           _G.nxvim_on_exit_code = code; _G.nxvim_on_exit_name = client.name \
         end",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;

    // Drive the loop until the exit has been processed and the hook ran.
    let mut code = Value::Nil;
    for _ in 0..100 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        code = exec_lua(&rpc, "return _G.nxvim_on_exit_code").await;
        if !code.is_nil() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        code.as_i64(),
        Some(0),
        "on_exit should run with the child's exit code (0)"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_on_exit_name")
            .await
            .as_str(),
        Some("mock"),
        "on_exit should receive the exiting client"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}
