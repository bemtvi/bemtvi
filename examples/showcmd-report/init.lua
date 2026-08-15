--------------------------------------------------------------------------------
-- `'showcmd'` and `'report'` — the two indicators that tell you what you typed
-- and what the last command did.
--
-- Run:
--   BEMTVI_CONFIG=examples/showcmd-report cargo run -p bemtvi -- examples/showcmd-report/sample.txt
--
-- Both are plain options, on by default; this file only makes them louder so the
-- effect is obvious, and adds one mapping to show that a half-typed *mapping*
-- lands in the corner too.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. `'showcmd'` — the partly-typed command, bottom-right.
--
-- Type-this:  2                (just the digit, then stop)
-- See-that:   `2` in the last line's right corner. Add `d` -> `2d`; add `3` ->
--             `2d3`. `<Esc>` clears it.
--
-- Every stage of the vim grammar shows: an armed register (`"a`), an operator
-- waiting for its motion (`2d`), a key waiting for its argument (`f`, `z`,
-- `<C-w>`). It is on by default — `vim.o.showcmd = false` turns it off.
--------------------------------------------------------------------------------
vim.o.showcmd = true

--------------------------------------------------------------------------------
-- 2. `'showcmd'` in Visual mode — the SIZE of the selection.
--
-- Type-this:  V j j           (linewise, three lines)
-- See-that:   `3` in the corner.
-- Type-this:  <Esc> v l l     (charwise, inside one line)
-- See-that:   `3` again — characters this time. Cross a line boundary and it
--             counts lines instead.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. A half-typed MAPPING shows too.
--
-- The keymap matcher withholds a mapped prefix before it ever reaches the
-- editor, so the corner is where you find out it is waiting for you.
--
-- Type-this:  <Space> f       (and stop)
-- See-that:   `<Space>f` in the corner. Press `s` to complete the mapping.
--------------------------------------------------------------------------------
vim.g.mapleader = " "
btv.keymap.set("n", "<leader>fs", function()
  btv.notify("the mapping fired")
end, { desc = "Say hello" })

--------------------------------------------------------------------------------
-- 4. `'report'` — how many lines the last command changed.
--
-- The default is 2: a command has to change MORE than two lines to say so, which
-- keeps an everyday `dd` / `p` quiet.
--
-- Type-this:  5dd             (delete five lines)
-- See-that:   `5 fewer lines` on the message line.
-- Type-this:  p               (put them back)
-- See-that:   `5 more lines`.
-- Type-this:  6yy
-- See-that:   `6 lines yanked`.  With `"a6yy` it names the register:
--             `6 lines yanked into "a`.
-- Type-this:  5>>
-- See-that:   `5 lines >ed 1 time`.
--------------------------------------------------------------------------------
vim.o.report = 2

--------------------------------------------------------------------------------
-- 5. `report = 0` reports EVERYTHING.
--
-- Uncomment the line below, restart, and a single `dd` says `1 line less` —
-- vim's wording, singular and all. A big `'report'` (99) silences the lot.
--------------------------------------------------------------------------------
-- vim.o.report = 0
