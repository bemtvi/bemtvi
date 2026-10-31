-- btv.picker deprioritized paths and the .gitignore pickers — a runnable playground.
--
--   BEMTVI_CONFIG=examples/picker-priority \
--     cargo run -p bemtvi -- examples/picker-priority/sample.txt
--
-- This directory is a tiny project with the noise a real one carries beside it: a
-- vendored `node_modules/`, a built `dist/`, a `yarn.lock` — and a `generated/` that
-- its `.gitignore` ignores. Every one of those files defines or calls the same function,
-- so one search hits them all. (These notes name it once, in §1, so that they don't
-- crowd the results themselves.)
--
-- The keys below are the shipped picker maps under bemtvi's default leader, `\`.

-- The pickers search the working directory, so work from this one whatever directory
-- the editor was started in.
local DIR = btv.utils.dirname(debug.getinfo(1, "S").source:sub(2))
btv.cmd("cd " .. DIR)

-- The ignored file, written here rather than committed: `generated/` is in this
-- directory's `.gitignore`, so it stays untracked (git would list a tracked file even
-- when ignored) and every search agrees it is ignored.
btv.async(function()
  btv.await(btv.fs.mkdir(btv.utils.path_join(DIR, "generated"), { recursive = true }))
  btv.await(btv.fs.write(
    btv.utils.path_join(DIR, "generated", "bundle.js"),
    -- the function's name, spelled so this line is not a hit itself
    "export const " .. "use" .. "Auth = () => ({})\n"
  ))
end)()

-- 1. Vendored paths list last.
--
--    TYPE  \fg  then  useAuth
--    SEE   the project's own hits first — `src/auth.ts`, `src/auth.test.ts`, and this
--          config — then `dist/auth.min.js`, `node_modules/auth-lib/index.js` and
--          `yarn.lock` below them. Nothing is hidden; the vendored hits are just out
--          of the way.
--
--    TYPE  <Esc>  then  \ff
--    SEE   the same order for files: `src/…` before `dist/`, `node_modules/` and
--          `yarn.lock`. Type `auth` and the vendored files stay below `src/` even
--          where they match as well.
--
--    The list is `btv.picker.defaults.deprioritize`: `node_modules/`, `dist/`,
--    `build/`, `target/`, `.venv/`, … every common lockfile, and `*.min.js` /
--    `*.min.css` / `*.map`.

-- 2. Adding to the list — test files last too.
--
--    TYPE  \pt  then  \fg  and the same search
--    SEE   `src/auth.test.ts` has moved down among the vendored hits; `src/auth.ts`
--          leads alone. `\pt` again puts the shipped list back.
--
--    The shipped list leaves tests alone (plenty of people grep their tests on
--    purpose). A config that wants them last extends the default rather than
--    retyping it, exactly as below.
local tests_last = false
btv.keymap.set("n", "<leader>pt", function()
  tests_last = not tests_last
  local list = btv.tbl.deepcopy(btv.picker.defaults.deprioritize)
  if tests_last then
    btv.list.extend(list, { "*.test.*" })
  end
  btv.picker.setup({ deprioritize = list })
  btv.notify(tests_last and "test files listed last" or "shipped deprioritize list")
end, { desc = "Toggle test files in the deprioritize list" })

-- 3. Respecting .gitignore, or searching everything.
--
--    TYPE  \fg  and the same search
--    SEE   no `generated/bundle.js`: `\fg` (`live_grep_git`) respects `.gitignore`.
--
--    TYPE  <Esc>  then  \fG  and the same search
--    SEE   `generated/bundle.js` is there: `\fG` (`live_grep`) searches every file,
--          ignored ones too. `\ff` / `\fF` are the same pair for finding files.
--
--    The box title says which one is open — "Live Grep" or "Live Grep (All Files)".
--    Like rg, the `.gitignore` counts because this directory is inside a git
--    repository (the bemtvi checkout); copied somewhere outside one it would not.
