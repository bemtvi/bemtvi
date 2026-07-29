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
    barrier, command, exec_lua, feed, message, poll_true, redraw_after, settle_ms,
    start_with_config, start_with_file_and_config, temp_dir,
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
    // every read, so re-editing the current file fires it again. (The bare `:e!` reloads
    // the same way — see `rereading_the_current_buffer_fires_the_enter_sequence` and
    // `reediting_with_no_argument_reloads_the_current_file` in `tests/buffers.rs`.)
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
    // one (vim's BufLeave → BufEnter bracket), and the *read* of the buffer we arrive at
    // happens between them — neovim leaves first, then reads: `BufLeave` → `BufReadPost`
    // → `BufEnter` (verified against 0.12.2).
    //
    // Regression: `BufLeave` fired after the announce, so a plugin saving the outgoing
    // buffer's state on the way out ran *after* the incoming buffer's `BufReadPost` had
    // restored state for the new one — the two handlers of one plugin, inverted.
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
         \x20 callback = function(x) _G.log[#_G.log+1] = 'enter' .. x.buf end })\n\
         vim.api.nvim_create_autocmd('BufReadPost', {\n\
         \x20 callback = function(x) _G.log[#_G.log+1] = 'read' .. x.buf end })\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await; // drop startup events
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "leave1,read2,enter2");
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
    // BufWinEnter fires for the startup file (like BufReadPost), then for a second file
    // when a window displays it. A no-arg `:split` — whose new window *inherits* the
    // buffer it was split off rather than being given one — and merely switching focus
    // between windows display nothing, so neither fires.
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
async fn bufwinenter_fires_for_a_second_window_showing_an_already_shown_buffer() {
    // neovim fires `BufWinEnter` per *window display*, not once per buffer: opening a
    // buffer in a second window fires again, because `do_ecmd` fires unconditionally for
    // an already-loaded buffer. (`:h BufWinEnter` still claims `:split` with a file
    // already open in a window doesn't trigger — stale; nvim 0.12.2 fires.)
    //
    // Regression: the model was a per-buffer visibility edge (0 -> >=1 windows), so the
    // second window showing an already-displayed buffer was silently swallowed — a
    // per-window plugin (a statusline, a scrollbar) never initialised in that window.
    let dir = temp_dir("au_bwe_second_win");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    std::fs::write(&b, "fn b() {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(&dir, a.to_str().unwrap(), BWE_INIT).await;
    // A split onto a different file: a fresh display, fires (this much always worked).
    redraw_after(&rpc, &mut incoming, &format!(":vsplit {}<CR>", b.display())).await;
    // A split onto a.rs, which the startup window is *already* showing. A second window
    // now displays it, so it fires again.
    redraw_after(&rpc, &mut incoming, &format!(":vsplit {}<CR>", a.display())).await;
    // …and a *tab* onto it is the same story: a new window, a new display. (`:tabnew`
    // reaches the buffer through the find-or-load kernel rather than a split, so it is a
    // genuinely separate path through the diff.)
    redraw_after(&rpc, &mut incoming, &format!(":tabnew {}<CR>", a.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.bwe, ','))").await;
    assert_eq!(msg, "a.rs,b.rs,a.rs,a.rs");
}

#[tokio::test]
async fn bufwinenter_fires_every_time_a_window_changes_buffer() {
    // Displaying a buffer in a window fires *every time*, including switching back to one
    // this window showed a moment ago — neovim's `enter_buffer` fires unconditionally for
    // an already-loaded buffer, so `:b#`-style ping-ponging fires on each hop. Under the
    // old per-buffer visibility model the return trip was silent whenever the buffer was
    // still displayed somewhere.
    //
    // One window throughout: `:b <name>` routes focus to a window already showing that
    // buffer when there is one (nxvim's own `:drop`-like behavior), which would move
    // focus rather than change any window's buffer — a different case, covered by the
    // navigation assertions in `tab_switch_fires_no_window_lifecycle_events`.
    let dir = temp_dir("au_bwe_switch");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    std::fs::write(&b, "fn b() {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(&dir, a.to_str().unwrap(), BWE_INIT).await;
    redraw_after(&rpc, &mut incoming, &format!(":edit {}<CR>", b.display())).await;
    redraw_after(&rpc, &mut incoming, ":b a.rs<CR>").await;
    redraw_after(&rpc, &mut incoming, ":b b.rs<CR>").await;
    assert_eq!(
        lua_message(&rpc, &mut incoming, "print(table.concat(_G.bwe, ','))").await,
        "a.rs,b.rs,a.rs,b.rs"
    );
}

#[tokio::test]
async fn bufwinenter_fires_again_when_the_displayed_buffer_is_reread() {
    // `:e!` re-reads the file into the same bufnr in the same window: no window changed
    // which buffer it holds, but neovim fires `BufWinEnter` off the read itself
    // (`open_buffer`, after the modelines). A visibility-edge model saw nothing change
    // and stayed silent, so anything that sets up window-local state from buffer content
    // never re-ran after a reload.
    let dir = temp_dir("au_bwe_reread");
    let a = dir.join("a.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    let (rpc, mut incoming) = start_with_file_and_config(&dir, a.to_str().unwrap(), BWE_INIT).await;
    redraw_after(&rpc, &mut incoming, ":e!<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.bwe, ','))").await;
    assert_eq!(msg, "a.rs,a.rs");
}

#[tokio::test]
async fn rereading_the_current_buffer_fires_the_enter_sequence() {
    // A re-read of the buffer that is already current runs neovim's whole enter
    // sequence over the fresh read — `BufReadPost` → `BufEnter` → `BufWinEnter` — and no
    // `BufLeave`, because nothing was left. (Measured on nvim 0.12.2: `:e!` logs exactly
    // `BufReadPost, BufEnter, BufWinEnter`.)
    //
    // Regression: `BufEnter` was derived purely from the current-buffer id *changing*, so
    // a reload — which by definition changes nothing — was silent, while its `BufReadPost`
    // and `BufWinEnter` siblings both fired. A handler that sets a buffer up on entry
    // (the `BufEnter`-registered half of a plugin whose other half runs on the read) never
    // re-ran after `:e!`, leaving state derived from the *old* contents in place.
    let dir = temp_dir("au_reread_enter");
    let a = dir.join("a.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         for _, e in ipairs({ 'BufReadPost', 'BufLeave', 'BufEnter', 'BufWinEnter' }) do\n\
         \x20 nx.on(e, function() _G.log[#_G.log+1] = e end)\n\
         end\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await; // drop the startup sequence
    redraw_after(&rpc, &mut incoming, ":e!<CR>").await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "BufReadPost,BufEnter,BufWinEnter");
}

#[tokio::test]
async fn splitting_onto_the_current_file_displays_without_reading() {
    // `:vsplit <the file this window already shows>` gives the new window something it
    // wasn't showing, so it *enters* and *displays* — but it reads nothing, because the
    // buffer is right there. nvim 0.12.2 logs exactly `BufEnter, BufWinEnter`.
    //
    // Regression: nxvim ran the command through the ordinary `:edit` reload, so it also
    // re-read the file from disk (`BufReadPost`, plus the undo tree re-rooted and an
    // `E37` on a modified buffer — see `splitting_onto_the_file_you_are_already_editing_
    // reads_nothing` in `tests/buffers.rs`). Firing `BufWinEnter` off that spurious read
    // made the display look correct while it was riding the wrong signal entirely.
    let dir = temp_dir("au_split_same");
    let a = dir.join("a.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.log = {}\n\
         for _, e in ipairs({ 'BufReadPost', 'BufLeave', 'BufEnter', 'BufWinEnter' }) do\n\
         \x20 nx.on(e, function() _G.log[#_G.log+1] = e end)\n\
         end\n",
    )
    .await;
    lua_message(&rpc, &mut incoming, "_G.log = {}").await; // drop the startup sequence
    redraw_after(&rpc, &mut incoming, &format!(":vsplit {}<CR>", a.display())).await;
    let msg = lua_message(&rpc, &mut incoming, "print(table.concat(_G.log, ','))").await;
    assert_eq!(msg, "BufEnter,BufWinEnter");
}

#[tokio::test]
async fn bufwinenter_fires_with_the_displaying_window_current() {
    // `BufWinEnter` is about a *window*, and neovim has entered that window by the time a
    // handler runs — so per-window setup (the whole reason it fires per window) has to
    // address the window that displayed, not whichever one happens to be focused. Here a
    // *background* window is filled while another has focus: `:bdelete` rebinds the windows
    // showing the deleted buffer onto a survivor.
    //
    // The two windows carry different `'colorcolumn'` values, so reading it inside the
    // handler distinguishes them: seeing the background window's `42` means the handler
    // really ran in that window's context, and `7` would mean it read the focused one.
    // The editor's own focus must not move — running a handler is not a reason to take the
    // user's cursor somewhere else.
    let dir = temp_dir("au_bwe_win_ctx");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    std::fs::write(&b, "fn b() {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.seen = {}\n\
         nx.on('BufWinEnter', function(a)\n\
         \x20 _G.seen[#_G.seen+1] = tostring(nx.win.current()) .. '/cc=' ..\n\
         \x20   tostring(nx.wo.colorcolumn) .. '/' ..\n\
         \x20   ((a.file ~= nil and a.file ~= '') and a.file:match('[^/]+$') or '(noname)')\n\
         end)\n",
    )
    .await;
    // Window 2 shows b.rs and is focused; give the two windows distinguishable state.
    redraw_after(&rpc, &mut incoming, &format!(":vsplit {}<CR>", b.display())).await;
    exec_lua(
        &rpc,
        "nx.wo[2].colorcolumn = '42'; nx.wo[1].colorcolumn = '7'; return 1",
    )
    .await;
    // …then move focus to window 1, leaving 2 in the background.
    redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.win.current()").await.as_u64(),
        Some(1),
        "window 1 has focus before the background display"
    );

    exec_lua(&rpc, "_G.seen = {}").await;
    redraw_after(&rpc, &mut incoming, ":bdelete b.rs<CR>").await;
    assert_eq!(
        lua_message(&rpc, &mut incoming, "print(table.concat(_G.seen, ','))").await,
        "2/cc=42/a.rs",
        "the fire runs in the window that displayed — its id, and its window-local options"
    );
    assert_eq!(
        exec_lua(&rpc, "return nx.win.current()").await.as_u64(),
        Some(1),
        "and the editor's own focus never moved"
    );
}

#[tokio::test]
async fn a_mutation_bound_to_the_current_window_raises_in_a_background_fire() {
    // The other half of the contract. The window context is the *mirror* one, so reads and
    // explicit-handle writes retarget but a mutation that binds to "current" only when it
    // drains — an ex-command, feedkeys — would still land in the focused window. nxvim
    // cannot retarget those, so it raises, naming the fire: a handler is told its `nx.cmd`
    // went nowhere rather than silently editing the wrong window.
    //
    // The lock is only on while the two differ. Everything the user types displays into the
    // window they are in, so the ordinary path is unlocked — asserted here too, or this
    // guard would be indistinguishable from banning `nx.cmd` in the handler outright.
    let dir = temp_dir("au_bwe_win_lock");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    std::fs::write(&b, "fn b() {}\n").expect("write b");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        a.to_str().unwrap(),
        "_G.err = 'never ran'\n\
         nx.on('BufWinEnter', function()\n\
         \x20 local ok, e = pcall(function() nx.cmd('normal! gg') end)\n\
         \x20 _G.err = ok and 'allowed' or tostring(e)\n\
         end)\n",
    )
    .await;
    // Focused window: the display is into the window running the handler, so nothing locks.
    redraw_after(&rpc, &mut incoming, &format!(":vsplit {}<CR>", b.display())).await;
    assert_eq!(
        exec_lua(&rpc, "return _G.err").await.as_str(),
        Some("allowed"),
        "a fire for the focused window is the plain path — no lock"
    );

    // Background window: locked, and the message names the event and the window.
    redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    exec_lua(&rpc, "_G.err = 'never ran'").await;
    redraw_after(&rpc, &mut incoming, ":bdelete b.rs<CR>").await;
    let err = exec_lua(&rpc, "return _G.err")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        err.contains("the BufWinEnter fire for window 2") && err.contains("nx.cmd"),
        "a drain-time mutation in a background fire must raise, naming the fire and what \
         was blocked; got {err:?}"
    );
}

/// Log every window/tab lifecycle event by name. Shared init for the tab-switch tests.
const WINEV_INIT: &str = "_G.ev = {}\n\
     for _, e in ipairs({ 'BufWinEnter', 'WinNew', 'WinClosed', 'WinResized',\n\
     \x20                 'WinEnter', 'BufEnter', 'TabNew', 'TabEnter' }) do\n\
     \x20 nx.autocmd.create(e, { callback = function() _G.ev[#_G.ev+1] = e end })\n\
     end\n";

/// The names logged into `_G.ev` so far, as exact tokens (`WinEnter` must not match
/// inside `BufWinEnter`).
async fn win_events(rpc: &Rpc) -> Vec<String> {
    exec_lua(rpc, "return table.concat(_G.ev, ' ')")
        .await
        .as_str()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn tab_switch_fires_no_window_lifecycle_events() {
    // Switching tabs moves focus between windows that all continue to exist, so neovim
    // fires only the focus events — `WinEnter` / `TabEnter` / `BufEnter` — and nothing
    // is created, closed, resized, or newly displayed.
    //
    // Regression: the lifecycle diff enumerated `Editor::window_ids()`, which walks only
    // the *active* tab of each layer, so leaving a tab read as "those windows closed"
    // and arriving as "these windows are new". One `:tabnext` fired
    // `WinNew WinClosed BufWinEnter WinResized` on top of the three real events — a
    // window-tracking plugin saw its windows destroyed and recreated on every `gt`.
    let dir = temp_dir("au_tabswitch_quiet");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    std::fs::write(&b, "fn b() {}\n").expect("write b");
    let (rpc, mut incoming) =
        start_with_file_and_config(&dir, a.to_str().unwrap(), WINEV_INIT).await;
    // A second tab, single window like the first — so the switch below cannot legitimately
    // resize anything. Its own events are not under test.
    redraw_after(&rpc, &mut incoming, &format!(":tabnew {}<CR>", b.display())).await;
    exec_lua(&rpc, "_G.ev = {}").await;

    redraw_after(&rpc, &mut incoming, ":tabnext<CR>").await;
    let ev = win_events(&rpc).await;
    for real in ["WinEnter", "TabEnter", "BufEnter"] {
        assert!(
            ev.iter().any(|e| e == real),
            "the switch must still fire {real}; got {ev:?}"
        );
    }
    for spurious in ["WinNew", "WinClosed", "WinResized", "BufWinEnter"] {
        assert!(
            !ev.iter().any(|e| e == spurious),
            "nothing was created/closed/resized/newly-displayed by a tab switch, but \
             {spurious} fired; got {ev:?}"
        );
    }

    // Switching back is symmetric — the *arriving* tab's windows are equally not new.
    exec_lua(&rpc, "_G.ev = {}").await;
    redraw_after(&rpc, &mut incoming, ":tabnext<CR>").await;
    let ev = win_events(&rpc).await;
    for spurious in ["WinNew", "WinClosed", "WinResized", "BufWinEnter"] {
        assert!(
            !ev.iter().any(|e| e == spurious),
            "switching back fired {spurious}; got {ev:?}"
        );
    }
}

#[tokio::test]
async fn tabnew_and_tabclose_still_fire_win_new_and_closed() {
    // The guard on the test above: spanning every tab must suppress only the *spurious*
    // create/close pairs a switch invented, not the real ones. A new tab genuinely adds a
    // window (`TabNew` + `WinNew`) and closing it genuinely destroys one (`WinClosed`).
    let dir = temp_dir("au_tabnew_winnew");
    let a = dir.join("a.rs");
    let b = dir.join("b.rs");
    std::fs::write(&a, "fn a() {}\n").expect("write a");
    std::fs::write(&b, "fn b() {}\n").expect("write b");
    let (rpc, mut incoming) =
        start_with_file_and_config(&dir, a.to_str().unwrap(), WINEV_INIT).await;

    redraw_after(&rpc, &mut incoming, &format!(":tabnew {}<CR>", b.display())).await;
    let ev = win_events(&rpc).await;
    for real in ["TabNew", "WinNew"] {
        assert!(
            ev.iter().any(|e| e == real),
            ":tabnew creates a tab and a window, so {real} must fire; got {ev:?}"
        );
    }

    exec_lua(&rpc, "_G.ev = {}").await;
    redraw_after(&rpc, &mut incoming, ":tabclose<CR>").await;
    let ev = win_events(&rpc).await;
    assert!(
        ev.iter().any(|e| e == "WinClosed"),
        ":tabclose destroys the tab's window, so WinClosed must fire; got {ev:?}"
    );
    // The surviving tab's window is landed *on*, not filled: it goes on showing the
    // buffer it has shown all along, so — as in neovim — closing a tab displays nothing.
    assert!(
        !ev.iter().any(|e| e == "BufWinEnter"),
        ":tabclose returns to a window already showing its buffer, so BufWinEnter must \
         not fire; got {ev:?}"
    );
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

/// Autocmd patterns speak the full `nx.glob` dialect, so `**` crosses path
/// separators explicitly. Before the glob convergence the pattern matcher translated
/// globs into Lua patterns, where `**` was just two `.*` in a row and `{a,b}` was
/// literal text — both silently mismatched.
#[tokio::test]
async fn autocmd_patterns_support_doublestar_and_brace_alternation() {
    let dir = temp_dir("au_glob_dialect");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let out = exec_lua(
        &rpc,
        "_G.hits = {}\n\
         local function on(pat)\n\
         \x20 vim.api.nvim_create_autocmd('User', { pattern = pat,\n\
         \x20   callback = function(a) _G.hits[#_G.hits + 1] = pat .. '<-' .. a.match end })\n\
         end\n\
         on('src/**/*.rs')\n\
         on('*.{rs,toml}')\n\
         on('{a,b}/x.txt')\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'src/a/b/mod.rs' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'Cargo.toml' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'b/x.txt' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'c/x.txt' })\n\
         return table.concat(_G.hits, ',')",
    )
    .await;
    assert_eq!(
        out.as_str(),
        Some(
            "src/**/*.rs<-src/a/b/mod.rs,\
             *.{rs,toml}<-src/a/b/mod.rs,\
             *.{rs,toml}<-Cargo.toml,\
             {a,b}/x.txt<-b/x.txt"
        ),
        "`**` must span directories and `{{a,b}}` must alternate. `*.{{rs,toml}}` is \
         separator-less, so vim's basename rule ALSO fires it for `src/a/b/mod.rs` \
         (tail `mod.rs`). `c/x.txt` matches nothing."
    );
}

/// The convergence must NOT change vim's own file-pattern rules, which differ from
/// the `nx.glob` defaults on two counts: a bare `*` crosses `/` (nx.glob defaults to
/// stopping at it), and a separator-less pattern matches the path *tail*. Both are
/// passed explicitly by the autocmd matcher.
#[tokio::test]
async fn autocmd_patterns_keep_vims_star_and_basename_rules() {
    let dir = temp_dir("au_glob_vimrules");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let out = exec_lua(
        &rpc,
        "_G.hits = {}\n\
         vim.api.nvim_create_autocmd('User', { pattern = '*.lua',\n\
         \x20 callback = function(a) _G.hits[#_G.hits + 1] = 'tail:' .. a.match end })\n\
         vim.api.nvim_create_autocmd('User', { pattern = '/etc/*',\n\
         \x20 callback = function(a) _G.hits[#_G.hits + 1] = 'abs:' .. a.match end })\n\
         -- a separator-less glob matches the path TAIL, at any depth\n\
         vim.api.nvim_exec_autocmds('User', { pattern = '/a/b/c/init.lua' })\n\
         -- a `*` in a rooted pattern still crosses `/` (vim's rule, not nx.glob's default)\n\
         vim.api.nvim_exec_autocmds('User', { pattern = '/etc/nginx/nginx.conf' })\n\
         return table.concat(_G.hits, ',')",
    )
    .await;
    assert_eq!(
        out.as_str(),
        Some("tail:/a/b/c/init.lua,abs:/etc/nginx/nginx.conf"),
        "vim's file-pattern rules must survive the glob convergence"
    );
}

/// A `FileType`-style metacharacter-free pattern stays an EXACT compare — it must not
/// start glob-matching a path's tail just because the engine underneath can. (`rust`
/// must not match `/a/b/rust`.)
#[tokio::test]
async fn a_metacharacter_free_pattern_stays_an_exact_compare() {
    let dir = temp_dir("au_glob_exact");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let out = exec_lua(
        &rpc,
        "_G.hits = {}\n\
         vim.api.nvim_create_autocmd('User', { pattern = 'rust',\n\
         \x20 callback = function(a) _G.hits[#_G.hits + 1] = a.match end })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = '/a/b/rust' })\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'rust' })\n\
         return table.concat(_G.hits, ',')",
    )
    .await;
    assert_eq!(
        out.as_str(),
        Some("rust"),
        "a glob-free pattern matches only itself, never a path whose tail equals it"
    );
}

/// An autocmd whose pattern is a glob that cannot COMPILE must fail loud at
/// registration. Matching happens inside a `pcall` per event fire (it has to — an
/// autocmd must not raise out of every subsequent event), so without a registration
/// check the autocmd would register happily and then silently never fire, for the rest
/// of the session, with no diagnostic anywhere. The error names the pattern and the
/// reason, and nothing is registered.
#[tokio::test]
async fn an_uncompilable_autocmd_pattern_fails_loud_at_registration() {
    let dir = temp_dir("au_glob_invalid");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let out = exec_lua(
        &rpc,
        "_G.hits = {}\n\
         local function reg(pat)\n\
         \x20 return select(2, pcall(vim.api.nvim_create_autocmd, 'User', { pattern = pat,\n\
         \x20   callback = function(a) _G.hits[#_G.hits + 1] = a.match end }))\n\
         end\n\
         local before = reg('*.before')\n\
         local one = tostring(reg('x[z-a]*.lua'))\n\
         -- a list form must be checked element-wise, not just the first entry\n\
         local list = tostring(reg({ '*.lua', 'y[9-0]*.rs' }))\n\
         local after = reg('*.after')\n\
         vim.api.nvim_exec_autocmds('User', { pattern = 'a.after' })\n\
         return one:gsub('\\n.*', '') .. '\\n' .. list:gsub('\\n.*', '')\n\
         \x20 .. '\\nids=' .. tostring(after - before)\n\
         \x20 .. ' fired=' .. table.concat(_G.hits, ',')",
    )
    .await;
    let s = out.as_str().unwrap_or_default();
    assert!(
        s.contains("invalid pattern \"x[z-a]*.lua\"") && s.contains("'z' > 'a'"),
        "the error must name the offending pattern and the reason, got {s:?}"
    );
    assert!(
        s.contains("invalid pattern \"y[9-0]*.rs\""),
        "every pattern in a list form must be checked, got {s:?}"
    );
    assert!(
        s.contains("nx.autocmd.create") && s.contains("User"),
        "the error must name the call and the event, got {s:?}"
    );
    assert!(
        !s.contains("nxvim:prelude/autocmd"),
        "the raise must be positioned at the CALLER's line, not at the prelude source \
         the caller never wrote (error level 3, not 2), got {s:?}"
    );
    assert!(
        s.contains("ids=1 fired=a.after"),
        "a valid pattern must still register and fire, and the two rejected ones must \
         have consumed no autocmd id (so the id after them is the id before them + 1), \
         got {s:?}"
    );
}

// ----- hot-path events are synchronous-only -----------------------------------
//
// The event set splits in two (`docs/plans/2026-07-26-async-event-model.md`):
// *hot-path* events fire while converging a single input tick — i.e. on nearly every
// keypress — and their handlers must be synchronous, so the settle protocol (the
// replay pass and the gated read chain) can never park the input tick. Returning a
// promise from one is a contract violation and raises. Every *other* event stays
// async-capable, which is what the lazy-plugin machinery rides.

#[tokio::test]
async fn hot_path_handler_returning_a_promise_raises_and_names_the_event() {
    // A `CursorMoved` handler that returns a promise is a mistake: the editor will
    // never await it, so the author is expecting sequencing that cannot happen. It
    // must fail LOUD rather than be tracked and silently dropped — and the message
    // has to name the event, or the user cannot tell which of their handlers is
    // wrong.
    let dir = temp_dir("au_hot_async");
    let file = dir.join("hot.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").expect("write file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "nx.autocmd.create('CursorMoved', {\n\
         \x20 callback = function() return nx.promise.resolve(1) end })\n",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "j").await);
    assert!(
        msg.contains("CursorMoved handlers must be synchronous"),
        "the raise names the event and the rule, got {msg:?}"
    );
    assert!(
        msg.contains("nx.schedule"),
        "and carries the escape hatch so the fix is obvious from the message, got {msg:?}"
    );
}

#[tokio::test]
async fn hot_path_handler_may_start_async_work_it_does_not_return() {
    // The rule is "don't return a promise", NOT "no async work": a hot-path handler
    // may still kick off async work fire-and-forget (the statusline / diff plugins
    // do exactly this). That must keep working and the work must actually run —
    // otherwise the hard error above would have made hot-path handlers useless.
    let dir = temp_dir("au_hot_fire_forget");
    let file = dir.join("hot.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").expect("write file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.ran = false\n\
         nx.autocmd.create('CursorMoved', {\n\
         \x20 callback = function()\n\
         \x20   nx.promise.resolve(1):next(function() _G.ran = true end)\n\
         \x20 end })\n",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "j").await);
    assert!(
        !msg.contains("must be synchronous"),
        "starting async work without returning it is legal, got {msg:?}"
    );
    let ran = exec_lua(&rpc, "return tostring(_G.ran)").await;
    assert_eq!(
        ran.as_str(),
        Some("true"),
        "and the async continuation actually ran"
    );
}

#[tokio::test]
async fn non_hot_path_handler_may_return_a_promise() {
    // `FileType` is NOT hot-path — it fires roughly once per buffer, and an async
    // handler there is the whole point (an `ft`-lazy plugin whose `config` is async).
    // It must be accepted, or the lazy-load path this plan exists to fix would be
    // outlawed by phase 1.
    let dir = temp_dir("au_nonhot_async");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.ran = false\n\
         nx.autocmd.create('FileType', {\n\
         \x20 callback = function()\n\
         \x20   return nx.promise.resolve(1):next(function() _G.ran = true end)\n\
         \x20 end })\n",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert!(
        !msg.contains("must be synchronous"),
        "a non-hot-path handler may return a promise, got {msg:?}"
    );
    let ran = exec_lua(&rpc, "return tostring(_G.ran)").await;
    assert_eq!(ran.as_str(), Some("true"), "and its promise ran");
}

// ----- registration-site capture ----------------------------------------------

#[tokio::test]
async fn autocmd_records_the_file_and_line_it_was_registered_at() {
    // Every autocmd carries `site` — where the code that installed it lives. This is
    // what turns "a FileType handler exceeded its budget" (unactionable with N plugins
    // loaded) into "init.lua:3". Captured once per REGISTRATION, never per fire.
    //
    // Both entry points must report the CALLER, not the prelude: `nx.on` forwards to
    // `nx.autocmd.create`, so a naive `debug.getinfo(2)` would blame
    // `nxvim:prelude/nx` for every `nx.on` subscription in existence.
    let dir = temp_dir("au_site");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "nx.autocmd.create('User', { pattern = 'Direct', callback = function() end })\n\
         nx.on('User', { pattern = 'ViaOn' }, function() end)\n",
    )
    .await;
    let v = exec_lua(
        &rpc,
        "local out = {}\n\
         for _, au in ipairs(nx.autocmd.get({ event = 'User' })) do\n\
         \x20 out[#out+1] = tostring(au.pattern) .. '@' .. tostring(au.site)\n\
         end\n\
         return table.concat(out, '|')",
    )
    .await;
    let s = v.as_str().unwrap_or("<nil>");
    assert!(
        s.contains("Direct@") && s.contains("ViaOn@"),
        "both autocmds report a site, got {s:?}"
    );
    assert!(
        !s.contains("nxvim:prelude/"),
        "the site is the CALLER's config, not the forwarding prelude module, got {s:?}"
    );
    // init.lua line 1 registers the direct one, line 2 the nx.on one.
    assert!(
        s.contains("Direct@") && s.contains(":1|"),
        "the direct registration reports its own line, got {s:?}"
    );
    assert!(
        s.ends_with(":2"),
        "and nx.on reports the caller's line, got {s:?}"
    );
}

#[tokio::test]
async fn the_autocmd_listing_names_each_callbacks_registration_site() {
    // The interactive half of the same affordance. `:autocmd` renders every callback
    // handler as a bare `<callback>` — which, with N of them registered for the same
    // event, tells you nothing about which is which. It carries the site now.
    let dir = temp_dir("au_site_list");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "nx.autocmd.create('User', { pattern = 'Listed', callback = function() end })\n",
    )
    .await;
    // The listing is multi-line, so it lands in the messages panel rather than on the
    // message line; read what `:autocmd` renders directly.
    let listing = exec_lua(&rpc, "return nx._ex_autocmd(false, 'User')").await;
    let s = listing.as_str().unwrap_or("<nil>");
    assert!(
        s.contains("<callback>") && s.contains("init.lua:1"),
        "the listing names where the handler was registered, got {s:?}"
    );
}

#[tokio::test]
async fn hot_path_violation_names_the_registration_site() {
    // Phase 1's raise fell back to the autocmd id; with sites captured it must name
    // the file:line instead — the whole point of phase 2 is that this message is
    // actionable without further digging.
    let dir = temp_dir("au_hot_site");
    let file = dir.join("hot.txt");
    std::fs::write(&file, "one\ntwo\nthree\n").expect("write file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "nx.autocmd.create('CursorMoved', {\n\
         \x20 callback = function() return nx.promise.resolve(1) end })\n",
    )
    .await;
    let msg = message(&redraw_after(&rpc, &mut incoming, "j").await);
    assert!(
        msg.contains("registered at") && msg.contains("init.lua:1"),
        "the raise names where the offending handler was installed, got {msg:?}"
    );
}

// ----- settle protocol: replay to late subscribers ----------------------------
//
// A handler registered while an event was still in its async tail must still receive
// that event — the async analogue of neovim's synchronous "when the fire returns,
// everything it triggered has finished". This is what makes an `ft`-lazy plugin with
// an async `config` work at all: the trigger loads the plugin, the plugin registers
// its own FileType handler a tick later, and that handler still runs for the buffer
// that woke it.

/// Wrap `nx.notify` so the warnings the settle protocol emits are capturable, on top
/// of their normal delivery (which lands them in `:messages` via `Editor::echo` →
/// `record_message`). Prepended to a test's config.
const CAPTURE_WARNS: &str = "_G.warns = {}\n\
     local _real_notify = nx.notify\n\
     nx.notify = function(m, l, o) _G.warns[#_G.warns+1] = tostring(m); return _real_notify(m, l, o) end\n";

/// Poll until the captured warnings contain `needle`, then return them all.
async fn warns_containing(rpc: &Rpc, needle: &str) -> String {
    let ok = poll_true(
        rpc,
        &format!("return table.concat(_G.warns, '\\n'):find({needle:?}, 1, true) ~= nil"),
    )
    .await;
    let all = exec_lua(rpc, "return table.concat(_G.warns, '\\n')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        ok,
        "timed out waiting for a warning containing {needle:?}; got {all:?}"
    );
    all
}

#[tokio::test]
async fn late_subscriber_registered_during_an_async_handler_still_gets_the_event() {
    // The core guarantee. A FileType handler goes async and, on resolving, registers a
    // SECOND FileType handler — exactly the shape of a lazy plugin whose `config` is
    // async. Without replay the second handler never runs for the buffer that
    // triggered the load, which is the defect this phase exists to fix.
    let dir = temp_dir("au_replay_late");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         nx.autocmd.create('FileType', { callback = function()\n\
         \x20 _G.log[#_G.log+1] = 'first'\n\
         \x20 return nx.promise.delay(1):next(function()\n\
         \x20   nx.autocmd.create('FileType', { callback = function(a)\n\
         \x20     _G.log[#_G.log+1] = 'late:' .. a.match\n\
         \x20   end })\n\
         \x20 end)\n\
         end })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return #_G.log == 2").await,
        "the late handler ran; log was {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, '|')").await
    );
    let log = exec_lua(&rpc, "return table.concat(_G.log, '|')").await;
    assert_eq!(
        log.as_str(),
        Some("first|late:rust"),
        "the handler registered during the async tail still received the event"
    );
}

#[tokio::test]
async fn replay_does_not_refire_handlers_that_already_ran() {
    // The watermark is an EXACT filter on autocmd id, not a blanket re-fire: a handler
    // present at first dispatch must not run twice. lazy.nvim's re-fire re-runs
    // everyone and leans on handlers being idempotent; we must not need that.
    let dir = temp_dir("au_replay_once");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.n = 0\n_G.late = 0\n\
         nx.autocmd.create('FileType', { callback = function()\n\
         \x20 _G.n = _G.n + 1\n\
         \x20 return nx.promise.delay(1):next(function()\n\
         \x20   nx.autocmd.create('FileType', { callback = function() _G.late = _G.late + 1 end })\n\
         \x20 end)\n\
         end })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.late >= 1").await,
        "the late handler ran at all"
    );
    let counts = exec_lua(&rpc, "return _G.n .. '/' .. _G.late").await;
    assert_eq!(
        counts.as_str(),
        Some("1/1"),
        "the original handler ran exactly once and the late one exactly once"
    );
}

#[tokio::test]
async fn a_fires_two_live_replay_paths_still_deliver_at_most_once() {
    // The hard case for "no handler ever sees the same event twice". A fire whose budget
    // expires has TWO live replay paths from then on: the timeout replay (which may arm
    // further rounds for late subscribers that are themselves async) and the eventual
    // late-settle replay of the handler that blew the budget. Given a watermark per
    // round, each path advances its own copy, both dispatch the same id range, and a
    // handler registered in between runs TWICE — the defect the watermark exists to
    // prevent. One shared cursor per fire is what makes the guarantee hold.
    //
    // Timeline: t=0 the slow handler starts (30ms budget, 200ms of work); t=10 an async
    // second handler is registered — the timeout replay at t=30 reaches it and arms a
    // round; t=60 the observer is registered, so BOTH the armed round (t≈130) and the
    // late-settle replay (t≈200) would deliver to it.
    let dir = temp_dir("au_replay_two_paths");
    let (rpc, _incoming) = start_with_config(
        &dir,
        &(CAPTURE_WARNS.to_string()
            + "_G.seen = 0\n\
         nx.autocmd.create('User', { pattern = 'Dup', timeout = 30, callback = function()\n\
         \x20 return nx.promise.delay(200)\n\
         end })\n\
         nx.promise.delay(10):next(function()\n\
         \x20 nx.autocmd.create('User', { pattern = 'Dup', callback = function()\n\
         \x20   return nx.promise.delay(100)\n\
         \x20 end })\n\
         end)\n\
         nx.promise.delay(60):next(function()\n\
         \x20 nx.autocmd.create('User', { pattern = 'Dup', callback = function()\n\
         \x20   _G.seen = _G.seen + 1\n\
         \x20 end })\n\
         end)\n\
         nx.autocmd.exec('User', { pattern = 'Dup' })\n"),
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.seen >= 1").await,
        "the late subscriber received the event at all"
    );
    // Wait on the *event* rather than a wall-clock delay: the late-settle warning is
    // emitted by the same callback that runs the second path's replay, so once it has
    // landed, a duplicate delivery would already have happened.
    warns_containing(&rpc, "settled").await;
    let n = exec_lua(&rpc, "return _G.seen").await;
    assert_eq!(
        n.as_i64(),
        Some(1),
        "delivered exactly once despite two live replay paths"
    );
}

#[tokio::test]
async fn a_handler_past_its_budget_warns_and_the_replay_happens_anyway() {
    // The budget bounds how long we WAIT, never whether late subscribers get the
    // event: one slow handler must not cost every other subscriber the fire (nor, once
    // the read chain gates on this, wedge the buffer half-initialized). The warning
    // must name the registration site or it is unactionable with N plugins loaded.
    let dir = temp_dir("au_replay_budget");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let cfg = format!(
        "{CAPTURE_WARNS}\
         _G.late = false\n\
         nx.autocmd.create('FileType', {{ timeout = 30, callback = function()\n\
         \x20 nx.promise.delay(1):next(function()\n\
         \x20   nx.autocmd.create('FileType', {{ callback = function() _G.late = true end }})\n\
         \x20 end)\n\
         \x20 return nx.promise.delay(1500)\n\
         end }})\n"
    );
    let (rpc, _incoming) = start_with_file_and_config(&dir, file.to_str().unwrap(), &cfg).await;
    assert!(
        poll_true(&rpc, "return _G.late == true").await,
        "the replay ran despite the slow handler still being in flight"
    );
    let msgs = warns_containing(&rpc, "budget").await;
    assert!(
        msgs.contains("exceeded its 30ms budget"),
        "the expiry warning names the budget, got {msgs:?}"
    );
    assert!(
        msgs.contains("init.lua:"),
        "and names the registration site, got {msgs:?}"
    );
}

#[tokio::test]
async fn a_handler_that_settles_late_warns_with_its_elapsed_time() {
    // A handler that blew its budget and then finished has to say so — that is the
    // only thing distinguishing "slow" from "hung", and they want different fixes.
    let dir = temp_dir("au_replay_late_settle");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let cfg = format!(
        "{CAPTURE_WARNS}\
         nx.autocmd.create('FileType', {{ timeout = 20, callback = function()\n\
         \x20 return nx.promise.delay(300)\n\
         end }})\n"
    );
    let (rpc, _incoming) = start_with_file_and_config(&dir, file.to_str().unwrap(), &cfg).await;
    let msgs = warns_containing(&rpc, "settled").await;
    assert!(
        msgs.contains("past its 20ms budget"),
        "the late-settle warning reports the elapsed time against the budget, got {msgs:?}"
    );
    // The site must survive to the COMPLETION warning. By then every handler is
    // `done`, so recomputing "who is unsettled" yields nothing — the sites have to be
    // the ones captured when the budget blew, or the warning names no one and is
    // useless. (Caught by examples/async-events, which printed `handler () settled`.)
    let late = msgs
        .lines()
        .find(|l| l.contains("settled"))
        .unwrap_or_default();
    assert!(
        late.contains("init.lua:"),
        "the late-settle warning still names the registration site, got {late:?}"
    );
}

#[tokio::test]
async fn a_hung_handler_stays_visible_in_autocmd_pending() {
    // A handler that never settles never warns on completion, so the expiry warning
    // plus this introspection listing are the only evidence it exists. Without it a
    // permanently-hung handler is invisible.
    let dir = temp_dir("au_pending");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        // A promise nobody ever resolves.
        "nx.autocmd.create('FileType', { timeout = 20, callback = function()\n\
         \x20 return nx.promise.new(function() end)\n\
         end })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return #nx.autocmd.pending() > 0").await,
        "the hung handler is listed"
    );
    let p = exec_lua(
        &rpc,
        "local p = nx.autocmd.pending()[1]\n\
         return p.event .. '@' .. tostring(p.site) .. '@' .. tostring(p.budget)",
    )
    .await;
    let s = p.as_str().unwrap_or("<nil>");
    assert!(
        s.starts_with("FileType@") && s.contains("init.lua:") && s.ends_with("@20"),
        "listed with its event, site and budget, got {s:?}"
    );
}

#[tokio::test]
async fn replay_gives_up_loudly_when_handlers_keep_registering_handlers() {
    // An unbounded registration loop must fail loud rather than spin forever — the
    // fixpoint needs a cap, and hitting it has to say so rather than going quiet.
    let dir = temp_dir("au_replay_cap");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let cfg = format!(
        "{CAPTURE_WARNS}\
         _G.n = 0\n\
         local function arm()\n\
         \x20 nx.autocmd.create('FileType', {{ callback = function()\n\
         \x20   _G.n = _G.n + 1\n\
         \x20   return nx.promise.delay(1):next(arm)\n\
         \x20 end }})\n\
         end\n\
         arm()\n"
    );
    let (rpc, _incoming) = start_with_file_and_config(&dir, file.to_str().unwrap(), &cfg).await;
    let msgs = warns_containing(&rpc, "did not converge").await;
    assert!(
        msgs.contains("FileType"),
        "the cap warning names the event, got {msgs:?}"
    );
    let n = exec_lua(&rpc, "return _G.n").await.as_i64().unwrap_or(-1);
    assert!(
        (1..=16).contains(&n),
        "and it terminated rather than spinning, got {n} rounds"
    );
}

// ----- the gated read chain ---------------------------------------------------
//
// Neovim's ordering guarantee is trivially "when BufReadPost returns, everything it
// triggered has finished, so FileType fires into a settled world" — it is synchronous.
// We reproduce it explicitly: each stage of BufReadPost -> FileType -> BufEnter waits
// for the previous stage's async handlers (and their replay rounds) to converge.

#[tokio::test]
async fn filetype_waits_for_an_async_bufreadpost_handler() {
    // The ordering payoff. A BufReadPost handler detects the filetype ASYNCHRONOUSLY
    // (reading content, consulting a server) and sets it. Ungated, FileType would
    // already have fired for the extension-derived filetype and the real one would
    // arrive a diff later via the ft_changed re-fire — two FileType events, the first
    // wrong. Gated, FileType fires ONCE, with the detected value.
    let dir = temp_dir("au_chain_ft");
    let file = dir.join("script.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.fts = {}\n\
         nx.autocmd.create('BufReadPost', { callback = function()\n\
         \x20 return nx.promise.delay(5):next(function() nx.bo[nx.buf.current()].filetype = 'detected' end)\n\
         end })\n\
         nx.autocmd.create('FileType', { callback = function(a) _G.fts[#_G.fts+1] = a.match end })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return #_G.fts > 0").await,
        "FileType eventually fired"
    );
    // Give any spurious second fire a real chance to land before asserting there isn't
    // one: wait out anything still in flight, then force a diff with a keypress — the
    // `ft_changed` re-fire rides that path, and it is what produced the second, wrong
    // FileType before the chain gated the sequence.
    settle_ms(&rpc, 80).await;
    feed(&rpc, "jk");
    barrier(&rpc).await;
    let fts = exec_lua(&rpc, "return table.concat(_G.fts, ',')").await;
    assert_eq!(
        fts.as_str(),
        Some("detected"),
        "FileType fired exactly once, with the filetype the async BufReadPost handler set"
    );
}

#[tokio::test]
async fn bufenter_is_sequenced_after_the_chains_gates() {
    // BufEnter stays a synchronous hot-path event, but its POSITION is deferred: it
    // must not beat a still-settling FileType, or a handler keyed on "the buffer is
    // ready" sees a half-announced buffer.
    let dir = temp_dir("au_chain_order");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         local function note(n) return function() _G.log[#_G.log+1] = n end end\n\
         nx.autocmd.create('BufReadPost', { callback = function()\n\
         \x20 _G.log[#_G.log+1] = 'read:start'\n\
         \x20 return nx.promise.delay(5):next(note('read:done'))\n\
         end })\n\
         nx.autocmd.create('FileType', { callback = note('filetype') })\n\
         nx.autocmd.create('BufEnter', { callback = note('bufenter') })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.log[#_G.log] == 'bufenter'").await,
        "the chain completed; log so far {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, ',')").await
    );
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        log.as_str(),
        Some("read:start,read:done,filetype,bufenter"),
        "the async BufReadPost handler fully settled before FileType, and BufEnter came last"
    );
}

#[tokio::test]
async fn bufwinenter_is_sequenced_after_the_chains_gates_too() {
    // The async twin of `deferred_open_fires_read_events_in_order_before_bufwinenter`,
    // which pins vim's order — BufReadPost -> FileType -> BufEnter -> BufWinEnter — on
    // the synchronous path. An async read handler must not reorder it: the window walk
    // runs in the same pass that parked the chain, so firing BufWinEnter there put it
    // SECOND ("read:start,bufwinenter,read:done,filetype,bufenter") — ahead of the very
    // events the chain exists to order, and against a buffer whose setup is incomplete.
    let dir = temp_dir("au_chain_bwe_order");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         local function note(n) return function() _G.log[#_G.log+1] = n end end\n\
         nx.autocmd.create('BufReadPost', { callback = function()\n\
         \x20 _G.log[#_G.log+1] = 'read:start'\n\
         \x20 return nx.promise.delay(5):next(note('read:done'))\n\
         end })\n\
         nx.autocmd.create('FileType', { callback = note('filetype') })\n\
         nx.autocmd.create('BufEnter', { callback = note('bufenter') })\n\
         nx.autocmd.create('BufWinEnter', { callback = note('bufwinenter') })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.log[#_G.log] == 'bufwinenter'").await,
        "the chain completed; log so far {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, ',')").await
    );
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        log.as_str(),
        Some("read:start,read:done,filetype,bufenter,bufwinenter"),
        "BufWinEnter waited behind the chain instead of jumping it, and stayed last"
    );
    // And exactly once — deferring must not leave the walk's baseline thinking the
    // buffer is still unshown, which would fire it again on the next diff. So force
    // that diff, after waiting out anything still in flight.
    settle_ms(&rpc, 60).await;
    feed(&rpc, "jk");
    barrier(&rpc).await;
    let n = exec_lua(
        &rpc,
        "local n = 0 for _, e in ipairs(_G.log) do if e == 'bufwinenter' then n = n + 1 end end return n",
    )
    .await;
    assert_eq!(n.as_i64(), Some(1), "BufWinEnter fired exactly once");
}

/// Config for the two deferred-tail tests below: once armed, the read chain parks on a
/// `BufReadPost` promise the test resolves by hand (`release_read`), so "while the chain is
/// parked" is a state the test *holds* rather than a timer it races — a `delay()` here
/// settles under load before the driving commands land. Armed by the test rather than from
/// the start, so the startup file's own read (which may beat the config that would gate it)
/// can't be the one parked. `BufWinEnter` logs the window it ran in.
const PARKED_READ: &str = "_G.log = {}\n\
     _G.gate = false\n\
     nx.autocmd.create('BufReadPost', { callback = function()\n\
     \x20 if not _G.gate then return end\n\
     \x20 return nx.promise.new(function(resolve) _G.release = resolve end)\n\
     end })\n\
     nx.on('BufWinEnter', function() _G.log[#_G.log+1] = nx.win.current() end)\n";

/// Arm [`PARKED_READ`]'s gate and drop whatever the startup sequence logged, so the next
/// read parks and the log describes only what the test drives.
async fn arm_parked_read(rpc: &Rpc) {
    exec_lua(rpc, "_G.gate = true _G.log = {} return 1").await;
}

/// Release [`PARKED_READ`]'s parked read and wait for the chain to complete (its deferred
/// `BufWinEnter` tail lands a tick later, off the promise drain).
async fn release_read(rpc: &Rpc) -> bool {
    exec_lua(rpc, "_G.release() return 1").await;
    poll_true(rpc, "return #_G.log > 0").await
}

#[tokio::test]
async fn every_window_that_displayed_while_the_chain_was_parked_fires() {
    // The deferred tail carries the window the fire is *about*, and a parked chain can
    // collect more than one: while an async `BufReadPost` handler is still settling, a
    // `:vsplit` gives a *second* window the same buffer. Both windows displayed it, so
    // both owe a `BufWinEnter` — that is the whole per-window rule, and per-window setup
    // skipped for one of them is exactly the bug the per-window model exists to fix.
    //
    // Regression: the tail was a single `Option<WindowId>`, so the second display
    // overwrote the first and window 1's fire was dropped outright — nothing re-detects
    // it, since by then the baseline already records the buffer as shown there.
    let dir = temp_dir("au_chain_bwe_multi");
    let file = dir.join("main.rs");
    let other = dir.join("other.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    std::fs::write(&other, "fn other() {}\n").expect("write second source file");
    let (rpc, _incoming) =
        start_with_file_and_config(&dir, file.to_str().unwrap(), PARKED_READ).await;
    // Read a file with the gate armed: its chain parks, so its own `BufWinEnter` for
    // window 1 is deferred. Then split a second window onto the same file before it
    // settles, so both displays are pending on the one chain.
    arm_parked_read(&rpc).await;
    feed(&rpc, &format!(":edit {}<CR>", other.display()));
    barrier(&rpc).await;
    feed(&rpc, &format!(":vsplit {}<CR>", other.display()));
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return table.concat(_G.log, ',')")
            .await
            .as_str(),
        Some(""),
        "the read is still parked, so both displays are pending on the one chain"
    );
    assert!(
        release_read(&rpc).await,
        "the chain completed once the read was released; got {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, ',')").await
    );
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        log.as_str(),
        Some("1,2"),
        "one fire per window that displayed, in the order they displayed"
    );
}

#[tokio::test]
async fn a_window_closed_while_the_chain_was_parked_does_not_fire() {
    // The other edge of the deferred tail: an async read handler runs for as long as it
    // takes, and the window that displayed can be gone by the time the chain completes.
    // neovim fires `BufWinEnter` from *inside* the window, so a window that no longer
    // shows the buffer has no display left to announce — and firing anyway would install a
    // dead window id as "current", pointing a handler's per-window setup at nothing.
    let dir = temp_dir("au_chain_bwe_closed");
    let file = dir.join("main.rs");
    let other = dir.join("other.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    std::fs::write(&other, "fn other() {}\n").expect("write second source file");
    let (rpc, _incoming) =
        start_with_file_and_config(&dir, file.to_str().unwrap(), PARKED_READ).await;
    // Two windows displayed it while the chain was parked; close the second before the
    // read settles, so only one of the two deferred fires still has a window.
    arm_parked_read(&rpc).await;
    feed(&rpc, &format!(":edit {}<CR>", other.display()));
    barrier(&rpc).await;
    feed(&rpc, &format!(":vsplit {}<CR>", other.display()));
    barrier(&rpc).await;
    feed(&rpc, ":quit<CR>");
    barrier(&rpc).await;
    assert_eq!(
        exec_lua(&rpc, "return table.concat(_G.log, ',')")
            .await
            .as_str(),
        Some(""),
        "the read is still parked, so nothing has fired yet"
    );
    assert!(
        release_read(&rpc).await,
        "the chain completed once the read was released"
    );
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        log.as_str(),
        Some("1"),
        "only the window still displaying the buffer fired"
    );
}

#[tokio::test]
async fn a_chain_with_no_async_handler_completes_within_one_tick() {
    // The mandatory fast path. Gating must cost nothing when nothing is async — which
    // is nearly every config. If the chain ever needed a tick to advance, every plain
    // file open would get slower.
    let dir = temp_dir("au_chain_sync");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         nx.autocmd.create('BufReadPost', { callback = function() _G.log[#_G.log+1] = 'read' end })\n\
         nx.autocmd.create('FileType', { callback = function() _G.log[#_G.log+1] = 'ft' end })\n\
         nx.autocmd.create('BufEnter', { callback = function() _G.log[#_G.log+1] = 'enter' end })\n",
    )
    .await;
    // The FIRST thing asked of the server after startup already sees a complete chain —
    // no polling, no extra tick.
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        log.as_str(),
        Some("read,ft,enter"),
        "a fully synchronous chain converged before the first request was answered"
    );
    let msg = message(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert!(!msg.contains("E5108"), "and raised nothing, got {msg:?}");
}

#[tokio::test]
async fn a_hung_bufreadpost_handler_does_not_wedge_the_chain() {
    // Liveness, and why the settle budget stops being merely diagnostic once the chain
    // gates on it: a handler that NEVER resolves must not leave the buffer permanently
    // half-announced with no FileType and no BufEnter. The budget expiry advances it.
    let dir = temp_dir("au_chain_hung");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.log = {}\n\
         nx.autocmd.create('BufReadPost', { timeout = 30, callback = function()\n\
         \x20 return nx.promise.new(function() end)\n\
         end })\n\
         nx.autocmd.create('FileType', { callback = function() _G.log[#_G.log+1] = 'ft' end })\n\
         nx.autocmd.create('BufEnter', { callback = function() _G.log[#_G.log+1] = 'enter' end })\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return #_G.log == 2").await,
        "the chain advanced past the hung handler; log {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, ',')").await
    );
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')").await;
    assert_eq!(
        log.as_str(),
        Some("ft,enter"),
        "FileType and BufEnter still fired, in order, despite a handler that never settles"
    );
}

#[tokio::test]
async fn deleting_a_buffer_mid_chain_does_not_panic_or_orphan_the_gate() {
    // A handler can do anything, including destroy the buffer being announced. The
    // chain must unwind rather than fire BufEnter for a dead buffer or leave a gate
    // parked forever.
    let dir = temp_dir("au_chain_bwipe");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.done = false\n\
         _G.after = {}\n\
         nx.autocmd.create('BufReadPost', { callback = function(a)\n\
         \x20 _G.wiped = a.buf\n\
         \x20 return nx.promise.delay(5):next(function()\n\
         \x20   nx.cmd('enew')\n\
         \x20   pcall(nx.cmd, 'bwipeout! ' .. a.buf)\n\
         \x20   _G.done = true\n\
         \x20 end)\n\
         end })\n\
         for _, ev in ipairs({ 'FileType', 'BufEnter', 'BufWinEnter' }) do\n\
         \x20 nx.autocmd.create(ev, { callback = function(x)\n\
         \x20   if _G.done and x.buf == _G.wiped then\n\
         \x20     _G.after[#_G.after+1] = x.event\n\
         \x20   end\n\
         \x20 end })\n\
         end\n",
    )
    .await;
    assert!(
        poll_true(&rpc, "return _G.done == true").await,
        "the handler ran and wiped the buffer"
    );
    // The server is still alive and answering — no panic, no wedged gate.
    let alive = exec_lua(&rpc, "return 1 + 1").await;
    assert_eq!(
        alive.as_i64(),
        Some(2),
        "the server survived the mid-chain wipe"
    );
    // And the chain was abandoned rather than driven on over a dead buffer: nothing it
    // had left to fire may reach the wiped bufnr. (The state it held — the chain entry
    // and its gate mapping — is dropped with it, so a handler that never settles cannot
    // leave either map populated for a buffer that no longer exists.)
    // The wait is real: the gate signal for the abandoned chain is still in flight when
    // `_G.done` becomes visible, so sampling immediately would assert nothing.
    settle_ms(&rpc, 60).await;
    feed(&rpc, "jk");
    barrier(&rpc).await;
    let after = exec_lua(&rpc, "return table.concat(_G.after, ',')").await;
    assert_eq!(
        after.as_str(),
        Some(""),
        "no chain stage fired for the wiped buffer"
    );
}

#[tokio::test]
async fn a_rejecting_async_read_handler_surfaces_its_rejection() {
    // The gated read chain must not be a place where errors go to die. A `BufReadPost`
    // handler whose promise rejects (a failed fetch, a throw in a `:next`) has to
    // surface exactly as it does on the ungated path — `all_settled` swallowing the
    // rejection for the *chain's* purposes (a broken handler must not wedge the buffer)
    // is a liveness decision, not a licence to hide the error. Before the fix this
    // printed nothing at all: `nx._fire_gated` collected the promise without the
    // `track_au_promise` `:catch` that `nx._fire` attaches, and `all_settled`'s own
    // rejection handler marked it handled, so even the generic unhandled-rejection
    // reporter stayed quiet.
    let dir = temp_dir("au_chain_reject");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, _incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        "_G.notified = {}\n\
         local orig = nx.notify\n\
         nx.notify = function(msg, level)\n\
         \x20 _G.notified[#_G.notified+1] = tostring(msg)\n\
         \x20 return orig(msg, level)\n\
         end\n\
         _G.log = {}\n\
         nx.autocmd.create('BufReadPost', { callback = function()\n\
         \x20 return nx.promise.delay(5):next(function() error('read blew up') end)\n\
         end })\n\
         nx.autocmd.create('FileType', { callback = function() _G.log[#_G.log+1] = 'ft' end })\n",
    )
    .await;
    assert!(
        poll_true(
            &rpc,
            "return table.concat(_G.notified, '\\n'):find('read blew up', 1, true) ~= nil"
        )
        .await,
        "a rejecting async BufReadPost handler must surface its rejection; got {:?}",
        exec_lua(&rpc, "return table.concat(_G.notified, '\\n')").await
    );
    let msg = exec_lua(&rpc, "return table.concat(_G.notified, '\\n')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        msg.contains("BufReadPost"),
        "and name the event that raised it, got {msg:?}"
    );
    // And the chain still advanced — surfacing the error must not change the liveness
    // guarantee that a broken handler cannot leave the buffer half-announced.
    assert!(
        poll_true(&rpc, "return #_G.log == 1").await,
        "the chain still reached FileType past the rejecting handler"
    );
}

#[tokio::test]
async fn re_reading_a_file_mid_chain_abandons_the_previous_chain() {
    // `:e!` on a buffer whose async `BufReadPost` handler is still in flight starts a
    // *fresh* chain for the same buffer. The old chain's gate must be abandoned with
    // it: the phase already does this when the buffer is deleted, and a re-read is the
    // same situation — the chain describes a read that no longer exists.
    //
    // Before the fix the stale gate stayed mapped to the buffer, so the FIRST read's
    // handler settling un-parked the SECOND read's chain and drove it on — firing
    // `FileType` while the second `BufReadPost` handler was still running, which is
    // precisely the ordering the chain exists to guarantee.
    let dir = temp_dir("au_chain_reread");
    let file = dir.join("main.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write source file");
    let (rpc, mut incoming) = start_with_file_and_config(
        &dir,
        file.to_str().unwrap(),
        // The first read parks for a long time so the re-read below lands underneath it;
        // the second settles fast, so the ordering assertion is about which gate released
        // `FileType`, not about who finished first.
        "_G.log = {}\n\
         _G.reads = 0\n\
         nx.autocmd.create('BufReadPost', { timeout = 30000, callback = function()\n\
         \x20 _G.reads = _G.reads + 1\n\
         \x20 local n = _G.reads\n\
         \x20 _G.log[#_G.log+1] = 'read' .. n .. ':start'\n\
         \x20 return nx.promise.delay(n == 1 and 1500 or 3000):next(function()\n\
         \x20   _G.log[#_G.log+1] = 'read' .. n .. ':done'\n\
         \x20 end)\n\
         end })\n\
         nx.autocmd.create('FileType', { callback = function() _G.log[#_G.log+1] = 'ft' end })\n\
         nx.autocmd.create('BufEnter', { callback = function() _G.log[#_G.log+1] = 'enter' end })\n\
         nx.autocmd.create('BufWinEnter', { callback = function() _G.log[#_G.log+1] = 'winenter' end })\n",
    )
    .await;
    // The first read is parked on a 1.5s handler; re-read now, from underneath it. The
    // second parks for longer, so the first handler settles while the SECOND chain is
    // the one in flight — the window in which a stale gate does its damage.
    assert!(
        poll_true(&rpc, "return _G.reads == 1").await,
        "the first read's handler started"
    );
    redraw_after(&rpc, &mut incoming, &format!(":e! {}<CR>", file.display())).await;
    assert!(
        poll_true(&rpc, "return _G.reads == 2").await,
        "the re-read started a second chain; log {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, ',')").await
    );
    // If the abandoned chain's gate is still live, the first handler settling un-parks
    // the SECOND chain and drives it on — firing `FileType` while the second read's
    // handler is still running.
    assert!(
        poll_true(
            &rpc,
            "return table.concat(_G.log, ','):find('ft', 1, true) ~= nil"
        )
        .await,
        "FileType fired; log {:?}",
        exec_lua(&rpc, "return table.concat(_G.log, ',')").await
    );
    settle_ms(&rpc, 100).await;
    let log = exec_lua(&rpc, "return table.concat(_G.log, ',')")
        .await
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        log, "read1:start,read2:start,read1:done,read2:done,ft,enter,winenter",
        "FileType waits for the SECOND read's handler — the abandoned first chain's \
         still-pending gate neither releases it early nor holds it — and the tail the \
         first chain had parked (this buffer's one entry, its first display) carries \
         over to the new chain rather than being dropped with it: nothing re-detects \
         either, since the buffer is already current and already in the displayed baseline"
    );
}

#[tokio::test]
async fn nx_on_takes_a_bare_handler_with_no_options_table() {
    // `nx.on(event, fn)` — the two-argument spelling a config reaches for when there
    // is nothing to configure. It has to work, because the failure mode when it does
    // not is disproportionate: the handler lands in `opts`, the registration raises
    // `attempt to index a function value` from inside `nxvim:prelude/autocmd`, and the
    // rest of the config file never runs. One wrong argument count silently costs
    // every line after it, and the message names the prelude rather than the caller.
    let dir = temp_dir("au_on_bare");
    let (rpc, _incoming) = start_with_config(
        &dir,
        "nx.on('User', function(ev) _G.saw = tostring(ev.match) end)\n\
         _G.reached_end = true\n",
    )
    .await;
    assert_eq!(
        exec_lua(&rpc, "return tostring(_G.reached_end)")
            .await
            .as_str(),
        Some("true"),
        "the config ran past the registration"
    );
    exec_lua(&rpc, "nx.autocmd.exec('User', { pattern = 'Bare' })").await;
    assert_eq!(
        exec_lua(&rpc, "return tostring(_G.saw)").await.as_str(),
        Some("Bare"),
        "and the bare handler fires with the event args"
    );
    // The shift must not cost the caller its registration site: `site` is what turns a
    // slow-handler warning into a file:line, and it is captured by walking past the
    // forwarding prelude frames — one more forward here would blame the prelude.
    let site = exec_lua(
        &rpc,
        "return tostring(nx.autocmd.get({ event = 'User' })[1].site)",
    )
    .await;
    let site = site.as_str().unwrap_or("<nil>");
    assert!(
        site.ends_with(":1") && !site.contains("nxvim:prelude/"),
        "the bare form still reports the caller's line, got {site:?}"
    );
}

#[tokio::test]
async fn autocmd_create_names_a_non_table_opts_instead_of_dying_on_index() {
    // The same mistake against the neovim-shaped signature, where `opts` really is
    // always a table. It cannot be silently reinterpreted here, so it has to be named:
    // `attempt to index a function value` blames the prelude for the caller's typo and
    // says nothing about the fix.
    let dir = temp_dir("au_create_badopts");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    let err = exec_lua(
        &rpc,
        "local ok, err = pcall(nx.autocmd.create, 'User', function() end)\n\
         return tostring(ok) .. '|' .. tostring(err)",
    )
    .await;
    let err = err.as_str().unwrap_or("<nil>");
    assert!(err.starts_with("false|"), "it raises, got {err:?}");
    assert!(
        err.contains("opts must be a table, got function") && err.contains("nx.on(event, fn)"),
        "the error names the argument and the spelling that takes a bare handler, got {err:?}"
    );
}
