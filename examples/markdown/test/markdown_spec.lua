-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/markdown
--
-- The float is a real window with a real buffer, so what it renders is
-- `t:lines()` and what it paints over that is `t:highlights()`. The point of the
-- demo is that the markdown is RENDERED — markers stripped, structure styled —
-- so the assertions compare the float's rows with the source.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample markdown, re-reading it so each test starts the same.
local function open(t)
  btv.layer.main()
  t:cmd("e " .. DIR .. "/sample.md")
  t:cmd("e!")
  t:feed("gg")
end

--- Close the float if one is up, so a test always starts from the buffer.
local function close_float(t)
  if btv.buf.name(0) == "[Rendered Markdown]" then
    t:feed("q")
  end
end

--- Open the rendered float and wait for its CONTENT — the buffer is named before
--- the render lands, so waiting on the name alone can read an empty float.
local function float(t)
  close_float(t)
  t:feed("K")
  t:wait_for(function()
    return btv.buf.name(0) == "[Rendered Markdown]" and #t:lines() > 1
  end, { message = "the markdown float never rendered" })
  return t:lines()
end

btv.test.describe("examples/markdown", function()
  -- Never leave a float open: the per-test baseline runs `enew!` in whatever
  -- window is current, which would blank the float's buffer for the next test.
  btv.test.after_each(function()
    if btv.buf.name(0) == "[Rendered Markdown]" then
      btv._feedkeys("q", true, false, true)
    end
  end)

  btv.test.it("K opens the rendered float over the buffer", function(t)
    open(t)
    local rendered = float(t)
    btv.test.expect(#rendered > 0).to_be(true)
    -- It is a float, so the source buffer is untouched behind it.
    t:feed("q")
    btv.test.expect(btv.bo.filetype).to_be("markdown")
  end)

  btv.test.it(":MarkdownFloat is the same command", function(t)
    open(t)
    t:cmd("MarkdownFloat")
    t:wait_for(function()
      return btv.buf.name(0) == "[Rendered Markdown]"
    end, { message = ":MarkdownFloat opened nothing" })
    t:feed("q")
  end)

  -- "`#`/`**`/fences stripped"
  btv.test.it("the markers are stripped from the rendered rows", function(t)
    open(t)
    local source = table.concat(t:lines(), "\n")
    local rows = float(t)
    local rendered = table.concat(rows, "\n")
    -- The source really does carry them…
    btv.test.expect(source).to_match("^#")
    btv.test.expect(source).to_contain("**")
    -- …and the rendering does not.
    btv.test.expect(rendered).never.to_contain("**")
    for _, row in ipairs(rows) do
      btv.test.expect(row:match("^#+%s")).to_be_nil()
    end
    t:feed("q")
  end)

  btv.test.it("the heading text itself survives the strip", function(t)
    open(t)
    local heading = (t:line(1) or ""):gsub("^#+%s*", "")
    local rendered = table.concat(float(t), "\n")
    btv.test.expect(rendered).to_contain(heading)
    t:feed("q")
  end)

  -- "bold/headings styled"
  btv.test.it("the structure is styled rather than spelled out", function(t)
    open(t)
    float(t)
    local groups = {}
    for row = 1, #t:lines() do
      for _, span in ipairs(t:highlights(row)) do
        groups[span[3]] = true
      end
    end
    -- Something was painted, and it is the markup vocabulary.
    local any_markup = false
    for group in pairs(groups) do
      if group:find("^@markup") then
        any_markup = true
      end
    end
    btv.test.expect(any_markup).to_be(true)
    t:feed("q")
  end)

  -- "a long document pages with the wheel or `j`/`k`/`<C-d>`/`<C-u>`"
  btv.test.it("the float scrolls like any window", function(t)
    open(t)
    float(t)
    btv.test.expect(t:view().topline).to_be(1)
    t:feed("<C-d>")
    btv.test.expect(t:view().topline > 1).to_be(true)
    t:feed("<C-u>")
    btv.test.expect(t:view().topline).to_be(1)
    t:feed("q")
  end)

  -- "`q` / `<Esc>` closes it"
  btv.test.it("q and <Esc> both close it", function(t)
    open(t)
    float(t)
    t:feed("q")
    t:wait_for(function()
      return btv.buf.name(0) ~= "[Rendered Markdown]"
    end, { message = "q did not close the float" })
    float(t)
    t:feed("<Esc>")
    t:wait_for(function()
      return btv.buf.name(0) ~= "[Rendered Markdown]"
    end, { message = "<Esc> did not close the float" })
  end)

  -- "ctx.wo.wrap = true — wrap long paragraphs within the float"
  btv.test.it("the float wraps its long paragraphs", function(t)
    open(t)
    float(t)
    btv.test.expect(btv.wo.wrap).to_be(true)
    t:feed("q")
  end)

  -- "mounted as a float … grab = true — modal: focus stays in the float"
  btv.test.it("the float is modal: focus stays in it", function(t)
    open(t)
    float(t)
    t:feed("<C-w>w")
    btv.test.expect(btv.buf.name(0)).to_be("[Rendered Markdown]")
    t:feed("<C-w>j")
    btv.test.expect(btv.buf.name(0)).to_be("[Rendered Markdown]")
    t:feed("q")
  end)

  -- "we mount the view `filetype = 'markdown'`, so the grammar's injections
  --  highlight the fenced code in its own language"
  btv.test.it("the float is a markdown buffer, so injections can run", function(t)
    open(t)
    float(t)
    btv.test.expect(btv.bo.filetype).to_be("markdown")
    -- The fences are kept in the text (hidden behind a blank overlay) — that is
    -- what lets the injection see a fenced block at all.
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("```")
    t:feed("q")
  end)

  -- The engine the demo calls directly.
  btv.test.it("btv.markdown.to_view returns view-ready lines and decor", function(t)
    open(t)
    local out = btv.markdown.to_view("# Title\n\nsome **bold** text\n")
    btv.test.expect(type(out.lines)).to_be("table")
    btv.test.expect(type(out.decor)).to_be("table")
    btv.test.expect(table.concat(out.lines, "\n")).to_contain("Title")
    btv.test.expect(table.concat(out.lines, "\n")).never.to_contain("**")
    btv.test.expect(#out.decor > 0).to_be(true)
  end)
end)
