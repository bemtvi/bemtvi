-- ~~~ nxvim format-on-save: BufWritePre fires (and awaits) before the bytes ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/format-on-save \
--       cargo run -p nxvim -- examples/format-on-save/sample.txt
--
-- vim's `BufWritePre` fires *before* the buffer is written to disk, so a handler
-- may mutate the buffer and the mutation is what gets saved. nxvim honors that —
-- and goes one step further: a `BufWritePre` handler may be **async** (return a
-- promise), and the write *waits* for it to settle before serializing. That makes
-- an async formatter (e.g. `nx.lsp.buf.format()`) usable for format-on-save.
--
-- Buffer text is mutated the vim way — `vim.cmd` running an ex-command — since the
-- Lua `nvim_*` surface is read-only. nxvim's `:s` has no `e` flag, so each handler
-- first checks (reading the buffer mirror) whether there's anything to change and
-- only substitutes when there is — a no-op save stays quiet.

-- Run `excmd` only if some line matches the Lua pattern `needle` — the guard that
-- replaces vim's `:s///e` "no error if no match".
local function sub_if(buf, needle, excmd)
  for _, line in ipairs(vim.api.nvim_buf_get_lines(buf, 0, -1, false)) do
    if line:find(needle) then
      vim.cmd(excmd)
      return
    end
  end
end

-- ~~~ 1. Synchronous trim-trailing-whitespace ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
--
-- The classic format-on-save. Because `BufWritePre` runs *before* the write, the
-- trimmed text is what lands on disk.
--
-- TYPE:  add trailing spaces to a line, then `:w`
-- SEE:   the trailing spaces are gone in the buffer AND on disk (`:e` re-reads it)
vim.api.nvim_create_autocmd("BufWritePre", {
  pattern = "*.txt",
  callback = function()
    -- `\s*$` matches the (possibly empty) run of trailing whitespace on every line,
    -- so it always matches — no "pattern not found", no guard needed.
    vim.cmd([[%s/\s*$//]])
  end,
})

-- ~~~ 2. A second synchronous "formatter" (handlers compose) ~~~~~~~~~~~~~~~~~~~~
--
-- Upcase a leading `todo:` into `TODO:`. Both this and the trim above run before
-- the bytes, so both mutations are saved.
--
-- TYPE:  add a line `todo: wire the thing`, then `:w`
-- SEE:   it becomes `TODO: wire the thing` on save
vim.api.nvim_create_autocmd("BufWritePre", {
  pattern = "*.txt",
  callback = function(args)
    sub_if(args.buf, "^todo:", [[%s/todo:/TODO:/]])
  end,
})

-- ~~~ 3. ASYNC format-on-save (the write awaits the promise) ~~~~~~~~~~~~~~~~~~~~
--
-- The handler returns a promise; nxvim holds the write until it settles, so an
-- async formatter's edits still make it into the saved file. Here we simulate a
-- formatter that takes ~50ms (a real one would be `nx.lsp.buf.format()`): after the
-- delay it rewrites `FIXME` to `TODO`.
--
-- The point: `:w` does not write the un-formatted bytes and format afterwards — it
-- waits, formats, then writes the formatted bytes. A rejecting formatter would NOT
-- block the save (the write still lands).
--
-- TYPE:  add a line `FIXME later`, then `:w`
-- SEE:   after a brief pause it becomes `TODO later`, and that is what is saved
vim.api.nvim_create_autocmd("BufWritePre", {
  pattern = "*.txt",
  callback = function(args)
    return nx.promise.delay(50):next(function()
      sub_if(args.buf, "FIXME", [[%s/FIXME/TODO/g]])
    end)
  end,
})

-- ~~~ 4. BufWritePost: react after the bytes are on disk ~~~~~~~~~~~~~~~~~~~~~~~~
--
-- The companion event, fired *after* the write — for "reload the affected tool"
-- side effects. Here we just echo a confirmation with the saved line count.
--
-- TYPE:  `:w`
-- SEE:   a `saved <name> (<n> lines)` message on the message line
vim.api.nvim_create_autocmd("BufWritePost", {
  pattern = "*.txt",
  callback = function(args)
    local n = #vim.api.nvim_buf_get_lines(args.buf, 0, -1, false)
    vim.notify(("saved %s (%d lines)"):format(args.file, n))
  end,
})
