-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/regexsyntax
--
-- Every case below types exactly what section A–G of the notes tells a reader to
-- type, and asserts the line it promises to leave behind.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample buffer at the top, off disk, on the default dialect. The
--- explicit `&` matters: `regexsyntax` is buffer-local, and the per-test baseline
--- restores the global and window-local options — not a buffer's own overrides.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set regexsyntax=pcre")
  t:cmd("setlocal regexsyntax&")
  t:feed("gg")
end

btv.test.describe("examples/regexsyntax", function()
  btv.test.it("the default dialect is pcre", function(t)
    open(t)
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=pcre")
  end)

  -- A) "the PCRE default — `$1` capture refs, canonical groups."
  btv.test.it("A — pcre takes bare groups and $1 replacement refs", function(t)
    open(t)
    t:cmd("2")
    t:cmd([[s/(\w+) (\w+)/$2 $1/]])
    btv.test.expect(t:line(2)).to_be("world hello")
  end)

  btv.test.it("A — vim's escaped groups are not a pcre pattern", function(t)
    open(t)
    -- In PCRE `\(\w\+\)` is a literal `(`, a word character and a literal `+`,
    -- so the substitution finds nothing and the line is untouched.
    t:cmd("2")
    t:cmd([[s/\(\w\+\) \(\w\+\)/\2 \1/]])
    btv.test.expect(t:line(2)).to_be("hello world")
    btv.test.expect(t:message()).to_contain("E486")
  end)

  -- B) "switch to vim's dialect."
  btv.test.it("B — :set regexsyntax=vim switches this buffer", function(t)
    open(t)
    t:cmd("set regexsyntax=vim")
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=vim")
  end)

  btv.test.it("B — an unknown dialect fails loud", function(t)
    open(t)
    t:cmd("set regexsyntax=perl")
    btv.test.expect(t:message()).to_contain("E474")
    -- …and leaves the buffer on the dialect it had.
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=pcre")
  end)

  -- C) "word boundaries — `\<`/`\>` match a *whole* word."
  btv.test.it("C — \\<foo\\> skips the foo inside foobar", function(t)
    open(t)
    t:cmd("set regexsyntax=vim")
    t:feed("gg0")
    t:feed([[/\<foo\><CR>]])
    btv.test.expect(t:cursor()[1]).to_be(1)
    -- 0-based column 11 — the standalone `foo`, not the one inside `foobar`.
    btv.test.expect(t:cursor()[2]).to_be(11)
  end)

  -- D) "vim groups + back-refs + `&` (whole match) in the replacement."
  btv.test.it("D — vim groups swap the words and \\u& title-cases them", function(t)
    open(t)
    t:cmd("set regexsyntax=vim")
    t:cmd("2")
    t:cmd([[s/\(\w\+\) \(\w\+\)/\2 \1/]])
    btv.test.expect(t:line(2)).to_be("world hello")
    t:cmd([[s/\w\+/\u&/g]])
    btv.test.expect(t:line(2)).to_be("World Hello")
  end)

  -- E) "the non-greedy `\{-}`."
  btv.test.it("E — \\{-} stops at the first `::`", function(t)
    open(t)
    t:cmd("set regexsyntax=vim")
    t:cmd("4")
    t:cmd([[s/zipfile:\/\/\(.\{-}\)::.*/\1/]])
    btv.test.expect(t:line(4)).to_be("/path/to/a")
  end)

  btv.test.it("E — the greedy `.*` would take the last one instead", function(t)
    open(t)
    t:cmd("set regexsyntax=vim")
    t:cmd("4")
    t:cmd([[s/zipfile:\/\/\(.*\)::.*/\1/]])
    btv.test.expect(t:line(4)).to_be("/path/to/a::b")
  end)

  -- F) "flip back any time."
  btv.test.it("F — :set rxs& drops the override back to the global default", function(t)
    open(t)
    t:cmd("set regexsyntax=vim")
    t:cmd("set regexsyntax&")
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=pcre")
    -- …and the pcre pattern works again.
    t:cmd("2")
    t:cmd([[s/(\w+) (\w+)/$2 $1/]])
    btv.test.expect(t:line(2)).to_be("world hello")
  end)

  -- G) "per-buffer: pin one buffer to vim, leave the next on the global default."
  btv.test.it("G — the override is per buffer, not editor-wide", function(t)
    open(t)
    -- `:setlocal`, so the global default stays pcre for the next buffer.
    t:cmd("setlocal regexsyntax=vim")
    local pinned = btv.buf.current()
    t:cmd("enew")
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=pcre")
    t:cmd("buffer " .. pinned)
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=vim")
  end)

  -- "A FileType autocmd is the idiomatic place to pin a dialect per buffer."
  btv.test.it("the config's FileType autocmd pins vim-script buffers", function(t)
    open(t)
    t:cmd("enew")
    t:cmd("set filetype=vim")
    t:cmd("set rxs?")
    btv.test.expect(t:message()).to_contain("regexsyntax=vim")
  end)

  -- "the `vim.fn.substitute()` Lua function ALWAYS speaks vim's dialect."
  btv.test.it("vim.fn.substitute stays on vim's dialect either way", function(t)
    open(t)
    t:cmd("set regexsyntax=pcre")
    btv.test
      .expect(vim.fn.substitute("hello world", [[\(\w\+\) \(\w\+\)]], [[\2 \1]], ""))
      .to_be("world hello")
  end)
end)
