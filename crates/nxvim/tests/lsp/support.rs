//! LSP-specific test helpers shared across the phase submodules.
//!
//! The generic black-box harness (server spawn, `feed`/`lines`/`exec_lua`, the
//! redraw accessors, temp-file + serialization helpers) comes from the shared
//! [`nxvim_test_harness`] crate, re-exported below. This module adds only the
//! parts unique to the LSP tests: the scripted mock-server configuration, the
//! record-file polling, and the LSP-aware redraw/panel accessors.

#![allow(dead_code)]
// The re-export list below is the shared convention for every test submodule;
// a couple of names (`connect`, `run as run_server`) aren't referenced now that
// spawning lives in the harness crate, but are kept for parity and future use.
#![allow(unused_imports)]

pub use nxvim_test_harness::*;

pub use nxvim_rpc::{connect, Incoming, Rpc};
pub use nxvim_server::{run as run_server, ServerInit};
pub use nxvim_tui::*;
pub use nxvim_view::View;
pub use rmpv::Value;
pub use serde_json::Value as Json;
pub use tokio::sync::mpsc::UnboundedReceiver;

pub use std::path::{Path, PathBuf};
pub use std::time::Duration;

use std::sync::OnceLock;

pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;
/// The hybrid number-column width the editor ships with — diagnostic/text screen
/// columns are offset by this much once the gutter is split off (matches
/// `screen.rs`).
pub const GUTTER: u16 = 4;
/// The width of the reserved diagnostic sign column (vim's fixed 2-cell
/// `signcolumn`). With signs on (the default), a diagnostic-bearing buffer's text
/// is offset by this much *in addition to* the [`GUTTER`].
pub const SIGN: u16 = 2;

/// Serializes the subprocess-spawning tests (shared env + server lifecycle).
pub use nxvim_test_harness::serial_lock as test_lock;

// ----- mock configuration ---------------------------------------------------

/// Write a mock script with a unique record file and point `NXVIM_LSP_CMD` at
/// `nxvim --__lsp-mock <script>`, so every configured filetype launches the mock
/// (the LSP analogue of `NXVIM_TS_WORKER`). `extra` merges script fields
/// (`position_encoding`, `sync_kind`, `exit_after_initialize`). Returns the
/// record file path. Also points the *syntax* worker env at the real binary so a
/// `.rs` buffer doesn't try to re-spawn this test executable as a ts worker.
pub fn configure_mock(tag: &str, extra: Json) -> PathBuf {
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
pub fn lsp_log_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nxvim-lsp-log-{}-{tag}.log", std::process::id()))
}

/// Parse the record file into the `{method, params}` objects the mock appended.
pub fn record_lines(path: &Path) -> Vec<Json> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The first recorded message with the given method, if any.
pub fn find<'a>(recs: &'a [Json], method: &str) -> Option<&'a Json> {
    recs.iter().find(|r| r["method"] == method)
}

/// True once a message with `method` has been recorded.
pub fn has_method(recs: &[Json], method: &str) -> bool {
    recs.iter().any(|r| r["method"] == method)
}

/// How many recorded messages carried `method`.
pub fn count_method(recs: &[Json], method: &str) -> usize {
    recs.iter().filter(|r| r["method"] == method).count()
}

// ----- server harness -------------------------------------------------------

/// A temp config dir whose `init.lua` registers the mock language server through
/// the Phase 7a Lua start path — `vim.lsp.config('mock', …)` + `vim.lsp.enable` —
/// for the filetypes the tests open. There is no built-in auto-spawn anymore, so
/// every LSP test starts its server this way. The config's `cmd` is a placeholder:
/// the real command is injected per-test via `$NXVIM_LSP_CMD` (see
/// `configure_mock`), which the server honors over the config's `cmd`.
pub fn lsp_config_dir() -> PathBuf {
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
pub async fn start(file: Option<String>) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_with_config_dir(file, lsp_config_dir()).await
}

/// Like [`start`], but with an explicit `config_dir` — for tests that need a
/// `vim.lsp.config`/`enable` arrangement other than the shared one (e.g. enabling
/// the server *interactively, after* the buffer is already open).
pub async fn start_with_config_dir(
    file: Option<String>,
    config_dir: PathBuf,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(
        ServerInit {
            file,
            config_dir: Some(config_dir),
            ..Default::default()
        },
        COLS,
        ROWS - 2,
    )
    .await
}

/// Force the server to process pending input and emit a redraw (which drives LSP
/// document sync), so the mock has a chance to receive the next message.
pub async fn barrier(rpc: &Rpc) {
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

/// Poll (bounded) until the record file satisfies `pred`, returning the parsed
/// records. Panics with the records seen if it never does.
pub async fn wait_for_record(
    rpc: &Rpc,
    record: &Path,
    pred: impl Fn(&[Json]) -> bool,
) -> Vec<Json> {
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

/// The bottom panel's `(title, lines)` from a redraw, if one is open.
pub fn panel_of(params: &[Value]) -> Option<(String, Vec<String>)> {
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
pub async fn wait_for_panel(
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
///
/// Kept local rather than delegating to the harness's `write_temp`: the LSP
/// reference/diagnostic panels word-wrap the file path to the panel width, so the
/// row counts these tests assert on are sensitive to the path *length*. This
/// preserves the original (shorter) `nxvim-lsp-<pid>-<tag>` shape so those
/// assertions stay valid; `write_temp`'s longer `nxvim_test_<tag>_<pid>_<n>`
/// shape tips a 2-reference list over the wrap boundary into 4 rows.
pub fn temp_file(tag: &str, ext: &str, content: &str) -> String {
    let path = std::env::temp_dir().join(format!("nxvim-lsp-{}-{tag}.{ext}", std::process::id()));
    std::fs::write(&path, content).unwrap();
    path.display().to_string()
}

/// Concatenate the `text` of every `content_changes` entry across all recorded
/// `didChange` notifications — the full text the deltas inserted.
pub fn did_change_text(recs: &[Json]) -> String {
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

/// Decode the `diagnostics` redraw key into per-row `(start, end, severity)`
/// screen-column spans (dropping the trailing style-id, which is `Nil` with no
/// colorscheme loaded).
pub fn diagnostics_of(params: &[Value]) -> Vec<Vec<(u64, u64, u64)>> {
    window0_get(params, "diagnostics")
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

/// Decode the `diagnostics_virt` redraw key into per-row inline virtual-text
/// decorations: `Some((text, severity))` for a row carrying one, `None` for a
/// bare row (the trailing style-id is dropped, `Nil` with no colorscheme).
pub fn diagnostics_virt_of(params: &[Value]) -> Vec<Option<(String, u64)>> {
    window0_get(params, "diagnostics_virt")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let t = row.as_array()?;
                    Some((t[0].as_str()?.to_string(), t[1].as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decode the `diagnostics_signs` redraw key into per-row gutter signs:
/// `Some((glyph, severity))` for a row carrying one, `None` for a bare row (the
/// trailing style-id is dropped, `Nil` with no colorscheme).
pub fn diagnostics_signs_of(params: &[Value]) -> Vec<Option<(String, u64)>> {
    window0_get(params, "diagnostics_signs")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let t = row.as_array()?;
                    Some((t[0].as_str()?.to_string(), t[1].as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `sign_column` redraw flag for window 0 (whether a sign column is reserved).
pub fn sign_column_of(params: &[Value]) -> bool {
    window0_get(params, "sign_column")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Poll (bounded) until a redraw whose `diagnostics_signs` has a sign on some row
/// arrives, returning that row's `(glyph, severity)`.
pub async fn wait_for_signs(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> (String, u64) {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if let Some(hit) = diagnostics_signs_of(&params).into_iter().flatten().next() {
                return hit;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("a diagnostic gutter sign never appeared in a redraw");
}

/// Poll (bounded) until a redraw whose `diagnostics_virt` has a non-empty row
/// arrives, returning that row's `(text, severity)`.
pub async fn wait_for_virt_text(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> (String, u64) {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if let Some(hit) = diagnostics_virt_of(&params).into_iter().flatten().next() {
                return hit;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("inline diagnostic virtual text never appeared in a redraw");
}

/// Poll (bounded) until a redraw whose `diagnostics` has at least one non-empty
/// row arrives, returning that redraw's params.
pub async fn wait_for_diagnostics(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Vec<Value> {
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
pub async fn wait_for_message(
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
pub fn file_uri(path: &str) -> String {
    format!("file://{path}")
}

/// An LSP `Location` (zero-width range at `line:character`) for the mock script's
/// `definition` / `references` / … fields.
pub fn location(path: &str, line: u32, character: u32) -> Json {
    serde_json::json!({
        "uri": file_uri(path),
        "range": {
            "start": { "line": line, "character": character },
            "end": { "line": line, "character": character },
        }
    })
}

/// The current cursor as `(1-based line, 0-based column)`, like neovim.
pub async fn cursor(rpc: &Rpc) -> (u64, u64) {
    cursor_u64(rpc).await
}

/// Poll (bounded) until the cursor reaches `want`, driving the loop so async LSP
/// replies are processed. Panics with the last position seen if it never does.
pub async fn wait_for_cursor(rpc: &Rpc, want: (u64, u64)) {
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
pub fn diag(line: u32, start: u32, end: u32, severity: u32, message: &str) -> Json {
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
pub fn has_highlights(params: &[Value]) -> bool {
    window0_get(params, "highlights")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.as_array().is_some_and(|spans| !spans.is_empty()))
        })
}

/// Poll until a redraw shows syntax highlights (the worker has caught up).
pub async fn wait_for_highlights(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>) {
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

// ----- text edits / workspace edits (formatting, rename, code actions) -------

/// An LSP `TextEdit` (`{range, newText}`) for the mock's `formatting`/`rename`/
/// `code_action` script fields. Positions are in the server's negotiated encoding.
pub fn text_edit(sl: u32, sc: u32, el: u32, ec: u32, new: &str) -> Json {
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
pub fn ws_changes(entries: &[(&str, Vec<Json>)]) -> Json {
    let mut changes = serde_json::Map::new();
    for (path, edits) in entries {
        changes.insert(file_uri(path), Json::Array(edits.clone()));
    }
    let mut edit = serde_json::Map::new();
    edit.insert("changes".to_string(), Json::Object(changes));
    Json::Object(edit)
}

/// Run an ex-command over RPC (awaiting the dispatch, not the async LSP reply).
pub async fn cmd(rpc: &Rpc, command: &str) {
    rpc.request("nvim_command", vec![Value::from(command)])
        .await
        .expect("command");
}

/// Poll (bounded) until the current buffer's lines equal `want`.
pub async fn wait_for_lines(rpc: &Rpc, want: &[&str]) {
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
pub async fn current_buf(rpc: &Rpc) -> u64 {
    rpc.request("nvim_get_current_buf", vec![])
        .await
        .unwrap()
        .as_u64()
        .unwrap()
}

/// Make buffer `id` current (the `nvim_set_current_buf` entry point).
pub async fn set_buf(rpc: &Rpc, id: u64) {
    rpc.request("nvim_set_current_buf", vec![Value::from(id)])
        .await
        .unwrap();
}

/// Editable lines of buffer `id` (read by handle, without switching to it).
pub async fn lines_of_buf(rpc: &Rpc, id: u64) -> Vec<String> {
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

// ----- Lua value accessor ----------------------------------------------------

/// A field of an rmpv map by key (the diagnostic tables `vim.diagnostic.get`
/// returns serialize to maps).
pub fn map_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val)
}

// ----- on_attach config dir --------------------------------------------------

/// A fresh config dir whose `init.lua` defines + enables the `mock` server with
/// the given `on_attach` body (a Lua chunk with `client`/`bufnr` in scope).
pub fn attach_config_dir(tag: &str, on_attach_body: &str) -> PathBuf {
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
