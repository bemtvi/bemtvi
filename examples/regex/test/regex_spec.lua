-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/regex
--
-- Every command in the tour reports through `vim.notify`, so the spec swaps in a
-- recorder for the duration of each command and asserts on what it was handed —
-- the exact text the notes promise a reader will see.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample buffer at the top. `:e` then `:e!` so a test that edited the
--- sample hands the next one the file back off disk.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Run `body` with `vim.notify` / `btv.notify` recording instead of reporting,
--- and return everything it was handed joined by newlines. Both spellings are
--- swapped: they are separate bindings, and the config calls the `vim.` one.
local function notified(body)
  local got = {}
  local prev_vim, prev_btv = vim.notify, btv.notify
  local record = function(msg)
    got[#got + 1] = tostring(msg)
  end
  vim.notify, btv.notify = record, record
  local ok, err = pcall(body)
  vim.notify, btv.notify = prev_vim, prev_btv
  if not ok then
    error(err, 0)
  end
  return table.concat(got, "\n")
end

btv.test.describe("examples/regex", function()
  -- ":Emails -> the two addresses, with user/host captures"
  btv.test.it(":Emails lists every address with its two captures", function(t)
    open(t)
    local out = notified(function()
      t:cmd("Emails")
    end)
    btv.test.expect(out).to_contain("found 2 email(s)")
    btv.test.expect(out).to_contain('jane.doe@acme.io   (user="jane.doe" host="acme.io")')
    btv.test.expect(out).to_contain('(user="j.smith+work" host="mail.example.com")')
    -- The "(no address)" line contributes nothing.
    btv.test.expect(out).never.to_contain("no address")
  end)

  btv.test.it(":Emails says so when the buffer holds none", function(t)
    open(t)
    t:cmd("enew!")
    local out = notified(function()
      t:cmd("Emails")
    end)
    btv.test.expect(out).to_be("no emails found")
  end)

  -- ":Numbers -> the sum of every integer in the buffer"
  btv.test.it(":Numbers sums every integer in the buffer", function(t)
    open(t)
    -- Every digit run in sample.txt, in buffer order.
    local want = 0
    for _, line in ipairs(btv.buf.lines(0, 0, -1)) do
      for n in line:gmatch("%d+") do
        want = want + tonumber(n)
      end
    end
    local out = notified(function()
      t:cmd("Numbers")
    end)
    btv.test.expect(out).to_be(("sum of every number in the buffer: %d"):format(want))
    -- …and that total is a real one, not zero on an empty walk.
    btv.test.expect(want > 0).to_be(true)
  end)

  -- ":Phones -> the two phone-number lines"
  btv.test.it(":Phones reports the lines a \\d{3}-\\d{3}-\\d{4} matches", function(t)
    open(t)
    local out = notified(function()
      t:cmd("Phones")
    end)
    btv.test.expect(out).to_contain("555-123-4567")
    btv.test.expect(out).to_contain("555-987-6543")
    -- The bare year-and-month digits on the "Order 4090" line are not a phone.
    btv.test.expect(out).never.to_contain("4090")
  end)

  -- "<cursor on the phone line> :Redact -> call ***-***-**** before noon"
  btv.test.it(":Redact masks the digits of the cursor line", function(t)
    open(t)
    t:feed("/call 555<CR>")
    local out = notified(function()
      t:cmd("Redact")
    end)
    btv.test.expect(out).to_contain("call ***-***-**** before noon")
    btv.test.expect(out).to_contain("10 digit(s) masked")
    -- A preview only: the buffer still holds the digits.
    btv.test.expect(t:line(t:cursor()[1])).to_contain("555-123-4567")
  end)

  -- Section 2 of the notes: the object API, exactly as it is spelled there.
  btv.test.it("the object API answers what the notes print", function(t)
    open(t)
    btv.test.expect(btv.regex([[\d+]]):match("order 4090 ships")).to_be("4090")
    local user, host = btv.regex([[(\w+)@(\w+)]]):match("a@b")
    btv.test.expect(user).to_be("a")
    btv.test.expect(host).to_be("b")
    btv.test.expect(btv.regex([[^\d{3}-\d{4}$]]):test("555-1234")).to_be(true)
    local s, e = btv.regex([[\bbeta\b]]):find("alpha beta")
    btv.test.expect(s).to_be(7)
    btv.test.expect(e).to_be(10)
    local squeezed, n = btv.regex([[\s+]]):gsub("a   b   c", " ")
    btv.test.expect(squeezed).to_be("a b c")
    btv.test.expect(n).to_be(2)
  end)

  -- Section 3: the three dialects the notes offer.
  btv.test.it("the engine option picks the dialect", function(t)
    open(t)
    -- pcre (the default) has no `\zs`; vim's does, and it moves the match start.
    local re = btv.regex([[foo\zsbar]], { engine = "vim" })
    local s, e = re:find("a foobar b")
    btv.test.expect(s).to_be(6)
    btv.test.expect(e).to_be(8)
    -- `plain` takes the pattern literally — `.` is a dot, not any character.
    btv.test.expect(btv.regex("a.c", { plain = true }):test("abc")).to_be(false)
    btv.test.expect(btv.regex("a.c", { plain = true }):test("a.c")).to_be(true)
  end)
end)
