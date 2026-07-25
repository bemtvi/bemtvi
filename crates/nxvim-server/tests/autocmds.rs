//! Behavior tests for the autocmd substrate, driven black-box over RPC exactly
//! like `editing.rs` / `buffers.rs`. Phase 1 proves the *bridge* in isolation —
//! registration, augroup `clear`, manual firing via `nvim_exec_autocmds`, and
//! deletion — with **zero** editor lifecycle wiring: a callback that `print`s a
//! marker is fired manually (through `:lua`), and the marker on the message line
//! is the observable assertion. (Editor-emitted events arrive in Phases 2–3.)
//!
//! Integration-test files don't share a module, so the `start*/feed/...` helpers
//! here are copied from the `editing.rs` pattern rather than imported.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_test_harness::{
    command, exec_lua, feed, message, redraw_after, start_with_config, start_with_file_and_config,
    temp_dir,
};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Feed a `:lua <chunk><CR>` line and return the resulting message line — the
/// channel a fired callback's `print` lands on.
async fn lua_message(rpc: &Rpc, incoming: &mut UnboundedReceiver<Incoming>, chunk: &str) -> String {
    message(&redraw_after(rpc, incoming, &format!(":lua {chunk}<CR>")).await)
}

#[tokio::test]
async fn exec_autocmds_runs_callback_with_buffer_and_match_args() {
    // A callback registered for a custom event runs on nvim_exec_autocmds and
    // sees the buffer/pattern it was fired with, surfaced via print().
    let dir = temp_dir("au_exec");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('User', {\n\
         \x20 pattern = 'Marker',\n\
         \x20 callback = function(a) print('buf=' .. tostring(a.buf) .. ' match=' .. tostring(a.match)) end,\n\
         })\n",
    )
    .await;
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'Marker', buffer = 7 })",
    )
    .await;
    assert_eq!(msg, "buf=7 match=Marker");
}

#[tokio::test]
async fn augroup_clear_drops_prior_autocmds_no_double_fire() {
    // Re-running nvim_create_augroup(name, {clear=true}) must remove the group's
    // previous autocmd, so firing the event runs the callback exactly once even
    // after a re-register (the re-sourcing-a-config case).
    let dir = temp_dir("au_clear");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "local function register(tag)\n\
         \x20 local g = vim.api.nvim_create_augroup('G', { clear = true })\n\
         \x20 vim.api.nvim_create_autocmd('User', { group = g, pattern = 'M',\n\
         \x20   callback = function() print('fire ' .. tag) end })\n\
         end\n\
         register('first')\n\
         register('second')\n",
    )
    .await;
    // Both registrations cleared the group first, so only the 'second' callback
    // survives; firing prints it once, not once per registration.
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M' })",
    )
    .await;
    assert_eq!(msg, "fire second");
}

#[tokio::test]
async fn del_autocmd_stops_the_callback_firing() {
    let dir = temp_dir("au_del");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.au = vim.api.nvim_create_autocmd('User', { pattern = 'M',\n\
         \x20 callback = function() print('should-not-fire') end })\n",
    )
    .await;
    // Delete it, then fire: the callback must not run, so the message line is
    // whatever the del+fire line itself prints (a sentinel) — not the callback.
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_del_autocmd(_G.au) \
         vim.api.nvim_exec_autocmds('User', { pattern = 'M' }) print('done')",
    )
    .await;
    assert_eq!(msg, "done");
}

#[tokio::test]
async fn get_autocmds_reflects_clear_and_del() {
    // The introspection affordance: after a clear + a del, only the live autocmd
    // remains, and nvim_get_autocmds reports its event.
    let dir = temp_dir("au_get");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_augroup('G', { clear = true })\n\
         vim.api.nvim_create_autocmd('FileType', { group = 'G', callback = function() end })\n\
         vim.api.nvim_create_augroup('G', { clear = true })\n\
         _G.keep = vim.api.nvim_create_autocmd('User', { callback = function() end })\n",
    )
    .await;
    // Scope the queries to this test's own group / event: group `G` is empty after
    // the clear (its `FileType` autocmd gone), and the kept `User` autocmd remains.
    // (An unfiltered or by-event `FileType` query would now also include nxvim's
    // built-in ftplugin autocmds — `FileType nxdir`/`qf` install the explorer /
    // quickfix buffer-local maps — exactly as neovim's own built-ins do.)
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "local g = vim.api.nvim_get_autocmds({ group = 'G' }) \
         local u = vim.api.nvim_get_autocmds({ event = 'User' }) \
         print(#g .. ':' .. #u .. ':' .. u[1].event)",
    )
    .await;
    assert_eq!(msg, "0:1:User");
}

#[tokio::test]
async fn buffer_local_autocmd_only_fires_for_its_buffer() {
    // opts.buffer scopes an autocmd: firing for a different buffer is a no-op,
    // firing for its buffer runs it.
    let dir = temp_dir("au_buflocal");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('User', { buffer = 3, pattern = 'M',\n\
         \x20 callback = function() print('ran') end })\n",
    )
    .await;
    let other = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M', buffer = 9 }) print('miss')",
    )
    .await;
    assert_eq!(
        other, "miss",
        "buffer-local autocmd must not fire for buffer 9"
    );
    let mine = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M', buffer = 3 })",
    )
    .await;
    assert_eq!(
        mine, "ran",
        "buffer-local autocmd must fire for its own buffer"
    );
}

#[tokio::test]
async fn buf_get_name_and_expand_read_the_snapshot() {
    // The snapshot backs nvim_buf_get_name(0) and expand('%'...): set it, then
    // read the path and its modifiers.
    let dir = temp_dir("au_snapshot");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "nx._set_cur_buf(4, '/tmp/foo/bar.rs') \
         print(vim.api.nvim_buf_get_name(0) .. '|' .. vim.fn.expand('%:t') .. '|' .. vim.fn.expand('%:h'))",
    )
    .await;
    assert_eq!(msg, "/tmp/foo/bar.rs|bar.rs|/tmp/foo");
}

// ----- Phase 2: editor-emitted buffer lifecycle events -----------------------

#[tokio::test]
async fn opening_a_file_fires_filetype_with_filetype_and_path() {
    // The startup seed fires FileType for the opened buffer, with the pattern set
    // to the detected filetype and `args.file` the buffer's path.
    let dir = temp_dir("au_ft");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'ft=' .. a.match .. ' file=' .. a.file end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, '|'))").await;
    assert_eq!(msg, format!("ft=rust file={}", file.display()));
}

#[tokio::test]
async fn extension_detection_covers_installable_treesitter_grammars() {
    // `EXT_FILETYPE` (nxvim-core) maps a file extension to its tree-sitter language
    // noun, which is also the filetype. Its coverage is the installable
    // nvim-treesitter grammar set, so opening a file of any such language detects the
    // filetype (and highlights once the grammar is `:TSInstall`ed). This guards the
    // expansion beyond the original core-16: each extension below was NOT recognized
    // before, so an empty/reverted table fails. `.rs` anchors the pre-existing set.
    let dir = temp_dir("au_ext_detect");
    let cases = [
        ("main.rs", "rust"), // pre-existing anchor
        ("app.rb", "ruby"),
        ("Main.kt", "kotlin"),
        ("lib.hs", "haskell"),
        ("build.gradle", "groovy"),
        ("query.sql", "sql"),
        ("component.tsx", "tsx"),
        ("style.scss", "scss"),
    ];
    for (name, _) in cases {
        std::fs::write(dir.join(name), "x\n").expect("write source file");
    }
    let (rpc, _incoming) = start_with_file_and_config(&dir, "main.rs", "").await;
    for (name, want) in cases {
        let path = dir.join(name);
        nxvim_test_harness::feed(&rpc, &format!(":edit {}<CR>", path.display()));
        let _ = rpc.request("nvim_get_mode", vec![]).await;
        let got = exec_lua(&rpc, "return vim.bo.filetype").await;
        assert_eq!(
            got.as_str().unwrap_or("<nil>"),
            want,
            "{name} should detect filetype {want}"
        );
    }
}

#[tokio::test]
async fn lifecycle_order_is_bufreadpost_filetype_bufenter() {
    // First open of a file fires the three events in neovim's order.
    let dir = temp_dir("au_order");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         local function rec(tag) return function() _G.log[#_G.log+1] = tag end end\n\
         vim.api.nvim_create_autocmd('BufReadPost', { callback = rec('read') })\n\
         vim.api.nvim_create_autocmd('FileType', { callback = rec('ft') })\n\
         vim.api.nvim_create_autocmd('BufEnter', { callback = rec('enter') })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "read,ft,enter");
}

#[tokio::test]
async fn bufread_alias_fires_as_bufreadpost() {
    // `BufRead` is neovim's muscle-memory alias for `BufReadPost`. Registering on
    // it must fire on the startup read, and — matching neovim — the callback sees
    // the *canonical* event name, since the alias is normalized at registration.
    let dir = temp_dir("au_bufread_alias");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufRead', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = a.event end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "BufReadPost");
}

#[tokio::test]
async fn bufwrite_alias_fires_as_bufwritepre() {
    // `BufWrite` is neovim's alias for `BufWritePre`. A real `:w` must fire the
    // aliased handler exactly once, reported under the canonical name — and a
    // handler registered on `BufWritePre` sees the same single fire (proving the
    // alias collapses onto one event rather than double-firing).
    let dir = temp_dir("au_bufwrite_alias");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         local function rec(a) _G.log[#_G.log+1] = a.event end\n\
         vim.api.nvim_create_autocmd('BufWrite', { callback = rec })\n\
         vim.api.nvim_create_autocmd('BufWritePre', { callback = rec })\n",
    )
    .await;
    command(&rpc, "w").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "BufWritePre,BufWritePre");
}

#[tokio::test]
async fn exec_autocmds_accepts_event_alias() {
    // `nvim_exec_autocmds('BufRead')` must fire handlers registered under either
    // spelling — the alias is canonicalized on the manual-fire path too, so an
    // exec of the alias and an exec of the canonical name are interchangeable.
    let dir = temp_dir("au_exec_alias");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'canon' end })\n\
         vim.api.nvim_create_autocmd('BufRead', {\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'alias' end })\n",
    )
    .await;
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.api.nvim_exec_autocmds('BufRead', {}); print(table.concat(_G.log, ','))",
    )
    .await;
    assert_eq!(msg, "canon,alias");
}

#[tokio::test]
async fn bufadd_fires_for_a_buffer_added_after_startup() {
    // `BufAdd` fires when a buffer is added to the list, before its `BufReadPost`,
    // with the added buffer as `<afile>`. The startup buffer's id is pre-seeded, so
    // it fires *only* for the second file opened here — not the first — mirroring
    // how `WinNew`/`TabNew` skip the initial window/tab.
    let dir = temp_dir("au_bufadd");
    let first = dir.join("first.rs");
    let second = dir.join("second.rs");
    std::fs::write(&first, "fn a() {}\n").expect("write first");
    std::fs::write(&second, "fn b() {}\n").expect("write second");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        first.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufAdd', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = a.event .. ':' .. a.file end })\n",
    )
    .await;
    command(&rpc, &format!("e {}", second.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, '|'))").await;
    assert_eq!(msg, format!("BufAdd:{}", second.display()));
}

#[tokio::test]
async fn bufcreate_alias_fires_as_bufadd() {
    // `BufCreate` is neovim's alias for `BufAdd`; a handler registered on it fires
    // on the add, reported under the canonical name.
    let dir = temp_dir("au_bufcreate_alias");
    let first = dir.join("first.rs");
    let second = dir.join("second.rs");
    std::fs::write(&first, "fn a() {}\n").expect("write first");
    std::fs::write(&second, "fn b() {}\n").expect("write second");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        first.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufCreate', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = a.event end })\n",
    )
    .await;
    command(&rpc, &format!("e {}", second.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "BufAdd");
}

#[tokio::test]
async fn encodingchanged_fires_on_fileencoding_change() {
    // Changing the current buffer's `'fileencoding'` fires `EncodingChanged`, whose
    // `<amatch>` is the new encoding label. Opening the file at utf-8 only seeds the
    // baseline (see the next test), so the single log entry is the latin1 change.
    let dir = temp_dir("au_encchange");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('EncodingChanged', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = a.event .. ':' .. a.match end })\n",
    )
    .await;
    command(&rpc, "set fileencoding=latin1").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, '|'))").await;
    assert_eq!(msg, "EncodingChanged:latin1");
}

#[tokio::test]
async fn opening_a_file_does_not_fire_encodingchanged() {
    // A file opened at its detected encoding is not a *change* — the baseline is
    // seeded silently, so `EncodingChanged` fires nothing on open (neovim, whose
    // global `encoding` is fixed, fires nothing there either).
    let dir = temp_dir("au_enc_noopen");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('EncodingChanged', {\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'fired' end })\n",
    )
    .await;
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "print('[' .. table.concat(_G.log, ',') .. ']')",
    )
    .await;
    assert_eq!(msg, "[]");
}

#[tokio::test]
async fn fileencoding_alias_fires_as_encodingchanged() {
    // `FileEncoding` is neovim's deprecated alias for `EncodingChanged`; a handler
    // on it fires on the change, reported under the canonical name.
    let dir = temp_dir("au_fenc_alias");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('FileEncoding', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = a.event end })\n",
    )
    .await;
    command(&rpc, "set fileencoding=latin1").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "EncodingChanged");
}

#[tokio::test]
async fn switching_buffers_fires_bufenter_but_not_refire_filetype() {
    // Opening a second file announces it (FileType fires once); switching back to
    // the first, already-announced buffer fires BufEnter only — no FileType re-fire.
    let dir = temp_dir("au_switch");
    let a = dir.join("a.rs");
    let b = dir.join("b.lua");
    std::fs::write(&a, "fn main() {}\n").expect("write a");
    std::fs::write(&b, "return {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'ft' .. a.buf end })\n\
         vim.api.nvim_create_autocmd('BufEnter', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'be' .. a.buf end })\n",
    )
    .await;
    // startup: buffer 1 (a.rs) -> ft1, be1.
    // :edit b.lua -> buffer 2 announced -> ft2, be2.
    // :b1 -> back to buffer 1, already announced -> be1 only.
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    redraw_after(&rpc, &mut incoming, ":b1<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "ft1,be1,ft2,be2,be1");
}

#[tokio::test]
async fn bufreadpost_callback_reads_buffer_name_from_snapshot() {
    // A BufReadPost callback resolves the buffer that fired via the snapshot —
    // nvim_buf_get_name(0) returns its path.
    let dir = temp_dir("au_readname");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function() _G.seen = vim.api.nvim_buf_get_name(0) end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, file.display().to_string());
}

#[tokio::test]
async fn bufreadpost_setting_filetype_fires_filetype_autocmd() {
    // A BufReadPost (here via the `BufRead` alias) callback that sets the buffer's
    // filetype — the standard "detect by shebang / content" pattern — must trigger
    // the matching `FileType` autocmd. The file has no recognized extension, so its
    // detected filetype is empty; the callback promotes it to `python`, and a
    // `FileType python` handler must fire for that late assignment.
    let dir = temp_dir("au_ft_from_bufread");
    let file = dir.join("script"); // no extension -> no detected filetype
    std::fs::write(&file, "#!/usr/bin/env -S uv run\nprint('hi')\n").expect("write script");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufRead', {\n\
         \x20 callback = function(ev)\n\
         \x20   if vim.bo[ev.buf].filetype == '' then\n\
         \x20     vim.bo[ev.buf].filetype = 'python'\n\
         \x20   end\n\
         \x20 end })\n\
         vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 pattern = 'python',\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'ft=' .. a.match end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, '|'))").await;
    assert_eq!(msg, "ft=python");
}

// ----- Phase 3: mode event (InsertEnter) -------------------------------------

#[tokio::test]
async fn entering_insert_fires_insertenter_once_per_entry() {
    // InsertEnter fires on the transition *into* insert — once on the `i`, not
    // per typed character — and again on a fresh entry via `o`.
    let dir = temp_dir("au_insert");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('InsertEnter', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    // `iabc<Esc>`: enter insert (fires once), type three chars (stay in insert —
    // no re-fire), leave. The count proves typing doesn't re-trigger the event.
    redraw_after(&rpc, &mut incoming, "iabc<Esc>").await;
    let after_i = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(
        after_i, "1",
        "InsertEnter fires once on i, not per typed char"
    );
    // `o<Esc>`: open a line (a fresh insert entry) and leave — fires again.
    redraw_after(&rpc, &mut incoming, "o<Esc>").await;
    let after_o = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(
        after_o, "2",
        "re-entering insert via o fires InsertEnter again"
    );
}

#[tokio::test]
async fn insertenter_sees_buffer_context() {
    // The InsertEnter callback resolves the current buffer via the snapshot, just
    // like the buffer events do.
    let dir = temp_dir("au_insert_ctx");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('InsertEnter', {\n\
         \x20 callback = function(a) _G.seen = a.buf .. ':' .. vim.api.nvim_buf_get_name(0) end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, format!("1:{}", file.display()));
}

// ----- ModeChanged: the general mode-transition signal -----------------------

#[tokio::test]
async fn modechanged_fires_with_old_new_pattern() {
    // ModeChanged fires on every reported-mode transition, carrying the `old:new`
    // code pair on `args.match` — `n:i` entering insert, `i:n` leaving, `n:v`
    // entering visual. This is the general signal a mode-reactive statusline uses.
    // Count per transition into a table (rather than overwriting one var) so the
    // `:lua print(...)` reads — which themselves dip through command-line mode — can
    // never clobber the value being asserted; every read happens back in normal mode.
    let dir = temp_dir("au_modechanged");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.hits = {}\n\
         vim.api.nvim_create_autocmd('ModeChanged', {\n\
         \x20 callback = function(a) _G.hits[a.match] = (_G.hits[a.match] or 0) + 1 end })\n",
    )
    .await;
    // `i<Esc>`: enter insert (n:i) then leave it (i:n), back in normal to read.
    redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    let into_insert = lua_message(&rpc, &mut incoming, "print(_G.hits['n:i'])").await;
    assert_eq!(into_insert, "1", "entering insert reports n:i");
    let out_of_insert = lua_message(&rpc, &mut incoming, "print(_G.hits['i:n'])").await;
    assert_eq!(out_of_insert, "1", "leaving insert reports i:n");
    // A non-insert transition fires too (the signal isn't insert-only): n→visual.
    redraw_after(&rpc, &mut incoming, "v<Esc>").await;
    let into_visual = lua_message(&rpc, &mut incoming, "print(_G.hits['n:v'])").await;
    assert_eq!(into_visual, "1", "entering visual reports n:v");
}

#[tokio::test]
async fn modechanged_glob_pattern_matches_only_its_transition() {
    // A `*:i` pattern matches any transition *into* insert (the glob the autocmd
    // matcher already supports), and is silent for transitions that don't end in
    // insert — so a handler scoped to one mode fires only for it.
    let dir = temp_dir("au_modechanged_glob");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('ModeChanged', { pattern = '*:i',\n\
         \x20 callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    // n→visual→normal: never ends in insert, so `*:i` stays silent.
    redraw_after(&rpc, &mut incoming, "v<Esc>").await;
    let after_visual = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after_visual, "0", "*:i ignores a visual round trip");
    // n→insert matches `*:i` once (read from normal mode after `<Esc>`; the i:n
    // leave and the `:` command-line dips don't end in insert, so the count stays 1).
    redraw_after(&rpc, &mut incoming, "i<Esc>").await;
    let after_insert = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after_insert, "1", "*:i matches the transition into insert");
}

// ----- Phase 5: window lifecycle events --------------------------------------

#[tokio::test]
async fn splitting_fires_winnew_winleave_winenter_in_order() {
    // `<C-w>s` creates a window (WinNew), leaves the old one (WinLeave), and
    // enters the new one (WinEnter). The `match` is the window id, so each marker
    // carries which window fired.
    let dir = temp_dir("au_win_split");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinNew', { callback = rec('new') })\n\
         vim.api.nvim_create_autocmd('WinLeave', { callback = rec('leave') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n",
    )
    .await;
    // Drop the startup WinEnter(1) so we observe only the split's events.
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    redraw_after(&rpc, &mut incoming, "<C-w>s").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "new2,leave1,enter2");
}

#[tokio::test]
async fn closing_a_window_fires_winclosed_then_winenter_survivor() {
    // `<C-w>c` on the focused window fires WinClosed for it and WinEnter for the
    // survivor that takes focus.
    let dir = temp_dir("au_win_close");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinClosed', { callback = rec('closed') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n",
    )
    .await;
    // Split (focus moves to the new window 2), then clear the log.
    redraw_after(&rpc, &mut incoming, "<C-w>s").await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // Close the focused window 2; window 1 survives and takes focus.
    redraw_after(&rpc, &mut incoming, "<C-w>c").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "closed2,enter1");
}

#[tokio::test]
async fn focus_motion_fires_winleave_and_winenter() {
    let dir = temp_dir("au_win_focus");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('WinLeave', { callback = rec('leave') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('enter') })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "<C-w>s").await; // two windows, focus on 2
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // `<C-w>j` moves focus from window 2 (top) down to window 1 (bottom).
    redraw_after(&rpc, &mut incoming, "<C-w>j").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "leave2,enter1");
}

// ----- Phase 3: tab lifecycle events -----------------------------------------

#[tokio::test]
async fn tabnew_fires_tabnew_tableave_tabenter_in_order() {
    // `:tabnew` creates a tab (TabNew), leaves the old one (TabLeave), and enters
    // the new one (TabEnter). The `match` is the tab id, so each marker says which.
    let dir = temp_dir("au_tab_new");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('TabNew', { callback = rec('new') })\n\
         vim.api.nvim_create_autocmd('TabLeave', { callback = rec('leave') })\n\
         vim.api.nvim_create_autocmd('TabEnter', { callback = rec('enter') })\n",
    )
    .await;
    // Drop any startup events so we observe only the `:tabnew` transition.
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    redraw_after(&rpc, &mut incoming, ":tabnew<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "new2,leave1,enter2");
}

#[tokio::test]
async fn tab_switch_brackets_the_window_events() {
    // A tab switch fires the bracket `TabLeave → WinLeave → WinEnter → TabEnter`.
    // Recording only the tags (no ids) makes the assertion purely about ordering.
    let dir = temp_dir("au_tab_bracket");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function() _G.log[#_G.log+1] = tag end end\n\
         vim.api.nvim_create_autocmd('TabLeave', { callback = rec('TL') })\n\
         vim.api.nvim_create_autocmd('WinLeave', { callback = rec('WL') })\n\
         vim.api.nvim_create_autocmd('WinEnter', { callback = rec('WE') })\n\
         vim.api.nvim_create_autocmd('TabEnter', { callback = rec('TE') })\n",
    )
    .await;
    // Two tabs (now on tab 2), then clear the log so only the switch is observed.
    redraw_after(&rpc, &mut incoming, ":tabnew<CR>").await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    // `gT` switches from tab 2 back to tab 1.
    redraw_after(&rpc, &mut incoming, "gT").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "TL,WL,WE,TE", "tab events bracket the window events");
}

#[tokio::test]
async fn tabclose_fires_tabenter_survivor_then_tabclosed() {
    // Closing a tab enters the survivor (TabEnter) and then announces the gone tab
    // (TabClosed) — the tab, and its windows, are already removed.
    let dir = temp_dir("au_tab_close");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function(a) _G.log[#_G.log+1] = tag .. a.match end end\n\
         vim.api.nvim_create_autocmd('TabEnter', { callback = rec('enter') })\n\
         vim.api.nvim_create_autocmd('TabClosed', { callback = rec('closed') })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":tabnew<CR>").await; // on tab 2
    lua_message(&rpc, &mut incoming, "_G.log = {}").await;
    redraw_after(&rpc, &mut incoming, ":tabclose<CR>").await; // back to tab 1
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "enter1,closed2");
}

// ----- :autocmd / :augroup / :doautocmd ex-commands --------------------------
// The Vimscript front-end (nx._ex_autocmd / _ex_augroup / _ex_doautocmd) drives
// the same store the nvim_* API uses. These feed the `:`-command forms over RPC
// and observe firing through a command-string autocmd's `print` / a counter.

#[tokio::test]
async fn ex_autocmd_defines_a_command_autocmd_that_doautocmd_fires() {
    // `:autocmd {event} {pat} {cmd}` registers a command-string autocmd;
    // `:doautocmd {event} {pat}` fires it, running the command (here a `:lua
    // print`). Uses the `:au` / `:doau` abbreviations to prove those resolve too.
    let dir = temp_dir("ex_au_define");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":au User Marker lua print('fired')<CR>",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":doau User Marker<CR>").await);
    assert_eq!(msg, "fired");
}

#[tokio::test]
async fn ex_augroup_block_assigns_the_current_group() {
    // `:augroup Foo` … `:augroup END` groups the autocmds defined between them, so
    // nvim_get_autocmds reports the group name — exactly as the API's `group=`.
    let dir = temp_dir("ex_aug_block");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(&rpc, &mut incoming, ":augroup Foo<CR>").await;
    redraw_after(&rpc, &mut incoming, ":autocmd User M lua print('x')<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup END<CR>").await;
    let name = exec_lua(
        &rpc,
        "local a = vim.api.nvim_get_autocmds({ event = 'User' }) return a[1].group_name",
    )
    .await;
    assert_eq!(
        name.as_str(),
        Some("Foo"),
        "the autocmd landed in group Foo"
    );
}

#[tokio::test]
async fn ex_autocmd_bang_clears_matching_autocmds() {
    // `:autocmd! {event}` removes every autocmd for that event, so a later
    // `:doautocmd` fires nothing. The sentinel printed by the fire line itself is
    // what lands on the message line — never the (now-cleared) callback.
    let dir = temp_dir("ex_au_bang");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User M lua print('should-not-fire')<CR>",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":autocmd! User<CR>").await;
    let gone = exec_lua(
        &rpc,
        "return #vim.api.nvim_get_autocmds({ event = 'User' })",
    )
    .await;
    assert_eq!(gone.as_u64(), Some(0), ":autocmd! User cleared the autocmd");
    let msg = lua_message(
        &rpc,
        &mut incoming,
        "vim.cmd('doautocmd User M') print('done')",
    )
    .await;
    assert_eq!(msg, "done", "the cleared autocmd did not fire");
}

#[tokio::test]
async fn ex_augroup_bang_deletes_the_group_and_its_autocmds() {
    // `:augroup! Foo` deletes the group and every autocmd in it.
    let dir = temp_dir("ex_aug_bang");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(&rpc, &mut incoming, ":augroup Foo<CR>").await;
    redraw_after(&rpc, &mut incoming, ":autocmd User M lua print('x')<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup END<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup! Foo<CR>").await;
    let gone = exec_lua(
        &rpc,
        "return #vim.api.nvim_get_autocmds({ event = 'User' })",
    )
    .await;
    assert_eq!(gone.as_u64(), Some(0), "the group's autocmd was removed");
    let id = exec_lua(&rpc, "return nx._augroups.Foo == nil").await;
    assert_eq!(id.as_bool(), Some(true), "the group name was deleted");
}

// ----- write events: BufWritePre / BufWritePost -----------------------------

#[tokio::test]
async fn writing_a_file_fires_bufwritepre_then_bufwritepost() {
    // A successful `:w` fires BufWritePre then BufWritePost, each carrying the
    // written file's path as `args.file` — the order is the neovim contract.
    let dir = temp_dir("au_write");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'pre:' .. a.file end })\n\
         vim.api.nvim_create_autocmd('BufWritePost', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'post:' .. a.file end })\n",
    )
    .await;
    // Modify the buffer, then write it.
    redraw_after(&rpc, &mut incoming, "ax<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(
        msg,
        format!("pre:{f},post:{f}", f = file.display()),
        "BufWritePre precedes BufWritePost, both with the file path"
    );
}

#[tokio::test]
async fn bufwritepre_mutation_reaches_disk() {
    // vim's `BufWritePre` fires *before* the buffer is serialized, so a handler
    // that mutates the buffer (format-on-save, trim trailing whitespace) changes
    // what lands on disk. Here the handler upcases the line via `:%s`; after `:w`
    // the on-disk bytes must be the mutated text, not the pre-mutation text.
    let dir = temp_dir("au_write_pre_mutates");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function() vim.cmd([[%s/hello/HELLO/]]) end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let on_disk = std::fs::read_to_string(&file).expect("read back");
    assert_eq!(
        on_disk, "HELLO\n",
        "a BufWritePre handler's buffer mutation must be written to disk"
    );
}

#[tokio::test]
async fn async_bufwritepre_settles_before_the_write() {
    // A BufWritePre handler may be *async*: it returns a promise, and the write must wait
    // for that promise to settle before serializing. Here the handler upcases the line
    // only after a timer resolves (a tick later); the write is deferred until then, so
    // the mutated text still reaches disk. This is the async format-on-save contract.
    let dir = temp_dir("au_write_pre_async");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function()\n\
         \x20   return nx.promise.delay(30):next(function() vim.cmd([[%s/hello/HELLO/]]) end)\n\
         \x20 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    // The write lands a tick after the async handler settles; poll for the on-disk bytes.
    let mut on_disk = String::new();
    for _ in 0..100 {
        on_disk = std::fs::read_to_string(&file).expect("read back");
        if on_disk == "HELLO\n" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        on_disk, "HELLO\n",
        "the write must wait for the async BufWritePre handler to settle before serializing"
    );
}

#[tokio::test]
async fn rejecting_async_bufwritepre_still_writes() {
    // `all_settled` never rejects, so a BufWritePre handler whose async work *fails* (a
    // formatter that blows up) must not block the save — the write still lands. Edit the
    // buffer first so the written bytes are observably the edited content.
    let dir = temp_dir("au_write_pre_reject");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function()\n\
         \x20   return nx.promise.delay(20):next(function() error('formatter blew up') end)\n\
         \x20 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "A world<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let mut on_disk = String::new();
    for _ in 0..100 {
        on_disk = std::fs::read_to_string(&file).expect("read back");
        if on_disk == "hello world\n" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        on_disk, "hello world\n",
        "a rejecting async BufWritePre handler must not block the write"
    );
}

// ----- non-gating events are async-tolerant (promise tracked, not awaited) --

#[tokio::test]
async fn non_gating_event_async_handler_runs_in_background() {
    // Only `BufWritePre` *awaits* its handlers; every other event is *async-tolerant*
    // — a handler may return a promise and the fire returns without blocking, but the
    // async work must still run to completion (it isn't dropped). Here a `User` handler
    // flips `_G.ran` a tick after a timer; firing the event returns at once, yet the
    // background side effect lands. Mutation-test: change the fire path to drop/cancel
    // the returned promise and `_G.ran` never flips.
    let dir = temp_dir("au_async_bg");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.ran = false\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'Bg',\n\
         \x20 callback = function()\n\
         \x20   return nx.promise.delay(20):next(function() _G.ran = true end)\n\
         \x20 end })\n",
    )
    .await;
    // Fire the event; the handler returns its promise and the fire returns immediately.
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'Bg' })",
    )
    .await;
    // The timer resolves a tick later; poll for the background side effect. Each
    // `exec_lua` round-trip also drives a server tick, draining the settled timer.
    let mut ran = false;
    for _ in 0..100 {
        if exec_lua(&rpc, "return _G.ran").await.as_bool() == Some(true) {
            ran = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        ran,
        "an async non-gating handler's promise must run to completion in the background"
    );
}

#[tokio::test]
async fn non_gating_event_async_rejection_surfaces() {
    // An async-tolerant handler whose promise *rejects* (a failed request, a throw in a
    // `:next`) must not vanish silently — the fire path tracks it and surfaces the
    // rejection on the message line, named for the event that raised it. The test
    // captures `nx.notify` (the reporter the tracker calls); without the tracker's
    // `:catch` this contextual message never appears (the generic unhandled-rejection
    // reporter would fire via `vim.notify` instead, which this recorder doesn't see).
    let dir = temp_dir("au_async_reject");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.notified = {}\n\
         local orig = nx.notify\n\
         nx.notify = function(msg, level)\n\
         \x20 _G.notified[#_G.notified+1] = tostring(msg)\n\
         \x20 return orig(msg, level)\n\
         end\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'Boom',\n\
         \x20 callback = function()\n\
         \x20   return nx.promise.delay(20):next(function() error('kaboom') end)\n\
         \x20 end })\n",
    )
    .await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'Boom' })",
    )
    .await;
    let mut msg = String::new();
    for _ in 0..100 {
        msg = exec_lua(&rpc, "return _G.notified[1] or ''")
            .await
            .as_str()
            .unwrap_or("")
            .to_string();
        if !msg.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        msg.contains("User") && msg.contains("rejected") && msg.contains("kaboom"),
        "a rejecting non-gating handler must surface a contextual rejection; got {msg:?}"
    );
}

#[tokio::test]
async fn wall_fires_bufwritepre_before_bytes_per_buffer() {
    // `:wall` must fire `BufWritePre` before each buffer's bytes — and with that buffer
    // made current, so a mutating handler targets the *right* buffer (vim's aucmd_prepbuf).
    // Two modified buffers; the handler appends `X` to every line. Both files must land
    // mutated: if the pre fired after the bytes, neither `X` reaches disk; if the handler
    // ran against the current buffer twice, the non-current buffer's `X` would be missing.
    let dir = temp_dir("au_wall_pre");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "a\n").expect("seed a");
    std::fs::write(&b, "b\n").expect("seed b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function() vim.cmd([[%s/$/X/]]) end })\n",
    )
    .await;
    // Modify buffer a, open + modify buffer b (leaving b current), so both are dirty.
    redraw_after(&rpc, &mut incoming, "A1<Esc>").await;
    redraw_after(&rpc, &mut incoming, &format!(":e {}<CR>", b.display())).await;
    redraw_after(&rpc, &mut incoming, "A2<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":wall<CR>").await;
    assert_eq!(
        std::fs::read_to_string(&a).expect("read a"),
        "a1X\n",
        "the non-current buffer's BufWritePre mutation must reach its own file"
    );
    assert_eq!(
        std::fs::read_to_string(&b).expect("read b"),
        "b2X\n",
        "the current buffer's BufWritePre mutation must reach its file"
    );
}

#[tokio::test]
async fn wqa_writes_all_with_bufwritepre_then_quits() {
    // `:wqa` writes every modified buffer — each firing its BufWritePre (mutation
    // reaching disk) — and only then quits. Proves the local quit gate waits for the
    // whole batch's writes to commit before replaying `:qa`.
    let dir = temp_dir("au_wqa");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "a\n").expect("seed a");
    std::fs::write(&b, "b\n").expect("seed b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "vim.api.nvim_create_autocmd('BufWritePre', {\n\
         \x20 callback = function() vim.cmd([[%s/$/X/]]) end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "A1<Esc>").await;
    redraw_after(&rpc, &mut incoming, &format!(":e {}<CR>", b.display())).await;
    redraw_after(&rpc, &mut incoming, "A2<Esc>").await;
    // Fire-and-forget the quit (the connection tears down as it exits).
    feed(&rpc, ":wqa<CR>");
    // Wait for exit (an `nxvim_exit` notification or the closed channel).
    let quit = {
        let timeout = std::time::Duration::from_secs(2);
        loop {
            match tokio::time::timeout(timeout, incoming.recv()).await {
                Ok(None) => break true,
                Ok(Some(Incoming::Notification { method, .. })) if method == "nxvim_exit" => {
                    break true
                }
                Ok(Some(_)) => continue,
                Err(_) => break false,
            }
        }
    };
    assert!(quit, "`:wqa` quits once every write has committed");
    assert_eq!(std::fs::read_to_string(&a).expect("read a"), "a1X\n");
    assert_eq!(std::fs::read_to_string(&b).expect("read b"), "b2X\n");
}

// ----- quit/exit lifecycle: QuitPre / ExitPre / VimLeavePre / VimLeave -------

/// Config installing an exit-lifecycle handler on each event. Every handler appends its
/// event name to `_G.log`; the three gating `*Pre` handlers additionally return a promise
/// and stash its resolver in `_G.res[event]`, so a test can release the exit one stage at a
/// time and read `_G.log` at each checkpoint — deterministic, unlike observing the exit's
/// final redraw (which races the connection teardown).
const EXIT_LIFECYCLE_CONFIG: &str = "_G.log = {}\n\
     _G.res = {}\n\
     local function gate(name) return function()\n\
     \x20 _G.log[#_G.log+1] = name\n\
     \x20 return nx.promise.new(function(resolve) _G.res[name] = resolve end)\n\
     end end\n\
     local function rec(name) return function() _G.log[#_G.log+1] = name end end\n\
     vim.api.nvim_create_autocmd('QuitPre', { callback = gate('QuitPre') })\n\
     vim.api.nvim_create_autocmd('ExitPre', { callback = gate('ExitPre') })\n\
     vim.api.nvim_create_autocmd('VimLeavePre', { callback = gate('VimLeavePre') })\n\
     vim.api.nvim_create_autocmd('VimLeave', { callback = rec('VimLeave') })\n";

/// The comma-joined exit-lifecycle log so far (see [`EXIT_LIFECYCLE_CONFIG`]).
async fn exit_log(rpc: &Rpc) -> String {
    exec_lua(rpc, "return table.concat(_G.log, ',')")
        .await
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Drain notifications until the editor exits — an `nxvim_exit` **or** a closed channel
/// (the quit tears the connection down, and the close can beat the racing `nxvim_exit`) —
/// returning `true`, or `false` on a 2s timeout (the editor stayed alive).
async fn wait_for_exit(incoming: &mut UnboundedReceiver<Incoming>) -> bool {
    let timeout = std::time::Duration::from_secs(2);
    loop {
        match tokio::time::timeout(timeout, incoming.recv()).await {
            Ok(None) => return true,
            Ok(Some(Incoming::Notification { method, .. })) if method == "nxvim_exit" => {
                return true
            }
            Ok(Some(_)) => {}
            Err(_) => return false,
        }
    }
}

#[tokio::test]
async fn qa_gated_exit_lifecycle_fires_in_order() {
    // A clean `:qa` fires QuitPre -> ExitPre -> VimLeavePre -> VimLeave in neovim order, and
    // each `*Pre` is an *await gate*: the exit doesn't advance to the next event until the
    // handler's promise settles. Walked one stage at a time by releasing each gate and
    // reading the log — so both the order and the gating are asserted. Mutation-test: drop a
    // stage's fire and its checkpoint reads wrong; make a stage non-gating and releasing a
    // gate over-runs (or the editor exits before VimLeavePre).
    let dir = temp_dir("au_qa_order");
    let (rpc, mut incoming) = start_with_config(&dir, EXIT_LIFECYCLE_CONFIG).await;
    // Fire-and-forget the quit; it parks on the QuitPre gate.
    feed(&rpc, ":qa<CR>");
    assert_eq!(
        exit_log(&rpc).await,
        "QuitPre",
        "the exit parks on QuitPre first"
    );
    // Release each gate in turn; the next event fires and parks on its own gate.
    feed(&rpc, ":lua _G.res.QuitPre(true)<CR>");
    assert_eq!(
        exit_log(&rpc).await,
        "QuitPre,ExitPre",
        "releasing QuitPre advances to ExitPre"
    );
    feed(&rpc, ":lua _G.res.ExitPre(true)<CR>");
    assert_eq!(
        exit_log(&rpc).await,
        "QuitPre,ExitPre,VimLeavePre",
        "releasing ExitPre advances to VimLeavePre"
    );
    // Releasing the last gate runs VimLeave and exits (VimLeave is fired in the same code
    // step that sets `should_quit`, so the observed exit proves it fired).
    feed(&rpc, ":lua _G.res.VimLeavePre(true)<CR>");
    assert!(
        wait_for_exit(&mut incoming).await,
        "releasing VimLeavePre fires VimLeave and exits the editor"
    );
}

#[tokio::test]
async fn async_exit_handler_gates_the_quit_until_it_settles() {
    // The core contract: an exit handler may return a promise and the quit *waits* for it —
    // async flush/cleanup before quitting. While the ExitPre gate is unresolved the editor is
    // still alive (a follow-up exec_lua answers) and VimLeavePre/VimLeave have NOT fired.
    // Mutation-test: make the exit sequence non-gating and the editor exits immediately, so
    // the exec_lua below fails (connection gone) instead of reading "QuitPre,ExitPre".
    let dir = temp_dir("au_qa_gate");
    let (rpc, mut incoming) = start_with_config(&dir, EXIT_LIFECYCLE_CONFIG).await;
    feed(&rpc, ":qa<CR>");
    feed(&rpc, ":lua _G.res.QuitPre(true)<CR>");
    // Parked on the ExitPre gate: QuitPre + ExitPre fired, the editor is alive, and the
    // sequence is holding VimLeavePre/VimLeave back.
    assert_eq!(
        exit_log(&rpc).await,
        "QuitPre,ExitPre",
        "the quit must park on the async ExitPre handler, still alive, before VimLeavePre"
    );
    // Releasing the remaining gates settles the sequence and the editor exits.
    feed(&rpc, ":lua _G.res.ExitPre(true)<CR>");
    feed(&rpc, ":lua _G.res.VimLeavePre(true)<CR>");
    assert!(
        wait_for_exit(&mut incoming).await,
        "resolving the exit handlers releases the gate and the editor exits"
    );
}

#[tokio::test]
async fn bang_quit_fires_exit_sequence_past_e37() {
    // `:qa!` skips only the E37 modified-buffer guard — it still fires the gated exit
    // sequence, so a VimLeavePre flush / VimLeave cleanup runs on a force-quit too. The
    // contrast is the assertion: a bare `:qa` on a modified buffer is *refused* (E37) and
    // fires NO exit events, while `:qa!` fires them.
    let dir = temp_dir("au_qa_bang");
    let (rpc, mut incoming) = start_with_config(&dir, EXIT_LIFECYCLE_CONFIG).await;
    // Dirty the buffer so a bare `:qa` is refused.
    redraw_after(&rpc, &mut incoming, "ihi<Esc>").await;
    feed(&rpc, ":qa<CR>");
    assert_eq!(
        exit_log(&rpc).await,
        "",
        "a bare `:qa` on a modified buffer is refused by E37 and fires no exit events"
    );
    // `:qa!` overrides E37 and begins the sequence (parks on the QuitPre gate).
    feed(&rpc, ":qa!<CR>");
    assert_eq!(
        exit_log(&rpc).await,
        "QuitPre",
        "`:qa!` fires the exit sequence past the E37 guard"
    );
    feed(&rpc, ":lua _G.res.QuitPre(true)<CR>");
    feed(&rpc, ":lua _G.res.ExitPre(true)<CR>");
    feed(&rpc, ":lua _G.res.VimLeavePre(true)<CR>");
    assert!(
        wait_for_exit(&mut incoming).await,
        "`:qa!` runs the full exit sequence to exit"
    );
}

#[tokio::test]
async fn bufwritepost_sees_the_written_buffer_as_unmodified() {
    // After a `:w`, the BufWritePost callback resolves the saved buffer via the
    // snapshot and `vim.bo.modified` reads the now-cleared `[+]` flag.
    let dir = temp_dir("au_write_clean");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('BufWritePost', {\n\
         \x20 callback = function() _G.seen = vim.api.nvim_buf_get_name(0) end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "ax<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, file.display().to_string());
}

#[tokio::test]
async fn write_autocmd_with_a_glob_pattern_matches_by_extension() {
    // A `BufWritePost *.txt` autocmd fires for a `.txt` file (the glob matches the
    // path tail) but a `*.rs` one does not — the file-pattern matching the events
    // need to be useful (format-on-save is `BufWritePre *.rs`).
    let dir = temp_dir("au_write_glob");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufWritePost', { pattern = '*.txt',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'txt' end })\n\
         vim.api.nvim_create_autocmd('BufWritePost', { pattern = '*.rs',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'rs' end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "ax<Esc>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "txt", "only the *.txt glob matches a .txt file");
}

// ----- BufNewFile vs BufReadPost --------------------------------------------

#[tokio::test]
async fn opening_a_nonexistent_file_fires_bufnewfile_not_bufreadpost() {
    // Editing a path with no file on disk fires BufNewFile (with the path), and
    // *not* BufReadPost — matching `vim file-that-does-not-exist`.
    let dir = temp_dir("au_newfile");
    let file = dir.join("brand_new.rs"); // deliberately not created
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufNewFile', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'new:' .. a.file end })\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'read' end })\n",
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, format!("new:{}", file.display()));
}

#[tokio::test]
async fn editing_a_file_into_the_reused_noname_buffer_fires_bufreadpost() {
    // `:edit <file>` into the startup [No Name] buffer reuses that buffer in place
    // (same bufnr) — and must still fire BufReadPost, then FileType. neovim fires
    // BufReadPost on *every* read, regardless of whether the buffer id was seen
    // before; reusing the throwaway must not swallow the read events.
    let dir = temp_dir("au_edit_reuse_read");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local function rec(tag) return function() _G.log[#_G.log+1] = tag end end\n\
         vim.api.nvim_create_autocmd('BufReadPost', { callback = rec('read') })\n\
         vim.api.nvim_create_autocmd('FileType', { callback = rec('ft') })\n",
    )
    .await;
    redraw_after(
        &rpc,
        &mut incoming,
        &format!(":edit {}<CR>", file.display()),
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "read,ft");
}

#[tokio::test]
async fn editing_a_new_file_into_the_reused_noname_buffer_fires_bufnewfile() {
    // The BufNewFile mirror of the reuse case: `:edit <path-with-no-file>` into the
    // startup [No Name] buffer reuses it and fires BufNewFile (not BufReadPost).
    let dir = temp_dir("au_edit_reuse_new");
    let file = dir.join("brand_new.rs"); // deliberately not created
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufNewFile', {\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'new:' .. a.file end })\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'read' end })\n",
    )
    .await;
    redraw_after(
        &rpc,
        &mut incoming,
        &format!(":edit {}<CR>", file.display()),
    )
    .await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, format!("new:{}", file.display()));
}

#[tokio::test]
async fn reediting_the_current_file_refires_bufreadpost() {
    // `:e! <file>` re-reads the current file in place; neovim re-fires BufReadPost on
    // every read, so re-editing the current file fires it again. (The bare `:e!` with
    // no path is a separate gap — nxvim's `:edit` requires a file argument — out of
    // scope for this bug.)
    let dir = temp_dir("au_reedit_read");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('BufReadPost', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    // startup read = 1.
    let before = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(before, "1", "startup read fires BufReadPost once");
    redraw_after(&rpc, &mut incoming, &format!(":e! {}<CR>", file.display())).await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(
        after, "2",
        ":e! <file> re-reads the file and re-fires BufReadPost"
    );
}

// ----- BufLeave / BufDelete --------------------------------------------------

#[tokio::test]
async fn switching_buffers_fires_bufleave_for_the_old_buffer() {
    // `:edit b` fires BufLeave for the buffer we leave, then BufEnter for the new
    // one (vim's BufLeave → BufEnter bracket).
    let dir = temp_dir("au_bufleave");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "a\n").expect("write a");
    std::fs::write(&b, "b\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufLeave', {\n\
         \x20 callback = function(x) _G.log[#_G.log+1] = 'leave' .. x.buf end })\n\
         vim.api.nvim_create_autocmd('BufEnter', {\n\
         \x20 callback = function(x) _G.log[#_G.log+1] = 'enter' .. x.buf end })\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await; // drop startup events
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "leave1,enter2");
}

#[tokio::test]
async fn deleting_a_buffer_fires_bufdelete_for_it() {
    // `:bdelete` fires BufDelete, with `args.buf` the deleted buffer's number.
    let dir = temp_dir("au_bufdelete");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "a\n").expect("write a");
    std::fs::write(&b, "b\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.seen = nil\n\
         vim.api.nvim_create_autocmd('BufDelete', { callback = function(x) _G.seen = x.buf end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await; // buffer 2
    redraw_after(&rpc, &mut incoming, ":bdelete<CR>").await; // delete buffer 2
    let msg = lua_message(&rpc, &mut incoming, "print(_G.seen)").await;
    assert_eq!(msg, "2", "BufDelete fired for the deleted buffer");
}

// ----- InsertLeave -----------------------------------------------------------

#[tokio::test]
async fn leaving_insert_fires_insertleave_once_per_exit() {
    // InsertLeave fires on the transition *out* of insert — once per `<Esc>`, the
    // mirror of InsertEnter.
    let dir = temp_dir("au_insertleave");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('InsertLeave', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "iabc<Esc>").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "InsertLeave fires once on <Esc>");
    redraw_after(&rpc, &mut incoming, "o<Esc>").await;
    let after2 = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after2, "2", "a fresh insert via o fires InsertLeave again");
}

// ----- TextChanged / TextChangedI -------------------------------------------

#[tokio::test]
async fn editing_in_normal_fires_textchanged() {
    // A change in Normal mode (`x` deletes a char) fires TextChanged.
    let dir = temp_dir("au_textchanged");
    let file = dir.join("f.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('TextChanged', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "x").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "deleting a char in Normal fires TextChanged");
    // A pure motion does not change text — no re-fire.
    redraw_after(&rpc, &mut incoming, "l").await;
    let after2 = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after2, "1", "a motion alone doesn't fire TextChanged");
}

#[tokio::test]
async fn typing_in_insert_fires_textchangedi_per_change() {
    // Each character typed in insert fires TextChangedI (entering insert with `i`
    // doesn't, leaving with `<Esc>` doesn't).
    let dir = temp_dir("au_textchangedi");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('TextChangedI', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "iab<Esc>").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "2", "two typed chars fire TextChangedI twice");
}

// ----- CursorMoved / CursorMovedI -------------------------------------------

#[tokio::test]
async fn moving_the_cursor_fires_cursormoved() {
    // A motion in Normal mode fires CursorMoved each time the cursor lands somewhere
    // new; switching to the command line to read the counter doesn't move it.
    let dir = temp_dir("au_cursormoved");
    let file = dir.join("f.txt");
    std::fs::write(&file, "line one\nline two\nline three\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('CursorMoved', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "j").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "moving down a line fires CursorMoved");
    redraw_after(&rpc, &mut incoming, "j").await;
    let after2 = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after2, "2", "a second motion fires it again");
}

#[tokio::test]
async fn moving_the_cursor_in_insert_fires_cursormovedi() {
    // Moving within insert mode (`<Right>`, no text change) fires CursorMovedI;
    // entering insert with `i` and leaving with `<Esc>` do not.
    let dir = temp_dir("au_cursormovedi");
    let file = dir.join("f.txt");
    std::fs::write(&file, "hello\n").expect("seed file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('CursorMovedI', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "i<Right><Esc>").await;
    let after = lua_message(&rpc, &mut incoming, "print(_G.n)").await;
    assert_eq!(after, "1", "<Right> in insert fires CursorMovedI once");
}

// ----- WinScrolled -----------------------------------------------------------

/// A 100-line buffer — taller than the 24-row test grid — so the viewport can
/// actually scroll.
fn seed_tall_file(dir: &std::path::Path) -> std::path::PathBuf {
    let file = dir.join("f.txt");
    let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    std::fs::write(&file, body).expect("seed file");
    file
}

#[tokio::test]
async fn scrolling_the_viewport_fires_winscrolled() {
    // `G` jumps to the last line, scrolling the viewport down; with a `WinScrolled`
    // handler active that fires once for the scrolled window, whose id is the match.
    let dir = temp_dir("au_winscrolled");
    let file = seed_tall_file(&dir);
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n_G.m = ''\n\
         vim.api.nvim_create_autocmd('WinScrolled', { callback = function(a) _G.n = _G.n + 1; _G.m = a.match end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "G").await;
    let n = exec_lua(&rpc, "return _G.n").await.as_u64();
    assert_eq!(n, Some(1), "scrolling to the bottom fires WinScrolled once");
    let m = exec_lua(&rpc, "return _G.m").await;
    assert_eq!(m.as_str(), Some("1"), "match is the scrolled window's id");
}

#[tokio::test]
async fn cursor_motion_within_the_viewport_does_not_fire_winscrolled() {
    // WinScrolled tracks the viewport (topline/leftcol), not the cursor: a `j` that
    // stays on screen must not fire it — proving it isn't a CursorMoved in disguise.
    let dir = temp_dir("au_winscrolled_nocursor");
    let file = seed_tall_file(&dir);
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n\
         vim.api.nvim_create_autocmd('WinScrolled', { callback = function() _G.n = _G.n + 1 end })\n",
    )
    .await;
    redraw_after(&rpc, &mut incoming, "j").await; // line 1 -> 2, still visible
    let n = exec_lua(&rpc, "return _G.n").await.as_u64();
    assert_eq!(n, Some(0), "an on-screen cursor move does not scroll");
}

#[tokio::test]
async fn set_topline_scrolls_an_inactive_window_and_fires_winscrolled() {
    // `nx.win.set_topline(win, N)` scrolls an explicit (here inactive) window to the
    // 1-based topline N; the change fires WinScrolled for that window on the next
    // tick. This is the primitive a side-by-side diff plugin uses to mirror scroll.
    let dir = temp_dir("au_set_topline");
    let file = seed_tall_file(&dir);
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n_G.m = ''\n\
         vim.api.nvim_create_autocmd('WinScrolled', { callback = function(a) _G.n = _G.n + 1; _G.m = a.match end })\n",
    )
    .await;
    // Vertical split: focus moves to the new window (id 2); window 1 is now inactive.
    redraw_after(&rpc, &mut incoming, "<C-w>v").await;
    // Reset counters and scroll the *inactive* window 1 to line 40, all off-tick
    // (nvim_exec_lua queues the op and drains it but doesn't emit lifecycle events).
    exec_lua(&rpc, "_G.n = 0; _G.m = ''; nx.win.set_topline(1, 40)").await;
    // A bare tick (`<Esc>` no-op) lets the lifecycle diff observe window 1's scroll.
    redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let n = exec_lua(&rpc, "return _G.n").await.as_u64();
    assert_eq!(n, Some(1), "the programmatic scroll fires WinScrolled once");
    let m = exec_lua(&rpc, "return _G.m").await;
    assert_eq!(
        m.as_str(),
        Some("1"),
        "for the inactive window that was scrolled"
    );
    let top = exec_lua(&rpc, "return nx.win.call(1, vim.fn.winsaveview).topline")
        .await
        .as_u64();
    assert_eq!(
        top,
        Some(40),
        "winsaveview reports window 1's new 1-based topline"
    );
}

#[tokio::test]
async fn set_leftcol_horizontally_scrolls_an_inactive_window() {
    // `nx.win.set_leftcol(win, N)` moves a window's first visible screen column
    // (0-based) — the `'nowrap'` companion of set_topline for a side-by-side diff.
    let dir = temp_dir("au_set_leftcol");
    let file = seed_tall_file(&dir);
    let (rpc, mut incoming) = start_with_file_and_config(&dir, file.to_str().unwrap(), "").await;
    // Vertical split: focus moves to the new window (id 2); window 1 is now inactive.
    redraw_after(&rpc, &mut incoming, "<C-w>v").await;
    exec_lua(&rpc, "nx.win.set_leftcol(1, 7)").await;
    let left = exec_lua(&rpc, "return nx.win.call(1, vim.fn.winsaveview).leftcol")
        .await
        .as_u64();
    assert_eq!(
        left,
        Some(7),
        "set_leftcol moves the inactive window's leftcol"
    );
}

#[tokio::test]
async fn ex_autocmd_once_fires_exactly_once() {
    // `++once` self-removes after the first fire: firing the event twice runs the
    // command (a counter bump) only once.
    let dir = temp_dir("ex_au_once");
    let (rpc, mut incoming) = start_with_config(&dir, "_G.n = 0\n").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User M ++once lua _G.n = _G.n + 1<CR>",
    )
    .await;
    redraw_after(&rpc, &mut incoming, ":doautocmd User M<CR>").await;
    redraw_after(&rpc, &mut incoming, ":doautocmd User M<CR>").await;
    let n = exec_lua(&rpc, "return _G.n").await;
    assert_eq!(n.as_u64(), Some(1), "++once autocmd fired exactly once");
}

/// Autocmd glob bracket classes support negation: shell-style `[!a]` and
/// vim-style `[^a]` both exclude the listed characters (they must not match the
/// characters literally, which is what an unrepaired Lua class would do).
#[tokio::test]
async fn glob_bracket_negation_excludes_the_listed_chars() {
    let dir = temp_dir("au_negclass");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let out = exec_lua(
        &rpc,
        "_G.hits = {}\n\
         vim.api.nvim_create_autocmd('User', { pattern = '[!a]*.txt',\n\
         \x20 callback = function(a) _G.hits[#_G.hits + 1] = 'bang:' .. a.match end })\n\
         vim.api.nvim_create_autocmd('User', { pattern = '[^b]*.txt',\n\
         \x20 callback = function(a) _G.hits[#_G.hits + 1] = 'caret:' .. a.match end })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'a.txt' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'b.txt' })\n\
         return table.concat(_G.hits, ',')",
    )
    .await;
    // 'a.txt' is excluded by [!a] but allowed by [^b]; 'b.txt' the other way round.
    assert_eq!(out.as_str(), Some("caret:a.txt,bang:b.txt"));
}

/// A malformed bracket class in an autocmd pattern (`foo[bar`) must not blow up
/// the event fire: autocmds registered after it still run, and the malformed
/// spelling itself still matches exactly (it just matches nothing as a glob).
#[tokio::test]
async fn malformed_glob_class_does_not_abort_the_event_fire() {
    let dir = temp_dir("au_badclass");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let out = exec_lua(
        &rpc,
        "_G.good, _G.exact = 0, 0\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'foo[bar',\n\
         \x20 callback = function() _G.exact = _G.exact + 1 end })\n\
         vim.api.nvim_create_autocmd('User', { pattern = '*.txt',\n\
         \x20 callback = function() _G.good = _G.good + 1 end })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'foobar.txt' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'foo[bar' })\n\
         return 'good=' .. _G.good .. ',exact=' .. _G.exact",
    )
    .await;
    assert_eq!(
        out.as_str(),
        Some("good=1,exact=1"),
        "the malformed class must not raise mid-fire (skipping later autocmds); the exact spelling still matches"
    );
}

/// Log the basename of every buffer that fires `BufWinEnter`. Shared init for the
/// tests below; the accumulated `_G.bwe` is read back with `print`.
const BWE_INIT: &str = "_G.bwe = {}\n\
     vim.api.nvim_create_autocmd('BufWinEnter', { callback = function(a)\n\
     \x20 local f = a.file\n\
     \x20 _G.bwe[#_G.bwe+1] = (f ~= nil and f ~= '') and f:match('[^/]+$') or 'noname'\n\
     end })\n";

#[tokio::test]
async fn bufwinenter_fires_once_per_file_when_first_shown_in_a_window() {
    // BufWinEnter fires for the startup file (like BufReadPost), then once for a
    // second file when it's first displayed in a window. A no-arg `:split` (the
    // buffer is already displayed) and merely switching focus between windows do
    // NOT re-fire it — it tracks a buffer's window-visibility going 0 -> >=1.
    let dir = temp_dir("au_bwe_basic");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn main() {}\n").expect("write a");
    std::fs::write(&b, "fn other() {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(&dir, a.to_str().unwrap(), BWE_INIT).await;
    // startup: a.rs displayed -> [a.rs]
    // :vsplit -> new window shows a.rs (already displayed) -> no fire
    redraw_after(&rpc, &mut incoming, ":vsplit<CR>").await;
    // :edit b.rs in the (current) split -> b.rs first shown -> [a.rs, b.rs]
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    // <C-w>w back to the other window (a.rs) -> already displayed -> no fire
    redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.bwe, ','))").await;
    assert_eq!(msg, "a.rs,b.rs");
}

#[tokio::test]
async fn bufwinenter_fires_for_a_buffer_shown_in_a_non_current_window() {
    // The restore case: a buffer displayed in a window that is NOT the current
    // window still fires BufWinEnter, even though the current-buffer
    // BufReadPost/BufEnter diff never visits it. Reproduced without a full shada
    // restore by planting a fresh buffer into a background window via
    // `nvim_win_set_buf` while a different window holds focus.
    let dir = temp_dir("au_bwe_bg");
    let a = dir.join("a.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    let (rpc, mut incoming) = start_with_file_and_config(&dir, a.to_str().unwrap(), BWE_INIT).await;
    // The startup window (shows a.rs) — capture it before the split moves focus.
    let win_a = rpc
        .request("nvim_get_current_win", vec![])
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    // A no-arg :split duplicates a.rs into a new (now current) window; a.rs is
    // already displayed so BufWinEnter does NOT fire. Focus is now on the split,
    // leaving win_a in the background. bwe stays [a.rs].
    redraw_after(&rpc, &mut incoming, ":split<CR>").await;
    let cur = rpc
        .request("nvim_get_current_win", vec![])
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    assert_ne!(cur, win_a, "the split holds focus, not win_a");
    // Create a fresh (unnamed) buffer and plant it into win_a — the background
    // window. It transitions from shown-in-no-window to shown, so BufWinEnter must
    // fire for it (as 'noname'), even though win_a is not the current window and
    // its buffer never became the current buffer.
    let newbuf = rpc
        .request("nvim_create_buf", vec![])
        .await
        .unwrap()
        .as_u64()
        .unwrap();
    rpc.request(
        "nvim_win_set_buf",
        vec![Value::from(win_a), Value::from(newbuf)],
    )
    .await
    .unwrap();
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.bwe, ','))").await;
    assert_eq!(msg, "a.rs,noname");
}

#[tokio::test]
async fn bufreadpost_setting_filetype_fires_filetype_once_in_order() {
    // The canonical "detect the filetype in a read autocmd" pattern: a `BufReadPost`
    // callback overrides the buffer's filetype (here `.lua` -> `python`, the shebang
    // case). `FileType` must fire exactly ONCE, for the callback's final filetype,
    // and right after `BufReadPost` (neovim's `BufReadPost` -> `FileType` order).
    //
    // Regression: the `FileType` decision used a snapshot taken *before* `BufReadPost`
    // ran, so it fired `FileType lua` (the stale, pre-override filetype) and only
    // reached `FileType python` a diff later via the `run_pending` re-diff — a
    // spurious extra fire, for the wrong filetype, decoupled from `BufReadPost`.
    // Exercised on the deferred open path (a `BufReadCmd` handler is registered, as
    // the built-in explorer does, so `:edit` defers), where the bug surfaced.
    let dir = temp_dir("au_ft_after_bufread");
    let file = dir.join("thing.lua"); // detected ft = lua, overridden to python
    std::fs::write(&file, "-- x\nreturn 1\n").expect("write");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufReadCmd', { pattern = '*',\n\
         \x20 callback = function(a) if a.isdir then return true end end })\n\
         vim.api.nvim_create_autocmd('BufReadPost', { pattern = '*',\n\
         \x20 callback = function(ev)\n\
         \x20   _G.log[#_G.log+1] = 'read'\n\
         \x20   vim.bo[ev.buf].filetype = 'python'\n\
         \x20 end })\n\
         vim.api.nvim_create_autocmd('FileType', { pattern = '*',\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'ft:' .. a.match end })\n",
    )
    .await;
    exec_lua(&rpc, "_G.log = {}").await; // drop the startup buffer's events
                                         // Read the log the moment `:edit` converges — `exec_lua` drives no further
                                         // lifecycle emit — so a `FileType` that only fires on a *later* input, or a
                                         // spurious extra fire, is observed rather than masked.
    nxvim_test_harness::feed(&rpc, &format!(":edit {}<CR>", file.display()));
    let _ = rpc.request("nvim_get_mode", vec![]).await;
    let v = exec_lua(&rpc, "return table.concat(_G.log, ' ')").await;
    assert_eq!(v.as_str().unwrap_or("<nil>"), "read ft:python");
}

#[tokio::test]
async fn deferred_open_fires_read_events_in_order_before_bufwinenter() {
    // On the deferred open path (a `BufReadCmd` handler registered, as the built-in
    // explorer does), a `:edit` into a *new* buffer must fire the read lifecycle in
    // neovim's order — `BufReadPost` -> `FileType` -> `BufEnter` -> `BufWinEnter` —
    // every event seeing the loaded buffer's final filetype.
    //
    // Regression: `BufWinEnter` was not gated by the buffer's pending-open state, so
    // it fired *first*, over the empty/unnamed placeholder buffer and against the
    // pre-load filetype — e.g. `BufWinEnter(lua) BufReadPost(lua) FileType(python)
    // BufEnter(python)`.
    let dir = temp_dir("au_deferred_order");
    let a = dir.join("a.txt");
    let b = dir.join("thing.lua"); // detected ft = lua, overridden to python on read
    std::fs::write(&a, "aaa\n").expect("write a");
    std::fs::write(&b, "-- b\nreturn 2\n").expect("write b");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufReadCmd', { pattern = '*',\n\
         \x20 callback = function(x) if x.isdir then return true end end })\n\
         -- Override the filetype first, so the loggers below observe the final one.\n\
         vim.api.nvim_create_autocmd('BufReadPost', { pattern = '*',\n\
         \x20 callback = function(ev) vim.bo[ev.buf].filetype = 'python' end })\n\
         for _, ev in ipairs({ 'BufReadPost', 'BufEnter', 'BufWinEnter' }) do\n\
         \x20 vim.api.nvim_create_autocmd(ev, { pattern = '*', callback = function(x)\n\
         \x20   _G.log[#_G.log+1] = x.event .. '(' .. vim.bo[x.buf].filetype .. ')' end })\n\
         end\n\
         vim.api.nvim_create_autocmd('FileType', { pattern = '*', callback = function(x)\n\
         \x20 _G.log[#_G.log+1] = 'FileType(' .. x.match .. ')' end })\n",
    )
    .await;
    exec_lua(&rpc, "_G.log = {}").await; // drop the startup file's events
    nxvim_test_harness::feed(&rpc, &format!(":edit {}<CR>", b.display()));
    let _ = rpc.request("nvim_get_mode", vec![]).await;
    let v = exec_lua(&rpc, "return table.concat(_G.log, ' ')").await;
    assert_eq!(
        v.as_str().unwrap_or("<nil>"),
        "BufReadPost(python) FileType(python) BufEnter(python) BufWinEnter(python)",
    );
}

#[tokio::test]
async fn bufreadpost_setting_fileencoding_does_not_fire_spurious_encodingchanged() {
    // A `BufReadPost` callback that sets `vim.bo.fileencoding` is part of
    // establishing the buffer's encoding on read — like detection — so it must NOT
    // fire `EncodingChanged` (which fires only on a *later* in-place change). The
    // baseline is seeded silently with the callback's value.
    //
    // Regression twin of the `FileType` fix: the encoding decision used a snapshot
    // taken *before* `BufReadPost` ran, so the baseline recorded the stale
    // pre-callback encoding and a `run_pending` re-diff then fired a spurious,
    // decoupled `EncodingChanged`.
    let dir = temp_dir("au_enc_from_bufread");
    let file = dir.join("thing.txt");
    std::fs::write(&file, "hello\n").expect("write");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('BufReadCmd', { pattern = '*',\n\
         \x20 callback = function(a) if a.isdir then return true end end })\n\
         vim.api.nvim_create_autocmd('BufReadPost', { pattern = '*',\n\
         \x20 callback = function(ev) vim.bo[ev.buf].fileencoding = 'latin1' end })\n\
         vim.api.nvim_create_autocmd('EncodingChanged', { pattern = '*',\n\
         \x20 callback = function(a) _G.log[#_G.log+1] = 'enc:' .. a.match end })\n",
    )
    .await;
    exec_lua(&rpc, "_G.log = {}").await; // drop startup events
    nxvim_test_harness::feed(&rpc, &format!(":edit {}<CR>", file.display()));
    let _ = rpc.request("nvim_get_mode", vec![]).await;
    let v = exec_lua(&rpc, "return table.concat(_G.log, ' ')").await;
    assert_eq!(
        v.as_str().unwrap_or("<nil>"),
        "",
        "no spurious EncodingChanged"
    );
}

// ----- group-scoped manual firing -------------------------------------------
// `nvim_exec_autocmds` narrows a manual fire to one augroup, so a plugin can
// re-run its OWN handlers for a buffer without re-broadcasting the event to
// every other subscriber (the editor's LSP/treesitter/editorconfig wiring, the
// user's own autocmds). Without the filter a scoped fire is a global one — and
// silently so, which is the dangerous shape.

#[tokio::test]
async fn exec_autocmds_group_fires_only_that_groups_handlers() {
    // Two groups + an ungrouped autocmd all listen for the same event. Firing
    // with `group=` runs exactly one of them.
    let dir = temp_dir("au_exec_group");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         local mine = vim.api.nvim_create_augroup('Mine', { clear = true })\n\
         local other = vim.api.nvim_create_augroup('Other', { clear = true })\n\
         vim.api.nvim_create_autocmd('User', { group = mine, pattern = 'M',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'mine' end })\n\
         vim.api.nvim_create_autocmd('User', { group = other, pattern = 'M',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'other' end })\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'M',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'ungrouped' end })\n",
    )
    .await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M', group = 'Mine' })",
    )
    .await;
    let v = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        v.as_str().unwrap_or("<nil>"),
        "mine",
        "only the named group's handler ran"
    );
}

#[tokio::test]
async fn exec_autocmds_group_accepts_an_id_and_omitting_it_fires_all() {
    // The group may be given as the augroup id (what nvim_create_augroup
    // returned), not just its name; and no `group` still fires everything.
    let dir = temp_dir("au_exec_group_id");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         _G.gid = vim.api.nvim_create_augroup('Mine', { clear = true })\n\
         vim.api.nvim_create_autocmd('User', { group = _G.gid, pattern = 'M',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'mine' end })\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'M',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'ungrouped' end })\n",
    )
    .await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M', group = _G.gid })",
    )
    .await;
    let by_id = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(by_id.as_str().unwrap_or("<nil>"), "mine", "id narrows too");

    exec_lua(&rpc, "_G.log = {}").await;
    exec_lua(
        &rpc,
        "vim.api.nvim_exec_autocmds('User', { pattern = 'M' })",
    )
    .await;
    let all = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        all.as_str().unwrap_or("<nil>"),
        "mine,ungrouped",
        "no group = every handler, as before"
    );
}

#[tokio::test]
async fn exec_autocmds_unknown_group_fails_loud() {
    // A typo'd group name must NOT degrade into an unfiltered (global) fire, nor
    // quietly match nothing — either way the caller's intent is lost silently.
    let dir = temp_dir("au_exec_group_bad");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "_G.log = {}\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'M',\n\
         \x20 callback = function() _G.log[#_G.log+1] = 'ungrouped' end })\n",
    )
    .await;
    let err = exec_lua(
        &rpc,
        "local ok, e = pcall(vim.api.nvim_exec_autocmds, 'User',\n\
         \x20 { pattern = 'M', group = 'NoSuchGroup' })\n\
         return tostring(ok) .. '|' .. tostring(e) .. '|' .. table.concat(_G.log, ',')",
    )
    .await;
    let s = err.as_str().unwrap_or("<nil>");
    assert!(
        s.starts_with("false|"),
        "an unknown group raises, got {s:?}"
    );
    assert!(
        s.contains("NoSuchGroup"),
        "the error names the group, got {s:?}"
    );
    assert!(
        s.ends_with("|"),
        "and nothing fired (no global broadcast), got {s:?}"
    );
}

#[tokio::test]
async fn ex_doautocmd_accepts_a_group_argument() {
    // `:doautocmd [group] {event} [pattern]` — vim's optional group word. The
    // first word is the group only when it names one AND an event follows, so a
    // bare `:doautocmd User M` still reads `User` as the event.
    let dir = temp_dir("ex_doau_group");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(&rpc, &mut incoming, ":augroup Mine<CR>").await;
    redraw_after(&rpc, &mut incoming, ":autocmd User M lua print('mine')<CR>").await;
    redraw_after(&rpc, &mut incoming, ":augroup END<CR>").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User M lua print('other')<CR>",
    )
    .await;

    let scoped = message(&redraw_after(&rpc, &mut incoming, ":doautocmd Mine User M<CR>").await);
    assert_eq!(scoped, "mine", "the group word scoped the fire");

    // Without the group word both fire; the last one printed wins the message line.
    let all = message(&redraw_after(&rpc, &mut incoming, ":doautocmd User M<CR>").await);
    assert_eq!(all, "other", "no group word = event, as before");
}

#[tokio::test]
async fn ex_doautocmd_unknown_group_word_is_still_read_as_an_event() {
    // The disambiguation is "is this a known augroup?" — an unknown first word is
    // an event name, which is what makes `:doautocmd User M` keep working.
    let dir = temp_dir("ex_doau_group_unknown");
    let (rpc, mut incoming) = start_with_config(&dir, "").await;
    redraw_after(
        &rpc,
        &mut incoming,
        ":autocmd User M lua print('fired')<CR>",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, ":doautocmd User M<CR>").await);
    assert_eq!(msg, "fired");
}

#[tokio::test]
async fn clear_autocmds_unknown_group_fails_loud_instead_of_clearing_everything() {
    // The same resolver guards nvim_clear_autocmds, where a silently-dropped group
    // filter is worse than a broadcast: an unfiltered clear deletes EVERY autocmd.
    let dir = temp_dir("au_clear_group_bad");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "vim.api.nvim_create_autocmd('User', { pattern = 'M', callback = function() end })\n",
    )
    .await;
    let v = exec_lua(
        &rpc,
        "local before = #vim.api.nvim_get_autocmds({ event = 'User' })\n\
         local ok = pcall(vim.api.nvim_clear_autocmds, { group = 'NoSuchGroup' })\n\
         local after = #vim.api.nvim_get_autocmds({ event = 'User' })\n\
         return tostring(ok) .. '|' .. before .. '|' .. after",
    )
    .await;
    assert_eq!(
        v.as_str().unwrap_or("<nil>"),
        "false|1|1",
        "it raised and left the autocmds alone"
    );
}

#[tokio::test]
async fn create_autocmd_unknown_group_fails_loud_instead_of_registering_ungrouped() {
    // The sharpest edge of the group-resolution family. `augroup(name, {clear=true})`
    // + `create_autocmd({group = name})` is THE neovim idiom for "re-sourcing my
    // config must not stack handlers". Resolving a bad name to nil registered the
    // autocmd as UNGROUPED, so no later clear could ever reach it — handlers pile up
    // on every reload, which is the exact failure the idiom exists to prevent.
    let dir = temp_dir("au_create_group_bad");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let v = exec_lua(
        &rpc,
        "local ok, e = pcall(vim.api.nvim_create_autocmd, 'User',\n\
         \x20 { group = 'NotYetCreated', pattern = 'M', callback = function() end })\n\
         local n = #vim.api.nvim_get_autocmds({ event = 'User' })\n\
         return tostring(ok) .. '|' .. tostring(e) .. '|' .. n",
    )
    .await;
    let s = v.as_str().unwrap_or("<nil>");
    assert!(s.starts_with("false|"), "it raised, got {s:?}");
    assert!(
        s.contains("invalid augroup 'NotYetCreated'"),
        "the error names the group, got {s:?}"
    );
    assert!(
        s.ends_with("|0"),
        "and registered nothing (not an unreachable ungrouped autocmd), got {s:?}"
    );
}
