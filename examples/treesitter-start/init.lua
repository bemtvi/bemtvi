-- ~~~ nxvim vim.treesitter.start / stop: turn highlighting on where the
--     extension table misses ~~~
--
-- nxvim highlights recognized file extensions automatically off its in-core
-- treesitter engine (the "highlight floor"). But a buffer the extension table
-- doesn't know — a `.txt` holding code, a `:enew` scratch buffer, a custom
-- filetype — gets nothing. `vim.treesitter.start(buf, lang)` is the bridge that
-- turns the *native* engine on for such a buffer at a language you choose; this
-- is exactly what an ftplugin or a `FileType` autocommand calls in a real config.
--
-- Unlike neovim, `start` does NOT spin up a Lua-side decoration-provider
-- highlighter on the redraw hot path — it flips a per-buffer override the Rust
-- engine reads, so highlighting stays synchronous and a highlight-only buffer is
-- parsed exactly once. (See ADR 0001, bridge #1.)
--
-- PREREQUISITE: a Rust parser installed in nxvim's data dir, laid out like
-- neovim's: `<data>/parser/rust.so` plus `<data>/queries/rust/highlights.scm`.
-- Without it the buffer simply stays un-highlighted (best-effort, no error).
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/treesitter-start \
--       cargo run -p nxvim -- examples/treesitter-start/sample.txt
--
-- The `.txt` buffer lights up with Rust highlighting on startup. Then toggle it.

--------------------------------------------------------------------------------
-- On startup: force Rust highlighting onto this buffer even though it's a `.txt`
-- the extension table doesn't map. This is the one line that matters.
--------------------------------------------------------------------------------
vim.treesitter.start(0, "rust")

--------------------------------------------------------------------------------
-- :TSStop — turn highlighting back off for this buffer. Because `stop` records
--    an *explicit* off state, it darkens the buffer even if the extension were a
--    recognized one (it isn't here) — useful to compare with/without.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSStop", function()
  vim.treesitter.stop(0)
  print("treesitter: stopped — buffer is now un-highlighted")
end, {})

--------------------------------------------------------------------------------
-- :TSStart — turn it back on (optionally pass a language; defaults to rust).
--    Try `:TSStart` after `:TSStop` to watch the highlights return.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TSStart", function(opts)
  local lang = opts.args ~= "" and opts.args or "rust"
  vim.treesitter.start(0, lang)
  print("treesitter: started in '" .. lang .. "'")
end, { nargs = "?" })

print("vim.treesitter.start demo: a .txt buffer highlighted as rust — try :TSStop / :TSStart")
