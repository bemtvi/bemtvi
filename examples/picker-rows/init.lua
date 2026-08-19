-- btv.picker row shape — heads, pinned tags and per-row color, plus the shipped
-- `diagnostics` picker that is built out of exactly those three.
--
--   BEMTVI_CONFIG=examples/picker-rows \
--     cargo run -p bemtvi -- examples/picker-rows/sample.txt
--
-- A picker row is not one long string. A source that knows its row has structure —
-- a location, a classification, a matched fragment — says so, and the widget fits
-- the parts separately instead of cropping one label down to a floating fragment.

vim.g.mapleader = " "

-- 1. The shipped diagnostics picker.
--
--    TYPE  \fd     (the SHIPPED pickers keep bemtvi's default `\` leader: their
--                    maps are registered when the prelude loads, before any config
--                    runs, so a later `mapleader` cannot reach them. The maps this
--                    file sets below are `<leader>…`, i.e. Space.)
--    SEE   every row leads with its severity letter in that severity's color
--          (`E` red, `W` yellow, `H` grey), then `file:line:col`, then the message
--          — `source: text`, folded onto ONE line however many lines the server
--          sent. Errors sort first, then by file and line.
--
--    Then NARROW the window (or open the picker in a small terminal)
--    SEE   the path elides, but the severity letter never does: it is a *pinned
--          tag*, so the elision happens after it.
btv.on("VimEnter", { desc = "picker-rows: seed example diagnostics" }, function()
  -- Stand-in for a language server: client-set diagnostics on this buffer, so the
  -- example needs no LSP installed. A real server's diagnostics render identically.
  -- `btv.diagnostic.set(namespace, bufnr, diagnostics)` — the namespace first.
  local buf = btv.buf.current()
  btv.diagnostic.set(btv.ns.create("example"), buf, {
    {
      lnum = 2,
      col = 0,
      severity = btv.diagnostic.severity.ERROR,
      source = "ty",
      -- A multi-line message: the row folds it, the float still shows it whole.
      message = "expected `String`,\n   found `&str`\n   note: consider `.to_string()`",
    },
    {
      lnum = 4,
      col = 6,
      severity = btv.diagnostic.severity.WARN,
      source = "ty",
      message = "unused variable `total` — prefix it with an underscore",
    },
    {
      lnum = 6,
      col = 2,
      severity = btv.diagnostic.severity.HINT,
      source = "ty",
      message = "this loop could be a `for` comprehension",
    },
  })
end)

-- 2. A source of your own, in the same shape.
--
--    TYPE  <leader>ft
--    SEE   `TODO` rows painted like errors and `NOTE` rows like hints, each with
--          its file:line head aligned down the list. `tag` is what stays when the
--          head has to elide; `hl` is any highlight group, resolved against the
--          live colorscheme (an undefined one just doesn't paint).
--
--    Then TYPE  a few letters
--    SEE   the fuzzy match highlights inside the BODY while the head keeps its
--          color — that separation is why `hl` paints the head and not the row.
--    `hl` names a highlight group, so the colors come from the THEME, never from a
--    hardcoded hex: `btv.hl.palette()` reports the running colorscheme's own hues and
--    `btv.hl.fallback` installs them as defaults that any theme claiming these names
--    overrides. Re-derive on `ColorScheme` and the marks track whatever you load.
local function paint()
  local p = btv.hl.palette()
  btv.hl.fallback("ExampleTodo", { fg = p.red, bold = true })
  btv.hl.fallback("ExampleNote", { fg = p.cyan })
end
paint()
btv.on("ColorScheme", {}, paint)

local MARKS = {
  { tag = "TODO", hl = "ExampleTodo", line = 3, text = "rewrite this in terms of the rope" },
  { tag = "NOTE", hl = "ExampleNote", line = 9, text = "the encoding seam handles this already" },
  { tag = "TODO", hl = "ExampleTodo", line = 14, text = "cover the empty-buffer case" },
}

btv.picker.source({
  name = "todos",
  title = "Marks",
  preview = "location",
  items = function(ctx)
    for _, m in ipairs(MARKS) do
      ctx.push({
        tag = m.tag,
        head = string.format("sample.txt:%d ", m.line),
        text = m.text,
        hl = m.hl,
        path = "sample.txt",
        row = m.line,
      })
    end
  end,
  confirm = function(item, mode, layer)
    btv.picker.edit(item, mode, layer)
  end,
})

btv.keymap.set("n", "<leader>ft", function()
  btv.picker.open("todos")
end, { desc = "picker-rows: the marks picker" })

-- 3. A plain row is unchanged.
--
--    TYPE  <leader>fp
--    SEE   a single-column list — no head, no tag, no color. Every field is
--          optional; declaring none of them is exactly the picker you had before.
btv.picker.source({
  name = "plain",
  title = "Plain rows",
  items = function(ctx)
    for _, s in ipairs({ "alpha", "beta", "gamma" }) do
      ctx.push({ text = s })
    end
  end,
  confirm = function(item)
    btv.notify("picked " .. item.text)
  end,
})

btv.keymap.set("n", "<leader>fp", function()
  btv.picker.open("plain")
end, { desc = "picker-rows: a plain single-column picker" })
