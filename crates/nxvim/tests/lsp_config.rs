//! Behavior tests for the **`nx.lsp` control surface** (Phase A,
//! docs/specs/2026-06-14-nx-lsp-design.md): the declarative config registry
//! (`nx.lsp.config` / `nx.lsp.enable`), the engine-side FileType → Start dispatch,
//! the `on_attach` lifecycle hook, and `nx.lsp.clients` introspection — all over
//! the intact engine.
//!
//! Wired like `lsp_float.rs`: the scripted mock language server (`nxvim
//! --__lsp-mock`, `nxvim_lsp::mock`) stands in for a real server, `$NXVIM_LSP_CMD`
//! overrides the spawn argv (so the config's `cmd` can be a placeholder), and the
//! `rust`-filetype buffer drives the dispatch. The process-global env means these
//! tests serialize on `serial_lock`.

use std::path::Path;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{
    attach, drain_to_latest_redraw, exec_lua, feed, map_get, serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const NXVIM_BIN: &str = env!("CARGO_BIN_EXE_nxvim");

/// Write a mock LSP script and point `$NXVIM_LSP_CMD` at the binary's
/// `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{NXVIM_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer (filetype `rust`, `foo` under the cursor) and attach — but,
/// unlike `lsp_float.rs::start`, do **not** start a server: the test drives the
/// declarative `nx.lsp.config` / `nx.lsp.enable` path itself.
async fn open_rust(dir: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    // Cursor on `foo` so a hover request has a symbol, and it stays there (the
    // reply's cursor-staleness gate passes).
    feed(&rpc, "0fw");
    (rpc, incoming)
}

/// The content float's lines on the latest redraw carrying a `float` map, or `None`.
async fn poll_float_lines(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| {
        matches!(map_get(m, "float"), Some(Value::Map(_)))
    })?;
    let Some(Value::Map(float)) = map_get(&map, "float") else {
        return None;
    };
    match map_get(float, "lines") {
        // Each wire line is a chunk run `[[text, style_id], …]` (the `virt_lines`
        // form, since the inline-float-highlighting change); an LSP hover/signature
        // is one un-styled chunk per line, so concatenate the chunk texts to recover
        // the plain line. (Mirrors the same helper in `lsp_float.rs`.)
        Some(Value::Array(lines)) => Some(
            lines
                .iter()
                .map(|row| {
                    row.as_array()
                        .map(|chunks| {
                            chunks
                                .iter()
                                .filter_map(|c| c.as_array()?.first()?.as_str())
                                .collect::<String>()
                        })
                        .unwrap_or_default()
                })
                .collect(),
        ),
        _ => Some(Vec::new()),
    }
}

/// Retry the `trigger` Lua until the content float carries a line containing `want`
/// (the server start + async reply take a moment). Panics after the window.
async fn await_float(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    trigger: &str,
    want: &str,
) -> Vec<String> {
    let mut last = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, trigger).await;
        if let Some(lines) = poll_float_lines(rpc, incoming).await {
            if lines.iter().any(|l| l.contains(want)) {
                return lines;
            }
            if !lines.is_empty() {
                last = lines;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the content float never contained {want:?}; last float lines: {last:?}");
}

/// Poll `expr` (a Lua expression, `return`-ed) until it equals `want` or the window
/// elapses; returns whether it matched. Used to wait on async attach side effects.
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..200 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// `nx.lsp.config(name, …)` accumulates across calls with neovim deep-merge
/// semantics: maps merge recursively, **lists replace**, scalars overwrite. (Pure
/// registry mechanics — no server, so no `serial_lock`/env needed.)
#[tokio::test]
async fn config_accumulates_with_deep_merge_and_list_replace() {
    let dir = temp_dir("lsp_cfg_merge");
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, "x\n").expect("write");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    let result = exec_lua(
        &rpc,
        r#"
        nx.lsp.config("svr", { cmd = { "a" }, settings = { foo = { bar = 1 } }, filetypes = { "rust" } })
        nx.lsp.config("svr", { settings = { foo = { baz = 2 } }, filetypes = { "c" } })
        local c = nx.lsp._config["svr"]
        return table.concat({
          c.cmd[1],                                   -- preserved across calls
          tostring(c.settings.foo.bar),               -- map merge keeps the old key
          tostring(c.settings.foo.baz),               -- map merge adds the new key
          c.filetypes[1] .. "/" .. tostring(c.filetypes[2]), -- list REPLACED, not appended
        }, "|")
        "#,
    )
    .await;
    assert_eq!(
        result.as_str(),
        Some("a|1|2|c/nil"),
        "deep-merge precedence wrong (map merge + list replace)"
    );

    // A non-string name fails loud (registrations-are-data, but typed).
    let errored = exec_lua(&rpc, "return tostring(pcall(nx.lsp.config, 123, {}))").await;
    assert_eq!(
        errored.as_str(),
        Some("false"),
        "config rejects a non-string name"
    );
}

/// The full declarative path: `config` + `enable` on a matching filetype starts the
/// server (engine-side FileType → Start dispatch) and a language verb's reply lands
/// on its surface — `nx.lsp.hover()` opening the content float.
#[tokio::test]
async fn enable_starts_a_server_and_hover_works() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_cfg_enable");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    let (rpc, mut incoming) = open_rust(&dir).await;

    // Declare the server and enable it; `enable` processes the already-open current
    // buffer (its FileType fired before the dispatcher existed), so the mock starts.
    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_float(&rpc, &mut incoming, "nx.lsp.hover()", "scripted hover").await;
    assert!(
        lines.iter().any(|l| l.contains("foo")),
        "hover float should carry the markup, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The built-in LSP keymaps are installed **buffer-local on `LspAttach`** by
/// `prelude/lsp.lua` (they are no longer Rust native defaults). Pressing the `K`
/// *key* — not calling `nx.lsp.hover()` — opens the hover float, proving the map was
/// installed when the mock attached and fires the verb. (Before any attach there is
/// no `K` map, so this only works once the server is bound.)
#[tokio::test]
async fn lsp_keymaps_install_on_attach_and_fire() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_cfg_keymaps");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown",
             "value": "`foo`: a scripted hover symbol" } } }"#,
    );
    let (rpc, mut incoming) = open_rust(&dir).await;

    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    // Wait for the server to bind the buffer, so the `LspAttach`-installed `K` map
    // exists before we press it (pressing an unmapped `K` would do nothing useful).
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should have attached to the buffer"
    );

    // Press the *key*. It must fire `nx.lsp.hover()` via the buffer-local map and
    // land the hover float; retry to absorb the async reply latency.
    let mut last = Vec::new();
    let mut got = None;
    for _ in 0..200 {
        feed(&rpc, "K");
        if let Some(lines) = poll_float_lines(&rpc, &mut incoming).await {
            if lines.iter().any(|l| l.contains("scripted hover")) {
                got = Some(lines);
                break;
            }
            if !lines.is_empty() {
                last = lines;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let lines =
        got.unwrap_or_else(|| panic!("the `K` key never opened a hover float; last: {last:?}"));
    assert!(
        lines.iter().any(|l| l.contains("foo")),
        "the K-key hover float should carry the markup, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// The `"*"` base layer is inherited by every server: with `filetypes` declared only
/// under `"*"` (and the named config carrying just `cmd`), the resolved config still
/// matches the `rust` buffer and starts — proving the `"*"` ⊕ named composition.
#[tokio::test]
async fn wildcard_base_filetypes_are_inherited() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_cfg_star");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown", "value": "from the star base" } } }"#,
    );
    let (rpc, mut incoming) = open_rust(&dir).await;

    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("*", { filetypes = { "rust" } })
        nx.lsp.config("mock", { cmd = { "placeholder" } })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_float(&rpc, &mut incoming, "nx.lsp.hover()", "star base").await;
    assert!(
        lines.iter().any(|l| l.contains("star base")),
        "the server should have started off the '*' filetypes, got {lines:?}"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// `on_attach(client, bufnr)` runs once the server binds the buffer (via the engine's
/// `LspAttach`), and `nx.lsp.clients({ bufnr })` then lists the attached client with
/// its resolved name and capabilities.
#[tokio::test]
async fn on_attach_runs_and_clients_lists_the_attached_client() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_cfg_attach");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown", "value": "h" } } }"#,
    );
    let (rpc, _incoming) = open_rust(&dir).await;

    exec_lua(
        &rpc,
        r#"
        _G.nxvim_attach_log = nil
        nx.lsp.config("mock", {
          cmd = { "placeholder" },
          filetypes = { "rust" },
          on_attach = function(client, bufnr)
            _G.nxvim_attach_log = client.name .. ":" .. tostring(bufnr)
          end,
        })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.nxvim_attach_log", "mock:1").await,
        "on_attach should have run with the client and bufnr"
    );

    // The attached client is now introspectable, filtered to the buffer.
    let info = exec_lua(
        &rpc,
        r#"
        local cs = nx.lsp.clients({ bufnr = 0 })
        local c = cs[1]
        return tostring(#cs) .. "|" .. tostring(c and c.name)
          .. "|" .. tostring(c and type(c.server_capabilities))
          .. "|" .. tostring(c and type(c.request))
        "#,
    )
    .await;
    assert_eq!(
        info.as_str(),
        Some("1|mock|table|function"),
        "nx.lsp.clients should surface the attached handle (name, caps, :request)"
    );

    std::env::remove_var("NXVIM_LSP_CMD");
}

/// `nx.lsp.request` with no client attached fails loud (returns without dispatching,
/// after a notify) rather than silently no-opping.
#[tokio::test]
async fn request_without_a_client_does_not_silently_dispatch() {
    let dir = temp_dir("lsp_cfg_noclient");
    let file_path = dir.join("a.txt");
    std::fs::write(&file_path, "x\n").expect("write");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // No server is enabled, so there is no client; the call resolves to nil and
    // returns (a loud notify, never a dispatch). It must not raise.
    let ok = exec_lua(
        &rpc,
        r#"
        local got = false
        local fired = pcall(function()
          nx.lsp.request("textDocument/hover", {}, function() got = true end)
        end)
        return tostring(fired) .. "|" .. tostring(got)
        "#,
    )
    .await;
    assert_eq!(
        ok.as_str(),
        Some("true|false"),
        "request with no client should return without dispatching, not error or call back"
    );
}
