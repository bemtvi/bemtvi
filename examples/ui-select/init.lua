-- ~~~ nxvim nx.ui.select playground: the floating selectable-list widget ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/ui-select \
--       cargo run -p nxvim -- examples/ui-select/sample.txt
--
-- `nx.ui.select(items, opts, on_choice)` is the callback-shaped chooser
-- (aliased by `vim.ui.select`). The SERVER owns the widget: it floats a
-- bordered list under the cursor, grabs every key, and resolves to one choice.
-- Lua only renders the labels up front and reacts to the result — nothing
-- blocks, no input loop runs in Lua (ADR 0002 / the float-widget spec).
--
-- Navigate the open menu with:  j / k  (or  <C-n> / <C-p>,  arrows,  gg / G)
-- Confirm with:  <CR>          Cancel with:  <Esc>  or  q

--------------------------------------------------------------------------------
-- 1. <leader>p — a plain string chooser.
--    TYPE:  \p           A menu of three fruits floats under the cursor.
--    Move with j/k, press <CR>. The choice (and its 1-based index) is echoed.
--    Press <Esc> instead and you'll see the cancel branch fire.
--------------------------------------------------------------------------------
vim.g.mapleader = "\\"

nx.keymap.set("n", "<leader>p", function()
  nx.ui.select({ "apple", "banana", "cherry" }, { prompt = "Pick a fruit:" }, function(item, idx)
    if item == nil then
      nx.notify("nothing picked (cancelled)")
    else
      nx.notify(("picked %s (#%d)"):format(item, idx))
    end
  end)
end)

--------------------------------------------------------------------------------
-- 2. <leader>c — choose a command to run.
--    Shows that `on_choice` can act on the editor: each entry carries a command
--    string, run via nx.cmd when chosen. TYPE:  \c  then pick one.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>c", function()
  local actions = {
    { label = "Save file", cmd = "write" },
    { label = "Split window", cmd = "split" },
    { label = "Show messages", cmd = "messages" },
  }
  nx.ui.select(actions, {
    prompt = "Action:",
    -- format_item renders the display label; on_choice still gets the ORIGINAL
    -- table, so the chosen entry's `cmd` round-trips even though only strings
    -- cross the bridge.
    format_item = function(a)
      return a.label
    end,
  }, function(choice)
    if choice then
      nx.cmd(choice.cmd)
    end
  end)
end)

--------------------------------------------------------------------------------
-- 3. A long list scrolls. <leader>n opens twenty entries; the box caps its
--    height and scrolls to keep the highlight visible as you j/k through it.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>n", function()
  local nums = {}
  for i = 1, 20 do
    nums[i] = "line " .. i
  end
  nx.ui.select(nums, { prompt = "Jump near:" }, function(item, idx)
    if idx then
      nx.notify("chose " .. item)
    end
  end)
end)
