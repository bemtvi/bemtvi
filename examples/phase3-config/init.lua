-- ~~~ bemtvi keymap Phase 3 playground ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/phase3-config \
--       cargo run -p bemtvi -- examples/phase3-config/sample.txt
--
-- Each section says what to TYPE and what you should SEE. Phase 3 adds: insert &
-- command-mode maps, buffer-local maps (opts.buffer), vim.keymap.del, and the
-- lower-level nvim_set_keymap / nvim_buf_set_keymap family. (Phases 1-2 — leader,
-- remap, mode-lists, visual maps — live in examples/phase2-config.)

vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. INSERT-MODE map: the classic jk -> <Esc>.
--    TYPE:  i  then type some text, then  jk
--    SEE :  you leave insert mode (no literal "jk" is inserted).
--    A lone j (not followed by k) still inserts a literal j — the withheld
--    prefix is replayed when the next key breaks the sequence.
--------------------------------------------------------------------------------
vim.keymap.set("i", "jk", "<Esc>")

--------------------------------------------------------------------------------
-- 2. COMMAND-LINE map: edit the ':' line itself.
--    TYPE:  :  then  qq
--    SEE :  the command line reads ":quit" — qq expands to "quit" in command mode
--           only (in normal mode, qq is unmapped). Press <CR> to actually quit,
--           or <Esc> to back out.
--------------------------------------------------------------------------------
vim.keymap.set("c", "qq", "quit")

--------------------------------------------------------------------------------
-- 3. BUFFER-LOCAL map (opts.buffer = 0 -> the buffer current at set-time, here
--    the sample.txt buffer the editor opened with).
--    TYPE:  <Space>b
--    SEE :  message "buffer-local: only in sample.txt".
--    Open another buffer (:enew) and press <Space>b there: nothing fires — the
--    map is scoped to this buffer. Come back (:buffer 1) and it works again.
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>b", function()
  print("buffer-local: only in sample.txt")
end, { buffer = 0 })

--------------------------------------------------------------------------------
-- 4. vim.keymap.del — set a map, then delete it.
--    <Space>g is mapped, then immediately deleted, so it never fires.
--    TYPE:  <Space>g
--    SEE :  nothing (the keys fall through; the map was removed at startup).
--    Re-sourcing a config that re-sets a map leaves exactly one mapping — it
--    can't double-fire — which is what makes augroup-clear-style reloads safe.
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>g", function() print("you should never see this") end)
vim.keymap.del("n", "<leader>g")

--------------------------------------------------------------------------------
-- 5. The lower-level nvim_set_keymap family. Unlike vim.keymap.set, it is
--    REMAPPABLE by default (the :map-family default).
--    R is the action; the low-level T remaps to R (so T runs R's action), while
--    the low-level U opts into noremap and feeds a literal R to the editor.
--    TYPE:  T   -> SEE message "R action (low-level remap target)"
--    TYPE:  U   -> a literal R reaches core (replace-pending), NO message.
--------------------------------------------------------------------------------
vim.keymap.set("n", "R", function() print("R action (low-level remap target)") end)
vim.api.nvim_set_keymap("n", "T", "R", {}) -- remappable by default -> runs R's map
vim.api.nvim_set_keymap("n", "U", "R", { noremap = true }) -- literal R to the editor
