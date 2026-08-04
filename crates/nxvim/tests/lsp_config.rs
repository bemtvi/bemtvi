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
    attach, command, drain_to_latest_redraw, exec_lua, feed, map_get, serial_lock, spawn, temp_dir,
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

/// The hover doc-float *window*'s rendered lines on the latest redraw, or `None`.
/// Hover is a real float window now (`windows[]` with `floating == true`, so it can
/// scroll), not the content-float `float` surface — this finds that window and
/// returns its plain-text rows. (Mirrors the helpers in `lsp_float.rs`.)
async fn poll_float_lines(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> Option<Vec<String>> {
    nxvim_test_harness::barrier(rpc).await;
    let map = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some())?;
    Some(window_lines(&floating_window(&map)?))
}

/// The first floating *window* (`windows[]` with `floating == true`) in a redraw —
/// the hover doc float — or `None`. The main editor window is `floating == false`.
fn floating_window(map: &[(Value, Value)]) -> Option<Vec<(Value, Value)>> {
    let windows = map_get(map, "windows")?.as_array()?;
    windows
        .iter()
        .filter_map(Value::as_map)
        .find(|w| map_get(w, "floating").and_then(Value::as_bool) == Some(true))
        .cloned()
}

/// A float window's rendered text rows (the redraw `lines` array — plain strings).
fn window_lines(win: &[(Value, Value)]) -> Vec<String> {
    match map_get(win, "lines") {
        Some(Value::Array(rows)) => rows
            .iter()
            .map(|r| r.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Retry the `trigger` Lua until the hover float window carries a line containing
/// `want` (the server start + async reply take a moment). Panics after the window.
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
    panic!("the hover float window never contained {want:?}; last float lines: {last:?}");
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

    // A list of TABLES is replaced whole too, and a shorter list leaves no tail —
    // the shape a tool-config plugin re-registers (efm's `settings.languages.<ft>`
    // is a list of `{ lintCommand = … }` / `{ formatCommand = … }` entries). Merged
    // index-wise instead, entry 1 would carry BOTH tools' keys and the dropped
    // entry 2 would survive: a config nobody wrote, silently driving the server.
    let relisted = exec_lua(
        &rpc,
        r#"
        nx.lsp.config("tools", { settings = { languages = { lua = {
          { lintCommand = "luacheck -" }, { formatCommand = "stylua -" },
        } } } })
        nx.lsp.config("tools", { settings = { languages = { lua = {
          { formatCommand = "stylua -" },
        } } } })
        local l = nx.lsp._config["tools"].settings.languages.lua
        return #l .. "|" .. tostring(l[1].lintCommand) .. "|" .. tostring(l[1].formatCommand)
        "#,
    )
    .await;
    assert_eq!(
        relisted.as_str(),
        Some("1|nil|stylua -"),
        "a re-registered list must replace, not fuse entry-by-entry"
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
/// on its surface — `nx.lsp.hover()` opening the hover float window.
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

/// `nx.lsp.enable` catches up on **every** buffer already read, not just the current
/// one. A server enabled lazily — a plugin that resolves its tools over the async
/// `nx.fs` seam before it registers the config, as nxvim-efmls-configs does for efm —
/// lands several ticks after the files were read, so the dispatcher it installs never
/// sees any of them. Only the current buffer used to be caught up: every other
/// already-read buffer was served by nothing for the rest of the session, since no
/// later event re-fires `FileType` for a buffer that already has one.
#[tokio::test]
async fn enable_catches_up_on_every_open_buffer() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_cfg_catchup");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown", "value": "scripted" } } }"#,
    );
    // `a.rs` is the startup buffer (1); `b.rs` is read next and becomes current (2).
    // Both filetypes settle before anything enables a server.
    let (rpc, _incoming) = open_rust(&dir).await;
    let second = dir.join("b.rs");
    std::fs::write(&second, "let bar = baz()\n").expect("write second file");
    command(&rpc, &format!("e {}", second.display())).await;

    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 2 })", "1").await,
        "the current buffer should carry the server"
    );
    // The buffer left behind was *bound* by the same catch-up, so going back to it
    // serves it — document sync is per current buffer, so its `didOpen` (and the
    // attach) lands the moment it is displayed again. Without the catch-up it stays
    // bound to nothing forever: nothing re-fires `FileType` for a buffer that has one.
    command(&rpc, "b 1").await;
    assert!(
        await_lua_eq(&rpc, "#nx.lsp.clients({ bufnr = 1 })", "1").await,
        "the buffer read BEFORE the enable must be caught up too"
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

/// A config with `root_markers` (no explicit `root_dir`) drives the async upward
/// `find_root` walk through the `nx.fs` seam, then starts the server. This is the
/// path the `ui-complete-lsp` example takes — and the one that regressed when
/// `find_root` returned the `nx.async` *wrapper* instead of calling it for a promise
/// (`:next` on a function → "attempt to index a function value"). The `.git` marker
/// in the temp dir resolves the root upward from `a.rs`.
#[tokio::test]
async fn root_markers_resolve_and_start_the_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp_cfg_rootmarkers");
    arm_mock(
        &dir,
        r#"{ "hover": { "contents": { "kind": "markdown", "value": "rooted hover" } } }"#,
    );
    // A `.git` marker so the upward `root_markers` search resolves a root.
    std::fs::create_dir_all(dir.join(".git")).expect("create .git marker");
    let (rpc, mut incoming) = open_rust(&dir).await;

    exec_lua(
        &rpc,
        r#"
        nx.lsp.config("mock", {
          cmd = { "placeholder" },
          filetypes = { "rust" },
          root_markers = { ".git" },
        })
        nx.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_float(&rpc, &mut incoming, "nx.lsp.hover()", "rooted hover").await;
    assert!(
        lines.iter().any(|l| l.contains("rooted hover")),
        "the server should have started via the root_markers path, got {lines:?}"
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

// ----- multiple servers on one buffer ---------------------------------------
// Phase 2 of docs/plans/2026-07-25-multi-server-lsp-attach.md. Two servers
// enabled for one filetype used to spawn both and attach ONE, nondeterministically
// (the last `LspOp::Start` won a single slot). Both must now attach.

/// Point `$NXVIM_LSP_CMD_<NAME>` at the mock with its own script, so two servers
/// are distinguishable. The blanket `$NXVIM_LSP_CMD` cannot do this — it would aim
/// both at one script, and no assertion could tell which server answered.
fn arm_mock_named(dir: &Path, name: &str, script: &str) {
    let file = dir.join(format!("mock-{name}.json"));
    std::fs::write(&file, script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        format!("NXVIM_LSP_CMD_{}", name.to_uppercase()),
        format!("{NXVIM_BIN} --__lsp-mock {}", file.display()),
    );
}

/// [`open_rust`] with the buffer's own body — the decoration tests need a line with
/// **multi-byte** text so a UTF-8 and a UTF-16 server disagree about columns.
async fn open_rust_with(dir: &Path, body: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file_path = dir.join("a.rs");
    std::fs::write(&file_path, body).expect("write test file");
    let init = ServerInit {
        file: Some(file_path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    feed(&rpc, "gg0");
    (rpc, incoming)
}

/// Enable two mock servers, `alpha` and `beta`, for the `rust` filetype and wait
/// for both to attach.
async fn enable_alpha_beta(rpc: &Rpc) {
    exec_lua(
        rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );
}

/// Fire `trigger` (a code-action Lua verb) until the chooser menu carries at least
/// `want` rows, and return its titles. The request is a fan-out round, so the menu
/// only opens once every asked server has answered.
async fn code_action_rows(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    trigger: &str,
    want: usize,
) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for _ in 0..200 {
        exec_lua(rpc, trigger).await;
        nxvim_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| {
            map_get(m, "menu").map(|v| !matches!(v, Value::Nil)) == Some(true)
        }) {
            if let Some(items) = map_get(&map, "menu").and_then(|m| {
                m.as_map()
                    .and_then(|mm| map_get(mm, "items"))
                    .and_then(|i| i.as_array().cloned())
            }) {
                rows = items
                    .iter()
                    .map(|i| i.as_str().unwrap_or_default().to_string())
                    .collect();
                if rows.len() >= want {
                    return rows;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    rows
}

#[tokio::test]
async fn two_servers_attach_to_one_buffer() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-two-servers");
    arm_mock_named(dir.as_path(), "alpha", "{}");
    arm_mock_named(dir.as_path(), "beta", "{}");
    let (rpc, _incoming) = open_rust(dir.as_path()).await;

    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;

    // Both must attach — and in ServerKey order, so the list is stable run to run.
    let attached = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local n = {}\n\
         \x20 for _, c in ipairs(vim.lsp.get_clients({ bufnr = 0 })) do n[#n+1] = c.name end\n\
         \x20 table.sort(n)\n\
         \x20 return table.concat(n, ',')\n\
         end)()",
        "alpha,beta",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");
    assert!(attached, "both servers attached to the buffer");
}

#[tokio::test]
async fn two_servers_both_receive_the_document_and_its_edits() {
    // The real proof that both are *attached*, not merely spawned: each server has
    // to hold the document. The mock answers hover from the text it was sent, so a
    // hover from each server round-trips only if that server got `didOpen` — and,
    // after typing, the `didChange` too (the journal is drained once and replayed
    // into each server's own shadow; a second server that missed it would answer
    // against stale text).
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-two-servers-doc");
    arm_mock_named(dir.as_path(), "alpha", "{}");
    arm_mock_named(dir.as_path(), "beta", "{}");
    let (rpc, _incoming) = open_rust(dir.as_path()).await;

    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    let both = await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await;
    assert!(both, "both servers attached");

    // Hover answers from the document the server holds, so a non-empty reply proves
    // that server received `didOpen`. Ask after an edit, so it also proves the
    // `didChange` reached it — the journal is drained once per sync and replayed into
    // each server's own shadow, so a server that missed it would answer stale text
    // (or error on a position past the end).
    feed(&rpc, "A_edited<Esc>");
    let _ = rpc.request("nvim_get_mode", vec![]).await;
    let hovered = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local ok = 0\n\
         \x20 for _, c in ipairs(vim.lsp.get_clients({ bufnr = 0 })) do\n\
         \x20   if c.name == 'alpha' or c.name == 'beta' then ok = ok + 1 end\n\
         \x20 end\n\
         \x20 return ok\n\
         end)()",
        "2",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");
    assert!(hovered, "both servers still attached after an edit");
}

#[tokio::test]
async fn a_request_routes_to_the_server_that_advertises_it() {
    // Phase 3a of docs/plans/2026-07-25-multi-server-lsp-attach.md. `alpha` sorts
    // first but withholds hoverProvider; `beta` offers it and has the answer. The
    // hover must reach beta. Picking by position in the map — what every request
    // path did before — would ask alpha and render nothing, which is exactly the
    // `pyright` + `ruff` failure in miniature (the linter has no hover).
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-route-by-cap");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "capabilities": { "hoverProvider": false },
             "hover": { "contents": "FROM-ALPHA" } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "hover": { "contents": "FROM-BETA" } }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;

    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // Panics if beta's hover never arrives — i.e. if the request went to alpha.
    let lines = await_float(&rpc, &mut incoming, "nx.lsp.hover()", "FROM-BETA").await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    let joined = lines.join(" ");
    assert!(
        !joined.contains("FROM-ALPHA"),
        "and not from the server that withholds hoverProvider, got {joined:?}"
    );
}

#[tokio::test]
async fn code_actions_merge_from_every_capable_server() {
    // Phase 3b. A linter's quick-fix and a type-checker's refactor are both things
    // you want offered; asking only one server silently hides half the menu. Both
    // servers' actions must appear in one chooser.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-ca-merge");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "code_action": [ { "title": "ALPHA-FIX", "kind": "quickfix" } ] }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "code_action": [ { "title": "BETA-REFACTOR", "kind": "refactor" } ] }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;

    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // The chooser lists both servers' actions.
    let rows = code_action_rows(&rpc, &mut incoming, "nx.lsp.code_action()", 2).await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    let joined = rows.join(" | ");
    assert!(
        joined.contains("ALPHA-FIX") && joined.contains("BETA-REFACTOR"),
        "the chooser merged both servers' actions, got {joined:?}"
    );
}

#[tokio::test]
async fn format_selects_the_named_server() {
    // Phase 5. `format({ name = … })` was REJECTED while nxvim modelled one server
    // per buffer (f516cbe3) — there was nothing to select. Now that a buffer carries
    // several it is the option that says "format with ruff, not pyright", so it is
    // modelled. Each mock rewrites line 1 to its own marker, so the buffer text says
    // which server formatted.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-format-name");
    let edit = |marker: &str| {
        format!(
            r#"{{ "formatting": [ {{ "range": {{ "start": {{ "line": 0, "character": 0 }},
                 "end": {{ "line": 0, "character": 15 }} }}, "newText": "{marker}" }} ] }}"#
        )
    };
    arm_mock_named(dir.as_path(), "alpha", &edit("BY-ALPHA"));
    arm_mock_named(dir.as_path(), "beta", &edit("BY-BETA"));
    let (rpc, _incoming) = open_rust(dir.as_path()).await;

    exec_lua(
        &rpc,
        "nx.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         nx.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // Name the SECOND server; alpha sorts first, so a default pick would use alpha.
    exec_lua(&rpc, "nx.lsp.format({ name = 'beta' })").await;
    let formatted = await_lua_eq(&rpc, "nx.buf.lines(0, 0, 1)[1]", "BY-BETA").await;

    // An unattached name must not silently format with someone else.
    let unknown = exec_lua(
        &rpc,
        "nx.lsp.format({ name = 'nosuch' })\n\
         return tostring(pcall(nx.lsp.format, { bogus = 1 }))",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        formatted,
        "the named server did the formatting, not the default pick"
    );
    assert_eq!(
        unknown.as_str(),
        Some("false"),
        "an unmodelled option still fails loud"
    );
}

// ----- routing a request to a NAMED client -----------------------------------
// `nx.lsp.hover{ name = … }` / `:LspHover <server>`: which of a buffer's attached
// servers answers. The default pick is the first, in name order, that advertises the
// feature — fine until two servers both do, when the one you want may be second.

/// Run `cmd` as an ex-command and return the message line it left. Each assertion
/// below expects a *different* message, so a stale frame can't pass by accident.
async fn message_after_cmd(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    cmd: &str,
) -> String {
    command(rpc, cmd).await;
    nxvim_test_harness::barrier(rpc).await;
    drain_to_latest_redraw(incoming, |m| !nxvim_test_harness::message(m).is_empty())
        .map(|m| nxvim_test_harness::message(&m))
        .unwrap_or_default()
}

#[tokio::test]
async fn a_request_routes_to_the_named_server() {
    // Both servers advertise hover and answer differently. `alpha` sorts first, so
    // the DEFAULT pick is alpha — which is what makes naming `beta` prove the route:
    // an ignored name renders FROM-ALPHA and the poll never sees FROM-BETA.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-route-by-name");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "hover": { "contents": "FROM-ALPHA" } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "hover": { "contents": "FROM-BETA" } }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    // The Lua option routes past the default pick…
    let named = await_float(
        &rpc,
        &mut incoming,
        "nx.lsp.hover({ name = 'beta' })",
        "FROM-BETA",
    )
    .await;
    // …and the ex-command's bare argument is the same route, back to the other one
    // (so this leg can't pass by the float simply lingering).
    let ex = await_float(
        &rpc,
        &mut incoming,
        "nx.cmd('LspHover alpha')",
        "FROM-ALPHA",
    )
    .await;

    // …and the argument completes from the buffer's own clients, in its own slot —
    // `:LspRename` takes the new identifier first, so its route is the SECOND word.
    let completions = exec_lua(
        &rpc,
        "local function at(line)\n\
         \x20 local out = {}\n\
         \x20 for _, c in ipairs(nx._cmdline_complete_run(line, #line)) do\n\
         \x20   out[#out + 1] = c.insert\n\
         \x20 end\n\
         \x20 return table.concat(out, ',')\n\
         end\n\
         return at('LspHover ') .. '/' .. at('LspRename Foo ') .. '/' .. at('LspRename ')",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert_eq!(
        completions.as_str(),
        Some("alpha,beta/alpha,beta/"),
        "the `[server]` argument completes the attached clients in its own slot"
    );

    assert!(
        !named.join(" ").contains("FROM-ALPHA"),
        "the named hover came from beta alone, got {named:?}"
    );
    assert!(
        !ex.join(" ").contains("FROM-BETA"),
        ":LspHover alpha came from alpha alone, got {ex:?}"
    );
}

#[tokio::test]
async fn a_named_route_that_cannot_be_honored_says_why() {
    // The three ways a route fails are three different fixes, so they must not
    // collapse into one message — and none of them may fall back to another server,
    // which is the whole point of naming one.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-route-errors");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "capabilities": { "hoverProvider": false },
             "hover": { "contents": "FROM-ALPHA" } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "hover": { "contents": "FROM-BETA" } }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    let unattached = message_after_cmd(&rpc, &mut incoming, "LspHover nosuch").await;
    let incapable = message_after_cmd(&rpc, &mut incoming, "LspHover alpha").await;
    let trailing = message_after_cmd(&rpc, &mut incoming, "LspHover alpha beta").await;
    // The Lua verb reports the same way (it settles `nil` rather than hovering).
    let lua_unattached = exec_lua(
        &rpc,
        "nx.lsp.hover({ name = 'nosuch' })\n\
         return tostring(pcall(nx.lsp.hover, { bogus = 1 }))",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        unattached.contains("No LSP client named 'nosuch'"),
        "an unattached name is reported by name, got {unattached:?}"
    );
    assert!(
        incapable.contains("'alpha' does not provide hover"),
        "an attached server that withholds the provider says so — not \
         'no client named alpha', and not beta's hover; got {incapable:?}"
    );
    assert!(
        trailing.contains("E488"),
        "a second server name is a typo, not a second route; got {trailing:?}"
    );
    assert_eq!(
        lua_unattached.as_str(),
        Some("false"),
        "an unmodelled option on a routed verb still fails loud"
    );
}

#[tokio::test]
async fn a_named_code_action_round_asks_only_that_server() {
    // The merge is the default and the right one — but "run eslint's fixes" needs to
    // exclude the other server's actions, not just prefer them.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-route-ca");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "code_action": [ { "title": "ALPHA-FIX", "kind": "quickfix" } ] }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "code_action": [ { "title": "BETA-REFACTOR", "kind": "refactor" } ] }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    let rows = code_action_rows(
        &rpc,
        &mut incoming,
        "nx.lsp.code_action({ name = 'beta' })",
        1,
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    let joined = rows.join(" | ");
    assert!(
        joined.contains("BETA-REFACTOR") && !joined.contains("ALPHA-FIX"),
        "the routed round listed beta's actions ALONE, got {joined:?}"
    );
}

// ----- Phase 4: per-server decorations (semantic tokens + inlay hints) --------
// Both are whole-buffer *caches*, so they don't merge like a fan-out round: each
// server is asked, each reply lands under its own server's document state, and the
// projection concatenates. The tests deliberately negotiate DIFFERENT encodings —
// two servers on one buffer at utf-8 and utf-16 is exactly where column math breaks
// (a shared encoding decodes the second server's columns into the middle of a
// multi-byte glyph).

/// A line whose columns differ between utf-8 bytes and utf-16 code units:
/// `let föö = 1` — `föö` is bytes 4..9 but utf-16 units 4..7.
const MULTIBYTE_LINE: &str = "let föö = 1\n";

#[tokio::test]
async fn semantic_tokens_merge_from_every_capable_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-semantic-merge");
    // alpha (utf-8) paints `let` as a keyword; beta (utf-16) paints `föö` as a
    // variable. Only alpha was ever asked before this phase — it is the first
    // capable server in key order — so beta's tokens never existed.
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "semantic_tokens": {
               "legend": { "tokenTypes": ["keyword"], "tokenModifiers": [] },
               "data": [0, 0, 3, 0, 0] } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "position_encoding": "utf-16",
             "semantic_tokens": {
               "legend": { "tokenTypes": ["variable"], "tokenModifiers": [] },
               "data": [0, 4, 3, 0, 0] } }"#,
    );
    let (rpc, _incoming) = open_rust_with(dir.as_path(), MULTIBYTE_LINE).await;
    enable_alpha_beta(&rpc).await;

    let alpha_token = await_lua_eq(
        &rpc,
        "(nx.lsp.semantic_tokens.get_at_pos(0, 0, 0)[1] or {}).type",
        "keyword",
    )
    .await;
    let beta_token = await_lua_eq(
        &rpc,
        "(nx.lsp.semantic_tokens.get_at_pos(0, 0, 4)[1] or {}).type",
        "variable",
    )
    .await;
    // Byte 8 is the *second* byte of the trailing `ö`. beta's token spans utf-16
    // units 4..7 = bytes 4..9, so it covers byte 8 — but only when decoded at
    // beta's OWN negotiated encoding. Decoded at alpha's utf-8 it would stop at
    // byte 7 and this column would be bare.
    let beta_encoding = await_lua_eq(
        &rpc,
        "(nx.lsp.semantic_tokens.get_at_pos(0, 0, 8)[1] or {}).type",
        "variable",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(alpha_token, "the first server's tokens still decode");
    assert!(
        beta_token,
        "the second server's tokens must be requested and decoded too"
    );
    assert!(
        beta_encoding,
        "the second server's tokens decode at ITS negotiated encoding (utf-16), \
         not the first server's"
    );
}

#[tokio::test]
async fn inlay_hints_merge_from_every_capable_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-inlay-merge");
    // alpha (utf-8) anchors at character 4 = byte 4; beta (utf-16) at character 7,
    // which is byte 9 — the same glyph boundary only if decoded at utf-16.
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "inlay_hints": [
               { "position": { "line": 0, "character": 4 }, "label": ":A", "kind": 1 } ] }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "position_encoding": "utf-16",
             "inlay_hints": [
               { "position": { "line": 0, "character": 7 }, "label": ":B", "kind": 1 } ] }"#,
    );
    let (rpc, _incoming) = open_rust_with(dir.as_path(), MULTIBYTE_LINE).await;
    enable_alpha_beta(&rpc).await;
    exec_lua(&rpc, "vim.lsp.inlay_hint.enable(true)").await;

    // Each hint's label with the byte column it decoded to, sorted.
    let merged = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local out = {}\n\
         \x20 for _, h in ipairs(nx.lsp.inlay_hint.get({ bufnr = 0 })) do\n\
         \x20   out[#out+1] = h.inlay_hint.label .. '@' .. h.inlay_hint.col\n\
         \x20 end\n\
         \x20 table.sort(out)\n\
         \x20 return table.concat(out, ',')\n\
         end)()",
        ":A@4,:B@9",
    )
    .await;
    // Each hint is tagged with the client that produced it, so a plugin can tell
    // pyright's hints from ruff's.
    let two_clients = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local ids, n = {}, 0\n\
         \x20 for _, h in ipairs(nx.lsp.inlay_hint.get({ bufnr = 0 })) do\n\
         \x20   if not ids[h.client_id] then ids[h.client_id] = true; n = n + 1 end\n\
         \x20 end\n\
         \x20 return n\n\
         end)()",
        "2",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        merged,
        "both servers' hints must be requested and decoded, each at its own \
         negotiated encoding"
    );
    assert!(two_clients, "each hint carries its producing client's id");
}

#[tokio::test]
async fn a_named_formatter_applies_edits_at_its_own_encoding() {
    // The other half of "two servers at different encodings": a REPLY's positions
    // are authored in the encoding of the server that produced it. `format{name=}`
    // makes that reachable — the named formatter need not be the buffer's first
    // server, and reading its utf-16 columns as utf-8 (what the apply path did,
    // deriving the encoding from the buffer's first server) shifts every edit on a
    // line with a multi-byte character.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-format-encoding");
    // `föö` is utf-16 units 4..7 but bytes 4..9. beta asks to replace exactly it.
    arm_mock_named(dir.as_path(), "alpha", "{}");
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "position_encoding": "utf-16",
             "formatting": [ { "range": { "start": { "line": 0, "character": 4 },
                                          "end":   { "line": 0, "character": 7 } },
                               "newText": "X" } ] }"#,
    );
    let (rpc, _incoming) = open_rust_with(dir.as_path(), MULTIBYTE_LINE).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(&rpc, "nx.lsp.format({ name = 'beta' })").await;
    let formatted = await_lua_eq(&rpc, "nx.buf.lines(0, 0, 1)[1]", "let X = 1").await;
    let line = exec_lua(&rpc, "return nx.buf.lines(0, 0, 1)[1]").await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        formatted,
        "the edit must convert at beta's utf-16, not alpha's utf-8 (got {:?}; \
         `let Xö = 1` is the utf-8 misread)",
        line.as_str()
    );
}

// ----- Phase 7: the surfaces that still resolved "the" server by position -----
// Everything below is one failure mode: a path that answered "which server?" with
// the buffer's FIRST attached one instead of the one that is actually involved —
// the same class the phases above fixed for sync, requests and decorations, left
// standing in the request *context*, the merged results, and the apply/dispatch
// follow-ups.

/// The `file://` URI of the buffer [`open_rust_with`] / [`open_rust`] opens, for a
/// mock script that must name the document it is answering about.
fn file_uri(dir: &Path) -> String {
    format!("file://{}", dir.join("a.rs").display())
}

#[tokio::test]
async fn merged_diagnostics_carry_the_client_that_published_each() {
    // `vim.diagnostic.get` returns ONE flat list per buffer, merged across every
    // attached server — so without a tag there is no way to tell the type-checker's
    // errors from the linter's, which is the first question anyone asks of a
    // two-server buffer. The semantic-token and inlay-hint mirrors are tagged with
    // their producing `client_id` for exactly this reason; the diagnostics mirror,
    // built by the same merge, was not.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-diag-clientid");
    let diag = |line: u32, msg: &str| {
        format!(
            r#"{{ "diagnostics": [ {{ "range": {{ "start": {{ "line": {line}, "character": 0 }},
                 "end": {{ "line": {line}, "character": 3 }} }},
                 "severity": 1, "message": "{msg}" }} ] }}"#
        )
    };
    arm_mock_named(dir.as_path(), "alpha", &diag(0, "FROM-ALPHA"));
    arm_mock_named(dir.as_path(), "beta", &diag(1, "FROM-BETA"));
    let (rpc, _incoming) = open_rust_with(dir.as_path(), "one\ntwo\n").await;
    enable_alpha_beta(&rpc).await;

    let tagged = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local out = {}\n\
         \x20 for _, d in ipairs(vim.diagnostic.get(0)) do\n\
         \x20   local c = d.client_id and vim.lsp.get_client_by_id(d.client_id)\n\
         \x20   out[#out+1] = (c and c.name or '?') .. ':' .. d.message\n\
         \x20 end\n\
         \x20 table.sort(out)\n\
         \x20 return table.concat(out, ',')\n\
         end)()",
        "alpha:FROM-ALPHA,beta:FROM-BETA",
    )
    .await;
    let got = exec_lua(
        &rpc,
        "return tostring(#vim.diagnostic.get(0)) .. '/' .. \
         tostring(vim.diagnostic.get(0)[1] and vim.diagnostic.get(0)[1].client_id)",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        tagged,
        "each merged diagnostic names the client that published it (count/first \
         client_id was {:?})",
        got.as_str()
    );
}

#[tokio::test]
async fn a_code_action_request_carries_each_servers_own_diagnostics() {
    // `context.diagnostics` is what a linter gates its quick-fixes on: ruff offers
    // "fix unused import" only when the request carries ruff's own diagnostic. The
    // fan-out asked every server but handed them all ONE list, harvested from the
    // buffer's first server — so the second server was asked about diagnostics it
    // never published, and its quick-fixes were silently never offered. Which is
    // the exact failure the code-action fan-out exists to prevent.
    //
    // Both mocks echo the request's `context.diagnostics` count back as the action
    // title (`code_action_echo_range`), so the chooser shows what each server was
    // actually sent. Only beta publishes a diagnostic, so a correct request gives
    // beta `diags=1` and alpha `diags=0`; the shared-list bug gives both `diags=0`.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-ca-context");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "code_action_echo_range": true }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "code_action_echo_range": true,
             "diagnostics": [ { "range": { "start": { "line": 0, "character": 0 },
                                           "end":   { "line": 0, "character": 15 } },
                                "severity": 2, "message": "BETA-DIAG" } ] }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;
    // The publish rides `didOpen`, so wait for it to land before asking.
    assert!(
        await_lua_eq(&rpc, "#vim.diagnostic.get(0)", "1").await,
        "beta's diagnostic reached the editor"
    );

    let rows = code_action_rows(&rpc, &mut incoming, "nx.lsp.code_action()", 2).await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    let joined = rows.join(" | ");
    assert!(
        joined.contains("diags=1"),
        "the server that published the diagnostic must be asked WITH it, got {joined:?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("diags=0")).count(),
        1,
        "and the server that published none must be asked with none — its \
         diagnostics are not the other server's to send, got {joined:?}"
    );
}

#[tokio::test]
async fn references_merge_deduplicate_and_decode_at_each_servers_encoding() {
    // The two halves of a merged location list, in one buffer:
    //
    //  * every location is converted with the encoding of the server that REPORTED
    //    it. `apply_lsp_locations` re-derived it from the buffer's first server, so
    //    a utf-16 server's columns were read as utf-8 bytes.
    //  * the same reference reported by both servers is shown ONCE. The old
    //    `Vec::dedup_by` collapses only *adjacent* duplicates, and a merged list is
    //    alpha's block followed by beta's — never adjacent, so it never collapsed
    //    anything.
    //
    // On `let föö = 1`, byte 10 (the `=`) is utf-8 character 10 and utf-16 unit 8.
    // Both servers report it; beta additionally reports byte 4. Correct: two rows,
    // 1-based columns 5 and 11. Reading beta at utf-8 puts its `=` at byte 8 —
    // a third, phantom row at column 9.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-refs-merge");
    let uri = file_uri(dir.as_path());
    let loc = |c: u32| {
        format!(
            r#"{{ "uri": "{uri}", "range": {{ "start": {{ "line": 0, "character": {c} }},
                 "end": {{ "line": 0, "character": {} }} }} }}"#,
            c + 1
        )
    };
    arm_mock_named(
        dir.as_path(),
        "alpha",
        &format!(r#"{{ "references": [ {} ] }}"#, loc(10)),
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        &format!(
            r#"{{ "position_encoding": "utf-16", "references": [ {}, {} ] }}"#,
            loc(8),
            loc(4)
        ),
    );
    let (rpc, _incoming) = open_rust_with(dir.as_path(), MULTIBYTE_LINE).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(
        &rpc,
        "_G.cols = nil\n\
         nx.lsp.references():next(function(items)\n\
         \x20 local c = {}\n\
         \x20 for _, it in ipairs(items or {}) do c[#c+1] = it.col end\n\
         \x20 table.sort(c)\n\
         \x20 _G.cols = table.concat(c, ',')\n\
         end)",
    )
    .await;
    let merged = await_lua_eq(&rpc, "_G.cols", "5,11").await;
    let got = exec_lua(&rpc, "return tostring(_G.cols)").await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        merged,
        "both servers' references merge, each decoded at its own encoding, with the \
         shared one shown once (got {:?}; `5,9,11` is the utf-8 misread plus the \
         duplicate that never collapsed)",
        got.as_str()
    );
}

#[tokio::test]
async fn a_rename_applies_its_edits_at_the_answering_servers_encoding() {
    // The twin of `a_named_formatter_applies_edits_at_its_own_encoding`, for the
    // path that got missed: formatting applies through `apply_formatting_edits`
    // (which takes the producing server's encoding), but a rename's `WorkspaceEdit`
    // goes through `apply_workspace_edit`, which re-derived the encoding from each
    // TARGET buffer's first server — overriding the origin it had been handed.
    //
    // alpha sorts first and withholds `renameProvider`, so the rename routes to
    // beta (utf-16), whose columns 4..7 are bytes 4..9 — the whole of `föö`.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-rename-encoding");
    let uri = file_uri(dir.as_path());
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "capabilities": { "renameProvider": false } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        &format!(
            r#"{{ "position_encoding": "utf-16",
                  "rename": {{ "changes": {{ "{uri}": [
                    {{ "range": {{ "start": {{ "line": 0, "character": 4 }},
                                   "end":   {{ "line": 0, "character": 7 }} }},
                       "newText": "X" }} ] }} }} }}"#
        ),
    );
    let (rpc, _incoming) = open_rust_with(dir.as_path(), MULTIBYTE_LINE).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(&rpc, "nx.lsp.rename('X')").await;
    let renamed = await_lua_eq(&rpc, "nx.buf.lines(0, 0, 1)[1]", "let X = 1").await;
    let line = exec_lua(&rpc, "return nx.buf.lines(0, 0, 1)[1]").await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        renamed,
        "the workspace edit must convert at the ANSWERING server's utf-16, not the \
         buffer's first server's utf-8 (got {:?}; `let Xö = 1` is the misread)",
        line.as_str()
    );
}

#[tokio::test]
async fn a_code_actions_command_runs_on_the_server_that_offered_it() {
    // A code action may carry a `command` instead of (or besides) an edit, which is
    // finished with a `workspace/executeCommand`. Its name and arguments are the
    // issuing server's own vocabulary — ruff's `source.fixAll` command means nothing
    // to pyright — so it must go back to the server that offered the action.
    //
    // The merged chooser already tracked each action's origin for `codeAction/resolve`
    // and then dropped it for the command, which went to the buffer's first server.
    // Only beta offers an action; each mock records what it receives, so the record
    // files say which server was actually asked to execute.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-ca-command");
    let rec = |name: &str| dir.join(format!("rec-{name}.jsonl"));
    arm_mock_named(
        dir.as_path(),
        "alpha",
        &format!(
            r#"{{ "record": "{}", "code_action": [] }}"#,
            rec("alpha").display()
        ),
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        &format!(
            r#"{{ "record": "{}",
                  "code_action": [ {{ "title": "BETA-CMD", "kind": "quickfix",
                    "command": {{ "title": "run", "command": "beta.doIt" }} }} ] }}"#,
            rec("beta").display()
        ),
    );
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    // Exactly one action survives, so `apply` makes it a one-shot: no chooser to
    // drive, the command dispatches straight away.
    exec_lua(&rpc, "nx.lsp.code_action({ apply = true })").await;

    let executed = |name: &str| {
        std::fs::read_to_string(rec(name))
            .unwrap_or_default()
            .contains("workspace/executeCommand")
    };
    let mut on_beta = false;
    for _ in 0..200 {
        if executed("beta") {
            on_beta = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let on_alpha = executed("alpha");

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        on_beta,
        "the command must execute on the server that offered the action"
    );
    assert!(
        !on_alpha,
        "and not on the buffer's first server, which never offered it"
    );
}

#[tokio::test]
async fn a_registered_command_handler_wins_over_the_server_round_trip() {
    // The other half of `nx.lsp.commands`: some code-action commands are defined to
    // run *client*-side — the server has no way to open a file or start a rename
    // itself — so a registered handler must pre-empt `workspace/executeCommand`. It
    // is handed the offering client's id, because one command name can mean
    // different things to two servers on the same buffer.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-ca-handler");
    let rec = dir.join("rec-beta.jsonl");
    arm_mock_named(dir.as_path(), "alpha", r#"{ "code_action": [] }"#);
    arm_mock_named(
        dir.as_path(),
        "beta",
        &format!(
            r#"{{ "record": "{}",
                  "code_action": [ {{ "title": "BETA-CMD", "kind": "quickfix",
                    "command": {{ "title": "run", "command": "beta.doIt",
                                  "arguments": [ "ARG-ONE" ] }} }} ] }}"#,
            rec.display()
        ),
    );
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(
        &rpc,
        "_G.ran = nil\n\
         nx.lsp.commands['beta.doIt'] = function(command, ctx)\n\
         \x20 local client = nx.lsp._clients[ctx.client_id]\n\
         \x20 _G.ran = (client and client.name or '?') .. '/' .. tostring(command.arguments[1])\n\
         end\n\
         nx.lsp.code_action({ apply = true })",
    )
    .await;
    let handled = await_lua_eq(&rpc, "_G.ran", "beta/ARG-ONE").await;
    // Give a stray round trip time to show up in the record before asserting on it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let executed = std::fs::read_to_string(&rec)
        .unwrap_or_default()
        .contains("workspace/executeCommand");

    // Registered on a shared table, so the next test in this process would inherit it.
    exec_lua(&rpc, "nx.lsp.commands['beta.doIt'] = nil").await;
    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        handled,
        "the registered handler runs, with the OFFERING client's id and the \
         command's arguments"
    );
    assert!(
        !executed,
        "and the server round trip is skipped — a client-side command is the \
         editor's to run"
    );
}

#[tokio::test]
async fn the_signature_autotrigger_fires_for_the_server_that_advertises_it() {
    // The auto-trigger's per-buffer gate asked "does THIS buffer's server advertise
    // trigger characters", resolving the server by position rather than by
    // capability — so on a buffer whose first server has no signature help (eslint
    // ahead of ts_ls), every typed `(` was swallowed even though the second server
    // offers it. The editor-wide trigger set is a union across servers, so core
    // raised the request correctly and the gate then dropped it on the floor.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-sig-autotrigger");
    // `null`, not `false`: lsp-types models `signatureHelpProvider` as an options
    // object (no `OneOf<bool, …>`), so a bare `false` fails to deserialize the whole
    // `initialize` result and the server never comes up at all.
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "capabilities": { "signatureHelpProvider": null } }"#,
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        r#"{ "signature_help": { "signatures": [ { "label": "FROM-BETA(x)" } ],
                                 "activeSignature": 0 } }"#,
    );
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;
    exec_lua(&rpc, "nx.lsp.signature_help_autotrigger(true)").await;

    // Typing the server's trigger character is the whole input — no Lua verb.
    feed(&rpc, "A(");
    let mut shown = Vec::new();
    let mut found = false;
    for _ in 0..200 {
        if let Some(lines) = poll_float_lines(&rpc, &mut incoming).await {
            if lines.iter().any(|l| l.contains("FROM-BETA")) {
                found = true;
                break;
            }
            if !lines.is_empty() {
                shown = lines;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        found,
        "the `(` must reach the server that advertises signature help, not be \
         dropped because the buffer's first server does not (last float: {shown:?})"
    );
}

#[tokio::test]
async fn document_symbols_only_ask_the_servers_that_advertise_them() {
    // `documentSymbol` fans out, but `ProviderCaps` modelled no provider flag for
    // it — so the routing predicate returned "not capability-gated" and failed
    // OPEN, asking every attached server including the ones that never advertised
    // it. A linter that answers the unsupported method with an error contributed a
    // wasted round trip per invocation; one that answers with junk contributed junk.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-docsym-gate");
    let uri = file_uri(dir.as_path());
    let sym = |name: &str| {
        format!(
            r#"[ {{ "name": "{name}", "kind": 12, "location": {{ "uri": "{uri}",
                 "range": {{ "start": {{ "line": 0, "character": 0 }},
                             "end": {{ "line": 0, "character": 3 }} }} }} }} ]"#
        )
    };
    // alpha withholds the provider but would still answer if asked — so a symbol
    // named ALPHA-SYM in the result proves the editor asked a server that said no.
    arm_mock_named(
        dir.as_path(),
        "alpha",
        &format!(
            r#"{{ "capabilities": {{ "documentSymbolProvider": false }},
                  "document_symbols": {} }}"#,
            sym("ALPHA-SYM")
        ),
    );
    arm_mock_named(
        dir.as_path(),
        "beta",
        &format!(r#"{{ "document_symbols": {} }}"#, sym("BETA-SYM")),
    );
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(
        &rpc,
        "_G.syms = nil\n\
         nx.lsp.document_symbol():next(function(items)\n\
         \x20 local n = {}\n\
         \x20 for _, it in ipairs(items or {}) do n[#n+1] = it.text end\n\
         \x20 table.sort(n)\n\
         \x20 _G.syms = table.concat(n, ' | ')\n\
         end)",
    )
    .await;
    let only_beta = await_lua_eq(
        &rpc,
        "tostring(_G.syms ~= nil and _G.syms:find('BETA-SYM', 1, true) ~= nil \
         and _G.syms:find('ALPHA-SYM', 1, true) == nil)",
        "true",
    )
    .await;
    let got = exec_lua(&rpc, "return tostring(_G.syms)").await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        only_beta,
        "only the server advertising documentSymbolProvider is asked, got {:?}",
        got.as_str()
    );
}

#[tokio::test]
async fn workspace_symbols_merge_from_every_capable_server() {
    // `workspace/symbol` was the one list-shaped kind left on the single-target
    // path, answered by the buffer's first server alone — although merging its
    // results is exactly as well-defined as merging `documentSymbol`'s, which
    // fans out. Two servers indexing one project each know symbols the other
    // doesn't; asking one silently halves the picker.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-wssym-merge");
    let uri = file_uri(dir.as_path());
    let sym = |name: &str| {
        format!(
            r#"{{ "workspace_symbols": [ {{ "name": "{name}", "kind": 12,
                 "location": {{ "uri": "{uri}",
                   "range": {{ "start": {{ "line": 0, "character": 0 }},
                               "end": {{ "line": 0, "character": 3 }} }} }} }} ] }}"#
        )
    };
    arm_mock_named(dir.as_path(), "alpha", &sym("ALPHA-WS"));
    arm_mock_named(dir.as_path(), "beta", &sym("BETA-WS"));
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(
        &rpc,
        "_G.ws = nil\n\
         nx.lsp.workspace_symbol('x'):next(function(items)\n\
         \x20 local n = {}\n\
         \x20 for _, it in ipairs(items or {}) do n[#n+1] = it.text end\n\
         \x20 table.sort(n)\n\
         \x20 _G.ws = table.concat(n, ' | ')\n\
         end)",
    )
    .await;
    let both = await_lua_eq(
        &rpc,
        "tostring(_G.ws ~= nil and _G.ws:find('ALPHA-WS', 1, true) ~= nil \
         and _G.ws:find('BETA-WS', 1, true) ~= nil)",
        "true",
    )
    .await;
    let got = exec_lua(&rpc, "return tostring(_G.ws)").await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        both,
        "both servers' workspace symbols merge into one list, got {:?}",
        got.as_str()
    );
}

#[tokio::test]
async fn lsp_info_reports_every_server_on_the_buffer() {
    // `:LspInfo` is the introspection surface for exactly the thing that became
    // plural, and its "Current buffer" block still described one server — the
    // first — so the encoding, sync kind, document version and diagnostic count it
    // showed were half the story, with no hint that a second server was attached.
    // ("Running servers" listed both all along, which is what made the header's
    // silence misleading rather than merely incomplete.)
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-info-servers");
    arm_mock_named(dir.as_path(), "alpha", "{}");
    arm_mock_named(dir.as_path(), "beta", "{}");
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    enable_alpha_beta(&rpc).await;

    exec_lua(&rpc, "nx.cmd('LspInfo')").await;
    // Only the block above "Running servers" — that section names both regardless.
    let listed = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local head = {}\n\
         \x20 for _, l in ipairs(nx.buf.lines(0, 0, -1)) do\n\
         \x20   if l == 'Running servers' then break end\n\
         \x20   head[#head+1] = l\n\
         \x20 end\n\
         \x20 local s = table.concat(head, '\\n')\n\
         \x20 return (s:find('alpha', 1, true) and 'a' or '') ..\n\
         \x20        (s:find('beta', 1, true) and 'b' or '')\n\
         end)()",
        "ab",
    )
    .await;

    std::env::remove_var("NXVIM_LSP_CMD_ALPHA");
    std::env::remove_var("NXVIM_LSP_CMD_BETA");

    assert!(
        listed,
        "the current-buffer block names every attached server, not just the first"
    );
}
