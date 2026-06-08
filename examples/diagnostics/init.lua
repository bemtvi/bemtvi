-- ~~~ nxvim diagnostic display surfaces: vim.diagnostic.config ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/diagnostics \
--       cargo run -p nxvim -- examples/diagnostics/sample.lua
--
-- Needs `lua-language-server` on your PATH (the `lua_ls` server). Install it from
-- your package manager (e.g. `brew install lua-language-server`) — without it the
-- editor still runs, it just won't start a server, so you'll see no diagnostics.
--
-- nxvim renders diagnostics several ways, each toggled by `vim.diagnostic.config`:
--
--   * underline  — the squiggle under the offending span (on by default)
--   * signs      — a severity glyph in a gutter sign column, on the offending
--     line (on by default, matching neovim 0.10)
--   * virtual_text — the message printed inline after the line (this file turns
--     it ON; it's off by default, matching neovim 0.10)
--   * the message line — the highest-severity diagnostic under the cursor is
--     echoed on the command line as you move (always on)
--
-- Everything below is yours to change.

--------------------------------------------------------------------------------
-- 1. CONFIGURE + ENABLE a server, so a buffer actually gets diagnostics. (Same
--    Phase 7 surface as examples/lsp-config — see that file for the full tour.)
--------------------------------------------------------------------------------
vim.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".git" },
})
vim.lsp.enable("lua_ls")

--------------------------------------------------------------------------------
-- 2. TURN ON INLINE VIRTUAL TEXT. `virtual_text = true` prints each line's most
--    severe diagnostic after its end-of-text, colored by severity. Pass a table
--    to set the leader glyph:
--
--        vim.diagnostic.config({ virtual_text = true })            -- default "■ "
--        vim.diagnostic.config({ virtual_text = { prefix = "» " } }) -- custom
--
--    `underline = false` would hide the squiggles; left on here so you can see
--    every surface at once.
--
--    SIGNS are on by default: each diagnostic line gets a severity glyph in a
--    2-cell gutter column to the left of the line numbers (default E/W/I/H). Pass
--    a `text` map to override the glyphs per severity, or `signs = false` to drop
--    the column entirely:
--
--        vim.diagnostic.config({ signs = false })  -- no sign column
--        vim.diagnostic.config({ signs = {         -- custom glyphs
--          text = { [vim.diagnostic.severity.ERROR] = "✘",
--                   [vim.diagnostic.severity.WARN]  = "▲" } } })
--------------------------------------------------------------------------------
vim.diagnostic.config({
  virtual_text = { prefix = "■ " },
  underline = true,
  signs = {
    text = {
      [vim.diagnostic.severity.ERROR] = "✘",
      [vim.diagnostic.severity.WARN] = "▲",
      [vim.diagnostic.severity.INFO] = "»",
      [vim.diagnostic.severity.HINT] = "›",
    },
  },
})

--------------------------------------------------------------------------------
-- 3. NAVIGATE. These ride publishDiagnostics, so they need no server request.
--      ]d  /  [d   -> jump to the next / previous diagnostic
--      <Space>e    -> open the full diagnostics list in the panel (<CR> jumps)
--      <Space>d    -> float the cursor line's diagnostics in full (source/code,
--                     multi-line messages the inline virtual text truncates)
--------------------------------------------------------------------------------
vim.g.mapleader = " "
vim.keymap.set("n", "]d", function() vim.diagnostic.goto_next() end)
vim.keymap.set("n", "[d", function() vim.diagnostic.goto_prev() end)
vim.keymap.set("n", "<leader>e", function() vim.diagnostic.setloclist() end)
vim.keymap.set("n", "<leader>d", function() vim.diagnostic.open_float() end)

-- TIP: toggle the inline text live from the command line while editing:
--     :lua vim.diagnostic.config({ virtual_text = not vim.diagnostic.config().virtual_text })
