-- ~~~ bemtvi btv.regex: a real regex engine for Lua strings ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/regex \
--       cargo run -p bemtvi -- examples/regex/sample.txt
--
-- Lua's built-in patterns are NOT regexes: no alternation, no `\d`/`\w` classes,
-- no lazy quantifiers, and magic characters that surprise you. `btv.regex(pattern,
-- opts)` compiles a real regex — the Rust `regex` crate by default
-- (`engine = "pcre"`), or the vim engine (`engine = "vim"`) — into a reusable
-- object whose methods mirror the `string` library:
--
--     re:find(s, init?)    -> start, end, caps…   (1-based, like string.find)
--     re:match(s, init?)   -> the capture(s)/match (like string.match)
--     re:gmatch(s)         -> iterator             (like string.gmatch)
--     re:gsub(s, repl, n?) -> newstring, count     (like string.gsub)
--     re:test(s)           -> boolean
--
-- Offsets are 1-based and byte-based, so `s:sub(re:find(s))` is the matched text.
-- btv.regex matches strings you already hold in Lua (no copy across the bridge) —
-- it does not edit the buffer. To search *buffer* text line by line in Rust, use
-- `btv.buf.search`. These commands read the buffer and report via `vim.notify`.

-- A reusable compiled pattern (compile once, use it many times). The two groups
-- capture the user and host parts of an address.
local email = btv.regex([[([\w.+-]+)@([\w-]+\.[\w.-]+)]])

-- :Emails — list every address in the buffer, with its user/host captures, using
-- :gmatch over each line. The two capture groups arrive per match.
vim.api.nvim_create_user_command("Emails", function()
  local found = {}
  for _, line in ipairs(btv.buf.lines(0, 0, -1)) do
    for user, host in email:gmatch(line) do
      found[#found + 1] = ("  %s@%s   (user=%q host=%q)"):format(user, host, user, host)
    end
  end
  vim.notify(#found == 0 and "no emails found"
    or ("found %d email(s):\n%s"):format(#found, table.concat(found, "\n")))
end, { desc = "List every email in the buffer (btv.regex :gmatch)" })

-- :Numbers — sum every integer in the buffer. `\d+` is a real digit run (Lua's
-- `%d+` works here too, but this shows :gmatch + tonumber over the whole buffer).
local int = btv.regex([[\d+]])
vim.api.nvim_create_user_command("Numbers", function()
  local total = 0
  for _, line in ipairs(btv.buf.lines(0, 0, -1)) do
    for n in int:gmatch(line) do
      total = total + tonumber(n)
    end
  end
  vim.notify(("sum of every number in the buffer: %d"):format(total))
end, { desc = "Sum every integer in the buffer (btv.regex :gmatch)" })

-- :Phones — report the lines that contain a phone number, with :test (does it
-- match) and :match (pull the matched text). A real `\d{3}` quantifier.
local phone = btv.regex([[\d{3}-\d{3}-\d{4}]])
vim.api.nvim_create_user_command("Phones", function()
  local hits = {}
  local rows = btv.buf.lines(0, 0, -1)
  for i, line in ipairs(rows) do
    if phone:test(line) then
      hits[#hits + 1] = ("  line %d: %s"):format(i, (phone:match(line)))
    end
  end
  vim.notify(#hits == 0 and "no phone numbers"
    or ("phone numbers:\n" .. table.concat(hits, "\n")))
end, { desc = "Find phone-number lines (btv.regex :test / :match)" })

-- :Redact — show the current line with its digits masked, a display-sanitization
-- preview built with :gsub (the count comes back as the second return value).
local digit = btv.regex([[\d]])
vim.api.nvim_create_user_command("Redact", function()
  local row = vim.api.nvim_win_get_cursor(0)[1]
  local line = btv.buf.lines(0, row - 1, row)[1] or ""
  local masked, n = digit:gsub(line, "*")
  vim.notify(("line %d, %d digit(s) masked:\n  %s"):format(row, n, masked))
end, { desc = "Preview the current line with digits masked (btv.regex :gsub)" })

--------------------------------------------------------------------------------
-- Try it (in sample.txt):
--
-- 1. Commands (each notifies; see :messages for the history):
--      :Emails    -> the two addresses, with user/host captures
--      :Numbers   -> the sum of every integer in the buffer
--      :Phones    -> the two phone-number lines
--      <cursor on the phone line>  :Redact   -> "call ***-***-**** before noon"
--
-- 2. The object API, straight from the cmdline (`:lua`):
--      :lua print(btv.regex([[\d+]]):match("order 4090 ships"))     -> 4090
--      :lua print(btv.regex([[(\w+)@(\w+)]]):match("a@b"))          -> a   b
--      :lua print(btv.regex([[^\d{3}-\d{4}$]]):test("555-1234"))    -> true
--      :lua local s,e = btv.regex([[\bbeta\b]]):find("alpha beta")
--           print(s, e)                                            -> 7   10
--      :lua print(btv.regex([[\s+]]):gsub("a   b   c", " "))        -> a b c   2
--
-- 3. Pick the dialect:
--      pcre (default): named groups, alternation, lazy quantifiers, `\d` `\w` …
--      vim:            `btv.regex([[foo\zsbar]], { engine = "vim" })` — `\zs`/`\ze`,
--                      look-around, in-pattern back-refs Just Work.
--      plain:          `btv.regex("a.c", { plain = true })` — literal, no metachars.
--------------------------------------------------------------------------------
