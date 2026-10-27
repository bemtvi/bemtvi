-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/single-file
--
-- The runner starts a plain editor, not the app's single-file one, so this holds the
-- config to what it gives (§2's mapping, §3). The lock and the disabled commands (§1, §2) are the
-- app's side, covered by bemtvi-gui's tests/embed.rs and the server's single_file.rs
-- and disabled_commands.rs.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/single-file", function()
  -- §2. "<Space>f (normal mode: this config maps it to the files picker)". The runner's
  -- editor is a plain one, so the picker opens here (the single-file editor refuses it).
  btv.test.it("§2 — <Space>f opens the files picker", function(t)
    open(t)
    t:feed("<Space>f")
    t:wait_for(function()
      return btv._picker ~= nil and btv._picker.source.name == "files"
    end, { message = "the files picker did not open" })
    t:feed("<Esc>")
  end)

  -- §3. ":Stamp → a line with today's date below the cursor"
  btv.test.it("§3 — :Stamp inserts today's date", function(t)
    open(t)
    t:cmd("Stamp")
    btv.test.expect(t:line(2)).to_be(os.date("%Y-%m-%d"))
  end)

  -- §3. "<Space>s (normal mode)"
  btv.test.it("§3 — <Space>s runs it too", function(t)
    open(t)
    t:feed("<Space>s")
    btv.test.expect(t:line(2)).to_be(os.date("%Y-%m-%d"))
  end)
end)
