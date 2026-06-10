//! Behavior tests for the `vim.*` surface a real plugin needs to *function* —
//! the APIs which-key.nvim drives: the keystroke observer (`vim.on_key`), the
//! keymap readers (`nvim_get_keymap` / `nvim_buf_get_keymap` / `vim.fn.maparg`),
//! the typeahead pair (`nvim_replace_termcodes` + `nvim_feedkeys`), scratch
//! buffers (`nvim_create_buf`), the blocking key read (`vim.fn.getcharstr`), and
//! the predefined-variable / global-option tables (`vim.v` / `vim.go`).
//!
//! Black-box like the rest: every test starts a real server, sources an
//! `init.lua`, drives it over the same msgpack-RPC a UI uses, and asserts on
//! observable Lua / editor state (`nvim_exec_lua` return values, buffer lines).
//! The `start_with_config` / `feed` / `exec_lua` helpers are copied from the
//! established pattern (integration-test files don't share a module).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, buf_lines, exec_lua, feed, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a server on its own thread, sourcing `init_lua` from a throwaway config
/// dir (also the runtimepath), and return a connected client.
async fn start_with_config(
    dir: &std::path::Path,
    init_lua: &str,
) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

fn as_str(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// A `Value::Array` of strings as a `Vec<String>` (for asserting `vim.split` /
/// `nr2char` results).
fn as_strs(v: &Value) -> Vec<String> {
    match v {
        Value::Array(items) => items.iter().map(as_str).collect(),
        _ => Vec::new(),
    }
}

/// A `Value::Array` of integers as a `Vec<i64>` (for asserting `str2list`).
fn as_ints(v: &Value) -> Vec<i64> {
    match v {
        Value::Array(items) => items.iter().filter_map(Value::as_i64).collect(),
        _ => Vec::new(),
    }
}

// ===================== vim.split / str2list / nr2char =======================
// The key-parsing surface which-key.nvim runs every keymap through: `Util.keys`
// round-trips an lhs via `str2list`/`nr2char`, and `Mappings.add` splits a
// multi-char mode string with `vim.split(modes, "")`. A regression here cost the
// plugin its whole load path (a `setup()` that hung, then errored).

#[tokio::test]
async fn vim_split_empty_separator_splits_into_characters() {
    let dir = temp_dir("split");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // An empty separator splits into individual characters, matching neovim —
    // with no leading/trailing empty segment. This *used to hang*: `string.find`
    // returns a zero-width match for "", so the split loop never advanced `pos`.
    // (which-key calls `vim.split("nxso", "")` to expand a mode string.)
    assert_eq!(
        as_strs(&exec_lua(&rpc, "return vim.split('nxso', '')").await),
        vec!["n", "x", "s", "o"],
    );
    // Empty input with empty separator is the empty list (not `{ "" }`).
    assert_eq!(
        exec_lua(&rpc, "return #vim.split('', '')").await,
        Value::from(0u64),
    );
    // A normal (non-empty) separator is unaffected.
    assert_eq!(
        as_strs(&exec_lua(&rpc, "return vim.split('a,b,c', ',')").await),
        vec!["a", "b", "c"],
    );
}

#[tokio::test]
async fn str2list_and_nr2char_round_trip() {
    let dir = temp_dir("str2list");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // str2list yields the codepoint of each character.
    assert_eq!(
        as_ints(&exec_lua(&rpc, "return vim.fn.str2list('AB')").await),
        vec![65, 66],
    );
    // nr2char is its inverse, including a multibyte codepoint (😀 = U+1F600).
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.nr2char(65)").await),
        "A",
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.fn.nr2char(0x1F600) == '\\240\\159\\152\\128'",
        )
        .await,
        Value::Boolean(true),
    );
    // Full round-trip over a multibyte string: str2list -> nr2char rebuilds it.
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "local out = {}\n\
                 for _, cp in ipairs(vim.fn.str2list('aé好')) do out[#out+1] = vim.fn.nr2char(cp) end\n\
                 return table.concat(out)",
            )
            .await
        ),
        "aé好",
    );
}

#[tokio::test]
async fn missing_vim_fn_fails_loud_with_its_name() {
    let dir = temp_dir("notimpl_fn");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // A `vim.fn.<unknown>` is callable (neovim-faithful: `if vim.fn.foo then` is
    // truthy), but *calling* it raises a named error — not the bare "attempt to
    // call a nil value" a missing field would give, which `nvim_exec_lua` would
    // otherwise swallow to the message line.
    assert_eq!(
        as_str(&exec_lua(&rpc, "return type(vim.fn.totally_made_up_fn)").await),
        "function",
    );
    let err = as_str(
        &exec_lua(
            &rpc,
            "local ok, e = pcall(vim.fn.totally_made_up_fn, 1, 2); return tostring(e)",
        )
        .await,
    );
    assert!(
        err.contains("vim.fn.totally_made_up_fn"),
        "error should name the missing function, got: {err:?}"
    );
    // It is also recorded in the gap registry for a future :checkhealth.
    assert_eq!(
        exec_lua(
            &rpc,
            "pcall(vim.fn.another_missing_one); return vim._notimpl_hits['vim.fn.another_missing_one'] == true",
        )
        .await,
        Value::Boolean(true),
    );
}

// =================== window view / screen position ==========================
// The popup-placement surface which-key reads to draw and scroll its float:
// winsaveview/winrestview (its scroll) and screenrow/screencol (cursor-overlap).

#[tokio::test]
async fn winsaveview_winrestview_round_trip_the_scroll() {
    let dir = temp_dir("winview");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // Fill the buffer past one screen so a non-1 topline is valid (the harness
    // window is 24 rows).
    exec_lua(
        &rpc,
        "local lines = {}\n\
         for i = 1, 60 do lines[i] = 'line ' .. i end\n\
         vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)",
    )
    .await;

    // A fresh view: cursor at the top, no scroll.
    assert_eq!(
        exec_lua(
            &rpc,
            "local v = vim.fn.winsaveview(); return { v.lnum, v.col, v.topline, v.leftcol }",
        )
        .await,
        Value::Array(vec![
            Value::from(1u64),
            Value::from(0u64),
            Value::from(1u64),
            Value::from(0u64),
        ]),
    );

    // winrestview scrolls to a given topline; winsaveview reads it back.
    assert_eq!(
        exec_lua(
            &rpc,
            "vim.fn.winrestview({ topline = 10 }); return vim.fn.winsaveview().topline",
        )
        .await,
        Value::from(10u64),
    );
}

#[tokio::test]
async fn screenrow_and_screencol_track_the_cursor() {
    let dir = temp_dir("screenpos");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    exec_lua(
        &rpc,
        "vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'hello', 'world', 'third' })",
    )
    .await;

    // Cursor at the top-left: row 1, and column 1 past the window's number gutter
    // (so >= 1; nxvim shows a gutter by default). Read the baseline, then assert
    // movement is reflected — robust to the exact gutter width.
    let base = as_ints(&exec_lua(&rpc, "return { vim.fn.screenrow(), vim.fn.screencol() }").await);
    assert_eq!(base[0], 1, "cursor starts on screen row 1");
    assert!(
        base[1] >= 1,
        "screencol is 1-based and past the gutter: {base:?}"
    );

    // Move down two lines and right two columns; the screen position follows.
    feed(&rpc, "jjll");
    assert_eq!(
        as_ints(&exec_lua(&rpc, "return { vim.fn.screenrow(), vim.fn.screencol() }").await),
        vec![base[0] + 2, base[1] + 2],
    );
}

// ============================ vim.go / vim.v ================================

#[tokio::test]
async fn vim_go_reads_and_writes_global_options() {
    let dir = temp_dir("go");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // A non-wired global option round-trips through the observable store.
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "vim.go.eventignore = 'all'; return vim.go.eventignore"
            )
            .await
        ),
        "all"
    );
    // A wired global option (ignorecase) set via vim.o is visible through vim.go —
    // both read the same core-backed mirror.
    assert_eq!(
        exec_lua(&rpc, "vim.o.ignorecase = true; return vim.go.ignorecase").await,
        Value::Boolean(true)
    );
}

#[tokio::test]
async fn vim_v_constants_and_vim_did_enter() {
    let dir = temp_dir("v_const");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // The boolean constants plugins compare against.
    assert_eq!(
        exec_lua(&rpc, "return vim.v['true']").await,
        Value::Boolean(true)
    );
    assert_eq!(
        exec_lua(&rpc, "return vim.v['false']").await,
        Value::Boolean(false)
    );
    // The startup VimEnter point has passed by the time a client can talk to us.
    assert_eq!(
        exec_lua(&rpc, "return vim.v.vim_did_enter").await,
        Value::from(1u64)
    );
}

#[tokio::test]
async fn vim_v_count_reflects_the_count_typed_before_a_mapping() {
    // A normal-mode mapping reads `v:count` while it fires; the count typed ahead
    // of the leader (`3<Space>c`) reaches the editor first, so the mapping sees 3.
    let dir = temp_dir("v_count");
    let init = r#"
        vim.g.mapleader = " "
        _G.seen = nil
        vim.keymap.set("n", "<leader>c", function()
          _G.seen = { count = vim.v.count, count1 = vim.v.count1 }
        end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, "3 c");
    assert_eq!(
        exec_lua(&rpc, "return _G.seen and _G.seen.count").await,
        Value::from(3u64)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.seen and _G.seen.count1").await,
        Value::from(3u64)
    );

    // With no count typed, v:count is 0 but v:count1 is 1 (vim's contract).
    feed(&rpc, " c");
    assert_eq!(
        exec_lua(&rpc, "return _G.seen.count").await,
        Value::from(0u64)
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.seen.count1").await,
        Value::from(1u64)
    );
}

// ===================== nvim_get_keymap / maparg ============================

#[tokio::test]
async fn maparg_and_get_keymap_read_back_registered_maps() {
    let dir = temp_dir("maparg");
    let init = r#"
        vim.keymap.set("n", "gh", "ZZ", { desc = "save-quit" })
        vim.keymap.set("n", "<F5>", function() end, { desc = "run", silent = true })
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    // maparg(name, mode) returns the rhs string of a string map.
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.maparg('gh', 'n')").await),
        "ZZ"
    );
    // maparg(..., dict=true) returns the full dict, with desc/silent surfaced.
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.maparg('gh', 'n', false, true).desc").await),
        "save-quit"
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.fn.maparg('<F5>', 'n', false, true).silent"
        )
        .await,
        Value::from(1u64)
    );
    // A function RHS reports rhs="" with the callback carried out-of-band.
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.maparg('<F5>', 'n', false, true).rhs").await),
        ""
    );
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "return type(vim.fn.maparg('<F5>', 'n', false, true).callback)"
            )
            .await
        ),
        "function"
    );
    // An unmapped lhs: "" (string form) / {} (dict form).
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.maparg('zzz', 'n')").await),
        ""
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return next(vim.fn.maparg('zzz', 'n', false, true)) == nil"
        )
        .await,
        Value::Boolean(true)
    );
    // nvim_get_keymap lists the global normal-mode maps (the two set above).
    assert_eq!(
        exec_lua(&rpc, "return #vim.api.nvim_get_keymap('n')").await,
        Value::from(2u64)
    );
}

#[tokio::test]
async fn buf_get_keymap_separates_buffer_local_from_global() {
    let dir = temp_dir("bufmap");
    let init = r#"
        vim.keymap.set("n", "gh", "ZZ")              -- global
        vim.keymap.set("n", "gl", "ZZ", { buffer = 0 })  -- buffer-local (startup buf)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    // The global reader sees only the global map; the buffer reader only the local.
    assert_eq!(
        exec_lua(&rpc, "return #vim.api.nvim_get_keymap('n')").await,
        Value::from(1u64)
    );
    assert_eq!(
        exec_lua(&rpc, "return #vim.api.nvim_buf_get_keymap(0, 'n')").await,
        Value::from(1u64)
    );
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.api.nvim_buf_get_keymap(0, 'n')[1].lhs").await),
        "gl"
    );
}

// ===================== nvim_create_buf =====================================

#[tokio::test]
async fn create_buf_makes_a_real_windowless_buffer() {
    let dir = temp_dir("create_buf");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // The new buffer gets a fresh id (the startup buffer is 1).
    let id = exec_lua(
        &rpc,
        "_G.b = vim.api.nvim_create_buf(false, true); return _G.b",
    )
    .await;
    let handle = id.as_u64().expect("bufnr");
    assert!(handle >= 2, "expected a fresh buffer id, got {handle}");

    // Lines written through the new handle are readable back through it…
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "vim.api.nvim_buf_set_lines(_G.b, 0, -1, false, {'x', 'y'}); \
                 return table.concat(vim.api.nvim_buf_get_lines(_G.b, 0, -1, false), ',')",
            )
            .await
        ),
        "x,y"
    );
    // …and reach the real server-side buffer (proving it was actually created).
    assert_eq!(buf_lines(&rpc, handle).await, vec!["x", "y"]);
    // The current buffer is untouched — the scratch buffer has no window.
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_get_current_buf()").await,
        Value::from(1u64)
    );
}

// ============== nvim_replace_termcodes + nvim_feedkeys =====================

#[tokio::test]
async fn feedkeys_noremap_types_into_the_buffer() {
    let dir = temp_dir("feed_n");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // replace_termcodes + feedkeys with the 'n' (noremap) flag: the keys are
    // parsed and fed straight to the editor — `ihello<Esc>` inserts "hello".
    exec_lua(
        &rpc,
        "local k = vim.api.nvim_replace_termcodes('ihello<Esc>', true, true, true) \
         vim.api.nvim_feedkeys(k, 'n', false)",
    )
    .await;
    assert_eq!(buf_lines(&rpc, 0).await, vec!["hello"]);
}

#[tokio::test]
async fn feedkeys_remap_runs_through_mappings() {
    // A `m`-flag feed is itself remapped: feeding the lhs of a map fires it.
    let dir = temp_dir("feed_m");
    let init = r#"
        vim.keymap.set("n", "<F2>", "iremapped<Esc>")
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    exec_lua(
        &rpc,
        "vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes('<F2>', true, true, true), 'm', false)",
    )
    .await;
    assert_eq!(buf_lines(&rpc, 0).await, vec!["remapped"]);
}

// ===================== vim.on_key ==========================================

#[tokio::test]
async fn on_key_observes_every_keystroke() {
    let dir = temp_dir("on_key");
    let init = r#"
        _G.keys = {}
        vim.on_key(function(_raw, key)
          _G.keys[#_G.keys + 1] = key
        end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, "jk");
    assert_eq!(
        as_str(&exec_lua(&rpc, "return table.concat(_G.keys, ',')").await),
        "j,k"
    );

    // vim.on_key(nil) clears observers — later keys are no longer recorded.
    exec_lua(&rpc, "vim.on_key(nil)").await;
    feed(&rpc, "l");
    assert_eq!(
        as_str(&exec_lua(&rpc, "return table.concat(_G.keys, ',')").await),
        "j,k"
    );
}

#[tokio::test]
async fn on_key_reports_special_keys_in_notation() {
    let dir = temp_dir("on_key_sp");
    let init = r#"
        _G.last = nil
        vim.on_key(function(_raw, key) _G.last = key end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, "<Esc>");
    assert_eq!(as_str(&exec_lua(&rpc, "return _G.last").await), "<Esc>");
}

// ===================== vim.fn.getcharstr ===================================

#[tokio::test]
async fn getcharstr_blocks_in_a_keymap_and_resumes_on_the_next_key() {
    // The which-key mechanism in miniature: a keymap RHS calls getcharstr(), which
    // parks the pumped coroutine; the next key the server receives resumes it with
    // that key's notation — and is consumed (does not reach the editor).
    let dir = temp_dir("getchar");
    let init = r#"
        vim.g.mapleader = " "
        _G.got = nil
        vim.keymap.set("n", "<leader>g", function()
          _G.got = vim.fn.getcharstr()
        end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, " g"); // fires the map; getcharstr parks
                      // Not resumed yet — still waiting for a key.
    assert_eq!(
        exec_lua(&rpc, "return _G.got == nil").await,
        Value::Boolean(true)
    );

    feed(&rpc, "x"); // resumes the parked getcharstr with "x"
    assert_eq!(as_str(&exec_lua(&rpc, "return _G.got").await), "x");
    // The "x" was consumed by getcharstr, not typed into the buffer.
    assert_eq!(buf_lines(&rpc, 0).await, vec![""]);
}

#[tokio::test]
async fn getcharstr_resumes_on_a_special_key_in_notation() {
    let dir = temp_dir("getchar_sp");
    let init = r#"
        vim.g.mapleader = " "
        _G.got = nil
        vim.keymap.set("n", "<leader>g", function() _G.got = vim.fn.getcharstr() end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, " g");
    feed(&rpc, "<Esc>");
    assert_eq!(as_str(&exec_lua(&rpc, "return _G.got").await), "<Esc>");
}

#[tokio::test]
async fn getcharstr_peek_returns_empty_without_blocking() {
    // getcharstr(1) is the non-blocking peek which-key's M.safe uses; with no
    // typeahead exposed it returns "" immediately (and never parks).
    let dir = temp_dir("getchar_peek");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.getcharstr(1)").await),
        ""
    );
}

#[tokio::test]
async fn getcharstr_loop_walks_a_sequence_then_feeds_keys() {
    // The full which-key shape: a trigger keymap runs a getchar loop, accumulating
    // keys until it has a leaf, then nvim_feedkeys executes the resolved sequence.
    // Here the leaf "x" feeds `ihi<Esc>` (noremap) — proving park→resume→feed all
    // compose in one flow.
    let dir = temp_dir("getchar_loop");
    let init = r#"
        vim.g.mapleader = " "
        vim.keymap.set("n", "<leader>k", function()
          local acc = ""
          for _ = 1, 2 do
            acc = acc .. vim.fn.getcharstr()
          end
          if acc == "ab" then
            vim.api.nvim_feedkeys(
              vim.api.nvim_replace_termcodes("ihi<Esc>", true, true, true), "n", false)
          end
        end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, " k"); // fire the trigger; getchar loop parks on the 1st read
    feed(&rpc, "a"); // resumes; parks again on the 2nd read
    feed(&rpc, "b"); // resumes; loop ends, feeds `ihi<Esc>`
    assert_eq!(buf_lines(&rpc, 0).await, vec!["hi"]);
}

// ===================== vim.co_pcall (yieldable protected call) ==============
// Phase 2 of docs/plans/2026-06-09-edit-host-and-browser-lua.md: the opt-in
// yieldable `pcall` primitive. On PUC Lua 5.1 the global `pcall` is a C frame
// and `pcall(getcharstr)` raises "attempt to yield across a C-call boundary";
// `vim.co_pcall` runs the protected fn in its own coroutine and relays the yield
// through a pure-Lua path so the blocking read actually parks and resumes.

#[tokio::test]
async fn co_pcall_wraps_a_blocking_getcharstr_and_reads_the_key() {
    // The which-key live-popup shape made opt-in: a protected blocking read.
    // Proves the relay reads input — that the parked getcharstr resumes with the
    // next key and co_pcall returns (true, key) — not merely that it doesn't error.
    let dir = temp_dir("co_pcall");
    let init = r#"
        vim.g.mapleader = " "
        _G.ok, _G.got = nil, nil
        vim.keymap.set("n", "<leader>g", function()
          _G.ok, _G.got = vim.co_pcall(vim.fn.getcharstr)
        end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, " g"); // fires the map; the protected getcharstr parks
    assert_eq!(
        exec_lua(&rpc, "return _G.got == nil").await,
        Value::Boolean(true)
    );

    feed(&rpc, "x"); // resumes the parked read through the relay with "x"
    assert_eq!(exec_lua(&rpc, "return _G.ok").await, Value::Boolean(true));
    assert_eq!(as_str(&exec_lua(&rpc, "return _G.got").await), "x");
    // The "x" was consumed by the read, not typed into the buffer.
    assert_eq!(buf_lines(&rpc, 0).await, vec![""]);
}

#[tokio::test]
async fn co_pcall_relays_several_blocking_reads_in_a_loop() {
    // Several keystrokes through one protected call: a getchar loop inside
    // vim.co_pcall accumulates two keys, then feeds a resolved sequence — proving
    // the relay forwards each resume down to the inner coroutine, repeatedly.
    let dir = temp_dir("co_pcall_loop");
    let init = r#"
        vim.g.mapleader = " "
        vim.keymap.set("n", "<leader>k", function()
          vim.co_pcall(function()
            local acc = ""
            for _ = 1, 2 do acc = acc .. vim.fn.getcharstr() end
            if acc == "ab" then
              vim.api.nvim_feedkeys(
                vim.api.nvim_replace_termcodes("ihi<Esc>", true, true, true), "n", false)
            end
          end)
        end)
    "#;
    let (rpc, _incoming) = start_with_config(&dir, init).await;

    feed(&rpc, " k"); // fire the trigger; parks on the 1st protected read
    feed(&rpc, "a"); // resumes; parks again on the 2nd
    feed(&rpc, "b"); // resumes; loop ends, feeds `ihi<Esc>`
    assert_eq!(buf_lines(&rpc, 0).await, vec!["hi"]);
}

#[tokio::test]
async fn co_pcall_returns_false_and_the_error_on_a_throwing_fn() {
    // Without blocking, co_pcall is plain pcall: it catches an error and returns
    // (false, message) rather than propagating.
    let dir = temp_dir("co_pcall_err");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    assert_eq!(
        exec_lua(
            &rpc,
            "local ok = vim.co_pcall(function() error('boom') end) return ok"
        )
        .await,
        Value::Boolean(false)
    );
    // The error value carries a "chunk:line:" prefix; assert the tail we control.
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "local _, e = vim.co_pcall(function() error('boom') end)
                 return tostring(e):match('boom$')"
            )
            .await
        ),
        "boom"
    );
}

#[tokio::test]
async fn co_pcall_forwards_args_and_returns_on_success() {
    // Args reach the protected fn and its return values come back after the ok flag.
    let dir = temp_dir("co_pcall_ret");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    assert_eq!(
        exec_lua(
            &rpc,
            "local ok, a, b = vim.co_pcall(function(x, y) return x + y, x * y end, 3, 4)
             return ok and (a == 7) and (b == 12)"
        )
        .await,
        Value::Boolean(true)
    );
}

#[tokio::test]
async fn co_xpcall_runs_the_message_handler_on_error() {
    // co_xpcall routes the error through msgh and returns (false, msgh-result).
    // The error value carries a "chunk:line:" prefix, so msgh normalizes it to a
    // marker we can assert exactly.
    let dir = temp_dir("co_xpcall");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "local ok, r = vim.co_xpcall(function() error('boom') end,
                   function(e) return 'h:' .. tostring(e):match('boom$') end)
                 return (ok == false) and r or 'NOPE'",
            )
            .await
        ),
        "h:boom"
    );
}

#[tokio::test]
async fn co_wrap_reraises_errors_and_returns_results() {
    // co_wrap re-raises (no ok flag) and otherwise yields the results.
    let dir = temp_dir("co_wrap");
    let (rpc, _incoming) = start_with_config(&dir, "").await;
    // success path: results pass straight through.
    assert_eq!(
        exec_lua(
            &rpc,
            "local g = vim.co_wrap(function(n) return n * 2 end) return g(21)"
        )
        .await,
        Value::from(42),
    );
    // error path: the wrapped call raises, so a guarding pcall catches it.
    assert_eq!(
        exec_lua(
            &rpc,
            "local g = vim.co_wrap(function() error('boom') end)
             local ok = pcall(g) return ok"
        )
        .await,
        Value::Boolean(false),
    );
}

// ============================ which-key display surface =====================
// The APIs which-key.nvim drives to *render* its popup: highlight reads
// (nvim_get_hl), context callbacks (nvim_buf_call / nvim_win_call), scratch-buffer
// teardown (nvim_buf_delete), and the width/character builtins its grid layout
// needs (strdisplaywidth / strchars / strcharpart / strtrans / keytrans).

#[tokio::test]
async fn nvim_get_hl_reads_colors_and_attrs() {
    let dir = temp_dir("get_hl");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // Define a group in one chunk; the registry fold + mirror push land before the
    // next chunk reads it back.
    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'WhichKey', { fg = '#ff0000', bg = '#0000ff', bold = true })",
    )
    .await;
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'WhichKey' }).fg"
        )
        .await,
        Value::from(0xff0000u64)
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'WhichKey' }).bg"
        )
        .await,
        Value::from(0x0000ffu64)
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'WhichKey' }).bold"
        )
        .await,
        Value::Boolean(true)
    );
    // An unknown group is an empty table (not an error).
    assert_eq!(
        exec_lua(
            &rpc,
            "return next(vim.api.nvim_get_hl(0, { name = 'Nope' })) == nil"
        )
        .await,
        Value::Boolean(true)
    );
}

#[tokio::test]
async fn nvim_get_hl_follows_links_only_when_asked() {
    let dir = temp_dir("get_hl_link");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'Base', { fg = '#00ff00' })\n\
         vim.api.nvim_set_hl(0, 'Alias', { link = 'Base' })",
    )
    .await;
    // Default (link = true): a link group reports its target, not colors.
    assert_eq!(
        as_str(
            &exec_lua(
                &rpc,
                "return vim.api.nvim_get_hl(0, { name = 'Alias' }).link"
            )
            .await
        ),
        "Base"
    );
    // link = false: follow the chain to the concrete definition.
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'Alias', link = false }).fg"
        )
        .await,
        Value::from(0x00ff00u64)
    );
}

#[tokio::test]
async fn nvim_buf_call_runs_with_buffer_as_current() {
    let dir = temp_dir("buf_call");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // Inside nvim_buf_call the scratch buffer is current, so a read of its lines
    // resolves to it; the return value propagates out.
    let lines = exec_lua(
        &rpc,
        "local b = vim.api.nvim_create_buf(false, true)\n\
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'inside' })\n\
         return vim.api.nvim_buf_call(b, function()\n\
           assert(vim.api.nvim_get_current_buf() == b, 'current buf not swapped')\n\
           return vim.api.nvim_buf_get_lines(0, 0, -1, false)\n\
         end)",
    )
    .await;
    assert_eq!(lines, Value::Array(vec![Value::from("inside")]));
    // The current buffer is restored after the call returns.
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_get_current_buf()").await,
        Value::from(1u64)
    );
}

#[tokio::test]
async fn nvim_win_call_swaps_window_context() {
    let dir = temp_dir("win_call");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // nvim_win_call(0, fn) runs with the current window current and returns fn's
    // result; the window number read inside matches.
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_win_call(0, function() return vim.fn.winnr() end)"
        )
        .await,
        Value::from(1u64)
    );
}

#[tokio::test]
async fn nvim_buf_delete_removes_the_buffer() {
    let dir = temp_dir("buf_delete");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // Create + populate + delete a scratch buffer in one chunk; the write-through
    // drops it from the mirror immediately.
    let result = exec_lua(
        &rpc,
        "local b = vim.api.nvim_create_buf(false, true)\n\
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'doomed' })\n\
         local before = vim.api.nvim_buf_is_valid(b)\n\
         vim.api.nvim_buf_delete(b, { force = true })\n\
         return { before, vim.api.nvim_buf_is_valid(b), b }",
    )
    .await;
    let arr = match result {
        Value::Array(a) => a,
        other => panic!("expected array, got {other:?}"),
    };
    assert_eq!(arr[0], Value::Boolean(true), "valid before delete");
    assert_eq!(arr[1], Value::Boolean(false), "invalid after delete");
    // The core really removed it: an RPC read of the handle yields no lines.
    let handle = arr[2].as_u64().expect("buffer handle");
    assert_eq!(buf_lines(&rpc, handle).await, Vec::<String>::new());
}

#[tokio::test]
async fn string_width_and_character_builtins() {
    let dir = temp_dir("strwidth");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // strchars counts codepoints, not bytes ("héllo" = 5 chars, 6 bytes).
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.strchars('h\\195\\169llo')").await,
        Value::from(5u64)
    );
    // strdisplaywidth: a tab from column 0 expands to the next tabstop (8).
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.strdisplaywidth('\\t')").await,
        Value::from(8u64)
    );
    // A wide (CJK) character counts as two cells (U+4E2D = e4 b8 ad).
    assert_eq!(
        exec_lua(&rpc, "return vim.fn.strdisplaywidth('\\228\\184\\173')").await,
        Value::from(2u64)
    );
    // strcharpart slices by character index, not byte index.
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.strcharpart('h\\195\\169llo', 1, 2)").await),
        "\u{e9}l"
    );
    // strtrans renders control characters readably (^I for tab, ^[ for esc).
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.strtrans('a\\tb\\27c')").await),
        "a^Ib^[c"
    );
    // keytrans round-trips notation unchanged (nxvim's internal form IS notation).
    assert_eq!(
        as_str(&exec_lua(&rpc, "return vim.fn.keytrans('<C-w>')").await),
        "<C-w>"
    );
    // nvim_strwidth measures display cells (a wide char is two) without tab expansion.
    assert_eq!(
        exec_lua(&rpc, "return vim.api.nvim_strwidth('a\\228\\184\\173b')").await,
        Value::from(4u64)
    );
}

#[tokio::test]
async fn example_whichkey_popup_runs_end_to_end() {
    // The examples/whichkey-popup config is a self-contained mini-which-key built
    // entirely on the display surface added for which-key (nvim_get_hl,
    // nvim_buf_call, nvim_buf_delete, strdisplaywidth/strtrans, …). Running it
    // here proves the whole flow composes in the live editor.
    let dir = temp_dir("wk_example");
    let init = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/whichkey-popup/init.lua"
    ))
    .expect("read example init.lua");
    let (rpc, _incoming) = start_with_config(&dir, &init).await;

    // Press leader: the popup opens as scratch buffer 2 with its three hint rows
    // and parks on getcharstr.
    feed(&rpc, " ");
    assert_eq!(
        buf_lines(&rpc, 2).await.len(),
        3,
        "popup shows three hint rows"
    );

    // Pick "f": the popup tears down (window close + buffer delete) and the action
    // runs — proving create_buf, set_lines, extmarks, nvim_get_hl (+ link follow),
    // open_win, nvim_buf_call, getcharstr, win_close and nvim_buf_delete all
    // compose without error.
    feed(&rpc, "f");
    assert_eq!(
        buf_lines(&rpc, 2).await,
        Vec::<String>::new(),
        "popup buffer deleted"
    );
}

#[tokio::test]
async fn buf_call_blocks_context_dependent_mutations() {
    // A mutation that binds to "current" at drain time (an ex-command, feedkeys)
    // would run against the real current buffer, not the one passed to
    // nvim_buf_call. nxvim can't retarget it, so it must raise rather than silently
    // mutate the wrong buffer.
    let dir = temp_dir("buf_call_lock");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // vim.cmd inside a differing-context buf_call raises (pcall catches it).
    assert_eq!(
        exec_lua(
            &rpc,
            "local b = vim.api.nvim_create_buf(false, true)\n\
             local ok = pcall(function()\n\
               vim.api.nvim_buf_call(b, function() vim.cmd('normal! dd') end)\n\
             end)\n\
             return ok",
        )
        .await,
        Value::Boolean(false)
    );
    // nvim_feedkeys is blocked the same way.
    assert_eq!(
        exec_lua(
            &rpc,
            "local b = vim.api.nvim_create_buf(false, true)\n\
             return (pcall(function()\n\
               vim.api.nvim_buf_call(b, function() vim.api.nvim_feedkeys('x', 'n', false) end)\n\
             end))",
        )
        .await,
        Value::Boolean(false)
    );
    // The error message names the call and the offending operation.
    let msg = exec_lua(
        &rpc,
        "local b = vim.api.nvim_create_buf(false, true)\n\
         local ok, e = pcall(function()\n\
           vim.api.nvim_buf_call(b, function() vim.cmd('write') end)\n\
         end)\n\
         return e",
    )
    .await;
    let msg = as_str(&msg);
    assert!(msg.contains("nvim_buf_call"), "names the call: {msg}");
    assert!(msg.contains("ex-command"), "names the op: {msg}");
}

#[tokio::test]
async fn call_allows_reads_and_explicit_handle_writes() {
    // The lock blocks ONLY context-dependent mutations. Reads and explicit-handle
    // writes (which resolve the swapped mirror and queue a concrete handle) stay
    // allowed inside a differing-context call.
    let dir = temp_dir("call_allow");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // An explicit-handle write inside buf_call targets the right buffer and is not
    // blocked.
    let lines = exec_lua(
        &rpc,
        "local b = vim.api.nvim_create_buf(false, true)\n\
         vim.api.nvim_buf_call(b, function()\n\
           vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'written' })\n\
         end)\n\
         return vim.api.nvim_buf_get_lines(b, 0, -1, false)",
    )
    .await;
    assert_eq!(lines, Value::Array(vec![Value::from("written")]));

    // A same-context call (target == real current) does NOT lock — an ex-command
    // there is fine (it already targets the right buffer).
    assert_eq!(
        exec_lua(
            &rpc,
            "return (pcall(function()\n\
               vim.api.nvim_buf_call(0, function() vim.cmd('noh') end)\n\
             end))",
        )
        .await,
        Value::Boolean(true)
    );

    // The lock is cleared after the call returns: an ex-command afterward works.
    assert_eq!(
        exec_lua(
            &rpc,
            "local b = vim.api.nvim_create_buf(false, true)\n\
             pcall(function() vim.api.nvim_buf_call(b, function() vim.cmd('noh') end) end)\n\
             return (pcall(function() vim.cmd('noh') end))",
        )
        .await,
        Value::Boolean(true)
    );
}
