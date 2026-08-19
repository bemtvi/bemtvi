-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/global-local-options
--
-- Each numbered TRY-IT is typed as written. The whole subject is the difference
-- between two tiers, so every case reads BOTH — `:set x?` / `btv.o` for the local
-- value, `:setglobal x?` / `btv.go` for the tier a new buffer is born from.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open one of the example's files, re-read so each test starts from the same text.
local function open(t, name)
  t:cmd("e " .. DIR .. "/" .. name)
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/global-local-options", function()
  -- 1. "`sample.txt` opened AFTER this config ran — and still got its settings"
  btv.test.it("try-it 1 — a file opened later inherits the config's settings", function(t)
    open(t, "sample.txt")
    t:cmd("set tabstop?")
    btv.test.expect(t:message()).to_contain("tabstop=3")
    btv.test.expect(btv.bo.tabstop).to_be(3)
    btv.test.expect(btv.bo.expandtab).to_be(true)
    t:cmd("set foldmarker?")
    btv.test.expect(t:message()).to_contain("foldmarker=<<<,>>>")
  end)

  btv.test.it("try-it 1 — <Tab> inserts three spaces", function(t)
    open(t, "sample.txt")
    t:feed("3GI<Tab>x<Esc>")
    btv.test.expect(t:line(3)).to_match("^   x")
    btv.test.expect(t:line(3)).never.to_contain("\t")
  end)

  btv.test.it("try-it 1 — the marker fold is closed on open, and zo/zc work", function(t)
    open(t, "sample.txt")
    local function fold_rows()
      local n = 0
      for _, text in ipairs(t:screen()) do
        if text:find("%d+ lines:") then
          n = n + 1
        end
      end
      return n
    end
    btv.test.expect(fold_rows()).to_be(1)
    -- The fold gutter is two cells wide (§2's foldcolumn = 2).
    btv.test.expect(btv.wo.foldcolumn).to_be(2)
    t:feed("5G")
    t:feed("zo")
    btv.test.expect(fold_rows()).to_be(0)
    t:feed("zc")
    btv.test.expect(fold_rows()).to_be(1)
  end)

  -- §2. The window options the config set through `vim.opt`.
  btv.test.it("§2 — the window options reached this window", function(t)
    open(t, "sample.txt")
    btv.test.expect(btv.wo.number).to_be(true)
    btv.test.expect(btv.wo.relativenumber).to_be(false)
    btv.test.expect(btv.wo.breakindent).to_be(true)
    btv.test.expect(btv.wo.showbreak).to_be("↪ ")
  end)

  -- 2. "The two tiers really are two values. `:setlocal` pins THIS buffer."
  btv.test.it("try-it 2 — :setlocal pins this buffer, the tier keeps its value", function(t)
    open(t, "sample.txt")
    t:cmd("setlocal tabstop=8")
    t:cmd("set tabstop?")
    btv.test.expect(t:message()).to_contain("tabstop=8")
    t:cmd("setglobal tabstop?")
    btv.test.expect(t:message()).to_contain("tabstop=3")
  end)

  btv.test.it("try-it 2 — a new buffer is born from the tier", function(t)
    open(t, "sample.txt")
    t:cmd("setlocal tabstop=8")
    btv.test.expect(btv.bo.tabstop).to_be(8)
    t:cmd("enew")
    t:cmd("set tabstop?")
    btv.test.expect(t:message()).to_contain("tabstop=3")
    btv.test.expect(btv.bo.tabstop).to_be(3)
  end)

  -- 3. "`vim.opt_global.commentstring` wrote the tier and no buffer … which every
  --     buffer with no `commentstring` of its own reads through"
  btv.test.it("try-it 3 — the tier is a read-time fallback for commentstring", function(t)
    open(t, "sample.txt")
    t:cmd("setglobal commentstring?")
    btv.test.expect(t:message()).to_contain("commentstring=## %s")
    t:feed("1Ggcc")
    btv.test.expect(t:line(1)).to_match("^## ")
    t:feed("u")
    -- …and a file opened afterwards follows it too.
    open(t, "other.txt")
    t:feed("1Ggcc")
    btv.test.expect(t:line(1)).to_match("^## ")
  end)

  btv.test.it("try-it 3 — :setlocal pins one buffer out of the tier's reach", function(t)
    open(t, "sample.txt")
    t:cmd([[setlocal commentstring=;;\ %s]])
    t:feed("1Ggcc")
    btv.test.expect(t:line(1)).to_match("^;; ")
    -- Everywhere else still follows the tier.
    open(t, "other.txt")
    t:feed("1Ggcc")
    btv.test.expect(t:line(1)).to_match("^## ")
  end)

  -- 4. "`vim.go` reads the global value, `vim.o` the local one"
  btv.test.it("try-it 4 — btv.o is the local read, btv.go the global one", function(t)
    open(t, "sample.txt")
    t:cmd("setlocal tabstop=8")
    btv.test.expect(vim.o.tabstop).to_be(8)
    btv.test.expect(vim.go.tabstop).to_be(3)
  end)

  -- §4. "the ftplugin case: one filetype's indent must not become everyone's default"
  btv.test.it("§4 — the FileType handler's :setlocal stays on that buffer", function(t)
    open(t, "sample.txt")
    t:cmd("e " .. DIR .. "/../folds/sample.lua")
    btv.test.expect(btv.bo.filetype).to_be("lua")
    btv.test.expect(btv.bo.tabstop).to_be(2)
    -- The tier is untouched, so nothing else moved.
    btv.test.expect(vim.go.tabstop).to_be(3)
    open(t, "other.txt")
    btv.test.expect(btv.bo.tabstop).to_be(3)
  end)

  -- 5. "Some buffer options have NO global value … bemtvi says so out loud"
  btv.test.it("try-it 5 — a read-decided option refuses a global write, loudly", function(t)
    open(t, "sample.txt")
    t:cmd("setglobal fileencoding=latin1")
    btv.test.expect(t:message()).to_contain("E5100")
    btv.test.expect(t:message()).to_contain("fileencoding")
    -- …and nothing was stored: the buffer keeps what its read decided.
    btv.test.expect(btv.bo.fileencoding).never.to_be("latin1")
  end)
end)
