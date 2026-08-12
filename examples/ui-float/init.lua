-- ~~~ bemtvi btv.ui.float playground: the list-less content float ~~~
--
-- Run it (from the repo root). For the LSP hover map (`K`) you need
-- `lua-language-server` on your PATH; the `\f` / `\F` maps work without it:
--
--     BEMTVI_CONFIG=examples/ui-float \
--       cargo run -p bemtvi -- examples/ui-float/sample.lua
--
-- `btv.ui.float(contents, opts)` is the list-LESS sibling of `btv.ui.select` —
-- both sit on the server's shared float layer (the float-widget spec, "What
-- stays out of this widget"). It renders plain content (a string, or a list of
-- line strings) in a bordered box; there is NO list, NO selection, NO input
-- grab. It is a transient popup: the NEXT key dismisses it. The SERVER owns the
-- float, its placement, and its dismissal — Lua just hands it the lines.
--
-- This is the same surface LSP hover and signature help render through natively
-- (see `btv.lsp.hover` below).
--
--   opts.border   = "none" | "single" | "rounded" | "double" | "solid"  (default "rounded")
--   opts.title    = a string drawn on the top border
--   opts.relative = "cursor" (default, anchors at the cursor) | "editor" (centered)

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. <leader>f — a cursor-anchored content float from a multi-line string.
--    TYPE:  \f          A bordered box floats by the cursor. Press any key to
--    dismiss it (it never grabs input — the key is still handled normally).
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>f", function()
  btv.ui.float(
    "btv.ui.float\n\nThe list-less content float.\nPress any key to dismiss.",
    { title = " info " }
  )
end)

--------------------------------------------------------------------------------
-- 2. <leader>F — a centered float from a list of lines, no border.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>F", function()
  btv.ui.float(
    { "centered over the editor", "", "relative = 'editor'" },
    { relative = "editor", border = "double" }
  )
end)

--------------------------------------------------------------------------------
-- 3. K — LSP hover through the content float.
--    btv.lsp.hover() requests hover for the symbol under the cursor; the
--    reply opens the float server-side (same surface as \f / \F above). Open
--    `sample.lua`, put the cursor on a stdlib symbol (e.g. `string` or `print`),
--    and press K. With no server attached it echoes "No language server attached".
--------------------------------------------------------------------------------
btv.keymap.set("n", "K", btv.lsp.hover)
-- Signature help (typically insert mode); manual trigger from normal mode here:
btv.keymap.set("n", "<leader>s", btv.lsp.signature_help)

--------------------------------------------------------------------------------
-- Attach lua-language-server to `lua` buffers so K has a server to ask, using the
-- declarative btv.lsp control surface: btv.lsp.config registers the server (cmd /
-- filetypes / root_markers), btv.lsp.enable turns it on, and the engine starts it on
-- the first `lua` buffer. (lua-language-server takes ~20s to index on first attach;
-- hover is empty until it warms up.)
btv.lsp.config("lua_ls", {
  cmd = { "lua-language-server" },
  filetypes = { "lua" },
  root_markers = { ".luarc.json", ".git" },
})
btv.lsp.enable("lua_ls")
