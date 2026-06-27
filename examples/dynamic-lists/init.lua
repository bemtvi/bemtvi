-- ~~~ nxvim: dynamic (named, function-sourced) quickfix / location lists ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/dynamic-lists \
--       cargo run -p nxvim -- examples/dynamic-lists/sample.txt
--
-- A *dynamic list* binds a NAME to a DATASOURCE FUNCTION. Calling
-- `nx.qf.refresh(name)` re-runs that function and rewrites the bound list in
-- place, so an open quickfix / location window repaints with the fresh results.
-- The source may return an items array directly, or a PROMISE resolving to one
-- (slow producers — LSP, ripgrep — never block the editor).
--
--   nx.qf.dynamic { name=, source=fn, loclist=, win=, title= }   register
--   nx.qf.refresh(name)   -> promise   re-run the source, redraw (action "r")
--   nx.qf.drop(name)      -> bool       forget the registration
--
-- See docs/features/quickfix-dock-lists.md.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. A dynamic QUICKFIX list of every TODO/FIXME line in the current buffer.
--    The source re-scans the live buffer each refresh, so editing the file and
--    pressing \tr repaints the open list — no stale snapshot.
--------------------------------------------------------------------------------
nx.qf.dynamic({
  name = "todos",
  title = "TODO / FIXME",
  source = function()
    local items = {}
    for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
      if line:find("TODO") or line:find("FIXME") then
        local typ = line:find("FIXME") and "E" or "W"
        items[#items + 1] =
          { filename = nx.buf.name(), lnum = i, col = 1, text = line, type = typ }
      end
    end
    return items
  end,
})

nx.keymap.set("n", "<leader>tr", function()
  nx.qf.refresh("todos"):next(function()
    nx.qf.open() -- :copen — show (or focus) the quickfix window
  end)
end, { desc = "Refresh the dynamic TODO quickfix list" })

--------------------------------------------------------------------------------
-- 2. A dynamic LOCATION list bound to THIS window. Because a loclist is
--    per-window, nx.qf.dynamic captures the current window now, so refreshing it
--    from anywhere still targets the right one.
--------------------------------------------------------------------------------
nx.qf.dynamic({
  name = "long-lines",
  loclist = true,
  title = "Lines over 40 cols",
  source = function()
    local items = {}
    for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
      if #line > 40 then
        items[#items + 1] =
          { filename = nx.buf.name(), lnum = i, col = 41, text = (#line .. " cols: " .. line) }
      end
    end
    return items
  end,
})

nx.keymap.set("n", "<leader>lr", function()
  nx.qf.refresh("long-lines"):next(function()
    nx.qf.lopen() -- :lopen — show this window's location list
  end)
end, { desc = "Refresh the dynamic long-lines location list" })

--------------------------------------------------------------------------------
-- 3. An ASYNC source: the function returns a promise (here a trivial delayed
--    one; in real configs this is where you await `nx.run`/LSP results). refresh
--    awaits it before writing the list.
--------------------------------------------------------------------------------
nx.qf.dynamic({
  name = "async-demo",
  title = "Async results",
  source = function()
    return nx.promise.delay(150):next(function()
      return {
        { filename = nx.buf.name(), lnum = 1, col = 1, text = "produced asynchronously" },
      }
    end)
  end,
})

nx.keymap.set("n", "<leader>ar", function()
  nx.qf.refresh("async-demo"):next(function()
    nx.qf.open()
  end)
end, { desc = "Refresh the async-sourced quickfix list" })

-- Step through whichever list is current with the usual commands:
--   :cnext / :cprev  (quickfix)     :lnext / :lprev  (location list)
--   <CR> in the list jumps to the entry      :cclose / :lclose closes it
