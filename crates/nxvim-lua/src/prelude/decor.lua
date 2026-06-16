-- nx.decor: viewport-scoped decoration providers (docs/specs/2026-06-11-native-plugin-api.md
-- §6; docs/plans/2026-06-15-nx-decor-viewport-decorations.md). A provider is woken
-- ONCE per visible-range change (scroll / resize / edit reflow), OFF the frame,
-- handed a snapshot of the visible slice, and PUBLISHES marks carrying a generation
-- token; a publish from a viewport the user has scrolled past is dropped. There is no
-- per-row / per-frame callback (ADR 0002 rule 4 forbids frame-time Lua) — the engine
-- re-enters Lua only when the viewport actually moves, which is why expensive,
-- on-screen-only decorations (rainbow parens, indent guides, inline blame) fit here.
--
-- This file: the provider registry + the off-tick dispatch + the `ctx` snapshot.
-- The server drains the per-window "viewport changed" signal (core
-- `editor/decor.rs`), builds `ctx`, and calls `nx._decor_dispatch(ctx)`; each matching
-- provider's `on_range(ctx, publish)` runs. `publish` normalizes the marks and lowers
-- them into the provider's extmark namespace via `nx._decor_publish`, so they RENDER
-- (gen-gated — a publish from a viewport the user already scrolled past is dropped).

nx.decor = nx.decor or {}

-- nx._decor holds the registered providers (and the last published marks, on
-- `nx._decor.last`, for inspection / tests). Each provider: { name, bufs, on_range, ns }.
nx._decor = nx._decor or { providers = {} }

-- nx.decor.provider { name, bufs?, on_range }: register a viewport decoration
-- provider. `on_range(ctx, publish)` is called off the frame, once per visible-range
-- change of a matching window, with a snapshot
--   ctx = { win, buf, top, bot, lines, filetype, buftype, gen }
-- (`top`/`bot` are 0-based inclusive buffer rows; `lines` is exactly that slice;
-- `gen` is the viewport generation a publish carries back). `bufs` scopes the
-- provider: `bufs.filetype = { "lua", "rust" }` runs it only in those filetypes,
-- `bufs.buftype = { "quickfix" }` only in those buffer kinds, `bufs.buf = id` only in a
-- specific buffer (constraints AND together); omitted ⇒ every buffer (the engine skips
-- non-matching windows). The provider calls
-- `publish(marks)` with a list of marks shaped like an extmark —
-- `{ row, col, end_row?, end_col?, hl?, priority? }` (row/col may be positional
-- `{ row, col, ... }` or named). v1 renders `hl`; other fields fold into the extmark
-- layer where they are accepted-but-unrendered until that layer grows them.
function nx.decor.provider(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("nx.decor.provider: requires a { name = <string>, on_range = <fn> } table", 2)
  end
  if type(spec.on_range) ~= "function" then
    error("nx.decor.provider('" .. spec.name .. "'): on_range must be a function", 2)
  end
  if spec.bufs ~= nil and type(spec.bufs) ~= "table" then
    error("nx.decor.provider('" .. spec.name .. "'): bufs must be a table", 2)
  end
  -- `debounce = <ms>` (optional): coalesce a fast continuous scroll into one run
  -- (Decision 2). Validated here; defaults to immediate (no debounce) so the common
  -- provider stays instant.
  if spec.debounce ~= nil and type(spec.debounce) ~= "number" then
    error("nx.decor.provider('" .. spec.name .. "'): debounce must be a number (milliseconds)", 2)
  end
  local provider = {
    name = spec.name,
    bufs = spec.bufs,
    debounce = spec.debounce,
    on_range = spec.on_range,
    -- One namespace per provider: a republish clears it and re-sets, wholesale.
    ns = nx.ns.create("nx.decor:" .. spec.name),
  }
  -- Re-registering a name replaces the provider (idempotent setup) rather than
  -- stacking duplicates.
  for i, p in ipairs(nx._decor.providers) do
    if p.name == provider.name then
      nx._decor.providers[i] = provider
      nx._decor_register()
      return
    end
  end
  nx._decor.providers[#nx._decor.providers + 1] = provider
  -- Tell the server a provider exists, so it starts dispatching viewport changes
  -- (it skips the whole off-tick path while none are registered).
  nx._decor_register()
end

-- Whether provider `p` runs for the window described by `ctx` — its `bufs` filter.
-- No `bufs` ⇒ every buffer. Otherwise every present constraint must hold (AND):
--   `bufs.filetype = { "lua", … }`        — the buffer's filetype must be in the list;
--   `bufs.buftype  = { "quickfix", … }`   — the buffer's buftype must be in the list
--                                            (`""` = an ordinary file/scratch buffer,
--                                            `"quickfix"` / `"terminal"` the special kinds);
--   `bufs.buf = id | { id, … }`           — per-buffer opt-in: the buffer id must match.
-- A window failing any present constraint runs no provider (and is never dispatched).
local function decor_matches(p, ctx)
  local bufs = p.bufs
  if not bufs then
    return true
  end
  -- bufs.filetype: the buffer's filetype must be one of the listed languages.
  local fts = bufs.filetype
  if fts then
    local ft = ctx.filetype or ""
    local ok = false
    for _, want in ipairs(fts) do
      if want == ft then
        ok = true
        break
      end
    end
    if not ok then
      return false
    end
  end
  -- bufs.buftype: the buffer's buftype must be one of the listed kinds — `""` (ordinary
  -- file/scratch), `"quickfix"` (quickfix or location-list display), or `"terminal"`.
  local bts = bufs.buftype
  if bts then
    local bt = ctx.buftype or ""
    local ok = false
    for _, want in ipairs(bts) do
      if want == bt then
        ok = true
        break
      end
    end
    if not ok then
      return false
    end
  end
  -- bufs.buf: per-buffer opt-in. A number scopes to one buffer; a list scopes to any
  -- of several. (Buffer ids the provider learns from a `ctx.buf` it cares about — e.g.
  -- an LSP/plugin attaching a decoration to the buffer it just created.)
  local want_buf = bufs.buf
  if want_buf ~= nil then
    if type(want_buf) == "number" then
      if ctx.buf ~= want_buf then
        return false
      end
    elseif type(want_buf) == "table" then
      local ok = false
      for _, id in ipairs(want_buf) do
        if ctx.buf == id then
          ok = true
          break
        end
      end
      if not ok then
        return false
      end
    end
  end
  return true
end

-- Normalize one published mark into the canonical form the extmark layer takes:
-- `row`/`col` may be positional (`{ row, col, ... }`) or named
-- (`{ row = R, col = C }`). A mark without numeric row/col fails loud (no silent skip).
-- v1 renders `hl` only (the spec's hl-only rainbow example); `virt_text`/`sign`/
-- `conceal` are not plumbed yet, so a mark with no `hl` can render nothing — rather
-- than silently no-op (CLAUDE.md: no silent stubs) that mark fails loud here, routing
-- through the provider-error path (Decision 6).
local function normalize_mark(m, name)
  local row = m[1] or m.row
  local col = m[2] or m.col
  if type(row) ~= "number" or type(col) ~= "number" then
    error("nx.decor provider '" .. name .. "': each mark needs a numeric row and col", 0)
  end
  if type(m.hl) ~= "string" then
    error(
      "nx.decor provider '" .. name .. "': each mark needs an `hl` group (v1 renders hl only)",
      0
    )
  end
  -- A decoration range needs both end coordinates to render (the extmark layer skips
  -- a point mark). The flagship shape gives only `end_col` for a same-line span — the
  -- spec's rainbow example does — so default the missing end coordinate to the start:
  -- `end_col` alone ⇒ the range `[col, end_col)` on `row`; `end_row` alone ⇒ `[col, col)`
  -- across rows. A mark with neither stays a point mark (accepted, renders nothing).
  local end_row, end_col = m.end_row, m.end_col
  if end_col ~= nil and end_row == nil then
    end_row = row
  elseif end_row ~= nil and end_col == nil then
    end_col = col
  end
  return {
    row = row,
    col = col,
    end_row = end_row,
    end_col = end_col,
    hl = m.hl,
    priority = m.priority,
  }
end

-- neovim disables a decoration provider after CB_MAX_ERROR consecutive callback
-- errors; nx.decor mirrors that (Decision 7) — a provider that keeps throwing is
-- silenced rather than spamming the message line every scroll.
local MAX_DECOR_ERRORS = 3

-- Run one provider's `on_range` for `ctx`, isolating a throw and tracking consecutive
-- failures. The `publish` closure normalizes the marks and lowers them into the
-- provider's namespace via `nx._decor_publish` (carrying `ctx.gen`, so the server drops
-- a publish from a viewport the user already scrolled past); it also records the latest
-- set on `nx._decor.last` for inspection / tests. An error is surfaced loud
-- (`E5108`-style, Decision 7); after MAX_DECOR_ERRORS consecutive failures the provider
-- is disabled (skipped until re-registered). A clean run resets the counter.
local function run_provider(p, ctx)
  local gen = ctx.gen
  local function publish(marks)
    if type(marks) ~= "table" then
      error("nx.decor provider '" .. p.name .. "': publish expects a list of marks", 0)
    end
    -- Normalize into the extmark-shaped form, then split into the parallel arrays
    -- the funnel takes (one bridge crossing per publish, not one per mark). The
    -- optional fields ride sentinels the Rust side reads back: -1 ⇒ unset for
    -- end_row/end_col/priority, "" ⇒ no hl.
    local out = {}
    local rows, cols, end_rows, end_cols, hls, prios = {}, {}, {}, {}, {}, {}
    for i = 1, #marks do
      local m = normalize_mark(marks[i], p.name)
      out[i] = m
      rows[i] = m.row
      cols[i] = m.col
      end_rows[i] = m.end_row or -1
      end_cols[i] = m.end_col or -1
      hls[i] = m.hl or ""
      prios[i] = m.priority or -1
    end
    nx._decor.last = { name = p.name, gen = gen, marks = out }
    nx._decor_publish(p.ns, gen, ctx.win, ctx.buf, rows, cols, end_rows, end_cols, hls, prios)
  end
  local ok, err = pcall(p.on_range, ctx, publish)
  if ok then
    p.errfails = 0
    return
  end
  p.errfails = (p.errfails or 0) + 1
  nx.notify("E5108: Error in nx.decor provider '" .. p.name .. "': " .. tostring(err), "error")
  if p.errfails >= MAX_DECOR_ERRORS then
    p.disabled = true
    nx.notify(
      "nx.decor: provider '"
        .. p.name
        .. "' disabled after "
        .. MAX_DECOR_ERRORS
        .. " consecutive errors",
      "error"
    )
  end
end

-- nx._decor_dispatch(ctx): the server calls this off-tick, once per visible-range
-- change, with ctx = { win, buf, top, bot, lines, filetype, buftype, gen }. Runs each matching,
-- non-disabled provider's `on_range` (via `run_provider`). A provider with a `debounce`
-- coalesces a fast continuous scroll into one run: each viewport change (re-)arms a
-- per-window trailing debounce, so `on_range` fires once the window stops moving for
-- `debounce` ms with the latest `ctx` (whose `gen` is still live when it fires). The
-- per-window coalescing in core already collapses changes between two drains; this adds
-- the time delay across a continuous gesture (Decision 2).
function nx._decor_dispatch(ctx)
  for _, p in ipairs(nx._decor.providers) do
    if not p.disabled and decor_matches(p, ctx) then
      if p.debounce and p.debounce > 0 then
        p._deb = p._deb or {}
        local d = p._deb[ctx.win]
        if not d then
          d = nx.utils.debounce(function(c)
            run_provider(p, c)
          end, p.debounce)
          p._deb[ctx.win] = d
        end
        d(ctx)
      else
        run_provider(p, ctx)
      end
    end
  end
end
