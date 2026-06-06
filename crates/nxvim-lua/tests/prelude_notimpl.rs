//! Phase 0 of docs/plans/2026-06-05-lsp-completion.md: every hollow `vim.*` stub now fails
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
-- A representative hollow stub: returning 0 here would hand a handler the wrong
-- buffer, so it raises naming itself instead. (The `vim.lsp.util.*` neighbours
-- became real in Phase 7; uri_to_bufnr stays a gap — no Lua buffer-creating
-- registry yet — so it is the standing representative.)
local ok, err = pcall(vim.uri_to_bufnr, "file:///tmp/x")
assert(not ok, "expected vim.uri_to_bufnr to raise")
assert(err:find("nxvim: not implemented: vim.uri_to_bufnr", 1, true),
  "error should name the function, got: " .. tostring(err))
-- The hit is recorded so the gaps a real config triggers stay trackable.
assert(vim._notimpl_hits["vim.uri_to_bufnr"] == true,
  "the hit should be recorded in vim._notimpl_hits")

-- The faithful neighbours are NOT routed through the raise. vim.schedule no
-- longer runs inline either: with the async runtime it *defers* the callback to
-- the server's convergence (registered in vim._cb_fns, run later by id) rather
-- than nesting it in the caller. In this bare runtime — no server draining the
-- queue — that means the callback is registered but has NOT run, which is exactly
-- the proof it deferred.
local ran = false
vim.schedule(function() ran = true end)
assert(not ran, "vim.schedule should defer its callback, not run it inline")
assert(next(vim._cb_fns) ~= nil, "the scheduled callback should be registered for later")
assert(type(vim.api.nvim_get_current_buf()) == "number",
  "nvim_get_current_buf should stay real, not raise")

-- A uv timer handle's :is_active() / :is_closing() can't be answered faithfully
-- from Lua: a one-shot timer auto-expires inside the Rust actor with no callback
-- back to Lua, so any constant we return (the old `true` / `false`) is a lie about
-- the handle's real state. They raise via vim._notimpl instead of faking it.
local timer = vim.uv.new_timer()
local ok_active, err_active = pcall(timer.is_active, timer)
assert(not ok_active, "timer:is_active should raise, not return a canned bool")
assert(err_active:find("nxvim: not implemented: vim.uv.timer:is_active", 1, true),
  "is_active error should name itself, got: " .. tostring(err_active))
local ok_closing, err_closing = pcall(timer.is_closing, timer)
assert(not ok_closing, "timer:is_closing should raise, not return a canned bool")
assert(err_closing:find("nxvim: not implemented: vim.uv.timer:is_closing", 1, true),
  "is_closing error should name itself, got: " .. tostring(err_closing))
assert(vim._notimpl_hits["vim.uv.timer:is_active"] == true, "is_active hit recorded")
assert(vim._notimpl_hits["vim.uv.timer:is_closing"] == true, "is_closing hit recorded")
-- The faithful neighbours on the same handle stay real.
assert(timer:stop() == 0, "timer:stop should stay real, returning 0")

return "ok"
"#,
        )
        .expect("harness ran")
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(report, "ok");
}
