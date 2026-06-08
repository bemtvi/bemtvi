//! Phase 7a, the real thing: drive the **vendored** nvim-lspconfig through
//! nxvim's `vim.lsp.config`/`vim.lsp.enable` framework. The server's runtimepath
//! points at `vendor/nvim-lspconfig`, so `vim.lsp.enable('lua_ls')` loads and runs
//! the project's *real* `lsp/lua_ls.lua` — its `cmd`, `filetypes`, and (nested)
//! `root_markers` — and starts a server. We swap the command for the scripted mock
//! (`$NXVIM_LSP_CMD`) so the test is hermetic, then assert the handshake carried
//! the **real** resolved root, the document opened with the right languageId, and
//! diagnostics flowed — proving the vendored config file actually drove the start.
//!
//! `lua_ls` is chosen deliberately: its config is pure data (`root_markers`), with
//! no `vim.system`/`cargo metadata` root logic (rust_analyzer/gopls) that nxvim's
//! minimal `vim.*` surface can't yet run.
//!
//! Skips cleanly (a passing no-op) when the submodule isn't checked out — populate
//! it with `git submodule update --init vendor/nvim-lspconfig`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{cursor_u64, drain_latest_redraw, feed, start_attached};
use rmpv::Value;
use serde_json::Value as Json;
use tokio::sync::mpsc::UnboundedReceiver;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// The checked-out nvim-lspconfig submodule, or `None` when it isn't populated.
fn lspconfig_dir() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/nvim-lspconfig");
    dir.join("lsp/lua_ls.lua").is_file().then_some(dir)
}

/// A unique temp path under `nxvim-lspconfig-test-<pid>/<tag>`.
fn scratch(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nxvim-lspconfig-{}-{tag}", std::process::id()))
}

/// Build a temp Lua "project": `.luarc.json` (a `lua_ls` root marker) at the root
/// and a source file one directory down, so resolving the root requires the
/// vendored config's upward `root_markers` search to actually walk a level.
/// Returns `(project_root, source_file)`.
fn make_project() -> (PathBuf, PathBuf) {
    let root = scratch("proj");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("mkdir project");
    std::fs::write(root.join(".luarc.json"), "{}\n").expect("write .luarc.json");
    let src = root.join("src/init.lua");
    std::fs::write(&src, "local M = {}\nreturn M\n").expect("write source");
    (root, src)
}

/// Point `$NXVIM_LSP_CMD` at the scripted mock (recording to a unique file, and
/// publishing one diagnostic on `didOpen`), redirect the LSP log, and keep the
/// syntax worker from re-spawning this test binary. Returns the record path.
fn configure_mock() -> PathBuf {
    let dir = std::env::temp_dir();
    let record = scratch("rec.jsonl");
    let script = scratch("script.json");
    let _ = std::fs::remove_file(&record);
    let diagnostics = serde_json::json!([{
        "range": { "start": { "line": 0, "character": 6 }, "end": { "line": 0, "character": 7 } },
        "severity": 1,
        "message": "lua_ls says hi",
    }]);
    // Scripted feature replies so the `on_attach` keymaps below exercise go-to and
    // hover end-to-end: definition jumps to `return M` (line 1), hover opens a panel.
    let definition = serde_json::json!({
        "uri": format!("file://{}", scratch("proj").join("src/init.lua").display()),
        "range": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 8 } },
    });
    let hover = serde_json::json!({
        "contents": { "kind": "markdown", "value": "local M: table\n" }
    });
    let body = serde_json::json!({
        "record": record.to_str().unwrap(),
        "position_encoding": "utf-8",
        "diagnostics": diagnostics,
        "definition": definition,
        "hover": hover,
    });
    std::fs::write(&script, serde_json::to_string(&body).unwrap()).expect("write script");

    let bin = env!("CARGO_BIN_EXE_nxvim");
    std::env::set_var(
        "NXVIM_LSP_CMD",
        format!("{bin} --__lsp-mock {}", script.display()),
    );
    std::env::set_var("NXVIM_TS_WORKER", bin);
    std::env::set_var(
        "NXVIM_LSP_LOG_FILE",
        dir.join(format!("nxvim-lspconfig-{}.log", std::process::id())),
    );
    std::env::set_var("NXVIM_LSP_LOG_LEVEL", "debug");
    // A real root must come from the config's own logic, not an override.
    std::env::remove_var("NXVIM_LSP_ROOT");
    record
}

/// A config dir whose `init.lua` is a user-style `lua_ls` setup: it merges an
/// `on_attach` (wiring buffer-local LSP keymaps) over the vendored base config and
/// enables the server. The base (cmd/filetypes/root_markers) still comes entirely
/// from the vendored `lsp/lua_ls.lua`; this only adds the attach hook, exercising
/// the Phase 7b Slice 3 `LspAttach`/`on_attach` path off the real config.
fn enable_config_dir() -> PathBuf {
    let dir = scratch("config");
    std::fs::create_dir_all(&dir).expect("mkdir config");
    std::fs::write(
        dir.join("init.lua"),
        "vim.lsp.config('lua_ls', {\n\
         \x20 on_attach = function(client, bufnr)\n\
         \x20   vim.keymap.set('n', '<Space>d', vim.lsp.buf.definition, { buffer = bufnr })\n\
         \x20   vim.keymap.set('n', '<Space>k', vim.lsp.buf.hover, { buffer = bufnr })\n\
         \x20 end,\n\
         })\n\
         vim.lsp.enable('lua_ls')\n",
    )
    .expect("write init.lua");
    dir
}

/// Start a server editing `file`, with the vendored nvim-lspconfig on the
/// runtimepath and the enable-only `init.lua` sourced; attach a UI.
async fn start(
    file: PathBuf,
    rtp: PathBuf,
    config_dir: PathBuf,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    let file = file.to_string_lossy().into_owned();
    start_attached(
        ServerInit {
            file: Some(file),
            config_dir: Some(config_dir),
            runtimepath: vec![rtp],
            ..Default::default()
        },
        COLS,
        ROWS - 2,
    )
    .await
}

/// The current window cursor as 0-based `(row, col)`. The crate's `cursor_u64`
/// returns the raw 1-based row; decrement it to match the rest of the asserts.
async fn cursor(rpc: &Rpc) -> (u64, u64) {
    let (row, col) = cursor_u64(rpc).await;
    (row.saturating_sub(1), col)
}

/// Poll (bounded) until the cursor reaches `want`, driving redraws meanwhile.
async fn wait_for_cursor(rpc: &Rpc, want: (u64, u64)) {
    for _ in 0..100 {
        barrier(rpc).await;
        if cursor(rpc).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "cursor never reached {want:?}; last was {:?}",
        cursor(rpc).await
    );
}

/// Force a redraw (which drives LSP document sync) so the mock advances.
async fn barrier(rpc: &Rpc) {
    rpc.request(
        "nvim_buf_get_lines",
        vec![
            Value::from(0u64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(false),
        ],
    )
    .await
    .expect("barrier");
}

/// Parse the mock's record file into `{method, params}` objects.
fn record_lines(path: &Path) -> Vec<Json> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Poll (bounded) until the record file satisfies `pred`, returning the records.
async fn wait_for_record(rpc: &Rpc, record: &Path, pred: impl Fn(&[Json]) -> bool) -> Vec<Json> {
    for _ in 0..100 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        let recs = record_lines(record);
        if pred(&recs) {
            return recs;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "record never satisfied predicate; saw: {:?}",
        record_lines(record)
    );
}

fn find<'a>(recs: &'a [Json], method: &str) -> Option<&'a Json> {
    recs.iter().find(|r| r["method"] == method)
}

/// Whether a `redraw` carries at least one non-empty `diagnostics` row. The
/// per-row `diagnostics` now live under the first window (`windows[0]`).
fn has_diagnostics(params: &[Value]) -> bool {
    let Some(Value::Map(map)) = params.first() else {
        return false;
    };
    let Some((_, Value::Array(windows))) = map.iter().find(|(k, _)| k.as_str() == Some("windows"))
    else {
        return false;
    };
    let Some(Value::Map(win)) = windows.first() else {
        return false;
    };
    let Some((_, Value::Array(rows))) = win.iter().find(|(k, _)| k.as_str() == Some("diagnostics"))
    else {
        return false;
    };
    rows.iter()
        .any(|row| row.as_array().is_some_and(|spans| !spans.is_empty()))
}

#[tokio::test]
async fn vendored_lua_ls_config_drives_the_start() {
    let Some(rtp) = lspconfig_dir() else {
        eprintln!("skipping: vendor/nvim-lspconfig not checked out");
        return;
    };

    let record = configure_mock();
    let (proj, src) = make_project();
    let (rpc, mut incoming) = start(src.clone(), rtp, enable_config_dir()).await;

    // The vendored `lsp/lua_ls.lua` loaded, matched the `lua` filetype, resolved
    // its root via the real (nested) `root_markers` up to the `.luarc.json` dir,
    // and started the (mock) server: `initialize` carries that real root.
    let recs = wait_for_record(&rpc, &record, |r| find(r, "initialize").is_some()).await;
    let root_uri = find(&recs, "initialize").unwrap()["params"]["rootUri"]
        .as_str()
        .unwrap_or_default();
    let want = format!("file://{}", proj.display());
    assert_eq!(
        root_uri, want,
        "rootUri is the project root the vendored config's root_markers found"
    );

    // The document opened with the filetype as its languageId.
    let recs = wait_for_record(&rpc, &record, |r| find(r, "textDocument/didOpen").is_some()).await;
    let did_open = find(&recs, "textDocument/didOpen").unwrap();
    assert_eq!(
        did_open["params"]["textDocument"]["languageId"].as_str(),
        Some("lua"),
        "didOpen languageId is the buffer's filetype"
    );

    // And diagnostics the (mock) server published flow through to a redraw —
    // the full path works end to end off the real config.
    let mut saw_diagnostics = false;
    for _ in 0..100 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if has_diagnostics(&params) {
                saw_diagnostics = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_diagnostics,
        "diagnostics from the vendored-config server never reached a redraw"
    );

    // The user-style `on_attach` ran on attach (off the vendored config): its
    // buffer-local `<Space>d` drives go-to-definition, jumping to the scripted
    // target (line 1, `return M`). That the key works at all proves `LspAttach`
    // fired and `on_attach` wired the map.
    feed(&rpc, " d");
    wait_for_cursor(&rpc, (1, 7)).await;
    assert!(
        find(&record_lines(&record), "textDocument/definition").is_some(),
        "the on_attach <Space>d should send a textDocument/definition request"
    );

    // And the `on_attach`-set `<Space>k` drives hover, opening the panel with the
    // markup rendered as plain lines.
    feed(&rpc, " k");
    let mut hovered = false;
    for _ in 0..100 {
        barrier(&rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(&mut incoming) {
            if panel_has(&params, "LSP hover") {
                hovered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        hovered,
        "the on_attach <Space>k should open the LSP hover panel"
    );
    assert!(
        find(&record_lines(&record), "textDocument/hover").is_some(),
        "the on_attach <Space>k should send a textDocument/hover request"
    );
}

/// Whether a `redraw` carries a panel whose title is `title` (the `panel` redraw
/// key projects `{ title, lines, … }`).
fn panel_has(params: &[Value], title: &str) -> bool {
    let Some(Value::Map(map)) = params.first() else {
        return false;
    };
    let Some((_, panel)) = map.iter().find(|(k, _)| k.as_str() == Some("panel")) else {
        return false;
    };
    let Some(fields) = panel.as_map() else {
        return false;
    };
    fields
        .iter()
        .any(|(k, v)| k.as_str() == Some("title") && v.as_str() == Some(title))
}
