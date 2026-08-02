-- ~~~ nxvim option scopes: :set vs :setlocal vs :setglobal ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/global-local-options \
--       cargo run -p nxvim -- examples/global-local-options/sample.txt
--
-- A buffer-local or window-local option has TWO values: the local one on each
-- buffer/window, and the GLOBAL one a newly created buffer is born from. Which one
-- you write is what decides whether a config line reaches the files you open later.
--
--     vim.opt.x        / :set x        -- this buffer/window AND the global value
--     vim.opt_local.x  / :setlocal x   -- this buffer/window only
--     vim.opt_global.x / :setglobal x  -- the global value only
--
-- Reads split the same way: `vim.o` / `vim.bo` / `vim.wo` report the local value,
-- `vim.go` / `vim.opt_global` the global one.

--------------------------------------------------------------------------------
-- 1. `vim.opt` — the one a config almost always wants. It moves BOTH tiers, so
--    every file opened from here on indents by 3 and folds by markers, not just
--    whatever buffer happened to be current while this file ran.
vim.opt.tabstop = 3
vim.opt.expandtab = true
vim.opt.foldmethod = "marker"
vim.opt.foldmarker = "<<<,>>>"

--------------------------------------------------------------------------------
-- 2. Window options carry the same two tiers. A split still copies the window it
--    came from (vim's rule), so these follow you into new splits either way; the
--    global value is what seeds a window with no source to copy — a dock.
vim.opt.number = true
vim.opt.relativenumber = false
vim.opt.foldcolumn = 2
vim.opt.breakindent = true
vim.opt.showbreak = "↪ "

--------------------------------------------------------------------------------
-- 3. `vim.opt_global` — move the tier WITHOUT writing any buffer's own value.
--    How far that reaches depends on how the option is stored:
--      * `tabstop` & co. live in a slot every buffer carries, so the tier is a
--        SEED — it reaches buffers created from here on, never one already open.
--      * `commentstring` / `foldexpr` / `foldmarker` live in a per-buffer map that
--        already spells "unset" as absence, so the tier is a read-time FALLBACK:
--        every buffer with no value of its own follows it, including open ones.
--        (nxvim deviates from vim here, deliberately — see
--        docs/plans/2026-08-01-global-local-options.md.)
--    Either way a buffer's OWN value, set with `:setlocal`, still wins.
vim.opt_global.commentstring = "## %s"

--------------------------------------------------------------------------------
-- 4. `vim.opt_local` — the ftplugin case: one filetype's indent must not become
--    everyone's default, so it writes the buffer and stops there.
nx.on("FileType", { pattern = "lua", callback = function()
  vim.opt_local.tabstop = 2
end })

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. `sample.txt` opened AFTER this config ran — and still got its settings:
--      TYPE:  :set tabstop?          -> tabstop=3
--      TYPE:  i<Tab>x<Esc>           -> three SPACES (expandtab), not a tab
--      TYPE:  :set foldmarker?       -> foldmarker=<<<,>>>
--    The fold in `sample.txt` is closed on open, and the fold gutter (section 2)
--    shows its marker two cells wide.
--      TYPE:  zo                     -> opens it; `zc` closes it again
--
-- 2. The two tiers really are two values. `:setlocal` pins THIS buffer:
--      TYPE:  :setlocal tabstop=8<CR>
--      TYPE:  :set tabstop?          -> tabstop=8   (this buffer)
--      TYPE:  :setglobal tabstop?    -> tabstop=3   (what the next file gets)
--      TYPE:  :enew<CR>              -> a new buffer…
--      TYPE:  :set tabstop?          -> tabstop=3   (…born from the global value)
--
-- 3. Section 3's `vim.opt_global.commentstring` wrote the tier and no buffer:
--      TYPE:  :setglobal commentstring?   -> commentstring=## %s
--    …which every buffer with no `commentstring` of its own reads through — this
--    one, and a file opened afterwards:
--      TYPE:  gcc                    -> the line is commented with "## "
--      TYPE:  u
--      TYPE:  :e other.txt<CR>
--      TYPE:  gcc                    -> "## " here too
--    Pin one buffer and the tier stops reaching it:
--      TYPE:  :setlocal commentstring=;;\ %s<CR>
--      TYPE:  u | gcc                -> ";; " in THIS buffer, "## " everywhere else
--
-- 4. `vim.go` reads the global value, `vim.o` the local one:
--      TYPE:  :lua nx.notify(vim.o.tabstop .. " vs " .. vim.go.tabstop)<CR>
--
-- 5. Some buffer options have NO global value, because the read decides them.
--    nxvim says so out loud instead of storing something nothing reads:
--      TYPE:  :setglobal fileencoding=latin1<CR>
--        -> E5100: fileencoding has no global value (the read decides it per buffer)
