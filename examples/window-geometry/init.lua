-- ~~~ bemtvi unified window-geometry playground ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/window-geometry \
--       cargo run -p bemtvi -- examples/window-geometry/sample.txt
--
-- bemtvi has ONE geometry vocabulary every windowed surface shares — floats,
-- `btv.view`, pickers, and the bottom panel:
--
--   * SIZE     `width` / `height` accept cells (a number) OR a viewport fraction
--              string: "50vw" (50% of the editor width), "30vh", or "50%". A
--              fractional size is resolved against the live editor area EVERY
--              layout, so it reflows when the terminal resizes.
--   * ALIGN    `align` is a 9-grid word — "top-left", "top", "top-right", "left",
--              "center", "right", "bottom-left", "bottom", "bottom-right" — that
--              places the box within the viewport (vs. the low-level NW/NE anchor
--              + row/col offset, still available on floats for nvim_open_win parity).
--   * MARGIN   `margin` insets an aligned box from the edges, so it can sit in a
--              corner WITHOUT kissing the border. A single number is the vertical
--              gap and the horizontal sides get 2x (terminal cells are ~2x taller
--              than wide, so the gap looks even). For full control pass
--              `{ vertical, horizontal }`, `{ top, right, bottom, left }`, or
--              `{ top = , right = , bottom = , left = }` (taken literally).
--
-- TRY IT (leader = space):
--   <leader>gf   float, top-right, 2-cell margin, fractional size
--   <leader>gc   float, centered, fractional size
--   <leader>gp   picker, bottom-left, 3-cell margin
--   <leader>gn   bottom panel, 30vh tall, 1-cell edge gap
--   q / <Esc>    dismiss a grabbing float

vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- The float views share a filetype; a FileType autocmd installs the dismiss keys
-- (the ordinary buffer-local-keymap mechanism, the same way the panel wires `q`).
-- Only one demo float is up at a time, so a single module-level handle suffices.
local active

btv.autocmd.create("FileType", {
  pattern = "btvgeom",
  callback = function(args)
    for _, key in ipairs({ "q", "<Esc>" }) do
      btv.keymap.set("n", key, function()
        if active then
          active:unmount()
          active = nil
        end
      end, { buffer = args.buf })
    end
  end,
})

--------------------------------------------------------------------------------
-- 1) Floats placed by `align` + `margin`, sized with viewport fractions. `btv.view`
--    is the read-only content surface; `:mount{ float = … }` takes the full
--    geometry. A grabbing float (the default) locks focus until dismissed.
local function corner_float()
  active = btv.view.create({ filetype = "btvgeom" })
  active:set_lines({
    "  top-right float   ",
    "",
    "  width  = 40vw      ",
    "  height = 40vh      ",
    "  align  = top-right ",
    "  margin = 2         ",
    "",
    "  q / <Esc> to close ",
  })
  active:mount({
    float = {
      width = "40vw",
      height = "40vh",
      align = "top-right",
      margin = 2,
      border = "rounded",
      title = "geometry",
    },
  })
end

local function centered_float()
  active = btv.view.create({ filetype = "btvgeom" })
  active:set_lines({
    "  centered float            ",
    "",
    "  60vw x 50vh, align center  ",
    "",
    "  q / <Esc> to close         ",
  })
  active:mount({
    float = { width = "60vw", height = "50vh", align = "center", border = "double" },
  })
end

--------------------------------------------------------------------------------
-- 2) A picker placed in a corner with a margin (it used to be centered-only).
btv.picker.source({
  name = "colours",
  items = function(ctx)
    for _, c in ipairs({ "crimson", "cerulean", "chartreuse", "amber", "indigo", "teal" }) do
      ctx.push({ text = c })
    end
  end,
  confirm = function(item)
    btv.notify("picked " .. item.text, 2)
  end,
})

local function corner_picker()
  btv.picker.open("colours", {
    width = "40vw",
    height = "40vh",
    align = "bottom-left",
    margin = 3,
  })
end

--------------------------------------------------------------------------------
-- 3) The bottom panel: a fractional height + a gap from the screen edges. The
--    panel stays bottom-anchored; `margin` lifts it off the left/right/bottom.
local function frac_panel()
  btv.panel.open({
    lines = {
      "scripted panel - height = 30vh, margin = 1",
      "",
      "the panel keeps its share of the screen on resize,",
      "and the margin leaves a one-cell gap from the edges.",
      "",
      "press q or <Esc> to dismiss.",
    },
    height = "30vh",
    margin = 1,
  })
end

--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>gf", corner_float, { desc = "geometry: top-right float" })
btv.keymap.set("n", "<leader>gc", centered_float, { desc = "geometry: centered float" })
btv.keymap.set("n", "<leader>gp", corner_picker, { desc = "geometry: bottom-left picker" })
btv.keymap.set("n", "<leader>gn", frac_panel, { desc = "geometry: 30vh bottom panel" })
