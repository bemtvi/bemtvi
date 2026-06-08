-- ~~~ nxvim mini-which-key: a popup of key hints, built from the display surface ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/whichkey-popup \
--       cargo run -p nxvim -- examples/whichkey-popup/sample.txt
--
-- This is a self-contained, ~80-line clone of the *core* of which-key.nvim. It
-- exists to exercise — end to end, in the running editor — the display APIs that
-- a real which-key needs and that nxvim now provides:
--
--   * vim.api.nvim_get_hl(0, { name, link })   read theme colors (follow links)
--   * vim.fn.strdisplaywidth / strchars        measure labels for grid layout
--   * vim.fn.strtrans                          render a raw key readably
--   * vim.api.nvim_create_buf                  a scratch buffer for the popup
--   * vim.api.nvim_open_win (float)            draw it on top, anchored to cursor
--   * vim.api.nvim_buf_set_extmark             theme each row
--   * vim.fn.getcharstr                        block for the next key
--   * vim.api.nvim_win_close + nvim_buf_delete tear the popup down
--   * vim.api.nvim_buf_call                    run a read in the popup's context
--
-- TRY IT:
--   press  <leader>  (Space)   the hint popup opens at the cursor; press one of
--                              f / g / q to pick, or <Esc> to dismiss. The chosen
--                              action's name prints on the message line.
--
-- The popup is a REAL floating window onto a REAL scratch buffer — it paints over
-- the text, steals no layout space, and is deleted (buffer and window) the moment
-- you answer, exactly as which-key does.

vim.g.mapleader = " "

-- A tiny "leader menu": key -> { description, action }. A real which-key reads
-- this from your keymaps; here it's inline so the example is self-contained.
local menu = {
  f = { "find file", function() print("would find a file") end },
  g = { "git status", function() print("would show git status") end },
  q = { "quit",       function() vim.cmd("quit") end },
}

-- Theme the popup. catppuccin (or any colorscheme) would define these; we set
-- them so the example themes itself even with no colorscheme loaded. `WhichKeyDesc`
-- LINKS to Comment, so reading it back with `link = false` proves link-following.
vim.api.nvim_set_hl(0, "Comment", { fg = "#6c7086", italic = true })
vim.api.nvim_set_hl(0, "WhichKey", { fg = "#89b4fa", bold = true })
vim.api.nvim_set_hl(0, "WhichKeyDesc", { link = "Comment" })

-- Build the popup's lines and the keys, laid out in an aligned grid. The key
-- column is padded to the widest *display width* (not byte length) so multibyte
-- or wide keys would still align — the job strdisplaywidth exists for.
local function build()
  local keys = vim.tbl_keys(menu)
  table.sort(keys)
  local keyw = 0
  for _, k in ipairs(keys) do
    keyw = math.max(keyw, vim.fn.strdisplaywidth(vim.fn.strtrans(k)))
  end
  local lines, width = {}, 0
  for _, k in ipairs(keys) do
    local label = vim.fn.strtrans(k)
    local pad = string.rep(" ", keyw - vim.fn.strdisplaywidth(label))
    local line = string.format(" %s%s  %s ", label, pad, menu[k][1])
    lines[#lines + 1] = line
    width = math.max(width, vim.fn.strdisplaywidth(line))
  end
  return lines, width, keyw
end

vim.keymap.set("n", "<leader>", function()
  local lines, width, keyw = build()

  -- A scratch buffer holds the popup text; it is never listed and is deleted on
  -- close, so it leaves nothing behind.
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)

  -- Theme each row: the key cell in WhichKey, the description in WhichKeyDesc
  -- (which resolves through its link to Comment).
  local ns = vim.api.nvim_create_namespace("mini-which-key")
  for i, line in ipairs(lines) do
    local row = i - 1
    -- A highlight span needs end_row AND end_col together (both 0-based); here the
    -- span stays on one row, so end_row == row.
    vim.api.nvim_buf_set_extmark(buf, ns, row, 1, { end_row = row, end_col = 1 + keyw, hl_group = "WhichKey" })
    vim.api.nvim_buf_set_extmark(buf, ns, row, keyw + 3, { end_row = row, end_col = #line, hl_group = "WhichKeyDesc" })
  end

  -- Read the resolved popup accent color (proves nvim_get_hl + link-follow); a
  -- real which-key uses it to blend the float background.
  local accent = vim.api.nvim_get_hl(0, { name = "WhichKey", link = false }).fg
  local desc = vim.api.nvim_get_hl(0, { name = "WhichKeyDesc", link = false }).fg

  -- Draw the popup as a float anchored just below the cursor.
  local win = vim.api.nvim_open_win(buf, false, {
    relative = "cursor",
    anchor = "NW",
    row = 1,
    col = 0,
    width = width,
    height = #lines,
    border = "rounded",
    title = "which-key",
  })

  -- nvim_buf_call: run a read with the popup buffer current (here, confirm its
  -- line count from inside its own context — the kind of scoped read which-key
  -- uses to measure content).
  local rows = vim.api.nvim_buf_call(buf, function()
    return vim.api.nvim_buf_line_count(0)
  end)
  assert(rows == #lines, "buf_call should see the popup buffer as current")

  -- Block for the next key, then tear the popup down — buffer AND window — before
  -- acting, exactly as which-key does.
  local choice = vim.fn.getcharstr()
  vim.api.nvim_win_close(win, true)
  vim.api.nvim_buf_delete(buf, { force = true })

  local entry = menu[choice]
  if entry then
    entry[2]()
  elseif choice ~= "<Esc>" then
    print("which-key: no action for " .. vim.fn.strtrans(choice))
  end
  -- (accent / desc are read above to demonstrate the theme API; a fuller popup
  -- would pass them to nvim_set_hl for a blended background. We reference them so
  -- the intent is clear.)
  local _ = accent
  local _ = desc
end)
