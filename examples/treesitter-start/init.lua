-- ~~~ nxvim nx.treesitter.start / stop: turn highlighting on where the
--     extension table misses ~~~
--
-- nxvim highlights recognized file extensions automatically off its in-core
-- treesitter engine (the "highlight floor"). But a buffer the extension table
-- doesn't know — a `.txt` holding code, a `:enew` scratch buffer, a custom
-- filetype — gets nothing. `nx.treesitter.start(buf, lang)` turns the engine on
-- for such a buffer at a language you choose; this is exactly what an ftplugin or
-- a `FileType` autocommand calls in a real config.
--
-- Under the hood these are verbs over declarative buffer state (the two nouns):
-- `start(buf, lang)` sets `nx.bo.filetype = lang` (which language) and
-- `nx.bo.ts_highlight = true` (paint it); `stop(buf)` sets `ts_highlight = false`
-- while keeping the filetype, so LSP/indent still see the language. No Lua runs on
-- the redraw hot path — the Rust engine reads the state — so a highlight-only
-- buffer is parsed exactly once.
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
nx.treesitter.start(0, "rust")

--------------------------------------------------------------------------------
-- :TSStop — turn highlighting off for this buffer (`ts_highlight = false`). The
--    filetype is kept, so this darkens treesitter without disturbing anything
--    keyed off the language.
--------------------------------------------------------------------------------
nx.command("TSStop", function()
  nx.treesitter.stop(0)
  print("treesitter: stopped — buffer is now un-highlighted")
end, {})

--------------------------------------------------------------------------------
-- :TSStart — turn it back on (optionally pass a language; defaults to rust).
--    Try `:TSStart` after `:TSStop` to watch the highlights return.
--------------------------------------------------------------------------------
nx.command("TSStart", function(opts)
  local lang = opts.args ~= "" and opts.args or "rust"
  nx.treesitter.start(0, lang)
  print("treesitter: started in '" .. lang .. "'")
end, { nargs = "?" })

print("nx.treesitter.start demo: a .txt buffer highlighted as rust — try :TSStop / :TSStart")
