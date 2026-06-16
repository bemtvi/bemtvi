-- ~~~ nxvim virtual-text & virtual-lines playground ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/virt-text \
--       cargo run -p nxvim -- examples/virt-text/sample.txt
--
-- Extmark virtual text/lines is nxvim's own feature, driven through the neovim
-- compat surface (`nvim_create_namespace` + `nvim_buf_set_extmark`). An extmark's
-- `decoration` table can carry:
--   * virt_text          inline / eol / overlay / right_align / win_col text on a line
--   * virt_lines         whole extra screen rows drawn above or below a line
-- Both reach the renderer end to end; this config drops one of each on the sample
-- buffer so you can see them in the TUI.
--
-- TRY IT interactively:
--   move around (hjkl, w, G)        the cursor steps OVER the virtual rows, never onto them
--   G  (jump to last line)          the view scrolls; the cursor stays visible even though
--                                   the virt_lines block ate extra screen rows (Phase 5 scroll math)
--   V  on the eol-note line         a `virt_text_hide` note vanishes while the line is selected;
--                                   <Esc> brings it back (the sibling note without the flag stays)
--   :set number                     gutter numbers skip the virtual rows (they have no buffer line)

--------------------------------------------------------------------------------
-- 1. Highlight groups for the virtual chunks, so colours show even with no
--    colourscheme loaded. (A real config would reuse its theme's groups.)
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "VtEol", { fg = "#565f89", italic = true })
vim.api.nvim_set_hl(0, "VtInline", { fg = "#1a1b26", bg = "#e0af68", bold = true })
vim.api.nvim_set_hl(0, "VtOverlay", { fg = "#1a1b26", bg = "#f7768e", bold = true })
vim.api.nvim_set_hl(0, "VtRight", { fg = "#1a1b26", bg = "#9ece6a", bold = true })
vim.api.nvim_set_hl(0, "VtWinCol", { fg = "#7dcfff" })
vim.api.nvim_set_hl(0, "VtLineHdr", { fg = "#bb9af7", bold = true })
vim.api.nvim_set_hl(0, "VtLineBody", { fg = "#565f89" })

local ns = vim.api.nvim_create_namespace("virt_text_demo")

--------------------------------------------------------------------------------
-- 2. Decorate the sample buffer once it is on screen. The extmarks are anchored
--    to specific (0-based) rows of sample.txt; they ride edits and undo like any
--    extmark. Guarded to the demo file so re-entering another buffer won't re-tag.
--------------------------------------------------------------------------------
local function decorate(buf)
  -- eol: a note after the line's last character.
  vim.api.nvim_buf_set_extmark(buf, ns, 2, 0, {
    virt_text = { { "  ← end-of-line note", "VtEol" } },
    virt_text_pos = "eol",
  })
  -- eol + virt_text_hide: this note disappears while the line is visually selected
  -- (try `V` on row 3), then returns on <Esc>. The plain note above always stays.
  vim.api.nvim_buf_set_extmark(buf, ns, 2, 0, {
    virt_text = { { "  (hides under selection)", "VtEol" } },
    virt_text_pos = "eol",
    virt_text_hide = true,
  })

  -- inline: spliced into the line, pushing the real text (and the cursor) right.
  -- Byte col 15 is just before the word "spliced" on row 3.
  vim.api.nvim_buf_set_extmark(buf, ns, 3, 15, {
    virt_text = { { "[INLINE]", "VtInline" } },
    virt_text_pos = "inline",
  })

  -- overlay: painted over the cells starting at the anchor column (no shift).
  -- "OVERLAY" begins at byte col 9 on row 4.
  vim.api.nvim_buf_set_extmark(buf, ns, 4, 9, {
    virt_text = { { "≈≈covered≈≈", "VtOverlay" } },
    virt_text_pos = "overlay",
  })

  -- right_align: flushed to the window's right edge.
  vim.api.nvim_buf_set_extmark(buf, ns, 5, 0, {
    virt_text = { { " right-aligned ", "VtRight" } },
    virt_text_pos = "right_align",
  })
  -- win_col: pinned to a fixed window column (here 50), independent of the anchor.
  vim.api.nvim_buf_set_extmark(buf, ns, 5, 0, {
    virt_text = { { "│col50", "VtWinCol" } },
    virt_text_win_col = 50,
  })

  -- virt_lines: whole extra rows. One ABOVE the `def compute…` line acting as a
  -- fold-style header, and two BELOW it as an annotation. They interleave into the
  -- window's rows — no gutter number, the cursor steps over them.
  vim.api.nvim_buf_set_extmark(buf, ns, 7, 0, {
    virt_lines = { { { "  ┌─ compute(): doubles and offsets ─┐", "VtLineHdr" } } },
    virt_lines_above = true,
  })
  vim.api.nvim_buf_set_extmark(buf, ns, 8, 0, {
    virt_lines = {
      { { "  └ note: pure, no side effects", "VtLineBody" } },
      { { "    used by the demo harness", "VtLineBody" } },
    },
  })
end

vim.api.nvim_create_autocmd({ "BufWinEnter", "BufReadPost" }, {
  callback = function(args)
    local name = vim.fn.fnamemodify(vim.fn.expand("%"), ":t")
    if name == "sample.txt" then
      decorate(args.buf or 0)
    end
  end,
})
