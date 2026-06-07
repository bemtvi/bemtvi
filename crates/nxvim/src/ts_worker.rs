//! The treesitter syntax **worker loop**.
//!
//! Reached only in worker mode (`nxvim --__ts-worker`): a separate process the
//! editor server spawns and supervises, so a crash in a compiled C grammar —
//! even a segfault — takes down only this process and never the editor. It
//! speaks nxvim's own msgpack-RPC ([`nxvim_rpc`]) over its stdio and drives the
//! in-process [`nxvim_ts::Engine`], which loads **installable** grammars from
//! disk by language and parses **incrementally** so huge files stay responsive.
//!
//! This is the RPC plumbing only; the parse + highlight logic lives in
//! `nxvim-ts`. (The whole worker layer is slated for removal once highlighting
//! moves in-process — see the in-process-treesitter design doc.)
//!
//! Protocol (all msgpack maps, server→worker unless noted):
//! - `ts_open`  `{buffer, tick, language, text, first_line, last_line}`
//! - `ts_edit`  `{buffer, tick, edits, first_line, last_line}`
//! - `ts_view`  `{buffer, first_line, last_line}`
//! - `ts_close` `{buffer}` — forget a deleted buffer's shadow + tree
//! - `ts_highlights` `{buffer, tick, first_line, last_line, spans}` (worker→server)
//! - `ts_error` `{buffer, language, message}` (worker→server)

use std::panic::AssertUnwindSafe;

use nxvim_core::{BufferEdit, BufferId, Span};
use nxvim_rpc::syntax::{decode_edits, encode_spans, EditWire, SpanWire};
use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_ts::{data_dir, Engine};
use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};

/// Run the worker loop over a connected stream (the server's child stdio) until
/// the parent closes the pipe. Each request is processed under `catch_unwind`, so
/// a Rust-level panic in one message degrades to "no spans" without dropping the
/// process (a hard C segfault is instead handled by the parent respawning us).
pub async fn run<S>(reader: impl AsyncRead + Unpin + Send + 'static, writer: S)
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
            match engine.open(BufferId(buffer), &lang, &text) {
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
            let edits: Vec<BufferEdit> = decode_edits(map_get(map, "edits"))
                .iter()
                .map(edit_from_wire)
                .collect();
            engine.edit(BufferId(buffer), &edits);
            send_highlights(engine, rpc, map, buffer);
        }
        "ts_view" => send_highlights(engine, rpc, map, buffer),
        "ts_close" => engine.close(BufferId(buffer)),
        _ => {}
    }
}

/// Compute highlights for the request's visible range and notify the server.
fn send_highlights(engine: &mut Engine, rpc: &Rpc, map: &[(Value, Value)], buffer: u64) {
    let id = BufferId(buffer);
    if engine.language_of(id).is_none() {
        return;
    }
    let tick = map_u64(map, "tick");
    let first = map_u64(map, "first_line") as usize;
    let last = map_u64(map, "last_line") as usize;
    let spans: Vec<SpanWire> = engine
        .highlights(id, first, last)
        .iter()
        .map(span_to_wire)
        .collect();
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

/// The `ts_edit` wire delta as the engine's [`BufferEdit`] (identical fields —
/// the wire shape was modeled on it). The conversion lives here, at the process
/// boundary, so the engine never sees a wire type.
fn edit_from_wire(e: &EditWire) -> BufferEdit {
    BufferEdit {
        start_byte: e.start_byte,
        old_end_byte: e.old_end_byte,
        new_end_byte: e.new_end_byte,
        start_point: e.start_point,
        old_end_point: e.old_end_point,
        new_end_point: e.new_end_point,
        text: e.text.clone(),
    }
}

/// The engine's [`Span`] as the `ts_highlights` wire tuple (identical fields).
fn span_to_wire(s: &Span) -> SpanWire {
    SpanWire {
        line: s.line,
        start_byte: s.start_byte,
        end_byte: s.end_byte,
        group: s.group.clone(),
    }
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
