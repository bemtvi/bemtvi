//! Phase 0 of docs/lsp-completion-plan.md: every hollow `vim.*` stub now fails
//! loud. Instead of returning a fake/empty value that makes a broken server look
//! configured, a not-yet-implemented function raises `nxvim: not implemented:
//! <name>` and records `<name>` in `vim._notimpl_hits` (the running scoreboard a
//! future `:checkhealth` / `vim.lsp._report` enumerates).

use nxvim_lua::LuaRuntime;

#[test]
fn notimpl_stub_raises_with_its_name_and_records_the_hit() {
    let rt = LuaRuntime::new(vec![]).expect("runtime");
    let report = rt
        .eval_to_value(
            r#"
-- A representative hollow stub: it used to return a fabricated zero-cursor
-- position params table; now it raises naming itself.
local ok, err = pcall(vim.lsp.util.make_position_params)
assert(not ok, "expected vim.lsp.util.make_position_params to raise")
assert(err:find("nxvim: not implemented: vim.lsp.util.make_position_params", 1, true),
  "error should name the function, got: " .. tostring(err))
-- The hit is recorded so the gaps a real config triggers stay trackable.
assert(vim._notimpl_hits["vim.lsp.util.make_position_params"] == true,
  "the hit should be recorded in vim._notimpl_hits")

-- The faithful neighbours are NOT routed through the raise: vim.schedule runs
-- its callback (a safe "soon"), and nvim_get_current_buf returns the real
-- snapshot buffer rather than raising.
local ran = false
vim.schedule(function() ran = true end)
assert(ran, "vim.schedule should still run its callback inline")
assert(type(vim.api.nvim_get_current_buf()) == "number",
  "nvim_get_current_buf should stay real, not raise")
return "ok"
"#,
        )
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}
