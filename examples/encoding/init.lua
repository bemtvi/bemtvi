-- ~~~ nxvim file encodings: read latin1 / utf-16 / invalid-UTF-8, round-trip on save ~~~
--
-- nxvim's internal text model is ALWAYS UTF-8 (the rope). `'fileencoding'` names
-- the charset of the bytes *on disk*; the read/write seam transcodes between the
-- two. Try it against the two sample files (from the repo root):
--
--     NXVIM_CONFIG=examples/encoding \
--       cargo run -p nxvim -- examples/encoding/latin1.txt
--
--     NXVIM_CONFIG=examples/encoding \
--       cargo run -p nxvim -- examples/encoding/invalid-utf8.txt
--
-- What to look for:
--
--   * `latin1.txt` is real ISO-8859-1/windows-1252 text. The single byte 0xe9
--     shows as `é`, and `:set fenc?` reports `fileencoding=latin1`. Save with `:w`
--     and the file stays latin1 — `é` is written back as the one byte 0xe9, NOT as
--     the two-byte utf-8 `é`. (Compare bytes with `xxd` before/after.)
--
--   * `invalid-utf8.txt` has a few bytes that aren't valid UTF-8. It used to refuse
--     to open; now it falls through `'fileencodings'` to the latin1 fallback, opens,
--     and `:w` reproduces the original bytes EXACTLY (windows-1252 is a total,
--     reversible single-byte codec — no lossy decode corrupts the file).
--
-- The detection order is `'fileencodings'` (a comma list), tried left to right:
--
--     vim.o.fileencodings = "ucs-bom,utf-8,latin1"   -- the DEFAULT
--       --  ucs-bom : sniff a leading BOM (utf-8 / utf-16le / utf-16be) first
--       --  utf-8   : strict UTF-8 (skipped on the first invalid byte)
--       --  latin1  : the always-succeeds fallback (windows-1252)
--
-- Per-buffer overrides (all equivalent ways to set the on-disk encoding):
--
--     vim.bo.fileencoding = "utf-8"   -- convert THIS buffer to utf-8 on next :w
--     :set fileencoding=utf-8         -- same, ex-command form (abbrev: fenc)
--     :set bomb                       -- (re-)emit a byte-order mark on write
--
-- Converting on save: open `latin1.txt`, then `:set fenc=utf-8` and `:w` — the file
-- is rewritten as UTF-8 (each accented byte becomes its multi-byte utf-8 form).
--
-- Fail-loud on save: nxvim never silently corrupts a file. Writing a character the
-- target encoding can't represent (e.g. a CJK char like 中 into a latin1 buffer)
-- aborts the write with `E513` and leaves the file untouched — rather than emitting
-- the HTML numeric character references encoding libraries fall back to.

-- Nothing to configure here — the defaults already do the right thing. This block
-- just makes the active detection list visible at startup.
vim.api.nvim_create_autocmd("BufReadPost", {
  callback = function(args)
    local enc = vim.bo[args.buf].fileencoding
    local bomb = vim.bo[args.buf].bomb and " (+BOM)" or ""
    vim.notify(("nxvim: opened with fileencoding=%s%s"):format(enc, bomb))
  end,
})
