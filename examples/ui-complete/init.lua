-- ~~~ nxvim nx.complete playground: the native completion engine ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/ui-complete \
--       cargo run -p nxvim -- examples/ui-complete/sample.txt
--
-- `nx.complete` is the native completion engine on the unified float-list widget
-- (docs/specs/2026-06-14-nx-ui-float-widget.md, Phase 4). Unlike the picker, the
-- BUFFER is the query: the popup floats over the text while your typing flows on
-- to the document, and the SERVER (Rust) owns trigger detection, the fuzzy
-- matcher (matched chars are highlighted), navigation, and the accept-edit. No
-- input loop runs in Lua (ADR 0002 rule 4).
--
-- This phase (4-A) ships the built-in `buffer` source — a rope-side scan of the
-- words already in your buffer. The `lsp` / `snippets` sources, plugin sources,
-- and the docs preview land in later sub-phases; referencing them here fails loud
-- on purpose (try it: add `{ "lsp" }` to `sources` below).
--
-- In insert mode, once you've typed `min_chars` of a word that prefixes another
-- word in the buffer, a popup appears. It opens with NOTHING selected (noselect,
-- like nvim-cmp) — so <CR> keeps inserting newlines until you actually pick a row:
--   <C-n> / <Tab> / <Down>   select / move down the list
--   <C-p> / <S-Tab> / <Up>   select / move up the list
--   <C-y> / <CR>             accept the highlighted row (only once one is selected)
--   <C-e>                    dismiss the popup (keep what you typed)
--   any other key            dismisses + takes its normal effect (type / <Esc> / …)
-- (A manual <C-Space> trigger preselects the first row, so <C-y>/<CR> accept at once.)

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- Enable the engine. `sources` lists the built-ins to draw from (only `buffer`
-- exists this phase). `min_chars` gates how long a prefix must be before the
-- popup opens; `auto` (default true) completes as you type. `keys` overrides any
-- of the four control actions — here we ALSO bind <CR> to accept, for folks who
-- prefer Enter-to-confirm (the default leaves <CR> as a literal newline so it
-- never eats a line break unexpectedly).
--------------------------------------------------------------------------------
nx.complete.setup {
  sources = {
    { "buffer", min_chars = 2 },
  },
  auto = true,
  keys = {
    next = { "<C-n>", "<Tab>" },
    prev = { "<C-p>", "<S-Tab>" },
    confirm = { "<C-y>", "<CR>" },
    abort = "<C-e>",
    -- A key that opens the popup ON DEMAND, ignoring `auto` / `min_chars` — handy
    -- to force completion on a 1-char (or empty) prefix, or with `auto = false`.
    trigger = "<C-Space>",
  },
}

-- The same thing is available as a Lua API — map it yourself if you prefer:
--   nx.keymap.set("i", "<C-x><C-n>", nx.complete.trigger)

-- Try it: open the sample, enter insert mode, and start retyping one of the long
-- identifiers (`config`, `connection`, `completion`, …). The popup offers the
-- matching words from the buffer; <C-y> accepts.
