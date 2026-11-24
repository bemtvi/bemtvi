-- ~~~ bemtvi btv.decor playground: a provider that asks the parse tree ~~~
--
-- `query = "..."` on `btv.decor.provider` runs a treesitter query over the visible
-- rows before each `on_range`, and hands the answer over as `ctx.query`. The provider
-- learns where the tables and code fences are from the grammar, instead of guessing
-- from the text.
--
-- Needs the markdown grammar. Check with `:TSInstallInfo`; get it with
-- `:TSInstall markdown`.
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/decor-query \
--       cargo run -p bemtvi -- examples/decor-query/sample.md
--
-- Type-this / see-that notes are on each section below.

--------------------------------------------------------------------------------
-- 1. The colours the provider paints with.
--
--    TYPE THIS: nothing. SEE THAT: every row of both tables carries a `T` in the
--    gutter, and every row of the code fence a `C`, the moment the file opens. The
--    paragraph line with a stray `|` has no sign: the grammar says it isn't a table.
--    Neither is the `| not | a table |` inside the fence.
--------------------------------------------------------------------------------
btv.hl.define(0, "QueryTableSign", { fg = "#89b4fa", bold = true })
btv.hl.define(0, "QueryFenceSign", { fg = "#a6e3a1", bold = true })

--------------------------------------------------------------------------------
-- 2. The provider. `ctx.query.matches` lists each match with its captured nodes,
--    each `{ row, col, end_row, end_col, kind = "..." }`. A node that crosses the
--    top or bottom of the window comes back whole, so a table cut by the screen edge
--    still reports its real first and last rows.
--
--    TYPE THIS: `5Gzt` (scroll so the first table's header is the top row), then
--               `<C-e>` once to push the header off screen.
--    SEE THAT:  the rows still on screen keep their `T`. The provider only marks
--               visible rows, but it knows where the table really starts.
--------------------------------------------------------------------------------
local SIGNS = {
  table = { "T", "QueryTableSign" },
  fence = { "C", "QueryFenceSign" },
}

btv.decor.provider({
  name = "decor-query",
  bufs = { filetype = { "markdown" } },
  query = [[
    (pipe_table) @table
    (fenced_code_block) @fence
  ]],
  on_range = function(ctx, publish)
    local q = ctx.query
    if q.pending then
      return -- the grammar is loading or the parse is unfinished: keep what's drawn
    end
    if not q.matches then
      publish({}) -- no grammar, or treesitter is off here: nothing to mark
      return
    end
    local marks = {}
    for _, m in ipairs(q.matches) do
      for name, nodes in pairs(m.captures) do
        local sign = SIGNS[name]
        for _, node in ipairs(nodes) do
          local first, last = node[1], node[3] - 1
          -- A node ending at column 0 stops before that row.
          if node[4] > 0 then
            last = node[3]
          end
          for row = math.max(first, ctx.top), math.min(last, ctx.bot) do
            marks[#marks + 1] = { row, 0, sign_text = sign[1], sign_hl_group = sign[2] }
          end
        end
      end
    end
    publish(marks)
  end,
})

--------------------------------------------------------------------------------
-- 3. Edits are re-queried.
--
--    TYPE THIS: `/billing<CR>dd`, then `u`.
--    SEE THAT:  the first table loses a `T` row with the line and gets it back on
--               undo. Type `o| x | y |<Esc>` on the table's last row and the new row
--               is marked too: it is part of the table now.
--
-- 4. Treesitter off means no answer.
--
--    TYPE THIS: `:lua btv.bo.ts_highlight = false`
--    SEE THAT:  every sign disappears: `ctx.query.error` now says the buffer has no
--               treesitter language, and the provider publishes nothing.
--               `:lua btv.bo.ts_highlight = true` brings them back.
--------------------------------------------------------------------------------
