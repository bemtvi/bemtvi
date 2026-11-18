--------------------------------------------------------------------------------
-- The native GUI's window: where it opens, how large, and whether maximized.
--
-- Run (the native GUI):
--   BEMTVI_CONFIG=examples/gui-window \
--     cargo run -p bemtvi-gui -- examples/gui-window/sample.txt
--
-- Three options, each `remember` by default — the window opens as it was when
-- the GUI last closed:
--   guiposition  remember | centered
--   guisize      remember | {W}%x{H}%   (a share of the screen, e.g. 60%x70%)
--   guimaximize  remember | always | never
--
-- The window opens before a config runs, so the GUI keeps the state it had
-- when it closed (`maximized`, `x`, `y`, `width`, `height` and the modes, in
-- `gui-window` under `stdpath("state")`) and opens by that next time. A config
-- that changes a mode takes effect the moment it runs — the window is centred,
-- resized or maximized there and then — and the window opens right from the
-- next launch on. The first launch opens 960x640, where the platform puts it.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. The defaults: remember all three.
--
-- Type-this:  (nothing — this config leaves them at `remember`) move the window,
--             resize it, then :qa<CR>, and run again.
-- See-that:   the window opens where you left it, as large. Maximize it, :qa<CR>,
--             run again: it opens maximized, and restoring it brings back the
--             size and place it had before.
--------------------------------------------------------------------------------
btv.o.guiposition = "remember"
btv.o.guisize = "remember"
btv.o.guimaximize = "remember"

--------------------------------------------------------------------------------
-- 2. Centred.
--
-- Type-this:  :set guiposition=centered<CR>
-- See-that:   the window moves to the middle of its screen at once, and opens
--             there from now on (with `btv.o.guiposition = "centered"` in a
--             config). On Linux the desktop places windows itself: the message
--             line says so instead.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. A share of the screen.
--
-- Type-this:  :set guisize=60%x70%<CR>
-- See-that:   the window resizes to 60% of the screen's width and 70% of its
--             height at once (unless it's maximized), and opens that large from
--             now on.
--
-- Type-this:  :set guisize?<CR>
-- See-that:   guisize=60%x70%
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 4. Always, or never, maximized.
--
-- Type-this:  :set guimaximize=always<CR>
-- See-that:   the window maximizes at once, and opens maximized from now on —
--             even if you restore it before quitting.
--
-- Type-this:  :set guimaximize=never<CR>
-- See-that:   it restores at once.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 5. A typo fails loud.
--
-- Type-this:  :set guisize=150%x50%<CR>
-- See-that:   E474: Invalid argument: guisize=150%x50% — and the size in effect
--             is unchanged. So does :set guiposition=left<CR>.
--
-- Type-this:  :set guisize&<CR>
-- See-that:   back to `remember` (:set guisize?<CR> says so).
--------------------------------------------------------------------------------
