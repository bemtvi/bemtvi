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
-- The built-in `buffer` source (Phase 4-A) is a rope-side scan of the words
-- already in your buffer — pure core, no Lua per keystroke. Phase 4-B adds
-- `nx.complete.source{}`: register your own ASYNC source whose `complete` function
-- streams candidates for the current prefix off the input path (debounced,
-- generation-gated, so a reply for a prefix you've typed past is dropped). The
-- `lsp` / `snippets` built-ins and the docs preview land in later sub-phases;
-- referencing an unregistered source name in `sources` fails loud on purpose.
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
-- A plugin ASYNC source (Phase 4-B). `complete(ctx, push, done)` runs off the
-- input path: it receives the live prefix in `ctx.prefix`, streams matching
-- candidates via `push` (a string, or `{ text = label, insert = applied-text }`),
-- and calls `done()` when finished. Here it offers a small fixed keyword set — but
-- the same shape drives an LSP/HTTP/`nx.spawn` source: register a `ctx.on_cancel`
-- reaper and the engine kills the in-flight job when you type past the prefix.
--------------------------------------------------------------------------------
local KEYWORDS = {
  "function",
  "return",
  "require",
  "completion",
  "connection",
  "configuration",
}
nx.complete.source {
  name = "keywords",
  -- Trailing delay (ms) before this source runs after a keystroke; coalesces a
  -- fast typist's keystrokes into one query. `0` would run on every key.
  debounce = 80,
  complete = function(ctx, push, done)
    for _, kw in ipairs(KEYWORDS) do
      -- Only offer keywords that actually extend the prefix — a faithful source
      -- reacts to its input rather than dumping a canned list.
      if kw ~= ctx.prefix and kw:sub(1, #ctx.prefix) == ctx.prefix then
        push(kw)
      end
    end
    done()
  end,
}

--------------------------------------------------------------------------------
-- Enable the engine. `sources` lists the sources to draw from — the native
-- `buffer` built-in plus the `keywords` source registered above. `min_chars`
-- gates how long a prefix must be before the popup opens; `auto` (default true)
-- completes as you type. `keys` overrides any of the four control actions — here
-- we ALSO bind <CR> to accept, for folks who prefer Enter-to-confirm (the default
-- leaves <CR> as a literal newline so it never eats a line break unexpectedly).
--------------------------------------------------------------------------------
nx.complete.setup {
  sources = {
    { "buffer", min_chars = 2 },
    { "keywords" },
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
-- matching words from the buffer AND the `keywords` async source (e.g. type
-- `conn` to see `connection` from both, or `func` to get `function`); <C-y>
-- accepts. The async source runs ~80 ms after you pause typing.
