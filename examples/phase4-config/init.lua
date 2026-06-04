-- ~~~ nxvim keymap Phase 4 playground: the `timeoutlen` idle flush ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/phase4-config \
--       cargo run -p nxvim -- examples/phase4-config/sample.txt
--
-- Phase 4 closes the "trailing-prefix lag" (design D4). nxvim's server has no
-- input timer, so a key that is a *live prefix* of a mapping is withheld until
-- the next keystroke. The TUI now arms a timeoutlen timer (~1s, vim's default)
-- after each key and, on idle, nudges the server to resolve that withheld key —
-- so a sequence completes on its own, the way real vim does.
--
-- Each section says what to TYPE and what you should SEE. The point of this
-- playground is the PAUSE: type, then WAIT ~1s without pressing anything.

vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. PREFIX-vs-BUILTIN: with `gh` mapped, `g` becomes a live prefix.
--    TYPE:  G            (jump to the last line, so the move is visible)
--    TYPE:  gg           then STOP — don't press anything else.
--    SEE :  after ~1s the cursor jumps to the FIRST line. The second `g` was
--           withheld (it could have continued to `gh`); the idle flush replayed
--           it, so core saw `gg` (go-to-top) and moved on its own.
--    Before Phase 4 the cursor would sit still until you pressed another key.
--    (Pressing `gh` instead still fires the map below — type it to compare.)
--------------------------------------------------------------------------------
vim.keymap.set("n", "gh", function()
  print("gh fired (the mapping, not the gg motion)")
end)

--------------------------------------------------------------------------------
-- 2. AMBIGUOUS short/long: `j` is both a complete map AND a prefix of `jk`.
--    TYPE:  j            then STOP — don't press anything else.
--    SEE :  after ~1s the message "j (the shorter map)" appears. The flush
--           resolves the ambiguity in favor of the SHORTER map — vim's
--           timeoutlen behavior — because no `k` followed.
--    TYPE:  jk  quickly  -> SEE "jk (the longer map)" instead: the `k` arrived
--           before the idle flush, so the longer map won.
--------------------------------------------------------------------------------
vim.keymap.set("n", "j", function()
  print("j (the shorter map)")
end)
vim.keymap.set("n", "jk", function()
  print("jk (the longer map)")
end)
