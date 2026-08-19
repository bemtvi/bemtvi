-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/helix
--
-- Helix is selection-first, so what a key DID is read off the selection it left
-- behind and the text it changed — every TRY-IT line is typed as written.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample in HELIX mode, re-reading it so each test starts the same.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  btv.helix.enable()
  t:feed("gg")
end

--- The text of the primary selection.
local function selected(t)
  return btv.win.selection_text and btv.win.selection_text(0) or nil
end

btv.test.describe("examples/helix", function()
  btv.test.it("§1 — the config turns the model on", function(t)
    open(t)
    btv.test.expect(t:mode()).to_be("hn")
    btv.test.expect(t:statusline():upper()).to_contain("HELIX")
  end)

  -- "a bare cursor is a width-1 selection"
  btv.test.it("every cursor is a selection", function(t)
    open(t)
    t:feed("3G")
    t:feed("0")
    -- `d` with no motion deletes the selection — one character.
    local before = t:line(3)
    t:feed("d")
    btv.test.expect(t:line(3)).to_be(before:sub(2))
  end)

  -- "w -> selects the word + trailing space, up to the next word"
  btv.test.it("try-it — a motion re-selects, and a verb acts on it now", function(t)
    open(t)
    t:feed("3G0")
    t:feed("wd")
    -- `The ` went, in two keystrokes, with no operator-pending wait.
    btv.test.expect(t:line(3)).to_be("quick brown fox jumps over the lazy dog.")
  end)

  btv.test.it("try-it — wc changes the selection and <Esc> resumes HELIX", function(t)
    open(t)
    t:feed("3G0")
    t:feed("wcXyz <Esc>")
    btv.test.expect(t:line(3)).to_be("Xyz quick brown fox jumps over the lazy dog.")
    btv.test.expect(t:mode()).to_be("hn")
  end)

  btv.test.it("try-it — wy yanks the selection", function(t)
    open(t)
    t:feed("3G0")
    t:feed("wy")
    btv.test.expect(vim.fn.getreg('"')).to_contain("The")
  end)

  -- "~ -> switch case of the selection"
  btv.test.it("try-it — ~ switches the selection's case", function(t)
    open(t)
    t:feed("3G0")
    t:feed("w~")
    btv.test.expect(t:line(3)).to_match("^tHE")
  end)

  -- "x -> select the whole line; x again -> grow one line down"
  btv.test.it("try-it — x selects the line, and again grows it downward", function(t)
    open(t)
    t:feed("3G0")
    t:feed("x")
    -- The head sits at the end of line 3: the whole line is the selection.
    btv.test.expect(t:cursor()[1]).to_be(3)
    btv.test.expect(t:cursor()[2]).to_be(#t:line(3) - 1)
    -- A verb acts on it now: the line's text goes.
    t:feed("d")
    btv.test.expect(t:line(3)).to_be("")
    btv.test.expect(t:line(4)).to_be("The quick brown fox jumps over the lazy dog.")
  end)

  btv.test.it("try-it — a second x grows the selection one line down", function(t)
    open(t)
    t:feed("3G0")
    t:feed("x")
    btv.test.expect(t:cursor()[1]).to_be(3)
    t:feed("x")
    btv.test.expect(t:cursor()[1]).to_be(4)
  end)

  -- §2. "X as an alias for x … this user map wins over any built-in"
  btv.test.it("§2 — the rebound X does what x does", function(t)
    open(t)
    t:feed("3G0")
    t:feed("X")
    btv.test.expect(t:cursor()[1]).to_be(3)
    btv.test.expect(t:cursor()[2]).to_be(#t:line(3) - 1)
    t:feed("d")
    btv.test.expect(t:line(3)).to_be("")
  end)

  -- §3. "gm jumps to the last line (like the built-in ge)"
  btv.test.it("§3 — the added gm goto entry reaches the last line", function(t)
    open(t)
    t:feed("gm")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    -- …and the built-in it copies does the same.
    t:feed("gg")
    btv.test.expect(t:cursor()[1]).to_be(1)
    t:feed("ge")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
  end)

  -- "gh / gl -> go to line start / end"
  btv.test.it("try-it — gh and gl move within the line", function(t)
    open(t)
    t:feed("3G")
    t:feed("gl")
    btv.test.expect(t:cursor()[2] > 0).to_be(true)
    t:feed("gh")
    btv.test.expect(t:cursor()[2]).to_be(0)
  end)

  -- "% -> select the whole file … d -> delete them all at once"
  btv.test.it("try-it — % selects the whole file", function(t)
    open(t)
    t:feed("%")
    t:feed("d")
    btv.test.expect(t:lines()).to_equal({ "" })
  end)

  -- "s the <CR> -> spawn one selection per match … d -> delete them all at once"
  btv.test.it("try-it — s splits the selection into one per match", function(t)
    open(t)
    t:feed("%")
    t:feed("sthe<CR>")
    t:feed("d")
    local text = table.concat(t:lines(), "\n")
    -- Every match went, in ONE verb — including the capitalised ones, since the
    -- split is smart-case like the search.
    btv.test.expect(text).never.to_contain("the ")
    btv.test.expect(text).never.to_contain("The quick")
    btv.test.expect(text).to_contain("quick brown fox")
  end)

  -- "/the <CR> -> jump to the next 'the' and select it"
  btv.test.it("try-it — search selects the whole match", function(t)
    open(t)
    t:feed("gg")
    t:feed("/the<CR>")
    -- The whole MATCH is the selection, not a point cursor — so `d` removes
    -- exactly it, and nothing else on the line.
    t:feed("d")
    btv.test.expect(t:line(3)).to_be(" quick brown fox jumps over the lazy dog.")
  end)

  -- "Search is smart-case here (a lowercase /the matches 'The')"
  btv.test.it("try-it — search is smart-case by default", function(t)
    open(t)
    t:feed("gg")
    t:feed("/helix<CR>")
    -- The sample's first line says "Helix"; a lowercase pattern found it.
    btv.test.expect(t:cursor()[1]).to_be(1)
    -- …and it can be turned off, as the notes say.
    btv.helix.smart_case(false)
    t:feed("gg")
    t:feed("/helix<CR>")
    btv.test.expect(t:message()).to_contain("attern not found")
    btv.helix.smart_case(true)
  end)

  -- "i / a -> insert before / append after the selection"
  btv.test.it("try-it — i and a enter insert at the selection's edges", function(t)
    open(t)
    t:feed("3G0")
    t:feed("w")
    t:feed("i<[")
    t:feed("<Esc>")
    btv.test.expect(t:line(3)).to_match("^<%[The")
    open(t)
    t:feed("3G0")
    t:feed("w")
    t:feed("a]>")
    t:feed("<Esc>")
    btv.test.expect(t:line(3)).to_match("^The ]>")
  end)

  -- "u / U -> undo / redo"
  btv.test.it("try-it — u undoes and U redoes", function(t)
    open(t)
    local before = t:line(3)
    t:feed("3G0wd")
    btv.test.expect(t:line(3)).never.to_be(before)
    t:feed("u")
    btv.test.expect(t:line(3)).to_be(before)
    t:feed("U")
    btv.test.expect(t:line(3)).never.to_be(before)
  end)

  -- ":helix toggles it interactively … Leave it off and bemtvi stays a plain vim."
  btv.test.it("try-it — :helix toggles back to vim, and back again", function(t)
    open(t)
    btv.test.expect(t:mode()).to_be("hn")
    t:cmd("helix")
    btv.test.expect(t:mode()).to_be("n")
    -- Plain vim again: `d` waits for a motion.
    t:feed("3G0dw")
    btv.test.expect(t:line(3)).to_be("quick brown fox jumps over the lazy dog.")
    t:cmd("helix")
    btv.test.expect(t:mode()).to_be("hn")
  end)
end)
