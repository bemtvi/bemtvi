//! Load/setup regression coverage for the small `vim.*` surface gaps that used to
//! break the real third-party plugins the user runs out of
//! `~/.config/nxvim/pack/plugins/start`. Each test boots a real server and either
//! exercises the primitive directly or `require(...).setup()`s the plugin,
//! asserting it completes without error. Plugins are not vendored, so the
//! plugin-driving tests SKIP (pass with an eprintln) when their dirs are absent,
//! matching the telescope/lspconfig load tests.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_test_harness::{exec_lua, temp_dir};
use rmpv::Value;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(rtp: Vec<PathBuf>) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir("plugin_compat");
    std::fs::write(dir.join("init.lua"), "").ok();
    let mut runtimepath = vec![dir.clone()];
    runtimepath.extend(rtp);
    let init = ServerInit {
        config_dir: Some(dir),
        runtimepath,
        ..Default::default()
    };
    let (server_end, client_end) = tokio::io::duplex(1 << 18);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("server runtime");
        let _ = runtime.block_on(run_server(server_end, init));
    });
    let (reader, writer) = tokio::io::split(client_end);
    let (rpc, incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(120u64), Value::from(40u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");
    (rpc, incoming)
}

/// The small `vim.*` surface nvim-cmp reaches for while building a completion
/// context and its float windows — each was missing and broke cmp at load (and
/// thus every cmp source). They are general primitives, so this asserts their
/// behavior directly (no plugin needed) rather than the full cmp load, which is
/// still blocked on `nvim_set_decoration_provider` (a redraw subsystem, tracked
/// separately). Covers: vim.uv.now (ms monotonic), vim.api.nvim_get_current_line,
/// vim.str_utfindex / vim.str_byteindex (both signatures), and vim.fn.exists.
#[tokio::test]
async fn cmp_vim_surface_primitives() {
    let (rpc, _incoming) = start(vec![]).await;
    let report = exec_lua(
        &rpc,
        r#"
        local function eq(a, b, msg) if a ~= b then error(msg..": "..tostring(a).." ~= "..tostring(b)) end end
        local ok, err = pcall(function()
          -- vim.uv.now / vim.loop.now: a number of milliseconds (monotonic).
          assert(type(vim.uv.now()) == "number", "uv.now not a number")
          assert(vim.loop.now == vim.uv.now, "loop.now should alias uv.now")

          -- vim.api.nvim_get_current_line: the cursor line's text.
          assert(type(vim.api.nvim_get_current_line()) == "string", "get_current_line")

          -- vim.str_utfindex: legacy (s[, byteidx]) -> utf32, utf16; an astral
          -- codepoint (😀 = 4 UTF-8 bytes) is 1 utf-32 unit but 2 utf-16 units.
          local s = "a😀b"
          local u32, u16 = vim.str_utfindex(s)
          eq(u32, 3, "utf32 count")
          eq(u16, 4, "utf16 count")
          -- 0.11+ form (s, encoding, byteidx).
          eq(vim.str_utfindex(s, "utf-16", #s), 4, "utf16 explicit")
          eq(vim.str_utfindex(s, "utf-32", #s), 3, "utf32 explicit")
          -- vim.str_byteindex round-trips the unit count back to a byte offset.
          eq(vim.str_byteindex(s, "utf-16", 4), #s, "byteindex utf16")

          -- vim.fn.exists: 1 for a modelled option, 0 for an unknown one.
          eq(vim.fn.exists("+number"), 1, "exists known option")
          eq(vim.fn.exists("+totally_not_an_option"), 0, "exists unknown option")
          vim.g.compat_probe = 7
          eq(vim.fn.exists("g:compat_probe"), 1, "exists g: var set")
          eq(vim.fn.exists("g:compat_absent"), 0, "exists g: var unset")
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
    assert_eq!(report.as_str().unwrap_or("<non-string>"), "OK");
}
