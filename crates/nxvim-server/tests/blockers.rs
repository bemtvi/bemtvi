//! Behavior tests for assorted `vim.*` surface: the keymap readers
//! (`nvim_get_keymap` / `nvim_buf_get_keymap`
//! / `vim.fn.maparg`), the typeahead pair (`nvim_replace_termcodes` +
//! `nvim_feedkeys`), scratch buffers (`nvim_create_buf`), and the
//! predefined-variable / global-option tables (`vim.v` / `vim.go`).
//!
//! Black-box like the rest: every test starts a real server, sources an
//! `init.lua`, drives it over the same msgpack-RPC a UI uses, and asserts on
//! observable Lua / editor state (`nvim_exec_lua` return values, buffer lines).
//! The `start_with_config` / `feed` / `exec_lua` helpers are copied from the
//! established pattern (integration-test files don't share a module).

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{attach, exec_lua, feed, spawn, temp_dir};
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
// The key-notation parsing surface keymap introspection runs every lhs through:
// round-tripping an lhs via `str2list`/`nr2char`, and splitting a multi-char mode
// string with `vim.split(modes, "")`. The empty-separator split once hung — a
// zero-width `string.find` match never advanced `pos` — so it is pinned below.

#[tokio::test]
async fn vim_split_empty_separator_splits_into_characters() {
    let dir = temp_dir("split");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // An empty separator splits into individual characters, matching neovim —
    // with no leading/trailing empty segment. This *used to hang*: `string.find`
    // returns a zero-width match for "", so the split loop never advanced `pos`.
    // (Expanding a multi-char mode string like "nxso" into its modes.)
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
            "pcall(vim.fn.another_missing_one); return nx._notimpl_hits['vim.fn.another_missing_one'] == true",
        )
        .await,
        Value::Boolean(true),
    );
}

// =================== window view / screen position ==========================
// The popup-placement surface a floating UI reads to draw and scroll its float:
// winsaveview/winrestview (the scroll) and screenrow/screencol (cursor-overlap).

#[tokio::test]
async fn screenrow_and_screencol_track_the_cursor() {
    let dir = temp_dir("screenpos");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    feed(&rpc, "ihello<CR>world<CR>third<Esc>gg");

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
    // nvim_get_keymap lists the global normal-mode maps — including the two set
    // above (alongside any shipped default maps, e.g. the `<leader>f*` pickers).
    assert_eq!(
        exec_lua(
            &rpc,
            "local seen = {}\n\
             for _, m in ipairs(vim.api.nvim_get_keymap('n')) do seen[m.lhs] = true end\n\
             return seen['gh'] and seen['<F5>'] or false"
        )
        .await,
        Value::Boolean(true)
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

    // The global reader sees the global map but not the buffer-local one; the
    // buffer reader sees only the local. (Shipped default maps, e.g. `<leader>f*`,
    // are global too, so assert separation by lhs rather than a global total.)
    assert_eq!(
        exec_lua(
            &rpc,
            "local seen = {}\n\
             for _, m in ipairs(vim.api.nvim_get_keymap('n')) do seen[m.lhs] = true end\n\
             return seen['gh'] and not seen['gl'] or false"
        )
        .await,
        Value::Boolean(true)
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

// ============== nvim_replace_termcodes + nvim_feedkeys =====================

// ========================= popup render surface ============================
// The APIs a floating popup UI drives to *render* itself: highlight reads
// (nvim_get_hl), context callbacks (nvim_buf_call / nvim_win_call), scratch-buffer
// teardown (nvim_buf_delete), and the width/character builtins a grid layout
// needs (strdisplaywidth / strchars / strcharpart / strtrans / keytrans).

#[tokio::test]
async fn nvim_get_hl_reads_colors_and_attrs() {
    let dir = temp_dir("get_hl");
    let (rpc, _incoming) = start_with_config(&dir, "").await;

    // Define a group in one chunk; the registry fold + mirror push land before the
    // next chunk reads it back.
    exec_lua(
        &rpc,
        "vim.api.nvim_set_hl(0, 'KeyHint', { fg = '#ff0000', bg = '#0000ff', bold = true })",
    )
    .await;
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'KeyHint' }).fg"
        )
        .await,
        Value::from(0xff0000u64)
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'KeyHint' }).bg"
        )
        .await,
        Value::from(0x0000ffu64)
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return vim.api.nvim_get_hl(0, { name = 'KeyHint' }).bold"
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
