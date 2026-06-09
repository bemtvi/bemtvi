-- ~~~ nxvim LSP inlay hints: inline type / parameter annotations ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/inlay-hints \
--       cargo run -p nxvim -- examples/inlay-hints/sample.lua
--
-- Needs `lua-language-server` on your PATH (the `lua_ls` server). Install it from
-- your package manager (e.g. `brew install lua-language-server`) — without it the
-- editor still runs, it just shows no hints (there's no server to ask).
--
-- WHAT THIS SHOWS. Inlay hints are the dim inline annotations a language server
-- injects *between* your code's own glyphs — a `: string` after a `local` whose
-- type isn't obvious, a `name:` before a call argument — that aren't in the file
-- but help you read it. nxvim requests them from the server, decodes them, and
-- paints each one INLINE at its column, pushing the real text (and the cursor) to
-- the right. Unlike semantic tokens, they are OPT-IN: nothing shows until you call
-- `vim.lsp.inlay_hint.enable(true)` (section 2 turns them on at attach).
--
-- (lua_ls only emits a `: type` hint where the type isn't already obvious — it
-- skips plain literals like `local x = 6.28`. The sample is written so you see
-- both a real `: string` type hint and the `name:` parameter hints.)
--
-- THE CATCH: a hint paints in the `LspInlayHint` highlight group; with none
-- defined it falls back to a dim gray. Section 3 links it so it matches your
-- theme. (lua_ls needs its hint settings on — section 1 enables them.)

--------------------------------------------------------------------------------
-- 1. CONFIGURE + ENABLE a server, asking it to PRODUCE hints. Most servers gate
--    inlay hints behind settings; lua_ls needs `hint.enable = true`. (This is the
--    server-side "make hints"; section 2 is the editor-side "show hints".)
--------------------------------------------------------------------------------
vim.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".git" },
  settings = {
    Lua = {
      hint = {
        enable = true,
        setType = true, -- `: T` type hints on locals/returns
        paramName = "All", -- `name:` hints before call arguments
        arrayIndex = "Enable",
      },
    },
  },
  -- Turn the editor-side projection on, but ONLY when the server actually offers
  -- inlay hints — `client.server_capabilities.inlayHintProvider` reads truthy once
  -- it advertised the feature (Phase 2 exposes the cap to Lua, exactly like the
  -- `hoverProvider` an on_attach branches `K` on). A server without it is skipped.
  on_attach = function(client, bufnr)
    if client.server_capabilities.inlayHintProvider then
      vim.lsp.inlay_hint.enable(true, { bufnr = bufnr })
      vim.api.nvim_echo("inlay hints: on for buffer " .. bufnr)
    else
      vim.api.nvim_echo("inlay hints: " .. client.name .. " offers none")
    end
  end,
})
vim.lsp.enable("lua_ls")

vim.g.mapleader = ' '
--------------------------------------------------------------------------------
-- 2. THE CONTROL SURFACE: `vim.lsp.inlay_hint.*`. Inlay hints are opt-in, so the
--    headline call is `enable`. A keymap to toggle them on the current buffer:
--------------------------------------------------------------------------------
vim.keymap.set("n", "<leader>ih", function()
  local on = vim.lsp.inlay_hint.is_enabled({ bufnr = 0 })
  vim.lsp.inlay_hint.enable(not on, { bufnr = 0 })
  vim.api.nvim_echo("inlay hints: " .. (on and "off" or "on"))
end, { desc = "inlay hints: toggle" })

-- READ the hints back from Lua with `get` (the Phase 2 read surface). `<leader>ic`
-- counts how many hints sit on the cursor line and shows the first — proof the
-- cache is queryable, not just paintable. `filter.range` narrows to a span; here
-- we ask for the whole cursor line (0-based).
vim.keymap.set("n", "<leader>ic", function()
  local row = vim.api.nvim_win_get_cursor(0)[1] - 1
  local hints = vim.lsp.inlay_hint.get({
    bufnr = 0,
    range = { start = { line = row, character = 0 }, ["end"] = { line = row, character = 9999 } },
  })
  if #hints == 0 then
    vim.api.nvim_echo("inlay hints: none on line " .. (row + 1))
  else
    vim.api.nvim_echo(#hints .. " hint(s) on line " .. (row + 1) .. ": " .. hints[1].inlay_hint.label)
  end
end, { desc = "inlay hints: get on cursor line" })

--------------------------------------------------------------------------------
-- 3. STYLE the hints. nxvim paints them in the `LspInlayHint` group; link it to
--    whatever your theme dims comments with (or set an explicit `fg`). With none
--    defined the built-in dim gray is used.
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "LspInlayHint", { link = "Comment" })

-- TIP: open the sample, toggle with `<leader>ih`, and watch the `: string` and
-- `name:` annotations splice in between your code — the real text shifts right to
-- make room, exactly like neovim's `vim.lsp.inlay_hint.enable`. Put the cursor on
-- the `local label = …` line and hit `<leader>ic` to read the same hints back
-- through `get`.
