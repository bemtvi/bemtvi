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

/// The `pack/plugins/start` directory of the user's nxvim config, or `None`.
fn pack_start() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join(".config/nxvim/pack/plugins/start");
    p.is_dir().then_some(p)
}

/// Resolve the given plugin dir names under `pack/plugins/start`, returning `None`
/// (so the caller skips) if any is missing — each must have a `lua/` subtree.
fn plugin_rtp(names: &[&str]) -> Option<Vec<PathBuf>> {
    let start = pack_start()?;
    let mut dirs = vec![];
    for n in names {
        let d = start.join(n);
        if !d.join("lua").is_dir() {
            return None;
        }
        dirs.push(d);
    }
    Some(dirs)
}

/// Run `code` (which must `return` a status string) through a server with `names`
/// on the runtimepath; assert it returned exactly `"OK"`. Skips if a dir is absent.
async fn assert_plugin_ok(names: &[&str], code: &str) {
    let Some(rtp) = plugin_rtp(names) else {
        eprintln!("skip: missing one of {names:?} under pack/plugins/start");
        return;
    };
    let (rpc, _incoming) = start(rtp).await;
    let report = exec_lua(&rpc, code).await;
    let report = report.as_str().unwrap_or("<non-string>").to_string();
    assert_eq!(report, "OK", "plugin load/setup failed:\n{report}");
}

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

/// LuaSnip: `luasnip/util/ext_opts.lua` calls vim.fn.hlexists to drop undefined
/// ext-mark highlight groups; without it `require('luasnip').setup{}` errored.
#[tokio::test]
async fn luasnip_loads() {
    assert_plugin_ok(
        &["LuaSnip"],
        r#"
        local ok, err = pcall(function() require('luasnip').setup({}) end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}

/// nvim-treesitter: `utils.lua` calls `vim.split(path, '.', true)` (the legacy
/// positional `plain` flag). Without backward-compat for a boolean 3rd arg,
/// vim.split indexed a boolean and `require('nvim-treesitter').setup()` errored.
#[tokio::test]
async fn nvim_treesitter_loads() {
    assert_plugin_ok(
        &["nvim-treesitter"],
        r#"
        -- The exact legacy call shape, plus the real setup it broke.
        assert(#vim.split("a.b.c", ".", true) == 3, "legacy plain split")
        local ok, err = pcall(function() require('nvim-treesitter').setup() end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}

/// trouble.nvim: `view/main.lua` tags its own windows with `vim.w[win].trouble`
/// and reads it back to skip them when choosing a target window. nxvim had no
/// `vim.w`, so that was an index-of-nil and `require('trouble').setup{}` errored.
#[tokio::test]
async fn trouble_loads() {
    assert_plugin_ok(
        &["trouble.nvim"],
        r#"
        -- vim.w round-trips a window-scoped var.
        vim.w.compat_marker = "x"
        assert(vim.w.compat_marker == "x", "vim.w round-trip")
        local ok, err = pcall(function() require('trouble').setup({}) end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}
