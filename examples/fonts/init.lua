-- ~~~ bemtvi fonts: a fallback list, tried in order for every character ~~~
--
-- Run it in the GUI (from the repo root):
--
--     BEMTVI_CONFIG=examples/fonts \
--       cargo run -p bemtvi-gui -- examples/fonts/sample.txt
--
-- The browser takes the same list. The terminal draws its own fonts and ignores it.
-- The fonts below are common ones; swap in what you have, since a font that isn't
-- installed is reported on the message line (that is note 1 in action).

btv.ui.font({
  { "JetBrains Mono", features = { "+zero" } },
  { "Symbols Nerd Font Mono", scale = 0.9 },
  "Noto Sans CJK JP",
  "Noto Color Emoji",
}, { size = 15 })

-- 1. The list. Each character comes from the first font that has it: line 1 from
--    JetBrains Mono, the icons on line 2 (a folder, a powerline arrow, a branch, Rust,
--    a terminal) from the Nerd symbols font, line 3 from the CJK font, line 4 from the
--    emoji font. A font you don't have is skipped and named on the message line:
--    `guifont: not installed: …`. If it was the first one, the system monospace takes
--    its place.

-- 2. The order is yours, for every script. Run
--    `:lua btv.ui.font({ "Noto Sans CJK JP", "JetBrains Mono" })`: the CJK font has
--    Latin letters too, so it now draws line 1 as well.

-- 3. A font's own weight. `:lua btv.ui.font({ { "JetBrains Mono", weight = "bold" } })`
--    draws all its text bold; bold highlights stay bold either way.

-- 4. A font's own size. The Nerd font above has `scale = 0.9`, a little smaller than
--    the text around it. Try `scale = 0.7` to see the icons shrink.

-- 5. Font features. `+zero` gives JetBrains Mono's slashed zero (line 5). In the
--    browser only the first font's features apply, and the message line says so for
--    any later font's.

-- 6. Typos are loud. `:set guifont=JetBrains\ Mono[wieght=500]` is refused with
--    `E474: Invalid argument: guifont: unknown option wieght on JetBrains Mono`, and
--    the font you had stays.
