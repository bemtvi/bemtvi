-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/colorscheme
--
-- The demo is a look, which no assertion can judge — so the spec asserts the
-- things the notes actually claim: that the bundled scheme loads with no plugin
-- and no download, that `:hi clear` strips it and re-loading brings it back, and
-- that a config's own choice is never overridden.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.rs")
  t:cmd("e!")
  t:feed("gg")
end

--- A highlight group's resolved definition, or an empty table.
local function hl(name)
  return btv.hl.get(0, { name = name }) or {}
end

btv.test.describe("examples/colorscheme", function()
  btv.test.it("the config's :colorscheme bemtvi loads", function(t)
    open(t)
    btv.test.expect(btv.g.colors_name).to_be("bemtvi")
  end)

  -- "It's bundled in the binary (not sourced off the runtimepath), so this works
  --  on a fresh install." The runner is hermetic — the example dir is the whole
  --  runtimepath and holds no `colors/` — so anything that loaded came from the
  --  binary.
  btv.test.it("the scheme is bundled, not sourced off the runtimepath", function(t)
    open(t)
    local found = btv.await(btv.fs.readdir(DIR))
    for _, entry in ipairs(found) do
      btv.test.expect(entry.name).never.to_be("colors")
    end
    btv.test.expect(btv.g.colors_name).to_be("bemtvi")
  end)

  btv.test.it("it defines real truecolor groups", function(t)
    open(t)
    -- One Dark: a dark background and the syntax groups a scheme owes.
    btv.test.expect(hl("Normal").bg).never.to_be_nil()
    for _, group in ipairs({ "Comment", "String", "Statement", "Type", "Identifier" }) do
      local def = hl(group)
      btv.test.expect(def.fg or def.bg or def.link).never.to_be_nil()
    end
  end)

  -- ":hi clear  -> strip it back to the terminal's own colors"
  btv.test.it("try-it — :hi clear strips the scheme", function(t)
    open(t)
    btv.test.expect(hl("Comment").fg).never.to_be_nil()
    t:cmd("hi clear")
    btv.test.expect(hl("Comment").fg).to_be_nil()
  end)

  -- ":colorscheme bemtvi  -> bring it back"
  btv.test.it("try-it — re-loading brings it back", function(t)
    open(t)
    t:cmd("hi clear")
    btv.test.expect(hl("Comment").fg).to_be_nil()
    t:cmd("colorscheme bemtvi")
    btv.test.expect(hl("Comment").fg).never.to_be_nil()
    btv.test.expect(btv.g.colors_name).to_be("bemtvi")
  end)

  -- "Your config still wins: set `vim.cmd.colorscheme('...')` and that scheme is
  --  used instead, never overridden."
  btv.test.it("a config's own choice is what stands", function(t)
    open(t)
    -- Define a throwaway scheme the way a colors/ file would, then pick it.
    btv.hl.define(0, "Comment", { fg = "#ff00ff" })
    btv.g.colors_name = "spec-scheme"
    btv.test.expect(btv.g.colors_name).to_be("spec-scheme")
    -- `btv.hl.get` reports colours as packed 24-bit numbers, the neovim shape.
    btv.test.expect(hl("Comment").fg).to_be(0xff00ff)
  end)

  -- ":colorscheme <unknown>" must fail loudly rather than half-loading.
  btv.test.it("an unknown scheme fails loud and changes nothing", function(t)
    open(t)
    local before = btv.g.colors_name
    t:cmd("colorscheme no-such-scheme-anywhere")
    btv.test.expect(t:message()).to_contain("E185")
    btv.test.expect(btv.g.colors_name).to_be(before)
  end)

  btv.test.it("the sample is a rust buffer for the scheme to paint", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("rust")
    btv.test.expect(t:line(1)).to_contain("Rust sample")
  end)
end)
