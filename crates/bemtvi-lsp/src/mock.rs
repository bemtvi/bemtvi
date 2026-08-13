//! A scripted mock language server for the test suite, the LSP analogue of the
//! syntax tests' fixture grammar.
//!
//! Reached only via `bemtvi --__lsp-mock <script>` (a hidden, debug-only mode of
//! the `bemtvi` binary). It speaks real LSP — `Content-Length` framing over
//! JSON-RPC 2.0 on stdio — but returns **scripted, deterministic** responses
//! from a JSON script file, and **records every notification it receives** to a
//! file the test reads back. This keeps the LSP tests hermetic and network-free,
//! exactly like `BEMTVI_TS_WORKER` does for the syntax worker.
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
//! - `never_answer_initialize`: if `true`, the mock accepts the pipe and then blocks
//!   forever without answering `initialize` — a server that spawns but never speaks.
//!   Distinct from `exit_after_initialize`: the process stays ALIVE, so the usual
//!   death detection never fires and only the supervisor's `INIT_GRACE` gets it.
//! - `never_answer_shutdown`: if `true`, the mock serves normally but blocks forever
//!   on `shutdown`, so teardown has to fall back on `SHUTDOWN_GRACE` and kill it.
//! - `diagnostics`: an array of LSP `Diagnostic` objects (`{range, severity,
//!   message}`). When set, the mock pushes a `textDocument/publishDiagnostics`
//!   notification for a document the moment it receives that document's
//!   `didOpen`, so a test can assert the editor renders them.
//! - `diagnostics_on_change`: if `true`, the mock also republishes on every
//!   `textDocument/didChange` — one warning on the changed line reading
//!   `typed: <the change's text>`, so the published set is derived from the actual
//!   edit. This is the per-keystroke re-diagnosis a real server does, which is what
//!   `vim.diagnostic.config({ update_in_insert = … })` gates.
//! - `progress`: an array of `$/progress` params (`{token, value}`, where `value`
//!   is a `WorkDoneProgress` — `{kind: "begin", title, …}` / `{kind: "report", …}` /
//!   `{kind: "end", …}`). Replayed in order, once, on the first `didOpen`, so a test
//!   drives a real begin/report/end sequence deterministically: a script that stops
//!   at a `report` leaves the task *running* (observable via `btv.lsp.progress()`),
//!   one that includes its `end` leaves the store empty. Several tokens may be
//!   interleaved in the one array, as a real server does.
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
//!   ⇒ `null` (no actions). The list is **filtered by the request's
//!   `context.only`** (kind hierarchy: `source.fixAll` matches `source.fixAll.ruff`)
//!   the way a compliant server filters, so a test can tell whether the editor sent
//!   the filter at all.
//! - `code_action_ignore_only`: `true` scripts a **non-compliant** server that
//!   returns every scripted action even when the request carried `context.only` —
//!   the case the editor's own reply-side filter has to cover.
//! - `code_action_echo_only`: `true` replies with a single action whose *title* is
//!   the `context.only` the request carried (`only=[source.fixAll]`), so a test can
//!   read what actually went over the wire off the chooser.
//! - `code_action_echo_range`: `true` replies with a single action whose *title* is
//!   the request's `range` and the number of `context.diagnostics` it carried
//!   (`range=[0,0-1,2] diags=1`) — the range twin of `code_action_echo_only`, so a
//!   test can read the selection the editor actually sent off the chooser.
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
//! - `inlay_hints`: the `InlayHint[]` returned for `textDocument/inlayHint`. When
//!   set, the mock advertises the `inlayHintProvider` capability (so the editor
//!   requests them; `resolveProvider` too when `inlay_resolve` is scripted). Absent
//!   ⇒ no provider, and the request falls back to `null`.
//! - `inlay_resolve`: the resolved `InlayHint` (its lazy `label` filled in)
//!   returned for `inlayHint/resolve`. Absent ⇒ `null`.
//! - `inlay_refresh`: when set, the FIRST `textDocument/inlayHint` request returns
//!   empty and the mock then sends a server→client `workspace/inlayHint/refresh`
//!   (the lua_ls "nothing ready yet — ask again" shape); later requests return
//!   `inlay_hints`. Proves the editor honors the refresh by re-querying.
//! - `config_pull`: an array of section names. After the client's `initialized`,
//!   the mock sends a server→client `workspace/configuration` request for those
//!   sections (the pull-config model lua_ls/gopls use) and records the editor's
//!   reply under the synthetic method `_config_response`, so a test can assert the
//!   client answered each section from the config's `settings`.
//! - `workspace_folders_pull`: if `true`, the mock sends a server→client
//!   `workspace/workspaceFolders` request after `initialized` (the pull a server
//!   that ignores the deprecated `rootUri` uses to find its workspace) and records
//!   the editor's reply under `_workspace_folders_response`.
//! - `register_capability`: an array of `Registration` objects
//!   (`{id, method, registerOptions}`) the mock registers dynamically via a
//!   server→client `client/registerCapability` right after `initialized` — what
//!   ruff/lua_ls/gopls do for `workspace/didChangeWatchedFiles`. The editor's answer
//!   is recorded under `_register_response` (`{result: …}` or `{error: …}`), so a
//!   test can tell an ack from the method-not-found a server reads as "this client
//!   cannot do dynamic registration".
//! - `unregister_after_watch_events`: after this many
//!   `workspace/didChangeWatchedFiles` notifications from the client, the mock sends
//!   `client/unregisterCapability` for everything in `register_capability` — the
//!   teardown half of the watch lifecycle.
//! - `apply_edit`: a `WorkspaceEdit` the mock pushes back as a server→client
//!   `workspace/applyEdit` when it receives a `workspace/executeCommand` — gopls's
//!   exact shape for a refactor delivered as a `command` (the command's own reply is
//!   `null`; the edit arrives as this request). The editor's response is recorded
//!   under the synthetic method `_apply_edit_response`, so a test can assert we
//!   answered `{"applied": true}` rather than the method-not-found this used to be.
//! - `apply_edit_by_command`: a `{ command: WorkspaceEdit }` map (overriding
//!   `apply_edit` when it names the executed command) so one session can drive several
//!   *different* server-initiated edits — a text edit, a `rename`, a `delete` — each
//!   through its own code action. Deterministic where a consumed-in-order list is not:
//!   re-running an action pushes the same edit again.
//! - `custom_replies`: a `{ method: result }` map scripting the reply to an
//!   otherwise-unhandled request — the generic `client:request` path (Phase 5).
//!   A method not in the map falls back to a `null` result.
//! - `reply_delay_ms`: milliseconds the mock sleeps before sending each scripted
//!   request *reply* (definition/hover/formatting/…), so a test can edit the
//!   buffer before the reply lands and prove the editor's stale-drop (e.g. the
//!   formatting content-version guard). `0`/absent ⇒ no delay.
//! - `stderr_noise`: bytes of **non-UTF-8** junk (`0xFF` lines) written to stderr
//!   before the mock starts serving, modeling a server whose stderr is binary /
//!   invalid UTF-8 (raw panic dumps, binary logging). Sized past the pipe
//!   capacity, this write only completes if the client keeps *draining* stderr
//!   through the invalid bytes; a stderr write failure kills the mock, exactly as
//!   it kills a real server (SIGPIPE for a C server, an `eprintln!` panic for a
//!   Rust one) when the client abandons the read end.

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

    // `stderr_noise`: flood stderr with invalid-UTF-8 lines *before* serving (see
    // the module doc). A write failure means the client abandoned the read end —
    // die, exactly as a real server does (SIGPIPE / `eprintln!` panic); the
    // fatality is the point, so a client that stops draining is caught by the
    // test rather than the mock quietly swallowing the broken pipe (Rust ignores
    // SIGPIPE, so `write_all` reports it as an error instead).
    if let Some(n) = script.get("stderr_noise").and_then(Value::as_u64) {
        let mut err = std::io::stderr().lock();
        let mut line = vec![0xFFu8; 4095];
        line.push(b'\n');
        let mut written = 0u64;
        while written < n {
            if err.write_all(&line).is_err() {
                std::process::exit(1);
            }
            written += line.len() as u64;
        }
        let _ = err.flush();
    }

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();

    // How many `textDocument/completion` requests we've answered, so
    // `completion_sequence` can hand back a different list per request.
    let mut completion_calls = 0usize;
    // How many `textDocument/inlayHint` requests we've answered, so an
    // `inlay_refresh` script can return empty on the first and hints after the
    // server→client refresh (the lua_ls "compute asynchronously" shape).
    let mut inlay_calls = 0usize;
    // Ids for the server→client requests the mock originates (config pull, refresh).
    let mut next_id = 10_000i64;
    // The id of the `workspace/configuration` pull we sent, so its response can be
    // recorded for the test to assert on.
    let mut config_req_id: Option<i64> = None;
    // Likewise for the `workspace/workspaceFolders` pull (`workspace_folders_pull`)
    // and the `client/registerCapability` (`register_capability`): a real server reads
    // both answers, so recording them is what makes "did the client actually answer?"
    // observable rather than assumed.
    let mut folders_req_id: Option<i64> = None;
    let mut register_req_id: Option<i64> = None;
    // How many `workspace/didChangeWatchedFiles` notifications the client has sent, so
    // `unregister_after_watch_events` can retire the registration on the Nth.
    let mut watch_events = 0u64;
    // Likewise for the `workspace/applyEdit` an `apply_edit` script pushes back: its
    // response is the editor's `{applied, failureReason?}`, which a test reads to
    // prove the edit was really applied (not merely acked).
    let mut apply_req_id: Option<i64> = None;
    // Whether the `progress` script has been replayed (once per session, on the first
    // `didOpen`) — see the `textDocument/didOpen` arm.
    let mut progress_sent = false;
    // The `window/workDoneProgress/create` requests still awaiting the client's answer,
    // as request id → token, and the tokens whose create the client REFUSED. A real
    // server reads that answer: gopls sends no `$/progress` for a token the client
    // would not create, so the mock doesn't either — that is what makes "did the editor
    // ack the create?" observable rather than assumed.
    let mut progress_creates: std::collections::HashMap<i64, String> =
        std::collections::HashMap::new();
    let mut progress_denied: std::collections::HashSet<String> = std::collections::HashSet::new();
    // The scripted `$/progress` payloads, held until every create is answered.
    let mut progress_queue: Vec<Value> = Vec::new();

    while let Some(msg) = read_message(&mut reader) {
        // A response to one of our server→client requests (it has an `id` but no
        // `method`): capture the config-pull / applyEdit answers for tests, then
        // ignore — never reply to a reply.
        if msg.get("method").is_none() {
            let rid = msg.get("id").and_then(Value::as_i64);
            if rid.is_some() && rid == config_req_id {
                append_record(&script, "_config_response", msg.get("result"));
            }
            if rid.is_some() && rid == apply_req_id {
                append_record(&script, "_apply_edit_response", msg.get("result"));
            }
            if rid.is_some() && rid == folders_req_id {
                append_record(&script, "_workspace_folders_response", msg.get("result"));
            }
            // A registration the client ERRORED (async-lsp's method-not-found default,
            // before `client/registerCapability` was answered) is recorded as such: a
            // real server reads that as "no dynamic registration", which is exactly the
            // conclusion the watch feature dies on.
            if rid.is_some() && rid == register_req_id {
                let outcome = match msg.get("error") {
                    Some(e) => json!({ "error": e }),
                    None => json!({ "result": msg.get("result").cloned().unwrap_or(Value::Null) }),
                };
                append_record(&script, "_register_response", Some(&outcome));
            }
            // The answer to a `window/workDoneProgress/create`. An `error` reply (what
            // async-lsp's method-not-found default produces for an unmodelled request)
            // means the client can't do progress for that token, so its updates are
            // dropped — the same conclusion gopls draws.
            if let Some(token) = rid.and_then(|r| progress_creates.remove(&r)) {
                if msg.get("error").is_some() {
                    progress_denied.insert(token);
                }
                if progress_creates.is_empty() && !progress_queue.is_empty() {
                    for params in std::mem::take(&mut progress_queue) {
                        let token = progress_token_key(params.get("token"));
                        if progress_denied.contains(&token) {
                            continue;
                        }
                        write_message(
                            &stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "$/progress",
                                "params": params,
                            }),
                        );
                    }
                }
            }
            continue;
        }
        // Record every client→server message (so tests can read back what the
        // client advertised at `initialize` and which notifications it sent).
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        append_record(&script, method, msg.get("params"));
        let id = msg.get("id").cloned();
        match method {
            "initialize" => {
                // A server that accepts the pipe and then never speaks — the "wedged
                // on startup" case the supervisor's `INIT_GRACE` exists for. It must
                // keep the process alive (a server that EXITS is detected by the
                // ordinary death path); only the answer is withheld.
                if script
                    .get("never_answer_initialize")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
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
                // A server that stops answering at teardown — the case
                // `SHUTDOWN_GRACE` bounds. It must stay alive and ignore `exit` too,
                // or the ordinary teardown completes and the grace is never reached.
                if script
                    .get("never_answer_shutdown")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
                if let Some(id) = id {
                    write_response(&stdout, id, Value::Null);
                }
            }
            "exit" => return,
            // After the client says it's `initialized`, a `config_pull` script makes
            // the mock pull its config the way lua_ls/gopls do: send a
            // `workspace/configuration` request for the scripted sections. The
            // client's response is recorded (`_config_response`) for the test, and if
            // it carries a truthy `hint.enable` we know the editor answered the pull
            // from the config's `settings`.
            "initialized" => {
                if let Some(sections) = script.get("config_pull").and_then(Value::as_array) {
                    let items: Vec<Value> =
                        sections.iter().map(|s| json!({ "section": s })).collect();
                    config_req_id = Some(next_id);
                    write_message(
                        &stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": next_id,
                            "method": "workspace/configuration",
                            "params": { "items": items },
                        }),
                    );
                    next_id += 1;
                }
                // `workspace_folders_pull`: pull the folder set the way a server that
                // trusts neither `rootUri` nor the pushed `workspaceFolders` does. The
                // client's answer is recorded as `_workspace_folders_response`.
                if script
                    .get("workspace_folders_pull")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    folders_req_id = Some(next_id);
                    write_message(
                        &stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": next_id,
                            "method": "workspace/workspaceFolders",
                            "params": Value::Null,
                        }),
                    );
                    next_id += 1;
                }
                // `register_capability`: register a capability dynamically, as ruff /
                // lua_ls / gopls do for `workspace/didChangeWatchedFiles` right after
                // `initialized`. The script IS the `registrations` array, so a test can
                // register several watchers (or an unhandled method) verbatim.
                if let Some(regs) = script.get("register_capability").and_then(Value::as_array) {
                    register_req_id = Some(next_id);
                    write_message(
                        &stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": next_id,
                            "method": "client/registerCapability",
                            "params": { "registrations": regs },
                        }),
                    );
                    next_id += 1;
                }
            }
            // The client reporting a watched file changed. With
            // `unregister_after_watch_events` scripted, the Nth one makes the mock
            // retire its registrations (`client/unregisterCapability`) — the way a
            // server drops a watch when its config changes — so a test can prove the
            // client really tears the watch down instead of leaking it for the session.
            "workspace/didChangeWatchedFiles" => {
                watch_events += 1;
                let after = script
                    .get("unregister_after_watch_events")
                    .and_then(Value::as_u64);
                if after == Some(watch_events) {
                    let ids: Vec<Value> = script
                        .get("register_capability")
                        .and_then(Value::as_array)
                        .map(|regs| {
                            regs.iter()
                                .filter_map(|r| {
                                    Some(json!({ "id": r.get("id")?, "method": r.get("method")? }))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    write_message(
                        &stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": next_id,
                            "method": "client/unregisterCapability",
                            "params": { "unregisterations": ids },
                        }),
                    );
                    next_id += 1;
                }
            }
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
                // `progress`: replay the scripted `$/progress` sequence, in order, on
                // the FIRST document opened. Ordered and eager (rather than timed) so
                // a test observes one deterministic end state: a script that stops at
                // a `report` leaves the task running and observable, and one that
                // includes its `end` leaves the store empty. Emitted only once — a
                // second buffer opening must not restart a finished task.
                if !progress_sent {
                    if let Some(items) = script.get("progress").and_then(Value::as_array) {
                        progress_sent = true;
                        // Ask permission per distinct token FIRST, exactly as gopls
                        // does, and hold the updates until the client has answered
                        // every one. A client that errors a create gets nothing.
                        // Keyed by the token's canonical spelling, but the create
                        // carries the token VERBATIM: the wire type is a
                        // `NumberOrString`, and a mock that stringified a numeric
                        // token would ask permission for a token it then never
                        // reports under, so a numeric-token test would exercise a
                        // shape no real server sends.
                        let mut tokens: Vec<(String, Value)> = Vec::new();
                        for params in items {
                            let raw = params.get("token").cloned().unwrap_or(Value::Null);
                            let token = progress_token_key(Some(&raw));
                            if !tokens.iter().any(|(k, _)| *k == token) {
                                tokens.push((token, raw));
                            }
                        }
                        progress_queue = items.clone();
                        for (token, raw) in tokens {
                            write_message(
                                &stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": next_id,
                                    "method": "window/workDoneProgress/create",
                                    "params": { "token": raw },
                                }),
                            );
                            progress_creates.insert(next_id, token);
                            next_id += 1;
                        }
                    }
                }
            }
            // `diagnostics_on_change`: re-diagnose the document on every edit, the
            // way a real server does after each `didChange` — the behavior
            // `update_in_insert` exists to tame. The published set is derived from
            // the change itself (its line, and the text that was typed), so a test
            // can tell *which* keystroke a rendered diagnostic came from rather than
            // only that some diagnostic exists. Replaces the whole set, as a real
            // `publishDiagnostics` does.
            "textDocument/didChange" => {
                if script
                    .get("diagnostics_on_change")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    if let Some(uri) = msg
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                    {
                        let change = msg
                            .pointer("/params/contentChanges")
                            .and_then(Value::as_array)
                            .and_then(|c| c.last());
                        // A range-less change is a full-document sync: report it on
                        // line 0, since it names no line of its own.
                        let line = change
                            .and_then(|c| c.pointer("/range/start/line"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let text = change
                            .and_then(|c| c.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        write_message(
                            &stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "method": "textDocument/publishDiagnostics",
                                "params": { "uri": uri, "diagnostics": [{
                                    "range": { "start": { "line": line, "character": 0 },
                                               "end":   { "line": line, "character": 1 } },
                                    "severity": 2,
                                    "message": format!("typed: {text}"),
                                }] },
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
            // Symbols: the scripted `DocumentSymbol[]`/`SymbolInformation[]` for the
            // document, and the `SymbolInformation[]`/`WorkspaceSymbol[]` matching a
            // `workspace/symbol` query. Absent ⇒ `null` (no symbols).
            "textDocument/documentSymbol" => {
                reply_scripted(&stdout, id, &script, "document_symbols")
            }
            "workspace/symbol" => reply_scripted(&stdout, id, &script, "workspace_symbols"),
            "textDocument/hover" => reply_scripted(&stdout, id, &script, "hover"),
            "textDocument/signatureHelp" => reply_scripted(&stdout, id, &script, "signature_help"),
            "textDocument/formatting" => reply_scripted(&stdout, id, &script, "formatting"),
            "textDocument/rename" => reply_scripted(&stdout, id, &script, "rename"),
            // Code actions: a REAL server honors the request's `context.only`, so the
            // mock does too — the scripted `code_action` list is filtered by the kinds
            // the editor asked for (LSP kind hierarchy: `source.fixAll` matches
            // `source.fixAll.ruff`). That makes "did the editor send `only`?" observable.
            // `code_action_ignore_only` scripts the *non-compliant* server that returns
            // everything regardless — the case the editor's own filter has to cover.
            "textDocument/codeAction" => {
                if let Some(id) = id {
                    let actions = script.get("code_action").cloned().unwrap_or(Value::Null);
                    let only = msg
                        .pointer("/params/context/only")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let ignore_only = script
                        .get("code_action_ignore_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // `code_action_echo_only`: reply with a single action whose TITLE is
                    // the `context.only` the request carried, so a test can read the
                    // wire off the chooser (the kinds are echoed as the action's kind
                    // too, so it survives the editor's own filter).
                    if script
                        .get("code_action_echo_only")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        let kinds: Vec<&str> = only.iter().filter_map(Value::as_str).collect();
                        write_response(
                            &stdout,
                            id,
                            json!([{
                                "title": format!("only=[{}]", kinds.join(",")),
                                "kind": kinds.first().copied().unwrap_or("quickfix"),
                            }]),
                        );
                        continue;
                    }
                    // `code_action_echo_range`: reply with a single action whose TITLE
                    // is the request's `range` plus how many `context.diagnostics` rode
                    // with it, so a test can read the selection off the chooser.
                    if script
                        .get("code_action_echo_range")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        let at =
                            |p: &str| msg.pointer(p).and_then(Value::as_u64).unwrap_or(u64::MAX);
                        let diags = msg
                            .pointer("/params/context/diagnostics")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        write_response(
                            &stdout,
                            id,
                            json!([{
                                "title": format!(
                                    "range=[{},{}-{},{}] diags={diags}",
                                    at("/params/range/start/line"),
                                    at("/params/range/start/character"),
                                    at("/params/range/end/line"),
                                    at("/params/range/end/character"),
                                ),
                                "kind": "quickfix",
                            }]),
                        );
                        continue;
                    }
                    let result = match (&actions, only.is_empty() || ignore_only) {
                        (Value::Array(list), false) => Value::Array(
                            list.iter()
                                .filter(|a| {
                                    let kind = a.get("kind").and_then(Value::as_str);
                                    only.iter().any(|o| {
                                        o.as_str().zip(kind).is_some_and(|(o, k)| {
                                            k == o || k.starts_with(&format!("{o}."))
                                        })
                                    })
                                })
                                .cloned()
                                .collect(),
                        ),
                        _ => actions,
                    };
                    write_response(&stdout, id, result);
                }
            }
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
            // Inlay hints: return the scripted `InlayHint[]` for the whole document.
            // Absent ⇒ `null` (no hints). With `inlay_refresh` set, the FIRST request
            // returns empty and the mock then sends a `workspace/inlayHint/refresh`
            // (the lua_ls "nothing ready yet, ask again" shape); later requests return
            // the scripted hints — so the test proves the editor honors the refresh.
            "textDocument/inlayHint" => {
                let call = inlay_calls;
                inlay_calls += 1;
                if script.get("inlay_refresh").is_some() && call == 0 {
                    if let Some(id) = id {
                        write_response(&stdout, id, json!([]));
                    }
                    write_message(
                        &stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": next_id,
                            "method": "workspace/inlayHint/refresh",
                            "params": null,
                        }),
                    );
                    next_id += 1;
                } else {
                    reply_scripted(&stdout, id, &script, "inlay_hints");
                }
            }
            // The resolved `InlayHint` (its lazy `label` filled in) for a hint the
            // editor sent to `inlayHint/resolve`. Absent ⇒ `null`, which can't
            // deserialize into an `InlayHint` — exercising the editor's
            // resolve-failure path (logged, placeholder dropped).
            "inlayHint/resolve" => reply_scripted(&stdout, id, &script, "inlay_resolve"),
            // Folding ranges: return the scripted `FoldingRange[]` for the whole
            // document. Absent ⇒ `null` (no folds).
            "textDocument/foldingRange" => reply_scripted(&stdout, id, &script, "folding_ranges"),
            // A refactor delivered as a `command`: the reply carries nothing, and the
            // edit comes back as a server→client `workspace/applyEdit` — the shape
            // gopls uses for `gopls.extract_to_new_file`. Send the request FIRST, so
            // the editor is applying while the command reply is still in flight
            // (exactly the interleaving a real server produces).
            "workspace/executeCommand" => {
                // The edit this command pushes back: the per-command map when it names
                // this command, else the single scripted `apply_edit`.
                let command = msg
                    .pointer("/params/command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let edit = script
                    .get("apply_edit_by_command")
                    .and_then(|m| m.get(command))
                    .or_else(|| script.get("apply_edit"));
                if let Some(edit) = edit {
                    apply_req_id = Some(next_id);
                    write_message(
                        &stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": next_id,
                            "method": "workspace/applyEdit",
                            "params": { "label": "mock refactor", "edit": edit },
                        }),
                    );
                    next_id += 1;
                }
                if let Some(id) = id {
                    write_response(&stdout, id, Value::Null);
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

/// The mock's own bookkeeping key for a scripted `$/progress` token — the same
/// decimal-spelling normalization the editor's `progress_token` applies, so a script
/// may mint either half of the wire's `NumberOrString` and the "which token did the
/// client refuse?" tracking still tells two numeric tokens apart. A token the script
/// omitted keys as `""`.
fn progress_token_key(token: Option<&Value>) -> String {
    match token {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
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
    // Advertise every feature provider bemtvi implements, so a config's `on_attach`
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
        "signatureHelpProvider": { "triggerCharacters": ["(", ","] },
        "completionProvider": {},
        "documentFormattingProvider": true,
        "renameProvider": true,
        "codeActionProvider": true,
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
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
    // Advertise the inlay-hint provider only when the script supplies `inlay_hints`
    // (so a test without it exercises a server that doesn't offer the feature). When
    // the script also scripts an `inlay_resolve` reply, advertise
    // `resolveProvider: true` so the editor knows it can resolve lazy hints.
    if script.get("inlay_hints").is_some() {
        if let Value::Object(base) = &mut capabilities {
            let provider = if script.get("inlay_resolve").is_some() {
                json!({ "resolveProvider": true })
            } else {
                json!(true)
            };
            base.insert("inlayHintProvider".to_string(), provider);
        }
    }
    // Advertise the folding-range provider only when the script supplies
    // `folding_ranges` (so a test without it exercises a server that doesn't offer
    // the feature, leaving the buffer unfolded).
    if script.get("folding_ranges").is_some() {
        if let Value::Object(base) = &mut capabilities {
            base.insert("foldingRangeProvider".to_string(), json!(true));
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
        "serverInfo": { "name": "bemtvi-lsp-mock" }
    })
}

/// Append one `{method, params}` JSON line to the script's `record` file (when
/// scripted). Called with each received message's own method, and with a
/// synthetic method (`_config_response`) to capture the *response* to a
/// server→client request the mock originated (e.g. the editor's
/// `workspace/configuration` answer), which carries no method of its own.
fn append_record(script: &Value, method: &str, params: Option<&Value>) {
    let Some(path) = script.get("record").and_then(Value::as_str) else {
        return;
    };
    let line = json!({ "method": method, "params": params });
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
