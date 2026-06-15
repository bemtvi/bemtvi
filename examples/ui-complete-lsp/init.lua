-- ~~~ nxvim nx.complete: the `lsp` source + the docs sidebar (Phase 4-D) ~~~
--
-- Run it (from the repo root) — needs `lua-language-server` on your PATH:
--
--     NXVIM_CONFIG=examples/ui-complete-lsp \
--       cargo run -p nxvim -- examples/ui-complete-lsp/sample.lua
--
-- This is the completion engine (docs/specs/2026-06-14-nx-ui-float-widget.md,
-- Phase 4) driving the built-in **`lsp`** source, with the **docs sidebar** beside
-- the popup. As you navigate the completion list, a float to the right (flipping
-- left when there's no room) shows the selected item's signature (`detail`) and
-- documentation. Many servers — lua_ls and rust_analyzer especially — send docs
-- only on demand, so nxvim issues `completionItem/resolve` for the highlighted row
-- and renders the reply **server-side** (no Lua at frame time, ADR 0002 rule 4).
--
-- In insert mode:
--   <C-n> / <Tab> / <Down>   select / move down  (the docs sidebar follows)
--   <C-p> / <S-Tab> / <Up>   select / move up
--   <C-y> / <CR>             accept the highlighted row (applies its textEdit)
--   <C-e>                    dismiss the popup
--   <C-Space>                manual trigger (preselects row 0, so docs show at once)
--
-- Note: lua-language-server takes ~20s to index on first attach — completions (and
-- so the docs sidebar) are empty until it finishes warming up. Give it a moment.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- Enable completion with the `lsp` source first (priority 100, above `buffer`),
-- and the native `buffer` word-scan as a fallback. `docs = true` is the default —
-- the sidebar only renders for `lsp` rows that carry documentation, so a buffer
-- word simply shows none. Set `docs = false` to turn the sidebar off.
nx.complete.setup {
  sources = { { "lsp" }, { "buffer", min_chars = 2 } },
  min_chars = 1,
  -- docs = false,  -- uncomment to hide the docs sidebar
}

--------------------------------------------------------------------------------
-- Attach lua-language-server to `lua` buffers. `nx.lsp.config` / `nx.lsp.enable`
-- (the eventual declarative user API) isn't wired yet, so a `FileType` autocmd
-- starts the server via the raw bridge — exactly the dispatch that API will own.
vim.api.nvim_create_autocmd("FileType", {
  pattern = "lua",
  callback = function(args)
    nx._lsp_start(
      "lua_ls",                 -- a name for the server
      { "lua-language-server" }, -- the spawn argv (must be on PATH)
      vim.fn.getcwd(),          -- the root dir
      "lua",                    -- the language id
      args.buf,                 -- the buffer to bind
      nil,
      nil,
      nil
    )
  end,
})
