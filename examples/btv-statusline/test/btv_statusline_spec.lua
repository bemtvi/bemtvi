-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/btv-statusline
--
-- It sources `init.lua` as a session would and then does the three things the
-- sample buffer tells a reader to try, reading the result off `t:statusline()` —
-- the composed bar the client would paint.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The `moves:N` counter the custom segment paints, as a number.
local function moves(t)
  return tonumber((t:statusline():match("moves:(%d+)")))
end

btv.test.describe("examples/btv-statusline", function()
  btv.test.it("the layout's built-in segments all reach the bar", function(t)
    open(t)
    local bar = t:statusline()
    -- left = { mode, git, filename, modified }
    btv.test.expect(bar).to_contain("sample.txt")
    -- right = { moves, diagnostics, filetype, location }
    btv.test.expect(bar).to_contain("moves:")
    -- `location` is the cursor's line:col — the cursor is parked at the top.
    btv.test.expect(bar).to_match("1:1")
  end)

  btv.test.it("the filetype segment shows the buffer's filetype", function(t)
    -- A `.txt` sample has no filetype, and the segment renders nothing for one —
    -- so this needs a file that has one.
    t:cmd("e " .. DIR .. "/init.lua")
    btv.test.expect(btv.bo.filetype).to_be("lua")
    btv.test.expect(t:statusline()).to_contain("lua")
  end)

  btv.test.it("the mode segment follows the mode", function(t)
    open(t)
    local normal = t:statusline()
    t:feed("i")
    local insert = t:statusline()
    t:feed("<Esc>")
    btv.test.expect(insert).never.to_be(normal)
    btv.test.expect(insert:upper()).to_contain("INSERT")
    btv.test.expect(normal:upper()).to_contain("NORMAL")
  end)

  btv.test.it("the modified segment appears once the buffer is dirty", function(t)
    open(t)
    local clean = t:statusline()
    t:feed("x")
    btv.test.expect(t:statusline()).never.to_be(clean)
    btv.test.expect(btv.bo.modified).to_be(true)
  end)

  btv.test.it("the location segment follows the cursor", function(t)
    open(t)
    t:feed("3G")
    btv.test.expect(t:statusline()).to_match("3:1")
  end)

  -- "Move the cursor up and down — the moves:N segment bumps every time
  -- CursorMoved fires (an explicit btv.statusline.invalidate)."
  btv.test.it("moving the cursor bumps the explicitly-invalidated segment", function(t)
    open(t)
    local before = moves(t)
    btv.test.expect(before).never.to_be_nil()
    t:feed("jjj")
    local after = moves(t)
    btv.test.expect(after > before).to_be(true)
  end)

  btv.test.it("a segment that is never invalidated does not re-render", function(t)
    open(t)
    local before = moves(t)
    -- `:` opens the command line and `<Esc>` leaves it — no CursorMoved, so the
    -- cached render stands. This is the whole point of the invalidation model.
    t:feed(":<Esc>")
    btv.test.expect(moves(t)).to_be(before)
  end)

  -- "…the git segment re-runs `git branch` off the editor thread and shows the
  -- current branch when there is one."
  btv.test.it("the git segment shows the branch git itself reports", function(t)
    open(t)
    local res = btv.await(btv.run({ cmd = "git", args = { "branch", "--show-current" } }))
    local branch = res.stdout:gsub("%s+$", "")
    if branch == "" then
      -- Not a repo (or a detached HEAD): the segment is empty by design, and the
      -- rest of the bar is unaffected.
      btv.test.expect(t:statusline()).to_contain("sample.txt")
      return
    end
    t:wait_for(function()
      return t:statusline():find(branch, 1, true) ~= nil
    end, { message = "the git segment never showed the branch " .. branch })
  end)

  btv.test.it("clicking the git segment re-fetches and says so", function(t)
    open(t)
    -- `on_click` is a `v:lua.` reference resolved at click time; a segment has no
    -- minwid, so a left-click arrives as (0, 1, "l", "").
    _G.on_git_click(0, 1, "l", "")
    t:wait_for(function()
      return (t:message() or ""):find("git: refreshing branch", 1, true) ~= nil
    end, { message = "the click handler said nothing" })
  end)

  btv.test.it("the segments are the ones btv.statusline.setup named", function(t)
    open(t)
    -- The `%`-format engine is the *other* surface (examples/statusline/); this
    -- one composes named segments, so 'statusline' itself stays unset.
    btv.test.expect(btv.o.statusline).to_be("")
    btv.test.expect(t:statusline()).never.to_be("")
  end)
end)
