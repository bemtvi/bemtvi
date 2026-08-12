-- ~~~ bemtvi shada: cross-session state — :wshada / :rshada + persistence ~~~
--
-- shada ("shared data") is the durable store of editor state that outlives the
-- process: your registers, marks, search/ex history, the jumplist, the
-- changelist, and the numbered marks '0–'9. bemtvi keeps it in a per-instance
-- redb file under  stdpath("state")/shada/  and merges it back on the next
-- launch — so a yank in one session pastes in the next, `A jumps to the file
-- you marked, `" reopens a file at its last cursor, and /history survives.
--
-- This example is meant to be run TWICE against a *scratch* state dir, so it
-- never touches your real ~/.local/state. From the repo root:
--
--     # First session — seed some state, then quit with :qa
--     XDG_STATE_HOME=/tmp/bemtvi-shada-demo BEMTVI_CONFIG=examples/shada \
--       cargo run -p bemtvi -- examples/shada/sample.txt
--
--     # Second session — the state from the first is restored
--     XDG_STATE_HOME=/tmp/bemtvi-shada-demo BEMTVI_CONFIG=examples/shada \
--       cargo run -p bemtvi -- examples/shada/sample.txt
--
-- (Delete /tmp/bemtvi-shada-demo to start fresh.)
--
-- What to do in the FIRST session:
--   :SeedShada     stash a register, set global mark A, push a search
--   <space>w       :wshada — flush the store NOW (or just :qa, which flushes too)
--   :qa            quit
--
-- What to do in the SECOND session:
--   "ap            paste register "a from last session
--   `A             jump to the global mark you set last session
--   /<Up><CR>      recall last session's search pattern
--   :rshada        re-read the store mid-session (picks up a sibling that exited)

-- :SeedShada — populate a handful of cross-session slots in one go, then tell
-- you what was stored. None of this is shada-specific API: shada persists the
-- *ordinary* editor state these commands touch.
vim.api.nvim_create_user_command("SeedShada", function()
  -- A named register: "a holds a charwise greeting.
  vim.fn.setreg("a", "hello from the previous session")
  -- A global file mark on the current line — `A jumps back to it next launch.
  vim.cmd("normal! mA")
  -- A search, which pushes the / history that <Up> recalls next launch.
  vim.fn.setreg("/", "needle")
  print('seeded: "a, global mark A (this line), and the / search "needle" — '
    .. "now :wshada (or :qa) and relaunch")
end, {})

-- <space>w — :wshada, the explicit flush. It writes this instance's store right
-- now (like :w for a buffer), so the state is durable immediately rather than
-- only at the debounced checkpoint or at exit. `'0` stays clean-exit-only — a
-- :wshada is not an exit, so it never rewrites the last-exit cursor.
vim.keymap.set("n", "<space>w", "<cmd>wshada<CR>", { desc = "flush the shada store now" })

-- <space>r — :rshada, the explicit re-read. It re-merges every *readable* store
-- in the dir (a still-live instance's file is locked, so you see its data only
-- after it exits — neovim's contract) into this running session. Use :rshada!
-- to overwrite a register you've already set this session; plain :rshada keeps
-- your live value and only fills empty slots.
vim.keymap.set("n", "<space>r", "<cmd>rshada<CR>", { desc = "re-read the shada store" })

-- A tiny status helper: show whether register "a survived from a prior session.
vim.api.nvim_create_user_command("ShadaShow", function()
  local a = vim.fn.getreg("a")
  if a == "" then
    print('register "a is empty — run :SeedShada in a prior session, :wshada, relaunch')
  else
    print('register "a (restored): ' .. a)
  end
end, {})
