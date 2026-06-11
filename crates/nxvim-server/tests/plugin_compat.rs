//! Load/setup regression coverage for the small `vim.*` surface gaps that used to
//! break the real third-party plugins the user runs out of
//! `~/.config/nxvim/pack/plugins/start`. Each test boots a real server and either
//! exercises the primitive directly or `require(...).setup()`s the plugin,
//! asserting it completes without error. Plugins are not vendored, so the
//! plugin-driving tests SKIP (pass with an eprintln) when their dirs are absent,
//! matching the telescope/lspconfig load tests.

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{run as run_server, ServerInit};
use nxvim_test_harness::{clone_plugin, exec_lua, feed, temp_dir};
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
            // `enable_io` (not just time): the async `vim.system` / `uv.spawn`
            // path reaps real child processes through `tokio::process`, which
            // needs the IO driver — without it a spawned `git` never delivers its
            // exit (lazy.nvim's clone would hang forever).
            .enable_io()
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

/// `vim.wait(time, cb, interval)` is a REAL loop pump, not a sleep: while it is
/// parked the server keeps draining timers, so a condition flipped by other async
/// work (here a `vim.defer_fn`) is actually observed and the call returns `true`
/// well before the timeout. (This is what nvim-cmp's `filter:sync` relies on.) The
/// chunk parks, so its return is discarded — it stashes the verdict in a global
/// that a later, non-parking chunk reads back.
#[tokio::test]
async fn vim_wait_pumps_loop_until_condition() {
    let (rpc, _incoming) = start(vec![]).await;
    let _ = exec_lua(
        &rpc,
        r#"
        _G.__wait = "pending"
        local done = false
        vim.defer_fn(function() done = true end, 40)
        local t0 = vim.uv.now()
        local ok, reason = vim.wait(2000, function() return done end, 5)
        _G.__wait = ("ok=%s reason=%s done=%s under=%s"):format(
          tostring(ok), tostring(reason), tostring(done), tostring(vim.uv.now() - t0 < 2000))
        "#,
    )
    .await;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let r = exec_lua(&rpc, "return _G.__wait").await;
        let s = r.as_str().unwrap_or("<nil>").to_string();
        if s != "pending" {
            assert_eq!(
                s, "ok=true reason=nil done=true under=true",
                "vim.wait verdict"
            );
            return;
        }
    }
    panic!("vim.wait never resolved");
}

/// A `vim.uv` timer handle answers `:is_active()` / `:is_closing()` faithfully
/// (they used to be loud `vim._notimpl` gaps): active once started, inactive after
/// `:stop()`, and a one-shot goes inactive after it fires. cmp-buffer's indexing
/// debounce gates on `:is_active()`, so a wrong answer silently zeroed completion.
#[tokio::test]
async fn uv_timer_is_active_tracks_lifecycle() {
    let (rpc, _incoming) = start(vec![]).await;
    let report = exec_lua(
        &rpc,
        r#"
        local function eq(a, b, m) if a ~= b then error(m..": "..tostring(a).." ~= "..tostring(b)) end end
        local ok, err = pcall(function()
          local t = vim.uv.new_timer()
          eq(t:is_active(), false, "fresh timer inactive")
          eq(t:is_closing(), false, "fresh timer not closing")
          t:start(10000, 0, function() end)  -- one-shot, far in the future
          eq(t:is_active(), true, "started timer active")
          t:stop()
          eq(t:is_active(), false, "stopped timer inactive")
          t:close()
          eq(t:is_closing(), true, "closed timer is closing")

          -- A one-shot goes inactive once it fires: stash and check after a tick.
          _G.__after = nil
          local s = vim.uv.new_timer()
          s:start(5, 0, function() _G.__after = s:is_active() end)
          eq(s:is_active(), true, "armed one-shot active")
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
    assert_eq!(report.as_str(), Some("OK"));
    // After the one-shot fires, its is_active() (captured in the callback) is false.
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let r = exec_lua(&rpc, "return tostring(_G.__after)").await;
        if r.as_str() == Some("false") {
            return;
        }
        if r.as_str() == Some("true") {
            panic!("a fired one-shot still reports is_active()");
        }
    }
    panic!("one-shot timer never fired");
}

/// A libuv `check` handle exposes both the function forms (`uv.check_start/stop`)
/// and the *method* forms (`handle:start/stop`). nvim-cmp's async Scheduler uses
/// `_executor:start(step)` to drive the coroutine queue running its whole
/// completion pipeline; the missing method form silently stalled every cmp menu.
#[tokio::test]
async fn uv_check_handle_has_start_stop_methods() {
    let (rpc, _incoming) = start(vec![]).await;
    let _ = exec_lua(
        &rpc,
        r#"
        _G.__chk = 0
        local c = vim.loop.new_check()
        assert(type(c.start) == "function", "Check:start method missing")
        assert(type(c.stop) == "function", "Check:stop method missing")
        assert(c:is_active() == false, "fresh check inactive")
        c:start(function()
          _G.__chk = _G.__chk + 1
          if _G.__chk >= 3 then c:stop() end
        end)
        "#,
    )
    .await;
    for _ in 0..40 {
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let r = exec_lua(&rpc, "return tostring(_G.__chk)").await;
        if r.as_str() == Some("3") {
            // It ran exactly to the self-stop and halted (didn't run away).
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let again = exec_lua(&rpc, "return _G.__chk").await;
            assert_eq!(again.as_u64(), Some(3), "check should stop itself at 3");
            return;
        }
    }
    panic!("check handle :start never drove its callback");
}

/// The legacy highlight/syntax `vim.fn` family nvim-cmp reaches for while building
/// its menu highlights: `hlID` mints a stable group id, `synIDattr` reads a group's
/// attributes, `synIDtrans` follows `:hi link`s, and `synstack`/`synID` honestly
/// report "no vim-regex syntax" (nxvim highlights via tree-sitter). All were loud
/// gaps before.
#[tokio::test]
async fn vim_fn_highlight_and_syntax_helpers() {
    let (rpc, _incoming) = start(vec![]).await;
    let report = exec_lua(
        &rpc,
        r##"
        local function eq(a, b, m) if a ~= b then error(m..": "..tostring(a).." ~= "..tostring(b)) end end
        local ok, err = pcall(function()
          vim.api.nvim_set_hl(0, "CmpProbe", { fg = "#112233", bold = true })
          vim.api.nvim_set_hl(0, "CmpProbeLink", { link = "CmpProbe" })
          local id = vim.fn.hlID("CmpProbe")
          assert(id > 0, "hlID nonzero")
          eq(vim.fn.hlID("CmpProbe"), id, "hlID stable")
          eq(vim.fn.synIDattr(id, "name"), "CmpProbe", "synIDattr name")
          eq(vim.fn.synIDattr(id, "fg"), "#112233", "synIDattr fg")
          eq(vim.fn.synIDattr(id, "bold"), "1", "synIDattr bold set")
          eq(vim.fn.synIDattr(id, "italic"), "", "synIDattr italic unset")
          -- synIDtrans follows the link to the concrete group.
          local linkid = vim.fn.hlID("CmpProbeLink")
          eq(vim.fn.synIDattr(vim.fn.synIDtrans(linkid), "fg"), "#112233", "synIDtrans resolves link")
          -- No vim-regex syntax engine: synstack empty, synID 0 (honest, not faked).
          eq(#vim.fn.synstack(1, 1), 0, "synstack empty")
          eq(vim.fn.synID(1, 1, 1), 0, "synID zero")
          -- An unminted id reads empty for every attribute.
          eq(vim.fn.synIDattr(999999, "name"), "", "unknown id empty")
        end)
        if ok then return "OK" else return tostring(err) end
        "##,
    )
    .await;
    assert_eq!(report.as_str(), Some("OK"));
}

/// nvim-cmp + cmp-buffer, end-to-end and LIVE: type a prefix in a buffer that
/// contains matching words, trigger completion, and assert cmp's menu actually
/// opens with the matched entries. This exercises the whole chain that the
/// `vim.*` fixes unblocked — the buffer source's debounced indexing
/// (`uv.timer:is_active`), cmp's async Scheduler (`uv.check:start`) driving the
/// filter→view pipeline, `vim.wait` in `filter:sync`, the menu's highlight
/// derivation (`hlID`/`synIDattr`), `vim.opt.eventignore`, `vim.list_slice`, and
/// `screenpos` positioning — none of which "loading" alone proves.
#[tokio::test]
async fn nvim_cmp_completes_buffer_source_live() {
    let Some(rtp) = plugin_rtp(&["nvim-cmp", "cmp-buffer"]) else {
        eprintln!("skip: could not clone nvim-cmp / cmp-buffer");
        return;
    };
    let (rpc, _incoming) = start(rtp).await;
    let setup = exec_lua(
        &rpc,
        r#"
        local ok, err = pcall(function()
          local cmp = require('cmp')
          -- cmp-buffer's after/plugin already registered the 'buffer' source.
          cmp.setup({ sources = { { name = 'buffer' } } })
        end)
        if ok then return "OK" else return tostring(err) end
        "#,
    )
    .await;
    assert_eq!(setup.as_str(), Some("OK"), "cmp.setup failed");
    // Buffer has the words; type a prefix on a new line, staying in INSERT mode.
    feed(&rpc, "ihello helicopter world<CR>hel");
    let _ = exec_lua(&rpc, "require('cmp').complete()").await;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        // PURE reads only (no cmp.sync, which would park on vim.wait): read the
        // entries straight off the menu view and its visibility.
        let probe = exec_lua(
            &rpc,
            r#"
            local cmp = require('cmp')
            local ev = cmp.core.view:_get_entries_view()
            local words = {}
            for _, e in ipairs(ev.entries or {}) do words[#words+1] = e:get_word() end
            return ("vis=%s words=%s"):format(
              tostring(cmp.core.view:visible()), table.concat(words, ","))
            "#,
        )
        .await;
        let s = probe.as_str().unwrap_or("");
        if s.contains("vis=true") && s.contains("helicopter") {
            // The menu is open and offers the matching word from the buffer.
            assert!(s.contains("hello"), "menu should also match 'hello': {s}");
            return;
        }
    }
    panic!("cmp completion menu never opened with the buffer's matching words");
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
            // `enable_io` (not just time): the async `vim.system` / `uv.spawn`
            // path reaps real child processes through `tokio::process`, which
            // needs the IO driver — without it a spawned `git` never delivers its
            // exit (lazy.nvim's clone would hang forever).
            .enable_io()
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

/// `vim.opt.<x>` must be neovim's rich Option object — list/flag/map aware, with
/// `:get`/`:append`/`:prepend`/`:remove` and the `+` operator — not a thin scalar
/// proxy. lazy.nvim drives its runtimepath entirely through this (`vim.opt.rtp =
/// {..}` then `vim.opt.rtp:append(dir)`), and crucially an appended rtp entry must
/// become `require`-able (package.path sync), the way neovim makes rtp drive Lua
/// module search.
#[tokio::test]
async fn vim_opt_is_a_rich_option_object() {
    let plug = temp_dir("vimopt_plugin");
    std::fs::create_dir_all(plug.join("lua")).unwrap();
    std::fs::write(plug.join("lua/optmod.lua"), "return { ok = true }\n").unwrap();

    let (rpc, _incoming) = start(vec![]).await;
    let report = exec_lua(
        &rpc,
        &format!(
            r#"
            local function eq(a,b,m) if a~=b then error(m..": "..tostring(a).." ~= "..tostring(b)) end end
            -- list option: assign / append / prepend / remove / get
            vim.opt.rtp = {{ "/a", "/b" }}
            eq(vim.o.rtp, "/a,/b", "list assign")
            vim.opt.rtp:append("/c"); eq(vim.o.rtp, "/a,/b,/c", "list append")
            vim.opt.rtp:prepend("/z"); eq(vim.o.rtp, "/z,/a,/b,/c", "list prepend")
            vim.opt.rtp:remove("/b"); eq(vim.o.rtp, "/z,/a,/c", "list remove")
            local got = vim.opt.rtp:get()
            eq(type(got), "table", "get is table"); eq(got[1], "/z", "get[1]")
            -- operator form
            vim.opt.rtp = vim.opt.rtp + "/op"
            eq(vim.o.rtp, "/z,/a,/c,/op", "plus operator")
            -- flag option (shortmess): char set
            vim.o.shortmess = "fi"
            vim.opt.shortmess:append("c"); eq(vim.opt.shortmess:get().c, true, "flag append c")
            vim.opt.shortmess:remove("f"); eq(vim.opt.shortmess:get().f, nil, "flag remove f")
            -- map option (listchars): key:val
            vim.opt.listchars = {{ eol = "x", tab = ">>" }}
            local lc = vim.opt.listchars:get()
            eq(lc.eol, "x", "map eol"); eq(lc.tab, ">>", "map tab")
            -- appended rtp dir becomes require-able (package.path sync)
            vim.opt.rtp:append("{plug}")
            eq(require("optmod").ok, true, "appended rtp dir is require-able")
            return "OK"
            "#,
            plug = plug.to_string_lossy()
        ),
    )
    .await;
    assert_eq!(
        report.as_str(),
        Some("OK"),
        "vim.opt rich Option object: {report:?}"
    );
}

/// The `vim.*` primitives a plugin manager needs at load before any plugin is
/// touched, asserted directly so they stay covered even when lazy.nvim can't be
/// cloned: `loadplugins` defaults on (lazy bails out of setup when it's falsy),
/// `vim.health.*` exists and records (lazy binds these into locals at load), and
/// `vim.env.VIMRUNTIME` is a non-nil string (lazy concatenates it unconditionally).
#[tokio::test]
async fn plugin_manager_load_primitives() {
    let (rpc, _incoming) = start(vec![]).await;
    let out = exec_lua(
        &rpc,
        r#"
        local function eq(a,b,m) if a~=b then error(m..": "..tostring(a).." ~= "..tostring(b)) end end
        eq(vim.go.loadplugins, true, "loadplugins defaults on")
        eq(type(vim.health), "table", "vim.health exists")
        eq(type(vim.health.start), "function", "health.start")
        eq(vim.health.report_ok, vim.health.ok, "report_ok aliases ok")
        vim.health.start("grp"); vim.health.ok("good")
        eq(#vim._health_report, 2, "health calls recorded")
        eq(type(vim.env.VIMRUNTIME), "string", "VIMRUNTIME is a string")
        vim.env.FOO_SHADOW = "bar"; eq(vim.env.FOO_SHADOW, "bar", "env write shadows")
        return "OK"
        "#,
    )
    .await;
    assert_eq!(out.as_str(), Some("OK"), "load primitives: {out:?}");
}

/// lazy.nvim loads, completes `setup()`, and manages + loads a local (`dir`)
/// plugin end-to-end on nxvim — config function runs, plugin marked loaded. All of
/// lazy's writes are redirected under a temp dir via its own root/state/lockfile
/// opts, so the test is hermetic without touching the real stdpath. Skips when the
/// clone can't be fetched (matching the other plugin tests).
#[tokio::test]
async fn lazy_nvim_loads_and_manages_a_plugin() {
    let Some(lazy) = clone_plugin("lazy.nvim") else {
        eprintln!("skip: could not clone lazy.nvim (no git / no network)");
        return;
    };
    // A local plugin for lazy to manage; unique module name to avoid require-cache
    // collisions with the other plugins loaded in this shared test binary.
    let plug = temp_dir("lazy_local_plugin");
    std::fs::create_dir_all(plug.join("lua/lazyhello")).unwrap();
    std::fs::write(
        plug.join("lua/lazyhello/init.lua"),
        "return { setup = function() _G.LAZYHELLO = true end }\n",
    )
    .unwrap();
    let state = temp_dir("lazy_state");

    let (rpc, _incoming) = start(vec![lazy]).await;
    let report = exec_lua(
        &rpc,
        &format!(
            r#"
            local plug, state = "{plug}", "{state}"
            local ok, err = pcall(function()
              require("lazy").setup({{
                {{ dir = plug, name = "lazyhello", lazy = false,
                   config = function() require("lazyhello").setup() end }},
              }}, {{
                root = state .. "/plugins",
                lockfile = state .. "/lazy-lock.json",
                state = state .. "/state.json",
                readme = {{ root = state .. "/readme" }},
                install = {{ missing = false }},
                change_detection = {{ enabled = false }},
                checker = {{ enabled = false }},
                performance = {{ rtp = {{ reset = false }} }},
              }})
            end)
            if not ok then return "ERR: " .. tostring(err) end
            local p = require("lazy.core.config").plugins["lazyhello"]
            return table.concat({{
              "setup_ran=" .. tostring(_G.LAZYHELLO == true),
              "managed=" .. tostring(p ~= nil),
              "loaded=" .. tostring(p ~= nil and p._ ~= nil and p._.loaded ~= nil),
            }}, "|")
            "#,
            plug = plug.to_string_lossy(),
            state = state.to_string_lossy()
        ),
    )
    .await;
    assert_eq!(
        report.as_str(),
        Some("setup_ran=true|managed=true|loaded=true"),
        "lazy.nvim should load and manage a local plugin: {report:?}"
    );
}

/// Run `git` under `cwd` with a fixed, config-independent identity so the source
/// repo is reproducible regardless of the developer's global git config. Panics on
/// failure — the source repo is test scaffolding, not the thing under test.
fn git_in(cwd: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "-c",
            "user.email=test@nxvim",
            "-c",
            "user.name=nxvim test",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed in {cwd:?}");
}

/// lazy.nvim's **install pipeline** — its real `git clone` over `uv.spawn` — runs
/// end-to-end on nxvim: `require("lazy").install({ wait = true })` synchronously
/// drives the async runner (the `vim.loop.new_check` executor + a blocking
/// `vim.wait` pump) through clone → checkout → load, and the freshly-cloned
/// plugin's `config` then runs.
///
/// Unlike `lazy_nvim_loads_and_manages_a_plugin` (which manages an in-place `dir`
/// plugin, never cloning), this exercises the git/uv machinery a plugin manager
/// leans on hardest. To stay hermetic and deterministic it clones from a *local*
/// source git repo built in the test (`file://…`), not the network — the clone
/// path through nxvim is identical, but the bytes are fixed and offline.
#[tokio::test]
async fn lazy_nvim_clones_and_loads_a_plugin() {
    let Some(lazy) = clone_plugin("lazy.nvim") else {
        eprintln!("skip: could not clone lazy.nvim (no git / no network)");
        return;
    };

    // A real git repo to clone *from*: a tiny plugin module on a `main` branch.
    let source = temp_dir("lazy_clone_source");
    std::fs::create_dir_all(source.join("lua/clonehello")).unwrap();
    std::fs::write(
        source.join("lua/clonehello/init.lua"),
        "return { setup = function() _G.CLONEHELLO = true end }\n",
    )
    .unwrap();
    git_in(&source, &["init", "--quiet"]);
    git_in(&source, &["add", "-A"]);
    git_in(&source, &["commit", "--quiet", "-m", "initial"]);
    git_in(&source, &["branch", "-M", "main"]);

    let state = temp_dir("lazy_clone_state");

    let (rpc, _incoming) = start(vec![lazy]).await;

    // Phase 1 — setup, then *kick off* the install without blocking. lazy's
    // git clone runs on `uv.spawn` off the input tick and its async runner is
    // driven by the `vim.loop.new_check` executor between ticks. A blocking
    // `install({ wait = true })` would park the chunk (its `vim.wait` pump yields
    // the coroutine) and hand `nvim_exec_lua` back `Nil` before the clone lands,
    // so instead we register a done-callback that flips a global and poll for it
    // from the Rust side — the same async shape `uv_process.rs` uses.
    let kickoff = exec_lua(
        &rpc,
        &format!(
            r#"
            _G.LAZY_DONE = false
            _G.LAZY_ERR = nil
            local ok, err = pcall(function()
              local source, state = "{source}", "{state}"
              require("lazy").setup({{
                {{ url = "file://" .. source, name = "clonehello", branch = "main",
                   lazy = false, config = function() require("clonehello").setup() end }},
              }}, {{
                root = state .. "/plugins",
                lockfile = state .. "/lazy-lock.json",
                state = state .. "/state.json",
                readme = {{ root = state .. "/readme" }},
                -- Don't auto-install on startup; we drive the clone explicitly below.
                install = {{ missing = false }},
                -- Plain local clone: no partial-clone filter (file:// transport
                -- doesn't advertise it).
                git = {{ filter = false }},
                pkg = {{ enabled = false }},
                change_detection = {{ enabled = false }},
                checker = {{ enabled = false }},
                performance = {{ rtp = {{ reset = false }} }},
              }})
              -- The real clone: `git clone file://… <root>/clonehello` over uv.
              -- Non-blocking — the async runner advances across ticks; the callback
              -- fires once the whole pipeline (clone → checkout → load) settles.
              local runner = require("lazy").install({{ show = false }})
              runner:wait(function() _G.LAZY_DONE = true end)
            end)
            if not ok then _G.LAZY_ERR = tostring(err); _G.LAZY_DONE = true end
            return "kicked"
            "#,
            source = source.to_string_lossy(),
            state = state.to_string_lossy()
        ),
    )
    .await;
    assert_eq!(
        kickoff.as_str(),
        Some("kicked"),
        "install kickoff: {kickoff:?}"
    );

    // Phase 2 — let the server advance the clone between polls (the runner's
    // `vim.loop.new_check` executor steps the git task off the input tick, and the
    // child's exit is reaped + delivered on a later tick).
    let mut done = false;
    for _ in 0..300 {
        if exec_lua(&rpc, "return _G.LAZY_DONE == true")
            .await
            .as_bool()
            == Some(true)
        {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(done, "lazy.nvim install never completed (clone hung)");

    // Phase 3 — finalize: surface any pipeline error, source the freshly-cloned
    // `lazy = false` plugin (its config runs), and report what landed.
    let report = exec_lua(
        &rpc,
        &format!(
            r#"
            if _G.LAZY_ERR then return "ERR: " .. _G.LAZY_ERR end
            require("lazy").load({{ plugins = {{ "clonehello" }} }})
            local p = require("lazy.core.config").plugins["clonehello"]
            local cloned_dir = "{state}" .. "/plugins/clonehello"
            return table.concat({{
              "cloned=" .. tostring(vim.fn.isdirectory(cloned_dir .. "/.git") == 1),
              "installed=" .. tostring(p ~= nil and p._ ~= nil and p._.installed == true),
              "requireable=" .. tostring((pcall(require, "clonehello"))),
              "config_ran=" .. tostring(_G.CLONEHELLO == true),
            }}, "|")
            "#,
            state = state.to_string_lossy()
        ),
    )
    .await;
    assert_eq!(
        report.as_str(),
        Some("cloned=true|installed=true|requireable=true|config_ran=true"),
        "lazy.nvim should git-clone and load a real plugin: {report:?}"
    );
}
