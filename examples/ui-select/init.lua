-- ~~~ nxvim nx.ui.select playground: the floating selectable-list widget ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/ui-select \
--       cargo run -p nxvim -- examples/ui-select/sample.txt
--
-- `nx.ui.select(items, opts)` returns a PROMISE of the chosen item (nil on
-- cancel) — react with `:next(fn)`, or await it inside `nx.async`. The SERVER
-- owns the widget: it floats a bordered list under the cursor, grabs every key,
-- and resolves to one choice. Lua only renders the labels up front and reacts to
-- the result — nothing blocks, no input loop runs in Lua (ADR 0002 / the
-- float-widget spec). (The 1-based index is dropped from the promise; the
-- `vim.ui.select` compat alias keeps the callback `(item, index)` shape.)
--
-- The menu opens NOSELECT (like the completion popup): nothing is highlighted
-- until you move, so <CR> on a just-opened menu does nothing. The first j / k
-- reveals the highlight at the first row; navigate from there.
-- Navigate the open menu with:  j / k  (or  <C-n> / <C-p>,  arrows,  gg / G)
-- Confirm with:  <CR>          Cancel with:  <Esc>  or  q

--------------------------------------------------------------------------------
-- 1. <leader>p — a plain string chooser.
--    TYPE:  \p           A menu of three fruits floats under the cursor.
--    Move with j/k (the first press reveals the highlight), press <CR>. The chosen
--    fruit is echoed; the promise resolves to nil on <Esc>, so you'll see the
--    cancel branch fire.
--------------------------------------------------------------------------------
vim.g.mapleader = "\\"

nx.keymap.set("n", "<leader>p", function()
  nx.ui.select({ "apple", "banana", "cherry" }, { prompt = "Pick a fruit:" }):next(function(item)
    if item == nil then
      nx.notify("nothing picked (cancelled)")
    else
      nx.notify("picked " .. item)
    end
  end)
end)

--------------------------------------------------------------------------------
-- 2. <leader>c — choose a command to run.
--    Shows that the resolved value can act on the editor: each entry carries a
--    command string, run via nx.cmd when chosen. TYPE:  \c  then pick one.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>c", function()
  local actions = {
    { label = "Save file", cmd = "write" },
    { label = "Split window", cmd = "split" },
    { label = "Show messages", cmd = "messages" },
  }
  nx.ui.select(actions, {
    prompt = "Action:",
    -- format_item renders the display label; the promise still resolves to the
    -- ORIGINAL table, so the chosen entry's `cmd` round-trips even though only
    -- strings cross the bridge.
    format_item = function(a)
      return a.label
    end,
  }):next(function(choice)
    if choice then
      nx.cmd(choice.cmd)
    end
  end)
end)

--------------------------------------------------------------------------------
-- 3. A long list scrolls, AWAITED linearly. <leader>n opens twenty entries; the
--    box caps its height and scrolls to keep the highlight visible as you j/k
--    through it. Here the chooser is awaited inside `nx.async` — `nx.await` of the
--    select promise reads like a blocking read, but nothing blocks (the coroutine
--    suspends and resumes when you confirm).
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>n", nx.async(function()
  local nums = {}
  for i = 1, 20 do
    nums[i] = "line " .. i
  end
  local item = nx.await(nx.ui.select(nums, { prompt = "Jump near:" }))
  if item then
    nx.notify("chose " .. item)
  end
end))
