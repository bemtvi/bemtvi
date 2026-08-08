-- ~~~ nxvim nx.decor playground: invalidating a provider when its DATA changes ~~~
--
-- A `nx.decor` provider is woken by the VIEWPORT: scroll, resize, or an edit to the
-- visible slice. That covers every decoration derived from the buffer text (see
-- `examples/rainbow/` and `examples/decor-todo/`). It does NOT cover a decoration
-- derived from something else — git blame that comes back off a promise, an LSP
-- response, a palette or setting the user just changed. Think of a rainbow-bracket
-- provider when the colour scheme is swapped: the brackets are still the same
-- brackets, so nothing about the viewport moved, and without a signal the screen keeps
-- showing the old colours until you happen to scroll or type.
--
-- `nx.decor.invalidate` is that signal: "my data changed, run me again". It marks the
-- windows you scope it to, and the engine re-dispatches them (off the frame, with a
-- fresh generation token) exactly as a scroll would.
--
--     nx.decor.invalidate()                  -- every visible window
--     nx.decor.invalidate({ buf = 0 })       -- every window showing this buffer
--     nx.decor.invalidate({ win = 0 })       -- just this window
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/decor-invalidate \
--       cargo run -p nxvim -- examples/decor-invalidate/sample.txt
--
-- Two things to try, below.

--------------------------------------------------------------------------------
-- 1. The provider. It flags lines longer than a LIMIT — but the limit is plugin
--    state, not buffer text, so the viewport signal knows nothing about it.
--    Nothing here is special: an ordinary provider publishing hl-only marks.
--------------------------------------------------------------------------------
nx.hl.define(0, "TooLong", { fg = "#f38ba8", bold = true })

local state = { limit = nil } -- nil until the async "config" lands (section 2)

nx.decor.provider({
  name = "long-lines",
  on_range = function(ctx, publish)
    -- No data yet ⇒ publish nothing. A provider always publishes: an empty list is
    -- how you clear your own marks (a republish replaces the namespace wholesale).
    if not state.limit then
      return publish({})
    end
    local marks = {}
    for i, line in ipairs(ctx.lines) do
      if #line > state.limit then
        -- Flag the overflow: from the limit column to the end of the line.
        marks[#marks + 1] = { ctx.top + i - 1, state.limit, end_col = #line, hl = "TooLong" }
      end
    end
    publish(marks)
  end,
})

--------------------------------------------------------------------------------
-- 2. DATA ARRIVING LATE. The limit is fetched asynchronously (a promise standing in
--    for a git call, an LSP round-trip, a config read). It resolves ~400ms after
--    startup — long after the buffer painted — and `invalidate` is what makes that
--    answer reach the screen.
--
--    SEE THAT: open the sample and sit still. The long lines stay plain for a beat,
--    then light up on their own. No key was pressed. Delete the `nx.decor.invalidate`
--    line below and they never light up at all until you scroll or edit.
--------------------------------------------------------------------------------
nx.promise.delay(400):next(function()
  state.limit = 48
  nx.decor.invalidate({ buf = 0 })
  print("nx.decor: limit arrived (48) — invalidate repainted with no input")
end)

--------------------------------------------------------------------------------
-- 3. A SETTING CHANGING. Same story, user-driven: the command below moves the limit,
--    which changes what should be drawn without touching the buffer or the viewport.
--
--    TYPE THIS:  :Limit 20     SEE THAT: far more of each line is flagged, at once.
--    TYPE THIS:  :Limit 70     SEE THAT: almost nothing is flagged.
--    TYPE THIS:  :Limit 20     then comment out the `invalidate` call and reload —
--                              the screen would not change until you scrolled.
--
--    Scoped to `buf = 0` (this buffer) rather than unscoped: the data is per-buffer,
--    and the scope wakes EVERY window showing it — try `:split` first and watch both
--    halves repaint from the one call.
--------------------------------------------------------------------------------
nx.command("Limit", function(opts)
  local n = tonumber(opts.args)
  if not n then
    return nx.notify("Limit: expects a number, e.g. :Limit 20", "error")
  end
  state.limit = n
  nx.decor.invalidate({ buf = 0 })
end, { nargs = 1, desc = "set the long-line limit and repaint the decor provider" })

--------------------------------------------------------------------------------
-- 4. Worth knowing: the scope is a WINDOW scope, not a per-provider one — every
--    provider matching the window re-runs, exactly as it would on a scroll.
--
--    The ask is a HINT, not a repaint. Like everything else you hand the decoration
--    engine it is optimistic: repeated asks for the same window coalesce, each window
--    is served at most once per pass, and a publish from a superseded run is dropped
--    by the generation check. Nothing is refused and nothing is lost — an ask stays
--    outstanding until it is served. That is also why asking to be re-run from inside
--    your own `on_range` cannot spin the editor; it just paces to the next pass. (It
--    is still pointless: you already hold the `ctx`, so publish what you want drawn
--    from the run you are in.)
--------------------------------------------------------------------------------

vim.o.number = true

print("nx.decor.invalidate: wait ~400ms for the async limit, then try :Limit 20")
