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
    // `vim._user_commands` globally, so it fired everywhere.
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
         for _, e in ipairs(vim._keymaps) do if e.buffer == 2 then kc = kc + 1 end end\n\
         return tostring(vim._buf_user_commands[2] ~= nil) .. ',' .. tostring(kc)";
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

#[cfg(unix)]
#[tokio::test]
async fn mkdir_honors_the_permissions_argument() {
    // `vim.fn.mkdir(path, "p", "0700")` must create a private directory, not one
    // with umask-default (world-readable) perms. init.lua runs at startup, so by
    // the time the server is up the directory exists with the requested mode.
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_dir("mkdir");
    let target = dir.join("private").join("nested");
    let init = format!(
        "vim.fn.mkdir('{}', 'p', '0700')\n",
        target.to_string_lossy()
    );
    let (_rpc, _incoming) = start_with_config(&dir, &init).await;

    let meta = std::fs::metadata(&target).expect("mkdir should have created the directory");
    assert_eq!(
        meta.permissions().mode() & 0o777,
        0o700,
        "mkdir should apply the prot argument, not the umask default"
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
async fn vim_fs_relpath_is_segment_aware() {
    let dir = temp_dir("relpath");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "print(vim.fs.relpath('/a/b', '/a/b/c/d') .. ' '\n\
           .. tostring(vim.fs.relpath('/a/b', '/a/bc')) .. ' '\n\
           .. vim.fs.relpath('/a/b', '/a/b'))\n",
    )
    .await;
    // Subpath -> relative remainder; "/a/bc" is NOT under "/a/b" (segment
    // boundary) -> nil; an equal path -> ".".
    assert_eq!(startup_message(&rpc, &mut incoming).await, "c/d nil .");
}

#[tokio::test]
async fn vim_json_decodes_and_encodes() {
    let dir = temp_dir("vimjson");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local d = vim.json.decode('{\"workspace_root\":\"/w\",\"n\":3,\"arr\":[10,20]}')\n\
         print(d.workspace_root .. ' ' .. d.n .. ' ' .. d.arr[2] .. ' ' .. vim.json.encode({ a = 1 }))\n",
    )
    .await;
    // Object -> string-keyed table, array -> 1-based sequence; encode emits an
    // object for a non-sequence table.
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "/w 3 20 {\"a\":1}"
    );
}

#[tokio::test]
async fn vim_lsp_get_clients_starts_empty() {
    let dir = temp_dir("getclients");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "print(#vim.lsp.get_clients() .. ' ' .. #vim.lsp.get_clients({ name = 'nope' }))\n",
    )
    .await;
    // No server has attached, so the list is empty with and without a filter.
    assert_eq!(startup_message(&rpc, &mut incoming).await, "0 0");
}

#[cfg(unix)]
#[tokio::test]
async fn lspconfig_style_root_dir_resolves_through_the_new_surface() {
    // A miniature `lsp/<name>.lua` shaped exactly like rust_analyzer's: its
    // `root_dir` reaches for vim.tbl_get, vim.system (shelling out), vim.json,
    // vim.fs.relpath and vim.lsp.get_clients. Driven through `vim.lsp.enable` +
    // the FileType dispatcher, the whole config must evaluate without a Lua error
    // — the regression the user hit ("attempt to call field 'tbl_get'").
    let dir = temp_dir("lspprobe");
    std::fs::create_dir_all(dir.join("lsp")).expect("create lsp dir");
    std::fs::write(
        dir.join("lsp").join("probe.lua"),
        // root_dir deliberately does NOT call on_dir: this asserts the API
        // surface a config evaluates, not the (separately covered) server spawn.
        r#"return {
  cmd = { 'true' },
  filetypes = { 'probe' },
  root_dir = function(bufnr, on_dir)
    local deep = vim.tbl_get(vim.lsp.config['probe'], 'settings', 'probe', 'missing')
    local res = vim.system({ '/bin/echo', '{"workspace_root":"/tmp/proj","n":2}' }, { text = true }):wait()
    local decoded = vim.json.decode(res.stdout)
    local rel = vim.fs.relpath('/a/b', '/a/x')
    local nclients = #vim.lsp.get_clients({ name = 'probe' })
    print(string.format('probe %s %s %d %s %d',
      decoded.workspace_root, tostring(deep), decoded.n, tostring(rel), nclients))
  end,
}
"#,
    )
    .expect("write lsp config");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.lsp.enable('probe')\nvim.lsp._on_filetype(0, 'probe')\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "probe /tmp/proj nil 2 nil 0",
        "an lspconfig-style root_dir should evaluate the new vim.* surface cleanly"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn host_primitives_for_lspconfig_are_available() {
    // The libuv/process/version surface the configs build defaults from. Bundled
    // into one assertion: cwd is resolvable, getpid is positive, a ubiquitous
    // binary (`sh`) is executable, vim.version() stringifies, vim.trim trims,
    // vim.empty_dict is empty, and the vim.lsp.rpc.start shim hands a cmd builder
    // back its argv (the mechanism behind the 20-plus rpc.start configs).
    let dir = temp_dir("hostprim");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local parts = {\n\
         \x20 tostring(vim.uv.cwd() ~= nil),\n\
         \x20 tostring(vim.fn.getpid() > 0),\n\
         \x20 tostring(vim.fn.executable('sh') == 1),\n\
         \x20 tostring(vim.version()),\n\
         \x20 vim.trim('  hi  '),\n\
         \x20 tostring(next(vim.empty_dict()) == nil),\n\
         \x20 table.concat(vim.lsp.rpc.start({ 'mybin', '--stdio' }, {}), ','),\n\
         }\n\
         print(table.concat(parts, ' '))\n",
    )
    .await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "true true true 0.11.0 hi true mybin,--stdio"
    );
}

#[tokio::test]
async fn vim_iter_handles_iterators_and_find_any() {
    // vim.iter must accept a stateless iterator triple (what vim.fs.parents
    // returns) — the fennel_ls/vala_ls root_dir pattern — and expose :find/:any.
    let dir = temp_dir("vimiter");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local p = vim.iter(vim.fs.parents('/a/b/c')):totable()\n\
         print(p[1] .. ' ' .. p[2] .. ' '\n\
           .. tostring(vim.iter({ 10, 20, 30 }):find(function(x) return x == 20 end)) .. ' '\n\
           .. tostring(vim.iter({ 1, 2, 3 }):any(function(x) return x > 2 end)))\n",
    )
    .await;
    // Ancestors of /a/b/c are /a/b, /a, … (the walk stops at /, not the cwd).
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        "/a/b /a 20 true"
    );
}

#[tokio::test]
async fn vim_fs_root_resolves_priority_tiers() {
    // vim.fs.root treats a list marker as an ordered priority chain (neovim 0.11):
    // the highest-priority tier with a match anywhere up the tree wins regardless
    // of depth, and a nested list is an equal-priority tier. Lay out a tree and
    // check both the ordered-beats-proximity rule and nested-tier matching.
    let dir = temp_dir("fsroot");
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join("sub").join("deep")).expect("mkdir tree");
    std::fs::write(proj.join("low"), "").expect("low marker"); // high up
    std::fs::write(proj.join("sub").join("g1"), "").expect("g1 marker"); // closer
    let src = proj.join("sub").join("deep").join("src.txt");
    let p = proj.to_string_lossy();

    // marker1: prefer 'top' (absent), then the equal-priority {g1,g2} tier (g1 is
    // at proj/sub) -> proj/sub. marker2: 'low' tier first (at proj) beats the
    // closer 'g1' (at proj/sub) -> proj.
    let init = format!(
        "print(vim.fs.root('{src}', {{ 'top', {{ 'g1', 'g2' }}, 'low' }}) .. ' | '\n\
         \x20 .. vim.fs.root('{src}', {{ 'low', 'g1' }}))\n",
        src = src.to_string_lossy()
    );
    let (rpc, mut incoming) = start_with_config(&dir, &init).await;
    assert_eq!(
        startup_message(&rpc, &mut incoming).await,
        format!("{p}/sub | {p}")
    );
}
