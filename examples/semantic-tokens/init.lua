-- ~~~ nxvim LSP semantic tokens: server-authoritative highlighting ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/semantic-tokens \
--       cargo run -p nxvim -- examples/semantic-tokens/sample.lua
--
-- Needs `lua-language-server` on your PATH (the `lua_ls` server). Install it from
-- your package manager (e.g. `brew install lua-language-server`) — without it the
-- editor still runs, it just falls back to the treesitter highlight floor.
--
-- WHAT THIS SHOWS. Treesitter colors the buffer syntactically — it can see that
-- `foo` is an identifier, but not whether it's a function, a read-only local, or
-- a parameter. The language server *knows*, and sends that classification as
-- "semantic tokens". nxvim decodes them and paints them OVER the treesitter floor
-- (at neovim's `semantic_tokens` priority, just above treesitter, below your own
-- extmarks). A server that's slow or absent simply leaves the syntactic colors
-- showing — semantic tokens only ever refine, never blank.
--
-- THE CATCH: semantic tokens paint only where your theme defines the matching
-- `@lsp.*` highlight group. With none defined, the decode still runs but every
-- token is dropped (so treesitter shows through, never a blank cell). Section 2
-- below defines a handful so you can actually see the effect.
--
-- REFRESH (Phase 2). The first request fetches the whole token set; once the
-- server returns a `resultId`, every later refresh sends `semanticTokens/full/
-- delta` quoting it, and the server ships only the *diff* — the edits are spliced
-- into nxvim's cached token array and re-decoded. It's a pure wire optimization:
-- the paint is identical, just cheaper per keystroke. A server that didn't
-- advertise delta support (or can't honor the `resultId`) transparently falls
-- back to a full set. Watch it on a DEBUG log: `NXVIM_LSP_LOG_LEVEL=debug` logs
-- `→ semanticTokens/full` on open, then `→ semanticTokens/full/delta` on edits.

--------------------------------------------------------------------------------
-- 1. CONFIGURE + ENABLE a server. (Same Phase 7 surface as examples/lsp-config.)
--    lua_ls advertises a `semanticTokensProvider`, so nxvim auto-requests the
--    whole-buffer token set on open and after every change — no extra wiring.
--------------------------------------------------------------------------------
vim.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".git" },
  -- on_attach branches on what the server advertised (Phase 3 exposes the
  -- `semanticTokensProvider` capability to Lua). lua_ls advertises it, so this
  -- echoes a confirmation; a server without it would skip the semantic wiring.
  on_attach = function(client, bufnr)
    if client.server_capabilities.semanticTokensProvider then
      vim.api.nvim_echo("semantic tokens: on for buffer " .. bufnr)
    end
  end,
})
vim.lsp.enable("lua_ls")

--------------------------------------------------------------------------------
-- 2. DEFINE the `@lsp.*` highlight groups so the tokens are visible. neovim's
--    scheme is `@lsp.type.<type>` (e.g. `@lsp.type.function`) plus, for a token
--    that carries a modifier, `@lsp.typemod.<type>.<modifier>` (e.g.
--    `@lsp.typemod.variable.readonly`). nxvim paints the most-specific group that
--    resolves, so a `typemod` link wins over its plain `type` when both exist.
--
--    Here we link them to the legacy syntax groups your colorscheme already
--    styles — link to whatever you like (or set explicit `fg`/`bg`).
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "@lsp.type.function", { link = "Function" })
vim.api.nvim_set_hl(0, "@lsp.type.method", { link = "Function" })
vim.api.nvim_set_hl(0, "@lsp.type.parameter", { link = "Identifier" })
vim.api.nvim_set_hl(0, "@lsp.type.variable", { link = "Identifier" })
vim.api.nvim_set_hl(0, "@lsp.type.property", { link = "Identifier" })
vim.api.nvim_set_hl(0, "@lsp.type.keyword", { link = "Keyword" })
vim.api.nvim_set_hl(0, "@lsp.type.comment", { link = "Comment" })
vim.api.nvim_set_hl(0, "@lsp.type.string", { link = "String" })
vim.api.nvim_set_hl(0, "@lsp.type.number", { link = "Number" })
-- A modifier example: a read-only local painted distinctly from a mutable one.
vim.api.nvim_set_hl(0, "@lsp.typemod.variable.readonly", { link = "Constant" })

-- TIP: open the sample, then turn the groups off live to watch the semantic
-- layer vanish back to the treesitter floor (and on again):
--
--     :lua vim.api.nvim_set_hl(0, "@lsp.type.function", {})
--     :lua vim.api.nvim_set_hl(0, "@lsp.type.function", { link = "Function" })

--------------------------------------------------------------------------------
-- 3. THE CONTROL SURFACE (Phase 3): `vim.lsp.semantic_tokens.*`. The projection
--    is automatic, but you can drive it by hand. These keymaps toggle, refresh,
--    and inspect the tokens for the current buffer.
--------------------------------------------------------------------------------
-- Hide / restore this buffer's semantic paint (the cache survives a stop, so the
-- restore is instant — no round-trip):
vim.keymap.set("n", "<leader>ss", vim.lsp.semantic_tokens.stop, { desc = "semantic: stop" })
vim.keymap.set("n", "<leader>sS", vim.lsp.semantic_tokens.start, { desc = "semantic: start" })

-- Force a fresh full request (drops the delta cursor and re-paints from the
-- server's current classification):
vim.keymap.set("n", "<leader>sr", function()
  vim.lsp.semantic_tokens.force_refresh(0)
end, { desc = "semantic: force refresh" })

-- Inspect the token(s) under the cursor — echoes `type` and any modifiers:
vim.keymap.set("n", "<leader>si", function()
  local toks = vim.lsp.semantic_tokens.get_at_pos(0)
  if #toks == 0 then
    vim.api.nvim_echo("no semantic token under cursor")
    return
  end
  local t = toks[1]
  local mods = table.concat(t.modifiers, ",")
  vim.api.nvim_echo("token: " .. t.type .. (mods ~= "" and (" [" .. mods .. "]") or ""))
end, { desc = "semantic: inspect token under cursor" })

-- The editor-wide gate (nxvim's switch; neovim has only the per-buffer start/stop):
--     :lua vim.lsp.semantic_tokens.enable(false)   -- hide everywhere
--     :lua vim.lsp.semantic_tokens.enable(true)    -- restore (re-requests buffers)
