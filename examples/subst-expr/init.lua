-- `:s/…/\=…/` — replacement expressions, evaluated in the bounded Lua sandbox.
--
-- Run this example with the config isolated from your real one:
--
--   BEMTVI_CONFIG=examples/subst-expr cargo run -p bemtvi -- examples/subst-expr/sample.txt
--
-- Every section below is *type this / see that*. The line numbers refer to
-- `sample.txt`, so the commands can be pasted as-is. `u` undoes each one.
--
-- You do not have to type them: `<leader>N` (leader is <Space> here) drops
-- demo N's command onto the command line, where you can read it, edit it, and
-- run it with <CR>. `:Cheat` lists them all. See the bottom of this file.
--
-- The replacement after `\=` is a Lua **expression** (not a statement block),
-- evaluated once per match with two things in scope:
--
--   m      the submatches — `m[0]` is the whole match, `m[1]`, `m[2]`, … the
--          capture groups. A group that did not participate is `nil`.
--   lnum   the 1-based line the match sits on.
--
-- Patterns are PCRE (bemtvi's `'regexsyntax'` default), so `\w+` is a bare `+`.

-- Line numbers make the `lnum` demo (section 4) legible.
btv.o.number = true

-- ===========================================================================
-- 1. `m[0]` is the whole match
--
--    type:  :4s/\w+/\=m[0]:upper()/g
--    see:   line 4 becomes  ALPHA BETA GAMMA
--
--    The `/g` flag matters: the expression runs once per *match*, not per line.

-- ===========================================================================
-- 2. Numbered groups, reordered
--
--    type:  :7,8s/(\w+)_(\w+)/\=m[2] .. "_" .. m[1]/
--    see:   one_two    -> two_one
--           three_four -> four_three

-- ===========================================================================
-- 3. Arithmetic on a captured number
--
--    type:  :11s/\d+/\=tonumber(m[0]) * 2/g
--    see:   item 7, item 21  ->  item 14, item 42
--
--    A number result is accepted and rendered the way Lua prints it, so this
--    does not come back as `14.0`.

-- ===========================================================================
-- 4. `lnum` — the line the match sits on
--
--    type:  :14,16s/tick/\=lnum/
--    see:   the three `tick` lines become  14, 15, 16

-- ===========================================================================
-- 5. A group that did not participate is `nil`, not ""
--
--    type:  :19s/(a)|(z)/\=type(m[2])/
--    see:   ab -> nilb
--
--    The second alternative never matched, so `m[2]` is absent from the table
--    and reads as `nil` — distinguishable from a group that matched empty.

-- ===========================================================================
-- 6. The sandbox is closed
--
--    type:  :22s/probe/\=type(io) .. " " .. type(require) .. " " .. type(btv)/
--    see:   probe -> nil nil nil
--
--    The expression runs in a second, tiny Lua VM with only the value-level
--    stdlib (`string`, `table`, `math`, `utf8`). There is no `io`, `os`,
--    `package`, `require`, `load`, `debug` — and no `btv.*`, so an expression
--    cannot reach editor state. `pcall` is absent too, on purpose: a deadline
--    unwinds as an ordinary error, and an expression able to catch one could
--    swallow its own time budget.

-- ===========================================================================
-- 7. Failure is loud — it never silently substitutes nothing
--
--    a syntax error, caught before the buffer is touched at all:
--      type:  :25s/alpha/\=m[/
--      see:   E1300: invalid expression: …          (line 25 unchanged)
--
--    an error raised at runtime, reported with the line it failed on:
--      type:  :25s/alpha/\=error("boom")/
--      see:   E1300: line 25: expression failed: boom
--
--    a result that is not a string or number, refused rather than coerced:
--      type:  :25s/alpha/\={}/
--      see:   E1300: … expected a string or number
--
--    and a runaway expression, abandoned at its deadline instead of hanging:
--      type:  :25s/alpha/\=(function() while true do end end)()/
--      see:   E1300: … exceeded its 50ms budget      (after ~50ms, not forever)

-- ===========================================================================
-- 8. The literal template form is untouched
--
--    type:  :28s/(\w+)_(\w+)/${2}_${1}/
--    see:   left_right -> right_left
--
--    Only a replacement *starting* with `\=` is an expression; everything else
--    is still the literal dialect. The braces are needed because `$2_` would
--    otherwise read as a group named `2_`.

-- ===========================================================================
-- Shortcuts: <leader>1 … <leader>8 put the matching command on the command
-- line, ready to read, edit and run with <CR>. Nothing executes on its own.
--
-- These are `<expr>` mappings, which is the point worth stealing: an `<expr>`
-- RHS *returns keys to feed* rather than performing an action, so returning
-- `":" .. cmd` opens the command line already filled in. It is the public way
-- to produce keystrokes from Lua — no reaching behind the API for a private
-- feedkeys.

btv.g.mapleader = " "

local DEMOS = {
  [[4s/\w+/\=m[0]:upper()/g]],
  [[7,8s/(\w+)_(\w+)/\=m[2] .. "_" .. m[1]/]],
  [[11s/\d+/\=tonumber(m[0]) * 2/g]],
  [[14,16s/tick/\=lnum/]],
  [[19s/(a)|(z)/\=type(m[2])/]],
  [[22s/probe/\=type(io) .. " " .. type(require) .. " " .. type(btv)/]],
  [[25s/alpha/\=error("boom")/]],
  [[28s/(\w+)_(\w+)/${2}_${1}/]],
}

for n, cmd in ipairs(DEMOS) do
  btv.keymap.set("n", "<leader>" .. n, function()
    return ":" .. cmd
  end, { expr = true, desc = "Demo " .. n .. " onto the command line" })
end

-- `:Cheat` lists them in a float — the message line would collapse the newlines
-- into `^J`, which is what the float surface is for.
local CHEATS = { "<leader>N puts the command on the cmdline; <CR> runs it.", "" }
for n, cmd in ipairs(DEMOS) do
  CHEATS[#CHEATS + 1] = string.format("  <leader>%d   :%s", n, cmd)
end

btv.command("Cheat", function()
  btv.ui.float(CHEATS, { title = " :s expression demos ", relative = "editor" })
end, { desc = "Show the :s expression demo commands" })
