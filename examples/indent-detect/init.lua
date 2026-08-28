-- ~~~ bemtvi indent detection: 'indentdetect' ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/indent-detect \
--       cargo run -p bemtvi -- examples/indent-detect/sample.txt
--
-- `'indentdetect'` is ON by default — this config exists to make what it does
-- visible, not to switch it on. Every time bemtvi reads a file it looks at the
-- file's own leading whitespace and sets that buffer's `'expandtab'` and
-- `'shiftwidth'` to match, so you keep indenting a file the way it is already
-- indented instead of the way your config would have. (This is what the vim-sleuth
-- plugin does for vim; in bemtvi it is built in, and there is nothing to install.)
--
-- The precedence is: the FILE beats your config, and anything you set AFTERWARDS
-- beats the file. So the config below is deliberately "wrong" for every sample
-- file — 8-wide tabs — and you will watch each file overrule it.

-- 1. A config that every sample file disagrees with, so the detection is visible.
--    Without `'indentdetect'` every buffer below would indent with one 8-wide tab.
vim.o.expandtab = false
vim.o.tabstop = 8
vim.o.shiftwidth = 8

-- 2. Show the detected style in the statusline, so you can read the verdict off
--    the screen instead of typing `:set et?` every time. It is recomputed when a
--    buffer is entered or read (there is no `OptionSet` event) — so after you set
--    the options by hand in step 6, ask `:set expandtab? shiftwidth?` instead.
--
--    TYPE:  (nothing — look at the bottom right of any window)
--    SEE:   `spaces:2`, `spaces:4` or `tabs` for the focused buffer.
btv.statusline.segment({
  name = "indent",
  events = { "BufEnter", "BufReadPost" },
  render = function()
    local text = btv.bo.expandtab and ("spaces:" .. tostring(btv.bo.shiftwidth)) or "tabs"
    return { { text = text } }
  end,
})

btv.statusline.setup({
  left = { "mode", "filename", "modified" },
  right = { "indent", "filetype", "location" },
})

-- 3. THE SAMPLE FILE — 2-space indented.
--
--    TYPE:  :set expandtab? shiftwidth?
--    SEE:   `expandtab` and `shiftwidth=2` — not the `noexpandtab` /
--           `shiftwidth=8` set above. The file won.
--
--    TYPE:  gg>>
--    SEE:   the first line moves right by exactly two SPACES.

-- 4. A TAB-INDENTED FILE, opened in the same session.
--
--    TYPE:  :e examples/indent-detect/tabbed.txt
--    SEE:   the statusline flips to `tabs`.
--
--    TYPE:  gg>>
--    SEE:   the line moves right by one real TAB.
--
--    TYPE:  :b#
--    SEE:   back on sample.txt, the statusline says `spaces:2` again — the
--           verdict is per buffer, not a global mode.

-- 5. A 4-SPACE FILE, to prove the WIDTH is read too, not just tabs-vs-spaces.
--
--    TYPE:  :e examples/indent-detect/four-space.txt
--    SEE:   `spaces:4`.
--
--    TYPE:  ggo  then press <Tab>
--    SEE:   four spaces, because `'softtabstop'` follows the detected
--           `'shiftwidth'`.

-- 6. YOUR SETTING STILL WINS — as long as it comes after the read.
--
--    TYPE:  :setlocal noexpandtab shiftwidth=8
--    SEE:   `>>` now inserts a tab (`:set expandtab? shiftwidth?` confirms it).
--           Detection runs when
--           the file is READ, so anything you (or an autocmd, or an
--           `.editorconfig` plugin) set afterwards is the last word.
--
--    TYPE:  :e!
--    SEE:   re-reading the file runs the detection again — back to `spaces:4`.

-- 7. TURNING IT OFF.
--
--    TYPE:  :set noindentdetect | e examples/indent-detect/tabbed.txt
--    SEE:   the statusline stays on whatever the config said (`tabs`, 8 wide,
--           from step 1) instead of being read off the file. `:set indentdetect`
--           turns it back on for the next read.
