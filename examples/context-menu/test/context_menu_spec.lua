-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/context-menu
--
-- Each numbered note in init.lua, done as written. The rows a right-click would show
-- are read through `btv.contextmenu.entries`; section 1 also drives the real gesture
-- (`t:mouse`), and reads the open menu with `t:menu()`.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Open the sample fresh, in Normal mode at the start of line 1.
local function open(t)
  t:feed("<Esc>")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg0")
end

--- The labels of the current buffer's menu right now, `"-"` for a separator.
local function labels()
  local out = {}
  for _, e in ipairs(btv.await(btv.contextmenu.entries(0))) do
    out[#out + 1] = e.separator and "-" or e.label
  end
  return out
end

--- Wait for the system clipboard (the runner's in-memory one) to hold `want`.
local function clipboard_holds(t, want)
  return t:wait_for(function()
    return btv.test.clipboard.peek() == want
  end, { message = "the clipboard never held " .. want })
end

--- Run the row labelled `label`, as choosing it does.
local function choose(label)
  for _, e in ipairs(btv.await(btv.contextmenu.entries(0))) do
    if e.label == label then
      return e.action({})
    end
  end
  error("no menu row " .. label)
end

btv.test.describe("examples/context-menu", function()
  btv.test.it("1 — a right-click opens the menu at the pointer, on the clicked word", function(t)
    open(t)
    local col = btv.win.textoff(btv.win.current()) + 8 -- inside `banana`
    t:mouse("right", "press", 0, col)
    local m = t:wait_for(function()
      local menu = t:menu()
      return menu and #menu.items > 0 and menu or nil
    end, { message = "no menu opened" })
    btv.test.expect(m.items[1]).to_be("Copy")
    btv.test.expect(btv.expand("<cword>")).to_be("banana")
    t:feed("<Esc>")
    btv.test.expect(t:menu()).to_be_nil()
  end)

  btv.test.it("1 — a selection stays, and Cut joins the menu", function(t)
    open(t)
    t:feed("jwviw") -- `quick` on line 2
    local rows = labels()
    btv.test.expect(rows[1]).to_be("Cut")
    btv.test.expect(rows[2]).to_be("Copy")
  end)

  btv.test.it("2 — the undo group is hidden", function(t)
    open(t)
    local rows = labels()
    btv.test.expect(rows).never.to_contain("Undo")
    btv.test.expect(rows).never.to_contain("Redo")
  end)

  btv.test.it("3 — Copy Word copies the word under the cursor", function(t)
    open(t)
    t:feed("wwl") -- inside `cherry`
    btv.test.expect(labels()).to_contain("Copy Word")
    choose("Copy Word")
    clipboard_holds(t, "cherry")
    -- Not offered over a selection.
    t:feed("viw")
    btv.test.expect(labels()).never.to_contain("Copy Word")
  end)

  btv.test.it("4 — the case group follows the clipboard rows", function(t)
    open(t)
    t:feed("j3w") -- `fox` on line 2
    local rows = labels()
    btv.test.expect(rows).to_contain("UPPERCASE")
    local sep_before
    for i, r in ipairs(rows) do
      if r == "UPPERCASE" then
        sep_before = rows[i - 1]
      end
    end
    btv.test.expect(sep_before).to_be("-")
    choose("UPPERCASE")
    btv.test
      .expect(t:wait_for(function()
        return t:line(2):find("FOX", 1, true) and t:line(2)
      end))
      .to_be("The quick brown FOX jumps over the lazy dog.")
  end)

  btv.test.it("5 — <leader>m puts the palette first in this buffer only", function(t)
    open(t)
    t:feed("<Bslash>m") -- `<leader>` is `\` unless a config sets `mapleader`
    btv.test.expect(labels()[1]).to_be("Command Palette")
    t:cmd("enew")
    btv.test.expect(labels()[1]).to_be("Copy")
  end)

  btv.test.it("6 — the file group waits for the disk, greyed size then Copy Path", function(t)
    open(t)
    local entries = btv.await(btv.contextmenu.entries(0))
    local size = entries[#entries - 1]
    btv.test.expect(size.label:match("^%d+ bytes on disk$")).never.to_be_nil()
    btv.test.expect(size.enabled).to_be(false)
    btv.test.expect(entries[#entries].label).to_be("Copy Path")
    choose("Copy Path")
    clipboard_holds(t, btv.buf.name(0))
  end)
end)
