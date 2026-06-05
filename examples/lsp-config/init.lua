-- ~~~ nxvim LSP playground: vim.lsp.config / enable + on_attach (Phase 7) ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/lsp-config \
--       cargo run -p nxvim -- examples/lsp-config/sample.lua
--
-- Needs `lua-language-server` on your PATH (the `lua_ls` server). Install it from
-- your package manager (e.g. `brew install lua-language-server`) — without it the
-- editor still runs, it just won't start a server.
--
-- This is the whole Phase 7 surface in one file: a server is configured and
-- enabled in Lua (7a), and `on_attach` wires the editing features through the
-- `vim.lsp.buf.*` / `vim.diagnostic.*` Lua entry points (7b). Nothing is built in
-- — every binding below is yours to change.

--------------------------------------------------------------------------------
-- 1. CONFIGURE THE SERVER. `vim.lsp.config(name, opts)` registers a server by a
--    name you choose. `cmd` is its argv, `filetypes` says which buffers it owns,
--    and `root_markers` is the upward file search that picks the workspace root.
--------------------------------------------------------------------------------
vim.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".git" },
})

--------------------------------------------------------------------------------
-- 2. on_attach: RUNS ONCE PER BUFFER when its server attaches (the editor fires
--    `LspAttach` right after the document opens). This is where you set the LSP
--    keymaps — buffer-local, so they only exist where a server is actually
--    attached. `client.server_capabilities` lets you skip a map the server can't
--    serve. The maps below drive the `vim.lsp.buf.*` Lua entry points.
--
--    With sample.lua open, TRY:
--      gd            -> jump to the definition under the cursor
--      gr            -> list references in the panel (<CR> jumps, q closes)
--      K             -> hover docs for the symbol under the cursor (panel)
--      <Space>rn     -> rename the symbol everywhere (type a new name, <CR>)
--      <Space>f      -> format the buffer
--      <Space>ca     -> list code actions in the panel (<CR> applies)
--      ]d  /  [d     -> jump to the next / previous diagnostic
--      <Space>e      -> open the diagnostics list in the panel
--------------------------------------------------------------------------------
local function on_attach(client, bufnr)
  local function map(lhs, rhs)
    vim.keymap.set("n", lhs, rhs, { buffer = bufnr })
  end

  -- Navigation: only map what the server advertises (capability-gated).
  local caps = client.server_capabilities
  if caps.definitionProvider then map("gd", vim.lsp.buf.definition) end
  if caps.referencesProvider then map("gr", vim.lsp.buf.references) end
  if caps.hoverProvider then map("K", vim.lsp.buf.hover) end
  if caps.renameProvider then
    map("<leader>rn", function() vim.lsp.buf.rename("new_name") end)
  end
  if caps.documentFormattingProvider then
    map("<leader>f", function() vim.lsp.buf.format() end)
  end
  if caps.codeActionProvider then
    map("<leader>ca", function() vim.lsp.buf.code_action() end)
  end

  -- Diagnostics: always available (they ride publishDiagnostics, not a request).
  map("]d", function() vim.diagnostic.goto_next() end)
  map("[d", function() vim.diagnostic.goto_prev() end)
  map("<leader>e", function() vim.diagnostic.setloclist() end)

  print("lua_ls attached to buffer " .. bufnr .. " (client #" .. client.id .. ")")
end

-- Attach the hook to the config (merges over the base above).
vim.lsp.config("lua_ls", { on_attach = on_attach })

--------------------------------------------------------------------------------
-- 3. ENABLE. `vim.lsp.enable(name)` turns the config on: the server starts for
--    the current buffer (and any future one) whose filetype it owns.
--------------------------------------------------------------------------------
vim.lsp.enable("lua_ls")

-- A leader you can feel: <Space> drives the <leader> maps above.
vim.g.mapleader = " "
