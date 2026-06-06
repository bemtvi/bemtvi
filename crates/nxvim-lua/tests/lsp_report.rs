//! Phase 1 of docs/plans/2026-06-05-lsp-completion.md: a config that errors at load, or a
//! server whose cmd can't be spawned, is no longer swallowed (degraded to `{}` /
//! a bare `return`). It is recorded — `vim._lsp_load_errors` / `vim._lsp_skipped`
//! — and enumerated by `vim.lsp._report()`, so no LSP failure stays silent.

use nxvim_lua::LuaRuntime;
use std::fs;
use std::path::PathBuf;

// A throwaway runtimepath holding two deliberately-broken `lsp/<name>.lua`
// configs: one that hits a not-implemented symbol at load, one whose cmd is an
// empty (unspawnable) argv.
fn temp_rtp() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nxvim-lsp-report-{}", std::process::id()));
    let lsp = dir.join("lsp");
    fs::create_dir_all(&lsp).unwrap();
    // Hits a Phase-0 raise at load: it used to vanish into an empty config.
    fs::write(
        lsp.join("badload.lua"),
        "vim.uri_to_bufnr('file:///x')\nreturn { filetypes = { 'foo' } }\n",
    )
    .unwrap();
    // Resolves, but its cmd is not a spawnable argv: it used to skip silently.
    fs::write(
        lsp.join("noargv.lua"),
        "return { cmd = {}, filetypes = { 'foo' } }\n",
    )
    .unwrap();
    dir
}

#[test]
fn load_errors_and_skips_are_recorded_not_swallowed() {
    let dir = temp_rtp();
    let rt = LuaRuntime::new(vec![dir.clone()]).expect("runtime");
    // A current buffer of filetype `foo`, so enabling drives the configs through
    // the resolve + start path on the spot.
    rt.set_buf_snapshot(1, "/work/proj/a.foo", "foo").unwrap();
    rt.exec("vim.lsp.enable({ 'badload', 'noargv' })")
        .expect("enable");

    let report = rt
        .eval_to_value(
            r#"
local r = vim.lsp._report()

-- The load error is recorded and names the not-implemented gap it hit.
local le = vim._lsp_load_errors.badload
assert(le ~= nil, "badload should appear in vim._lsp_load_errors")
assert(le:find("not implemented", 1, true),
  "the load error should name the gap, got: " .. tostring(le))
assert(r.load_errors.badload == le, "_report should surface the load error")

-- The unspawnable server is recorded with a reason, not silently dropped.
local sk = vim._lsp_skipped.noargv
assert(sk ~= nil, "noargv should appear in vim._lsp_skipped")
assert(r.skipped.noargv == sk, "_report should surface the skip")

-- _report enumerates the enabled set and the Phase-0 not-impl hits.
assert(vim.tbl_contains(r.enabled, 'badload') and vim.tbl_contains(r.enabled, 'noargv'),
  "_report.enabled should list the enabled servers")
assert(vim.tbl_contains(r.notimpl_hits, 'vim.uri_to_bufnr'),
  "_report.notimpl_hits should include the gap badload hit")
return "ok"
"#,
        )
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");

    let _ = fs::remove_dir_all(&dir);
}
