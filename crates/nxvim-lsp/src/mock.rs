//! A scripted mock language server for the test suite, the LSP analogue of the
//! syntax tests' fixture grammar.
//!
//! Reached only via `nxvim --__lsp-mock <script>` (a hidden, debug-only mode of
//! the `nxvim` binary). It speaks real LSP — `Content-Length` framing over
//! JSON-RPC 2.0 on stdio — but returns **scripted, deterministic** responses
//! from a JSON script file, and **records every notification it receives** to a
//! file the test reads back. This keeps the LSP tests hermetic and network-free,
//! exactly like `NXVIM_TS_WORKER` does for the syntax worker.
//!
//! Script fields (all optional):
//! - `record`: path to append received notifications to, one JSON object per
//!   line (`{"method": …, "params": …}`).
//! - `position_encoding`: `"utf-8"` (default) | `"utf-16"` | `"utf-32"` — the
//!   server's chosen `positionEncoding` capability.
//! - `sync_kind`: `"incremental"` (default) | `"full"` | `"none"` — the
//!   `textDocumentSync` capability.
//! - `exit_after_initialize`: if `true`, the mock replies to `initialize` then
//!   exits, to exercise the supervisor's respawn/breaker path.
//! - `diagnostics`: an array of LSP `Diagnostic` objects (`{range, severity,
//!   message}`). When set, the mock pushes a `textDocument/publishDiagnostics`
//!   notification for a document the moment it receives that document's
//!   `didOpen`, so a test can assert the editor renders them.
//! - `definition` / `declaration` / `type_definition` / `implementation` /
//!   `references`: the scripted result returned verbatim for the matching
//!   `textDocument/*` request (a `Location`, an array of `Location`s, or — for
//!   the goto family — a `LocationLink[]`; `references` is a `Location[]`).
//!   Absent ⇒ a `null` result (no locations).
//! - `hover`: the scripted `Hover` result (`{contents, range?}`, where
//!   `contents` is a `MarkupContent`, a `MarkedString`, or an array) returned for
//!   `textDocument/hover`. Absent ⇒ `null` (no hover).
//! - `signature_help`: the scripted `SignatureHelp` result (`{signatures,
//!   activeSignature?, activeParameter?}`) returned for
//!   `textDocument/signatureHelp`. Absent ⇒ `null` (no signature help).
//! - `completion`: the scripted `textDocument/completion` result — a
//!   `CompletionItem[]` or a `CompletionList` (`{isIncomplete, items}`) —
//!   returned for every completion request. Absent ⇒ `null` (no candidates).
//! - `completion_sequence`: an array of completion results consumed **one per
//!   `textDocument/completion` request** (overriding `completion` when present),
//!   so a test can return a broad `isIncomplete:true` list first and a narrowed
//!   list on the re-request, exercising the live re-request path. Past the end of
//!   the array ⇒ `null`.

use std::io::{BufRead, BufReader, Write};

use serde_json::{json, Value};

/// Run the mock over this process's stdio until the client closes the pipe (or
/// sends `exit`). Synchronous and self-contained: a dedicated process that does
/// nothing but answer one editor, so blocking stdio is simplest and correct.
pub fn run(script_path: &str) {
    let script: Value = std::fs::read_to_string(script_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();

    // How many `textDocument/completion` requests we've answered, so
    // `completion_sequence` can hand back a different list per request.
    let mut completion_calls = 0usize;

    while let Some(msg) = read_message(&mut reader) {
        // Record every client→server message (so tests can read back what the
        // client advertised at `initialize` and which notifications it sent).
        record(&script, &msg);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                if let Some(id) = id {
                    write_response(&stdout, id, initialize_result(&script));
                }
                if script
                    .get("exit_after_initialize")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return;
                }
            }
            "shutdown" => {
                if let Some(id) = id {
                    write_response(&stdout, id, Value::Null);
                }
            }
            "exit" => return,
            // On `didOpen`, push any scripted diagnostics for the just-opened
            // document so the editor has something to render (real servers
            // publish asynchronously after the open; the mock does it eagerly and
            // deterministically). The notification needs no reply.
            "textDocument/didOpen" => {
                if let Some(diagnostics) = script.get("diagnostics") {
                    if let Some(uri) = msg
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                    {
                        write_message(
                            &stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "textDocument/publishDiagnostics",
                                "params": { "uri": uri, "diagnostics": diagnostics },
                            }),
                        );
                    }
                }
            }
            // Language-feature requests: answer with the scripted result for the
            // matching script field (a Location / Location[] / LocationLink[]),
            // or `null` if the script doesn't define one.
            "textDocument/definition" => reply_scripted(&stdout, id, &script, "definition"),
            "textDocument/declaration" => reply_scripted(&stdout, id, &script, "declaration"),
            "textDocument/typeDefinition" => {
                reply_scripted(&stdout, id, &script, "type_definition")
            }
            "textDocument/implementation" => reply_scripted(&stdout, id, &script, "implementation"),
            "textDocument/references" => reply_scripted(&stdout, id, &script, "references"),
            "textDocument/hover" => reply_scripted(&stdout, id, &script, "hover"),
            "textDocument/signatureHelp" => reply_scripted(&stdout, id, &script, "signature_help"),
            // Completion: a `completion_sequence` entry (one per request) wins over
            // the single `completion` field, so a test can narrow the list on the
            // re-request triggered as the prefix grows.
            "textDocument/completion" => {
                if let Some(id) = id {
                    let result = completion_result(&script, &mut completion_calls);
                    write_response(&stdout, id, result);
                }
            }
            // Any other request must be answered or the client would wait forever;
            // notifications need no reply.
            _ => {
                if let Some(id) = id {
                    write_response(&stdout, id, Value::Null);
                }
            }
        }
    }
}

/// The scripted `InitializeResult` capabilities: chosen position encoding and
/// document-sync kind. `textDocumentSync` is the bare kind number
/// (`0`=none, `1`=full, `2`=incremental), which lsp-types parses as a
/// `TextDocumentSyncCapability::Kind`.
fn initialize_result(script: &Value) -> Value {
    let encoding = script
        .get("position_encoding")
        .and_then(Value::as_str)
        .unwrap_or("utf-8");
    let sync = script
        .get("sync_kind")
        .and_then(Value::as_str)
        .unwrap_or("incremental");
    let sync_kind = match sync {
        "none" => 0,
        "full" => 1,
        _ => 2,
    };
    json!({
        "capabilities": {
            "positionEncoding": encoding,
            "textDocumentSync": sync_kind,
        },
        "serverInfo": { "name": "nxvim-lsp-mock" }
    })
}

/// Append a received message (anything carrying a `method`) to the script's
/// `record` file as one JSON line.
fn record(script: &Value, msg: &Value) {
    let Some(path) = script.get("record").and_then(Value::as_str) else {
        return;
    };
    let Some(method) = msg.get("method") else {
        return;
    };
    let line = json!({ "method": method, "params": msg.get("params") });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// The completion result for the `call`-th `textDocument/completion` request
/// (0-based), advancing `call`. `completion_sequence[call]` wins when present
/// (`null` past its end); otherwise the single `completion` field is reused for
/// every request; `null` when neither is scripted.
fn completion_result(script: &Value, call: &mut usize) -> Value {
    let n = *call;
    *call += 1;
    if let Some(seq) = script.get("completion_sequence").and_then(Value::as_array) {
        return seq.get(n).cloned().unwrap_or(Value::Null);
    }
    script.get("completion").cloned().unwrap_or(Value::Null)
}

/// Answer a request with the script's `field` value (cloned), or `null` when the
/// field is absent. A no-op if the message carried no id (a malformed request).
fn reply_scripted(stdout: &std::io::Stdout, id: Option<Value>, script: &Value, field: &str) {
    if let Some(id) = id {
        let result = script.get(field).cloned().unwrap_or(Value::Null);
        write_response(stdout, id, result);
    }
}

/// Write a JSON-RPC response (`{jsonrpc, id, result}`) with `Content-Length`.
fn write_response(stdout: &std::io::Stdout, id: Value, result: Value) {
    write_message(
        stdout,
        &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

/// Frame and write one message: `Content-Length: N\r\n\r\n<body>`.
fn write_message(stdout: &std::io::Stdout, msg: &Value) {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = stdout.lock();
    let _ = write!(out, "Content-Length: {}\r\n\r\n", body.len());
    let _ = out.write_all(&body);
    let _ = out.flush();
}

/// Read one `Content-Length`-framed JSON message, or `None` at EOF.
fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // blank line ends the headers
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}
