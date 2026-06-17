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
-- Attach lua-language-server to `lua` buffers via the declarative nx.lsp control
-- surface: nx.lsp.config registers the server, nx.lsp.enable activates it, and the
-- engine starts it on the first `lua` buffer (resolving the root upward from the
-- file through root_markers). on_attach runs once the server has bound the buffer —
-- the place to set buffer-local LSP keymaps.
nx.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".luarc.jsonc", ".git" },
  on_attach = function(_client, bufnr)
    local function map(lhs, fn) nx.keymap.set("n", lhs, fn, { buffer = bufnr }) end
    -- Go-to / references / symbols all open in nx.picker (with the "location"
    -- preview) when there's more than one hit; a single definition jumps straight.
    map("gd", nx.lsp.definition)
    map("gr", nx.lsp.references)
    map("gO", nx.lsp.document_symbol) -- this file's symbols, in the picker
    map("<leader>ws", nx.lsp.workspace_symbol) -- prompt → workspace/symbol picker
    map("K", nx.lsp.hover)
    map("<leader>rn", nx.lsp.rename)
    map("<leader>ca", nx.lsp.code_action)
    -- Inlay hints are off by default — turn them on for this buffer (the engine
    -- requests them and paints the type/parameter annotations inline).
    nx.lsp.inlay_hint.enable(true, { bufnr = bufnr })
    -- Toggle them with <leader>ih.
    map("<leader>ih", function()
      nx.lsp.inlay_hint.enable(not nx.lsp.inlay_hint.is_enabled({ bufnr = bufnr }), { bufnr = bufnr })
    end)
  end,
})
nx.lsp.enable("lua_ls")
