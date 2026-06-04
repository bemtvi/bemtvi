-- ~~~ nxvim keymap Phase 2 playground ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/phase2-config \
--       cargo run -p nxvim -- examples/phase2-config/sample.txt
--
-- Each section says what to TYPE and what you should SEE. Messages print to the
-- bottom message line. Phase 2 covers: <leader>, remap vs noremap, remap chains,
-- mode-lists ({ 'n', 'v' }), and Visual-mode (x) maps.

-- <leader> is expanded at SET-TIME from vim.g.mapleader, so set it FIRST, before
-- any map that uses <leader>. We use Space here (the popular choice).
vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. <leader> + a function RHS
--    TYPE:  <Space>h
--    SEE :  message "hello from <leader> ..."
--    (<Space> alone is withheld as a live prefix until the next key arrives.)
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>h", function()
  print("hello from <leader>  (mapleader = Space)")
end)

--------------------------------------------------------------------------------
-- 2. A noremap STRING RHS (the vim.keymap.set default)
--    TYPE:  Y          then   p
--    SEE :  Y yanks to end-of-line (it's mapped to y$); p pastes it back.
--------------------------------------------------------------------------------
vim.keymap.set("n", "Y", "y$")

--------------------------------------------------------------------------------
-- 3. remap vs noremap — the headline Phase 2 distinction
--    X is the "action" (a function map).
--    Q is a REMAP   to X  -> pressing Q runs X's action (RHS re-fed through maps).
--    W is a NOREMAP to X  -> pressing W feeds a *literal* X to the editor
--                            (delete-char-under-cursor); the action does NOT run.
--
--    TYPE:  Q   -> SEE message "X ACTION fired"
--    TYPE:  W   -> SEE a character deleted, NO message (literal X reached core)
--------------------------------------------------------------------------------
vim.keymap.set("n", "X", function() print("X ACTION fired") end)
vim.keymap.set("n", "Q", "X", { remap = true }) -- recursive: resolves to X's map
vim.keymap.set("n", "W", "X") -- non-recursive: literal X to the editor

--------------------------------------------------------------------------------
-- 4. A remap CHAIN through several hops:  <leader>a -> <leader>b -> <leader>c-fn
--    TYPE:  <Space>a
--    SEE :  message "reached c via a -> b -> c"  (two remap hops, then the fn)
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>a", "<leader>b", { remap = true })
vim.keymap.set("n", "<leader>b", "<leader>c", { remap = true })
vim.keymap.set("n", "<leader>c", function() print("reached c via a -> b -> c") end)

--------------------------------------------------------------------------------
-- 5. A self-referential remap terminates instead of hanging (maxmapdepth).
--    Z -> Z (remap) loops until the per-keystroke budget runs out, then falls
--    through to one literal Z.
--    TYPE:  Z   -> the editor stays responsive; one literal Z reaches core.
--------------------------------------------------------------------------------
vim.keymap.set("n", "Z", "Z", { remap = true })

--------------------------------------------------------------------------------
-- 6. A mode-LIST map: fires in every listed mode.
--    TYPE (normal):  <Space>p              -> SEE "fires in normal AND visual"
--    TYPE (visual):  v<Space>p             -> SEE the same message
--------------------------------------------------------------------------------
vim.keymap.set({ "n", "v" }, "<leader>p", function()
  print("fires in normal AND visual")
end)

--------------------------------------------------------------------------------
-- 7. A Visual-ONLY (x-mode) map. Applies in Visual / Visual-Line, NOT in Normal.
--    TYPE:  vL   -> in Visual, L is mapped to $, extending the selection to EOL.
--    (In Normal, L is unmapped and keeps its normal meaning.)
--------------------------------------------------------------------------------
vim.keymap.set("x", "L", "$")

--------------------------------------------------------------------------------
-- 8. Multi-key withhold/replay still passes unmapped prefixes to core.
--    gh is mapped; gg is NOT, so the withheld g replays and gg = go-to-top.
--    TYPE:  gh        -> SEE "gh mapping"
--    TYPE:  gg        -> cursor jumps to the first line (core's gg, via replay)
--------------------------------------------------------------------------------
vim.keymap.set("n", "gh", function() print("gh mapping") end)

-- Note: insert/command-mode maps (e.g. 'i', 'jk', '<Esc>') and buffer-local maps
-- are Phase 3 — not wired here. String RHSs that run an ex-command use the
-- ':cmd<CR>' colon form, e.g.  vim.keymap.set('n', '<leader>t', ':echo hi<CR>')
-- the '<cmd>...<cr>' notation is a later phase.
