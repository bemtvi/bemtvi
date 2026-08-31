-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/glob
--
-- Every numbered command is run and its report read back — but the sharp
-- assertions are on `btv.glob` itself, since what the demo is teaching is a set of
-- exact answers (`*` stops at `/`, path style is an option, a set is one pass).

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- Every command reports through `vim.notify`; record it at the source, since the
-- reports are multi-line and the message line holds only the last of them.
local notified = {}
do
  local real = vim.notify
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

--- Open the sample path list, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/glob", function()
  btv.test.it("the config announces its commands", function(t)
    local banner
    for _, m in ipairs(notified) do
      if m:find("btv.glob example loaded", 1, true) then
        banner = m
      end
    end
    btv.test.expect(banner).never.to_be_nil()
    for _, cmd in ipairs({
      ":Glob",
      ":GlobRegex",
      ":GlobStar",
      ":GlobWindows",
      ":GlobIgnore",
      ":GlobBench",
      ":GlobIsGlob",
    }) do
      btv.test.expect(banner).to_contain(cmd)
    end
  end)

  -- 1. ":Glob **/*.rs" and ":Glob *.rs  (note: matches ONLY the root-level .rs files)"
  btv.test.it("§1 — :Glob splits the buffer's paths into matched and not", function(t)
    open(t)
    t:cmd("Glob **/*.rs")
    local report = last_notify()
    btv.test.expect(report).to_contain("main.rs")
    btv.test.expect(report).to_contain("matched")
    btv.test.expect(report).to_contain("not matched")
  end)

  btv.test.it("§1 — `*` stops at `/`, `**` crosses it", function(t)
    open(t)
    -- The claim, made exactly: a nested path matches only the `**` form.
    btv.test.expect(btv.glob.match("*.rs", "main.rs")).to_be(true)
    btv.test.expect(btv.glob.match("*.rs", "src/main.rs")).to_be(false)
    btv.test.expect(btv.glob.match("**/*.rs", "src/main.rs")).to_be(true)
    -- "`**` crosses separators, and spans zero of them too"
    btv.test.expect(btv.glob.match("**/*.rs", "main.rs")).to_be(true)
  end)

  btv.test.it("§1 — brace alternation and a negated class work", function(t)
    open(t)
    btv.test.expect(btv.glob.match("src/**/*.{rs,toml}", "src/a/b.rs")).to_be(true)
    btv.test.expect(btv.glob.match("src/**/*.{rs,toml}", "src/a/b.toml")).to_be(true)
    btv.test.expect(btv.glob.match("src/**/*.{rs,toml}", "src/a/b.lua")).to_be(false)
    btv.test.expect(btv.glob.match("**/[!_]*.lua", "conf/init.lua")).to_be(true)
    btv.test.expect(btv.glob.match("**/[!_]*.lua", "conf/_private.lua")).to_be(false)
  end)

  -- 2. ":GlobRegex — show the regex a glob translates to … how the `*` case carries
  --     a `[^/]` separator exclusion that the `**` case does not"
  btv.test.it("§2 — :GlobRegex shows the compiled regex, both ways", function(t)
    open(t)
    t:cmd("GlobRegex *.lua")
    local report = last_notify()
    btv.test.expect(report).to_contain("translates to")
    btv.test.expect(report).to_contain("literal_separator = false")
    -- The default carries the separator exclusion; the opt-out does not.
    btv.test.expect(btv.glob.to_regex("*.lua")).to_contain("[^/]")
    btv.test
      .expect(btv.glob.to_regex("*.lua", { literal_separator = false })).never
      .to_contain("[^/]")
  end)

  -- 3. ":GlobStar — the `*`-stops-at-`/` default, side by side with its two escapes"
  btv.test.it("§3 — :GlobStar reports one path judged four ways", function(t)
    open(t)
    t:cmd("GlobStar")
    local report = last_notify()
    btv.test.expect(report).to_contain("conf/nvim/init.lua")
    -- The four verdicts the report prints, asserted directly.
    local path = "conf/nvim/init.lua"
    btv.test.expect(btv.glob.match("*.lua", path)).to_be(false)
    btv.test.expect(btv.glob.match("**/*.lua", path)).to_be(true)
    btv.test.expect(btv.glob.match("*.lua", path, { basename = true })).to_be(true)
    btv.test.expect(btv.glob.match("*.lua", path, { literal_separator = false })).to_be(true)
  end)

  -- 4. ":GlobWindows — path style is an OPTION, not the machine you're on."
  btv.test.it("§4 — the same call gives opposite answers per path style", function(t)
    open(t)
    t:cmd("GlobWindows")
    btv.test.expect(last_notify()).to_contain("windows style")
    local win, uni = { style = "windows" }, { style = "unix" }
    btv.test.expect(btv.glob.match([[src\*.rs]], [[src\main.rs]], win)).to_be(true)
    -- "spellings normalize"
    btv.test.expect(btv.glob.match("src/*.rs", [[src\main.rs]], win)).to_be(true)
    -- "`*` stops at `\` too"
    btv.test.expect(btv.glob.match([[src\*.rs]], [[src\a\main.rs]], win)).to_be(false)
    -- "drive letter is a component"
    btv.test.expect(btv.glob.match([[C:/**/*.txt]], [[C:\Users\me\a.txt]], win)).to_be(true)
    -- unix style: `\` is the ESCAPE character
    btv.test.expect(btv.glob.match([[a\*b]], "a*b", uni)).to_be(true)
    btv.test.expect(btv.glob.match([[a\*b]], "axyzb", uni)).to_be(false)
  end)

  -- 5. ":GlobIgnore — one compiled RegexSet tests a path against every pattern in
  --     one pass, and `:matches` says which ones hit"
  btv.test.it("§5 — a glob set reports which patterns hit", function(t)
    open(t)
    t:cmd("GlobIgnore")
    local report = last_notify()
    btv.test.expect(report).to_contain("ignore set:")
    btv.test.expect(report).to_contain("kept")
    btv.test.expect(report).to_contain("dropped")
    local set = btv.glob.set({ "**/target/**", "**/.git/**", "**/*.tmp" })
    btv.test.expect(set:test("target/debug/x")).to_be(true)
    btv.test.expect(set:matches("target/debug/x")).to_equal({ 1 })
    btv.test.expect(set:matches("notes.tmp")).to_equal({ 3 })
    btv.test.expect(set:matches("src/main.rs")).to_equal({})
    btv.test.expect(set:patterns()[2]).to_be("**/.git/**")
  end)

  -- 6. ":GlobBench — 50k one-shot matches … watch it finish in milliseconds"
  btv.test.it("§6 — the compile cache makes 50k matches cheap", function(t)
    open(t)
    t:cmd("GlobBench")
    local report = last_notify()
    btv.test.expect(report).to_contain("50000 matches")
    btv.test.expect(report).to_contain("one parse + one regex build total")
    -- Every one of them a hit, which is what makes the count meaningful.
    btv.test.expect(report).to_match("^50000 matches")
  end)

  -- 7. ":GlobIsGlob — the canonical 'is this a pattern or a plain path' predicate"
  btv.test.it("§7 — is_glob tells a pattern from a plain path", function(t)
    open(t)
    t:cmd("GlobIsGlob")
    btv.test.expect(last_notify()).to_contain("is_glob:")
    btv.test.expect(btv.glob.is_glob("src/main.rs")).to_be(false)
    btv.test.expect(btv.glob.is_glob("**/*.rs")).to_be(true)
    btv.test.expect(btv.glob.is_glob("a?b")).to_be(true)
    btv.test.expect(btv.glob.is_glob("a{b,c}")).to_be(true)
    btv.test.expect(btv.glob.is_glob("[abc]")).to_be(true)
  end)

  -- The commands read the BUFFER, so editing it changes the answers.
  btv.test.it("the commands match against the buffer, so an edit changes them", function(t)
    open(t)
    t:feed("Gozz-added-by-the-spec.rs<Esc>")
    t:cmd("Glob **/*.rs")
    btv.test.expect(last_notify()).to_contain("zz-added-by-the-spec.rs")
    -- …and comment lines are skipped, as the sample says.
    btv.test.expect(last_notify()).never.to_contain("# A list of paths")
  end)
end)
