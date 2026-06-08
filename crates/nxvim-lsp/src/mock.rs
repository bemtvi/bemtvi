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
//! - `formatting`: the `TextEdit[]` returned for `textDocument/formatting`.
//! - `rename`: the `WorkspaceEdit` returned for `textDocument/rename` (either a
//!   `{changes}` map or `{documentChanges}`).
//! - `code_action`: the `(CodeAction | Command)[]` returned for
//!   `textDocument/codeAction` (tests script `CodeAction`s carrying an eager
//!   `edit`, or lazy ones with only `data` to drive `codeAction/resolve`). Absent
//!   ⇒ `null` (no actions).
//! - `code_action_resolve`: the resolved `CodeAction` (with its `edit` filled in)
//!   returned for `codeAction/resolve`. Absent ⇒ `null`.
//! - `completion_resolve`: the resolved `CompletionItem` (its lazy
//!   `documentation`/`detail` filled in) returned for `completionItem/resolve`.
//!   Absent ⇒ `null` (which fails to deserialize, exercising the resolve-failure
//!   path: logged, the item stays docless).
//! - `semantic_tokens`: scripts `textDocument/semanticTokens/full` and
//!   `full/delta`. An object `{ legend: { tokenTypes, tokenModifiers },
//!   data: [..u32..], result_id?, delta? }` — the `legend` is advertised as the
//!   `semanticTokensProvider` capability (so the editor decodes against it) and the
//!   `data`/`result_id` are returned for every `semanticTokens/full` request. When
//!   the editor sends `full/delta` (it does once it has cached a `result_id`), the
//!   `delta` field scripts the reply: `{ result_id?, edits: [{ start, deleteCount,
//!   data }] }` returns those edits, or `{ data: [..u32..], result_id? }` returns a
//!   fresh full set (the server's transparent fallback). Absent `delta` ⇒ a
//!   `full/delta` request is answered with the full `data`/`result_id` (the same
//!   full-set fallback). Absent `semantic_tokens` ⇒ the server advertises no
//!   semantic-tokens provider and the request falls back to `null`.
//! - `custom_replies`: a `{ method: result }` map scripting the reply to an
//!   otherwise-unhandled request — the generic `client:request` path (Phase 5).
//!   A method not in the map falls back to a `null` result.
//! - `reply_delay_ms`: milliseconds the mock sleeps before sending each scripted
//!   request *reply* (definition/hover/formatting/…), so a test can edit the
//!   buffer before the reply lands and prove the editor's stale-drop (e.g. the
//!   formatting content-version guard). `0`/absent ⇒ no delay.

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
            "textDocument/formatting" => reply_scripted(&stdout, id, &script, "formatting"),
            "textDocument/rename" => reply_scripted(&stdout, id, &script, "rename"),
            "textDocument/codeAction" => reply_scripted(&stdout, id, &script, "code_action"),
            "codeAction/resolve" => reply_scripted(&stdout, id, &script, "code_action_resolve"),
            // Completion: a `completion_sequence` entry (one per request) wins over
            // the single `completion` field, so a test can narrow the list on the
            // re-request triggered as the prefix grows.
            "textDocument/completion" => {
                if let Some(id) = id {
                    let result = completion_result(&script, &mut completion_calls);
                    write_response(&stdout, id, result);
                }
            }
            // The resolved `CompletionItem` (its lazy `documentation`/`detail`
            // filled in) for the item the editor selected. Absent ⇒ `null`, which
            // can't deserialize into a `CompletionItem` — exercising the editor's
            // resolve-failure path (logged, item left docless).
            "completionItem/resolve" => reply_scripted(&stdout, id, &script, "completion_resolve"),
            // Semantic tokens: return the scripted packed `data` (+ optional
            // `result_id`) for the whole document. Absent ⇒ `null` (no tokens).
            "textDocument/semanticTokens/full" => {
                if let Some(id) = id {
                    let result = semantic_full_result(&script);
                    write_response(&stdout, id, result);
                }
            }
            // A `full/delta` refresh (the editor sends it once it has cached a
            // `result_id`): return the scripted `delta` (edits, or a fresh full
            // set), falling back to the full set when no `delta` is scripted.
            "textDocument/semanticTokens/full/delta" => {
                if let Some(id) = id {
                    let result = semantic_delta_result(&script);
                    write_response(&stdout, id, result);
                }
            }
            // Any other request must be answered or the client would wait forever;
            // notifications need no reply. A `custom_replies` map (method ->
            // result) scripts the answer to a generic `client:request` (Phase 5);
            // an unscripted method falls back to `null`.
            _ => {
                if let Some(id) = id {
                    let result = script
                        .get("custom_replies")
                        .and_then(|m| m.get(method))
                        .cloned()
                        .unwrap_or(Value::Null);
                    write_response(&stdout, id, result);
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
    // Advertise every feature provider nxvim implements, so a config's `on_attach`
    // can read `client.server_capabilities.*Provider` (Phase 7b Slice 3). A script
    // may override the whole `capabilities` object to test a server that withholds
    // some providers.
    let mut capabilities = json!({
        "positionEncoding": encoding,
        "textDocumentSync": sync_kind,
        "definitionProvider": true,
        "declarationProvider": true,
        "typeDefinitionProvider": true,
        "implementationProvider": true,
        "referencesProvider": true,
        "hoverProvider": true,
        "signatureHelpProvider": {},
        "completionProvider": {},
        "documentFormattingProvider": true,
        "renameProvider": true,
        "codeActionProvider": true,
    });
    // Advertise the semantic-tokens provider only when the script supplies a
    // legend (so a test without `semantic_tokens` exercises a server that doesn't
    // offer the feature). When the script also scripts a `delta` reply, advertise
    // `full: { delta: true }` so the editor sends `full/delta`; otherwise a plain
    // `full: true`.
    if let Some(legend) = script.pointer("/semantic_tokens/legend") {
        if let Value::Object(base) = &mut capabilities {
            let full = if script.pointer("/semantic_tokens/delta").is_some() {
                json!({ "delta": true })
            } else {
                json!(true)
            };
            base.insert(
                "semanticTokensProvider".to_string(),
                json!({ "legend": legend, "full": full }),
            );
        }
    }
    if let Some(Value::Object(overrides)) = script.get("capabilities") {
        if let Value::Object(base) = &mut capabilities {
            for (k, v) in overrides {
                base.insert(k.clone(), v.clone());
            }
        }
    }
    json!({
        "capabilities": capabilities,
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

/// The `semanticTokens/full` reply: the scripted packed `data` (+ optional
/// `resultId`), or `null` when no `semantic_tokens` is scripted.
fn semantic_full_result(script: &Value) -> Value {
    let Some(st) = script.get("semantic_tokens") else {
        return Value::Null;
    };
    let mut out = json!({ "data": st.get("data").cloned().unwrap_or(json!([])) });
    if let Some(rid) = st.get("result_id") {
        out["resultId"] = rid.clone();
    }
    out
}

/// The `semanticTokens/full/delta` reply: the scripted `delta` — either an
/// `{ edits, resultId? }` diff or a `{ data, resultId? }` fresh full set — and the
/// full set as the fallback when no `delta` is scripted (the server answering a
/// delta request with a full result).
fn semantic_delta_result(script: &Value) -> Value {
    let Some(st) = script.get("semantic_tokens") else {
        return Value::Null;
    };
    let Some(delta) = st.get("delta") else {
        return semantic_full_result(script);
    };
    let mut out = json!({});
    if let Some(edits) = delta.get("edits") {
        out["edits"] = edits.clone();
    } else if let Some(data) = delta.get("data") {
        out["data"] = data.clone();
    }
    if let Some(rid) = delta.get("result_id") {
        out["resultId"] = rid.clone();
    }
    out
}

/// Answer a request with the script's `field` value (cloned), or `null` when the
/// field is absent. A no-op if the message carried no id (a malformed request).
/// Honors `reply_delay_ms`: sleeps that long before replying, so a test can edit
/// the buffer before the reply lands (exercising the editor's stale-drop).
fn reply_scripted(stdout: &std::io::Stdout, id: Option<Value>, script: &Value, field: &str) {
    if let Some(id) = id {
        if let Some(ms) = script.get("reply_delay_ms").and_then(Value::as_u64) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
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
