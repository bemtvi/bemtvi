-- btv.statusline: the declarative segment registry (the lualine shape) —
-- docs/specs/2026-06-11-native-plugin-api.md §2,
-- docs/plans/2026-06-15-btv-statusline-segments.md. Distinct from the `'statusline'`
-- `%`-format engine: here a config names ordered *segments* for the left and
-- right halves, and the server composes + paints them natively.
--
-- Two kinds of segment:
--   * Built-ins (`mode`, `filename`, `location`, `diagnostics`, …) resolve in
--     core from the per-window status context every frame — no Lua per frame.
--   * Custom segments (`btv.statusline.segment{}`) run their `render(ctx)` only
--     when invalidated — an explicit `btv.statusline.invalidate(name)` (the async
--     pattern) or one of the segment's declared autocmd `events`. The server
--     caches the published cells and paints them until the next invalidation
--     (ADR 0002 rule 4: no re-entering Lua every redraw).

btv.statusline = btv.statusline or {}
-- Registered custom segments (`btv.statusline.segment{}`), keyed by name. Each is a
-- `{ name, render = function(ctx) -> cells, events = { ... } }` spec.
btv.statusline._segments = btv.statusline._segments or {}
-- Autocmd ids registered for invalidation, keyed by target (`"global"` or
-- `"win:<id>"`), so a re-`setup{}` of one target replaces only its own autocmds and
-- leaves the others (the global layout and each window-local one) intact.
btv.statusline._au = btv.statusline._au or {}

-- The built-in segments resolved natively in core (see
-- `bemtvi_core::statusline::builtin_segment`). `setup{}` accepts these names
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

-- `btv.statusline.segment` { name = `"git"`, events = { `"BufEnter"`, `"DirChanged"` },
--   render = function(ctx) return { { text = `" main"`, hl = `"StatusGit"` } } end }
-- Register a custom segment. `render(ctx)` (ctx = { buf, win, focused }) returns a
-- list of cells `{ text = "…", hl = "Group"? }`, or nil/empty for nothing.
-- `events` (optional) are standard autocmd event names that invalidate it.
function btv.statusline.segment(spec)
  if type(spec) ~= "table" then
    error("btv.statusline.segment: expected a table, got " .. type(spec))
  end
  if type(spec.name) ~= "string" then
    error("btv.statusline.segment: 'name' must be a string")
  end
  if type(spec.render) ~= "function" then
    error("btv.statusline.segment: 'render' must be a function")
  end
  if spec.events ~= nil and type(spec.events) ~= "table" then
    error("btv.statusline.segment: 'events' must be a list of event names")
  end
  -- `on_click` (optional) is a `v:lua.<fn>` reference fired on a left-click of the
  -- segment's cells — the same bridge the `%@…%X` format handlers use, kept as a
  -- string (not a function) so it crosses to the server with no per-cell registry.
  -- A cell may override it with its own `on_click`.
  if spec.on_click ~= nil and type(spec.on_click) ~= "string" then
    error("btv.statusline.segment: 'on_click' must be a 'v:lua.<fn>' string")
  end
  btv.statusline._segments[spec.name] = spec
end

-- Validate one side's (`left`/`right`) list of segment names: each must be a
-- built-in or a registered custom segment — an unknown name is a hard error (no
-- silent blank), the same no-stub rule `btv.complete`'s source list enforces.
local function name_list(spec, side)
  if spec == nil then
    return {}
  end
  if type(spec) ~= "table" then
    error("btv.statusline.setup: '" .. side .. "' must be a list of segment names")
  end
  local out = {}
  for _, name in ipairs(spec) do
    if type(name) ~= "string" then
      error("btv.statusline.setup: '" .. side .. "' entries must be strings, got " .. type(name))
    end
    if not BUILTIN[name] and not btv.statusline._segments[name] then
      error(
        "btv.statusline.setup: unknown segment '"
          .. name
          .. "' (not a built-in or a registered btv.statusline.segment)"
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

-- Normalize a segment's `render(ctx)` result (a list of `{ text, hl, on_click }`
-- cells, or nil) into the parallel `texts` / `groups` / `clicks` arrays the publish
-- bridge takes. A render error becomes a loud `E:<name>` cell rather than failing
-- silently (CLAUDE.md no-silent-stub rule). A cell's click handler is its own
-- `on_click` (a `v:lua.<fn>` string), falling back to the segment-wide `on_click`;
-- an empty string means the cell is not clickable.
local function resolve(name, spec, ctx)
  local ok, cells = pcall(spec.render, ctx)
  local texts, groups, clicks = {}, {}, {}
  if not ok then
    texts[1] = "E:" .. name
    groups[1] = "ErrorMsg"
    clicks[1] = ""
  elseif type(cells) == "table" then
    for _, cell in ipairs(cells) do
      if type(cell) == "table" and type(cell.text) == "string" then
        texts[#texts + 1] = cell.text
        -- An empty group string means "the base StatusLine highlight".
        groups[#groups + 1] = type(cell.hl) == "string" and cell.hl or ""
        local click = (type(cell.on_click) == "string" and cell.on_click) or spec.on_click
        clicks[#clicks + 1] = type(click) == "string" and click or ""
      end
    end
  end
  return texts, groups, clicks
end

-- Re-run one custom segment's render **for every window** and publish its
-- resolved cells per window. Each window's `render(ctx)` sees that window's own
-- `{ buf, win, focused }`, so a segment can vary by the window's buffer or by
-- whether it holds focus. Driven by the server (`run_statusline_rerender`) from
-- `run_pending` with a freshly pushed window mirror, so `btv.win.list()` /
-- `btv.win.buf()` / `btv.win.current()` read the post-transition layout.
function btv.statusline._rerender(name)
  local spec = btv.statusline._segments[name]
  if not spec then
    return
  end
  local cur = btv.win.current()
  for _, win in ipairs(btv.win.list()) do
    local ctx = { buf = btv.win.buf(win), win = win, focused = win == cur }
    local texts, groups, clicks = resolve(name, spec, ctx)
    btv._statusline_publish(win, name, texts, groups, clicks)
  end
end

-- `btv.statusline.invalidate(name)`: mark a custom segment dirty so the server
-- re-renders it (per window) when the current input settles. The async pattern:
-- a job finishes, caches its data, then invalidates its own segment. Deferring to
-- the server (rather than rendering inline) means a re-render always runs against
-- a fresh window mirror — see `btv._statusline_invalidate`.
function btv.statusline.invalidate(name)
  if type(name) ~= "string" then
    error("btv.statusline.invalidate: expected a segment name, got " .. type(name))
  end
  btv._statusline_invalidate(name)
end

-- (Re)register one target's invalidation autocmds: drop the ones it installed
-- before, then create one per (custom segment, declared event) that invalidates
-- the segment. The window set / focus / per-window buffer changes are detected
-- server-side, so a segment need not declare WinEnter/WinNew to stay correct
-- across splits — only its own non-structural triggers (e.g. DirChanged).
local function register_events(target_key, names)
  for _, id in ipairs(btv.statusline._au[target_key] or {}) do
    pcall(btv.autocmd.del, id)
  end
  local ids = {}
  for _, name in ipairs(names) do
    local spec = btv.statusline._segments[name]
    for _, ev in ipairs(spec.events or {}) do
      -- A two-word entry `"Event Pattern"` (e.g. `"User DaemonStatusChanged"`) narrows the
      -- trigger to that pattern; a bare event name matches all patterns, as before.
      local event, pattern = ev:match("^(%S+)%s+(.+)$")
      ids[#ids + 1] = btv.autocmd.create(event or ev, {
        pattern = pattern,
        callback = function()
          btv.statusline.invalidate(name)
        end,
      })
    end
  end
  btv.statusline._au[target_key] = ids
end

-- Resolve the `win` opt to `(target, target_key)`: `nil` → the global layout;
-- a window id (0 = the current window) → a window-local override.
local function target_of(win, fname)
  if win ~= nil and type(win) ~= "number" then
    error(fname .. ": 'win' must be a window id (number)")
  end
  if win == 0 then
    win = btv.win.current()
  end
  return win, win and ("win:" .. win) or "global"
end

-- `btv.statusline.setup` { left = { `"mode"`, `"filename"` }, right = { `"diagnostics"`, `"location"` } }
-- Activate a segment layout. While the global layout is active it takes precedence
-- over the `'statusline'` `%`-format for every window without its own override.
--
-- `opts.win` (0 = current) sets a **window-local** layout that overrides the
-- global one for that window. `opts.format = true` opts a window back to the
-- `'statusline'` `%`-format even while a global segment layout is active (the
-- per-region mix); for the global target it clears the global layout.
--
-- `opts.separator` is the connector painted before, between, and after the
-- segments of each half (default `" "`, in the base `StatusLine` look). Pass `""`
-- to disable it — a powerline / themed statusline manages its own padding and
-- section arrows and wants a seamless coloured bar with no unstyled gaps.
function btv.statusline.setup(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("btv.statusline.setup: expected a table, got " .. type(opts))
  end
  local target, target_key = target_of(opts.win, "btv.statusline.setup")

  -- Activate server-side; this marks every referenced custom segment dirty, so
  -- the server renders them (per window) when this input settles. No inline
  -- render — the server drives it with a fresh window mirror and per-window ctx.
  if opts.format then
    register_events(target_key, {})
    btv._statusline_setup(target, "format", {}, {})
    return
  end
  local left = name_list(opts.left, "left")
  local right = name_list(opts.right, "right")
  -- `opts.separator` is the connector painted between/around segments (default a
  -- single space). A powerline / themed statusline that owns its own padding and
  -- section arrows passes `""` for a seamless, gap-free coloured bar.
  local separator = opts.separator
  if separator ~= nil and type(separator) ~= "string" then
    error("btv.statusline.setup: `separator` must be a string")
  end
  btv._statusline_setup(target, "segments", left, right, separator)
  register_events(target_key, custom_names(left, right))
end

-- Fire a `'statusline'` `%@handler@…%X` click region's callback. `handler` is the
-- raw string between `%@` and `@` in the format — required to be a `v:lua.<expr>`
-- reference (the same bridge `%{}`/`%!` use), naming a Lua function. Called by the
-- server (`run_statusline_click`) with neovim's click arguments:
--   (minwid, clicks, button, modifiers)
-- where `button` is `"l"`/`"r"`/`"m"` and `modifiers` is a string of `"s"`/`"c"`/`"a"` (shift /
-- ctrl / alt). Resolves the expression to a function and calls it; a non-`v:lua`
-- handler, an unresolvable expression, or a non-function result errors loud
-- (CLAUDE.md no-silent-stub) so a misconfigured region is visible, not ignored.
function btv._statusline_click(handler, minwid, clicks, button, mods)
  local expr = type(handler) == "string" and handler:match("^v:lua%.(.+)$")
  if not expr then
    error("statusline click handler must be a 'v:lua.<fn>' reference, got: " .. tostring(handler))
  end
  -- `loadstring` (the Lua 5.1 string loader; PUC 5.4 keeps it via runtime.rs's
  -- `loadstring = loadstring or load` shim) compiles the bare `v:lua` expression.
  local chunk, err = loadstring("return " .. expr)
  if not chunk then
    error("statusline click handler 'v:lua." .. expr .. "': " .. tostring(err))
  end
  local fn = chunk()
  if type(fn) ~= "function" then
    error("statusline click handler 'v:lua." .. expr .. "' is not a function")
  end
  fn(minwid, clicks, button, mods)
end

-- `btv.statusline.reset([win])`: drop a window-local override (0 = current window) so
-- the window re-inherits the global layout. With no `win` it clears the global
-- layout, returning every inheriting window to the `'statusline'` `%`-format.
function btv.statusline.reset(win)
  local target, target_key = target_of(win, "btv.statusline.reset")
  register_events(target_key, {})
  btv._statusline_setup(target, "inherit", {}, {})
end
