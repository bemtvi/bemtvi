-- ~~~ bemtvi smooth scrolling: the viewport slides, it doesn't teleport ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/smooth-scroll \
--       cargo run -p bemtvi -- examples/smooth-scroll/sample.txt
--
-- The scroll commands (<C-d> / <C-u> half-page, <C-f> / <C-b> full-page, the
-- mouse wheel, and off-screen jumps like `G`) ANIMATE: the editor applies the
-- scroll instantly but hands the client a self-contained `scroll` descriptor, and
-- the client slides the viewport to the destination over a short duration
-- (neoscroll.nvim's feel, built in). Two global options tune it — both are real
-- bemtvi options, settable from `:set`, `vim.o`, or `btv.o`:
--
--   scrollanim          (boolean, default on) — animate, or snap. `noscrollanim`
--                       turns the slide off entirely; the viewport jumps as
--                       before. (`:set noscrollanim` / `:set scrollanim`)
--   scrollanimduration  (number, ms, default 160) — the LONGEST a slide may last.
--                       The per-scroll duration scales with the travel distance
--                       (8ms/line) and is clamped to this ceiling; a smaller value
--                       is snappier. `0` disables animation, like `noscrollanim`.
--                       (`:set scrollanimduration=…` — abbreviation `scad`)
--
-- TRY IT interactively (the sample is 120 lines, well over one screen):
--   <C-d> / <C-u>            half-page slide down / up
--   <C-f> / <C-b>            full-page slide down / up — the longest slide
--   G then gg                jump to the bottom, then the top — both animate a
--                            bounded slide (capped at ~2 screens of travel)
--   :set scrollanimduration=400   slow it right down, then scroll again
--   :set scrollanimduration=60    make it snappy
--   :set noscrollanim             turn it off — scrolls teleport now
--   :set scrollanim               turn it back on
--   :set scrollanim? scrollanimduration?   query the values (echoed to :messages)
--   :ScrollReport            re-run those queries from Lua

-- These are GLOBAL options. Set them through any of the three equivalent routes;
-- here we use `vim.o`, which reaches the same core state `:set` writes. Start with
-- a slightly longer-than-default slide so the animation is easy to see.
vim.o.scrollanimduration = 220

--------------------------------------------------------------------------------
-- :ScrollReport — echo the current smooth-scroll options. Reads them back through
-- the Lua bridge (`vim.o`), proving the value the core holds round-trips to Lua.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("ScrollReport", function()
  vim.notify(
    string.format(
      "scrollanim=%s  scrollanimduration=%d",
      tostring(vim.o.scrollanim),
      vim.o.scrollanimduration
    )
  )
end, {})

vim.notify("smooth scroll demo: press <C-d> / <C-f> and watch the viewport slide")
