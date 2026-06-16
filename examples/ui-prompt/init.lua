-- ~~~ nxvim nx.ui.input / nx.ui.confirm playground: the command-line prompts ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/ui-prompt \
--       cargo run -p nxvim -- examples/ui-prompt/sample.txt
--
-- `nx.ui.input` and `nx.ui.confirm` are two of the four small async UI
-- primitives the native-plugin API names (input / select / confirm / float).
-- BOTH are PROMISE-ONLY and NON-BLOCKING (ADR 0002 rule 3): the Lua call returns
-- a promise at once and it settles on a LATER tick when you submit / cancel — so
-- you react with `:next(fn)` (or await it inside `nx.async`). They open over the
-- editor's COMMAND LINE (only one prompt at a time); neovim spells these as the
-- blocking `vim.fn.input` / `vim.fn.confirm`, which the nx model deliberately omits.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. <leader>i — nx.ui.input: a one-line text prompt.
--    TYPE:  \i      The command line opens labelled "Your name: ". Type a name
--    and press <CR> — it echoes back. Press <Esc> to cancel (the callback runs
--    with `nil`, so a caller can clean up).
--
--    opts.prompt  = the label drawn ahead of the editable line
--    opts.default = text prefilled into the line (cursor at its end)
--    The promise resolves to the entered string ("" on an empty <CR>), or nil on cancel.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>i", function()
  nx.ui.input({ prompt = "Your name: " }):next(function(text)
    if text == nil then
      nx.notify("input cancelled")
    else
      nx.notify("hello, " .. text)
    end
  end)
end)

--------------------------------------------------------------------------------
-- 2. <leader>r — nx.ui.input with a prefilled default.
--    TYPE:  \r      The line is pre-filled with the current file name; edit it
--    or accept it with <CR>.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>r", function()
  nx.ui.input({ prompt = "Rename to: ", default = vim.fn.expand("%:t") }):next(function(name)
    if name and name ~= "" then
      nx.notify("would rename to " .. name)
    end
  end)
end)

--------------------------------------------------------------------------------
-- 3. <leader>d — nx.ui.confirm: a yes/no confirmation.
--    TYPE:  \d      The command line shows "Delete this line? [Y/n]". Press `y`
--    (or <CR>, since Yes is the default) to confirm, `n` or <Esc> to decline.
--    The promise resolves to a BOOLEAN — true on Yes, false on No / cancel.
--    For an arbitrary multi-choice menu use nx.ui.select instead.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>d", function()
  nx.ui.confirm("Delete this line?"):next(function(ok)
    if ok then
      nx.cmd("delete") -- the `:delete` ex command removes the current line
      nx.notify("line deleted")
    else
      nx.notify("kept")
    end
  end)
end)

--------------------------------------------------------------------------------
-- 4. <leader>q — nx.ui.confirm defaulting to No (the safe choice on <CR>).
--    TYPE:  \q      "Quit without saving? [y/N]" — <CR> declines.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>q", function()
  nx.ui.confirm("Quit without saving?", { default = false }):next(function(ok)
    if ok then
      nx.cmd("qa!")
    else
      nx.notify("staying")
    end
  end)
end)
