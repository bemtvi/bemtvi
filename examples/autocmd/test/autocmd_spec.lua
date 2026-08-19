-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/autocmd
--
-- It sources `init.lua` as a session would and then types exactly what the
-- sample buffer tells a reader to type, asserting on the message line each of
-- those lines promises.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The whole message log, as one string.
---
--- `t:message()` is only the newest line, and the interesting ones here are
--- announced from *inside* a `:e` — which paints its own message once it
--- finishes, wiping theirs. `:messages` is what the sample buffer itself tells a
--- reader to run, and its panel is an ordinary buffer, so the spec reads it the
--- same way a reader looks at it.
local function messages(t)
  t:cmd("messages")
  local log = table.concat(t:lines(), "\n")
  t:feed("q")
  return log
end

--- Drop the log so a test only sees what it triggered itself.
local function clear_messages(t)
  t:cmd("messages clear")
end

btv.test.describe("examples/autocmd", function()
  -- §2. The Demo augroup's command-string autocmd, on a real editor event.
  btv.test.it("§2 — reading a .txt file echoes the Demo augroup's banner", function(t)
    -- Read on its own, with nothing typed after: `:echo` writes the message line
    -- but — like vim — is not recorded in `:messages`, so the next keypress that
    -- repaints it is the last chance to see it.
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(t:message()).to_contain("Read a .txt file (from the Demo augroup)")
  end)

  btv.test.it("§2 — re-sourcing the group does not stack duplicates", function(t)
    open(t)
    local function count()
      local n = 0
      for _, au in ipairs(btv.autocmd.get({ event = "BufReadPost" })) do
        if au.pattern == "*.txt" then
          n = n + 1
        end
      end
      return n
    end
    local before = count()
    -- `augroup Demo` + `autocmd!` is exactly what the config does on each source.
    t:cmd("augroup Demo")
    t:cmd("autocmd!")
    t:cmd([[autocmd BufReadPost *.txt echo "Read a .txt file (from the Demo augroup)"]])
    t:cmd("augroup END")
    btv.test.expect(count()).to_be(before)
  end)

  -- §3 / §4. Two interchangeable triggers for the same `User Greet` handler.
  btv.test.it("§4 — :doautocmd User Greet runs the ex-command autocmd", function(t)
    open(t)
    t:cmd("doautocmd User Greet")
    btv.test.expect(t:message()).to_contain("Hello from the Greet autocmd")
  end)

  btv.test.it("§3 — :Greet is the same trigger through a user command", function(t)
    open(t)
    t:cmd("Greet")
    btv.test.expect(t:message()).to_contain("Hello from the Greet autocmd")
  end)

  -- The sample buffer's own TRY-IT list, typed as written.
  btv.test.it(":autocmd User lists the User autocmds", function(t)
    open(t)
    -- The listing is generated in Lua, so it can be captured directly — which is
    -- exactly what `:autocmd User` puts on screen.
    local listing = btv.exec("autocmd User", true)
    btv.test.expect(listing).to_contain("User")
    btv.test.expect(listing).to_contain("Greet")
  end)

  btv.test.it(":autocmd User Hi … registers another, fired by :doautocmd", function(t)
    open(t)
    t:cmd([[autocmd User Hi echo "hi there"]])
    t:cmd("doautocmd User Hi")
    btv.test.expect(t:message()).to_contain("hi there")
  end)

  btv.test.it(":autocmd! User clears every User autocmd", function(t)
    open(t)
    t:cmd("autocmd! User")
    btv.test.expect(#btv.autocmd.get({ event = "User" })).to_be(0)
    -- With nothing registered, firing it says nothing.
    t:cmd("echo 'marker'")
    t:cmd("doautocmd User Greet")
    btv.test.expect(t:message()).never.to_contain("Hello from the Greet autocmd")
  end)

  -- §1. Real editor events through the Lua API.
  btv.test.it("§1 — InsertEnter announces the file being edited", function(t)
    open(t)
    t:feed("i")
    btv.test.expect(t:message()).to_contain("InsertEnter: now editing sample.txt")
    t:feed("<Esc>")
  end)

  btv.test.it("§5 — InsertLeave announces the return to normal mode", function(t)
    open(t)
    t:feed("i<Esc>")
    btv.test.expect(t:message()).to_contain("back to normal mode")
  end)

  btv.test.it("§5 — TextChanged fires on a normal-mode edit", function(t)
    open(t)
    t:feed("x")
    btv.test.expect(t:message()).to_contain("buffer changed")
  end)

  btv.test.it("§5 — BufNewFile fires for a path with no file on disk", function(t)
    open(t)
    clear_messages(t)
    local path = btv.test.tempdir() .. "/brand-new.txt"
    t:cmd("e " .. path)
    local log = messages(t)
    btv.test.expect(log).to_contain("new file!")
    btv.test.expect(log).to_contain("brand-new.txt")
    -- BufNewFile fires *instead of* the read banner: there was nothing to read.
    btv.test.expect(log).never.to_contain("Read a .txt file")
  end)

  btv.test.it("§5 — BufWritePost announces the save", function(t)
    open(t)
    local path = btv.test.tempdir() .. "/written.txt"
    t:cmd("e " .. path)
    t:feed("isaved by the spec<Esc>")
    t:cmd("w")
    btv.test.expect(t:message()).to_contain("saved ")
    btv.test.expect(t:message()).to_contain("written.txt")
  end)

  -- §1. The `once` autocmd removed itself the first time it fired, at startup.
  btv.test.it("§1 — the once-only BufEnter greeting does not fire again", function(t)
    open(t)
    t:cmd("echo 'marker'")
    t:cmd("enew")
    btv.test.expect(t:message()).never.to_contain("this fires once")
  end)
end)
