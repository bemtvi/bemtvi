//! Load/setup regression coverage for the small `vim.*` surface gaps that used to
//! break the real third-party plugins the user runs out of
//! `~/.config/nxvim/pack/plugins/start`. Each test boots a real server and either
//! exercises the primitive directly or `require(...).setup()`s the plugin,
//! asserting it completes without error. Plugins are not vendored, so the
//! plugin-driving tests SKIP (pass with an eprintln) when their dirs are absent,
//! matching the telescope/lspconfig load tests.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_test_harness::{clone_plugin, exec_lua, temp_dir};
use rmpv::Value;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedReceiver;

/// Clone each named plugin (pinned, via the harness) into the shared cache and
/// return their checkout paths for the runtimepath, or `None` (so the caller skips)
/// when any can't be fetched — hermetic, independent of the developer's local
/// install.
fn plugin_rtp(names: &[&str]) -> Option<Vec<PathBuf>> {
    names.iter().map(|n| clone_plugin(n)).collect()
}

/// Run `code` (which must `return` a status string) through a server with `names`
/// on the runtimepath; assert it returned exactly `"OK"`. Skips if a clone fails.
async fn assert_plugin_ok(names: &[&str], code: &str) {
    let Some(rtp) = plugin_rtp(names) else {
        eprintln!("skip: could not clone one of {names:?} (no git / no network)");
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
/// behavior directly (no plugin needed); the full cmp load is now covered by
/// `nvim_cmp_loads` above (the decoration-provider subsystem closed the last gap).
/// Covers: vim.uv.now (ms monotonic), vim.api.nvim_get_current_line,
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

/// nvim-cmp: `cmp.setup{}` builds the default *custom entries* view, which calls
/// `vim.api.nvim_set_decoration_provider` to highlight each entry's matched chars.
/// That API (and the ephemeral extmarks its callbacks place) was the last gap —
/// without it the view errored at construction, failing cmp's load and every cmp
/// source that `require('cmp')`. With the decoration-provider subsystem in place,
/// `require('cmp')` + `cmp.setup{}` completes. (The provider *firing* — ephemeral
/// highlights reaching the projection — is proven directly in `decoration.rs`.)
#[tokio::test]
async fn nvim_cmp_loads() {
    assert_plugin_ok(
        &["nvim-cmp"],
        r#"
        local ok, err = pcall(function()
          local cmp = require('cmp')
          cmp.setup({})
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
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

/// nvim-dap: defines its breakpoint/stopped signs at module load via
/// sign_getdefined + sign_define, so the definition registry must work for
/// `require('dap')` (and its dependents dap-python / nvim-dap-virtual-text) to
/// load. Sign *placement* stays unimplemented (no sign-column render) and is not
/// exercised here. nvim-dap-python additionally needs vim.fn.trim at setup.
#[tokio::test]
async fn nvim_dap_loads() {
    assert_plugin_ok(
        &["nvim-dap", "nvim-dap-python", "nvim-dap-virtual-text"],
        r#"
        -- The exact define/query round-trip dap relies on at load.
        assert(#vim.fn.sign_getdefined("Nope") == 0, "undefined sign -> empty")
        vim.fn.sign_define("CompatBp", { text = "B", texthl = "SignColumn" })
        local got = vim.fn.sign_getdefined("CompatBp")
        assert(#got == 1 and got[1].text == "B" and got[1].texthl == "SignColumn", "define round-trip")
        assert(vim.fn.trim("  hi \t") == "hi", "trim both ends")
        local ok, err = pcall(function()
          require('dap')
          require('nvim-dap-virtual-text').setup()
          require('dap-python').setup("python")
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}

/// nvim_set_hl must write through to the `vim._hl_defs` mirror *immediately*, so a
/// same-turn `nvim_get_hl` / `hlexists` sees the group — the mirror is otherwise
/// only refreshed between turns (gated on the registry generation). This is the
/// read-after-write guarantee that lets an `init.lua` set a colorscheme and then
/// configure a statusline plugin (lualine) in one chunk. Also covers the legacy
/// `nvim_get_hl_by_name` reader lualine calls. No plugin needed — pure surface.
#[tokio::test]
async fn nvim_set_hl_writes_through_same_turn() {
    let (rpc, _incoming) = start(vec![]).await;
    let report = exec_lua(
        &rpc,
        r#"
        -- read-after-write, same chunk: set then get with no turn boundary.
        vim.api.nvim_set_hl(0, 'X', { fg = '#112233', bg = '#445566', bold = true })
        assert(vim.fn.hlexists('X') == 1, "hlexists sees same-turn group")
        local d = vim.api.nvim_get_hl(0, { name = 'X' })
        assert(d.fg == 0x112233, "fg int: " .. tostring(d.fg))
        assert(d.bg == 0x445566, "bg int: " .. tostring(d.bg))
        assert(d.bold == true, "bold attr set")
        assert(d.italic == nil, "unset attr absent, not false")

        -- named color + NONE parse exactly as the core parser does.
        vim.api.nvim_set_hl(0, 'Named', { fg = 'red', bg = 'NONE' })
        local n = vim.api.nvim_get_hl(0, { name = 'Named' })
        assert(n.fg == 0xff0000, "named red -> 0xff0000")
        assert(n.bg == nil, "NONE -> no color")

        -- a link group round-trips and resolves through link = false.
        vim.api.nvim_set_hl(0, 'L', { link = 'X' })
        assert(vim.api.nvim_get_hl(0, { name = 'L' }).link == 'X', "link stored")
        assert(vim.api.nvim_get_hl(0, { name = 'L', link = false }).fg == 0x112233, "link resolved")

        -- a blank def clears the group (removes the key), matching core.
        vim.api.nvim_set_hl(0, 'X', {})
        assert(vim.fn.hlexists('X') == 0, "blank def clears the group")
        assert(next(vim.api.nvim_get_hl(0, { name = 'X' })) == nil, "cleared group reads {}")

        -- legacy nvim_get_hl_by_name shape (foreground/background ints) lualine reads.
        local legacy = vim.api.nvim_get_hl_by_name('Named', true)
        assert(legacy.foreground == 0xff0000, "legacy .foreground int")
        assert(legacy.background == nil, "legacy .background absent")
        assert(not pcall(vim.api.nvim_get_hl_by_name, 'Named', false), "cterm read fails loud")
        return "OK"
        "#,
    )
    .await;
    assert_eq!(report.as_str(), Some("OK"), "set_hl write-through surface");
}

/// lualine.nvim derives its statusline palette from the `Normal` (and friends)
/// highlight groups, in the *same chunk* a typical `init.lua` loads the
/// colorscheme — so it only works once `nvim_set_hl` writes through to the mirror
/// immediately. This drives lualine's *real* `extract_highlight_colors('Normal')`
/// (`utils/utils.lua` — the `hlexists` + `nvim_get_hl_by_name` reader whose
/// `nil` return was the exact crash the plan diagnosed at `highlight.lua:54`)
/// right after loading tokyonight, and asserts it now yields concrete colors.
///
/// This is the focused proof of the highlight read-after-write specifically.
#[tokio::test]
async fn lualine_extracts_colorscheme_palette_same_turn() {
    assert_plugin_ok(
        &["lualine.nvim", "tokyonight.nvim"],
        r#"
        local ok, err = pcall(function()
          -- Colorscheme and palette read in one chunk: pre-fix, hlexists('Normal')
          -- was 0 (stale mirror) and nvim_get_hl_by_name absent, so this returned
          -- nil and lualine crashed indexing it.
          require('tokyonight').load()
          local utils = require('lualine.utils.utils')
          local normal = utils.extract_highlight_colors('Normal')
          assert(normal ~= nil, "extract_highlight_colors('Normal') is nil")
          assert(normal.fg and normal.fg:match('^#%x%x%x%x%x%x$'),
            "no fg hex: " .. tostring(normal.fg))
          assert(normal.bg and normal.bg:match('^#%x%x%x%x%x%x$'),
            "no bg hex: " .. tostring(normal.bg))
          -- scope form returns a single channel (what lualine's theme builder uses).
          assert(utils.extract_highlight_colors('Normal', 'fg') == normal.fg, "scope read")
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}

/// The `vim.uv.new_fs_event` fix unblocks lualine's git-branch component, which at
/// module load builds the watcher handle that previously crashed (`attempt to call
/// field 'new_fs_event'`). Requiring it now succeeds, and the handle it builds —
/// via the same `vim.loop` table lualine uses — supports the start/stop/close
/// lifecycle the component drives on `.git/HEAD`. (The watcher itself, and the luv
/// loop-timer function-forms lualine's refresh timer uses, are proven firing
/// end-to-end in `async_runtime.rs`.)
///
/// (A *full* `lualine.setup{}` — exercised by `lualine_loads` below — additionally
/// needs `vim.api.nvim_exec` output capture for lualine's autocmd dedupe; that gap
/// is now closed too, outside the two watcher/timer gaps this change targets.)
#[tokio::test]
async fn lualine_branch_component_builds_its_fs_watcher() {
    assert_plugin_ok(
        &["lualine.nvim", "tokyonight.nvim"],
        r#"
        local ok, err = pcall(function()
          -- Module load builds a vim.uv.new_fs_event handle (git_branch.lua:20) —
          -- the exact call that used to crash require of the branch component.
          local branch = require('lualine.components.branch.git_branch')
          assert(branch ~= nil, "branch component failed to load")
          -- The handle type the component builds, exercised through lualine's path.
          local ev = vim.loop.new_fs_event()
          assert(type(ev.start) == 'function' and type(ev.stop) == 'function',
            "fs_event handle missing start/stop")
          ev:close()
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}

/// `vim.api.nvim_exec(src, output)` runs ex-command(s) and, when `output` is true,
/// returns their captured text as a string (else `""`). lualine's `define_autocmd`
/// dedupes via `nvim_exec('au lualine <event> <pat>', true):find(cmd)` — it reads
/// the `:au` listing back to decide whether its autocmd is already registered, so a
/// missing `nvim_exec` (or one that can't capture the listing) breaks `setup{}`.
/// The listing itself is produced in-Lua by the `vim._ex_autocmd` driver, so
/// `nvim_exec` must route the autocmd family there and capture its return — not
/// queue it like `vim.cmd` (whose output would surface async, uncapturable).
#[tokio::test]
async fn nvim_exec_captures_autocmd_listing() {
    let (rpc, _incoming) = start(vec![]).await;
    let report = exec_lua(
        &rpc,
        r#"
        local ok, err = pcall(function()
          -- Register a string-command autocmd in group 'lualine' synchronously
          -- (in-VM registry), exactly the shape lualine's define_autocmd lands.
          local grp = vim.api.nvim_create_augroup('lualine', { clear = true })
          vim.api.nvim_create_autocmd('BufEnter', {
            group = grp, pattern = '*', command = 'echo "branch"' })

          -- capture=true: the :au listing for that group/event, the string lualine
          -- :find's its command body in.
          local out = vim.api.nvim_exec('au lualine BufEnter *', true)
          assert(type(out) == 'string', 'nvim_exec(.., true) must return a string')
          assert(out:find('echo "branch"', 1, true),
            'listing is missing the registered command body:\n' .. out)
          assert(not out:find('NOPE_unregistered', 1, true), 'false-positive find')

          -- capture=false: runs but yields no value to read back ("" not nil).
          assert(vim.api.nvim_exec('au lualine BufEnter *', false) == '',
            'nvim_exec(.., false) must return ""')
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
    assert_eq!(
        report.as_str(),
        Some("OK"),
        "nvim_exec autocmd-listing capture"
    );
}

/// The headline lualine proof: with the highlight read-after-write, the fs_event /
/// luv-timer fixes, *and* `nvim_exec` output capture all in place, a full
/// `require('lualine').setup{}` completes in the same chunk a colorscheme loads —
/// the realistic `init.lua` path. `setup{}` reaches `define_autocmd`, which calls
/// `nvim_exec('au lualine …', true)` to dedupe; before this it errored on the
/// absent `nvim_exec`.
#[tokio::test]
async fn lualine_loads() {
    assert_plugin_ok(
        &["lualine.nvim", "tokyonight.nvim"],
        r#"
        local ok, err = pcall(function()
          require('tokyonight').load()
          require('lualine').setup{}
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
}

/// A package's `plugin/` and `after/plugin/` Lua scripts must be sourced at startup
/// — neovim's `pack/*/start` package behavior. nvim-cmp wires its autocmd engine in
/// `plugin/cmp.lua` and cmp-buffer registers its source in
/// `after/plugin/cmp_buffer.lua`; neither runs without this, so cmp can't work.
/// Verifies: `plugin/` runs (recursively), `after/plugin/` runs *after* it, and the
/// scripts can see state `init.lua` set (init.lua is sourced first).
#[tokio::test]
async fn pack_plugin_scripts_are_sourced_at_startup() {
    let dir = temp_dir("plugin_scripts");
    // init.lua runs before plugins; leave a marker the plugin scripts can observe.
    std::fs::write(dir.join("init.lua"), "vim.g.from_init = 'init'\n").unwrap();
    // A fake plugin on the runtimepath: a top-level plugin script, a nested one
    // (proving the walk recurses), and an after/plugin script (loaded last).
    let plug = temp_dir("fake_plugin");
    std::fs::create_dir_all(plug.join("plugin/nested")).unwrap();
    std::fs::create_dir_all(plug.join("after/plugin")).unwrap();
    std::fs::write(
        plug.join("plugin/a.lua"),
        "vim.g.plugin_ran = (vim.g.plugin_ran or 0) + 1\nvim.g.plugin_saw_init = vim.g.from_init\n",
    )
    .unwrap();
    std::fs::write(
        plug.join("plugin/nested/b.lua"),
        "vim.g.nested_ran = true\n",
    )
    .unwrap();
    std::fs::write(
        plug.join("after/plugin/z.lua"),
        // Runs after plugin/, so it sees the count plugin/a.lua set.
        "vim.g.after_saw_plugin = vim.g.plugin_ran\n",
    )
    .unwrap();

    let init = ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir, plug],
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
    let (rpc, _incoming) = connect(reader, writer);
    rpc.request(
        "nvim_ui_attach",
        vec![Value::from(80u64), Value::from(24u64), Value::Map(vec![])],
    )
    .await
    .expect("ui attach");

    let out = exec_lua(
        &rpc,
        r#"return string.format("%s|%s|%s|%s",
             tostring(vim.g.plugin_ran),
             tostring(vim.g.nested_ran),
             tostring(vim.g.plugin_saw_init),
             tostring(vim.g.after_saw_plugin))"#,
    )
    .await;
    // plugin/a.lua ran once; nested ran; it saw init.lua's marker; after/plugin saw
    // the plugin count (so after/plugin sourced strictly after plugin/).
    assert_eq!(
        out.as_str(),
        Some("1|true|init|1"),
        "plugin/ + after/plugin/ scripts must be sourced at startup (after init.lua)"
    );
}
