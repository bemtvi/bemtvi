-- ~~~ bemtvi treesitter highlighting via btv.bo: turn it on where the extension
--     table misses ~~~
--
-- bemtvi highlights recognized file extensions automatically off its in-core
-- treesitter engine (the "highlight floor"). But a buffer the extension table
-- doesn't know — a `.txt` holding code, a `:enew` scratch buffer, a custom
-- filetype — gets nothing. You turn the engine on for such a buffer through two
-- declarative buffer-local nouns:
--
--   btv.bo.filetype      WHICH language the buffer is (drives the parser choice,
--                       and anything else keyed off the filetype)
--   btv.bo.ts_highlight  WHETHER the in-core engine paints it
--
-- Set both and the buffer lights up; this is exactly what an ftplugin or a
-- `FileType` autocommand does in a real config. No Lua runs on the redraw hot
-- path — the Rust engine reads the buffer state — so a highlight-only buffer is
-- parsed exactly once. `btv.bo.<opt>` targets the current buffer; `btv.bo[buf].<opt>`
-- targets a specific one.
--
-- PREREQUISITE: a Rust parser installed in bemtvi's data dir, laid out like
-- neovim's: `<data>/parser/rust.so` plus `<data>/queries/rust/highlights.scm`.
-- Without it the buffer simply stays un-highlighted (best-effort, no error).
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/treesitter-start \
--       cargo run -p bemtvi -- examples/treesitter-start/sample.txt
--
-- The `.txt` buffer lights up with Rust highlighting on startup. Then toggle it.

--------------------------------------------------------------------------------
-- On startup: force Rust highlighting onto this buffer even though it's a `.txt`
-- the extension table doesn't map. These two lines are the whole story.
--------------------------------------------------------------------------------
btv.bo.filetype = "rust"
btv.bo.ts_highlight = true

--------------------------------------------------------------------------------
-- :TSStop — turn highlighting off for this buffer (`ts_highlight = false`). The
--    filetype is kept, so this darkens treesitter without disturbing anything
--    keyed off the language.
--------------------------------------------------------------------------------
btv.command("TSStop", function()
  btv.bo.ts_highlight = false
  print("treesitter: stopped — buffer is now un-highlighted")
end, {})

--------------------------------------------------------------------------------
-- :TSStart — turn it back on (optionally pass a language; defaults to rust). It
--    sets the filetype noun, then flips `ts_highlight` on.
--    Try `:TSStart` after `:TSStop` to watch the highlights return.
--------------------------------------------------------------------------------
btv.command("TSStart", function(opts)
  local lang = opts.args ~= "" and opts.args or "rust"
  btv.bo.filetype = lang
  btv.bo.ts_highlight = true
  print("treesitter: started in '" .. lang .. "'")
end, { nargs = "?" })

print("btv.bo treesitter demo: a .txt buffer highlighted as rust — try :TSStop / :TSStart")
