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

-- The sources implemented natively so far. Any other name in `setup{ sources }`
-- is a hard error (no silent stub): a source that "registers" but never produces
-- candidates is exactly the quietly-broken shape the project forbids.
local BUILTIN_SOURCES = { buffer = true }

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

  -- Validate every source name; capture the buffer source's min_chars override.
  local min_chars = opts.min_chars
  local saw_buffer = false
  for _, src in ipairs(sources) do
    local name = type(src) == "table" and src[1] or src
    if type(name) ~= "string" then
      error("nx.complete.setup: each source needs a string name as element [1]")
    end
    if not BUILTIN_SOURCES[name] then
      error(
        "nx.complete source '"
          .. name
          .. "' not yet implemented (Phase 4-A ships only 'buffer'; see "
          .. "docs/plans/2026-06-15-nx-complete-completion-engine.md)"
      )
    end
    if name == "buffer" then
      saw_buffer = true
      if type(src) == "table" and src.min_chars ~= nil then
        min_chars = src.min_chars
      end
    end
  end
  if not saw_buffer then
    error("nx.complete.setup: Phase 4-A requires the 'buffer' source")
  end

  local auto = opts.auto
  if auto == nil then
    auto = true
  end
  local keys = opts.keys or {}

  nx._complete_setup(
    auto == true,
    min_chars or 1,
    key_list(keys.next, "next"),
    key_list(keys.prev, "prev"),
    key_list(keys.confirm, "confirm"),
    key_list(keys.abort, "abort")
  )
end

-- Plugin / async sources are a later sub-phase. Fail loud rather than pretending
-- to register one.
function nx.complete.source(_)
  error(
    "nx.complete.source{} (plugin sources) is not implemented yet — Phase 4-A "
      .. "ships only the built-in 'buffer' source via nx.complete.setup{}"
  )
end
