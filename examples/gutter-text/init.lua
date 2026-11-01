-- ~~~ bemtvi gutter text: a column of a plugin's own text, beside the line numbers ~~~
--
-- `gutter_text` is an extmark key: a list of `{ text, group? }` chunks that fill the
-- gutter text column on the mark's line. The column sits between the line numbers and
-- the text, and it is as wide as the widest `gutter_text` in the buffer, so a shorter
-- one is padded to line up. It is not part of the line: the text keeps its own columns,
-- so a sideways scroll, a click, `$` and wrapping all work as if the column were a
-- second number gutter.
--
-- A diff needs this because its lines are not the file's. Each line has an old number,
-- a new one, or both, and the editor's own line numbers count the diff's lines.
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/gutter-text \
--       cargo run -p bemtvi -- examples/gutter-text/sample.txt
--
-- The sample is one hunk of a unified diff. Each section below says what to type and
-- what you should see.
--
-- 1. Old and new numbers.
--    See: each line of the hunk shows its old number, its new one, or both, left of the
--    text; removed lines show only the old number in red, added lines only the new one
--    in green. The `@@` header line has none, and its cell is blank.
--
-- 2. The column is not part of the line.
--    Type: `:set nowrap`, then `$` on line 6 (the long one).
--    See: the text scrolls sideways and the numbers stay where they are.
--    Type: `0`, then click a character of the text.
--    See: the cursor lands on the character you clicked, not one numbers-width to its
--    right.
--
-- 3. Marks move with their lines.
--    Type: `ggo` then `<Esc>` to open a line under the header.
--    See: every number moves down with its line; the new line has none.
--    Type: `u`. See: they move back.
--
-- 4. A blame column.
--    Type: `:GutterBlame`.
--    See: the numbers give way to a commit hash and an author per line. One author is
--    written in CJK, two cells per character, and the column still lines up: its width
--    is measured in screen cells.
--    Type: `:GutterDiff` to go back to the numbers, `:GutterOff` to clear the column.

--------------------------------------------------------------------------------
-- Colours (a colorscheme would give these; the example defines its own).
--------------------------------------------------------------------------------
btv.hl.define(0, "GutterOld", { fg = "#f38ba8" }) -- an old line number, removed
btv.hl.define(0, "GutterNew", { fg = "#a6e3a1" }) -- a new line number, added
btv.hl.define(0, "GutterRemoved", { bg = "#3b2530" }) -- a removed line's tint
btv.hl.define(0, "GutterAdded", { bg = "#25362b" }) -- an added line's tint
btv.hl.define(0, "GutterHash", { fg = "#f9e2af" }) -- a blame hash

-- One namespace owns the column, so switching between the diff and the blame view
-- is a clear and a repaint.
local ns = btv.ns.create("gutter-text")

-- A number right-aligned in a 4-cell field with a space after it, or a blank field.
local function field(n)
  return n and string.format("%4d ", n) or "     "
end

--- Paint old and new line numbers over the unified diff in the current buffer: the
--- numbering starts at each `@@ -old,n +new,m @@` header and advances per line kind.
local function paint_diff()
  btv.buf.clear_namespace(0, ns, 0, -1)
  local old, new
  for i, line in ipairs(btv.buf.lines(0, 0, -1)) do
    local o, n = line:match("^@@ %-(%d+),?%d* %+(%d+)")
    if o then
      old, new = tonumber(o), tonumber(n)
    elseif old then
      local kind = line:sub(1, 1)
      local chunks, tint
      if kind == "-" then
        chunks, tint = { { field(old), "GutterOld" }, { field(nil) } }, "GutterRemoved"
        old = old + 1
      elseif kind == "+" then
        chunks, tint = { { field(nil) }, { field(new), "GutterNew" } }, "GutterAdded"
        new = new + 1
      else
        chunks = { { field(old) }, { field(new) } }
        old, new = old + 1, new + 1
      end
      btv.buf.set_extmark(0, ns, i - 1, 0, { gutter_text = chunks, line_hl_group = tint })
    end
  end
end

-- A made-up blame, one entry per line of the sample: a short hash and an author.
local BLAME = {
  { "3f9a1c2", "ana" },
  { "3f9a1c2", "ana" },
  { "77be0d4", "李雷" },
  { "77be0d4", "李雷" },
  { "77be0d4", "李雷" },
  { "3f9a1c2", "ana" },
  { "c01dbee", "joão" },
  { "c01dbee", "joão" },
  { "3f9a1c2", "ana" },
}

--- Paint the blame column: a hash in its own colour, then the author.
local function paint_blame()
  btv.buf.clear_namespace(0, ns, 0, -1)
  local n = btv.buf.line_count(0)
  for i, entry in ipairs(BLAME) do
    if i > n then
      break
    end
    local hash, author = entry[1], entry[2]
    btv.buf.set_extmark(0, ns, i - 1, 0, {
      gutter_text = { { hash, "GutterHash" }, { " " .. author .. " " } },
    })
  end
end

vim.o.number = true

btv.command(
  "GutterDiff",
  paint_diff,
  { desc = "old and new line numbers in the gutter text column" }
)
btv.command("GutterBlame", paint_blame, { desc = "a blame column in the gutter text column" })
btv.command("GutterOff", function()
  btv.buf.clear_namespace(0, ns, 0, -1)
end, { desc = "clear the gutter text column" })

-- Paint on startup so the numbers show the moment the file opens.
paint_diff()
