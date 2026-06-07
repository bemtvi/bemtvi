-- ~~~ nxvim synchronous prompts playground: vim.fn.input + vim.fn.confirm ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/sync-prompts \
--       cargo run -p nxvim -- examples/sync-prompts/sample.txt
--
-- Unlike the async `vim.ui.input` (callback) surface, `vim.fn.input` and
-- `vim.fn.confirm` BLOCK and RETURN the answer inline: the keymap body reads the
-- result straight off the call. nxvim runs a mapping/`:lua`/command inside a
-- coroutine, so the prompt suspends the body on the command line and resumes it
-- with the answer. Each section says what to TYPE and what to SEE.

vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. vim.fn.input: prompt for a line of text; the typed string is returned.
--    TYPE:  <Space>i
--    SEE :  a "Your name: " prompt (prefilled with "anon"). Edit it, press <CR>
--           -> "hello, <text>!". Press <Esc> instead -> "" (empty string), so
--           the message reads "hello, !" — input() returns "" on cancel (the
--           contract difference from vim.ui.input, which hands its callback nil).
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>i", function()
  local name = vim.fn.input({ prompt = "Your name: ", default = "anon" })
  print("hello, " .. name .. "!")
end)

--------------------------------------------------------------------------------
-- 2. vim.fn.confirm: a single-key button dialog; the 1-based index is returned.
--    Buttons mark their accelerator with `&` ("&Yes" -> press y/Y).
--    TYPE:  <Space>d
--    SEE :  "Delete the line? [Y]es, [N]o, [C]ancel: ". Press y to delete the
--           current line, n/c (or <Esc>) to leave it. confirm() returns 1/2/3
--           for the button, 0 if cancelled.
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>d", function()
  local choice = vim.fn.confirm("Delete the line?", "&Yes\n&No\n&Cancel", 2)
  if choice == 1 then
    local row = vim.api.nvim_win_get_cursor(0)[1] -- 1-based
    vim.api.nvim_buf_set_lines(0, row - 1, row, false, {})
    print("deleted")
  else
    print("kept (chose " .. choice .. ")")
  end
end)

--------------------------------------------------------------------------------
-- 3. Chaining prompts: input() then confirm(), each resuming the same body — ask
--    for replacement text, confirm, then replace the current line with it.
--    TYPE:  <Space>r
--    SEE :  "New text: " (type a line, <CR>), then "Replace this line? [Y]es,
--           [N]o: ". On Yes the current line becomes what you typed.
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>r", function()
  local to = vim.fn.input("New text: ")
  if to == "" then
    print("(nothing entered)")
    return
  end
  if vim.fn.confirm("Replace this line?", "&Yes\n&No", 1) == 1 then
    local row = vim.api.nvim_win_get_cursor(0)[1] -- 1-based
    vim.api.nvim_buf_set_lines(0, row - 1, row, false, { to })
    print("replaced")
  else
    print("(cancelled)")
  end
end)
