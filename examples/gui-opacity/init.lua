--------------------------------------------------------------------------------
-- 'guiopacity' — let the window's backdrop show through the background.
--
-- Run (the native GUI on Windows 11 — the window's backdrop is its Mica):
--   BEMTVI_CONFIG=examples/gui-opacity \
--     cargo run -p bemtvi-gui -- examples/gui-opacity/sample.txt
--
-- The option is how opaque, in percent, the GUI paints the colorscheme's
-- `Normal` background. Below 100 the rest is Mica: Windows' material, tinted by
-- your wallpaper, the same one behind the title bar.
--
-- Only the `Normal` background thins. Everything that has a background of its
-- own — a selection, the status line, a search match, the cursor line, a float,
-- a dock with its own `Normal` — has its own knob, `'guihlopacity'` (section 4),
-- and the text always stays solid.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. Set in a config: mostly solid, with a hint of the wallpaper.
--
-- Type-this:  (nothing — this config sets it below)
-- See-that:   the editor area picks up the title bar's tint; drag the window
--             over something colourful and the colour moves with it.
--------------------------------------------------------------------------------
btv.o.guiopacity = 85

-- The cursor line, so there's a secondary background to look at from the start.
btv.o.cursorline = true

--------------------------------------------------------------------------------
-- 2. All the way: pure Mica behind the text.
--
-- Type-this:  :set guiopacity=0<CR>
-- See-that:   the editor area is the same material as the title bar, edge to
--             edge.
--
-- Type-this:  Vj
-- See-that:   the selection stays solid — only `Normal` is see-through. So do
--             the cursor line and the status line.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. Back to solid.
--
-- Type-this:  :set guiopacity=100<CR>
-- See-that:   the colorscheme's background, exactly as without the option (100
--             is the default).
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 4. The other backgrounds: 'guihlopacity'.
--
-- Every background that isn't `Normal` — selections, search matches, the cursor
-- line, line tints, the status and tab lines, a dock's own `Normal`, floats and
-- popups — takes this second percentage. Cursors and underlines are ink, not
-- backgrounds, and stay solid.
--
-- Type-this:  :set guiopacity=0 guihlopacity=50<CR>
-- See-that:   the cursor line and the status line are half-tinted bands over
--             the Mica.
--
-- Type-this:  <Space>f
-- See-that:   a float opens under the cursor, over the text, and NONE of that text
--             shows through: a float clears what's beneath it, so behind its own
--             lines there's only Mica and its half-strength background.
--
-- Type-this:  :set guihlopacity=0<CR>  then  <Space>f  again
-- See-that:   the float's text sits on pure Mica, still with nothing of the
--             buffer behind it (moving the cursor closes the float).
--------------------------------------------------------------------------------
btv.keymap.set("n", "<Space>f", function()
  btv.ui.float({
    "A float clears the editor under it:",
    "behind these lines there's only the",
    "window's Mica and the float's own",
    "background, at 'guihlopacity'.",
  }, { title = " guihlopacity " })
end, { desc = "Open a float over the text" })

--------------------------------------------------------------------------------
-- 5. A percentage, checked.
--
-- Type-this:  :set guiopacity=150<CR>
-- See-that:   E474: Invalid argument — not quietly clamped to 100.
--
-- Type-this:  :set guiopacity?<CR>
-- See-that:   the value still in effect (the rejected write changed nothing).
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 6. Where it can't.
--
-- Showing the backdrop takes a Direct3D 12 swapchain the compositor reads alpha
-- from, which the GUI only has on Windows so far. Anywhere else — or on Windows
-- with `WGPU_BACKEND=vulkan` — anything below 100 (in either option) is an error
-- in the message line and the backgrounds stay solid; it never looks set while
-- doing nothing:
--     WGPU_BACKEND=vulkan BEMTVI_CONFIG=examples/gui-opacity \
--       cargo run -p bemtvi-gui -- examples/gui-opacity/sample.txt
--
-- The terminal UI and the web client have no backdrop behind their cells and
-- ignore both options.
--------------------------------------------------------------------------------
