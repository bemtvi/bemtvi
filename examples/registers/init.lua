-- ~~~ nxvim registers: the Lua surface — setreg / getreg / getregtype + :put ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/registers \
--       cargo run -p nxvim -- examples/registers/shopping.txt
--
-- Phases 1–3 grew nxvim's one unnamed slot into vim's full register file
-- (named "a–"z, the numbered delete ring "1–"9, the yank register "0, the
-- small-delete "-, the read-only specials "% "/ ":, and :registers). Phase 4
-- adds the *programmatic* surface — the same registers, reachable from Lua:
--
--   vim.fn.setreg(name, value [, opts])  -- write a register
--   vim.fn.getreg(name)                  -- read its text
--   vim.fn.getregtype(name)              -- "v" charwise / "V" linewise
--   :put [x]                             -- paste register x BELOW, linewise
--
-- so a plugin or a user command can stash and recall text without driving the
-- keyboard. Everything below is wired through those four entry points.

-- Seed two registers at startup, before you touch the keyboard:
--   "h  a charwise greeting  (paste it inline with  "hp )
--   "t  a two-line todo block (drop it in with       :put t )
-- setreg with a *string* is charwise; with a *list* it is linewise (one item
-- per line) — exactly vim's rule.
vim.fn.setreg("h", "hello from setreg")
vim.fn.setreg("t", { "- buy milk", "- water plants" })

-- :Stash — copy the current line into register "s, linewise, so it round-trips
-- as a whole line through :put. Reads the line via the buffer API, writes it
-- with setreg, then echoes getreg/getregtype to prove the write landed in core.
vim.api.nvim_create_user_command("Stash", function()
  local row = vim.api.nvim_win_get_cursor(0)[1]
  local line = vim.api.nvim_buf_get_lines(0, row - 1, row, false)[1] or ""
  vim.fn.setreg("s", line, "l")
  print(('stashed "%s" into "s [%s]'):format(line, vim.fn.getregtype("s")))
end, {})

-- :Stashed — recall what :Stash left in "s, as a normal linewise paste below
-- the cursor. `:put s` is the linewise paste; it ignores the cursor column.
vim.api.nvim_create_user_command("Stashed", function()
  if vim.fn.getreg("s") == "" then
    print('"s is empty — run :Stash on a line first')
    return
  end
  vim.cmd("put s")
end, {})

-- :Shout — read register "h, upper-case it, and write it back (append-style)
-- into "h with the `a` flag, demonstrating a read→transform→write cycle entirely
-- in Lua. After running it once, `"hp` pastes "hello from setregHELLO FROM SETREG".
vim.api.nvim_create_user_command("Shout", function()
  local text = vim.fn.getreg("h")
  vim.fn.setreg("h", text:upper(), "a") -- 'a' = append to the current contents
  print('"h is now: ' .. vim.fn.getreg("h"))
end, {})

-- A keymap demo: <space>p drops the seeded todo block below the current line.
-- (`:put t` is linewise; the list we seeded into "t pastes as its own lines.)
vim.keymap.set("n", "<space>p", function()
  vim.cmd("put t")
end, { desc = "put the seeded todo register" })
