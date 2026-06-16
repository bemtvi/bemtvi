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
-- Autocmd ids registered for invalidation, keyed by target ("global" or
-- "win:<id>"), so a re-`setup{}` of one target replaces only its own autocmds and
-- leaves the others (the global layout and each window-local one) intact.
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

-- Normalize a segment's `render(ctx)` result (a list of `{ text, hl }` cells, or
-- nil) into the parallel `texts` / `groups` arrays the publish bridge takes. A
-- render error becomes a loud `E:<name>` cell rather than failing silently
-- (CLAUDE.md no-silent-stub rule).
local function resolve(name, spec, ctx)
  local ok, cells = pcall(spec.render, ctx)
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
  return texts, groups
end

-- Re-run one custom segment's render **for every window** and publish its
-- resolved cells per window. Each window's `render(ctx)` sees that window's own
-- `{ buf, win, focused }`, so a segment can vary by the window's buffer or by
-- whether it holds focus. Driven by the server (`run_statusline_rerender`) from
-- `run_pending` with a freshly pushed window mirror, so `nx.win.list()` /
-- `nx.win.buf()` / `nx.win.current()` read the post-transition layout.
function nx.statusline._rerender(name)
  local spec = nx.statusline._segments[name]
  if not spec then
    return
  end
  local cur = nx.win.current()
  for _, win in ipairs(nx.win.list()) do
    local ctx = { buf = nx.win.buf(win), win = win, focused = win == cur }
    local texts, groups = resolve(name, spec, ctx)
    nx._statusline_publish(win, name, texts, groups)
  end
end

-- nx.statusline.invalidate(name): mark a custom segment dirty so the server
-- re-renders it (per window) when the current input settles. The async pattern:
-- a job finishes, caches its data, then invalidates its own segment. Deferring to
-- the server (rather than rendering inline) means a re-render always runs against
-- a fresh window mirror — see `nx._statusline_invalidate`.
function nx.statusline.invalidate(name)
  if type(name) ~= "string" then
    error("nx.statusline.invalidate: expected a segment name, got " .. type(name))
  end
  nx._statusline_invalidate(name)
end

-- (Re)register one target's invalidation autocmds: drop the ones it installed
-- before, then create one per (custom segment, declared event) that invalidates
-- the segment. The window set / focus / per-window buffer changes are detected
-- server-side, so a segment need not declare WinEnter/WinNew to stay correct
-- across splits — only its own non-structural triggers (e.g. DirChanged).
local function register_events(target_key, names)
  for _, id in ipairs(nx.statusline._au[target_key] or {}) do
    pcall(nx.autocmd.del, id)
  end
  local ids = {}
  for _, name in ipairs(names) do
    local spec = nx.statusline._segments[name]
    for _, ev in ipairs(spec.events or {}) do
      ids[#ids + 1] = nx.autocmd.create(ev, {
        callback = function()
          nx.statusline.invalidate(name)
        end,
      })
    end
  end
  nx.statusline._au[target_key] = ids
end

-- Resolve the `win` opt to `(target, target_key)`: `nil` → the global layout;
-- a window id (0 = the current window) → a window-local override.
local function target_of(win, fname)
  if win ~= nil and type(win) ~= "number" then
    error(fname .. ": 'win' must be a window id (number)")
  end
  if win == 0 then
    win = nx.win.current()
  end
  return win, win and ("win:" .. win) or "global"
end

-- nx.statusline.setup { left = { "mode", "filename" }, right = { "diagnostics", "location" } }
-- Activate a segment layout. While the global layout is active it takes precedence
-- over the `'statusline'` `%`-format for every window without its own override.
--
-- `opts.win` (0 = current) sets a **window-local** layout that overrides the
-- global one for that window. `opts.format = true` opts a window back to the
-- `'statusline'` `%`-format even while a global segment layout is active (the
-- per-region mix); for the global target it clears the global layout.
function nx.statusline.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.statusline.setup: expected a table, got " .. type(opts))
  end
  local target, target_key = target_of(opts.win, "nx.statusline.setup")

  -- Activate server-side; this marks every referenced custom segment dirty, so
  -- the server renders them (per window) when this input settles. No inline
  -- render — the server drives it with a fresh window mirror and per-window ctx.
  if opts.format then
    register_events(target_key, {})
    nx._statusline_setup(target, "format", {}, {})
    return
  end
  local left = name_list(opts.left, "left")
  local right = name_list(opts.right, "right")
  nx._statusline_setup(target, "segments", left, right)
  register_events(target_key, custom_names(left, right))
end

-- nx.statusline.reset([win]): drop a window-local override (0 = current window) so
-- the window re-inherits the global layout. With no `win` it clears the global
-- layout, returning every inheriting window to the `'statusline'` `%`-format.
function nx.statusline.reset(win)
  local target, target_key = target_of(win, "nx.statusline.reset")
  register_events(target_key, {})
  nx._statusline_setup(target, "inherit", {}, {})
end
