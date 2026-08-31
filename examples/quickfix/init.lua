-- ~~~ bemtvi quickfix & location-list playground ~~~
--
-- The quickfix list is vim's "run a tool, get a navigable list of file:line hits,
-- step through them" workflow. bemtvi has the whole thing: the `errorformat`
-- engine, the `:copen` window, `:make`/`:grep`/`:vimgrep` producers, the 10-deep
-- list-history stack (`:colder`/`:cnewer`), and per-window LOCATION lists (every
-- `:c*` command has a window-scoped `:l*` twin). It is all exposed to Lua through
-- the canonical `btv.qf` surface (with `vim.fn.setqflist`/`getqflist` aliases).
--
-- Launch it (from the repo root, so the relative paths the fake compiler prints
-- resolve against the CWD):
--
--     BEMTVI_CONFIG=examples/quickfix \
--       cargo run -p bemtvi -- examples/quickfix/sample.c
--
-- WHAT TO TRY:
--   :make            run the fake compiler ('makeprg' below). It prints three
--                    gcc-style diagnostics against sample.c; bemtvi parses them
--                    with 'errorformat', opens the quickfix window, and jumps to
--                    the first error (line 9).
--   :cnext / :cprev  step through the errors; <CR> in the quickfix window jumps
--                    to the entry under the cursor. :copen / :cclose toggle it.
--   :grep TODO       run 'grepprg' (plain grep -n) for TODO across the file; the
--                    hits become a new quickfix list. Now you have two lists:
--   :colder          walk back to the previous list (the :make errors); :cnewer
--                    walks forward again. bemtvi keeps the last 10.
--   :vimgrep /TODO/ %   the in-process searcher (no external process — works even
--                    on the web build). `%` is the current file.
--   <leader>q  (\q)  populate + open the quickfix list from a Lua table via
--                    btv.qf (see the keymap below) — the dogfooded API.
--   :LDiag           fill this window's LOCATION list from a canned set and open
--                    it with :lopen. Location lists are per-window: split the
--                    window (<C-w>v) and each side keeps its own :lopen list.
--   :lnext / :lprev   navigate the location list; :lopen / :lclose toggle its
--                    window. Run them from the window that OWNS the list (the one
--                    :LDiag filled) — the location window itself only displays it.

local btv = btv

-- 'errorformat' for the fake compiler's gcc-style `file:line:col: kind: message`.
-- (This is also bemtvi's default efm, set explicitly here so the tour is legible.)
btv.o.errorformat = "%f:%l:%c: %t%*[^:]: %m,%f:%l:%c: %m,%f:%l: %m"

-- 'makeprg' — the build command :make runs. Our stand-in compiler prints three
-- diagnostics on stderr and exits non-zero, exactly like a failed `cc`.
btv.o.makeprg = "sh examples/quickfix/fakecc.sh"

-- 'grepprg' — :grep runs this ($* is replaced with the command's arguments).
-- The default already greps recursively; we spell it out for the tour.
btv.o.grepprg = "grep -n $* examples/quickfix/sample.c /dev/null"

-- <leader>q: build a quickfix list straight from Lua and open it. This is the
-- canonical btv.qf surface — the same one :make feeds under the hood.
vim.g.mapleader = "\\"
vim.keymap.set("n", "<leader>q", function()
  btv.qf.setqflist({
    {
      filename = "examples/quickfix/sample.c",
      lnum = 9,
      col = 18,
      text = "missing ';'",
      type = "E",
    },
    {
      filename = "examples/quickfix/sample.c",
      lnum = 14,
      col = 28,
      text = "typo: totl",
      type = "W",
    },
    {
      filename = "examples/quickfix/sample.c",
      lnum = 15,
      col = 5,
      text = "unknown function",
      type = "E",
    },
  }, " ", { title = "Hand-built list" })
  btv.qf.open()
end, { desc = "populate + open the quickfix list from Lua" })

-- :LDiag — demonstrate a per-window LOCATION list. setloclist(0, …) targets the
-- current window; :lopen shows it. (vim.diagnostic.setloclist drives this same
-- path from real LSP diagnostics.)
vim.api.nvim_create_user_command("LDiag", function()
  vim.fn.setloclist(0, {
    {
      filename = "examples/quickfix/sample.c",
      lnum = 14,
      col = 28,
      text = "this window's note",
      type = "W",
    },
    {
      filename = "examples/quickfix/sample.c",
      lnum = 15,
      col = 5,
      text = "another note",
      type = "I",
    },
  }, " ", { title = "Window-local notes" })
  vim.cmd("lopen")
end, { desc = "fill + open this window's location list" })
