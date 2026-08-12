-- ~~~ bemtvi btv.ui.open playground: hand a path / URL to the OS opener ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/ui-open \
--       cargo run -p bemtvi -- examples/ui-open/sample.txt
--
-- `btv.ui.open(uri)` hands a file path or a URL to the platform opener — `open`
-- on macOS, `explorer` on Windows, `xdg-open` elsewhere — and runs it OFF-TICK.
-- It is PROMISE-ONLY (ADR 0002): the call returns at once with a promise of the
-- opener's exit result `{ code, stdout, stderr }` (the `btv.run` shape). Like
-- `btv.run` it RESOLVES rather than rejects — a missing opener is `code = -1`, a
-- non-zero opener exit is that code — so you decide what a failure means with
-- `:next(fn)` / `:catch(fn)`. The neovim muscle-memory alias `vim.ui.open(path)`
-- drives the same path (it returns the promise in place of neovim's SystemObj).

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. <leader>o — open a URL in your browser.
--    TYPE:  \o      The platform opener launches https://bemtvi.dev in your
--    default browser. (Nothing appears in the editor — the opener is a separate
--    process; this echoes a confirmation once it has launched.)
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>o", function()
  btv.ui.open("https://bemtvi.dev"):next(function(r)
    if r.code == 0 then
      btv.notify("opened https://bemtvi.dev")
    else
      btv.notify("could not open the URL (opener exit " .. r.code .. ")", "warn")
    end
  end)
end)

--------------------------------------------------------------------------------
-- 2. <leader>O — open the file under the cursor (gx-style).
--    TYPE:  \O      Put the cursor on the path/URL on the first line of
--    sample.txt and press \O; the word under the cursor is handed to the opener.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>O", function()
  local target = vim.fn.expand("<cWORD>")
  btv.ui.open(target)
end)

--------------------------------------------------------------------------------
-- 3. <leader>v — vim.ui.open: the neovim muscle-memory alias (same behavior).
--    Plugins that call `vim.ui.open(url)` (link-openers, LSP showDocument) run
--    unchanged through this.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>v", function()
  vim.ui.open("https://github.com")
end)
