-- ~~~ bemtvi autocmd playground: the :autocmd / :augroup / :doautocmd commands ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/autocmd \
--       cargo run -p bemtvi -- examples/autocmd/sample.txt
--
-- An *autocommand* runs an ex-command (or, from Lua, a callback) when an event
-- fires — opening a file, switching buffers, entering insert mode, or a custom
-- `User` event you trigger yourself. An *augroup* is a named bucket of them, so a
-- config can clear and re-register its own autocmds without touching anyone
-- else's. bemtvi drives the same store from two front-ends: the `vim.api.nvim_*`
-- functions (used below in §1 and §5) and the Vimscript-style `:`-commands
-- (§2–§4), which this example exists to show off. §5 wires the editor-lifecycle
-- events (BufWritePre/Post, InsertLeave, TextChanged, BufNewFile, …) that fire as
-- you edit.
--
-- A command-string autocmd runs an EX-COMMAND when it fires — e.g. `:echo`,
-- which evaluates a Vim expression onto the message line.
--
-- TRY IT interactively — watch the MESSAGE LINE (and `:messages`):
--   :autocmd User Greet echo "hi from a command-string autocmd"
--                              register a command-string autocmd for `User Greet`
--   :doautocmd User Greet      fire it now (the manual trigger)
--   :autocmd User              list the autocmds registered for the `User` event
--   :autocmd! User             remove every `User` autocmd
--   :augroup Demo              open the `Demo` group; subsequent :autocmd's join it
--   :augroup END               close it (back to the default/no group)
--   :augroup! Demo             delete the `Demo` group and all its autocmds
--   i ... <Esc>                InsertEnter fires (see §1) and announces itself
--
-- The `:autocmd {event} {pat} {cmd}` form takes a PATTERN positionally — use `*`
-- to match every file. `++once` makes an autocmd fire a single time then vanish.

--------------------------------------------------------------------------------
-- 1. A couple of editor-event autocmds registered through the Lua API, so you
--    can see real events (not just :doautocmd) flow through the same store.
--------------------------------------------------------------------------------
vim.api.nvim_create_autocmd("InsertEnter", {
  callback = function()
    vim.notify("InsertEnter: now editing " .. vim.fn.expand("%:t"))
  end,
})

-- A `++once`-style API autocmd: greet the first buffer entered, then self-remove.
vim.api.nvim_create_autocmd("BufEnter", {
  once = true,
  callback = function(a)
    vim.notify("first BufEnter (buf " .. tostring(a.buf) .. ") — this fires once")
  end,
})

--------------------------------------------------------------------------------
-- 2. The same thing via the `:augroup` / `:autocmd` EX-COMMANDS. Defining the
--    group here (clear-on-resource) means re-sourcing this config never stacks
--    duplicate autocmds. `vim.cmd` runs the `:`-commands exactly as if you typed
--    them, so this block is a faithful stand-in for an init.vim `augroup` stanza.
--------------------------------------------------------------------------------
vim.cmd("augroup Demo")
vim.cmd("autocmd!") -- clear the Demo group (no-op on first source)
-- Command-string autocmd: echo a banner whenever any *.txt file is read.
vim.cmd([[autocmd BufReadPost *.txt echo "Read a .txt file (from the Demo augroup)"]])
vim.cmd("augroup END")

--------------------------------------------------------------------------------
-- 3. A user command that fires a custom `User` event, so `:Greet` and
--    `:doautocmd User Greet` are interchangeable triggers for §4's autocmd.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("Greet", function()
  vim.cmd("doautocmd User Greet")
end)

--------------------------------------------------------------------------------
-- 4. Register a `User Greet` handler through the ex-command, then prove it works
--    at startup by firing it once. Look for "Hello from the Greet autocmd" on the
--    message line right after launch.
--------------------------------------------------------------------------------
vim.cmd([[autocmd User Greet echo "Hello from the Greet autocmd"]])
vim.cmd("doautocmd User Greet")

--------------------------------------------------------------------------------
-- 5. The editor-lifecycle events. These fire as you edit — no `:doautocmd`
--    needed. A glob `pattern` (e.g. `*.txt`) matches by file extension.
--
--    TRY IT:
--      :w                 -> "saved <file>" (BufWritePost)
--      i ... <Esc>        -> "back to normal" (InsertLeave)
--      dd / x / p         -> "buffer changed" (TextChanged)
--      :e nonexistent.txt -> "new file!" (BufNewFile, not BufReadPost)
--------------------------------------------------------------------------------
local lifecycle = vim.api.nvim_create_augroup("LifecycleDemo", { clear = true })

-- BufWritePre runs before each save, BufWritePost just after — the hooks plugins
-- use for format-on-save (Pre) and lint/reload/notify (Post). A `*.txt` pattern
-- scopes them to text files.
vim.api.nvim_create_autocmd("BufWritePost", {
  group = lifecycle,
  pattern = "*.txt",
  callback = function(a)
    vim.notify("saved " .. a.file)
  end,
})

-- InsertLeave is the mirror of InsertEnter (§1) — handy for clearing search
-- highlight or saving on exit from insert.
vim.api.nvim_create_autocmd("InsertLeave", {
  group = lifecycle,
  callback = function()
    vim.notify("back to normal mode")
  end,
})

-- TextChanged fires whenever the buffer's text changes in Normal mode (its
-- TextChangedI twin fires per keystroke in Insert) — the signal live linters and
-- autosave plugins watch.
vim.api.nvim_create_autocmd("TextChanged", {
  group = lifecycle,
  callback = function()
    vim.notify("buffer changed")
  end,
})

-- BufNewFile fires (instead of BufReadPost) when you open a path with no file on
-- disk yet — where a template/skeleton plugin drops in boilerplate.
vim.api.nvim_create_autocmd("BufNewFile", {
  group = lifecycle,
  callback = function(a)
    vim.notify("new file! " .. a.file)
  end,
})
