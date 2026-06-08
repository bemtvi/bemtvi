-- ~~~ nxvim treesitter injections: one buffer, more than one language ~~~
--
-- An "injection" is a region of a buffer that belongs to another grammar: SQL in
-- a Rust string, Lua in `vim.cmd[[ … ]]`, a ```rust block in markdown. nxvim's
-- injection bridge (ADR 0001, #5) keeps the discipline of the other bridges: the
-- vendored `vim.treesitter.query` resolves the injection *query* (faithful), and
-- the in-core engine *executes* it — running the query over the live tree each
-- parse, parsing every injected region with its child grammar, and painting the
-- child's captures over the host's. All synchronous, all per-edit, no Lua on the
-- redraw hot path.
--
-- Most injections need no config at all: a host grammar that ships an
-- `injections.scm` (markdown, for one) injects automatically. This example shows
-- the *custom* path — `vim.treesitter.query.set(lang, 'injections', text)`, which
-- runs through the same resolver and is pushed to the engine like any other query.
--
-- PREREQUISITE: a Rust parser + queries in nxvim's data dir, laid out like
-- neovim's: `<data>/parser/rust.so` and `<data>/queries/rust/highlights.scm`.
-- Without it the buffer simply isn't highlighted (best-effort, no error).
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/treesitter-injections \
--       cargo run -p nxvim -- examples/treesitter-injections/sample.rs
--
-- On startup the Rust source *inside the string literal* is highlighted as Rust —
-- `fn` is a keyword, not part of one flat string. Toggle with the commands below.

--------------------------------------------------------------------------------
-- On startup: inject Rust into Rust string bodies. `@injection.content` marks the
-- region (the string's text); `#set! injection.language "rust"` names the grammar
-- to parse it with. The injected captures paint OVER the host's `@string`.
--------------------------------------------------------------------------------
local INJECT_RUST_IN_STRINGS =
  '((string_content) @injection.content (#set! injection.language "rust"))'

vim.treesitter.query.set("rust", "injections", INJECT_RUST_IN_STRINGS)

--------------------------------------------------------------------------------
-- :TSInjectOn — (re)enable the string→Rust injection.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSInjectOn", function()
  vim.treesitter.query.set("rust", "injections", INJECT_RUST_IN_STRINGS)
  print("rust string bodies are now injected as rust")
end, {})

--------------------------------------------------------------------------------
-- :TSInjectOff — drop the injection query; string bodies go back to flat @string.
--    Pass nil as the text to clear the override.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSInjectOff", function()
  vim.treesitter.query.set("rust", "injections", nil)
  print("rust injection cleared — string bodies are flat again")
end, {})

print("treesitter injections demo — toggle with :TSInjectOff / :TSInjectOn")
