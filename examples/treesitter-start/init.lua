-- ~~~ nxvim treesitter highlighting via nx.bo: turn it on where the extension
--     table misses ~~~
--
-- nxvim highlights recognized file extensions automatically off its in-core
-- treesitter engine (the "highlight floor"). But a buffer the extension table
-- doesn't know — a `.txt` holding code, a `:enew` scratch buffer, a custom
-- filetype — gets nothing. You turn the engine on for such a buffer through two
-- declarative buffer-local nouns:
--
--   nx.bo.filetype      WHICH language the buffer is (drives the parser choice,
--                       and anything else keyed off the filetype)
--   nx.bo.ts_highlight  WHETHER the in-core engine paints it
--
-- Set both and the buffer lights up; this is exactly what an ftplugin or a
-- `FileType` autocommand does in a real config. No Lua runs on the redraw hot
-- path — the Rust engine reads the buffer state — so a highlight-only buffer is
-- parsed exactly once. `nx.bo.<opt>` targets the current buffer; `nx.bo[buf].<opt>`
-- targets a specific one.
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
-- the extension table doesn't map. These two lines are the whole story.
--------------------------------------------------------------------------------
nx.bo.filetype = "rust"
nx.bo.ts_highlight = true

--------------------------------------------------------------------------------
-- :TSStop — turn highlighting off for this buffer (`ts_highlight = false`). The
--    filetype is kept, so this darkens treesitter without disturbing anything
--    keyed off the language.
--------------------------------------------------------------------------------
nx.command("TSStop", function()
  nx.bo.ts_highlight = false
  print("treesitter: stopped — buffer is now un-highlighted")
end, {})

--------------------------------------------------------------------------------
-- :TSStart — turn it back on (optionally pass a language; defaults to rust). It
--    sets the filetype noun, then flips `ts_highlight` on.
--    Try `:TSStart` after `:TSStop` to watch the highlights return.
--------------------------------------------------------------------------------
nx.command("TSStart", function(opts)
  local lang = opts.args ~= "" and opts.args or "rust"
  nx.bo.filetype = lang
  nx.bo.ts_highlight = true
  print("treesitter: started in '" .. lang .. "'")
end, { nargs = "?" })

print("nx.bo treesitter demo: a .txt buffer highlighted as rust — try :TSStop / :TSStart")
