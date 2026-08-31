-- ~~~ bemtvi window-local options: a per-window number gutter ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/window-local-options \
--       cargo run -p bemtvi -- examples/window-local-options/sample.txt
--
-- `number` / `relativenumber` are window-local in vim: two windows onto the SAME
-- buffer can show different line-number gutters. bemtvi stores them on each window
-- (a split inherits them from the window it splits off), so `:set` / `:setlocal`
-- and `vim.wo` target the focused window only — the sibling is untouched.
--
-- TRY IT interactively:
--   <C-w>v               split this buffer left/right (both start with the
--                        default hybrid gutter: number + relativenumber)
--   :setlocal nonumber norelativenumber   drop the gutter in THIS split only
--   <C-w>w               hop to the other split — it still has its gutter
--   :GutterDemo          do the same thing from Lua across both windows

--------------------------------------------------------------------------------
-- :GutterDemo — open a vertical split and give each window a different gutter
-- from Lua. The new split starts as a clone of this one, then we override it:
-- the left (new, focused) window goes bare, the original keeps hybrid numbers.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("GutterDemo", function()
  local original = vim.api.nvim_get_current_win()
  vim.cmd("vsplit") -- queued: the new (focused) window exists on the NEXT tick

  btv.on_next_tick(function()
    local fresh = vim.api.nvim_get_current_win()

    -- Window-local writes via vim.wo: they change only the named window's gutter.
    vim.wo[fresh].number = false
    vim.wo[fresh].relativenumber = false
    vim.wo[original].number = true
    vim.wo[original].relativenumber = true

    vim.notify(
      string.format(
        "win %d: gutter off  |  win %d: hybrid gutter — same buffer, two gutters",
        fresh,
        original
      )
    )
  end)
end, {})

--------------------------------------------------------------------------------
-- :GutterReport — read the per-window option back (through vim.wo and the
-- nvim_win_get_option / nvim_get_option_value getters, which all agree).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("GutterReport", function()
  local out = {}
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    out[#out + 1] = string.format(
      "win %d: number=%s relativenumber=%s",
      w,
      tostring(vim.wo[w].number),
      tostring(vim.api.nvim_win_get_option(w, "relativenumber"))
    )
  end
  vim.notify(table.concat(out, "  |  "))
end, {})

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. :GutterDemo          -> a vertical split; left window has NO gutter, the
--                            right (original) keeps the hybrid numbers. Same
--                            buffer, two different gutters.
-- 2. :GutterReport        -> "win N: number=false ... | win M: number=true ..."
-- 3. By hand:  <C-w>v  then  :setlocal nonumber   -> only the focused split loses
--    its numbers;  <C-w>w  shows the sibling still has them.
--------------------------------------------------------------------------------
