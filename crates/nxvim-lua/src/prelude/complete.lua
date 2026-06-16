-- nx.complete: the native completion engine over the unified float-list widget
-- (docs/specs/2026-06-14-nx-ui-float-widget.md, Phase 4-A;
-- docs/plans/2026-06-15-nx-complete-completion-engine.md). Unlike the picker, the
-- buffer is the query: the popup floats over the text while typing flows on to
-- the document, and the server (Rust) owns trigger, ranking, navigation, and the
-- accept-edit. The native `buffer` word-scan source runs entirely in core — no Lua
-- per keystroke (ADR 0002 rule 4); the `lsp` source is server-native; `snippets` is
-- a built-in; and plugin sources register via `nx.complete.source{}`.
--
-- `nx.complete.setup{}` validates the configured sources, resolves the options, and
-- hands them to the server via `nx._complete_setup`. `nx.complete.source{}` registers
-- an async plugin source and `nx.complete.trigger()` opens the popup manually; an
-- unknown source name fails loud rather than silently no-opping.

nx.complete = nx.complete or {}
-- Registered async sources (`nx.complete.source{}`), keyed by name. Each is a
-- `{ name, complete = function(ctx) -> promise?, debounce }` spec; `setup{}`
-- selects which become active.
nx.complete._sources = nx.complete._sources or {}

-- The built-in sources. `buffer` is matched in core (no Lua per keystroke); `lsp`
-- is server-native (Phase 4-C: the engine issues `textDocument/completion` and
-- applies the chosen item's `textEdit` on accept). Any other name in
-- `setup{ sources }` must be a *registered* async source (`nx.complete.source{}`)
-- — an unknown name is a hard error (no silent stub): a source that "registers" but
-- never produces candidates is exactly the quietly-broken shape the project forbids.
local BUILTIN_SOURCES = { buffer = true, lsp = true, snippets = true }

-- Default merge priority per built-in source — higher wins, so `lsp` candidates lead
-- `buffer` words of equal match quality. An entry's explicit `priority` overrides.
local DEFAULT_PRIORITY = { lsp = 100, snippets = 90, buffer = 10 }

-- The default debounce (ms) before an async source re-runs on a prefix edit — the
-- global knob, overridable per source (`debounce = N`). `0` runs on every
-- keystroke. The native `buffer` source is never debounced (it is pure core).
nx.complete.debounce = nx.complete.debounce or 120

-- Normalize a `keys` entry to a list of notation strings: a bare string becomes a
-- one-element list, a list passes through, nil becomes empty (the server keeps
-- that action's built-in default). Anything else is a config error.
local function key_list(spec, action)
  if spec == nil then
    return {}
  elseif type(spec) == "string" then
    return { spec }
  elseif type(spec) == "table" then
    for _, k in ipairs(spec) do
      if type(k) ~= "string" then
        error("nx.complete.setup: keys." .. action .. " must be string(s), got " .. type(k))
      end
    end
    return spec
  end
  error("nx.complete.setup: keys." .. action .. " must be a string or list of strings")
end

-- nx.complete.setup { sources = { { "buffer", min_chars = 3 } }, auto = true,
--   keys = { next = "<C-n>", prev = "<C-p>", confirm = "<C-y>", abort = "<C-e>" } }
-- Enables the engine. `sources` is a list of `{ name, opts... }` entries — a
-- built-in (`buffer` / `lsp` / `snippets`) or a registered plugin source.
-- `min_chars` (from a source, or the top level) gates the prefix length before the
-- popup opens. `auto` (default true) completes as you type. `keys` overrides any of
-- the four control actions.
function nx.complete.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.complete.setup: expected a table, got " .. type(opts))
  end

  local sources = opts.sources or { { "buffer" } }
  if type(sources) ~= "table" then
    error("nx.complete.setup: `sources` must be a list")
  end

  -- Validate every source name; capture the buffer source's min_chars override, the
  -- per-source merge priority, whether the `lsp` source is configured, and the active
  -- async sources (registered via `nx.complete.source{}`).
  local min_chars = opts.min_chars
  local async = {}
  local saw_lsp = false
  local saw_snippets = false
  local buffer_priority, lsp_priority, snippets_priority = 0, 0, 0
  -- The union of every active source's trigger chars, as a set (dedup) and an
  -- ordered string handed to the engine (`trigger_chars`). Each char wakes only the
  -- source(s) that declared it; the engine folds it into the prefix/anchor.
  local trigger_set, trigger_chars = {}, ""
  for _, src in ipairs(sources) do
    local name = type(src) == "table" and src[1] or src
    if type(name) ~= "string" then
      error("nx.complete.setup: each source needs a string name as element [1]")
    end
    local registered = nx.complete._sources[name]
    if not BUILTIN_SOURCES[name] and not registered then
      error(
        "nx.complete source '"
          .. name
          .. "' not found — register it with nx.complete.source{} first, or use a "
          .. "built-in ('buffer' / 'lsp'). See docs/plans/2026-06-15-nx-complete-completion-engine.md"
      )
    end
    -- Resolve this entry's merge priority (explicit override, else the built-in
    -- default, else 0). Higher wins in the merged view.
    local priority = (type(src) == "table" and src.priority) or DEFAULT_PRIORITY[name] or 0
    if name == "buffer" then
      buffer_priority = priority
      if type(src) == "table" and src.min_chars ~= nil then
        min_chars = src.min_chars
      end
    elseif name == "lsp" then
      saw_lsp = true
      lsp_priority = priority
    elseif name == "snippets" then
      saw_snippets = true
      snippets_priority = priority
    end
    if registered then
      -- A per-entry `debounce` override (`{ "name", debounce = N }`) wins over the
      -- source's own, which wins over the global default (resolved at run time).
      local debounce = (type(src) == "table" and src.debounce) or registered.debounce
      -- The source's trigger chars (if any) gate its dispatch (`nx._complete_run`)
      -- and join the engine's `trigger_chars` so core folds them into the prefix.
      local chars = registered.trigger and registered.trigger.chars or nil
      if chars then
        for _, c in ipairs(chars) do
          if not trigger_set[c] then
            trigger_set[c] = true
            trigger_chars = trigger_chars .. c
          end
        end
      end
      async[#async + 1] = {
        name = name,
        complete = registered.complete,
        resolve = registered.resolve,
        debounce = debounce,
        chars = chars,
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
  local keys = opts.keys or {}

  -- Activate the async sources for `nx._complete_run`; `has_async` (a Lua async
  -- source OR the server-native `lsp` source) tells the engine (core) to emit a
  -- `(gen, ctx)` per trigger so the server dispatches off the input path.
  nx._complete = {
    sources = async,
    gen = 0,
    debounce = nx.complete.debounce,
    -- The global trigger-char set: in a trigger context a *plain* source (no
    -- `trigger`) stays quiet, so it doesn't compete with the trigger-char source.
    triggers = trigger_set,
    -- Lazy-docs (`resolve`) bookkeeping: a monotonic id stamped onto each
    -- resolvable pushed row and a map id → { resolve, item } the server's
    -- `nx._complete_resolve(id)` looks the callback + original item up in.
    resolve_next = 0,
    resolve_items = {},
  }
  -- The server-native `lsp` and `snippets` sources both dispatch off the input
  -- path, so either (like a Lua async source) needs the `(gen, ctx)` emit.
  local has_async = #async > 0 or saw_lsp or saw_snippets

  nx._complete_setup(
    auto == true,
    min_chars or 1,
    key_list(keys.next, "next"),
    key_list(keys.prev, "prev"),
    key_list(keys.confirm, "confirm"),
    key_list(keys.abort, "abort"),
    has_async,
    saw_lsp,
    buffer_priority,
    lsp_priority,
    docs == true,
    trigger_chars,
    saw_snippets,
    snippets_priority
  )

  -- `keys.trigger` (a string or list) installs an insert-mode mapping that opens
  -- the popup on demand — the keypress half of "trigger by keypress / Lua API".
  -- Handy with `auto = false` (manual-only completion). The Lua API is
  -- `nx.complete.trigger()` (below); mapping it yourself works too.
  for _, lhs in ipairs(key_list(keys.trigger, "trigger")) do
    nx.keymap.set("i", lhs, nx.complete.trigger, { desc = "nx.complete: open completion" })
  end
end

-- nx.complete.trigger(): manually open (or refresh) the completion popup at the
-- cursor, ignoring `auto` / `min_chars` (an explicit request always offers what's
-- there). A no-op outside insert mode or before `nx.complete.setup{}`.
function nx.complete.trigger()
  nx._complete_trigger()
end

-- nx.complete.source { name, complete = function(ctx)[, debounce] }: register an
-- **async** completion source. `complete` streams candidates for the prefix in
-- `ctx` ({ prefix, buf, row, col }): it calls `ctx.push(item)` per result — a
-- string (used as both the menu label and the inserted text) or a table
-- { text = <label>, insert = <applied on accept> } — and signals completion by
-- *returning* (an `nx.async` source returns its promise; a synchronous one just
-- returns — nx is promise-only, so there is no `done` callback).
-- The source runs off the input path (debounced by `debounce` ms, default
-- `nx.complete.debounce`), and its results are generation-gated: a reply for a
-- prefix the user has typed past is dropped. Register a `ctx.on_cancel(fn)` reaper
-- to kill an in-flight job when the next prefix supersedes this one. Activate the
-- source by listing its name in `nx.complete.setup{ sources = { ... } }`.
--
-- `trigger = { chars = { ":" } }` (optional) gates the source: the engine wakes it
-- only when the completion prefix leads with one of those chars (the emoji shape),
-- folding the char into the prefix so the source matches `:smi` and accept replaces
-- from the `:`. `resolve = function(item)` (optional) supplies docs lazily: push an
-- item with no `doc`, and when the user selects it the engine calls `resolve(item)`,
-- which returns a PROMISE of the docs — a doc string, or an item whose `.doc` is
-- used. Use it when computing docs up front for every candidate is wasteful.
function nx.complete.source(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("nx.complete.source: requires a { name = <string>, complete = <fn> } table", 2)
  end
  if type(spec.complete) ~= "function" then
    error("nx.complete.source('" .. spec.name .. "'): complete must be a function", 2)
  end
  if spec.resolve ~= nil and type(spec.resolve) ~= "function" then
    error("nx.complete.source('" .. spec.name .. "'): resolve must be a function", 2)
  end
  if BUILTIN_SOURCES[spec.name] then
    error("nx.complete.source: '" .. spec.name .. "' is a reserved built-in source name", 2)
  end
  -- `trigger = { chars = { ":" } }` (optional): the engine wakes this source only
  -- when the completion prefix leads with one of these chars (and folds the char
  -- into the prefix, so the source matches `:smi`). Validate the shape up front —
  -- a malformed trigger silently never firing is the quietly-broken shape forbidden.
  if spec.trigger ~= nil then
    if type(spec.trigger) ~= "table" or type(spec.trigger.chars) ~= "table" then
      error(
        "nx.complete.source('" .. spec.name .. "'): trigger must be { chars = { <string>... } }",
        2
      )
    end
    for _, c in ipairs(spec.trigger.chars) do
      if type(c) ~= "string" or #c == 0 then
        error(
          "nx.complete.source('" .. spec.name .. "'): trigger.chars must be non-empty strings",
          2
        )
      end
    end
  end
  nx.complete._sources[spec.name] = spec
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
      pcall(reap)
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

-- nx._complete_run(gen, ctx): dispatch the active async sources whose trigger gate
-- the prefix satisfies, under `gen`. Called by the server once per trigger that has
-- an async source. Each source is debounced (a new prefix cancels the in-flight run
-- and any pending timer); its `ctx.push`es land via `nx._complete_push`, and when
-- ALL dispatched sources for this gen have settled (their returned promise resolves,
-- or they returned synchronously), a single `nx._complete_finish(gen)` lets the
-- server close a confirmed-empty popup.
function nx._complete_run(gen, ctx)
  local c = nx._complete
  if not c or #c.sources == 0 then
    return
  end
  complete_cancel_inflight(c)
  c.gen = gen
  -- A fresh run rebuilds the menu, so the previous run's resolve handles are dead
  -- (their rows are gone); drop them before the new pushes assign fresh ids.
  c.resolve_items = {}
  -- Only the sources whose trigger gate matches this prefix run; the rest are
  -- dormant, so they owe no `done()`.
  local active = {}
  for _, source in ipairs(c.sources) do
    if source_wakes(c, source, ctx.prefix) then
      active[#active + 1] = source
    end
  end
  if #active == 0 then
    -- Nothing wakes for this prefix (e.g. only trigger-char sources, no trigger
    -- char typed) — tell the server so it can close a confirmed-empty popup.
    nx._complete_finish(gen)
    return
  end
  -- One `done()` is owed per dispatched source; the last to finish signals the server.
  local pending = #active

  local function finish_one()
    if nx._complete ~= c or c.gen ~= gen then
      return -- a newer prefix already superseded this run
    end
    pending = pending - 1
    if pending <= 0 then
      nx._complete_finish(gen)
    end
  end

  for _, source in ipairs(active) do
    -- The actual invocation — deferred behind the debounce.
    local function dispatch()
      if nx._complete ~= c or c.gen ~= gen then
        return -- the run was superseded while the debounce was pending
      end
      local run_ctx = {
        prefix = ctx.prefix,
        buf = ctx.buf,
        row = ctx.row,
        col = ctx.col,
        gen = gen,
        on_cancel = function(fn)
          if nx._complete == c and c.gen == gen then
            c.reapers[#c.reapers + 1] = fn
          end
        end,
      }
      local labels, inserts, docs, resolves, batched = {}, {}, {}, {}, 0
      local function flush()
        if batched > 0 then
          nx._complete_push(gen, labels, inserts, docs, resolves)
          labels, inserts, docs, resolves, batched = {}, {}, {}, {}, 0
        end
      end
      local function push(item)
        -- Drop a push from a superseded prefix or a torn-down engine.
        if nx._complete ~= c or c.gen ~= gen then
          return
        end
        local label, insert, doc, resolve_id = nil, nil, nil, 0
        if type(item) == "table" then
          label = item.text or item.label or tostring(item.insert)
          insert = item.insert or label
          -- Inline docs for the sidebar (`""` ⇒ none); a source with a `resolve`
          -- callback instead gets a resolve id, so the server fetches docs lazily
          -- (only for the row the user actually lands on).
          doc = item.doc
          if not doc and source.resolve then
            c.resolve_next = c.resolve_next + 1
            resolve_id = c.resolve_next
            c.resolve_items[resolve_id] = { resolve = source.resolve, item = item }
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
        if batched >= FLUSH_N then
          flush()
        end
      end
      -- The source emits through `run_ctx.push` (the sink) and signals completion
      -- by *returning* — a promise (nx.async) or nothing (synchronous). nx is
      -- promise-only, so there is no `done` callback passed in.
      run_ctx.push = push
      -- `finish_one` is owed exactly once per dispatched source. nx.promise.try
      -- folds a synchronous throw and an async rejection into one chain: notify on
      -- either (`:catch`), then settle exactly once whichever way it goes
      -- (`:finally`).
      nx.promise
        .try(source.complete, run_ctx)
        :catch(function(err)
          nx.notify("nx.complete: source '" .. source.name .. "' error: " .. tostring(err), "error")
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
      c.timers[#c.timers + 1] = nx.timer(dispatch, delay)
    else
      dispatch()
    end
  end
end

-- nx._complete_resolve(id): the server asks the plugin source that produced
-- resolve-handle `id` to fetch its lazy docs (the selected row carried a `resolve`
-- callback but no inline `doc`). Look the `(resolve, item)` up, invoke
-- `resolve(item)` — which returns a PROMISE of the docs (a doc string, or an item
-- whose `.doc` is used) — and route the resolved docs back to the server via
-- `nx._complete_resolve_done(id, doc)`. A no-op for an unknown / stale id (the run
-- that produced it was superseded). Phase 4-E.
function nx._complete_resolve(id)
  local c = nx._complete
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
    nx._complete_resolve_done(id, doc or "")
  end
  -- `deliver` is the success action (not a finally), so a throw inside it must NOT
  -- re-trigger the rejection path — `:next(deliver, on_err)` attaches the error
  -- handler to the source promise, not to deliver's result. nx.promise.try folds a
  -- synchronous throw from `resolve` into that same rejection path.
  nx.promise.try(entry.resolve, entry.item):next(deliver, function(err)
    nx.notify("nx.complete: resolve error: " .. tostring(err), "error")
    -- Stamp it resolved-but-docless so the server never re-fires for this row.
    nx._complete_resolve_done(id, "")
  end)
end
