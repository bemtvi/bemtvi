-- ~~~ bemtvi keyboard macros: record with <F2>, replay with <F3> ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/macros \
--       cargo run -p bemtvi -- examples/macros/sample.txt
--
-- Macros are built in — this config sets nothing you need for them to work. It
-- shows the three things worth customizing, and the try-this list at the bottom
-- is the actual point.
--
--     <F2>{reg}   start recording into register {reg}   (uppercase appends)
--     <F2>        stop recording
--     <F3>{reg}   play it back      ({count}<F3>{reg} repeats, <F3><F3> = last)
--     <F3>:       re-run the last ex command

-- 1. The vim spelling, if your fingers insist. bemtvi keeps `q` and `@` free (a
--    view/dock pane binds `q` to close, and vim's `q` shadows a good key for
--    everyone who never records). Mapping them back works because a recording
--    captures what you TYPED — the mapping's left-hand side — and replays it
--    through the keymap engine, so the map fires again.
--
--    Uncomment to opt in:
-- btv.keymap.set("n", "q", "<F2>", { desc = "Record macro (vim's q)" })
-- btv.keymap.set("n", "@", "<F3>", { desc = "Play macro (vim's @)" })

-- 2. Show the recording in the statusline. `macro` is a built-in segment; it
--    renders `recording @a` while a recording is open and nothing otherwise.
--    (The message line says the same thing without any config at all.)
btv.statusline.setup({
  left = { "mode", "macro", "filename", "modified" },
  right = { "diagnostics", "filetype", "location" },
})

-- 3. A macro is an ordinary register holding readable key notation, so it can be
--    written by hand — no recording session needed. This one wraps the word under
--    the cursor in `**bold**` markers and moves to the next word.
btv.reg.set("b", "yiwciw**<C-r>0**<Esc>w")

-- …and a keymap can play a register directly. (The leader defaults to `\` as in
-- vim; this example uses Space, so the mapping below is `<Space>b`.)
vim.g.mapleader = " "
btv.keymap.set("n", "<leader>b", function()
  btv.macro.play("b")
end, { desc = "Bold the word under the cursor (macro register b)" })

-- 4. `btv.macro.executing()` is the cheap way for a plugin to skip work no human
--    is watching. This one just proves the hook fires.
btv.on("CursorMoved", function()
  if btv.macro.executing() then
    btv.g.macro_moves = (btv.g.macro_moves or 0) + 1
  end
end)

--------------------------------------------------------------------------------
-- Try it (in sample.txt):
--
-- 1. Record and replay a line edit:
--      gg                 -> first line
--      <F2>a              -> the message line reads `recording @a`
--      I- <Esc>j          -> prefix this line with "- ", go down
--      <F2>               -> stop; the announcement clears
--      <F3>a              -> the next line gets the same treatment
--      99<F3>a            -> …and all the rest. It STOPS at the last line:
--                            `j` fails there, and a failure ends the run.
--
-- 2. Look at what you recorded — it is just text in register `a`:
--      :registers a       -> a  I-<Space><Esc>j
--      "ap                -> pastes those keystrokes into the buffer (then `u`)
--
-- 3. The hand-written one from section 3, over the "TODO" paragraph:
--      /TODO<CR>          -> jump to it
--      <leader>b          -> **TODO**, cursor on the next word
--      <F3>b              -> same thing, one word at a time
--      3<F3>b             -> three more words
--
-- 4. Append to a recording (uppercase register):
--      <F2>c  x  <F2>     -> register c deletes one character
--      <F2>C  x  <F2>     -> APPEND: register c now deletes two
--      <F3>c              -> two characters go
--
-- 5. Repeat the last ex command with <F3>: —
--      :s/o/0/<CR>        -> substitute on this line
--      j<F3>:             -> same substitution on the next line
--      j<F3>:             -> and the next
--
-- 6. A macro can call a macro:
--      <F2>d  <F3>b j0  <F2>   -> `d` runs `b`, then moves down to column 0
--      5<F3>d                  -> bolds the first word of five lines
