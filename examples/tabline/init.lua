-- ~~~ nxvim tabline playground: a custom 'tabline' built from vim.fn ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/tabline \
--       cargo run -p nxvim -- examples/tabline/sample.txt
--
-- The tabline is driven by the 'tabline' option's %-format engine — the SAME
-- engine as 'statusline', plus the %T / %X tab click-region items. This config
-- sets `tabline = '%!v:lua.require("myutils").my_tab_line()'`, so on every redraw
-- the engine calls myutils.my_tab_line() and re-parses what it returns. That
-- result assembles the whole line in Lua: one %#TabLine(Sel)#-coloured, %nT-tagged
-- label per tab (each label a %{} expression calling my_tab_label, which reads the
-- tab via vim.fn.tabpagebuflist / tabpagenr / bufname and vim.bo[n].modified), a
-- %#TabLineFill# spacer, and a right-aligned %999X "close" region.
--
-- The actual label/line code lives in lua/myutils.lua (the tabline subset of a
-- real ~/.config/nvim config, copied verbatim).
--
-- TRY IT interactively:
--   :tabedit examples/tabs/sample.txt   a second tab appears; the tabline lists both
--   gt / gT                             switch tabs — the active label recolours (TabLineSel)
--   edit a tab's buffer (i…<Esc>)       that tab's label gains a `*` modified marker
--   :tabclose                           back to one tab — showtabline=1 hides the line
--   :set tabline=                       fall back to nxvim's built-in tab cells

--------------------------------------------------------------------------------
-- Tabline highlight groups. `%#TabLineSel#` (active tab), `%#TabLine#` (inactive),
-- and `%#TabLineFill#` (the strip past the last label) in the returned format
-- string resolve against these. A real config gets them from its colourscheme;
-- hard-coded here to be runnable standalone.
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "TabLineSel", { fg = "#1a1b26", bg = "#7aa2f7", bold = true })
vim.api.nvim_set_hl(0, "TabLine", { fg = "#c0caf5", bg = "#3b4261" })
vim.api.nvim_set_hl(0, "TabLineFill", { fg = "#565f89", bg = "#1a1b26" })

-- Always show the tabline, even with a single tab, so the playground has
-- something to look at on first launch (vim's default `showtabline=1` would hide
-- it until a second tab exists).
vim.o.showtabline = 2

-- Wire the custom tabline. The %! form makes the engine treat the Lua result as
-- the whole line and re-parse it (so the %#…#, %nT, %=, %999X items inside are
-- honoured); `require('myutils')` resolves lua/myutils.lua on the runtimepath.
vim.o.tabline = "%!v:lua.require('myutils').my_tab_line()"
