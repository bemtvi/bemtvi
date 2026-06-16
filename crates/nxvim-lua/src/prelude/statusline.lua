-- nx.statusline: the declarative segment registry (the lualine shape) —
-- docs/specs/2026-06-11-native-plugin-api.md §2,
-- docs/plans/2026-06-15-nx-statusline-segments.md. Distinct from the `'statusline'`
-- `%`-format engine: here a config names ordered *segments* for the left and
-- right halves, and the server composes + paints them natively.
--
-- Two kinds of segment:
--   * Built-ins (`mode`, `filename`, `location`, `diagnostics`, …) resolve in
--     core from the per-window status context every frame — no Lua per frame.
--   * Custom segments (`nx.statusline.segment{}`) run their `render(ctx)` only
--     when invalidated — an explicit `nx.statusline.invalidate(name)` (the async
--     pattern) or one of the segment's declared autocmd `events`. The server
--     caches the published cells and paints them until the next invalidation
--     (ADR 0002 rule 4: no re-entering Lua every redraw).

nx.statusline = nx.statusline or {}
-- Registered custom segments (`nx.statusline.segment{}`), keyed by name. Each is a
-- `{ name, render = function(ctx) -> cells, events = { ... } }` spec.
nx.statusline._segments = nx.statusline._segments or {}
-- Autocmd ids registered by the active layout for invalidation, so a re-`setup{}`
-- can remove the old ones before installing the new set.
nx.statusline._au = nx.statusline._au or {}

-- The built-in segments resolved natively in core (see
-- `nxvim_core::statusline::builtin_segment`). `setup{}` accepts these names
-- directly; any other name must be a registered custom segment.
local BUILTIN = {
  mode = true,
  filename = true,
  filepath = true,
  filetype = true,
  encoding = true,
  location = true,
  modified = true,
  readonly = true,
  diagnostics = true,
}

-- nx.statusline.segment { name = "git", events = { "BufEnter", "DirChanged" },
--   render = function(ctx) return { { text = " main", hl = "StatusGit" } } end }
-- Register a custom segment. `render(ctx)` (ctx = { buf, win, focused }) returns a
-- list of cells `{ text = "…", hl = "Group"? }`, or nil/empty for nothing.
-- `events` (optional) are standard autocmd event names that invalidate it.
function nx.statusline.segment(spec)
  if type(spec) ~= "table" then
    error("nx.statusline.segment: expected a table, got " .. type(spec))
  end
  if type(spec.name) ~= "string" then
    error("nx.statusline.segment: 'name' must be a string")
  end
  if type(spec.render) ~= "function" then
    error("nx.statusline.segment: 'render' must be a function")
  end
  if spec.events ~= nil and type(spec.events) ~= "table" then
    error("nx.statusline.segment: 'events' must be a list of event names")
  end
  nx.statusline._segments[spec.name] = spec
end

-- Validate one side's (`left`/`right`) list of segment names: each must be a
-- built-in or a registered custom segment — an unknown name is a hard error (no
-- silent blank), the same no-stub rule nx.complete's source list enforces.
local function name_list(spec, side)
  if spec == nil then
    return {}
  end
  if type(spec) ~= "table" then
    error("nx.statusline.setup: '" .. side .. "' must be a list of segment names")
  end
  local out = {}
  for _, name in ipairs(spec) do
    if type(name) ~= "string" then
      error("nx.statusline.setup: '" .. side .. "' entries must be strings, got " .. type(name))
    end
    if not BUILTIN[name] and not nx.statusline._segments[name] then
      error(
        "nx.statusline.setup: unknown segment '"
          .. name
          .. "' (not a built-in or a registered nx.statusline.segment)"
      )
    end
    out[#out + 1] = name
  end
  return out
end

-- The distinct custom (non-built-in) segment names referenced by a layout.
local function custom_names(left, right)
  local seen, out = {}, {}
  for _, list in ipairs({ left, right }) do
    for _, name in ipairs(list) do
      if not BUILTIN[name] and not seen[name] then
        seen[name] = true
        out[#out + 1] = name
      end
    end
  end
  return out
end

-- The ctx handed to a custom segment's render(ctx). v1 renders for the focused
-- window's buffer; per-window active/inactive differentiation is deferred (the
-- server shares one cell cache per segment — see the plan's "Out of scope").
local function render_ctx()
  local buf = nx._cur_buf and nx._cur_buf.bufnr or 0
  local win = 0
  if nx.api and nx.api.nvim_get_current_win then
    local ok, w = pcall(nx.api.nvim_get_current_win)
    if ok and type(w) == "number" then
      win = w
    end
  end
  return { buf = buf, win = win, focused = true }
end

-- Re-run one custom segment's render and publish its resolved cells to the
-- server. A render error publishes a loud `E:<name>` cell rather than failing
-- silently (CLAUDE.md no-silent-stub rule).
function nx.statusline._rerender(name)
  local spec = nx.statusline._segments[name]
  if not spec then
    return
  end
  local ok, cells = pcall(spec.render, render_ctx())
  local texts, groups = {}, {}
  if not ok then
    texts[1] = "E:" .. name
    groups[1] = "ErrorMsg"
  elseif type(cells) == "table" then
    for _, cell in ipairs(cells) do
      if type(cell) == "table" and type(cell.text) == "string" then
        texts[#texts + 1] = cell.text
        -- An empty group string means "the base StatusLine highlight".
        groups[#groups + 1] = type(cell.hl) == "string" and cell.hl or ""
      end
    end
  end
  nx._statusline_publish(name, texts, groups)
end

-- nx.statusline.invalidate(name): recompute a custom segment now. The async
-- pattern: a job finishes, caches its data, then invalidates its own segment.
function nx.statusline.invalidate(name)
  if type(name) ~= "string" then
    error("nx.statusline.invalidate: expected a segment name, got " .. type(name))
  end
  nx.statusline._rerender(name)
end

-- nx.statusline.setup { left = { "mode", "filename" }, right = { "diagnostics", "location" } }
-- Activate a segment layout. While a layout is active it takes precedence over
-- the `'statusline'` format option for every window.
function nx.statusline.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.statusline.setup: expected a table, got " .. type(opts))
  end
  local left = name_list(opts.left, "left")
  local right = name_list(opts.right, "right")

  -- Drop the autocmds a previous setup installed for invalidation.
  for _, id in ipairs(nx.statusline._au) do
    pcall(nx.autocmd.del, id)
  end
  nx.statusline._au = {}

  nx._statusline_setup(left, right)

  -- For each referenced custom segment: register one autocmd per declared event
  -- that re-renders it, and render it once now to populate the server cache.
  for _, name in ipairs(custom_names(left, right)) do
    local spec = nx.statusline._segments[name]
    for _, ev in ipairs(spec.events or {}) do
      local id = nx.autocmd.create(ev, {
        callback = function()
          nx.statusline._rerender(name)
        end,
      })
      nx.statusline._au[#nx.statusline._au + 1] = id
    end
    nx.statusline._rerender(name)
  end
end
