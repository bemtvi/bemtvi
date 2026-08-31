-- ~~~ bemtvi btv.complete playground: the native completion engine ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/ui-complete \
--       cargo run -p bemtvi -- examples/ui-complete/sample.txt
--
-- `btv.complete` is the native completion engine on the unified float-list widget
-- (docs/specs/2026-06-14-btv-ui-float-widget.md, Phase 4). Unlike the picker, the
-- BUFFER is the query: the popup floats over the text while your typing flows on
-- to the document, and the SERVER (Rust) owns trigger detection, the fuzzy
-- matcher (matched chars are highlighted), navigation, and the accept-edit. No
-- input loop runs in Lua (ADR 0002 rule 4).
--
-- The built-in `buffer` source (Phase 4-A) is a rope-side scan of the words
-- already in your buffer — pure core, no Lua per keystroke. Phase 4-B adds
-- `btv.complete.source{}`: register your own ASYNC source whose `complete` function
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
-- A plugin ASYNC source (Phase 4-B). `complete(ctx)` runs off the input path: it
-- receives the live prefix in `ctx.prefix`, streams matching candidates via
-- `ctx.push` (a string, or `{ text = label, insert = applied-text }`), and signals
-- completion by RETURNING — synchronously here, or a promise for a real async
-- source. The same shape drives an LSP/HTTP/`btv.run_stream` source wrapped in
-- `btv.async`: register a `ctx.on_cancel` reaper (e.g. `stream:kill()`) and the
-- engine kills the in-flight job when you type past the prefix.
--------------------------------------------------------------------------------
local KEYWORDS = {
  "function",
  "return",
  "require",
  "completion",
  "connection",
  "configuration",
}
btv.complete.source({
  name = "keywords",
  -- Trailing delay (ms) before this source runs after a keystroke; coalesces a
  -- fast typist's keystrokes into one query. `0` would run on every key.
  debounce = 80,
  complete = function(ctx)
    for _, kw in ipairs(KEYWORDS) do
      -- Only offer keywords that actually extend the prefix — a faithful source
      -- reacts to its input rather than dumping a canned list. No inline `doc`, so
      -- docs are fetched lazily via `resolve` below — only for the row you land on.
      if kw ~= ctx.prefix and kw:sub(1, #ctx.prefix) == ctx.prefix then
        ctx.push({ text = kw, insert = kw })
      end
    end
  end,
  -- `resolve(item)` (Phase 4-E): supply the docs sidebar LAZILY. The engine calls
  -- this only when you SELECT a row that had no inline `doc` — so a costly lookup
  -- (here just a string) runs once per landed-on row, not for every candidate. It
  -- returns a PROMISE of the docs (a doc string, or an item whose `.doc` is used).
  resolve = function(item)
    return btv.promise.resolve({
      doc = "keyword: " .. item.text .. "\n(" .. #item.text .. " chars)",
    })
  end,
})

--------------------------------------------------------------------------------
-- A TRIGGER-CHAR source (Phase 4-E) — the emoji shape from the plugin-API spec.
-- `trigger = { chars = { ":" } }` makes the engine wake this source ONLY after a
-- `:` (and fold the `:` into the prefix, so it matches `:smi` and accepting it
-- replaces from the `:`). In a trigger context the `buffer` / `keywords` sources
-- stay quiet — the colon belongs to the emoji source. Each item carries inline
-- `doc`, shown in the docs sidebar beside the popup.
--------------------------------------------------------------------------------
local EMOJI = {
  { ":smile:", "😄" },
  { ":heart:", "❤️" },
  { ":rocket:", "🚀" },
  { ":tada:", "🎉" },
  { ":thumbsup:", "👍" },
}
btv.complete.source({
  name = "emoji",
  debounce = 0,
  trigger = { chars = { ":" } },
  complete = function(ctx)
    for _, e in ipairs(EMOJI) do
      if e[1]:sub(1, #ctx.prefix) == ctx.prefix then
        ctx.push({ text = e[1], insert = e[2], doc = e[1] .. "  →  " .. e[2] })
      end
    end
  end,
})

--------------------------------------------------------------------------------
-- Enable the engine. `sources` lists the sources to draw from — the native
-- `buffer` built-in plus the `keywords` source registered above. `min_chars`
-- gates how long a prefix must be before the popup opens; `auto` (default true)
-- completes as you type. `keys` overrides any of the four control actions — here
-- we ALSO bind <CR> to accept, for folks who prefer Enter-to-confirm (the default
-- leaves <CR> as a literal newline so it never eats a line break unexpectedly).
--------------------------------------------------------------------------------
btv.complete.setup({
  sources = {
    { "buffer", min_chars = 2 },
    { "keywords" },
    { "emoji" },
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
})

-- The same thing is available as a Lua API — map it yourself if you prefer:
--   btv.keymap.set("i", "<C-x><C-n>", btv.complete.trigger)

-- Try it: open the sample, enter insert mode, and start retyping one of the long
-- identifiers (`config`, `connection`, `completion`, …). The popup offers the
-- matching words from the buffer AND the `keywords` async source (e.g. type
-- `conn` to see `connection` from both, or `func` to get `function`); <C-y>
-- accepts. The async source runs ~80 ms after you pause typing. Land on a
-- `keywords` row and its docs appear beside the popup, fetched lazily via `resolve`.
--
-- Then type a `:` and a letter (`:sm`, `:ro`, …): the emoji source wakes on the
-- colon — the buffer/keyword sources go quiet — and offers `:smile:` / `:rocket:`
-- with the glyph in the docs sidebar; <C-y> replaces `:sm` with 😄.
