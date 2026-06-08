//! LSP Phase 7b Slice 3: LspAttach / on_attach / server_capabilities, plus the
//! `before_init` / `on_init` / `on_exit` lifecycle hooks and config forwarding.
//!
//! When a buffer's first `didOpen` goes out under an initialized server, the
//! server fires `LspAttach` with `data.client_id`; the default autocmd in the
//! `nxvim.lsp.enable` augroup resolves the client and runs the config's
//! `on_attach(client, bufnr)` — the call site that wires buffer-local LSP keymaps
//! and reads `client.server_capabilities`.

use crate::support::*;

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
