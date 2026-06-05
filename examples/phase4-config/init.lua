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
-- 1. AMBIGUOUS MAPPED PREFIX: `gg` is a real prefix of the `ggx` MAPPING.
--    TYPE:  G            (jump to the last line, so the move is visible)
--    TYPE:  gg           then STOP — don't press anything else.
--    SEE :  after ~1s the cursor jumps to the FIRST line. `gg` is a live prefix
--           of `ggx`, so it is genuinely held (a following `x` would take the
--           map); the idle flush resolves it to the `gg` built-in because no `x`
--           followed. TYPE `ggx` quickly instead and the MAPPING fires.
--    NOTE:  a `gg` that does NOT prefix a mapping (e.g. with only `gh` mapped) is
--           now INSTANT — no wait — via the built-in disambiguation. See the
--           examples/keymap-builtin playground. The flush below is for the
--           genuinely-ambiguous *mapped* case only.
--------------------------------------------------------------------------------
vim.keymap.set("n", "ggx", function()
  print("ggx fired (the mapping; the held gg prefix completed it)")
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

--------------------------------------------------------------------------------
-- 3. <nowait>: skip the wait entirely. `,` is a complete map AND a prefix of
--    `,x`, but it's marked nowait, so it fires the INSTANT you press it.
--    TYPE:  ,            (just the comma)
--    SEE :  "comma (nowait — fired without waiting for ,x)" immediately — no
--           pause, no need to press anything else. (Pressing `,x` can never
--           reach the longer map now; that's the nowait trade-off.)
--------------------------------------------------------------------------------
vim.keymap.set("n", ",", function()
  print("comma (nowait — fired without waiting for ,x)")
end, { nowait = true })
vim.keymap.set("n", ",x", function()
  print("you won't see this — , fired first")
end)

--------------------------------------------------------------------------------
-- 4. <silent>: run the mapping but keep the command line clean.
--    TYPE:  <Space>n     -> SEE the message "not silent: you can read me".
--    TYPE:  <Space>q     -> SEE nothing on the command line, BUT the output is
--           still logged: run  :messages  and "silent: only in :messages" is
--           there. <silent> hides the transient display, not the history.
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>n", function()
  print("not silent: you can read me")
end)
vim.keymap.set("n", "<leader>q", function()
  print("silent: only in :messages")
end, { silent = true })

--------------------------------------------------------------------------------
-- 5. <unique>: refuse to clobber an existing map. We map `<leader>u`, then try
--    to re-map it with unique = true — which errors (E227) instead of
--    overwriting. The pcall below captures that error so sourcing still
--    succeeds; press <Space>u to confirm the ORIGINAL map survived.
--    TYPE:  <Space>u  -> SEE "original <leader>u (the unique re-map was refused)".
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>u", function()
  print("original <leader>u (the unique re-map was refused)")
end)
local ok, err = pcall(function()
  vim.keymap.set("n", "<leader>u", function()
    print("you won't see this — unique refused the overwrite")
  end, { unique = true })
end)
if not ok then
  -- E227 surfaced as expected; leave a breadcrumb in :messages.
  print("(<leader>u unique clash refused: " .. tostring(err) .. ")")
end

--------------------------------------------------------------------------------
-- 6. <expr>: the RHS function RETURNS the keys to feed, computed at press time.
--    Here `H` jumps to the top or the bottom depending on a flag a second key
--    flips — the whole point of <expr>: the keys depend on state.
--    TYPE:  G   (go to the last line)  then  H   -> SEE the cursor jump to the
--           TOP (the flag starts true, so H returns "gg").
--    TYPE:  <Space>f  to flip the flag, then  H  -> SEE it jump to the BOTTOM (H
--           now returns "G").  (<Space>f is a normal map; H is the <expr> one.)
--    The sandbox: an <expr> RHS must only compute keys — if it tries to change
--    the editor (e.g. vim.cmd("...")), it raises a textlock error and feeds
--    nothing. Reading state (vim.g, vim.b, …) is fine.
--------------------------------------------------------------------------------
vim.g.expr_top = true
vim.keymap.set("n", "H", function()
  return vim.g.expr_top and "gg" or "G"
end, { expr = true })
vim.keymap.set("n", "<leader>f", function()
  vim.g.expr_top = not vim.g.expr_top
  print("expr target flipped -> " .. (vim.g.expr_top and "top (gg)" or "bottom (G)"))
end)
