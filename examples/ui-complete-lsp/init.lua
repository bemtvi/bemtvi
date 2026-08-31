-- ~~~ bemtvi btv.complete: the `lsp` source + the docs sidebar (Phase 4-D) ~~~
--
-- Run it (from the repo root) — needs `lua-language-server` on your PATH:
--
--     BEMTVI_CONFIG=examples/ui-complete-lsp \
--       cargo run -p bemtvi -- examples/ui-complete-lsp/sample.lua
--
-- This is the completion engine (docs/specs/2026-06-14-btv-ui-float-widget.md,
-- Phase 4) driving the built-in **`lsp`** source, with the **docs float** beside the
-- popup — a real, non-focusable float window (the same model LSP hover uses), so the
-- docs are syntax-highlighted (the signature is fenced in the buffer's language) and
-- scroll with the mouse wheel. As you navigate the list, the float to the right
-- (flipping left when there's no room) shows the selected item's signature (`detail`)
-- and documentation. Many servers — lua_ls and rust_analyzer especially — send docs
-- only on demand, so bemtvi issues `completionItem/resolve` for the highlighted row
-- and renders the reply **server-side** (no Lua at frame time, ADR 0002 rule 4).
--
-- In insert mode:
--   <C-n> / <Tab> / <Down>   select / move down  (the docs sidebar follows)
--   <C-p> / <S-Tab> / <Up>   select / move up
--   <C-y> / <CR>             accept the highlighted row (applies its textEdit)
--   <C-e>                    dismiss the popup
--   <C-Space>                manual trigger (preselects row 0, so docs show at once)
--   <C-k>                    signature help for the call under the cursor — type
--                            `print(` and press <C-k> to float the parameter hints
--
-- Note: lua-language-server takes ~20s to index on first attach — completions (and
-- so the docs sidebar) are empty until it finishes warming up. Give it a moment.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- Enable completion with the `lsp` source first (priority 100, above `buffer`),
-- and the native `buffer` word-scan as a fallback. `docs = true` is the default —
-- the docs float only renders for `lsp` rows that carry documentation, so a buffer
-- word simply shows none. Set `docs = false` to turn it off. `docs_wrap` (default
-- true) wraps a long doc line within the float instead of truncating it.
btv.complete.setup({
  sources = { { "lsp" }, { "buffer", min_chars = 2 } },
  min_chars = 1,
  -- docs = false,       -- uncomment to hide the docs float
  -- docs_wrap = false,  -- uncomment to truncate long doc lines instead of wrapping
})

--------------------------------------------------------------------------------
-- Attach lua-language-server to `lua` buffers via the declarative btv.lsp control
-- surface: btv.lsp.config registers the server, btv.lsp.enable activates it, and the
-- engine starts it on the first `lua` buffer (resolving the root upward from the
-- file through root_markers). on_attach runs once the server has bound the buffer —
-- the place to set buffer-local LSP keymaps.
btv.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".luarc.jsonc", ".git" },
  on_attach = function(_client, bufnr)
    local function map(lhs, fn)
      btv.keymap.set("n", lhs, fn, { buffer = bufnr })
    end
    -- Go-to / references / symbols all open in btv.picker (with the "location"
    -- preview) when there's more than one hit; a single definition jumps straight.
    map("gd", btv.lsp.definition)
    map("gr", btv.lsp.references)
    map("gO", btv.lsp.document_symbol) -- this file's symbols, in the picker
    map("<leader>ws", btv.lsp.workspace_symbol) -- prompt → workspace/symbol picker
    map("K", btv.lsp.hover)
    map("<leader>rn", btv.lsp.rename)
    map("<leader>ca", btv.lsp.code_action)
    -- Signature help in INSERT mode: type a call like `print(` and press <C-k> to
    -- float the parameter hints (the active parameter is shown in brackets). This
    -- matches the built-in default; setting it here keeps the example self-contained.
    btv.keymap.set("i", "<C-k>", btv.lsp.signature_help, { buffer = bufnr })
    -- Inlay hints are off by default — turn them on for this buffer (the engine
    -- requests them and paints the type/parameter annotations inline).
    btv.lsp.inlay_hint.enable(true, { bufnr = bufnr })
    -- Toggle them with <leader>ih.
    map("<leader>ih", function()
      btv.lsp.inlay_hint.enable(
        not btv.lsp.inlay_hint.is_enabled({ bufnr = bufnr }),
        { bufnr = bufnr }
      )
    end)
  end,
})
btv.lsp.enable("lua_ls")
