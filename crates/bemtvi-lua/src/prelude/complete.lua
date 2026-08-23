-- btv.complete: the native completion engine over the unified float-list widget
-- (docs/specs/2026-06-14-btv-ui-float-widget.md, Phase 4-A;
-- docs/plans/2026-06-15-btv-complete-completion-engine.md). Unlike the picker, the
-- buffer is the query: the popup floats over the text while typing flows on to
-- the document, and the server (Rust) owns trigger, ranking, navigation, and the
-- accept-edit. The native `buffer` word-scan source runs entirely in core — no Lua
-- per keystroke (ADR 0002 rule 4); the `lsp` source is server-native; `snippets` is
-- a built-in; and plugin sources register via `btv.complete.source{}`.
--
-- `btv.complete.setup{}` captures the engine options (keys, `auto`, the built-in
-- sources) and hands them to the server via `btv._complete_setup`. `btv.complete.source{}`
-- registers an async plugin source **and joins it to the live engine incrementally** —
-- a source registered after `setup{}` (or lazy-loaded) starts contributing without the
-- user re-listing it, so a plugin adds completions by registering, not by asking the
-- user to route it. Both funnel through `reconcile()`, which derives the active set
-- from the captured config plus the whole source registry and re-pushes it to the
-- server. `btv.complete.trigger()` opens the popup manually; a source name explicitly
-- listed in `setup{ sources }` that is neither built-in nor registered fails loud.

btv.complete = btv.complete or {}
-- Registered async sources (`btv.complete.source{}`), keyed by name. Each is a
-- `{ name, complete = function(ctx) -> promise?, debounce }` spec; `setup{}`
-- selects which become active.
btv.complete._sources = btv.complete._sources or {}

-- The built-in sources. `buffer` is matched in core (no Lua per keystroke); `lsp`
-- is server-native (Phase 4-C: the engine issues `textDocument/completion` and
-- applies the chosen item's `textEdit` on accept). Any other name in
-- `setup{ sources }` must be a *registered* async source (`btv.complete.source{}`)
-- — an unknown name is a hard error (no silent stub): a source that "registers" but
-- never produces candidates is exactly the quietly-broken shape the project forbids.
local BUILTIN_SOURCES = { buffer = true, lsp = true, snippets = true }

-- Default merge priority per built-in source — higher wins, so `lsp` candidates lead
-- `buffer` words of equal match quality. An entry's explicit `priority` overrides.
-- Per-source **bias** added to a row's fuzzy score to break near-ties (the merge is
-- fuzzy-first, then this bias). Small on purpose: a clearly better match from any
-- source still wins; equally-good candidates just tip toward lsp > snippets > buffer.
local DEFAULT_PRIORITY = { lsp = 8, snippets = 5, buffer = 0 }

-- The default debounce (ms) before an async source re-runs on a prefix edit — the
-- global knob, overridable per source (`debounce = N`). `0` runs on every
-- keystroke. The native `buffer` source is never debounced (it is pure core).
btv.complete.debounce = btv.complete.debounce or 120

-- Normalize a `keys` entry to a list of notation strings (nil → empty: the server
-- keeps that action's built-in default) — the shared `btv.utils.str_list`.
local function key_list(spec, action)
  return btv.utils.str_list(spec, "btv.complete.setup: keys." .. action)
end

-- reconcile(): derive the ACTIVE completion set from the engine config captured by
-- the last `btv.complete.setup{}` (`btv.complete._config`) plus **every** source
-- registered with `btv.complete.source{}`, then (re)push it to the server. `setup{}`
-- calls it once; every later `source{}` calls it again — so a plugin source
-- registered *after* the user's `setup{}` (or lazy-loaded) joins the live engine
-- without the user re-listing it. A no-op until `setup{}` has run (the config it
-- reads doesn't exist yet); a `source{}` registered first is picked up when `setup{}`
-- eventually reconciles. A registered source is active by default; `setup{ exclusive
-- = true }` restricts the active set to only the sources named in `setup{ sources }`.
local function reconcile()
  local cfg = btv.complete._config
  if not cfg then
    return
  end

  -- The native sources' gates/priorities and the min across every active source
  -- (the global open gate). `all_min` starts nil and lowers as sources are seen.
  --
  -- `saw_buffer` rides its own wire slot rather than being read off a `0` gate:
  -- `sources` is the list of sources to draw from, so a `setup{}` that names others
  -- and omits `buffer` gets no buffer words — but `0` is also a legal `min_chars`
  -- ("no gate"), so overloading it would silently disable the source for anyone who
  -- asked for completion from the first character. (The default `sources` is
  -- `{ { "buffer" } }`, so a bare `setup{}` still has them.)
  local buffer_min_chars, lsp_min_chars, snippets_min_chars = 1, 1, 1
  local buffer_priority, lsp_priority, snippets_priority = 0, 0, 0
  local saw_buffer, saw_lsp, saw_snippets = false, false, false
  local all_min = nil
  local function lower(mc)
    all_min = math.min(all_min or mc, mc)
  end

  local bi = cfg.builtins
  if bi.buffer then
    saw_buffer = true
    buffer_priority, buffer_min_chars = bi.buffer.priority, bi.buffer.min_chars
    lower(bi.buffer.min_chars)
  end
  if bi.lsp then
    saw_lsp, lsp_priority, lsp_min_chars = true, bi.lsp.priority, bi.lsp.min_chars
    lower(bi.lsp.min_chars)
  end
  if bi.snippets then
    saw_snippets, snippets_priority, snippets_min_chars =
      true, bi.snippets.priority, bi.snippets.min_chars
    lower(bi.snippets.min_chars)
  end

  -- Every registered plugin source joins the active set (that is the incremental
  -- seam) unless `exclusive` mode limits it to the named ones, or it was opted out
  -- with `enabled = false` (on the spec or the `setup{}` entry). Per-source options
  -- resolve override (the `setup{}` entry) → the source's own default → the global
  -- fallback. The union of their trigger chars is handed to the engine.
  local async = {}
  local trigger_set, trigger_chars = {}, ""
  for name, registered in pairs(btv.complete._sources) do
    local override = cfg.overrides[name]
    local off = registered.enabled == false or (override and override.enabled == false)
    local active = not off and ((not cfg.exclusive) or override ~= nil)
    if active then
      -- Merge priority: the `setup{}` entry's explicit override wins, else the source's
      -- own declared `priority`, else the plugin-source default (0). Higher wins.
      local priority = (override and override.priority) or registered.priority or 0
      -- Per-source `min_chars`: the entry override, else the source's own declared
      -- default, else the top-level `min_chars`, else 1. The source contributes only
      -- once the prefix reaches it (checked in `btv._complete_run`); a trigger-char
      -- source or a manual trigger bypasses it.
      local min_chars = (override and override.min_chars)
        or registered.min_chars
        or cfg.top_min
        or 1
      -- Debounce: the entry override wins over the source's own (resolved to the
      -- global default at run time when both are nil).
      local debounce = (override and override.debounce) or registered.debounce
      local chars = registered.trigger and registered.trigger.chars or nil
      if chars then
        for _, c in ipairs(chars) do
          if not trigger_set[c] then
            trigger_set[c] = true
            trigger_chars = trigger_chars .. c
          end
        end
      end
      lower(min_chars)
      async[#async + 1] = {
        name = name,
        complete = registered.complete,
        resolve = registered.resolve,
        debounce = debounce,
        chars = chars,
        min_chars = min_chars,
        priority = priority,
      }
    end
  end
  -- The global open gate: the popup opens at the lowest per-source threshold (each
  -- source then filters by its own). Falls back to the top-level / default when empty.
  local min_chars = all_min or cfg.top_min or 1

  -- (Re)publish the active async sources for `btv._complete_run`. A live reconcile (a
  -- late `source{}`) preserves the in-flight lazy-docs / on_accept bookkeeping and the
  -- current generation so an open popup mid-resolve isn't torn down; a first setup
  -- starts them fresh (`prev` is nil).
  local prev = btv._complete
  btv._complete = {
    sources = async,
    gen = prev and prev.gen or 0,
    debounce = btv.complete.debounce,
    -- The global trigger-char set: in a trigger context a *plain* source (no
    -- `trigger`) stays quiet, so it doesn't compete with the trigger-char source.
    triggers = trigger_set,
    -- Lazy-docs (`resolve`) bookkeeping: a monotonic id stamped onto each resolvable
    -- pushed row and a map id → { resolve, item } the server's
    -- `btv._complete_resolve(id)` looks the callback + original item up in.
    resolve_next = prev and prev.resolve_next or 0,
    resolve_items = prev and prev.resolve_items or {},
    -- `on_accept` bookkeeping, shaped like the resolve map: a monotonic id stamped
    -- onto each pushed row that carries an `on_accept`, and a map id → { on_accept,
    -- item } the server's `btv._complete_run_accept(id, …)` looks the callback up in
    -- when that row is accepted (its accept is delegated so core doesn't splice).
    accept_next = prev and prev.accept_next or 0,
    accept_items = prev and prev.accept_items or {},
  }
  -- The server-native `lsp` and `snippets` sources both dispatch off the input path,
  -- so either (like a Lua async source) needs the `(gen, ctx)` emit.
  local has_async = #async > 0 or saw_lsp or saw_snippets
  local keys = cfg.keys

  btv._complete_setup(
    cfg.auto == true,
    -- `{ open, buffer, lsp, snippets, buffer_listed }` (one packed tuple slot): the
    -- global open gate, each native source's own threshold, and whether `buffer` was
    -- listed in `sources` at all (`1`/`0` — a gate cannot carry that, since `0` is a
    -- legal gate). Lua sources carry theirs on `btv._complete.sources` (gated in
    -- `btv._complete_run`).
    { min_chars, buffer_min_chars, lsp_min_chars, snippets_min_chars, saw_buffer and 1 or 0 },
    key_list(keys.next, "next"),
    key_list(keys.prev, "prev"),
    key_list(keys.confirm, "confirm"),
    key_list(keys.abort, "abort"),
    has_async,
    saw_lsp,
    -- `{ buffer, lsp, snippets }` merge priorities (one packed tuple slot).
    { buffer_priority, lsp_priority, snippets_priority },
    cfg.docs == true,
    cfg.docs_wrap == true,
    trigger_chars,
    saw_snippets,
    cfg.accept,
    cfg.confirm_first == true
  )
end

-- btv.complete.setup { sources = { { `"buffer"`, min_chars = 3 } }, auto = true,
--   keys = { next = `"<C-n>"`, prev = `"<C-p>"`, confirm = { `"<C-y>"`, `"<CR>"` }, abort = `"<C-e>"` } }
-- (`confirm` accepts the highlighted row; `<CR>` only accepts once you've navigated
--  to one — an unnavigated popup is noselect, so Enter still inserts a newline.)
-- Enables the engine and captures its options. `sources` is a list of `{ name, opts... }`
-- entries — a built-in (`buffer` / `lsp` / `snippets`) or a registered plugin source.
-- Listing a plugin source is **optional**: every source registered with
-- `btv.complete.source{}` is active by default, so a plugin adds completions just by
-- registering (even after this call). List it only to override its options (`min_chars`
-- / `priority` / `debounce`) or to name it under `exclusive` mode. `exclusive = true`
-- restricts the active set to only the sources named here (opt out of auto-join).
-- `min_chars` gates how many prefix chars a source needs before it contributes, and is
-- honored **per source**: `{ "buffer", min_chars = 3 }, { "btvsnip", min_chars = 2 }`
-- shows snippets from 2 chars while buffer words wait for 3. A top-level `min_chars`
-- sets the default for sources that don't override it; the popup opens at the *minimum*
-- across all sources (each then filters by its own). A trigger-char source and a manual
-- trigger (`btv.complete.trigger()`) bypass the gate. `auto` (default true) completes as
-- you type. With `auto = false` nothing opens on its own, but a manual trigger still
-- **follows the prefix**: the popup it opens narrows as you type and widens as you
-- backspace, until you accept, abort, or type a prefix nothing matches — so on-demand
-- completion is a session, not a one-shot snapshot. `keys` overrides any of
-- the four control actions. `accept` (default `"replace"`) decides what the confirm
-- keys do when the caret is in the *middle* of a word: `"replace"` swaps the whole
-- word, `"insert"` keeps the suffix past the cursor. `btv.complete.accept{ behavior }`
-- overrides it for a specific key (bind one key to each behavior).
-- `confirm` (default `"selected"`) decides what a confirm key does when **nothing is
-- selected** — the popup is noselect until you navigate. `"selected"` keeps confirm
-- inert (a mapped `<CR>` still inserts a newline until you pick a row); `"first"` accepts
-- the top row un-navigated (Enter-to-accept). An explicit selection confirms either way.
function btv.complete.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("btv.complete.setup: expected a table, got " .. type(opts))
  end

  local sources = opts.sources or { { "buffer" } }
  if type(sources) ~= "table" then
    error("btv.complete.setup: `sources` must be a list")
  end

  -- Split the listed sources into built-in activations (`buffer` / `lsp` / `snippets`,
  -- with their resolved priority + gate) and per-plugin-source option overrides. A
  -- *listed* name that is neither built-in nor registered fails loud; an *unlisted*
  -- registered source still auto-joins (via `reconcile`).
  local top_min = opts.min_chars
  local builtins = {}
  local overrides = {}
  for _, src in ipairs(sources) do
    local o = type(src) == "table" and src or {}
    local name = type(src) == "table" and src[1] or src
    if type(name) ~= "string" then
      error("btv.complete.setup: each source needs a string name as element [1]")
    end
    local registered = btv.complete._sources[name]
    if not BUILTIN_SOURCES[name] and not registered then
      error(
        "btv.complete source '"
          .. name
          .. "' not found — register it with btv.complete.source{} first, or use a "
          .. "built-in ('buffer' / 'lsp' / 'snippets'). "
          .. "See docs/plans/2026-06-15-btv-complete-completion-engine.md"
      )
    end
    if BUILTIN_SOURCES[name] then
      -- A built-in's priority resolves from the entry, else its default; its gate from
      -- the entry, else the top-level `min_chars`, else 1.
      builtins[name] = {
        priority = o.priority or DEFAULT_PRIORITY[name] or 0,
        min_chars = o.min_chars or top_min or 1,
      }
    else
      -- Only carry the explicitly-set overrides; `reconcile` fills the rest from the
      -- source's own declared defaults so an unlisted source resolves identically.
      overrides[name] = {
        priority = o.priority,
        min_chars = o.min_chars,
        debounce = o.debounce,
        enabled = o.enabled,
      }
    end
  end

  local auto = opts.auto
  if auto == nil then
    auto = true
  end
  -- The docs sidebar (the selected item's documentation, beside the popup) is on by
  -- default; `docs = false` turns it off. Only the server-native `lsp` source ever
  -- has docs to show, so a buffer-only config never renders one regardless.
  local docs = opts.docs
  if docs == nil then
    docs = true
  end
  -- The docs float wraps a long doc line within itself by default; `docs_wrap = false`
  -- truncates long lines at the float's edge instead. The wheel still scrolls it
  -- vertically either way.
  local docs_wrap = opts.docs_wrap
  if docs_wrap == nil then
    docs_wrap = true
  end
  -- `accept` (default "replace") decides what the confirm keys do when the caret sits
  -- in the *middle* of a word: "replace" swaps the whole word, "insert" keeps the
  -- suffix past the cursor. `btv.complete.accept{ behavior = … }` overrides it per-key.
  local accept = opts.accept
  if accept == nil then
    accept = "replace"
  elseif accept ~= "insert" and accept ~= "replace" then
    error("btv.complete.setup: `accept` must be 'insert' or 'replace', got " .. tostring(accept))
  end
  -- `confirm` (default "selected") decides what a confirm key (`keys.confirm`) does when
  -- **nothing is selected yet** — the popup opens *noselect* (nothing highlighted until
  -- you navigate with `<C-n>`/`<C-p>`). "selected" leaves confirm inert until you pick a
  -- row, so a mapped `<CR>` still inserts a newline while the popup is up (the safe
  -- default). "first" makes confirm accept the *top* row even un-navigated (Enter-to-
  -- accept). An explicit selection always confirms under either mode.
  local confirm = opts.confirm
  if confirm == nil then
    confirm = "selected"
  elseif confirm ~= "selected" and confirm ~= "first" then
    error("btv.complete.setup: `confirm` must be 'selected' or 'first', got " .. tostring(confirm))
  end

  -- Capture the engine config so a later `btv.complete.source{}` can reconcile the
  -- active set against it without a fresh `setup{}`. `reconcile` derives everything
  -- source-related (the async list, trigger chars, `has_async`, the open gate) from
  -- this plus the source registry.
  btv.complete._config = {
    auto = auto,
    docs = docs,
    docs_wrap = docs_wrap,
    accept = accept,
    confirm_first = confirm == "first",
    top_min = top_min,
    keys = opts.keys or {},
    exclusive = opts.exclusive == true,
    builtins = builtins,
    overrides = overrides,
  }
  reconcile()

  local keys = btv.complete._config.keys
  -- `keys.trigger` (a string or list) installs the insert-mode mapping(s) that open
  -- the popup on demand — the keypress half of "trigger by keypress / Lua API".
  -- Unset, it defaults to `<C-Space>` / `<C-x><C-o>`, installed as overridable
  -- `default` maps (so a user `vim.keymap.set` for those keys still wins); pass an
  -- empty list to disable it entirely. The Lua API is `btv.complete.trigger()` (below);
  -- mapping it yourself works too. (These are ordinary Lua maps — the trigger is no
  -- longer a Rust native default.)
  local trigger_default = keys.trigger == nil
  local trigger_keys = trigger_default and { "<C-Space>", "<C-x><C-o>" }
    or key_list(keys.trigger, "trigger")
  for _, lhs in ipairs(trigger_keys) do
    btv.keymap.set("i", lhs, btv.complete.trigger, {
      default = trigger_default,
      desc = "btv.complete: open completion",
    })
  end
end

-- btv.complete.trigger(): manually open (or refresh) the completion popup at the
-- cursor, ignoring `auto` / `min_chars` (an explicit request always offers what's
-- there). A no-op outside insert mode or before `btv.complete.setup{}`.
--
-- The popup it opens is a **session**, not a snapshot: it keeps following the prefix
-- through the edits that follow — narrowing as you type, widening as you backspace —
-- even with `auto = false`, and ends when it closes (accept, abort, `<Esc>`, or a
-- prefix nothing matches). The whole session keeps the manual contract: `min_chars`
-- stays bypassed, and the top row stays preselected so a confirm key
-- (`<C-y>` / `<CR>`) accepts without a separate navigation step.
function btv.complete.trigger()
  btv._complete_trigger()
end

-- btv.complete.accept(behavior): accept the highlighted completion row under an
-- explicit accept behavior, ignoring the engine's configured default. `behavior` is
-- `"insert"` (replace only the typed prefix, keeping any word suffix past the cursor)
-- or `"replace"` (swap the whole word the caret sits inside); nil / omitted uses the
-- configured default. Passed a table, it reads `behavior` from it (so
-- `btv.complete.accept{ behavior = "replace" }` works too). Bind two keys to the two
-- behaviors for insert-vs-replace on demand, e.g.
--   `btv.keymap.set("i", "<C-y>", function() btv.complete.accept("insert") end)`
--   `btv.keymap.set("i", "<C-l>", function() btv.complete.accept("replace") end)`
-- A no-op when the popup is closed or nothing is highlighted (like the confirm key).
function btv.complete.accept(behavior)
  if type(behavior) == "table" then
    behavior = behavior.behavior
  end
  if behavior ~= nil and behavior ~= "insert" and behavior ~= "replace" then
    error("btv.complete.accept: behavior must be 'insert' or 'replace', got " .. tostring(behavior))
  end
  btv._complete_accept(behavior or "")
end

-- `btv.complete.choice(items, opts)` — open a **non-grabbing** dropdown at the cursor
-- listing `items` (strings), the completion-popup widget rather than the grabbing
-- `btv.ui.select`: `<C-n>`/`<C-p>` move, `<C-y>`/`<CR>` pick, and typing / `<Esc>`
-- dismiss it (input keeps flowing to the buffer). Accepting a row **replaces**
-- `opts.range` — `{ start_row, start_col, end_row, end_col }`, 0-based byte cols — with
-- the pick; the row already sitting in that range is preselected. There is no callback:
-- the splice is a normal buffer edit, so a caller watching the buffer (`btv.buf.attach`
-- `on_bytes`) reacts to it — how a plugin snippet engine drives a `${1|a,b,c|}` choice
-- tabstop and syncs its mirrors. Defaults the range to the empty span at the cursor
-- (inserting the pick) when `opts.range` is omitted.
function btv.complete.choice(items, opts)
  if type(items) ~= "table" then
    error("btv.complete.choice: items must be a list of strings", 2)
  end
  if #items == 0 then
    return
  end
  opts = opts or {}
  local r = opts.range
  if r == nil then
    local pos = btv.win.cursor(0) -- { row (1-based), col (0-based byte) }
    r = { pos[1] - 1, pos[2], pos[1] - 1, pos[2] }
  end
  btv._choice_menu(items, r[1], r[2], r[3], r[4])
end

-- btv.complete.source { name, complete = function(ctx)[, debounce] }: register an
-- **async** completion source. `complete` streams candidates for the prefix in
-- `ctx` ({ prefix, buf, row, col }): it calls `ctx.push(item)` per result — a
-- string (used as both the menu label and the inserted text) or a table
-- { text = <label>, insert = <applied on accept>[, kind = <label>] } — where an
-- optional `kind` is the short category shown right-aligned on the row (`"Function"`,
-- `"Module"`, …), like an LSP item's kind — and signals completion by
-- *returning* (an `btv.async` source returns its promise; a synchronous one just
-- returns — btv is promise-only, so there is no `done` callback).
-- The source runs off the input path (debounced by `debounce` ms, default
-- `btv.complete.debounce`), and its results are generation-gated: a reply for a
-- prefix the user has typed past is dropped. Register a `ctx.on_cancel(fn)` reaper
-- to kill an in-flight job when the next prefix supersedes this one.
--
-- Registering **activates** the source: it joins the live engine immediately (or when
-- `btv.complete.setup{}` next runs, if it hasn't yet) — a plugin adds completions by
-- calling this, with no need for the user to list it in `setup{ sources }`. The spec's
-- own `priority` (merge rank, default 0), `min_chars` (its prefix gate), and `debounce`
-- become the source's defaults; a `setup{ sources }` entry for the same name overrides
-- any of them, and `enabled = false` (here or on that entry) opts it out. A `setup{
-- exclusive = true }` engine ignores unlisted sources.
--
-- `trigger = { chars = { ":" } }` (optional) gates the source: the engine wakes it
-- only when the completion prefix leads with one of those chars (the emoji shape),
-- folding the char into the prefix so the source matches `:smi` and accept replaces
-- from the `:`. `resolve = function(item)` (optional) supplies docs lazily: push an
-- item with no `doc`, and when the user selects it the engine calls `resolve(item)`,
-- which returns a PROMISE of the docs — a doc string, or an item whose `.doc` is
-- used. Use it when computing docs up front for every candidate is wasteful.
--
-- A pushed item may carry `on_accept = function(item, ctx)` — a per-item callback that
-- OWNS the edit when that row is accepted, run INSTEAD of core splicing the item's
-- `insert`. `ctx` is `{ buf, start_row, start_col, end_row, end_col }` — the trigger
-- word RANGE under the cursor (0-based, byte columns, end-exclusive), ready to hand to
-- `btv.buf.set_text`. This is the seam a snippet engine uses: match a trigger, then on
-- accept `btv.buf.set_text(ctx.buf, ctx.start_row, ctx.start_col, ctx.end_row,
-- ctx.end_col, {""})` and `btv.snippet.expand(body)` (or drive its own tabstop session).
-- Not a snippet-only hook — additionalTextEdits, post-accept commands, and any
-- non-literal expander use it too.
function btv.complete.source(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("btv.complete.source: requires a { name = <string>, complete = <fn> } table", 2)
  end
  if type(spec.complete) ~= "function" then
    error("btv.complete.source('" .. spec.name .. "'): complete must be a function", 2)
  end
  if spec.resolve ~= nil and type(spec.resolve) ~= "function" then
    error("btv.complete.source('" .. spec.name .. "'): resolve must be a function", 2)
  end
  if spec.min_chars ~= nil and type(spec.min_chars) ~= "number" then
    error("btv.complete.source('" .. spec.name .. "'): min_chars must be a number", 2)
  end
  if BUILTIN_SOURCES[spec.name] then
    error("btv.complete.source: '" .. spec.name .. "' is a reserved built-in source name", 2)
  end
  -- `trigger = { chars = { ":" } }` (optional): the engine wakes this source only
  -- when the completion prefix leads with one of these chars (and folds the char
  -- into the prefix, so the source matches `:smi`). Validate the shape up front —
  -- a malformed trigger silently never firing is the quietly-broken shape forbidden.
  if spec.trigger ~= nil then
    if type(spec.trigger) ~= "table" or type(spec.trigger.chars) ~= "table" then
      error(
        "btv.complete.source('" .. spec.name .. "'): trigger must be { chars = { <string>... } }",
        2
      )
    end
    for _, c in ipairs(spec.trigger.chars) do
      if type(c) ~= "string" or #c == 0 then
        error(
          "btv.complete.source('" .. spec.name .. "'): trigger.chars must be non-empty strings",
          2
        )
      end
    end
  end
  btv.complete._sources[spec.name] = spec
  -- Join it to the live engine now (or leave it for `setup{}` to pick up if the engine
  -- isn't configured yet — `reconcile` no-ops until then). This is the incremental seam:
  -- a source registered after `setup{}` starts contributing without a re-`setup{}`.
  reconcile()
end

-- Batch async candidates to the server (one bridge crossing per chunk, like the
-- picker) rather than one per item.
local FLUSH_N = 256

-- Reap the active completion run's in-flight jobs (a source's `on_cancel`) and
-- cancel any pending debounce timers, so a new prefix — or a fresh `setup{}` —
-- stops the current sources mid-flight.
local function complete_cancel_inflight(c)
  if c.reapers then
    for _, reap in ipairs(c.reapers) do
      local ok, err = pcall(reap)
      if not ok then
        btv.notify("btv.complete: on_cancel error: " .. tostring(err), 4)
      end
    end
  end
  if c.timers then
    for _, t in ipairs(c.timers) do
      t:stop()
    end
  end
  c.reapers, c.timers = {}, {}
end

-- The first UTF-8 char of `s` (for trigger gating), or "" when empty. Byte-pattern
-- so a multibyte char isn't split — trigger chars are usually ASCII punctuation,
-- but the gate stays correct for any leading char.
local function first_char(s)
  return s:match("^[%z\1-\127\194-\244][\128-\191]*") or ""
end

-- Whether `source` should run for `prefix` under the trigger gate (Phase 4-E): a
-- source with `trigger.chars` wakes only when the prefix leads with one of them; a
-- plain source stays quiet in any trigger context (the prefix leads with *some*
-- registered trigger char), so it never competes with the trigger-char source.
local function source_wakes(c, source, prefix)
  local lead = first_char(prefix)
  if source.chars then
    for _, ch in ipairs(source.chars) do
      if ch == lead then
        return true
      end
    end
    return false
  end
  return lead == "" or not c.triggers[lead]
end

-- Whether `source` has met its own `min_chars` gate for this run. A manual trigger
-- (`ctx.manual`) offers everything, and a trigger-char source is woken by the char
-- itself — both bypass. Otherwise the prefix must reach the source's `min_chars`
-- (counted in characters, so a multibyte prefix is measured correctly).
local function source_meets_min(source, ctx)
  if ctx.manual or source.chars then
    return true
  end
  local n = utf8.len(ctx.prefix) or #ctx.prefix
  return n >= (source.min_chars or 1)
end

-- btv._complete_run(gen, ctx): dispatch the active async sources whose trigger gate
-- the prefix satisfies, under `gen`. Called by the server once per trigger that has
-- an async source. Each source is debounced (a new prefix cancels the in-flight run
-- and any pending timer); its `ctx.push`es land via `btv._complete_push`, and when
-- ALL dispatched sources for this gen have settled (their returned promise resolves,
-- or they returned synchronously), a single `btv._complete_finish(gen)` lets the
-- server close a confirmed-empty popup.
function btv._complete_run(gen, ctx)
  local c = btv._complete
  if not c or #c.sources == 0 then
    return
  end
  complete_cancel_inflight(c)
  c.gen = gen
  -- A fresh run rebuilds the menu, so the previous run's resolve / accept handles are
  -- dead (their rows are gone); drop them before the new pushes assign fresh ids.
  c.resolve_items = {}
  c.accept_items = {}
  -- Only the sources whose trigger gate matches this prefix run; the rest are
  -- dormant, so they owe no `done()`.
  local active = {}
  for _, source in ipairs(c.sources) do
    if source_wakes(c, source, ctx.prefix) and source_meets_min(source, ctx) then
      active[#active + 1] = source
    end
  end
  if #active == 0 then
    -- Nothing wakes for this prefix (e.g. only trigger-char sources, no trigger
    -- char typed) — tell the server so it can close a confirmed-empty popup.
    btv._complete_finish(gen)
    return
  end
  -- One `done()` is owed per dispatched source; the last to finish signals the server.
  local pending = #active

  local function finish_one()
    if btv._complete ~= c or c.gen ~= gen then
      return -- a newer prefix already superseded this run
    end
    pending = pending - 1
    if pending <= 0 then
      btv._complete_finish(gen)
    end
  end

  for _, source in ipairs(active) do
    -- The actual invocation — deferred behind the debounce.
    local function dispatch()
      if btv._complete ~= c or c.gen ~= gen then
        return -- the run was superseded while the debounce was pending
      end
      local run_ctx = {
        prefix = ctx.prefix,
        buf = ctx.buf,
        row = ctx.row,
        col = ctx.col,
        gen = gen,
        on_cancel = function(fn)
          if btv._complete == c and c.gen == gen then
            c.reapers[#c.reapers + 1] = fn
          end
        end,
      }
      local labels, inserts, docs, resolves, accepts, kinds, batched = {}, {}, {}, {}, {}, {}, 0
      local function flush()
        if batched > 0 then
          btv._complete_push(
            gen,
            labels,
            inserts,
            docs,
            resolves,
            accepts,
            kinds,
            source.priority or 0
          )
          labels, inserts, docs, resolves, accepts, kinds, batched = {}, {}, {}, {}, {}, {}, 0
        end
      end
      local function push(item)
        -- Drop a push from a superseded prefix or a torn-down engine.
        if btv._complete ~= c or c.gen ~= gen then
          return
        end
        local label, insert, doc, resolve_id, accept_id, kind = nil, nil, nil, 0, 0, nil
        if type(item) == "table" then
          label = item.text or item.label or tostring(item.insert)
          insert = item.insert or label
          -- The short kind label shown right-aligned on the row (`"Function"`,
          -- `"Snippet"`, …); `nil` ⇒ the row shows no kind column.
          kind = item.kind
          -- Inline docs for the sidebar (`""` ⇒ none); a source with a `resolve`
          -- callback instead gets a resolve id, so the server fetches docs lazily
          -- (only for the row the user actually lands on).
          doc = item.doc
          if not doc and source.resolve then
            c.resolve_next = c.resolve_next + 1
            resolve_id = c.resolve_next
            c.resolve_items[resolve_id] = { resolve = source.resolve, item = item }
          end
          -- A per-item `on_accept` delegates this row's accept to Lua: it runs the
          -- callback (which owns the edit — e.g. a snippet expansion over the trigger
          -- range) INSTEAD of core splicing `insert`. Assign it an accept id the
          -- delegated-accept drain resolves back to the callback + item.
          if type(item.on_accept) == "function" then
            c.accept_next = c.accept_next + 1
            accept_id = c.accept_next
            c.accept_items[accept_id] = { on_accept = item.on_accept, item = item }
          elseif item.on_accept ~= nil then
            error("btv.complete source item: on_accept must be a function", 2)
          end
        else
          label = tostring(item)
          insert = label
        end
        batched = batched + 1
        labels[batched] = label
        inserts[batched] = insert
        docs[batched] = doc or ""
        resolves[batched] = resolve_id
        accepts[batched] = accept_id
        kinds[batched] = kind or ""
        if batched >= FLUSH_N then
          flush()
        end
      end
      -- The source emits through `run_ctx.push` (the sink) and signals completion
      -- by *returning* — a promise (btv.async) or nothing (synchronous). btv is
      -- promise-only, so there is no `done` callback passed in.
      run_ctx.push = push
      -- `finish_one` is owed exactly once per dispatched source. btv.promise.try
      -- folds a synchronous throw and an async rejection into one chain: notify on
      -- either (`:catch`), then settle exactly once whichever way it goes
      -- (`:finally`).
      btv.promise
        .try(source.complete, run_ctx)
        :catch(function(err)
          btv.notify(
            "btv.complete: source '" .. source.name .. "' error: " .. tostring(err),
            "error"
          )
        end)
        :finally(function()
          flush()
          finish_one()
        end)
    end

    local delay = source.debounce
    if delay == nil then
      delay = c.debounce
    end
    if delay and delay > 0 then
      c.timers[#c.timers + 1] = btv.timer(dispatch, delay)
    else
      dispatch()
    end
  end
end

-- btv._complete_resolve(id): the server asks the plugin source that produced
-- resolve-handle `id` to fetch its lazy docs (the selected row carried a `resolve`
-- callback but no inline `doc`). Look the `(resolve, item)` up, invoke
-- `resolve(item)` — which returns a PROMISE of the docs (a doc string, or an item
-- whose `.doc` is used) — and route the resolved docs back to the server via
-- `btv._complete_resolve_done(id, doc)`. A no-op for an unknown / stale id (the run
-- that produced it was superseded). Phase 4-E.
function btv._complete_resolve(id)
  local c = btv._complete
  local entry = c and c.resolve_items and c.resolve_items[id]
  if not entry then
    return
  end
  local function deliver(resolved)
    local doc
    if type(resolved) == "table" then
      doc = resolved.doc
    elseif type(resolved) == "string" then
      doc = resolved
    end
    btv._complete_resolve_done(id, doc or "")
  end
  -- `deliver` is the success action (not a finally), so a throw inside it must NOT
  -- re-trigger the rejection path — `:next(deliver, on_err)` attaches the error
  -- handler to the source promise, not to deliver's result. btv.promise.try folds a
  -- synchronous throw from `resolve` into that same rejection path.
  btv.promise.try(entry.resolve, entry.item):next(deliver, function(err)
    btv.notify("btv.complete: resolve error: " .. tostring(err), "error")
    -- Stamp it resolved-but-docless so the server never re-fires for this row.
    btv._complete_resolve_done(id, "")
  end)
end

-- btv._complete_run_accept(id, buf, start_row, start_col, end_row, end_col): the server
-- accepted a plugin row whose item carried an `on_accept`. Look the callback + item up
-- and run it, handing it the item and a `ctx` describing the buffer and the trigger
-- RANGE it should replace (the word under the cursor — `(start_row, start_col)` to
-- `(end_row, end_col)`, 0-based, byte columns). The callback owns the edit: it typically
-- `btv.buf.set_text`s an expansion over that range, or `btv.snippet.expand`s a body. A
-- no-op for an unknown / stale id (the producing run was superseded).
function btv._complete_run_accept(id, buf, start_row, start_col, end_row, end_col)
  local c = btv._complete
  local entry = c and c.accept_items and c.accept_items[id]
  if not entry then
    return
  end
  local ctx = {
    buf = buf,
    -- The trigger range to replace (end-exclusive), ready to hand to btv.buf.set_text.
    start_row = start_row,
    start_col = start_col,
    end_row = end_row,
    end_col = end_col,
  }
  local ok, err = pcall(entry.on_accept, entry.item, ctx)
  if not ok then
    btv.notify("btv.complete: on_accept error: " .. tostring(err), "error")
  end
end

-- `btv.complete.scorer(src)`: install a **re-ranker** over the completion popup's
-- rows, or clear it with `nil`. The sibling of `btv.picker.scorer`.
--
-- `src` is a string of Lua *source* — an expression, not a function value —
-- because the re-ranker runs in the bounded compute sandbox: a second, pure VM
-- with a wall-clock deadline, no editor state and no `btv.*`. A closure cannot
-- cross between VMs, so the source crosses instead and is compiled there.
--
-- Four names are in scope, and the expression returns a number — the new sort
-- key, **higher first**:
--
-- ```
-- label   the candidate's text
-- query   the word prefix being completed
-- score   the native key this row already earned (fuzzy score + source bias)
-- kind    the row's kind label: "Snippet", an LSP kind name, or "" (a buffer word)
-- ```
--
-- `score` is the **blended** key the popup already sorted on — the fuzzy score
-- plus the source's own bias (`lsp` 8 > snippets 5 > buffer 0) — so nudging it
-- composes with source order instead of fighting it:
--
-- ```lua
-- -- keep snippets available but below real code completions
-- btv.complete.scorer([[ score - (kind == "Snippet" and 20 or 0) ]])
--
-- -- prefer shorter candidates among equally-good matches
-- btv.complete.scorer([[ score - #label / 10 ]])
--
-- btv.complete.scorer(nil)   -- back to the native order
-- ```
--
-- It re-ranks the **filtered** rows only, at most the top 1000 of them, and at
-- most once per repaint rather than once per streamed batch — an LSP server can
-- answer with thousands of candidates. The caret follows the row it was standing
-- on, so a re-rank never accepts a candidate you did not choose.
--
-- The sandbox is **stateless**: nothing carries from one call to the next, and
-- assigning a global raises. A scorer that errors, exceeds its deadline, or
-- returns a non-number reports once and is then uninstalled, rather than
-- repeating the error on every keystroke.
function btv.complete.scorer(src)
  btv._sandbox_set("complete.scorer", btv._complete_set_scorer, src)
end
