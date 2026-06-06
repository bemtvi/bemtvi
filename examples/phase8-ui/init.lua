-- ~~~ nxvim LSP Phase 8 playground: vim.ui.* + command dispatch ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/phase8-ui \
--       cargo run -p nxvim -- examples/phase8-ui/sample.txt
--
-- Phase 8 wires the interactive surface configs use: `vim.ui.select` (a panel
-- picker), `vim.ui.input` (a command-line prompt), `vim.ui.open` (the OS opener),
-- and LSP command dispatch (`vim.lsp.commands` client-side, else the server's
-- `workspace/executeCommand`). The first three need no language server, so this
-- playground demos them directly. Each section says what to TYPE and what to SEE.

vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. vim.ui.select: pick one of a list in the panel.
--    TYPE:  <Space>s
--    SEE :  a panel titled "Pick a fruit:" with three rows. Move with j/k and
--           press <CR> on one — the message line echoes "you picked: <fruit>".
--           (Press q to dismiss without picking; no choice is delivered.)
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>s", function()
  vim.ui.select(
    { "apple", "banana", "cherry" },
    { prompt = "Pick a fruit:" },
    function(item, idx)
      if item then
        print("you picked: " .. item .. " (row " .. idx .. ")")
      end
    end
  )
end)

--------------------------------------------------------------------------------
-- 2. vim.ui.select with format_item: the rows are RENDERED from richer items,
--    but on_choice still receives the original table.
--    TYPE:  <Space>c
--    SEE :  a panel of colour NAMES; <CR> echoes the picked colour's hex code
--           (proof the callback got the table, not the displayed label).
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>c", function()
  vim.ui.select(
    { { name = "Red", hex = "#ff0000" }, { name = "Green", hex = "#00ff00" }, { name = "Blue", hex = "#0000ff" } },
    { prompt = "Pick a colour:", format_item = function(it) return it.name end },
    function(item)
      if item then print("hex is " .. item.hex) end
    end
  )
end)

--------------------------------------------------------------------------------
-- 3. vim.ui.input: prompt for a line of text on the command line.
--    TYPE:  <Space>i
--    SEE :  a "Your name: " prompt (prefilled with "anon"). Edit it, press <CR>
--           -> "hello, <text>!". Press <Esc> instead -> "(cancelled)": a
--           cancelled input hands the callback nil, exactly like neovim.
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>i", function()
  vim.ui.input({ prompt = "Your name: ", default = "anon" }, function(text)
    if text == nil then
      print("(cancelled)")
    else
      print("hello, " .. text .. "!")
    end
  end)
end)

--------------------------------------------------------------------------------
-- 4. vim.lsp.commands: register a client-side command handler. With a language
--    server attached, a code action whose `command` is "demo.greet" (or an
--    explicit vim.lsp.buf.execute_command) runs THIS function locally instead of
--    being relayed to the server as workspace/executeCommand. Unregistered
--    commands ARE relayed. (No server is started here; this just shows the shape.)
--------------------------------------------------------------------------------
vim.lsp.commands["demo.greet"] = function(command, ctx)
  print("ran demo.greet client-side for buffer " .. tostring(ctx.bufnr))
end

--------------------------------------------------------------------------------
-- 5. vim.ui.open: hand a path/URL to the OS opener (open / xdg-open) via the
--    async vim.system. Uncomment to try it (it launches your browser):
--------------------------------------------------------------------------------
-- vim.keymap.set("n", "<leader>o", function()
--   vim.ui.open("https://neovim.io")
-- end)
