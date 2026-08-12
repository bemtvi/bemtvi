-- ~~~ bemtvi fillchars: choose (or hide) the end-of-buffer `~` markers ~~~
--
-- Run it (from the repo root) against the short sample buffer:
--
--     BEMTVI_CONFIG=examples/fillchars \
--       cargo run -p bemtvi -- examples/fillchars/sample.txt
--
-- `'fillchars'` is the window-local `key:char` list that chooses the characters
-- drawn in structural spots. bemtvi honors the `eob` key today: the filler char
-- drawn on every screen row PAST the end of the buffer (vim's `~`). The sample is
-- a few lines tall, so most of the window is end-of-buffer fill — the perfect
-- place to see the marker.
--
--   :set fillchars=eob:\           blank the markers (an escaped trailing space) —
--                                  the empty rows below the text go plain
--   :set fillchars=eob:~           restore vim's default `~`
--   :set fillchars=eob:·           use a mid-dot instead
--   :set fillchars?                echo the current value
--
-- Like the number gutter, `'fillchars'` is per WINDOW: a split can blank its own
-- markers while the sibling keeps them. Set it from Lua with `vim.wo.fillchars`.

-- This config blanks the `~` out of the box, the most-requested setting (the
-- empty area below the text reads as clean blank rows). Comment this out to get
-- the default `~` look back.
vim.wo.fillchars = "eob: "

--------------------------------------------------------------------------------
-- :TildeBack / :TildeHide — flip the current window's markers from Lua.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TildeBack", function()
  vim.wo.fillchars = "eob:~"
  vim.notify("end-of-buffer markers: ~ (vim default)")
end, {})

vim.api.nvim_create_user_command("TildeHide", function()
  vim.wo.fillchars = "eob: "
  vim.notify("end-of-buffer markers: hidden (blank)")
end, {})

--------------------------------------------------------------------------------
-- :FillReport — read the per-window value back through vim.wo.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("FillReport", function()
  local out = {}
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    out[#out + 1] = string.format("win %d: fillchars=%q", w, vim.wo[w].fillchars)
  end
  vim.notify(table.concat(out, "  |  "))
end, {})

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. On open the rows below the three sample lines are BLANK (no `~`), because
--    this config set `fillchars=eob:<space>` above.
-- 2. :TildeBack       -> the `~` markers come back.
-- 3. <C-w>v then :TildeHide  -> blank this split only; <C-w>w shows the sibling
--    still has whatever it had (window-local).
-- 4. :FillReport      -> 'win N: fillchars="eob: "  |  win M: fillchars="eob:~"'
--------------------------------------------------------------------------------
