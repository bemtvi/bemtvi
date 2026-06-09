//! End-to-end against the **real** `lua-language-server` (skips when it isn't
//! installed, like the `lspconfig_configs` / `telescope_load` tests). This is the
//! gold-standard proof that the inlay-hint surface actually lights up a real,
//! pull-config server — the exact `examples/inlay-hints/` scenario — rather than
//! only the scripted mock. It exercises the two pieces a real server needs that the
//! mock can only approximate: the `workspace/configuration` pull (lua_ls reads its
//! `hint.enable` this way) and the `workspace/inlayHint/refresh` re-query (lua_ls
//! computes hints asynchronously and only signals readiness via refresh).

use crate::support::*;

/// Whether `lua-language-server` is on PATH and runnable.
fn lua_ls_available() -> bool {
    std::process::Command::new("lua-language-server")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Poll (bounded, generous — a cold lua_ls loads its meta library before it has
/// any hints) until some window row carries an inlay hint, returning its
/// `(row, col, text)`. Panics with a timeout otherwise.
async fn wait_for_any_inlay(
    rpc: &Rpc,
    incoming: &mut UnboundedReceiver<Incoming>,
) -> (usize, u64, String) {
    for _ in 0..400 {
        barrier(rpc).await;
        tokio::task::yield_now().await;
        if let Some(params) = drain_latest_redraw(incoming) {
            if let Some(rows) = window0_get(&params, "inlay_hints").and_then(Value::as_array) {
                for (row, hints) in rows.iter().enumerate() {
                    if let Some(first) = hints.as_array().and_then(|h| h.first()) {
                        if let Some(a) = first.as_array() {
                            let col = a.first().and_then(Value::as_u64).unwrap_or(0);
                            let text = a.get(1).and_then(Value::as_str).unwrap_or("").to_string();
                            if !text.trim().is_empty() {
                                return (row, col, text);
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    panic!("no inlay hint from lua-language-server within timeout");
}

#[tokio::test]
async fn real_lua_ls_inlay_hints_appear() {
    let _guard = test_lock().lock().await;
    if !lua_ls_available() {
        eprintln!("skip: lua-language-server not installed");
        return;
    }
    // Drive the REAL server: clear the mock override so the config's `cmd` is used,
    // and point the syntax worker at the real nxvim binary (the buffer is `.lua`).
    std::env::remove_var("NXVIM_LSP_CMD");
    std::env::set_var("NXVIM_TS_WORKER", env!("CARGO_BIN_EXE_nxvim"));

    // A buffer with a function whose call sites pick up `name:` parameter hints and
    // whose locals pick up `: type` hints — the canonical lua_ls inlay-hint output.
    let dir = std::env::temp_dir().join(format!("nxvim-lua_ls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create workspace");
    std::fs::write(dir.join(".git"), "").ok(); // a root marker so lua_ls roots here
    let file = dir.join("probe.lua");
    std::fs::write(
        &file,
        "local function area(width, height)\n  return width * height\n end\nprint(area(2, 3))\n",
    )
    .expect("write probe.lua");

    let cfg = std::env::temp_dir().join(format!("nxvim-lua_ls-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&cfg).expect("create config dir");
    std::fs::write(
        cfg.join("init.lua"),
        "vim.lsp.config('lua_ls', { \
           cmd = { 'lua-language-server' }, \
           filetypes = { 'lua' }, \
           root_markers = { '.git' }, \
           settings = { Lua = { hint = { enable = true, setType = true, paramName = 'All', arrayIndex = 'Enable' } } }, \
           on_attach = function(client, bufnr) \
             if client.server_capabilities.inlayHintProvider then \
               vim.lsp.inlay_hint.enable(true, { bufnr = bufnr }) \
             end \
           end, \
         })\n\
         vim.lsp.enable('lua_ls')\n",
    )
    .expect("write init.lua");

    let (rpc, mut incoming) =
        start_with_config_dir(Some(file.display().to_string()), cfg.clone()).await;

    let (row, _col, text) = wait_for_any_inlay(&rpc, &mut incoming).await;
    // lua_ls emits `name:` parameter hints (e.g. `width:`/`height:` on the call) and
    // `: <type>` hints; either proves the round-trip lit up. The call is on line 4
    // (row index varies with the gutter), so just assert a real label arrived.
    assert!(
        text.contains(':'),
        "expected a lua_ls inlay hint label (got {text:?} on row {row})"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&cfg);
}
