-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/explorer
--
-- The explorer is a real buffer, so the listing is `t:lines()` and the selection
-- is the cursor. Every WHAT-TO-TYPE line is typed as written.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")
local TREE = DIR .. "/tree"

--- Open the sample tree as a directory listing.
local function open(t)
  t:cmd("e " .. TREE)
  t:wait_for(function()
    return (t:line(1) or "") == "../"
  end, { message = "the directory never listed" })
end

--- Move the cursor onto the row whose text is `name`.
local function select_row(t, name)
  for i, line in ipairs(t:lines()) do
    if line == name then
      t:feed(i .. "G")
      return i
    end
  end
  error("no row named " .. name .. " in " .. table.concat(t:lines(), ", "), 0)
end

btv.test.describe("examples/explorer", function()
  -- "the listing, directories first (suffixed `/`), then files, each group sorted
  --  by name, with a `../` up-entry on top"
  btv.test.it("a directory opens as the listing the notes describe", function(t)
    open(t)
    btv.test.expect(t:lines()).to_equal({ "../", "src/", "notes.txt", "readme.txt" })
  end)

  btv.test.it("the listing is a real buffer, so the usual motions work", function(t)
    open(t)
    t:feed("G")
    btv.test.expect(t:cursor()[1]).to_be(4)
    t:feed("gg")
    btv.test.expect(t:cursor()[1]).to_be(1)
    t:feed("jj")
    btv.test.expect(t:cursor()[1]).to_be(3)
  end)

  -- "The listing is effectively 'nomodifiable': editing keys are inert."
  btv.test.it("editing keys cannot corrupt the picture of the filesystem", function(t)
    open(t)
    local before = t:lines()
    t:feed("x")
    t:feed("dd")
    t:feed("ihello<Esc>")
    t:feed("p")
    btv.test.expect(t:lines()).to_equal(before)
    btv.test.expect(t:mode()).to_be("n")
  end)

  -- "The command line and search still work."
  btv.test.it("the command line and search still work", function(t)
    open(t)
    t:feed("/readme<CR>")
    btv.test.expect(t:current_line()).to_be("readme.txt")
    t:cmd("1")
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)

  -- "<CR> on a directory (`src/`) descends into it, re-listing in the same window"
  btv.test.it("<CR> on a directory descends, re-listing in place", function(t)
    open(t)
    select_row(t, "src/")
    t:feed("<CR>")
    t:wait_for(function()
      return (t:lines() or {})[2] == "lib.rs"
    end, { message = "never descended into src/" })
    btv.test.expect(t:lines()).to_equal({ "../", "lib.rs", "main.rs" })
  end)

  -- "<CR> on `../` (or `-`) goes up to the parent directory"
  btv.test.it("<CR> on ../ goes back up", function(t)
    open(t)
    select_row(t, "src/")
    t:feed("<CR>")
    t:wait_for(function()
      return (t:lines() or {})[2] == "lib.rs"
    end, { message = "never descended into src/" })
    t:feed("gg<CR>")
    t:wait_for(function()
      return (t:lines() or {})[2] == "src/"
    end, { message = "never went back up" })
    btv.test.expect(t:lines()).to_equal({ "../", "src/", "notes.txt", "readme.txt" })
  end)

  btv.test.it("- is the same as <CR> on ../", function(t)
    open(t)
    select_row(t, "src/")
    t:feed("<CR>")
    t:wait_for(function()
      return (t:lines() or {})[2] == "lib.rs"
    end, { message = "never descended into src/" })
    t:feed("-")
    t:wait_for(function()
      return (t:lines() or {})[2] == "src/"
    end, { message = "`-` never went up" })
  end)

  -- "<CR> on a file opens that file for editing"
  btv.test.it("<CR> on a file opens it", function(t)
    open(t)
    select_row(t, "readme.txt")
    t:feed("<CR>")
    t:wait_for(function()
      return (btv.buf.name(0) or ""):find("readme.txt", 1, true) ~= nil
    end, { message = "<CR> never opened the file" })
    btv.test.expect(btv.bo.modifiable).to_be(true)
    btv.test.expect(#t:lines() > 0).to_be(true)
  end)

  -- "Opening a file WIPES the listing buffer (it was just a picker), so it does
  --  not linger in `:ls` or as the alternate."
  btv.test.it("the listing does not linger in :ls once a file is opened", function(t)
    open(t)
    select_row(t, "readme.txt")
    t:feed("<CR>")
    t:wait_for(function()
      return (btv.buf.name(0) or ""):find("readme.txt", 1, true) ~= nil
    end, { message = "<CR> never opened the file" })
    t:cmd("ls")
    local listing = table.concat(t:lines(), "\n")
    btv.test.expect(listing).never.to_contain("tree\n")
    btv.test.expect(listing).never.to_contain("/tree\"")
    t:feed("q")
  end)

  -- ":e . opens the current directory" — the other way in, from inside the editor.
  btv.test.it(":e on a directory path opens the explorer", function(t)
    t:cmd("e " .. TREE .. "/src")
    t:wait_for(function()
      return (t:line(1) or "") == "../"
    end, { message = ":e on a directory did not list it" })
    btv.test.expect(t:lines()).to_equal({ "../", "lib.rs", "main.rs" })
  end)
end)
