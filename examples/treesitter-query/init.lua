-- ~~~ nxvim vim.treesitter.query.set: customize what the engine paints ~~~
--
-- nxvim highlights from its in-core treesitter engine, which compiles ONE
-- `highlights.scm` per grammar. The query-resolution bridge (ADR 0001, #4) lets a
-- config/plugin customize that paint without nxvim reimplementing neovim's
-- query-merge rules: `vim.treesitter.query.set(lang, name, text)` runs through the
-- vendored `vim.treesitter.query` resolver (faithful), and the server pushes the
-- merged string to the engine, which recompiles and repaints. Lua resolves, the
-- engine executes.
--
-- Two flavors, both shown below:
--   * REPLACE — `set(lang, 'highlights', text)` with no modeline swaps the whole
--     query (exactly like neovim). Only what `text` captures gets painted.
--   * EXTEND  — a leading `;extends` line merges `text` ON TOP of the base query
--     (the engine's `highlights.scm`, found because its data dir is on the
--     runtimepath), so the base captures stay and yours are added.
--
-- PREREQUISITE: a Rust parser + queries installed in nxvim's data dir, laid out
-- like neovim's: `<data>/parser/rust.so` and `<data>/queries/rust/highlights.scm`.
-- Without them the buffer simply isn't highlighted (best-effort, no error).
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/treesitter-query \
--       cargo run -p nxvim -- examples/treesitter-query/sample.rs
--
-- On startup the buffer keeps its normal Rust highlighting PLUS every identifier
-- painted as @variable (the EXTEND below). Then toggle with the commands.

--------------------------------------------------------------------------------
-- On startup: extend the base rust highlights so identifiers also paint as
-- @variable. `;extends` keeps the base query (keywords, strings, …) and adds this.
--------------------------------------------------------------------------------
vim.treesitter.query.set("rust", "highlights", ";extends\n(identifier) @variable")

--------------------------------------------------------------------------------
-- :TSQueryReplace — REPLACE the whole query with an identifier-only one. After
--    this, keywords/strings are no longer painted — only identifiers (@variable).
--    Proves a no-modeline set replaces rather than extends.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSQueryReplace", function()
  vim.treesitter.query.set("rust", "highlights", "(identifier) @variable")
  print("rust highlights REPLACED with an identifier-only query")
end, {})

--------------------------------------------------------------------------------
-- :TSQueryExtend — re-apply the `;extends` overlay (base query + @variable).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSQueryExtend", function()
  vim.treesitter.query.set("rust", "highlights", ";extends\n(identifier) @variable")
  print("rust highlights EXTENDED: base query + identifiers as @variable")
end, {})

--------------------------------------------------------------------------------
-- :TSQueryReset — drop the override entirely; the engine reverts to the on-disk
--    `highlights.scm`. Pass nil as the text to clear.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSQueryReset", function()
  vim.treesitter.query.set("rust", "highlights", nil)
  print("rust highlights reset to the on-disk query")
end, {})

print("vim.treesitter.query.set demo — try :TSQueryReplace, :TSQueryExtend, :TSQueryReset")
