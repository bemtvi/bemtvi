use crate::support::*;

// ----- Lua plugin runtime (init.lua + require over the runtimepath) ----------

#[tokio::test]
async fn init_lua_runs_at_startup_and_require_resolves_runtimepath_modules() {
    // A throwaway config dir doubling as a runtimepath entry. `init.lua` pulls a
    // module off the runtimepath via `require` and prints the value it returns;
    // observing it on the message line proves both the module search
    // (`package.path` seeded from the runtimepath) and startup sourcing.
    let dir = temp_dir("rtp");
    std::fs::create_dir_all(dir.join("lua")).expect("create lua dir");
    std::fs::write(
        dir.join("lua").join("probe.lua"),
        "return { greeting = 'loaded from probe' }\n",
    )
    .expect("write probe module");
    std::fs::write(
        dir.join("init.lua"),
        "local probe = require('probe')\nprint(probe.greeting)\n",
    )
    .expect("write init.lua");

    let (rpc, mut incoming) = start_with(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir.clone()],
        ..Default::default()
    })
    .await;

    // Empty input is a no-op edit that still triggers a redraw, carrying the
    // message `init.lua` left behind at startup.
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("loaded from probe"),
        "init.lua should run and require() should resolve modules on the runtimepath"
    );
}

#[tokio::test]
async fn missing_init_lua_is_harmless() {
    // A config dir with no init.lua must start cleanly (no config is normal).
    let dir = temp_dir("noinit");
    let (rpc, mut incoming) = start_with(ServerInit {
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    })
    .await;

    feed(&rpc, "ihello<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello"]);
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some(""),
        "no init.lua → no startup message or error"
    );
}

// ----- vim.* surface (Phase 2): helpers, options, user commands -------------

#[tokio::test]
async fn vim_tbl_deep_extend_merges_nested_tables() {
    let dir = temp_dir("tbl");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local r = vim.tbl_deep_extend('force', {a=1, b={c=2}}, {b={d=3}})\n\
         print(r.a .. ',' .. r.b.c .. ',' .. r.b.d)\n",
    )
    .await;
    assert_eq!(startup_message(&rpc, &mut incoming).await, "1,2,3");
}

#[tokio::test]
async fn vim_g_round_trips_a_global() {
    let dir = temp_dir("vimg");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.g.colors_name = 'mocha'\nprint(vim.g.colors_name)\n",
    )
    .await;
    assert_eq!(startup_message(&rpc, &mut incoming).await, "mocha");
}

#[tokio::test]
async fn vim_cmd_is_callable_and_indexable() {
    // The indexable form `vim.cmd.set("number")` must build and run `:set
    // number`, observable as the redraw's `number` flag flipping on.
    let dir = temp_dir("vimcmd");
    let (rpc, mut incoming) = start_with_config(&dir, "vim.cmd.set('number')\n").await;
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert!(
        field(&map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "vim.cmd.set('number') should enable the number option"
    );
}

#[tokio::test]
async fn vim_fn_stdpath_returns_an_nxvim_path() {
    let dir = temp_dir("stdpath");
    let (rpc, mut incoming) = start_with_config(&dir, "print(vim.fn.stdpath('cache'))\n").await;
    let msg = startup_message(&rpc, &mut incoming).await;
    assert!(
        !msg.is_empty() && msg.contains("nxvim"),
        "stdpath('cache') should be a non-empty nxvim path, got {msg:?}"
    );
}

#[tokio::test]
async fn user_command_registers_and_dispatches() {
    // Register `:Greet` from init.lua, then invoke it with an argument; its
    // callback's print() should land on the message line.
    let dir = temp_dir("usercmd");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_user_command('Greet', function(o) print('hi ' .. o.args) end, {})\n",
    )
    .await;
    let map = redraw_after(&rpc, &mut incoming, ":Greet there<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hi there"),
        "typed :Greet should dispatch to the Lua user command"
    );
}

#[tokio::test]
async fn buf_local_user_command_is_scoped_to_its_buffer() {
    // `nvim_buf_create_user_command(buf, …)` registers a *buffer-local* command:
    // it dispatches only while `buf` is the current buffer, and is unknown
    // (E492) from any other buffer. Before the per-buffer registry it leaked into
    // `nx._user_commands` globally, so it fired everywhere.
    let a = write_temp("a", "txt", "a1\n");
    let b = write_temp("b", "txt", "b1\n");
    let (rpc, mut incoming) = start(None).await;

    command(&rpc, &format!("e {a}")).await; // buffer 1
    command(&rpc, &format!("e {b}")).await; // buffer 2, current
    exec_lua(
        &rpc,
        "vim.api.nvim_buf_create_user_command(2, 'BufLocal', \
         function() print('local hit') end, {})",
    )
    .await;

    // In its own buffer the command dispatches.
    let map = redraw_after(&rpc, &mut incoming, ":BufLocal<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("local hit"),
        ":BufLocal should dispatch in the buffer it was registered for"
    );

    // Switch to buffer 1 (the alternate); the command must be unknown there.
    feed(&rpc, "<C-^>");
    let map = redraw_after(&rpc, &mut incoming, ":BufLocal<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E492: Not an editor command: BufLocal"),
        "a buffer-local command must not leak into other buffers"
    );

    // Back in buffer 2 it dispatches again.
    feed(&rpc, "<C-^>");
    let map = redraw_after(&rpc, &mut incoming, ":BufLocal<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("local hit"),
        ":BufLocal should still dispatch back in its own buffer"
    );

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn deleting_a_buffer_purges_its_buffer_local_commands_and_keymaps() {
    // A deleted buffer's buffer-local registrations must not outlive it — else a
    // later buffer that reuses the bufnr would inherit a stale `:Cmd` / mapping.
    // Mirrors neovim, which drops buffer-local commands and maps on bufwipe.
    let a = write_temp("a", "txt", "a1\n");
    let b = write_temp("b", "txt", "b1\n");
    let (rpc, _incoming) = start(None).await;

    command(&rpc, &format!("e {a}")).await; // buffer 1
    command(&rpc, &format!("e {b}")).await; // buffer 2, current
    exec_lua(
        &rpc,
        "vim.api.nvim_buf_create_user_command(2, 'BufLocal', function() end, {})\n\
         vim.api.nvim_buf_set_keymap(2, 'n', '<leader>x', ':echo 1<CR>', {})",
    )
    .await;

    // Probe both registries for a buffer-2 entry: "<has-command>,<keymap-count>".
    let probe = "local kc = 0\n\
         for _, e in ipairs(nx._keymaps) do if e.buffer == 2 then kc = kc + 1 end end\n\
         return tostring(nx._buf_user_commands[2] ~= nil) .. ',' .. tostring(kc)";
    assert_eq!(
        exec_lua(&rpc, probe).await.as_str(),
        Some("true,1"),
        "buffer 2 should own one local command and one local keymap before deletion"
    );

    // Delete buffer 2 (switches to the alternate); its locals must be purged.
    command(&rpc, "bdelete").await;
    assert_eq!(
        exec_lua(&rpc, probe).await.as_str(),
        Some("false,0"),
        "deleting a buffer must purge its buffer-local commands and keymaps"
    );

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

#[tokio::test]
async fn unknown_command_still_reports_the_standard_error() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":Frobnicate<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E492: Not an editor command: Frobnicate"),
        "a command with no core handler and no user command is still an error"
    );
}

#[tokio::test]
async fn getcmdtype_reflects_the_active_command_line() {
    // `vim.fn.getcmdtype()` returns the type character of the open command line
    // (`:` ex, `/`/`?` search) or `""` when none is open. nvim-treesitter-context
    // calls it from a scheduled callback to skip work while the user is typing a
    // command. The value is mirrored from the editor's live cmdline state each
    // chunk, so a Lua read mid-command-line reflects this frame.
    let (rpc, _incoming) = start(None).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getcmdtype()").await.as_str(),
        Some(""),
        "getcmdtype() is empty when no command line is open"
    );

    feed(&rpc, ":");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getcmdtype()").await.as_str(),
        Some(":"),
        "an open `:` ex command line reports ':'"
    );

    feed(&rpc, "<Esc>/");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getcmdtype()").await.as_str(),
        Some("/"),
        "an open `/` forward search reports '/'"
    );

    feed(&rpc, "<Esc>?");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getcmdtype()").await.as_str(),
        Some("?"),
        "an open `?` backward search reports '?'"
    );

    feed(&rpc, "<Esc>");
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.getcmdtype()").await.as_str(),
        Some(""),
        "leaving the command line restores the empty getcmdtype()"
    );
}

#[tokio::test]
async fn ex_command_defines_a_user_command() {
    // `:command! Name {repl}` registers a user command whose replacement runs as
    // an ex-command on invocation — the way nvim-treesitter and most vimscript
    // plugins define their commands. Before this, the line failed with
    // `E492: Not an editor command: command!`, aborting the sourcing plugin.
    let (rpc, mut incoming) = start(None).await;
    command(&rpc, "command! Hello echo 'hi there'").await;
    let map = redraw_after(&rpc, &mut incoming, ":Hello<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("hi there"),
        ":command should register a user command that runs its replacement"
    );
}

#[tokio::test]
async fn ex_command_expands_q_args_in_the_replacement() {
    // `<q-args>` in the replacement expands to the (quoted) argument text at
    // invocation — the common form for forwarding an argument into the command.
    let (rpc, mut incoming) = start(None).await;
    command(&rpc, "command! -nargs=1 Say echo <q-args>").await;
    let map = redraw_after(&rpc, &mut incoming, ":Say boom<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("boom"),
        "<q-args> should expand to the quoted argument text"
    );
}

#[tokio::test]
async fn ex_command_without_bang_refuses_to_clobber() {
    // Re-defining an existing command without `!` is E174, matching vim — the
    // bang is the opt-in to replace. (Plugins always use `command!` for exactly
    // this reason.)
    let (rpc, mut incoming) = start(None).await;
    command(&rpc, "command Once echo 1").await;
    let map = redraw_after(&rpc, &mut incoming, ":command Once echo 2<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E174: Command already exists: add ! to replace it"),
        "redefining a command without ! should report E174"
    );
}

#[tokio::test]
async fn recursive_user_command_does_not_wedge_the_server() {
    // A user command whose callback re-invokes itself feeds run_pending's
    // fixpoint loop forever: each round runs the Lua callback, which queues the
    // command again. The server must cap the recursion, report it, and stay
    // responsive — not spin and wedge the single-threaded loop.
    let dir = temp_dir("recurse");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_user_command('Loop', function() vim.cmd('Loop') end, {})\n",
    )
    .await;

    // Before the fix this never returns (the server thread spins in
    // run_pending), so the whole exchange must complete within a timeout.
    let map = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        redraw_after(&rpc, &mut incoming, ":Loop<CR>"),
    )
    .await
    .expect("recursive command wedged the server: run_pending never converged");

    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("E132: command recursion limit exceeded"),
        "self-recursive command should be capped with an error, not loop forever"
    );

    // The server is still alive and processing input after bailing out.
    feed(&rpc, "ihi<Esc>");
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), lines(&rpc))
        .await
        .expect("server unresponsive after capping recursion");
    assert_eq!(
        got,
        vec!["hi".to_string()],
        "editing should work normally once the runaway command is stopped"
    );
}

#[tokio::test]
async fn colorscheme_style_plugin_load_runs_clean() {
    // A miniature plugin mimicking catppuccin's shape: setup() merges config,
    // load() sets options/globals and fires nvim_set_hl (incl. a link), and it
    // registers a user command and an autocmd. The whole load must run without a
    // Lua error — proving the Phase 2 surface is broad enough for that pattern.
    let dir = temp_dir("scheme");
    std::fs::create_dir_all(dir.join("lua").join("minischeme")).expect("create module dir");
    std::fs::write(
        dir.join("lua").join("minischeme").join("init.lua"),
        "local M = { options = {} }\n\
         function M.setup(conf)\n\
           M.options = vim.tbl_deep_extend('force', { flavour = 'default' }, conf or {})\n\
         end\n\
         function M.load()\n\
           if not M.options.flavour then M.setup() end\n\
           vim.o.termguicolors = true\n\
           vim.g.colors_name = 'minischeme-' .. M.options.flavour\n\
           vim.api.nvim_set_hl(0, 'Normal', { fg = '#cdd6f4', bg = '#1e1e2e' })\n\
           vim.api.nvim_set_hl(0, 'Comment', { fg = '#6c7086', italic = true })\n\
           vim.api.nvim_set_hl(0, '@keyword', { link = 'Keyword' })\n\
           vim.api.nvim_create_user_command('MiniScheme', function() M.load() end, {})\n\
           vim.api.nvim_create_autocmd('ColorScheme', { pattern = 'minischeme', callback = function() end })\n\
         end\n\
         return M\n",
    )
    .expect("write module");

    let (rpc, mut incoming) = start_with_config(
        &dir,
        "require('minischeme').setup({ flavour = 'mocha' })\n\
         require('minischeme').load()\n\
         print('ok ' .. tostring(vim.g.colors_name) .. ' tgc=' .. tostring(vim.o.termguicolors))\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "ok minischeme-mocha tgc=true",
        "the colorscheme-style load path should complete without error"
    );
}

// ----- vim.* surface the lsp/<server>.lua configs reach for --------------------
// nvim-lspconfig's `lsp/rust_analyzer.lua` (loaded by `vim.lsp.enable`) calls
// vim.tbl_get / vim.fs.relpath / vim.system / vim.json / vim.lsp.get_clients in
// its `root_dir`; before these existed, enabling it raised
// "attempt to call field 'tbl_get' (a nil value)". These cover the surface.

#[tokio::test]
async fn vim_tbl_get_follows_a_nested_key_path() {
    let dir = temp_dir("tblget");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local t = { a = { b = { c = 42 } } }\n\
         print(tostring(vim.tbl_get(t, 'a', 'b', 'c')) .. ' '\n\
           .. tostring(vim.tbl_get(t, 'a', 'x', 'c')) .. ' '\n\
           .. tostring(vim.tbl_get(t, 'a', 'b', 'c', 'd')))\n",
    )
    .await;
    // Present path -> value; a missing intermediate key -> nil; descending past a
    // scalar (c is 42, not a table) -> nil rather than an error.
    assert_eq!(startup_message(&rpc, &mut incoming).await, "42 nil nil");
}

#[tokio::test]
async fn vim_json_encodes_compact_and_pretty_and_round_trips() {
    let dir = temp_dir("json");
    // `vim.json`/`nx.json` is the public codec: a compact encode by default, a
    // 2-space multi-line document with `{ pretty = true }`, and a decode that
    // round-trips — so no plugin re-implements a pretty printer of its own.
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local v = { a = 1 }\n\
         local compact = vim.json.encode(v)\n\
         local pretty = nx.json.encode(v, { pretty = true })\n\
         local back = vim.json.decode(pretty)\n\
         print(compact .. ' | multiline=' .. tostring(pretty:find('\\n', 1, true) ~= nil)\n\
           .. ' | rt=' .. tostring(back.a))\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "{\"a\":1} | multiline=true | rt=1"
    );
}
