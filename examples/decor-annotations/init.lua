-- ~~~ bemtvi btv.decor playground: the full decoration vocabulary ~~~
--
-- A `btv.decor` provider publishes marks that take the SAME option table
-- `btv.buf.set_extmark` takes — they are validated and lowered by the same code — so a
-- viewport-scoped provider is not limited to highlight spans. This example draws all
-- four payloads at once, off the frame, recomputed only when the viewport moves:
--
--   * a gutter SIGN               (`sign_text` + `sign_hl_group`)
--   * inline VIRTUAL TEXT         (`virt_text`, end-of-line)
--   * a full-width LINE BACKGROUND(`line_hl_group`)
--   * a highlight SPAN            (`hl`, the decor-native shorthand for `hl_group`)
--
-- What `publish` adds over placing the marks yourself is the LIFECYCLE: a publish from
-- a viewport you have already scrolled past is dropped, and each republish replaces the
-- provider's previous marks wholesale — so the provider never has to clear up after
-- itself. That is exactly the shape an inline-blame or per-hunk-sign feature wants.
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/decor-annotations \
--       cargo run -p bemtvi -- examples/decor-annotations/sample.txt
--
-- Type-this / see-that notes are on each section below.

--------------------------------------------------------------------------------
-- 1. The highlight groups the provider paints with.
--
--    TYPE THIS: nothing — see that the sample's `!` / `?` / `>` lines are decorated
--    the moment the file opens, with no keypress at all (the provider is dispatched
--    off the viewport signal, which fires on the initial paint).
--------------------------------------------------------------------------------
btv.hl.define(0, "AnnErrorSign", { fg = "#f38ba8", bold = true })
btv.hl.define(0, "AnnWarnSign", { fg = "#fab387", bold = true })
btv.hl.define(0, "AnnNote", { fg = "#89b4fa", italic = true })
btv.hl.define(0, "AnnErrorLine", { bg = "#3a2431" })
btv.hl.define(0, "AnnMarker", { fg = "#f38ba8", bold = true })

--------------------------------------------------------------------------------
-- 2. The provider. `on_range(ctx, publish)` runs OFF the frame, once per
--    visible-range change, and is handed only the visible slice (`ctx.lines`) — so
--    the cost is bounded by the window height, never by the file size.
--
--    TYPE THIS: <C-e> / <C-d> to scroll, or `G` to jump to the end.
--    SEE THAT:  the newly-revealed lines decorate, and the lines you scrolled away
--               from cost nothing to leave behind.
--------------------------------------------------------------------------------
btv.decor.provider({
  name = "annotations",
  on_range = function(ctx, publish)
    local marks = {}
    for i, line in ipairs(ctx.lines) do
      -- `ctx.lines[i]` is buffer row `ctx.top + i - 1` (0-based rows).
      local row = ctx.top + i - 1

      -- A line opening with `!` is an "error": a sign, a full-width line background,
      -- and an end-of-line note. Three payloads on ONE mark.
      if line:match("^%s*!") then
        marks[#marks + 1] = {
          row,
          0,
          sign_text = "E>",
          sign_hl_group = "AnnErrorSign",
          line_hl_group = "AnnErrorLine",
          virt_text = { { "  ← needs attention", "AnnNote" } },
        }
      -- A line opening with `?` is a "warning": just a gutter sign. A sign-only mark
      -- is legal — before the vocabulary was shared, `publish` rejected any mark that
      -- carried no `hl`, which is why sign-drawing plugins had to route around it.
      elseif line:match("^%s*%?") then
        marks[#marks + 1] = { row, 0, sign_text = "W>", sign_hl_group = "AnnWarnSign" }
      end

      -- Every `>` in the line gets a highlight SPAN, the classic decor payload.
      -- `hl` is the decor-native shorthand for the extmark `hl_group` key.
      local from = 1
      while true do
        local s, e = line:find(">", from, true)
        if not s then
          break
        end
        marks[#marks + 1] = { row, s - 1, end_col = e, hl = "AnnMarker" }
        from = e + 1
      end
    end
    -- One publish per run: it replaces this provider's previous marks wholesale.
    publish(marks)
  end,
})

--------------------------------------------------------------------------------
-- 3. `btv.decor.invalidate()` — re-run the provider when the data it draws FROM
--    changed, rather than waiting for the user to scroll.
--
--    TYPE THIS: :AnnFlip<CR>
--    SEE THAT:  the error tint changes colour immediately, without scrolling. (A
--               provider is otherwise only woken by a viewport change, so a palette
--               swap would keep painting the old colours until you moved.)
--------------------------------------------------------------------------------
local flipped = false
btv.command("AnnFlip", function()
  flipped = not flipped
  btv.hl.define(0, "AnnErrorLine", { bg = flipped and "#243a31" or "#3a2431" })
  btv.decor.invalidate()
end, { desc = "flip the annotation line tint and re-dispatch the provider" })
