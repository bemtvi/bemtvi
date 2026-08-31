--------------------------------------------------------------------------------
-- 'guiglyphoverflow' — when an icon may be drawn bigger than its cell.
--
-- Run (a PIXEL renderer: the GUI, or the web client — the TUI can't, since the
-- terminal draws the glyphs itself):
--   BEMTVI_CONFIG=examples/glyph-overflow \
--     cargo run -p bemtvi-gui -- examples/glyph-overflow/sample.txt
--
-- A Nerd Font icon is designed square — a full em wide — where a coding font's
-- cell is only ~0.6 em. Fitting it to the one cell the editor reserves for it
-- shrinks it to about 60%, which is why icons next to a filename can look
-- undersized. Drawing it at full size instead paints over whatever is beside it.
--
-- The way out is the neighbour: when the next cell is BLANK there is nothing to
-- paint over, so the glyph can have that space. This is wezterm's
-- `allow_square_glyphs_to_overflow_width` — same three modes, same default.
--
-- The column model never changes. The icon still OCCUPIES one cell, so the
-- cursor, selections and every column count are untouched; only the ink grows.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. The default: when-followed-by-space.
--
-- Leave this section as it is and open the sample. Its first block has an icon
-- with a space after it on one line and the same icon jammed against a letter on
-- the next.
--
-- Type-this:  (nothing — just look at sample.txt)
-- See-that:   the icon followed by a space is drawn at full size; the same icon
--             with a letter right after it is shrunk into its own cell so the
--             two glyphs don't collide.
--
-- "Full size" means the size the font drew it, no more: the borrowed cell stops
-- the icon shrinking, it never magnifies it. If you want them smaller still,
-- lower the render ceiling — `bemtvi-gui --emoji-scale 0.9` (a ceiling above 1
-- won't push an overflowing icon past its natural size).
--
-- Explicit, if you like — this is what the empty default resolves to on a client
-- that hasn't been told otherwise on the command line:
--     btv.o.guiglyphoverflow = "when-followed-by-space"   -- "space" for short
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2. Turn it off: every icon shrinks to fit its cell.
--
-- Type-this:  :set guiglyphoverflow=never<CR>
-- See-that:   the spaced icons snap down to the size the crammed ones already
--             render at. Nothing can overhang anything.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. Always: full-size icons, whatever follows.
--
-- Type-this:  :set guiglyphoverflow=always<CR>
-- See-that:   the crammed icons grow to full size too — and overlap the letter
--             beside them. That is the trade the mode names.
--
-- Type-this:  :set guiglyphoverflow=alway<CR>
-- See-that:   E474: Invalid argument — an enumerated option, so a typo is loud
--             rather than leaving you wondering why nothing changed.
--
-- Type-this:  :set guiglyphoverflow?<CR>
-- See-that:   the mode still in effect (the rejected write changed nothing).
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 4. Which glyphs this touches, and which it must not.
--
-- Only glyphs whose ink is roughly SQUARE and wider than one cell — the icons.
-- A powerline separator is tall and narrow, and a box-drawing rule is wide and
-- thin; both are meant to fill exactly their cell and tile seamlessly with their
-- neighbours, so neither is ever grown or shrunk. Two-cell glyphs (CJK, emoji)
-- already have the room their design wants and are left alone as well.
--
-- Type-this:  :set guiglyphoverflow=always<CR>
-- See-that:   the separator run and the box-drawing rule in the sample's last
--             two blocks are pixel-identical in all three modes.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 5. Setting it for good.
--
-- In your own init.lua, pick the mode once:
--     btv.o.guiglyphoverflow = "always"
--
-- Or per-launch, before any config runs (the GUI's startup default, which an
-- init.lua that sets the option then overrides):
--     bemtvi-gui --glyph-overflow always
--     BEMTVI_GUI_GLYPH_OVERFLOW=never bemtvi-gui
--
-- The web client reads the same option, and takes `?glyph-overflow=always` on
-- its URL as the startup default.
--------------------------------------------------------------------------------

-- Left at the default so section 1 is what you see first. Uncomment to start
-- somewhere else:
-- btv.o.guiglyphoverflow = "always"

btv.autocmd.create("UIEnter", {
  once = true,
  callback = function()
    print(
      "guiglyphoverflow="
        .. (
          btv.o.guiglyphoverflow ~= "" and btv.o.guiglyphoverflow
          or "<client default: when-followed-by-space>"
        )
    )
  end,
})
