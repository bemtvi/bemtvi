//! The nxvim treesitter syntax worker.
//!
//! This crate is reached **only in worker mode** (`nxvim --__ts-worker`): a
//! separate process the editor server spawns and supervises, so that a crash in
//! a compiled C grammar — even a segfault — takes down only this process and
//! never the editor. It speaks nxvim's own msgpack-RPC ([`nxvim_rpc`]) over its
//! stdio, loads **installable** grammars from disk by language, and parses
//! **incrementally** so huge files stay responsive.
//!
//! Protocol (all msgpack maps, server→worker unless noted):
//! - `ts_open`  `{buffer, tick, language, text, first_line, last_line}`
//! - `ts_edit`  `{buffer, tick, edits, first_line, last_line}`
//! - `ts_view`  `{buffer, first_line, last_line}`
//! - `ts_close` `{buffer}` — forget a deleted buffer's shadow + tree
//! - `ts_highlights` `{buffer, tick, first_line, last_line, spans}` (worker→server)
//! - `ts_error` `{buffer, language, message}` (worker→server)

mod engine;
mod loader;

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;

use nxvim_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};

use engine::Engine;
use nxvim_rpc::syntax::{decode_edits, encode_spans};

/// Resolve nxvim's data directory (where `parser/` and `queries/` live).
/// `$NXVIM_DATA_DIR` overrides everything (used by tests); otherwise the
/// platform's standard per-user data location, suffixed `nxvim`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("NXVIM_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(dir).join("nxvim");
    }
    #[cfg(not(windows))]
    {
        if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("nxvim");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".local/share/nxvim");
        }
    }
    PathBuf::from(".nxvim")
}

/// Run the worker loop over a connected stream (the server's child stdio) until
/// the parent closes the pipe. Each request is processed under `catch_unwind`, so
/// a Rust-level panic in one message degrades to "no spans" without dropping the
/// process (a hard C segfault is instead handled by the parent respawning us).
pub async fn run_worker<S>(reader: impl AsyncRead + Unpin + Send + 'static, writer: S)
where
    S: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, mut incoming) = connect(reader, writer);
    let mut engine = Engine::new(data_dir());
    // Test-only: when `$NXVIM_TS_RECORD` is set, append a one-line summary of each
    // incoming message (notably the per-`ts_edit` delta size). Lets the test
    // suite prove that editing a huge file sends tiny deltas, not the whole file.
    let recorder = std::env::var("NXVIM_TS_RECORD").ok();

    while let Some(message) = incoming.recv().await {
        let Incoming::Notification { method, params } = message else {
            continue; // the worker answers only notifications
        };
        if let Some(path) = &recorder {
            record(path, &method, &params);
        }
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            handle(&mut engine, &rpc, &method, &params);
        }));
        if outcome.is_err() {
            eprintln!("nxvim-ts: recovered from a panic handling '{method}'");
        }
    }
}

fn handle(engine: &mut Engine, rpc: &Rpc, method: &str, params: &[Value]) {
    let Some(Value::Map(map)) = params.first() else {
        return;
    };
    let buffer = map_u64(map, "buffer");
    match method {
        "ts_open" => {
            let lang = map_str(map, "language");
            // Test hook: a reserved language that hard-crashes the process, so the
            // crash-isolation/respawn path can be exercised deterministically. No
            // real grammar is named this.
            if lang == "__crash" {
                std::process::abort();
            }
            let text = map_str(map, "text");
            match engine.open(buffer, &lang, &text) {
                Ok(()) => send_highlights(engine, rpc, map, buffer),
                Err(message) => rpc.notify(
                    "ts_error",
                    vec![Value::Map(vec![
                        (Value::from("buffer"), Value::from(buffer)),
                        (Value::from("language"), Value::from(lang.as_str())),
                        (Value::from("message"), Value::from(message.as_str())),
                    ])],
                ),
            }
        }
        "ts_edit" => {
            let edits = decode_edits(map_get(map, "edits"));
            engine.edit(buffer, &edits);
            send_highlights(engine, rpc, map, buffer);
        }
        "ts_view" => send_highlights(engine, rpc, map, buffer),
        "ts_close" => engine.close(buffer),
        _ => {}
    }
}

/// Compute highlights for the request's visible range and notify the server.
fn send_highlights(engine: &mut Engine, rpc: &Rpc, map: &[(Value, Value)], buffer: u64) {
    if engine.language_of(buffer).is_none() {
        return;
    }
    let tick = map_u64(map, "tick");
    let first = map_u64(map, "first_line") as usize;
    let last = map_u64(map, "last_line") as usize;
    let spans = engine.highlights(buffer, first, last);
    rpc.notify(
        "ts_highlights",
        vec![Value::Map(vec![
            (Value::from("buffer"), Value::from(buffer)),
            (Value::from("tick"), Value::from(tick)),
            (Value::from("first_line"), Value::from(first as u64)),
            (Value::from("last_line"), Value::from(last as u64)),
            (Value::from("spans"), encode_spans(&spans)),
        ])],
    );
}

/// Append a one-line summary of an incoming message to the record file. For
/// `ts_open` it notes the full-text length; for `ts_edit`, the number of deltas
/// and their total inserted-byte size — the figure that should stay tiny on a
/// huge file. Test instrumentation only (gated by `$NXVIM_TS_RECORD`).
fn record(path: &str, method: &str, params: &[Value]) {
    use std::io::Write;
    let mut line = method.to_string();
    if let Some(Value::Map(map)) = params.first() {
        if let Some(text) = map_get(map, "text").and_then(Value::as_str) {
            line.push_str(&format!(" text={}", text.len()));
        }
        if let Some(Value::Array(edits)) = map_get(map, "edits") {
            let delta: usize = edits
                .iter()
                .filter_map(Value::as_array)
                .filter_map(|a| a.get(9))
                .filter_map(Value::as_str)
                .map(str::len)
                .sum();
            line.push_str(&format!(" edits={} delta={}", edits.len(), delta));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn map_get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn map_u64(map: &[(Value, Value)], key: &str) -> u64 {
    map_get(map, key).and_then(Value::as_u64).unwrap_or(0)
}

fn map_str(map: &[(Value, Value)], key: &str) -> String {
    map_get(map, key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}
