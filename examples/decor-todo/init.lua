-- ~~~ bemtvi btv.decor playground: TODO-keyword highlighting, debounced ~~~
--
-- A second `btv.decor` example (the flagship is `examples/rainbow/`). It shows two
-- Phase-4 conveniences:
--
--   * `debounce = <ms>` — coalesce a fast continuous scroll into ONE provider run.
--     A held <C-e>/<C-d> fires `on_range` once the viewport stops moving for `ms`,
--     not on every intermediate row. The per-window coalescing in core already
--     collapses changes between two drains; `debounce` adds the trailing delay
--     across a continuous gesture. Omit it (as rainbow does) for instant feedback.
--
--   * scoping with `bufs` — this provider runs in any buffer (no `bufs`); scope it
--     with `bufs.filetype = { "lua", … }` for language-specific runs, or
--     `bufs.buf = <id>` to opt a single buffer in (per-buffer opt-in).
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/decor-todo \
--       cargo run -p bemtvi -- examples/decor-todo/sample.lua
--
-- The keywords colour the instant the file opens; scroll and the newly-revealed
-- lines colour once the scroll settles (the debounce).

--------------------------------------------------------------------------------
-- The keyword → highlight-group map. btv.hl.define is the canonical highlight
-- setter (vim.api.nvim_set_hl is its alias); ns 0 is the global table.
--------------------------------------------------------------------------------
local KEYWORDS = {
  TODO = "TodoKeyword",
  FIXME = "FixmeKeyword",
  HACK = "HackKeyword",
  XXX = "HackKeyword",
  NOTE = "NoteKeyword",
}
btv.hl.define(0, "TodoKeyword", { fg = "#89b4fa", bold = true })
btv.hl.define(0, "FixmeKeyword", { fg = "#f38ba8", bold = true })
btv.hl.define(0, "HackKeyword", { fg = "#fab387", bold = true })
btv.hl.define(0, "NoteKeyword", { fg = "#a6e3a1", bold = true })

--------------------------------------------------------------------------------
-- The provider. `on_range(ctx, publish)` runs off the frame, once per visible-
-- range change of a matching window — here, after a 60ms quiet period so a fast
-- scroll re-runs it only once. `ctx` is a snapshot, never live state:
--   { win, buf, top, bot, lines, filetype, gen }   -- top/bot 0-based inclusive
-- It walks only `ctx.lines` (the visible slice), finds each keyword, and publishes
-- one `hl`-only mark per occurrence (extmark-shaped, absolute buffer coordinates).
--------------------------------------------------------------------------------
btv.decor.provider({
  name = "todo-keywords",
  debounce = 60,
  on_range = function(ctx, publish)
    local marks = {}
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      for word, group in pairs(KEYWORDS) do
        -- Find every occurrence of the bare keyword on the line (plain find).
        local from = 1
        while true do
          local s, e = line:find(word, from, true)
          if not s then
            break
          end
          -- 1-based Lua string offsets → 0-based extmark columns; end is exclusive.
          marks[#marks + 1] = { row, s - 1, end_col = e, hl = group }
          from = e + 1
        end
      end
    end
    publish(marks) -- carries ctx.gen; folded into the next frame, or dropped if scrolled past
  end,
})

vim.o.number = true

print("btv.decor todo-keywords: TODO/FIXME/HACK/XXX/NOTE colour by kind (debounced on scroll)")
