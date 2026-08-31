-- ~~~ bemtvi btv.statusline playground: the declarative segment registry ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/btv-statusline \
--       cargo run -p bemtvi -- examples/btv-statusline/sample.txt
--
-- Unlike the `'statusline'` `%`-format engine (see examples/statusline/), this is
-- the LUALINE-shaped surface: you name ordered *segments* for the left and right
-- halves and the server composes + paints them. Two kinds of segment:
--
--   * Built-ins (mode / filename / filetype / location / diagnostics / …) resolve
--     natively every frame — no Lua per frame.
--   * Custom segments (btv.statusline.segment{}) run their render() only when
--     invalidated: an explicit btv.statusline.invalidate(name), or one of the
--     segment's declared autocmd `events`.

-- A couple of highlight groups so the custom segments stand out. (Built-ins use
-- StatusLine / the Diagnostic* groups your colorscheme already defines.)
vim.api.nvim_set_hl(0, "StatusGit", { fg = "#a6e3a1", bold = true })
vim.api.nvim_set_hl(0, "StatusClock", { fg = "#89b4fa" })

--------------------------------------------------------------------------------
-- A custom "git" segment. The render() is cheap and reads a cache; the real work
-- (running `git`) happens off the editor thread, and on_exit invalidates the
-- segment so the new branch reaches the next paint — the canonical async shape.
--------------------------------------------------------------------------------
local git_branch = nil

btv.statusline.segment({
  name = "git",
  -- Recompute the branch when you enter a buffer or change directory; the async
  -- job below also invalidates explicitly when it finishes.
  events = { "BufEnter", "DirChanged" },
  -- Clicking the branch re-runs the fetch. `on_click` is a `v:lua.<fn>` reference
  -- (the same bridge the `%@…%X` format handlers use), called on a left-click with
  -- (minwid, clicks, button, modifiers); a segment has no minwid, so it is 0.
  on_click = "v:lua.on_git_click",
  render = function()
    return git_branch and { { text = " " .. git_branch, hl = "StatusGit" } } or nil
  end,
})

-- Kick off `git branch --show-current` without blocking; cache + invalidate when
-- it returns. btv.run is the promise-shaped one-shot process API (btv is
-- promise-only) — await it inside btv.async so the fetch reads top-to-bottom. If
-- git isn't a repo it yields nothing and the segment stays empty.
local refresh_git = btv.async(function()
  local res = btv.await(btv.run({ cmd = "git", args = { "branch", "--show-current" } }))
  local branch = res.stdout:gsub("%s+$", "")
  git_branch = branch ~= "" and branch or nil
  btv.statusline.invalidate("git")
end)
refresh_git()
btv.autocmd.create("DirChanged", { callback = refresh_git })

-- The git segment's click handler: re-fetch the branch and say so. (Defined after
-- refresh_git; `on_click` resolves the `v:lua.` reference lazily, at click time.)
function _G.on_git_click(_minwid, _clicks, _button, _mods)
  refresh_git()
  vim.cmd("echo 'git: refreshing branch…'")
end

--------------------------------------------------------------------------------
-- A trivial "clock-ish" custom segment driven purely by explicit invalidation —
-- shows how a plugin pushes new content on its own schedule. Here we just bump a
-- counter every time the cursor lands on a new line, to prove invalidation works
-- without any built-in event.
--------------------------------------------------------------------------------
local moves = 0
btv.statusline.segment({
  name = "moves",
  render = function()
    return { { text = "moves:" .. moves, hl = "StatusClock" } }
  end,
})
btv.autocmd.create("CursorMoved", {
  callback = function()
    moves = moves + 1
    btv.statusline.invalidate("moves")
  end,
})

--------------------------------------------------------------------------------
-- The layout. Built-ins + the two custom segments above. `mode` and `filename`
-- on the left; `diagnostics`, `filetype`, and the cursor `location` on the right.
--------------------------------------------------------------------------------
btv.statusline.setup({
  left = { "mode", "git", "filename", "modified" },
  right = { "moves", "diagnostics", "filetype", "location" },
})
