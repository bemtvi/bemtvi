-- ~~~ bemtvi undo steps: typing undoes about a word at a time ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/undo-grouping \
--       cargo run -p bemtvi -- examples/undo-grouping/sample.txt
--
-- Undo steps need no config. vim makes a whole insert one step; bemtvi ends a
-- step at each word, line break, typing/deleting switch, pause or cursor move.
-- The one mapping below shows the other half: a mapping's output is one step.

-- 1. Words. Go to the last line (`G`) and type `ofoo bar baz<Esc>`. Press `u`
--    three times: the line reads `foo bar`, then `foo`, then it is gone (the
--    `o` and the first word are one step). `<C-r>` brings the words back in order.

-- 2. Typing then deleting. Type `oabcd<BS><BS>xy<Esc>`, then `u`: `xy` goes and
--    the line reads `ab`. `u` again restores both deleted letters (`abcd`).

-- 3. A pause. Type `ofoo`, wait two seconds, type `bar<Esc>`, then `u`: only
--    `bar` goes, even though `foobar` is one word.

-- 4. A cursor move. Type `ofoobar`, press `<Left>` three times, type `X<Esc>`.
--    `u` takes the `X` back alone and leaves `foobar`.

-- 5. A mapping is one step. In Insert, `<C-t>` types `TODO: `. Type
--    `o<C-t>check this<Esc>`, then `u` three times: the line reads `TODO: check`,
--    then `TODO: ` (the mapping's text is one step, space and all), then it is gone.
btv.keymap.set("i", "<C-t>", "TODO: ", { desc = "Insert a TODO marker" })

-- 6. The tree still holds every step. After any of the above, `g-` walks back
--    through the states one word at a time and `g+` walks forward, and
--    `:undolist` lists them.
