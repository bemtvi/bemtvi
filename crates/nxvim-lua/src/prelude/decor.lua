-- nx.decor: viewport-scoped decoration providers (docs/specs/2026-06-11-native-plugin-api.md
-- §6; docs/plans/2026-06-15-nx-decor-viewport-decorations.md). A provider is woken
-- ONCE per visible-range change (scroll / resize / edit reflow), OFF the frame,
-- handed a snapshot of the visible slice, and PUBLISHES marks carrying a generation
-- token; a publish from a viewport the user has scrolled past is dropped. There is no
-- per-row / per-frame callback (ADR 0002 rule 4 forbids frame-time Lua) — the engine
-- re-enters Lua only when the viewport actually moves, which is why expensive,
-- on-screen-only decorations (rainbow parens, indent guides, inline blame) fit here.
--
-- Phase 2 (this file): the provider registry + the off-tick dispatch + the `ctx`
-- snapshot. The server drains the per-window "viewport changed" signal (core
-- `editor/decor.rs`), builds `ctx`, and calls `nx._decor_dispatch(ctx)`; each matching
-- provider's `on_range(ctx, publish)` runs. `publish` normalizes + records the marks;
-- lowering them into the extmark layer so they RENDER (gen-gated) is Phase 3.

nx.decor = nx.decor or {}

-- nx._decor holds the registered providers (and, in Phase 2, the last published marks
-- for inspection / tests). Each provider: { name, bufs, on_range, ns }.
nx._decor = nx._decor or { providers = {} }

-- nx.decor.provider { name, bufs?, on_range }: register a viewport decoration
-- provider. `on_range(ctx, publish)` is called off the frame, once per visible-range
-- change of a matching window, with a snapshot
--   ctx = { win, buf, top, bot, lines, filetype, gen }
-- (`top`/`bot` are 0-based inclusive buffer rows; `lines` is exactly that slice;
-- `gen` is the viewport generation a publish carries back). `bufs` scopes the
-- provider: `bufs.filetype = { "lua", "rust" }` runs it only in those filetypes;
-- omitted ⇒ every buffer (the engine skips non-matching windows). The provider calls
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
  local provider = {
    name = spec.name,
    bufs = spec.bufs,
    on_range = spec.on_range,
    -- One namespace per provider: a republish clears it and re-sets, wholesale
    -- (Phase 3). Allocated now (cheap, Lua-side) so the dispatch path is ready.
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
-- No `bufs` ⇒ every buffer; `bufs.filetype` ⇒ the buffer's filetype must be listed.
local function decor_matches(p, ctx)
  local bufs = p.bufs
  if not bufs then
    return true
  end
  local fts = bufs.filetype
  if fts then
    local ft = ctx.filetype or ""
    for _, want in ipairs(fts) do
      if want == ft then
        return true
      end
    end
    return false
  end
  return true
end

-- Normalize one published mark into the canonical form the extmark layer takes
-- (Phase 3 lowers it): `row`/`col` may be positional (`{ row, col, ... }`) or named
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

-- nx._decor_dispatch(ctx): the server calls this off-tick, once per visible-range
-- change, with ctx = { win, buf, top, bot, lines, filetype, gen }. Runs each matching
-- provider's on_range(ctx, publish), isolating a throwing provider (surfaced, never
-- wedges the dispatch). `publish(marks)` normalizes the marks and lowers them into the
-- provider's namespace via `nx._decor_publish` (carrying `ctx.gen`, so the server drops
-- a publish from a viewport the user already scrolled past). It also records the latest
-- set on `nx._decor.last` for inspection / tests.
function nx._decor_dispatch(ctx)
  for _, p in ipairs(nx._decor.providers) do
    if decor_matches(p, ctx) then
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
      if not ok then
        nx.notify("nx.decor: provider '" .. p.name .. "' error: " .. tostring(err), "error")
      end
    end
  end
end
