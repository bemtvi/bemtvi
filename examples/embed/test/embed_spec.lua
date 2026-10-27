-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/embed
--
-- The runner embeds the editor in no app, so this holds the config to its
-- plain-bemtvi branch: it loads without an app, and <Space>p says there's none.
-- The app's side (§2–§4 in the app) is covered by bemtvi-gui's tests/embed.rs.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

btv.test.describe("examples/embed", function()
  -- §1. "nil in plain bemtvi"
  btv.test.it("§1 — no app embeds the editor here", function()
    btv.test.expect(btv.host.name()).to_be(nil)
  end)

  -- §3. "In plain bemtvi: no app to preview in"
  btv.test.it("§3 — <Space>p says there's no app to preview in", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    t:feed("<Space>p")
    btv.test.expect(t:message()).to_contain("no app to preview in")
  end)

  -- §3, in the app's branch: what the keymap would send raises without one.
  btv.test.it("§3 — sending with no app raises", function()
    local ok, err = pcall(btv.host.send, "preview", {})
    btv.test.expect(ok).to_be(false)
    btv.test.expect(tostring(err)).to_contain("no app embeds this editor")
  end)
end)
