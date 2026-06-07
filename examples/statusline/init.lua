-- ~~~ nxvim statusline playground: a lualine-style 'statusline' from vim.fn ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/statusline \
--       cargo run -p nxvim -- examples/statusline/sample.txt
--
-- The status line is driven by the 'statusline' option's %-format engine. This
-- config sets `statusline = '%!v:lua.statusline()'`, so on EVERY redraw the engine
-- calls _G.statusline() and re-parses whatever it returns. That lets us assemble
-- the whole line in Lua from the Phase 5 `vim.fn` surface — mode(), line(), col(),
-- expand(), fnamemodify() — and fold in %#Group# colour switches, %= alignment,
-- and %m (the built-in modified flag) from the earlier statusline phases.
--
-- TRY IT interactively:
--   move around (hjkl, w, G)      the ruler block updates live every redraw
--   i / v / V / R   then <Esc>    the mode block recolours and relabels
--   edit the buffer (x, dd, i…)   a [+] modified flag appears next to the name
--   :e examples/tabs/sample.txt   the file block follows the current buffer
--   :set statusline=              fall back to nxvim's built-in default look

--------------------------------------------------------------------------------
-- 1. Segment colours. `nvim_set_hl` defines the highlight groups that the
--    `%#Group#` switches in the returned format string resolve against. (A real
--    config would pull these from its colourscheme; hard-coded here to be runnable
--    standalone.)
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "StlModeNormal", { fg = "#1a1b26", bg = "#7aa2f7", bold = true })
vim.api.nvim_set_hl(0, "StlModeInsert", { fg = "#1a1b26", bg = "#9ece6a", bold = true })
vim.api.nvim_set_hl(0, "StlModeVisual", { fg = "#1a1b26", bg = "#bb9af7", bold = true })
vim.api.nvim_set_hl(0, "StlFile", { fg = "#c0caf5", bg = "#3b4261" })
vim.api.nvim_set_hl(0, "StlRuler", { fg = "#1a1b26", bg = "#7aa2f7", bold = true })

--------------------------------------------------------------------------------
-- 2. mode() short code -> { label, highlight group }. vim.fn.mode() returns the
--    same single letters neovim does ("n"/"i"/"v"/"V"/"R"/"c").
--------------------------------------------------------------------------------
local MODES = {
  n = { "NORMAL", "StlModeNormal" },
  i = { "INSERT", "StlModeInsert" },
  R = { "REPLACE", "StlModeInsert" },
  v = { "VISUAL", "StlModeVisual" },
  V = { "V-LINE", "StlModeVisual" },
  c = { "COMMAND", "StlModeNormal" },
}

--------------------------------------------------------------------------------
-- 3. The builder. Returns a %-format string; because the option is `%!…`, the
--    engine re-parses the result, so the %#…#, %=, %m and %% items below are
--    honoured. Everything dynamic is read live through vim.fn.
--------------------------------------------------------------------------------
function _G.statusline()
  local mode = MODES[vim.fn.mode()] or { vim.fn.mode():upper(), "StlModeNormal" }

  -- Filename via the path builtins: the tail for the name, and the directory made
  -- relative to cwd then $HOME (`:~:.:h`) for a little context.
  local tail = vim.fn.expand("%:t")
  if tail == "" then tail = "[No Name]" end
  local dir = vim.fn.fnamemodify(vim.fn.expand("%"), ":~:.:h")
  local where = (dir ~= "" and dir ~= ".") and (dir .. "/") or ""

  -- The ruler reads the live cursor and buffer size.
  local line, col, last = vim.fn.line("."), vim.fn.col("."), vim.fn.line("$")
  local pct = last > 1 and math.floor((line - 1) / (last - 1) * 100) or 100

  return table.concat({
    "%#", mode[2], "# ", mode[1], " ",            -- coloured mode block
    "%#StlFile# ", where, tail, "%m ",            -- file (+ built-in modified flag)
    "%#StatusLine#%=",                            -- neutral spacer; %= pushes right
    "%#StlRuler# ", tostring(line), ":", tostring(col),
    "  ", tostring(pct), "%% ",                   -- %% is a literal percent sign
  })
end

vim.o.statusline = "%!v:lua.statusline()"
