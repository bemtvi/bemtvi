-- ~~~ nxvim nx.decor playground: rainbow-delimiters as a viewport provider ~~~
--
-- This is the flagship `nx.decor` example — a whole rainbow-parens plugin in one
-- provider. `nx.decor` is nxvim's answer to neovim's decoration provider: instead
-- of an `on_win`/`on_line` callback the renderer fires per visible row *every
-- frame* (which ADR 0002 rule 4 forbids and the Lua backend can't host), a
-- provider is woken **once per visible-range change** — scroll, resize, edit
-- reflow — *off the frame path*, handed a snapshot of the visible slice, and it
-- **publishes** marks. The marks carry a generation token, so a publish from a
-- viewport you have already scrolled past is dropped, not painted late.
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/rainbow \
--       cargo run -p nxvim -- examples/rainbow/sample.lua
--
-- The brackets colour by nesting depth the instant the file opens; scroll with
-- <C-e>/<C-d> or `G` and the newly-revealed lines colour as they come into view.

--------------------------------------------------------------------------------
-- The six depth colours the marks reference. nx.hl.define is the canonical
-- highlight setter (vim.api.nvim_set_hl is its alias); ns 0 is the global table.
--------------------------------------------------------------------------------
local RAINBOW = { "Rainbow1", "Rainbow2", "Rainbow3", "Rainbow4", "Rainbow5", "Rainbow6" }
local COLORS = { "#f38ba8", "#fab387", "#f9e2af", "#a6e3a1", "#89b4fa", "#cba6f7" }
for i, group in ipairs(RAINBOW) do
  nx.hl.define(0, group, { fg = COLORS[i], bold = true })
end

--------------------------------------------------------------------------------
-- The provider. `on_range(ctx, publish)` runs off the frame, once per visible-
-- range change of a matching window. `ctx` is a snapshot, never live state:
--   { win, buf, top, bot, lines, filetype, gen }   -- top/bot 0-based inclusive
-- It walks only `ctx.lines` (the visible slice — never the whole file), matches
-- the depth of each delimiter, and publishes one `hl`-only mark per bracket. A
-- mark is extmark-shaped: `{ row, col, end_col = col+1, hl = group }` with `row`
-- in absolute (buffer) coordinates.
--------------------------------------------------------------------------------
nx.decor.provider {
  name = "rainbow",
  bufs = { filetype = { "lua", "rust", "json", "javascript", "c" } },
  on_range = function(ctx, publish)
    local marks, depth = {}, 0
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      for col = 1, #line do
        local c = line:sub(col, col)
        if c == "(" or c == "[" or c == "{" then
          marks[#marks + 1] = { row, col - 1, end_col = col, hl = RAINBOW[depth % 6 + 1] }
          depth = depth + 1
        elseif c == ")" or c == "]" or c == "}" then
          depth = math.max(0, depth - 1)
          marks[#marks + 1] = { row, col - 1, end_col = col, hl = RAINBOW[depth % 6 + 1] }
        end
      end
    end
    publish(marks) -- carries ctx.gen; folded into the next frame, or dropped if scrolled past
  end,
}

vim.o.number = true

print("nx.decor rainbow: brackets colour by nesting depth — scroll to see new lines colour in")
