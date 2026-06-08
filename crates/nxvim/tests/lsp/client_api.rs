//! LSP Phase 5: generic `client:request` / `client:notify` and `execute_command`.
//!
//! `client:request(method, params, handler)` issues a real, arbitrary-method LSP
//! request and routes the raw JSON reply back to the Lua handler off-tick; an
//! unsupported method fails loud (the handler's `err` is set). `client:notify`
//! fires a generic notification. These are the seam server-specific commands
//! (`:LspCargoReload`, `switchSourceHeader`) and config `handlers` build on.

use crate::support::*;

/// Poll a global Lua boolean flag until it is set, or panic after a bounded wait.
/// Mirrors the `_G.nxvim_on_exit_*` polling the on_exit test uses for off-tick
/// effects.
async fn wait_for_global_flag(rpc: &Rpc, flag: &str) {
    for _ in 0..60 {
        if exec_lua(rpc, &format!("return {flag}")).await.as_bool() == Some(true) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {flag} to be set");
}

#[tokio::test]
async fn client_request_round_trips_a_custom_method() {
    let _guard = test_lock().lock().await;
    // `on_attach` issues a `workspace/executeCommand` via `client:request`; the
    // mock answers it from `custom_replies`, and the handler — fired off-tick when
    // the reply lands — stashes the result in globals the test reads back. This
    // proves the full round-trip: the request reaches the server with its params,
    // and the server's JSON result reaches the Lua handler.
    let file = temp_file("cr-custom", "rs", "fn main() {}\n");
    let record = configure_mock(
        "cr-custom",
        serde_json::json!({
            "custom_replies": {
                "workspace/executeCommand": { "ok": true, "echoed": "hi" }
            }
        }),
    );
    let cfg = attach_config_dir(
        "cr-custom",
        "client:request('workspace/executeCommand', { command = 'mock.run' }, \
           function(err, result) \
             _G.nxvim_cr_err = err \
             _G.nxvim_cr_ok = result and result.ok \
             _G.nxvim_cr_echoed = result and result.echoed \
             _G.nxvim_cr_done = true \
           end)",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    wait_for_global_flag(&rpc, "_G.nxvim_cr_done").await;

    // The result reached the handler...
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_cr_ok").await.as_bool(),
        Some(true),
        "the server's JSON result reaches the client:request handler"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_cr_echoed").await.as_str(),
        Some("hi"),
        "the full result table is handed to the handler"
    );
    // ...with no error on a successful reply.
    assert!(
        exec_lua(&rpc, "return _G.nxvim_cr_err").await.is_nil(),
        "a successful reply hands the handler a nil err"
    );
    // ...and the request actually went out with its params.
    let recs = record_lines(&record);
    let req = find(&recs, "workspace/executeCommand").expect("the request reached the server");
    assert_eq!(
        req["params"]["command"].as_str(),
        Some("mock.run"),
        "client:request forwards the params verbatim: {req:?}"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

#[tokio::test]
async fn client_request_unsupported_method_fails_loud() {
    let _guard = test_lock().lock().await;
    // A method not in the dispatch table must not silently no-op: the handler's
    // `err` is set with a message naming the unsupported method, and the bogus
    // request never reaches the server.
    let file = temp_file("cr-bad", "rs", "fn main() {}\n");
    let record = configure_mock("cr-bad", serde_json::json!({}));
    let cfg = attach_config_dir(
        "cr-bad",
        "client:request('nxvim/totally-made-up', {}, \
           function(err, result) \
             _G.nxvim_bad_err = err \
             _G.nxvim_bad_done = true \
           end)",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    wait_for_global_flag(&rpc, "_G.nxvim_bad_done").await;

    let err = exec_lua(&rpc, "return _G.nxvim_bad_err").await;
    let err = err
        .as_str()
        .expect("an unsupported method hands the handler an err");
    assert!(
        err.contains("unsupported method") && err.contains("nxvim/totally-made-up"),
        "the error names the unsupported method (fail loud): {err:?}"
    );
    assert!(
        !has_method(&record_lines(&record), "nxvim/totally-made-up"),
        "an unsupported request is rejected before it reaches the server"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

#[tokio::test]
async fn client_notify_reaches_the_server() {
    let _guard = test_lock().lock().await;
    // `client:notify` fires a generic notification fire-and-forget; the mock
    // records it, so we can assert it arrived with its params.
    let file = temp_file("cn", "rs", "fn main() {}\n");
    let record = configure_mock("cn", serde_json::json!({}));
    let cfg = attach_config_dir("cn", "client:notify('$/setTrace', { value = 'verbose' })");
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "$/setTrace")).await;
    let note = find(&recs, "$/setTrace").expect("the notification reached the server");
    assert_eq!(
        note["params"]["value"].as_str(),
        Some("verbose"),
        "client:notify forwards the params verbatim: {note:?}"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

#[tokio::test]
async fn execute_command_relays_to_the_server() {
    let _guard = test_lock().lock().await;
    // With no client-side `vim.lsp.commands` handler registered,
    // `vim.lsp.buf.execute_command` relays the command to the server as a
    // `workspace/executeCommand` request — the mock records it with its params.
    let file = temp_file("xc-relay", "rs", "fn main() {}\n");
    let record = configure_mock(
        "xc-relay",
        serde_json::json!({
            "custom_replies": { "workspace/executeCommand": { "applied": true } }
        }),
    );
    let cfg = attach_config_dir(
        "xc-relay",
        "vim.lsp.buf.execute_command({ command = 'mock.organizeImports', \
           arguments = { 'a', 2 } }, { client_id = client.id })",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;

    let recs = wait_for_record(&rpc, &record, |r| has_method(r, "workspace/executeCommand")).await;
    let req = find(&recs, "workspace/executeCommand").expect("the command reached the server");
    assert_eq!(
        req["params"]["command"].as_str(),
        Some("mock.organizeImports"),
        "execute_command relays the command name: {req:?}"
    );
    assert_eq!(
        req["params"]["arguments"][0].as_str(),
        Some("a"),
        "execute_command forwards the arguments: {req:?}"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}

#[tokio::test]
async fn execute_command_runs_a_client_side_command() {
    let _guard = test_lock().lock().await;
    // A command registered in `vim.lsp.commands` runs client-side: the handler
    // fires (stashing its args in a global) and the command is NOT relayed to the
    // server (config `commands` dispatch, Phase 8).
    let file = temp_file("xc-local", "rs", "fn main() {}\n");
    let record = configure_mock("xc-local", serde_json::json!({}));
    let cfg = attach_config_dir(
        "xc-local",
        "vim.lsp.commands['mock.local'] = function(cmd, ctx) \
           _G.nxvim_local_cmd = cmd.command \
           _G.nxvim_local_arg = cmd.arguments and cmd.arguments[1] \
           _G.nxvim_local_done = true \
         end\n\
         vim.lsp.buf.execute_command({ command = 'mock.local', arguments = { 'hi' } }, \
           { client_id = client.id })",
    );
    let (rpc, _incoming) = start_with_config_dir(Some(file), cfg.clone()).await;
    wait_for_record(&rpc, &record, |r| has_method(r, "textDocument/didOpen")).await;

    wait_for_global_flag(&rpc, "_G.nxvim_local_done").await;
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_local_cmd").await.as_str(),
        Some("mock.local"),
        "the client-side handler runs with the command"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.nxvim_local_arg").await.as_str(),
        Some("hi"),
        "the handler receives the command's arguments"
    );
    // The client-side handler short-circuits the server relay.
    assert!(
        !has_method(&record_lines(&record), "workspace/executeCommand"),
        "a client-side command must not be relayed to the server"
    );

    let _ = std::fs::remove_dir_all(&cfg);
}
