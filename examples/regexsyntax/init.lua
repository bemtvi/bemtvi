-- ~~~ bemtvi 'regexsyntax': pick the regex dialect for `/` search and `:substitute` ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/regexsyntax \
--       cargo run -p bemtvi -- examples/regexsyntax/sample.txt
--
-- bemtvi's `/` search and `:s` substitute speak ONE of two regex dialects, chosen
-- by the `regexsyntax` option:
--
--   * "pcre" (the DEFAULT) — canonical / perl-compatible regex (the Rust `regex`
--     crate). Bare `+ ? ( ) | { }` are operators, `\` escapes them to literals,
--     and a replacement uses `$1` / `${name}` capture refs. No back-references or
--     look-around.
--
--   * "vim" — the real vim "magic" dialect, matched by bemtvi's embedded copy of
--     vim's own regexp engine. `\(\)` groups, `\1` / `&` back-refs, `\<`/`\>` word
--     boundaries, the non-greedy `\{-}` family, `\zs`/`\ze`, look-around (`\@=`),
--     and the `\u \U \l \L \e \E` replacement case modifiers — exactly as in vim.
--
-- It is a GLOBAL-LOCAL option: a global default plus an optional per-buffer
-- override, so one buffer can use vim's dialect while the rest stay on pcre.
--     vim.o.regexsyntax  = "vim"     -- the GLOBAL default (every buffer that
--                                       hasn't pinned its own dialect)
--     vim.bo.regexsyntax = "vim"     -- override THIS buffer only
--     :set regexsyntax=vim           -- override the current buffer (like :set ts=)
--     :setlocal rxs=vim              -- same, with the abbreviation
--     :set regexsyntax&              -- drop the buffer override (follow global)
-- An unknown value fails loud (`E474`), never silently sticks you on a dialect.

-- A FileType autocmd is the idiomatic place to pin a dialect per buffer — e.g.
-- make only Vim-script buffers use vim's own regex dialect for `/` and `:s`:
vim.api.nvim_create_autocmd("FileType", {
  pattern = "vim",
  callback = function(args)
    vim.bo[args.buf].regexsyntax = "vim"
  end,
})

-- Leave the global default "pcre" active at startup; the walkthrough below
-- toggles per-buffer by hand so you can feel the difference. To make vim the
-- global default instead, uncomment:
-- vim.o.regexsyntax = "vim"

--------------------------------------------------------------------------------
-- Try it (the buffer is examples/regexsyntax/sample.txt):
--
-- A) The PCRE default — `$1` capture refs, canonical groups.
--      :2<CR>                               -- jump to "hello world"
--      :s/(\w+) (\w+)/$2 $1/<CR>            -- swap the two words -> "world hello"
--    (`\<foo\>` here would be an INVALID pattern: `\<` is not PCRE.)
--
-- B) Switch to vim's dialect:
--      :set regexsyntax=vim<CR>
--      :set rxs?<CR>                        -- echoes "regexsyntax=vim"
--
-- C) Word boundaries — `\<`/`\>` match a *whole* word:
--      gg0                                  -- line 1: "foo foobar foo"
--      /\<foo\><CR>                         -- skips the "foo" inside "foobar",
--                                              lands on the standalone one at col 11
--
-- D) Vim groups + back-refs + `&` (whole match) in the replacement:
--      :2<CR>
--      :s/\(\w\+\) \(\w\+\)/\2 \1/<CR>      -- "hello world" -> "world hello"
--      :s/\w\+/\u&/g<CR>                    -- Title-Case the line -> "World Hello"
--
-- E) The non-greedy `\{-}` (lspconfig's strip_archive_subpath shape):
--      :4<CR>                               -- "zipfile:///path/to/a::b::c"
--      :s/zipfile:\/\/\(.\{-}\)::.*/\1/<CR> -- stops at the FIRST "::" -> "/path/to/a"
--
-- F) Flip back any time:
--      :set regexsyntax=pcre<CR>            -- (or `:set rxs&` to reset to default)
--
-- G) Per-buffer: pin one buffer to vim, leave the next on the global default:
--      :set regexsyntax=vim<CR>             -- THIS buffer -> vim
--      :enew<CR>                            -- a new buffer follows the global...
--      :set rxs?<CR>                        -- ...echoes "regexsyntax=pcre"
--      :bp<CR>                              -- back; :set rxs? -> "regexsyntax=vim"
--
-- Note: the `vim.fn.substitute()` Lua function and `vim.regex()` ALWAYS speak vim's
-- dialect (plugins expect that) regardless of this option — `regexsyntax` only
-- governs the editor's own `/` and `:s`.
--------------------------------------------------------------------------------
