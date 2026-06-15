-- nx.complete: the native completion engine over the unified float-list widget
-- (docs/specs/2026-06-14-nx-ui-float-widget.md, Phase 4-A;
-- docs/plans/2026-06-15-nx-complete-completion-engine.md). Unlike the picker, the
-- buffer is the query: the popup floats over the text while typing flows on to
-- the document, and the server (Rust) owns trigger, ranking, navigation, and the
-- accept-edit. This phase ships only the native `buffer` word-scan source, which
-- runs entirely in core — no Lua per keystroke (ADR 0002 rule 4).
--
-- `nx.complete.setup{}` is the whole surface for now: it validates the configured
-- sources, resolves the options, and hands them to the server via
-- `nx._complete_setup`. Plugin sources (`nx.complete.source{}`), the `lsp` /
-- `snippets` built-ins, and the docs preview land in later sub-phases — calling
-- the unimplemented surface fails loud rather than silently no-opping.

nx.complete = nx.complete or {}
-- Registered async sources (`nx.complete.source{}`), keyed by name. Each is a
-- `{ name, complete = function(ctx, push, done), debounce }` spec; `setup{}`
-- selects which become active.
nx.complete._sources = nx.complete._sources or {}

-- The built-in sources. `buffer` is matched in core (no Lua per keystroke); `lsp`
-- is server-native (Phase 4-C: the engine issues `textDocument/completion` and
-- applies the chosen item's `textEdit` on accept). Any other name in
-- `setup{ sources }` must be a *registered* async source (`nx.complete.source{}`)
-- — an unknown name is a hard error (no silent stub): a source that "registers" but
-- never produces candidates is exactly the quietly-broken shape the project forbids.
local BUILTIN_SOURCES = { buffer = true, lsp = true }

-- Default merge priority per built-in source — higher wins, so `lsp` candidates lead
-- `buffer` words of equal match quality. An entry's explicit `priority` overrides.
local DEFAULT_PRIORITY = { lsp = 100, buffer = 10 }

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
-- Enables the engine. `sources` is a list of `{ name, opts... }` entries; only
-- `"buffer"` is recognized this phase. `min_chars` (from the buffer source, or
-- the top level) gates the prefix length before the popup opens. `auto` (default
-- true) completes as you type. `keys` overrides any of the four control actions.
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
  local buffer_priority, lsp_priority = 0, 0
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
    end
    if registered then
      -- A per-entry `debounce` override (`{ "name", debounce = N }`) wins over the
      -- source's own, which wins over the global default (resolved at run time).
      local debounce = (type(src) == "table" and src.debounce) or registered.debounce
      async[#async + 1] = { name = name, complete = registered.complete, debounce = debounce }
    end
  end

  local auto = opts.auto
  if auto == nil then
    auto = true
  end
  local keys = opts.keys or {}

  -- Activate the async sources for `nx._complete_run`; `has_async` (a Lua async
  -- source OR the server-native `lsp` source) tells the engine (core) to emit a
  -- `(gen, ctx)` per trigger so the server dispatches off the input path.
  nx._complete = { sources = async, gen = 0, debounce = nx.complete.debounce }
  local has_async = #async > 0 or saw_lsp

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
    lsp_priority
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

-- nx.complete.source { name, complete = function(ctx, push, done)[, debounce] }:
-- register an **async** completion source. `complete` streams candidates for the
-- prefix in `ctx` ({ prefix, buf, row, col }): it calls `push(item)` per result —
-- a string (used as both the menu label and the inserted text) or a table
-- { text = <label>, insert = <applied on accept> } — and `done()` when finished.
-- The source runs off the input path (debounced by `debounce` ms, default
-- `nx.complete.debounce`), and its results are generation-gated: a reply for a
-- prefix the user has typed past is dropped. Register a `ctx.on_cancel(fn)` reaper
-- to kill an in-flight job when the next prefix supersedes this one. Activate the
-- source by listing its name in `nx.complete.setup{ sources = { ... } }`.
function nx.complete.source(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("nx.complete.source: requires a { name = <string>, complete = <fn> } table", 2)
  end
  if type(spec.complete) ~= "function" then
    error("nx.complete.source('" .. spec.name .. "'): complete must be a function", 2)
  end
  if BUILTIN_SOURCES[spec.name] then
    error("nx.complete.source: '" .. spec.name .. "' is a reserved built-in source name", 2)
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

-- nx._complete_run(gen, ctx): dispatch every active async source for `ctx.prefix`
-- under `gen`. Called by the server once per trigger that has an async source. Each
-- source is debounced (a new prefix cancels the in-flight run and any pending
-- timer); its `push`es land via `nx._complete_push`, and when ALL sources for this
-- gen have called `done()`, a single `nx._complete_finish(gen)` lets the server
-- close a confirmed-empty popup.
function nx._complete_run(gen, ctx)
  local c = nx._complete
  if not c or #c.sources == 0 then
    return
  end
  complete_cancel_inflight(c)
  c.gen = gen
  -- One `done()` is owed per source; the last to finish signals the server.
  local pending = #c.sources

  local function finish_one()
    if nx._complete ~= c or c.gen ~= gen then
      return -- a newer prefix already superseded this run
    end
    pending = pending - 1
    if pending <= 0 then
      nx._complete_finish(gen)
    end
  end

  for _, source in ipairs(c.sources) do
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
      local labels, inserts, batched = {}, {}, 0
      local function flush()
        if batched > 0 then
          nx._complete_push(gen, labels, inserts)
          labels, inserts, batched = {}, {}, 0
        end
      end
      local function push(item)
        -- Drop a push from a superseded prefix or a torn-down engine.
        if nx._complete ~= c or c.gen ~= gen then
          return
        end
        local label, insert
        if type(item) == "table" then
          label = item.text or item.label or tostring(item.insert)
          insert = item.insert or label
        else
          label = tostring(item)
          insert = label
        end
        batched = batched + 1
        labels[batched] = label
        inserts[batched] = insert
        if batched >= FLUSH_N then
          flush()
        end
      end
      local done_called = false
      local function done()
        if done_called then
          return
        end
        done_called = true
        flush()
        finish_one()
      end
      local ok, err = pcall(source.complete, run_ctx, push, done)
      if not ok then
        nx.notify("nx.complete: source '" .. source.name .. "' error: " .. tostring(err), "error")
        done()
      end
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
