-- btv.decor: viewport-scoped decoration providers (docs/specs/2026-06-11-native-plugin-api.md
-- §6; docs/plans/2026-06-15-btv-decor-viewport-decorations.md). A provider is woken
-- ONCE per visible-range change (scroll / resize / edit reflow), OFF the frame,
-- handed a snapshot of the visible slice, and PUBLISHES marks carrying a generation
-- token; a publish from a viewport the user has scrolled past is dropped. There is no
-- per-row / per-frame callback (ADR 0002 rule 4 forbids frame-time Lua) — the engine
-- re-enters Lua only when the viewport actually moves, which is why expensive,
-- on-screen-only decorations (rainbow parens, indent guides, inline blame) fit here.
--
-- This file: the provider registry + the off-tick dispatch + the `ctx` snapshot.
-- The server drains the per-window "viewport changed" signal (core
-- `editor/decor.rs`), builds `ctx`, and calls `btv._decor_dispatch(ctx)`; each matching
-- provider's `on_range(ctx, publish)` runs. `publish` normalizes the marks and lowers
-- them into the provider's extmark namespace via `btv._decor_publish`, so they RENDER
-- (gen-gated — a publish from a viewport the user already scrolled past is dropped).
--
-- A published mark carries the SAME option vocabulary as `btv.buf.set_extmark` and is
-- validated + lowered by the same code (`btv._extmark_split_opts`), so the two surfaces
-- cannot drift: highlights, gutter signs, virtual text/lines and line backgrounds all
-- draw from a provider. What `publish` adds over placing marks yourself is the
-- LIFECYCLE — the generation gate that drops a publish from a scrolled-past viewport,
-- and the wholesale clear-and-reset of the provider's namespace on every republish.

btv.decor = btv.decor or {}

-- btv._decor holds the registered providers (and the last published marks, on
-- `btv._decor.last`, for inspection / tests). Each provider: { name, bufs, on_range, ns }.
btv._decor = btv._decor or { providers = {} }

-- `btv.decor.provider { name, bufs?, on_range }`: register a viewport decoration
-- provider. `on_range(ctx, publish)` is called off the frame, once per visible-range
-- change of a matching window, with a snapshot
--   `ctx = { win, buf, top, bot, lines, filetype, buftype, gen }`
-- (`top`/`bot` are 0-based inclusive buffer rows; `lines` is exactly that slice;
-- `gen` is the viewport generation a publish carries back). `bufs` scopes the
-- provider: `bufs.filetype = { "lua", "rust" }` runs it only in those filetypes,
-- `bufs.buftype = { "quickfix" }` only in those buffer kinds, `bufs.buf = id` only in a
-- specific buffer (constraints AND together); omitted ⇒ every buffer (the engine skips
-- non-matching windows). The provider calls
-- `publish(marks)` with a list of marks shaped like an extmark: `row`/`col` (positional
-- `{ row, col, ... }` or named) plus **any option `btv.buf.set_extmark` takes** — they
-- are validated and lowered by the same code, so a provider draws the full decoration
-- vocabulary, not just highlights:
-- ```lua
-- publish {
--   { row, 0, end_col = 4, hl = "Comment" },                   -- a highlight span
--   { row, 0, sign_text = ">>", sign_hl_group = "DiffAdd" },   -- a gutter sign
--   { row, 0, virt_text = { { "  3 days ago", "Comment" } } }, -- inline blame
--   { row, 0, line_hl_group = "DiffChange" },                  -- a line background
-- }
-- ```
-- `hl` is the decor-native shorthand for `hl_group`. A mark that would draw nothing —
-- carrying no `hl_group`, `virt_text`, `virt_lines`, `sign_text`, `line_hl_group` or
-- `line_fill` — fails loud rather than silently painting nothing, and so does a mark
-- with an unknown key.
function btv.decor.provider(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("btv.decor.provider: requires a { name = <string>, on_range = <fn> } table", 2)
  end
  if type(spec.on_range) ~= "function" then
    error("btv.decor.provider('" .. spec.name .. "'): on_range must be a function", 2)
  end
  if spec.bufs ~= nil and type(spec.bufs) ~= "table" then
    error("btv.decor.provider('" .. spec.name .. "'): bufs must be a table", 2)
  end
  -- `debounce = <ms>` (optional): coalesce a fast continuous scroll into one run
  -- (Decision 2). Validated here; defaults to immediate (no debounce) so the common
  -- provider stays instant.
  if spec.debounce ~= nil and type(spec.debounce) ~= "number" then
    error("btv.decor.provider('" .. spec.name .. "'): debounce must be a number (milliseconds)", 2)
  end
  local provider = {
    name = spec.name,
    bufs = spec.bufs,
    debounce = spec.debounce,
    on_range = spec.on_range,
    -- One namespace per provider: a republish clears it and re-sets, wholesale.
    ns = btv.ns.create("btv.decor:" .. spec.name),
  }
  -- Re-registering a name replaces the provider (idempotent setup) rather than
  -- stacking duplicates.
  for i, p in ipairs(btv._decor.providers) do
    if p.name == provider.name then
      btv._decor.providers[i] = provider
      btv._decor_register()
      return
    end
  end
  btv._decor.providers[#btv._decor.providers + 1] = provider
  -- Tell the server a provider exists, so it starts dispatching viewport changes
  -- (it skips the whole off-tick path while none are registered).
  btv._decor_register()
end

-- The option keys `btv.decor.invalidate` accepts. An unknown key fails loud rather
-- than silently widening the scope to "everything" — notably `name`, which reads like
-- a per-provider filter but is not one (an invalidation re-runs every provider matching
-- the window, the same way a scroll does).
local INVALIDATE_KEYS = { buf = true, win = true }

-- `btv.decor.invalidate([opts])`: tell the engine a provider has new content to draw,
-- and re-dispatch it. A provider is normally woken by the viewport signal — scroll,
-- resize, or an edit to the visible slice — so a change in the data it draws *from*
-- (git blame that came back off a promise, an LSP response, a palette or setting the
-- user just changed) would otherwise not repaint until the user happened to move: a
-- rainbow-bracket provider whose colours were swapped keeps painting the old ones
-- until you scroll past them. This is that missing edge — it marks the windows in
-- scope, and the next engine pass re-runs `on_range` there with a fresh snapshot and
-- a fresh generation token.
--
-- The scope (omitted ⇒ every visible window):
-- ```lua
-- btv.decor.invalidate()                  -- every visible window
-- btv.decor.invalidate({ buf = bufnr })   -- every window showing that buffer (0 = current)
-- btv.decor.invalidate({ win = winid })   -- exactly that window (0 = current)
-- ```
-- Pass `buf` for the usual case: a provider's data is per-buffer, and the same buffer
-- can be open in several splits, each of which needs its own viewport re-run. Passing
-- both `buf` and `win` is an error (they are alternative scopes, not a conjunction).
--
-- Scoped to the *window*, not to the provider: every provider matching the window runs
-- again, exactly as it would on a scroll. A publish still in flight from the run this
-- supersedes is dropped by the generation check, so nothing is lost.
--
-- It is a HINT, not a repaint: like everything else a plugin hands the decoration
-- engine, the ask is optimistic and the engine decides when it is served. Repeated
-- asks for the same window coalesce, and each window is served at most ONCE per pass —
-- so a provider that asks to be re-run in response to its own run cannot spin the
-- editor, it just paces to the next pass. Nothing is dropped; an ask stays outstanding
-- until it is served. (Asking from inside your own `on_range` is still pointless work
-- rather than an error: you already hold the `ctx`, so publish what you want drawn
-- from the run you are in.)
--
-- The re-dispatch happens on the current pass, off the frame like every other
-- dispatch — no redraw is forced and no Lua runs at frame time.
function btv.decor.invalidate(opts)
  if opts == nil then
    return btv._decor_invalidate(nil, nil)
  end
  if type(opts) ~= "table" then
    error("btv.decor.invalidate: expects an optional { buf = <n> } or { win = <n> } table", 2)
  end
  for k in pairs(opts) do
    if not INVALIDATE_KEYS[k] then
      error("btv.decor.invalidate: unknown option '" .. tostring(k) .. "' (accepts buf, win)", 2)
    end
  end
  if opts.buf ~= nil and opts.win ~= nil then
    error("btv.decor.invalidate: pass buf OR win, not both (they are alternative scopes)", 2)
  end
  if opts.win ~= nil then
    if type(opts.win) ~= "number" then
      error("btv.decor.invalidate: win must be a window id (0 = the current window)", 2)
    end
    -- `0` is "the current window" throughout the btv API; resolve it here so the scope
    -- the engine gets is always a concrete id.
    local win = opts.win
    if win == 0 then
      win = btv.win.current()
    end
    return btv._decor_invalidate(win, nil)
  end
  if opts.buf ~= nil then
    if type(opts.buf) ~= "number" then
      error("btv.decor.invalidate: buf must be a buffer number (0 = the current buffer)", 2)
    end
    local buf = opts.buf
    if buf == 0 then
      buf = btv.buf.current()
    end
    return btv._decor_invalidate(nil, buf)
  end
  -- An empty table is the same unscoped ask as no argument at all.
  return btv._decor_invalidate(nil, nil)
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

-- The keys that carry a mark's POSITION rather than its decoration — stripped before
-- the rest is handed to the shared extmark opt-splitter.
local MARK_POSITION_KEYS = { [1] = true, [2] = true, row = true, col = true }

-- The decoration keys that actually PAINT. A mark carrying none of them draws
-- nothing, which — rather than silently no-op (CLAUDE.md: no silent stubs) — fails
-- loud through the provider-error path (Decision 6). Kept as an explicit list so a
-- key that is accepted-and-stored but not yet painted (`conceal`, `spell`, …) can't
-- masquerade as a drawn decoration.
local MARK_RENDERS = {
  hl_group = true,
  virt_text = true,
  virt_lines = true,
  sign_text = true,
  line_hl_group = true,
  line_fill = true,
}

-- Normalize one published mark into `{ row, col, opts }`, where `opts` is exactly the
-- option table `btv.buf.set_extmark` takes: `row`/`col` may be positional
-- (`{ row, col, ... }`) or named, and every other key is an extmark decoration option,
-- validated by the SAME splitter the extmark surface uses (`btv._extmark_split_opts`) —
-- so the two surfaces share one vocabulary and an unknown key fails loud here too.
-- `hl` is accepted as the decor-native shorthand for `hl_group`.
local function normalize_mark(m, name)
  local row = m[1] or m.row
  local col = m[2] or m.col
  if type(row) ~= "number" or type(col) ~= "number" then
    error("btv.decor provider '" .. name .. "': each mark needs a numeric row and col", 0)
  end
  -- Everything that isn't the position is an extmark option. `hl` is the established
  -- decor spelling of `hl_group`; carry it over so existing providers keep working.
  local opts = {}
  for k, v in pairs(m) do
    if not MARK_POSITION_KEYS[k] then
      opts[k] = v
    end
  end
  if opts.hl ~= nil then
    if opts.hl_group ~= nil then
      error("btv.decor provider '" .. name .. "': pass `hl` OR `hl_group`, not both", 0)
    end
    opts.hl_group, opts.hl = opts.hl, nil
  end
  -- A decoration range needs both end coordinates to render (the extmark layer skips a
  -- point mark), and the splitter rejects one without the other. The flagship shape
  -- gives only `end_col` for a same-line span — the spec's rainbow example does — so
  -- default the missing end coordinate to the start: `end_col` alone ⇒ the range
  -- `[col, end_col)` on `row`; `end_row` alone ⇒ `[col, col)` across rows.
  if opts.end_col ~= nil and opts.end_row == nil and opts.end_line == nil then
    opts.end_row = row
  elseif (opts.end_row ~= nil or opts.end_line ~= nil) and opts.end_col == nil then
    opts.end_col = col
  end
  -- Validate + split with the extmark surface's own rules. Level 0: inside a provider
  -- there is no useful source position to blame, and the message routes through the
  -- provider-error path anyway.
  local hl_group, end_row, end_col, priority, decoration, right_gravity, end_right_gravity =
    btv._extmark_split_opts(opts, "btv.decor provider '" .. name .. "'", 0)
  -- Only now — with the vocabulary validated, so a typo names ITSELF rather than
  -- reporting as "draws nothing" — reject a mark with nothing to paint.
  local renders = false
  for k in pairs(opts) do
    if MARK_RENDERS[k] then
      renders = true
      break
    end
  end
  if not renders then
    error(
      "btv.decor provider '"
        .. name
        .. "': this mark would draw nothing — give it one of "
        .. "hl_group (or hl), virt_text, virt_lines, sign_text, line_hl_group, line_fill",
      0
    )
  end
  return {
    row = row,
    col = col,
    opts = opts,
    -- The split payload the bridge takes, per mark — the same shape `btv._extmark_set`
    -- is handed for a directly-placed mark.
    wire = {
      row = row,
      col = col,
      end_row = end_row,
      end_col = end_col,
      hl_group = hl_group,
      priority = priority,
      decoration = decoration,
      right_gravity = right_gravity,
      end_right_gravity = end_right_gravity,
    },
  }
end

-- neovim disables a decoration provider after CB_MAX_ERROR consecutive callback
-- errors; btv.decor mirrors that (Decision 7) — a provider that keeps throwing is
-- silenced rather than spamming the message line every scroll.
local MAX_DECOR_ERRORS = 3

-- Run one provider's `on_range` for `ctx`, isolating a throw and tracking consecutive
-- failures. The `publish` closure normalizes the marks and lowers them into the
-- provider's namespace via `btv._decor_publish` (carrying `ctx.gen`, so the server drops
-- a publish from a viewport the user already scrolled past); it also records the latest
-- set on `btv._decor.last` for inspection / tests. An error is surfaced loud
-- (`E5108`-style, Decision 7); after `MAX_DECOR_ERRORS` consecutive failures the provider
-- is disabled (skipped until re-registered). A clean run resets the counter.
local function run_provider(p, ctx)
  local gen = ctx.gen
  local function publish(marks)
    if type(marks) ~= "table" then
      error("btv.decor provider '" .. p.name .. "': publish expects a list of marks", 0)
    end
    -- Normalize each mark into the extmark-shaped form, then hand the whole batch over
    -- in ONE bridge crossing (not one per mark). Each entry carries the same split
    -- payload `btv._extmark_set` takes for a directly-placed mark, so the decoration
    -- vocabulary is shared rather than re-encoded.
    local out, wire = {}, {}
    for i = 1, #marks do
      local m = normalize_mark(marks[i], p.name)
      out[i] = m
      wire[i] = m.wire
    end
    btv._decor.last = { name = p.name, gen = gen, marks = out }
    btv._decor_publish(p.ns, gen, ctx.win, ctx.buf, wire)
  end
  local ok, err = pcall(p.on_range, ctx, publish)
  if ok then
    p.errfails = 0
    return
  end
  p.errfails = (p.errfails or 0) + 1
  btv.notify("E5108: Error in btv.decor provider '" .. p.name .. "': " .. tostring(err), "error")
  if p.errfails >= MAX_DECOR_ERRORS then
    p.disabled = true
    btv.notify(
      "btv.decor: provider '"
        .. p.name
        .. "' disabled after "
        .. MAX_DECOR_ERRORS
        .. " consecutive errors",
      "error"
    )
  end
end

-- `btv._decor_dispatch(ctx)`: the server calls this off-tick, once per visible-range
-- change, with `ctx = { win, buf, top, bot, lines, filetype, buftype, gen }`. Runs each matching,
-- non-disabled provider's `on_range` (via `run_provider`). A provider with a `debounce`
-- coalesces a fast continuous scroll into one run: each viewport change (re-)arms a
-- per-window trailing debounce, so `on_range` fires once the window stops moving for
-- `debounce` ms with the latest `ctx` (whose `gen` is still live when it fires). The
-- per-window coalescing in core already collapses changes between two drains; this adds
-- the time delay across a continuous gesture (Decision 2).
function btv._decor_dispatch(ctx)
  for _, p in ipairs(btv._decor.providers) do
    if not p.disabled and decor_matches(p, ctx) then
      if p.debounce and p.debounce > 0 then
        p._deb = p._deb or {}
        local d = p._deb[ctx.win]
        if not d then
          d = btv.utils.debounce(function(c)
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

-- `btv.decor.expr(src)`: install a **frame-time** paint block over every visible
-- line, or clear it with `nil`. The pure sibling of `btv.decor.provider`.
--
-- `src` is a string of Lua *source* — a function **body**, not a value — because
-- the block runs in the bounded compute sandbox: a second, pure VM with a
-- wall-clock deadline, no editor state and no `btv.*`. A closure cannot cross
-- between VMs, so the source crosses instead and is compiled there. It is a body
-- rather than a single expression because a per-line paint loops over the matches
-- on the line, so it ends in its own `return`.
--
-- Two names are in scope, and the block returns a list of spans to highlight:
--
-- ```
-- line    the text of the line being painted
-- lnum    its 1-based line number
-- ```
--
-- Each span is `{ first, last, group }` — **1-based inclusive** columns, which is
-- what `string.find` hands back, so a match drops straight in:
--
-- ```lua
-- btv.decor.expr([[
--   local out, i = {}, 1
--   while true do
--     local s, e = line:find("TODO", i, true)
--     if not s then break end
--     out[#out + 1] = { s, e, "Todo" }
--     i = e + 1
--   end
--   return out
-- ]])
--
-- btv.decor.expr(nil)   -- stop painting
-- ```
--
-- Returning an empty list (or `nil`) declines the line. A malformed span fails
-- loud rather than silently not painting.
--
-- **A span can draw more than a colour.** Beside the three positional slots it
-- takes named keys, each the scalar form of the extmark key it lowers into:
--
-- ```
-- virt_text    text drawn beside the line
-- virt_hl      its highlight group
-- virt_pos     where it draws: "eol" (the default), "inline", "overlay", "right_align"
-- sign_text    a 1-2 cell glyph in the gutter
-- sign_hl      its highlight group
-- line_hl      a highlight group backing the whole line
-- ```
--
-- ```lua
-- -- a badge after the match, and a gutter mark on its line
-- btv.decor.expr([[
--   local s, e = line:find("FIXME", 1, true)
--   if not s then return {} end
--   return { { s, e, "Todo", virt_text = "  <- fix me", virt_hl = "Comment",
--             sign_text = ">>", sign_hl = "DiagnosticWarn" } }
-- ]])
-- ```
--
-- The columns and the group are each optional *when the span carries a
-- decoration*: a sign or a line background anchors on the line, not on a stretch
-- of it, so `{ sign_text = ">>" }` is a whole span. What is refused is a span that
-- draws **nothing** — no group and no decoration — and a qualifier with nothing to
-- qualify (`virt_hl` without `virt_text`), because both are half-written spans that
-- would otherwise vanish in silence.
--
-- **Which one to use.** `btv.decor.provider` is the general surface: full Lua,
-- async, any editor state, and the whole extmark vocabulary (including virtual
-- *lines* and per-chunk styling, which a span cannot express). It runs *off* the
-- frame, so its marks land on the next one. `btv.decor.expr` can only see the line
-- it was handed — and in exchange it runs *during* the frame, so a paint that is a
-- pure function of the text (indent guides, colour swatches, trailing whitespace, a
-- keyword badge) appears in the same frame as the edit or scroll, with no
-- round trip and no flicker.
--
-- It is evaluated over the visible rows of each window, memoized on the viewport,
-- so a steady screen makes no calls at all. A block that errors, exceeds its
-- deadline, or returns a malformed span reports once and is then uninstalled.
function btv.decor.expr(src)
  if src ~= nil and type(src) ~= "string" then
    error("btv.decor.expr: expected a string of Lua source (or nil), got " .. type(src), 2)
  end
  btv.decor._expr_src = src
  btv._decor_set_expr(src)
end
