//! Behavior tests for the **`btv.lsp` control surface** (Phase A,
//! docs/specs/2026-06-14-btv-lsp-design.md): the declarative config registry
//! (`btv.lsp.config` / `btv.lsp.enable`), the engine-side FileType → Start dispatch,
//! the `on_attach` lifecycle hook, and `btv.lsp.clients` introspection — all over
//! the intact engine.
//!
//! Wired like `lsp_float.rs`: the scripted mock language server (`bemtvi
//! --__lsp-mock`, `bemtvi_lsp::mock`) stands in for a real server, `$BEMTVI_LSP_CMD`
//! overrides the spawn argv (so the config's `cmd` can be a placeholder), and the
//! `rust`-filetype buffer drives the dispatch. The process-global env means these
//! tests serialize on `serial_lock`.

use std::path::Path;
use std::time::Duration;

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{
    attach, command, drain_to_latest_redraw, exec_lua, feed, map_get, serial_lock, spawn, temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const BEMTVI_BIN: &str = env!("CARGO_BIN_EXE_bemtvi");

/// Write a mock LSP script and point `$BEMTVI_LSP_CMD` at the binary's
/// `--__lsp-mock` mode. The caller holds `serial_lock`.
fn arm_mock(dir: &Path, script: &str) {
    std::fs::write(dir.join("mock.json"), script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        "BEMTVI_LSP_CMD",
        format!("{BEMTVI_BIN} --__lsp-mock {}/mock.json", dir.display()),
    );
}

/// Open a `.rs` buffer (filetype `rust`, `foo` under the cursor) and attach — but,
/// unlike `lsp_float.rs::start`, do **not** start a server: the test drives the
/// declarative `btv.lsp.config` / `btv.lsp.enable` path itself.
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
    bemtvi_test_harness::barrier(rpc).await;
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

/// [`await_float`] for a test that needs the *styling* as well as the text: retry
/// `trigger` until a float window carries a line containing `want`, returning the whole
/// redraw (for its `styles` palette) alongside the window.
async fn await_float_redraw(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
    trigger: &str,
    want: &str,
) -> (Vec<(Value, Value)>, Vec<(Value, Value)>) {
    for _ in 0..200 {
        exec_lua(rpc, trigger).await;
        bemtvi_test_harness::barrier(rpc).await;
        if let Some(map) = drain_to_latest_redraw(incoming, |m| floating_window(m).is_some()) {
            let win = floating_window(&map).expect("a floating window");
            if window_lines(&win).iter().any(|l| l.contains(want)) {
                return (map, win);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the float window never contained {want:?}");
}

/// One window row's highlight spans as `(start column, group)`, in wire order — the
/// `highlights` key carries `[start, end, group, style_id]` per span.
fn row_spans(win: &[(Value, Value)], row: usize) -> Vec<(u64, String)> {
    let Some(rows) = map_get(win, "highlights").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(spans) = rows.get(row).and_then(Value::as_array) else {
        return Vec::new();
    };
    spans
        .iter()
        .filter_map(Value::as_array)
        .filter_map(|s| Some((s[0].as_u64()?, s[2].as_str()?.to_string())))
        .collect()
}

/// The resolved style of the `line_fill` overlay chunk on window row `row` — the `─`
/// run a section header's rule continues out to the float's edge with — looked up in
/// the frame's `styles` palette.
fn header_fill_style(
    redraw: &[(Value, Value)],
    win: &[(Value, Value)],
    row: usize,
) -> Option<Vec<(Value, Value)>> {
    let styles = map_get(redraw, "styles")?.as_array()?;
    let rows = map_get(win, "virt_text")?.as_array()?;
    let id = rows
        .get(row)?
        .as_array()?
        .iter()
        .filter_map(Value::as_array)
        .filter(|p| p[0].as_u64() == Some(2))
        .filter_map(|p| p[3].as_array()?.first()?.as_array()?[1].as_u64())
        .next()?;
    match styles.get(id as usize)? {
        Value::Map(m) => Some(m.clone()),
        _ => None,
    }
}

/// A color channel (`fg` / `bg`) of a wire style map as `0xRRGGBB`, or `None` when the
/// style leaves it unset.
fn hl_color(style: &[(Value, Value)], key: &str) -> Option<u64> {
    style
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
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

/// `btv.lsp.config(name, …)` accumulates across calls with neovim deep-merge
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
        btv.lsp.config("svr", { cmd = { "a" }, settings = { foo = { bar = 1 } }, filetypes = { "rust" } })
        btv.lsp.config("svr", { settings = { foo = { baz = 2 } }, filetypes = { "c" } })
        local c = btv.lsp._config["svr"]
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
        btv.lsp.config("tools", { settings = { languages = { lua = {
          { lintCommand = "luacheck -" }, { formatCommand = "stylua -" },
        } } } })
        btv.lsp.config("tools", { settings = { languages = { lua = {
          { formatCommand = "stylua -" },
        } } } })
        local l = btv.lsp._config["tools"].settings.languages.lua
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
    let errored = exec_lua(&rpc, "return tostring(pcall(btv.lsp.config, 123, {}))").await;
    assert_eq!(
        errored.as_str(),
        Some("false"),
        "config rejects a non-string name"
    );
}

/// The full declarative path: `config` + `enable` on a matching filetype starts the
/// server (engine-side FileType → Start dispatch) and a language verb's reply lands
/// on its surface — `btv.lsp.hover()` opening the hover float window.
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
        btv.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_float(&rpc, &mut incoming, "btv.lsp.hover()", "scripted hover").await;
    assert!(
        lines.iter().any(|l| l.contains("foo")),
        "hover float should carry the markup, got {lines:?}"
    );
    // The lone contributor renders bare: no `─ mock ────` section rule, which on a
    // one-server buffer (the common case) would head every hover with the only name
    // there is.
    assert!(
        lines.iter().all(|l| !l.starts_with('─')),
        "a single server's hover takes no section heading, got {lines:?}"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// `btv.lsp.enable` catches up on **every** buffer already read, not just the current
/// one. A server enabled lazily — a plugin that resolves its tools over the async
/// `btv.fs` seam before it registers the config, as bemtvi-efmls-configs does for efm —
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
        btv.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "#btv.lsp.clients({ bufnr = 2 })", "1").await,
        "the current buffer should carry the server"
    );
    // The buffer left behind was *bound* by the same catch-up, so going back to it
    // serves it — document sync is per current buffer, so its `didOpen` (and the
    // attach) lands the moment it is displayed again. Without the catch-up it stays
    // bound to nothing forever: nothing re-fires `FileType` for a buffer that has one.
    command(&rpc, "b 1").await;
    assert!(
        await_lua_eq(&rpc, "#btv.lsp.clients({ bufnr = 1 })", "1").await,
        "the buffer read BEFORE the enable must be caught up too"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// The built-in LSP keymaps are installed **buffer-local on `LspAttach`** by
/// `prelude/lsp.lua` (they are no longer Rust native defaults). Pressing the `K`
/// *key* — not calling `btv.lsp.hover()` — opens the hover float, proving the map was
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
        btv.lsp.config("mock", { cmd = { "placeholder" }, filetypes = { "rust" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    // Wait for the server to bind the buffer, so the `LspAttach`-installed `K` map
    // exists before we press it (pressing an unmapped `K` would do nothing useful).
    assert!(
        await_lua_eq(&rpc, "#btv.lsp.clients({ bufnr = 0 })", "1").await,
        "the mock server should have attached to the buffer"
    );

    // Press the *key*. It must fire `btv.lsp.hover()` via the buffer-local map and
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

    std::env::remove_var("BEMTVI_LSP_CMD");
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
        btv.lsp.config("*", { filetypes = { "rust" } })
        btv.lsp.config("mock", { cmd = { "placeholder" } })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_float(&rpc, &mut incoming, "btv.lsp.hover()", "star base").await;
    assert!(
        lines.iter().any(|l| l.contains("star base")),
        "the server should have started off the '*' filetypes, got {lines:?}"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// A config with `root_markers` (no explicit `root_dir`) drives the async upward
/// `find_root` walk through the `btv.fs` seam, then starts the server. This is the
/// path the `ui-complete-lsp` example takes — and the one that regressed when
/// `find_root` returned the `btv.async` *wrapper* instead of calling it for a promise
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
        btv.lsp.config("mock", {
          cmd = { "placeholder" },
          filetypes = { "rust" },
          root_markers = { ".git" },
        })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    let lines = await_float(&rpc, &mut incoming, "btv.lsp.hover()", "rooted hover").await;
    assert!(
        lines.iter().any(|l| l.contains("rooted hover")),
        "the server should have started via the root_markers path, got {lines:?}"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// `on_attach(client, bufnr)` runs once the server binds the buffer (via the engine's
/// `LspAttach`), and `btv.lsp.clients({ bufnr })` then lists the attached client with
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
        _G.bemtvi_attach_log = nil
        btv.lsp.config("mock", {
          cmd = { "placeholder" },
          filetypes = { "rust" },
          on_attach = function(client, bufnr)
            _G.bemtvi_attach_log = client.name .. ":" .. tostring(bufnr)
          end,
        })
        btv.lsp.enable("mock")
        "#,
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.bemtvi_attach_log", "mock:1").await,
        "on_attach should have run with the client and bufnr"
    );

    // The attached client is now introspectable, filtered to the buffer.
    let info = exec_lua(
        &rpc,
        r#"
        local cs = btv.lsp.clients({ bufnr = 0 })
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
        "btv.lsp.clients should surface the attached handle (name, caps, :request)"
    );

    std::env::remove_var("BEMTVI_LSP_CMD");
}

/// `btv.lsp.request` with no client attached fails loud (returns without dispatching,
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
          btv.lsp.request("textDocument/hover", {}, function() got = true end)
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

/// Point `$BEMTVI_LSP_CMD_<NAME>` at the mock with its own script, so two servers
/// are distinguishable. The blanket `$BEMTVI_LSP_CMD` cannot do this — it would aim
/// both at one script, and no assertion could tell which server answered.
fn arm_mock_named(dir: &Path, name: &str, script: &str) {
    let file = dir.join(format!("mock-{name}.json"));
    std::fs::write(&file, script).expect("write mock script");
    // SAFETY: serialized on `serial_lock`, so no other test races this env mutation.
    std::env::set_var(
        format!("BEMTVI_LSP_CMD_{}", name.to_uppercase()),
        format!("{BEMTVI_BIN} --__lsp-mock {}", file.display()),
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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );
}

/// Issue an `btv.lsp.*` verb and poll until its promise settles with a value, which is
/// returned as a string.
///
/// The verb is re-issued only while the promise keeps settling `nil` — the servers may
/// still be starting — and never *while* a round is open, since a second round
/// supersedes the first (settling it `nil`) and would race this poll forever. Panics
/// rather than returning empty, so a broken verb fails loudly here.
async fn await_verb_result(rpc: &Rpc, verb: &str) -> String {
    let issue = format!("_G.V = nil ({verb}):next(function(v) _G.V = v or false end)");
    for _ in 0..40 {
        exec_lua(rpc, &issue).await;
        for _ in 0..40 {
            let got = exec_lua(rpc, "return tostring(_G.V)").await;
            match got.as_str().unwrap_or_default() {
                "nil" => tokio::time::sleep(Duration::from_millis(25)).await,
                // Resolved `nil` — nothing answered yet; issue a fresh round.
                "false" => break,
                value => return value.to_string(),
            }
        }
    }
    panic!("{verb} never resolved with a value");
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
        bemtvi_test_harness::barrier(rpc).await;
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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");
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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");
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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // Panics if beta's hover never arrives — i.e. if the request went to alpha.
    let lines = await_float(&rpc, &mut incoming, "btv.lsp.hover()", "FROM-BETA").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // The chooser lists both servers' actions.
    let rows = code_action_rows(&rpc, &mut incoming, "btv.lsp.code_action()", 2).await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    let joined = rows.join(" | ");
    assert!(
        joined.contains("ALPHA-FIX") && joined.contains("BETA-REFACTOR"),
        "the chooser merged both servers' actions, got {joined:?}"
    );
}

#[tokio::test]
async fn format_selects_the_named_server() {
    // Phase 5. `format({ name = … })` was REJECTED while bemtvi modelled one server
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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // Name the SECOND server; alpha sorts first, so a default pick would use alpha.
    exec_lua(&rpc, "btv.lsp.format({ name = 'beta' })").await;
    let formatted = await_lua_eq(&rpc, "btv.buf.lines(0, 0, 1)[1]", "BY-BETA").await;

    // An unattached name must not silently format with someone else.
    let unknown = exec_lua(
        &rpc,
        "btv.lsp.format({ name = 'nosuch' })\n\
         return tostring(pcall(btv.lsp.format, { bogus = 1 }))",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
// `btv.lsp.hover{ name = … }` / `:LspHover <server>`: which of a buffer's attached
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
    bemtvi_test_harness::barrier(rpc).await;
    drain_to_latest_redraw(incoming, |m| !bemtvi_test_harness::message(m).is_empty())
        .map(|m| bemtvi_test_harness::message(&m))
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
        "btv.lsp.hover({ name = 'beta' })",
        "FROM-BETA",
    )
    .await;
    // …and the ex-command's bare argument is the same route, back to the other one
    // (so this leg can't pass by the float simply lingering).
    let ex = await_float(
        &rpc,
        &mut incoming,
        "btv.cmd('LspHover alpha')",
        "FROM-ALPHA",
    )
    .await;

    // …and the argument completes from the buffer's own clients, in its own slot —
    // `:LspRename` takes the new identifier first, so its route is the SECOND word.
    let completions = exec_lua(
        &rpc,
        "local function at(line)\n\
         \x20 local out = {}\n\
         \x20 for _, c in ipairs(btv._cmdline_complete_run(line, #line)) do\n\
         \x20   out[#out + 1] = c.insert\n\
         \x20 end\n\
         \x20 return table.concat(out, ',')\n\
         end\n\
         return at('LspHover ') .. '/' .. at('LspRename Foo ') .. '/' .. at('LspRename ')",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
async fn hover_merges_every_capable_server_in_priority_order() {
    // neovim's `vim.lsp.buf.hover` asks every client and composes one float; bemtvi
    // does the same, because on a `pyright` + `ruff` buffer each server knows
    // something the other doesn't. `priority` then decides the ORDER — which neovim
    // has no answer for (it walks an unordered client table).
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-hover-merge");
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
    // beta outranks alpha, so it must lead — the reverse of the alphabetical default.
    exec_lua(
        &rpc,
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' }, priority = 1 })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' }, priority = 9 })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    let lines = await_float(&rpc, &mut incoming, "btv.lsp.hover()", "FROM-ALPHA").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    let joined = lines.join("\n");
    assert!(
        joined.contains("FROM-BETA"),
        "one float carries BOTH servers' hovers, got {lines:?}"
    );
    let beta_at = joined.find("FROM-BETA").expect("beta section");
    let alpha_at = joined.find("FROM-ALPHA").expect("alpha section");
    assert!(
        beta_at < alpha_at,
        "the higher-priority server's section leads, got {lines:?}"
    );
    // Each section is headed by a labelled rule naming its client — `─ alpha ───…`,
    // the float-title shape — or the reader can't tell which server made which claim,
    // and the two hovers run together with nothing between them. The trailing `─` run
    // is a `line_fill` overlay the client draws to the float's width, so the buffer
    // line is just the label (the `assert` below pins the fill itself).
    let heading = |name: &str| lines.iter().position(|l| l.trim() == format!("─ {name}"));
    let (alpha_row, beta_row) = (heading("alpha"), heading("beta"));
    assert!(
        alpha_row.is_some() && beta_row.is_some(),
        "each section is headed by a labelled rule naming its client, got {lines:?}"
    );
    // ...and the section's content starts on the very next row: the rule already
    // separates, so a blank row under it would read as a detached heading.
    assert_eq!(
        lines
            .get(beta_row.expect("beta heading") + 1)
            .map(String::as_str),
        Some("FROM-BETA"),
        "the section's content hugs its rule, got {lines:?}"
    );
    let alpha_row = alpha_row.expect("alpha heading");
    assert_eq!(
        lines.get(alpha_row + 1).map(String::as_str),
        Some("FROM-ALPHA"),
        "the section's content hugs its rule, got {lines:?}"
    );
    // Above the rule, though, one blank row parts it from the section before, so the
    // previous server's last line doesn't run into the next title. The float's own top
    // border does that job for the first section, which takes none.
    assert_eq!(
        lines.get(alpha_row - 1).map(String::as_str),
        Some(""),
        "a blank row parts the second section from the first, got {lines:?}"
    );
    assert_eq!(
        beta_row,
        Some(0),
        "the first section takes no leading blank"
    );
}

/// A merged round where each server's signature is laid out **vertically** (both
/// carry parameters): a `<client>: ` prefix cannot survive a split signature — it
/// would push the parameters out of alignment and repeat the name on every row — so
/// each block takes a labelled-rule heading row (`─ alpha `, the hover float's section
/// header; the `─` run out to the float's edge is a `line_fill` overlay).
#[tokio::test]
async fn merged_signatures_with_parameters_take_a_client_heading() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-sig-merge-vert");
    let sig = |name: &str| {
        format!(
            r#"{{ "signature_help": {{ "signatures": [
                 {{ "label": "{name}(x: int, y: int)",
                    "parameters": [ {{ "label": "x: int" }}, {{ "label": "y: int" }} ] }} ],
                 "activeSignature": 0, "activeParameter": 0 }} }}"#
        )
    };
    arm_mock_named(dir.as_path(), "alpha", &sig("alpha_sig"));
    arm_mock_named(dir.as_path(), "beta", &sig("beta_sig"));
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    exec_lua(
        &rpc,
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    let shown = await_verb_result(&rpc, "btv.lsp.signature_help()").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(
        lines,
        vec![
            "─ alpha ",
            "alpha_sig(",
            "    x: int,",
            "    y: int,",
            ")",
            "", // one blank row parts a block from the next; none above the first
            "─ beta ",
            "beta_sig(",
            "    x: int,",
            "    y: int,",
            ")",
        ],
        "each split signature is headed by its client, got {shown:?}"
    );
}

/// The section header is drawn as *float chrome*: its `─` rule — the leading glyph and
/// the fill that runs on to the float's edge — takes **`FloatBorder`**, the very group
/// the box around it is painted in, so the header reads as border inset with a title
/// rather than as a third colour inside the popup. The client's name then has to be an
/// accent **distinct from that border** (`Special`) or it disappears into the rule it
/// is inset in — under a theme like catppuccin, whose `FloatBorder` and headings are
/// both blue, an unchanged heading group would leave `─ alpha ─────` one flat colour.
#[tokio::test]
async fn a_section_header_rules_in_the_float_border_colour_and_labels_in_an_accent() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-sig-header-hl");
    let sig = |name: &str| {
        format!(
            r#"{{ "signature_help": {{ "signatures": [
                 {{ "label": "{name}(x: int, y: int)",
                    "parameters": [ {{ "label": "x: int" }}, {{ "label": "y: int" }} ] }} ],
                 "activeSignature": 0, "activeParameter": 0 }} }}"#
        )
    };
    arm_mock_named(dir.as_path(), "alpha", &sig("alpha_sig"));
    arm_mock_named(dir.as_path(), "beta", &sig("beta_sig"));
    let (rpc, mut incoming) = open_rust(dir.as_path()).await;
    // Two float-chrome colours a theme would define; the accent is a third, so the
    // assertions can tell the rule, the label, and the popup body apart.
    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'FloatBorder', { fg = '#89b4fa', bg = '#181825' })\n\
         vim.api.nvim_set_hl(0, 'NormalFloat', { fg = '#cdd6f4', bg = '#181825' })\n\
         vim.api.nvim_set_hl(0, 'Special',     { fg = '#f5c2e7' })\n\
         btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    let (redraw, win) =
        await_float_redraw(&rpc, &mut incoming, "btv.lsp.signature_help()", "─ alpha").await;
    let header = window_lines(&win)
        .iter()
        .position(|l| l.starts_with("─ alpha"))
        .expect("the header row");

    // The rule glyph leading the label, and the label itself.
    let spans = row_spans(&win, header);
    assert_eq!(
        spans.first().map(|(start, group)| (*start, group.as_str())),
        Some((0, "FloatBorder")),
        "the header's leading rule takes the float's own border group, got {spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|(start, group)| *start > 0 && group == "Special"),
        "the client's name takes an accent distinct from the rule, got {spans:?}"
    );

    // ...and the `─` fill running from the label to the float's edge is the same
    // border colour as the leading glyph — one rule, not two halves.
    let fill = header_fill_style(&redraw, &win, header).expect("the header's fill chunk style");
    assert_eq!(
        hl_color(&fill, "fg"),
        Some(0x89b4fa),
        "the fill continues the rule in the border colour, got {fill:?}"
    );

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");
}

#[tokio::test]
async fn signature_help_merges_every_capable_server() {
    // Same argument as hover: two servers can both describe the call under the cursor,
    // and showing one silently hides the other. bemtvi shows them together (labelled)
    // where neovim shows one at a time with `<C-s>` to cycle — the float here is
    // passive, so there is no mode to leave. A parameterless signature like these
    // stays on one line, headed by its client's labelled rule like any other block.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-sig-merge");
    let sig = |label: &str| {
        format!(
            r#"{{ "signature_help": {{ "signatures": [ {{ "label": "{label}" }} ],
                 "activeSignature": 0 }} }}"#
        )
    };
    arm_mock_named(dir.as_path(), "alpha", &sig("alpha_sig(x)"));
    arm_mock_named(dir.as_path(), "beta", &sig("beta_sig(y)"));
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    exec_lua(
        &rpc,
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' }, priority = 9 })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // The promise resolves with the shown text, so assert on that rather than on the
    // float's pixels (`lsp_float.rs` covers the rendering).
    let shown = await_verb_result(&rpc, "btv.lsp.signature_help()").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    assert!(
        shown.contains("alpha_sig(x)") && shown.contains("beta_sig(y)"),
        "both servers' signatures are shown, got {shown:?}"
    );
    assert!(
        shown.find("beta_sig").unwrap() < shown.find("alpha_sig").unwrap(),
        "in priority order (beta outranks alpha), got {shown:?}"
    );
    // Each block is headed by its client's labelled rule, on its own row — and the
    // signature it heads follows immediately, with no blank row between.
    let lines: Vec<&str> = shown.lines().collect();
    assert_eq!(
        lines,
        vec!["─ beta ", "beta_sig(y)", "", "─ alpha ", "alpha_sig(x)"],
        "each signature is headed by its client's rule, got {shown:?}"
    );
}

/// A **lone** contributor's signature renders bare: naming the only server there is
/// would put a rule on every signature popup in the common one-server buffer.
#[tokio::test]
async fn a_single_servers_signature_takes_no_client_heading() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-sig-solo");
    arm_mock_named(
        dir.as_path(),
        "alpha",
        r#"{ "signature_help": { "signatures": [ { "label": "solo_sig(x)" } ],
             "activeSignature": 0 } }"#,
    );
    let (rpc, _incoming) = open_rust(dir.as_path()).await;
    exec_lua(
        &rpc,
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.enable({ 'alpha' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "1").await,
        "the server attached"
    );

    let shown = await_verb_result(&rpc, "btv.lsp.signature_help()").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");

    assert_eq!(
        shown, "solo_sig(x)",
        "the lone signature shows bare — no client heading, got {shown:?}"
    );
}

/// A `"definition"` mock script pointing at 0-based `line` of `uri`.
fn definition_at(uri: &str, line: u32) -> String {
    format!(
        r#"{{ "definition": {{ "uri": "{uri}", "range": {{
             "start": {{ "line": {line}, "character": 0 }},
             "end": {{ "line": {line}, "character": 1 }} }} }} }}"#
    )
}

#[tokio::test]
async fn goto_merges_both_servers_places() {
    // The goto family merges like references: a definition can genuinely live in two
    // places to two servers (a generated stub and its source, a `.d.ts` and its
    // implementation), and asking only the first silently hides one of them. Each mock
    // names a DIFFERENT line, so a round that reached one server resolves 1 item.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-goto-merge");
    let uri = format!("file://{}", dir.as_path().join("a.rs").display());
    arm_mock_named(dir.as_path(), "alpha", &definition_at(&uri, 1));
    arm_mock_named(dir.as_path(), "beta", &definition_at(&uri, 3));
    let (rpc, _incoming) = open_rust_with(dir.as_path(), "one\ntwo\nthree\nfour\n").await;
    enable_alpha_beta(&rpc).await;

    let items = await_verb_result(
        &rpc,
        "btv.lsp.definition():next(function(i) return i and #i end)",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    assert_eq!(
        items, "2",
        "both servers' definitions are in the merged list"
    );
}

#[tokio::test]
async fn goto_still_jumps_when_the_merged_list_holds_one_place() {
    // The behavior that must NOT change on the way to merging: when the merged list
    // holds one place — a one-server buffer, or two servers that agree — `gd` still
    // jumps rather than opening a picker over a list of one. Both mocks name the same
    // line, so the dedup (on CONVERTED byte positions) has to collapse them.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-goto-jump");
    let uri = format!("file://{}", dir.as_path().join("a.rs").display());
    arm_mock_named(dir.as_path(), "alpha", &definition_at(&uri, 2));
    arm_mock_named(dir.as_path(), "beta", &definition_at(&uri, 2));
    let (rpc, _incoming) = open_rust_with(dir.as_path(), "one\ntwo\nthree\nfour\n").await;
    enable_alpha_beta(&rpc).await;

    let items = await_verb_result(
        &rpc,
        "btv.lsp.definition():next(function(i) return i and #i end)",
    )
    .await;
    // `cursor()` is 1-based, so the target's 0-based line 2 reads as row 3.
    let mut jumped = false;
    for _ in 0..80 {
        if bemtvi_test_harness::cursor(&rpc).await.0 == 3 {
            jumped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    assert_eq!(
        items, "1",
        "two servers naming the SAME place merge to one item, not two"
    );
    assert!(jumped, "and a single place still jumps the cursor to it");
}

#[tokio::test]
async fn priority_picks_the_default_server_for_a_single_target_verb() {
    // The other half of `priority`: not just presentation order, but WHICH server a
    // verb that can only go to one asks. Each mock rewrites line 1 to its own marker,
    // so the buffer text says who formatted. `alpha` sorts first, so an ignored
    // priority formats with alpha.
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-priority-pick");
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
        "btv.lsp.config('alpha', { cmd = { 'unused' }, filetypes = { 'rust' } })\n\
         btv.lsp.config('beta',  { cmd = { 'unused' }, filetypes = { 'rust' }, priority = 10 })\n\
         btv.lsp.enable({ 'alpha', 'beta' })",
    )
    .await;
    assert!(
        await_lua_eq(&rpc, "#vim.lsp.get_clients({ bufnr = 0 })", "2").await,
        "both servers attached"
    );

    // No `name`: the pick is the ranking's alone.
    exec_lua(&rpc, "btv.lsp.format()").await;
    let formatted = await_lua_eq(&rpc, "btv.buf.lines(0, 0, 1)[1]", "BY-BETA").await;
    // `:LspInfo` explains the pick — it lists in routing order with the rank. The
    // command is deferred (it opens a scratch listing), so drain a tick before reading
    // the buffer it left focused.
    command(&rpc, "LspInfo").await;
    let info = exec_lua(&rpc, "return table.concat(btv.buf.lines(0, 0, 20), '\\n')").await;
    // A non-integer rank is a config that meant something and didn't say it.
    let bad = exec_lua(
        &rpc,
        "return tostring(pcall(btv.lsp.config, 'x', { priority = 'high' }))",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    assert!(
        formatted,
        "the highest-priority capable server formatted, not the alphabetical default"
    );
    let info = info.as_str().unwrap_or_default();
    let beta_at = info.find("server:      beta");
    let alpha_at = info.find("server:      alpha");
    assert!(
        beta_at.is_some() && alpha_at.is_some() && beta_at < alpha_at,
        ":LspInfo lists in routing order, got:\n{info}"
    );
    assert!(
        info.contains("priority:    10") && info.contains("priority:    0  (default)"),
        ":LspInfo shows each rank and marks the default, got:\n{info}"
    );
    assert_eq!(
        bad.as_str(),
        Some("true"),
        "a bad priority is caught at start, not at registration"
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
        "btv.lsp.hover({ name = 'nosuch' })\n\
         return tostring(pcall(btv.lsp.hover, { bogus = 1 }))",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
        "btv.lsp.code_action({ name = 'beta' })",
        1,
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
        "(btv.lsp.semantic_tokens.get_at_pos(0, 0, 0)[1] or {}).type",
        "keyword",
    )
    .await;
    let beta_token = await_lua_eq(
        &rpc,
        "(btv.lsp.semantic_tokens.get_at_pos(0, 0, 4)[1] or {}).type",
        "variable",
    )
    .await;
    // Byte 8 is the *second* byte of the trailing `ö`. beta's token spans utf-16
    // units 4..7 = bytes 4..9, so it covers byte 8 — but only when decoded at
    // beta's OWN negotiated encoding. Decoded at alpha's utf-8 it would stop at
    // byte 7 and this column would be bare.
    let beta_encoding = await_lua_eq(
        &rpc,
        "(btv.lsp.semantic_tokens.get_at_pos(0, 0, 8)[1] or {}).type",
        "variable",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
         \x20 for _, h in ipairs(btv.lsp.inlay_hint.get({ bufnr = 0 })) do\n\
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
         \x20 for _, h in ipairs(btv.lsp.inlay_hint.get({ bufnr = 0 })) do\n\
         \x20   if not ids[h.client_id] then ids[h.client_id] = true; n = n + 1 end\n\
         \x20 end\n\
         \x20 return n\n\
         end)()",
        "2",
    )
    .await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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

    exec_lua(&rpc, "btv.lsp.format({ name = 'beta' })").await;
    let formatted = await_lua_eq(&rpc, "btv.buf.lines(0, 0, 1)[1]", "let X = 1").await;
    let line = exec_lua(&rpc, "return btv.buf.lines(0, 0, 1)[1]").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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

    let rows = code_action_rows(&rpc, &mut incoming, "btv.lsp.code_action()", 2).await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
         btv.lsp.references():next(function(items)\n\
         \x20 local c = {}\n\
         \x20 for _, it in ipairs(items or {}) do c[#c+1] = it.col end\n\
         \x20 table.sort(c)\n\
         \x20 _G.cols = table.concat(c, ',')\n\
         end)",
    )
    .await;
    let merged = await_lua_eq(&rpc, "_G.cols", "5,11").await;
    let got = exec_lua(&rpc, "return tostring(_G.cols)").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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

    exec_lua(&rpc, "btv.lsp.rename('X')").await;
    let renamed = await_lua_eq(&rpc, "btv.buf.lines(0, 0, 1)[1]", "let X = 1").await;
    let line = exec_lua(&rpc, "return btv.buf.lines(0, 0, 1)[1]").await;

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
    exec_lua(&rpc, "btv.lsp.code_action({ apply = true })").await;

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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
    // The other half of `btv.lsp.commands`: some code-action commands are defined to
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
         btv.lsp.commands['beta.doIt'] = function(command, ctx)\n\
         \x20 local client = btv.lsp._clients[ctx.client_id]\n\
         \x20 _G.ran = (client and client.name or '?') .. '/' .. tostring(command.arguments[1])\n\
         end\n\
         btv.lsp.code_action({ apply = true })",
    )
    .await;
    let handled = await_lua_eq(&rpc, "_G.ran", "beta/ARG-ONE").await;
    // Give a stray round trip time to show up in the record before asserting on it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let executed = std::fs::read_to_string(&rec)
        .unwrap_or_default()
        .contains("workspace/executeCommand");

    // Registered on a shared table, so the next test in this process would inherit it.
    exec_lua(&rpc, "btv.lsp.commands['beta.doIt'] = nil").await;
    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
    exec_lua(&rpc, "btv.lsp.signature_help_autotrigger(true)").await;

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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
         btv.lsp.document_symbol():next(function(items)\n\
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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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
         btv.lsp.workspace_symbol('x'):next(function(items)\n\
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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

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

    exec_lua(&rpc, "btv.cmd('LspInfo')").await;
    // Only the block above "Running servers" — that section names both regardless.
    let listed = await_lua_eq(
        &rpc,
        "(function()\n\
         \x20 local head = {}\n\
         \x20 for _, l in ipairs(btv.buf.lines(0, 0, -1)) do\n\
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

    std::env::remove_var("BEMTVI_LSP_CMD_ALPHA");
    std::env::remove_var("BEMTVI_LSP_CMD_BETA");

    assert!(
        listed,
        "the current-buffer block names every attached server, not just the first"
    );
}

/// Open `dir/<sub>/<stem>.rs` as the startup buffer, with the mock armed but no
/// server enabled yet. Used by the rootless tests, which need the file to sit in a
/// *subdirectory* so a per-directory root is distinguishable from the temp root.
async fn open_rust_at(path: &Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
    std::fs::write(path, "let foo = bar()\n").expect("write test file");
    let init = ServerInit {
        file: Some(path.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Enable the `mock` config with markers that cannot resolve (nothing named
/// `.bemtvi-marker` exists anywhere above a temp dir), so every buffer takes the
/// no-root-found path.
async fn enable_markerless_mock(rpc: &Rpc) {
    exec_lua(
        rpc,
        r#"
        btv.lsp.config("mock", {
          cmd = { "placeholder" },
          filetypes = { "rust" },
          root_markers = { ".bemtvi-marker" },
        })
        btv.lsp.enable("mock")
        "#,
    )
    .await;
}

/// The `Running servers` lines off a fresh `:LspInfo`.
async fn running_server_lines(rpc: &Rpc) -> Vec<String> {
    exec_lua(rpc, "btv.cmd('LspInfo')").await;
    let got = exec_lua(
        rpc,
        "return (function()\n\
         \x20 local out, seen = {}, false\n\
         \x20 for _, l in ipairs(btv.buf.lines(0, 0, -1)) do\n\
         \x20   if seen and l:find('mock', 1, true) then out[#out+1] = l end\n\
         \x20   if l == 'Running servers' then seen = true end\n\
         \x20 end\n\
         \x20 return table.concat(out, '\\n')\n\
         end)()",
    )
    .await;
    got.as_str()
        .unwrap_or("")
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Markerless buffers in DIFFERENT directories share ONE rootless server instead of
/// getting one instance apiece, rooted at each file's own directory.
///
/// bemtvi used to substitute the file's parent directory when the `root_markers` walk
/// came up empty, which made every such directory its own `ServerKey`. Jumping into a
/// dependency — a stdlib stub under `~/.cache/uv`, say — therefore started a *second*
/// full set of language servers and handed each one that directory as `rootUri`, so
/// they indexed a tree the user never opened. neovim leaves `config.root_dir` nil
/// (`vim.lsp.start`) and `reuse_client_default` reuses any rootless client of the same
/// name; this is that behavior.
#[tokio::test]
async fn markerless_buffers_share_one_rootless_server() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-cfg-rootless");
    let rec = dir.join("rec.jsonl");
    // Every instance records to the SAME file, so counting `initialize` lines counts
    // instances — which is the claim, and unlike `:LspInfo`'s buffer tally it does not
    // depend on which buffer happens to be current (`sync_lsp` opens documents for the
    // current buffer only).
    arm_mock(
        dir.as_path(),
        &format!(r#"{{ "record": "{}" }}"#, rec.display()),
    );
    let count = |method: &str| {
        std::fs::read_to_string(&rec)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(&format!(r#""method":"{method}""#)))
            .count()
    };
    let (rpc, _incoming) = open_rust_at(&dir.join("a").join("one.rs")).await;
    enable_markerless_mock(&rpc).await;
    // The first buffer must reach `didOpen` while it is still current.
    for _ in 0..200 {
        if count("textDocument/didOpen") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // A second markerless buffer, in a different directory.
    let second = dir.join("b").join("two.rs");
    std::fs::create_dir_all(second.parent().unwrap()).expect("mkdir b");
    std::fs::write(&second, "let baz = qux()\n").expect("write second file");
    exec_lua(&rpc, &format!("btv.cmd('e {}')", second.display())).await;
    for _ in 0..200 {
        if count("textDocument/didOpen") == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let opened = count("textDocument/didOpen");
    let started = count("initialize");
    let lines = running_server_lines(&rpc).await;

    std::env::remove_var("BEMTVI_LSP_CMD");

    assert_eq!(opened, 2, "both buffers should have opened a document");
    assert_eq!(
        started, 1,
        "both markerless buffers share ONE server instance, not one per directory"
    );
    assert_eq!(
        lines.len(),
        1,
        "`:LspInfo` lists that single instance once, got {lines:?}"
    );
    assert!(
        !lines[0].contains(&dir.join("a").display().to_string()),
        "a rootless server must not be rooted at a file's own directory, got {lines:?}"
    );
}

/// A rootless server is initialized with **neither** root spelling — no `rootUri` and
/// no `workspaceFolders`, which is single-file mode: the protocol's way of saying
/// "there is no workspace here". Sending the file's own directory in either field
/// tells the server to treat that directory as a project and index it. Both are
/// asserted because bemtvi sends both when there IS a root (pyright reads only
/// `workspaceFolders`, older servers only `rootUri`), so either one alone leaking a
/// root would put the server back to indexing a tree nobody opened.
#[tokio::test]
async fn a_rootless_server_is_initialized_without_a_root_uri() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-cfg-norooturi");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        dir.as_path(),
        &format!(r#"{{ "record": "{}" }}"#, rec.display()),
    );
    let (rpc, _incoming) = open_rust_at(&dir.join("a").join("one.rs")).await;
    enable_markerless_mock(&rpc).await;

    // Parsed inside the poll, not after it: `initialize` is a long line and the read
    // can catch the mock mid-append, so a half-written line means "not yet", not a
    // malformed record.
    let mut init = None;
    for _ in 0..200 {
        init = std::fs::read_to_string(&rec)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(r#""method":"initialize""#))
            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok());
        if init.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    std::env::remove_var("BEMTVI_LSP_CMD");

    let params = init.expect("the mock should have recorded `initialize`");
    let empty = |field: &str| {
        let got = params.pointer(&format!("/params/{field}")).cloned();
        assert!(
            matches!(got, None | Some(serde_json::Value::Null)),
            "a markerless buffer must not hand the server a {field}, got {got:?}"
        );
    };
    empty("rootUri");
    empty("workspaceFolders");
}

/// Collapsing rootless buffers onto one instance must not collapse them onto a
/// *rooted* one: a project server keeps its own root (and its own child), and the
/// out-of-tree buffer gets the rootless instance beside it. This is the shape of the
/// real report — a project buffer plus a jumped-into stdlib stub.
#[tokio::test]
async fn a_rooted_server_and_a_rootless_one_are_separate_instances() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-cfg-rooted-plus");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        dir.as_path(),
        &format!(r#"{{ "record": "{}" }}"#, rec.display()),
    );
    // `a/` is a project (it holds the marker); `b/` is out of tree.
    let project = dir.join("a");
    std::fs::create_dir_all(project.join(".bemtvi-marker")).expect("mkdir marker");
    let (rpc, _incoming) = open_rust_at(&project.join("one.rs")).await;
    enable_markerless_mock(&rpc).await;

    let inits = |want: usize| {
        let rec = rec.clone();
        async move {
            for _ in 0..200 {
                let found: Vec<serde_json::Value> = std::fs::read_to_string(&rec)
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.contains(r#""method":"initialize""#))
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect();
                if found.len() >= want {
                    return found;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Vec::new()
        }
    };
    assert_eq!(inits(1).await.len(), 1, "the project server should start");

    let loose = dir.join("b").join("two.rs");
    std::fs::create_dir_all(loose.parent().unwrap()).expect("mkdir b");
    std::fs::write(&loose, "let baz = qux()\n").expect("write loose file");
    exec_lua(&rpc, &format!("btv.cmd('e {}')", loose.display())).await;

    let found = inits(2).await;
    std::env::remove_var("BEMTVI_LSP_CMD");

    assert_eq!(
        found.len(),
        2,
        "the out-of-tree buffer starts its own server"
    );
    let roots: Vec<Option<&str>> = found
        .iter()
        .map(|v| v.pointer("/params/rootUri").and_then(|r| r.as_str()))
        .collect();
    assert!(
        roots.iter().any(|r| r.is_none()),
        "the out-of-tree buffer's server is rootless, got {roots:?}"
    );
    assert!(
        roots
            .iter()
            .any(|r| r.is_some_and(|r| r.ends_with(&project.display().to_string()))),
        "the project server keeps its own root, got {roots:?}"
    );
}

/// The `workspace/workspaceFolders` pull from a rootless server is answered with
/// null, not an invented folder. The push and the pull must agree, and the pull is
/// the one a server issues precisely when it distrusts what it was pushed — answering
/// it with the file's directory would hand back the workspace `initialize` just
/// declined to claim.
#[tokio::test]
async fn a_rootless_server_pulling_workspace_folders_gets_none() {
    let _guard = serial_lock().lock().await;
    let dir = temp_dir("lsp-cfg-nofolders");
    let rec = dir.join("rec.jsonl");
    arm_mock(
        dir.as_path(),
        &format!(
            r#"{{ "record": "{}", "workspace_folders_pull": true }}"#,
            rec.display()
        ),
    );
    let (rpc, _incoming) = open_rust_at(&dir.join("a").join("one.rs")).await;
    enable_markerless_mock(&rpc).await;

    let mut reply = None;
    for _ in 0..200 {
        reply = std::fs::read_to_string(&rec)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(r#""method":"_workspace_folders_response""#))
            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok());
        if reply.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    std::env::remove_var("BEMTVI_LSP_CMD");

    let reply = reply.expect("the client must answer workspace/workspaceFolders");
    let folders = reply.pointer("/params").cloned();
    assert!(
        matches!(folders, None | Some(serde_json::Value::Null)),
        "a rootless client has no folders to hand back, got {folders:?}"
    );
}
