-- ~~~ bemtvi statusline playground: a lualine-style 'statusline' from vim.fn ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/statusline \
--       cargo run -p bemtvi -- examples/statusline/sample.txt
--
-- The status line is driven by the 'statusline' option's %-format engine. This
-- config sets `statusline = '%!v:lua.statusline()'`, so on EVERY redraw the engine
-- calls _G.statusline() and re-parses whatever it returns. That lets us assemble
-- the whole line in Lua from the Phase 5 `vim.fn` surface — mode(), line(), col(),
-- expand(), fnamemodify() — and fold in %#Group# colour switches, %= alignment,
-- and %m (the built-in modified flag) from the earlier statusline phases.
--
-- The encoding block shows the OTHER kind of %{} expression: a pure Vim
-- expression with no Lua. `%{&fileencoding}` reads the buffer option directly, and
-- `%{&bomb?"[bom]":""}` uses the ternary to tag a byte-order mark — exactly like
-- neovim, which has no %-letter for the encoding. (Anything that isn't `v:lua.…`
-- runs through bemtvi's pure expression evaluator: literals, &options, comparison,
-- ternary. A bare variable or unknown option fails loud on the line, never silently.)
--
-- TRY IT interactively:
--   move around (hjkl, w, G)      the ruler block updates live every redraw
--   i / v / V / R   then <Esc>    the mode block recolours and relabels
--   edit the buffer (x, dd, i…)   a [+] modified flag appears next to the name
--   :set fileencoding=latin1      the %{&fileencoding} block switches to "latin1"
--   :set bomb                     a "[bom]" tag appears via the %{&bomb?…} ternary
--   :e examples/tabs/sample.txt   the file block follows the current buffer
--   click the filename block       a `%@v:lua.…@…%X` click region echoes the path
--   :set statusline=              fall back to bemtvi's built-in default look

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
-- 3a. A click handler. `%@v:lua.fn@ … %X` in the format makes the wrapped text a
--    clickable region: a mouse click on those cells calls _G.on_name_click with
--    neovim's click arguments (minwid, clicks, button, modifiers). Here clicking
--    the filename block toggles the buffer's modified-look by echoing its path —
--    a real config might open a buffer picker or a git menu. (The handler must be
--    a `v:lua.` reference, like the %{}/%! expressions.)
--------------------------------------------------------------------------------
function _G.on_name_click(minwid, clicks, button, mods)
  local where = vim.fn.expand("%:p")
  if where == "" then where = "[No Name]" end
  vim.cmd(string.format("echo 'clicked %s (button=%s clicks=%d)'", where, button, clicks))
end

--------------------------------------------------------------------------------
-- 3. The builder. Returns a %-format string; because the option is `%!…`, the
--    engine re-parses the result, so the %#…#, %=, %m, %% and %@…%X items below
--    are honoured. Everything dynamic is read live through vim.fn.
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
    "%@v:lua.on_name_click@",                     -- start a clickable region…
    "%#StlFile# ", where, tail, "%m ",            -- file (+ built-in modified flag)
    "%X",                                         -- …end it: the file block is clickable
    "%#StatusLine#%=",                            -- neutral spacer; %= pushes right
    [[%#StlFile# %{&fileencoding}%{&bomb?"[bom]":""} ]], -- encoding via pure %{&opt}
    "%#StlRuler# ", tostring(line), ":", tostring(col),
    "  ", tostring(pct), "%% ",                   -- %% is a literal percent sign
  })
end

vim.o.statusline = "%!v:lua.statusline()"
