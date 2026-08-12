-- ~~~ bemtvi silent commands: the `:silent[!]` modifier and `btv.cmd`'s mods ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/silent-commands \
--       cargo run -p bemtvi -- examples/silent-commands/sample.txt
--
-- A command's chatter belongs to whoever typed it — not to a keymap or a plugin
-- running it on your behalf. vim's answer is the `:silent` modifier, and bemtvi
-- honors both halves of it:
--
--   :silent  {cmd}   suppress {cmd}'s ordinary output — ERRORS STILL SHOW
--   :silent! {cmd}   suppress its errors too (the "run it if you can" form)
--
-- The split matters: a bare `:silent` that also ate errors would turn a broken
-- mapping into a mapping that does nothing, quietly. Vim switches messages back
-- on the moment an error fires, and so does bemtvi.
--
-- From Lua the same modifier is a table argument rather than a string prefix:
--
--   btv.cmd("write", { silent = true })                     -- :silent write
--   btv.cmd("Foo",   { silent = true, emsg_silent = true })  -- :silent! Foo
--
-- `vim.cmd` (the muscle-memory alias) takes it in every form it accepts —
-- `vim.cmd(str, opts)`, `vim.cmd{ cmd = …, mods = … }`, `vim.cmd.write{ mods = … }`
-- — all of which compile down to the very same ex modifier. A `mods` key bemtvi
-- doesn't dispatch raises by name instead of being quietly dropped.

-- ~~~ 1. The modifier, typed ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
--
-- TYPE:  :echom 'hello'          SEE:  `hello` on the message line
-- TYPE:  :silent echom 'hello'   SEE:  nothing — and `:messages` has no entry
--                                      for it either (vim drops it, not just the
--                                      line)
-- TYPE:  :silent NotACommand     SEE:  E492 still reported — errors survive
-- TYPE:  :silent! NotACommand    SEE:  nothing at all

-- ~~~ 2. A keymap that saves without the "written" chatter ~~~~~~~~~~~~~~~~~~~~~
--
-- `btv.cmd(cmd, opts)` is the canonical Lua funnel; `opts` carries the modifiers.
-- The write still happens, and a write *error* would still reach you.
--
-- TYPE:  <Space>w
-- SEE:   the buffer is saved with no "…written" message (`:w` alone reports it)
vim.keymap.set("n", "<Space>w", function()
  btv.cmd("write", { silent = true })
end, { desc = "save quietly" })

-- ~~~ 3. Best-effort setup: `silent!` for a command that may not exist ~~~~~~~~~
--
-- The plugin-manager idiom — run it if it's installed, say nothing if it isn't.
-- Without `emsg_silent` this would paint E492 on the message line at startup.
--
-- TYPE:  <Space>o
-- SEE:   nothing happens and nothing is reported (`:OptionalPluginCommand` is
--        not defined). Drop `emsg_silent` and the same map reports E492.
vim.keymap.set("n", "<Space>o", function()
  btv.cmd("OptionalPluginCommand", { silent = true, emsg_silent = true })
end, { desc = "run an optional command, quietly" })

-- ~~~ 4. Errors are NOT swallowed by a bare `silent` ~~~~~~~~~~~~~~~~~~~~~~~~~~~
--
-- The same call without `emsg_silent`. This is the behavior that keeps a silent
-- keymap debuggable.
--
-- TYPE:  <Space>e
-- SEE:   E492: Not an editor command: OptionalPluginCommand
vim.keymap.set("n", "<Space>e", function()
  btv.cmd("OptionalPluginCommand", { silent = true })
end, { desc = "silent, but errors still report" })

-- ~~~ 5. The `vim.cmd` alias forms ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
--
-- All three spellings reach the same modifier. The structured form is neovim's
-- `nvim_cmd` shape, with `mods` where neovim puts it.
--
-- TYPE:  <Space>v
-- SEE:   the cursor jumps to the last line (the `$` ran) with no output from the
--        three `echo`s that ran alongside it
vim.keymap.set("n", "<Space>v", function()
  vim.cmd("echo 'string form'", { silent = true })
  vim.cmd({ cmd = "echo", args = { "'structured form'" }, mods = { silent = true } })
  vim.cmd.echo({ args = { "'indexed form'" }, mods = { silent = true } })
  vim.cmd("$")
end, { desc = "every vim.cmd form takes mods" })

-- ~~~ 6. An unsupported modifier fails loud ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
--
-- bemtvi dispatches `silent` / `emsg_silent`; anything else raises rather than
-- being silently ignored, so a config never *looks* like it applied a modifier
-- that did nothing.
--
-- TYPE:  <Space>x
-- SEE:   an error naming `keepjumps`
vim.keymap.set("n", "<Space>x", function()
  btv.cmd("normal! G", { keepjumps = true })
end, { desc = "an unknown modifier raises" })
