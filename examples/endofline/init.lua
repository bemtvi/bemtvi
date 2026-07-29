-- ~~~ nxvim trailing newlines: 'endofline' / 'fixendofline' ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/endofline \
--       cargo run -p nxvim -- examples/endofline/sample.txt
--
-- `sample.txt` is deliberately stored WITHOUT a trailing newline.
--
-- Internally nxvim's text rope always ends in a `\n` — that phantom newline is
-- what vim's line model calls the implicit terminator after the last line, and
-- every byte offset, mark and tree-sitter point in the editor relies on it. So
-- the rope alone cannot say whether the file on disk really ended with one. Two
-- buffer-local options carry that fact:
--
--   * endofline    (eol)    — whether this buffer's document ends with a line
--                             break. Set from the bytes when the file is read;
--                             you rarely set it by hand.
--   * fixendofline (fixeol) — whether a write SUPPLIES a missing terminator.
--                             On by default, exactly as in vim, so files gain a
--                             trailing newline on save unless you opt out.
--
-- Under the defaults nothing changes: saving `a\nb` writes `a\nb\n`. Turn
-- `fixendofline` off and the file round-trips byte for byte instead.

-- 1. OPT OUT GLOBALLY. These are buffer-local options, so `vim.o` routes the
--    write to the buffer that is current as the config runs. Pair it with an
--    autocmd so every buffer opened later gets the same treatment.
vim.o.fixendofline = false

vim.api.nvim_create_autocmd("BufReadPost", {
  pattern = "*",
  callback = function(args)
    vim.bo[args.buf].fixendofline = false
  end,
})

-- 2. A CUE ON THE STATUS LINE. The built-in status line already appends
--    `[noeol]` next to the encoding for a buffer holding an unterminated FILE —
--    the only visible sign that a save might change the file's last byte.
--    Nothing to configure; it is shown for `sample.txt` out of the box.
--
--    "File" is the operative word: an empty document (a brand-new file, the
--    `[No Name]` buffer you start with) has no final newline either, and neither
--    does a scratch surface like `:messages`, but none of them is a file missing
--    a terminator — so they are not marked.
--
--    Rolling your own? `&endofline` and `&buftype` both resolve in a `%{}`
--    expression, which is exactly how the built-in narrows it:
-- vim.o.statusline =
--   '%f%m%={&endofline || &buftype != "" ? "" : "[noeol]"}  %l,%c '
--
--    `&fixendofline` resolves too, if you want to distinguish a terminator that
--    is about to be SUPPLIED from one that will be preserved:
-- vim.o.statusline =
--   '%f%m%={&endofline ? "" : (&fixeol ? "[+eol]" : "[noeol]")}  %l,%c '

-- 3. THE OTHER DIRECTION. To normalize a tree of files the opposite way — always
--    terminate, never ask — leave `fixendofline` at its default (drop section 1)
--    and the write does it for you.

--------------------------------------------------------------------------------
-- Try it (in `sample.txt`):
--
-- 1. READ DETECTION — the flag came off the bytes, nothing guessed it:
--      TYPE:  :set endofline?        -> echoes "noendofline"
--      TYPE:  :set fixendofline?     -> echoes "nofixendofline"  (section 1)
--      SEE:   the status line's right side reads "utf-8[noeol]"
--
-- 2. BYTE-FOR-BYTE ROUND TRIP — save an untouched buffer and the file is
--    unchanged, trailing byte included:
--      TYPE:  :w<CR>
--      THEN, in a shell:   tail -c 20 examples/endofline/sample.txt | xxd
--      SEE:   the last byte is `2e` (a `.`), NOT `0a` — no newline was added
--
-- 3. THE DEFAULT, FOR CONTRAST — turn the fixer back on and save again:
--      TYPE:  :set fixeol<CR>:w<CR>
--      SEE:   `:set endofline?` now echoes "endofline" — the write supplied the
--             terminator, so the flag reports what is actually on disk, and the
--             `[noeol]` marker is gone from the status line
--      (Undo the experiment with `:set noeol nofixeol<CR>:w<CR>`.)
--
-- 4. ONLY THE LAST LINE IS AFFECTED. `endofline` describes the document's END,
--    not any one line — append a line and the new last line is the bare one:
--      TYPE:  :set nofixeol<CR>Gonew last line<Esc>:w<CR>
--      SEE:   every earlier line is still terminated; only "new last line" is not
--
-- 5. AN EMPTY FILE STAYS EMPTY. A 0-byte file has no final newline either, so
--    nxvim writes it back at 0 bytes rather than growing it to one:
--      TYPE:  :e /tmp/nxvim-empty-demo<CR>:w<CR>:!wc -c /tmp/nxvim-empty-demo<CR>
--      SEE:   0 bytes
--
-- 6. LANGUAGE SERVERS SEE THE DOCUMENT, NOT THE ROPE. With an LSP attached, the
--    text nxvim sends is the document — no phantom newline — so a formatter that
--    adds or removes a trailing newline actually changes `:set endofline?`.
--------------------------------------------------------------------------------
