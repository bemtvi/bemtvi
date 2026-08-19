-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/macros
--
-- All six numbered TRY-IT blocks, typed exactly as written. A macro register is
-- ordinary text, so what was recorded is asserted directly — and what a replay
-- did is asserted on the buffer.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Play a macro and let the whole run finish. A `{count}<F3>{reg}` expands into
--- as many keystrokes as the macro has, driven between ticks — so a replay needs
--- more than the one tick `t:feed` settles.
local function play(t, keys)
  t:feed(keys)
  t:sleep(30)
end

btv.test.describe("examples/macros", function()
  -- 1. "Record and replay a line edit."
  btv.test.it("try-it 1 — <F2>a records, <F2> stops, <F3>a replays", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>a")
    -- "the message line reads `recording @a`"
    btv.test.expect(t:message()).to_contain("recording @a")
    btv.test.expect(t:statusline()).to_contain("recording @a")
    t:feed("I- <Esc>j")
    t:feed("<F2>")
    btv.test.expect(t:line(1)).to_be("- alpha bravo charlie")
    -- "the announcement clears"
    btv.test.expect(t:statusline()).never.to_contain("recording")
    play(t, "<F3>a")
    btv.test.expect(t:line(2)).to_be("- delta echo foxtrot")
  end)

  btv.test.it("try-it 1 — a counted replay stops where the macro fails", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>aI- <Esc>j<F2>")
    play(t, "99<F3>a")
    -- Every line got the prefix, down to the last one — where `j` fails and the
    -- run ends rather than grinding the final line 90 more times.
    local n = #t:lines()
    for i = 1, n do
      btv.test.expect(t:line(i)).to_match("^%- ")
    end
    btv.test.expect(t:cursor()[1]).to_be(n)
  end)

  -- 2. "Look at what you recorded — it is just text in register `a`."
  btv.test.it("try-it 2 — the recording is readable key notation in a register", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>aI- <Esc>j<F2>")
    btv.test.expect(vim.fn.getreg("a")).to_be("I-<Space><Esc>j")
    t:cmd("registers a")
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("I-<Space><Esc>j")
    t:feed("q")
  end)

  btv.test.it("try-it 2 — \"ap pastes the keystrokes as text", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>aI- <Esc>j<F2>")
    t:feed('gg"ap')
    btv.test.expect(t:line(1)).to_contain("I-<Space><Esc>j")
    t:feed("u")
  end)

  -- 3. "The hand-written one from section 3, over the 'TODO' paragraph."
  btv.test.it("try-it 3 — the hand-written register b bolds a word", function(t)
    open(t)
    -- §3 wrote it with `btv.reg.set`, no recording session at all.
    btv.test.expect(vim.fn.getreg("b")).to_be("yiwciw**<C-r>0**<Esc>w")
    t:feed("/TODO<CR>")
    t:feed("<Space>b")
    btv.test.expect(t:line(8)).to_be("**TODO** write the release notes")
    -- "cursor on the next word"
    btv.test.expect(t:cursor()[2]).to_be(#"**TODO** ")
  end)

  btv.test.it("try-it 3 — <F3>b is the same macro, and takes a count", function(t)
    open(t)
    t:feed("/TODO<CR>")
    play(t, "<F3>b")
    btv.test.expect(t:line(8)).to_be("**TODO** write the release notes")
    play(t, "3<F3>b")
    btv.test.expect(t:line(8)).to_be("**TODO** **write** **the** **release** notes")
  end)

  -- 4. "Append to a recording (uppercase register)."
  btv.test.it("try-it 4 — an uppercase register APPENDS to the recording", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>cx<F2>")
    btv.test.expect(vim.fn.getreg("c")).to_be("x")
    t:feed("<F2>Cx<F2>")
    btv.test.expect(vim.fn.getreg("c")).to_be("xx")
    -- "register c now deletes two"
    t:cmd("e!")
    t:feed("gg")
    play(t, "<F3>c")
    btv.test.expect(t:line(1)).to_be("pha bravo charlie")
  end)

  -- 5. "Repeat the last ex command with <F3>:"
  btv.test.it("try-it 5 — <F3>: re-runs the last ex command", function(t)
    open(t)
    t:feed("gg")
    t:feed(":s/o/0/<CR>")
    btv.test.expect(t:line(1)).to_be("alpha brav0 charlie")
    play(t, "j<F3>:")
    btv.test.expect(t:line(2)).to_be("delta ech0 foxtrot")
    play(t, "j<F3>:")
    btv.test.expect(t:line(3)).to_be("g0lf hotel india")
  end)

  -- 6. "A macro can call a macro."
  btv.test.it("try-it 6 — a macro can call a macro", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>d")
    t:feed("<F3>bj0")
    t:feed("<F2>")
    btv.test.expect(vim.fn.getreg("d")).to_contain("<F3>b")
    t:cmd("e!")
    t:feed("gg")
    play(t, "5<F3>d")
    for i = 1, 5 do
      btv.test.expect(t:line(i)).to_match("^%*%*")
    end
    btv.test.expect(t:line(6)).never.to_match("^%*%*")
  end)

  -- "<F3><F3> = last"
  btv.test.it("<F3><F3> replays the last macro", function(t)
    open(t)
    t:feed("gg")
    t:feed("<F2>aI- <Esc>j<F2>")
    play(t, "<F3>a")
    btv.test.expect(t:line(2)).to_match("^%- ")
    play(t, "<F3><F3>")
    btv.test.expect(t:line(3)).to_match("^%- ")
  end)

  -- §2. "`macro` is a built-in segment; it renders `recording @a` while a
  --      recording is open and nothing otherwise."
  btv.test.it("§2 — the macro statusline segment is in the layout", function(t)
    open(t)
    btv.test.expect(t:statusline()).never.to_contain("recording")
    t:feed("<F2>z")
    btv.test.expect(t:statusline()).to_contain("recording @z")
    t:feed("<F2>")
    btv.test.expect(t:statusline()).never.to_contain("recording")
  end)

  -- §4. "`btv.macro.executing()` is the cheap way for a plugin to skip work no
  --      human is watching."
  btv.test.it("§4 — btv.macro.executing() is true only during a replay", function(t)
    open(t)
    btv.test.expect(btv.macro.executing()).to_be_nil()
    btv.g.macro_moves = 0
    t:feed("gg")
    t:feed("<F2>ajjj<F2>")
    -- Recording is not executing, so the counter stayed put.
    btv.test.expect(btv.g.macro_moves).to_be(0)
    play(t, "gg<F3>a")
    btv.test.expect(btv.g.macro_moves > 0).to_be(true)
    btv.test.expect(btv.macro.executing()).to_be_nil()
  end)

  -- §1. "Mapping them back works because a recording captures what you TYPED."
  btv.test.it("§1 — q and @ can be mapped back to the vim spelling", function(t)
    open(t)
    btv.keymap.set("n", "q", "<F2>", { desc = "Record macro (vim's q)" })
    btv.keymap.set("n", "@", "<F3>", { desc = "Play macro (vim's @)" })
    t:feed("gg")
    t:feed("qaI- <Esc>jq")
    btv.test.expect(vim.fn.getreg("a")).to_be("I-<Space><Esc>j")
    play(t, "@a")
    btv.test.expect(t:line(2)).to_match("^%- ")
  end)
end)
