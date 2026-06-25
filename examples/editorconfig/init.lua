-- ~~~ nxvim .editorconfig support ~~~
--
-- nxvim reads `.editorconfig` files out of the box — there is nothing to enable
-- in this init.lua. It is bundled, on by default, and mirrors neovim's surface.
-- This file only demonstrates the toggles and how to inspect what was resolved.
--
-- Run it (from the repo root) against the sample files:
--
--     NXVIM_CONFIG=examples/editorconfig \
--       cargo run -p nxvim -- examples/editorconfig/app.py
--
-- On open, nxvim walks up from the file, reads every `.editorconfig` (stopping at
-- one with `root = true`), matches the path against each `[glob]` section, and
-- applies the merged properties to the buffer's options:
--
--     indent_style -> expandtab        indent_size -> shiftwidth / softtabstop
--     tab_width    -> tabstop          end_of_line -> fileformat (lf/crlf/cr)
--     charset      -> fileencoding (+ bomb for utf-8-bom)
--
-- The neighbouring `.editorconfig` gives `app.py` 4-space indent, `mod.lua`
-- 2-space indent, and `Makefile` real 8-wide tabs — all from one project file.

-- The toggle, exactly like neovim:
--
--   vim.g.editorconfig = false           -- turn it off globally (default: true)
--   vim.b.editorconfig = false           -- turn it off for the current buffer
--   vim.b[bufnr].editorconfig = false    -- ...or a specific buffer
--
-- A buffer's explicit value wins over the global one. For example, opt one
-- filetype out while leaving the rest on:
vim.api.nvim_create_autocmd("FileType", {
  pattern = "markdown",
  callback = function(args)
    -- Prose: let your own settings win, not the project's .editorconfig.
    vim.b[args.buf].editorconfig = false
  end,
})

-- Inspect the resolved properties for a buffer (handy for debugging a project's
-- rules, and the way to reach properties with no nxvim option, e.g.
-- trim_trailing_whitespace / max_line_length):
vim.api.nvim_create_user_command("EditorConfigShow", function()
  local props = nx.editorconfig.properties(0)
  if not props then
    vim.notify("no .editorconfig resolved for this buffer")
    return
  end
  local parts = {}
  for k, v in pairs(props) do
    parts[#parts + 1] = k .. " = " .. tostring(v)
  end
  table.sort(parts)
  vim.notify("editorconfig: " .. table.concat(parts, ", "))
end, {})

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. Open `app.py` (matches `[*]`): `i<Tab>x<Esc>` inserts FOUR spaces.
--    Run `:EditorConfigShow` -> indent_size = 4, indent_style = space, ...
--
-- 2. Open `mod.lua` (`:e examples/editorconfig/mod.lua`): the `[*.lua]` section
--    narrows indent_size to 2, so `i<Tab>x<Esc>` inserts TWO spaces.
--
-- 3. Open `Makefile` (`:e examples/editorconfig/Makefile`): `[{Makefile,*.mk}]`
--    sets real tabs 8 cells wide -> `i<Tab>x<Esc>` inserts a literal "\t".
--
-- 4. Toggle it off and reload to see the defaults come back:
--      :lua vim.g.editorconfig = false
--      :e! examples/editorconfig/app.py     -> indent is no longer forced.
--------------------------------------------------------------------------------
