-- nx.picker: the native fuzzy finder over the unified float-list widget
-- (docs/specs/2026-06-14-nx-ui-float-widget.md, Phase 2). A picker is a centered
-- float with a prompt that grabs input; the server owns the prompt, the Rust
-- fuzzy matcher, navigation, and the generation token, so Lua only ever sees
-- "open", "run the source for this query" and "confirm". Sources are thin Lua
-- drivers: they stream candidates in via `ctx.push`, and handle `confirm(item)`.
--
-- The full item tables stay Lua-side (`nx._picker.active.items`); only a display
-- label + an integer key cross the bridge, exactly like nx.ui.select — so an
-- item's arbitrary fields (path/row/col) never need to serialize.

nx.picker = nx.picker or {}
nx.picker._sources = nx.picker._sources or {}

-- nx._picker holds the *active* picker: the running source, its per-generation
-- item array (keyed by the integer `key` pushed to the widget), the live
-- generation, and the current run's `on_cancel` (a superseded dynamic query runs
-- it to reap its job).
nx._picker = nx._picker or nil

-- The default debounce (ms) before a DYNAMIC source re-runs on a query edit — the
-- global knob. Override it here (`nx.picker.debounce = 400`), per source
-- (`debounce = N` on the source), or per open (`nx.picker.open(name, {debounce=N})`);
-- the more specific wins. `0` disables the debounce (re-run on every keystroke).
nx.picker.debounce = nx.picker.debounce or 250

-- ----- rebindable picker keys -----------------------------------------------
-- Every picker key is an ordinary `picker`-mode keymap, NOT a hardcoded grab: the
-- server selects the `picker` bucket while a picker owns input, so navigation /
-- confirm / cancel / preview-scroll / query-edit are all configurable with
-- `nx.keymap.set('picker', '<key>', nx.picker.actions.<name>)` exactly like any
-- other mode. `nx.picker.actions.<name>` fires the named action through the keymap
-- engine (nx._picker_action -> Editor::apply_picker_action). The only key NOT a map
-- is an arbitrary printable char — there is no way to enumerate every char, so an
-- unmapped printable simply inserts into the query (the picker's text fallthrough).
nx.picker.actions = nx.picker.actions or {}
for _, name in ipairs({
  "next",
  "prev",
  "confirm",
  "cancel",
  "preview_half_down",
  "preview_half_up",
  "preview_page_down",
  "preview_page_up",
  "backspace",
  "delete",
  "left",
  "right",
  "to_start",
  "to_end",
}) do
  nx.picker.actions[name] = function()
    nx._picker_action(name)
  end
end

-- The default picker bindings — `default = true` so a user `nx.keymap.set('picker',
-- …)` for the same key wins by the precedence ladder; binding a key to an empty
-- function (`nx.keymap.set('picker', '<C-n>', function() end)`) disables it. These
-- mirror the keys the picker used to hardcode.
for _, m in ipairs({
  { "<C-n>", "next", "Next item" },
  { "<Down>", "next", "Next item" },
  { "<C-p>", "prev", "Previous item" },
  { "<Up>", "prev", "Previous item" },
  { "<CR>", "confirm", "Confirm selection" },
  { "<Esc>", "cancel", "Cancel" },
  { "<C-d>", "preview_half_down", "Preview half-page down" },
  { "<C-u>", "preview_half_up", "Preview half-page up" },
  { "<C-f>", "preview_page_down", "Preview page down" },
  { "<C-b>", "preview_page_up", "Preview page up" },
  { "<BS>", "backspace", "Delete char before cursor" },
  { "<Del>", "delete", "Delete char under cursor" },
  { "<Left>", "left", "Cursor left" },
  { "<Right>", "right", "Cursor right" },
  { "<Home>", "to_start", "Cursor to start" },
  { "<End>", "to_end", "Cursor to end" },
}) do
  nx.keymap.set("picker", m[1], nx.picker.actions[m[2]], { default = true, desc = m[3] })
end

-- nx.picker.source { name, items = function(ctx), dynamic, confirm }: register a
-- source. `items` streams candidates: it calls `ctx.push(item)` per result (an
-- item is a table with a `text` display field, plus any data the `confirm` needs)
-- and signals completion by *returning* — a synchronous source just returns when
-- its loop ends, an asynchronous one is wrapped in `nx.async` and returns the
-- promise (the engine awaits it; nx is promise-only, so there is no `done`
-- callback). A streaming source consumes a `nx.run_stream` with `nx.await_each`,
-- and reaps its job on close via `ctx.on_cancel`. `dynamic = true` re-runs `items`
-- on every prompt edit (live grep — the matcher is bypassed); the default is a
-- static source matched locally in Rust as you type. `confirm(item)` acts on the
-- chosen item. Optional `width` / `height` fix the box size — a cell count
-- (number) or a CSS-style viewport fraction string ("80vw" / "60vh" / "50%");
-- omitted ⇒ the default (~80vw x 60vh). The picker is never content-sized.
-- Optional `prompt_pos` = "top" (default) or "bottom" places the input above or
-- below the results list.
--
-- For a `dynamic` source (which spawns per query), `debounce` (ms) sets the
-- trailing delay before a query edit re-runs the source — so a fast typist spawns
-- one search per pause, not one per keystroke, and a new keystroke cancels the
-- in-flight search. It defaults to the global `nx.picker.debounce` (250), and is
-- also overridable per open via `nx.picker.open(name, { debounce = N })`; `0`
-- disables it. While that search runs, the PREVIOUS results stay
-- on screen (the list never flashes empty); they are swapped out only when the new
-- search's first result arrives, or cleared if it matched nothing. The widget
-- windows its rendering and matches incrementally, so a source can stream 100k+
-- candidates and stay fast; `max_results` (default 100000) is only a runaway-source
-- safety bound.
function nx.picker.source(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("nx.picker.source: requires a { name = <string>, items = <fn> } table", 2)
  end
  if type(spec.items) ~= "function" then
    error("nx.picker.source('" .. spec.name .. "'): items must be a function", 2)
  end
  if spec.preview ~= nil and spec.preview ~= "file" and spec.preview ~= "location" then
    error("nx.picker.source('" .. spec.name .. '\'): preview must be "file" or "location"', 2)
  end
  nx.picker._sources[spec.name] = spec
end

-- Normalize a size value for the bridge: a number is a cell count, a string is a
-- raw spec ("80vw" / "60vh" / "50%" — a CSS-style viewport fraction), nil falls
-- back to the picker default. Returns a string (or nil).
local function size_str(v)
  if v == nil then
    return nil
  elseif type(v) == "number" then
    return tostring(v)
  else
    return tostring(v)
  end
end

-- nx.picker.open(name[, opts]): open the picker for the registered source `name`.
-- `opts.width` / `opts.height` set a FIXED box size — a cell count (e.g. 100) or a
-- CSS-style viewport fraction string ("80vw" / "60vh" / "50%") — overriding the
-- source's own `width`/`height`, which override the picker default. The picker is
-- never content-sized (a content-hugging box looks ragged). `opts.prompt_pos`
-- ("top" / "bottom") likewise overrides the source's `prompt_pos`.
function nx.picker.open(name, opts)
  local source = nx.picker._sources[name]
  if not source then
    error("nx.picker.open: no such source '" .. tostring(name) .. "'", 2)
  end
  opts = opts or {}
  -- Resolve the preview kind: per-open overrides per-source. nil ⇒ no preview pane.
  local preview = opts.preview
  if preview == nil then
    preview = source.preview
  end
  if preview ~= nil and preview ~= "file" and preview ~= "location" then
    error('nx.picker.open: preview must be "file" or "location"', 2)
  end
  nx._picker = { source = source, items = {}, gen = 0, on_cancel = nil, preview = preview }
  local width = size_str(opts.width ~= nil and opts.width or source.width)
  local height = size_str(opts.height ~= nil and opts.height or source.height)
  -- Resolve the debounce: per-open overrides per-source overrides the global
  -- default. `0` is a valid (truthy) value — disable — so test for `nil`, not `or`.
  local debounce = opts.debounce
  if debounce == nil then
    debounce = source.debounce
  end
  if debounce == nil then
    debounce = nx.picker.debounce
  end
  nx._picker.debounce_ms = debounce or 250
  -- Prompt position: per-open overrides per-source overrides the default ("top").
  -- "bottom" puts the input under the results (telescope-style); anything else is
  -- top. Resolved to a bool for the bridge.
  local prompt_pos = opts.prompt_pos
  if prompt_pos == nil then
    prompt_pos = source.prompt_pos
  end
  local prompt_bottom = prompt_pos == "bottom"
  -- The server opens the centered widget and kicks the initial run (gen 0, "").
  nx._picker_open(source.dynamic == true, width, height, prompt_bottom, preview ~= nil)
end

-- Cap on streamed results past which the job is reaped — a *safety* bound against
-- a runaway/infinite source, not a UI limit: the widget windows its projection and
-- matches incrementally, so 100k candidates render and filter fast. Overridable per
-- source (`max_results = N`). `FLUSH_N` batches the Lua→server crossings.
local MAX_RESULTS = 100000
local FLUSH_N = 1000

-- Reap the active picker's in-flight job (a live-grep `rg`) and cancel a pending
-- debounce, so a new keystroke — or closing the picker — stops the current search.
local function picker_cancel_inflight(p)
  if p.on_cancel then
    local cancel = p.on_cancel
    p.on_cancel = nil
    cancel()
  end
  if p.debounce then
    p.debounce:stop()
    p.debounce = nil
  end
end

-- nx._picker_run(gen, query): (re-)run the active source for `query` under `gen`.
-- Called by the server on open (gen 0, "") and on each dynamic query edit. A
-- **dynamic** source is DEBOUNCED — a query edit cancels the in-flight job and any
-- pending run, then schedules the search `debounce` ms later, so a fast typist
-- spawns one process per pause, not one per keystroke. Static / the initial run
-- start immediately (no process churn to debounce).
function nx._picker_run(gen, query)
  local p = nx._picker
  if not p then
    return
  end
  -- A new query: stop whatever the previous one started (job + pending debounce).
  -- NOTE: `p.items` is NOT reset — it is append-only with absolute keys, so a result
  -- still displayed from the previous query stays confirmable while the new search
  -- is in flight (the server swaps the displayed rows only when new ones arrive).
  -- The whole table is freed when the picker closes.
  picker_cancel_inflight(p)
  p.gen = gen

  -- The actual source invocation — deferred behind the debounce for dynamic edits.
  local function start()
    p.debounce = nil
    -- The picker may have closed (or moved on) while the debounce was pending.
    if nx._picker ~= p or p.gen ~= gen then
      return
    end

    local ctx = {
      query = query,
      cwd = vim.fn.getcwd(),
      gen = gen,
      -- A source registers a reaper for its in-flight job; the next run (or close)
      -- invokes it. Only the *current* run of the *active* picker registers — the
      -- identity check (`nx._picker == p`) drops a registration from a run whose
      -- picker has since closed (a new picker reuses generation 0).
      on_cancel = function(fn)
        if nx._picker == p and p.gen == gen then
          p.on_cancel = fn
        end
      end,
    }

    -- Candidates are buffered and crossed to the server in batches (one bridge call
    -- per ~`FLUSH_N` items, not per item) — the key to streaming 100k results fast.
    -- When the picker carries a preview pane, the per-item target travels in parallel
    -- arrays: `paths` (both kinds; "" ⇒ that row has no target) and, for the
    -- "location" kind, 0-based `rows` / `cols`. nil arrays ⇒ no preview (the common
    -- nx.ui.select / preview-less picker path is unchanged).
    local pv = p.preview -- nil | "file" | "location"
    local labels, keys, batched = {}, {}, 0
    local paths = pv and {} or nil
    local rows = pv == "location" and {} or nil
    local cols = pv == "location" and {} or nil
    local pushed = 0 -- this run's result count, for the cap (p.items is session-wide)
    local function flush()
      if batched > 0 then
        nx._picker_push(gen, labels, keys, paths, rows, cols)
        labels, keys, batched = {}, {}, 0
        if paths then
          paths = {}
        end
        if rows then
          rows, cols = {}, {}
        end
      end
    end
    local function push(item)
      -- Drop a push from a run the user has typed past (`p.gen ~= gen`) OR from a
      -- run whose picker has already closed (`nx._picker ~= p`). The identity check
      -- is essential: generation resets to 0 on every open, so a stale gen-0 push
      -- from a closed picker's orphaned job would otherwise collide with a freshly
      -- opened picker (also gen 0).
      if nx._picker ~= p or p.gen ~= gen then
        return
      end
      -- Result cap: a broad query can stream forever — reap the job once this run
      -- has enough (the matched rows the user scrolls are at the top).
      local cap = p.source.max_results or MAX_RESULTS
      if pushed >= cap then
        flush()
        picker_cancel_inflight(p)
        return
      end
      pushed = pushed + 1
      local entry = item
      if type(entry) ~= "table" then
        entry = { text = tostring(entry) }
      end
      -- `p.nitems` is an O(1) running count (no `#p.items` border-search per item),
      -- and the absolute key into the session-wide `p.items`.
      p.nitems = (p.nitems or 0) + 1
      p.items[p.nitems] = entry
      batched = batched + 1
      labels[batched] = entry.text or tostring(entry)
      keys[batched] = p.nitems
      if paths then
        -- "" ⇒ this row has no target (e.g. an unnamed buffer): the pane shows a
        -- "no preview" placeholder, never a silent blank.
        paths[batched] = entry.path or ""
        if rows then
          -- Items are 1-based (vim/rg convention); the widget's loc is 0-based.
          rows[batched] = math.max(0, (entry.row or 1) - 1)
          cols[batched] = math.max(0, (entry.col or 1) - 1)
        end
      end
      if batched >= FLUSH_N then
        flush()
      end
    end
    -- `done()` settles the picker: flush any tail, then a query that matched nothing
    -- clears its now-stale rows (a matched one already swapped them in via flush).
    local function done()
      flush()
      if nx._picker == p and p.gen == gen then
        nx._picker_finish(gen)
      end
    end
    -- The source emits through `ctx.push` (the sink) and signals completion by
    -- *returning* (a promise from nx.async, or nothing for a synchronous source) —
    -- nx is promise-only, so there is no `done` callback passed in.
    ctx.push = push

    -- Drive the source's completion. nx.promise.try unifies a synchronous source
    -- (returns nil ⇒ already done) and an async one (returns a promise that settles
    -- when its coroutine finishes), AND folds a synchronous throw into the same
    -- rejection path: notify on either (`:catch`), then `done()` exactly once
    -- whichever way it goes (`:finally`) — never a wedged picker.
    nx.promise
      .try(p.source.items, ctx)
      :catch(function(err)
        nx.notify("nx.picker: source '" .. p.source.name .. "' error: " .. tostring(err), "error")
      end)
      :finally(done)
  end

  local delay = p.debounce_ms or 0
  if p.source.dynamic and gen > 0 and delay > 0 then
    p.debounce = nx.timer(start, delay) -- trailing debounce; a new edit reschedules
  else
    start()
  end
end

-- nx._picker_result(key): the picker resolved. `key` (an integer) confirms the
-- item under that key for the current generation; `nil` cancels. Either way the
-- active picker is cleared (and a pending job reaped).
function nx._picker_result(key)
  local p = nx._picker
  nx._picker = nil
  if not p then
    return
  end
  -- Reap any in-flight search and cancel a pending debounce on close.
  picker_cancel_inflight(p)
  if key == nil then
    return -- cancelled
  end
  local item = p.items[key]
  if item and p.source.confirm then
    local ok, err = pcall(p.source.confirm, item)
    if not ok then
      nx.notify("nx.picker: confirm error: " .. tostring(err), "error")
    end
  end
end

-- nx.picker.edit(item): the common confirm action — open `item.path`, and if the
-- item carries a 1-based `row` (and optional 1-based `col`, as live_grep's items
-- do), jump the cursor there. Uses the supported `nx._win_set_cursor` bridge: the
-- mutating `vim.api.nvim_*` surface (incl. `nvim_win_set_cursor`) is intentionally
-- nil in Lua (ADR 0002), so a plugin must go through `nx.*` / keystrokes to move
-- the cursor. The `:edit` runs (and loads the buffer) before the queued cursor op
-- is applied, so window 0 already shows the opened file when the cursor moves.
function nx.picker.edit(item)
  vim.cmd("edit " .. item.path)
  if item.row then
    nx._win_set_cursor(0, item.row - 1, math.max(0, (item.col or 1) - 1))
  end
end

-- ----- built-in sources ------------------------------------------------------
-- Shipped defaults exercising the three source shapes; a config can register more.

-- files: a static source — `rg --files` streamed in, fuzzy-matched locally. An
-- nx.async source: iterate the run_stream's batches with nx.await_each, pushing
-- each path; returning ends the run. The stream is reaped on close via on_cancel.
nx.picker.source({
  name = "files",
  preview = "file", -- the preview pane shows the file's head
  items = nx.async(function(ctx)
    local stream =
      nx.run_stream({ cmd = "rg", args = { "--files", "--color=never" }, cwd = ctx.cwd })
    -- Reap the `rg` job when the picker closes, so a confirmed/cancelled picker
    -- doesn't leave a process streaming paths into the void.
    ctx.on_cancel(function()
      stream:kill()
    end)
    for batch in nx.await_each(stream) do
      for _, l in ipairs(batch) do
        if l ~= "" then
          ctx.push({ text = l, path = l })
        end
      end
    end
  end),
  confirm = function(item)
    nx.picker.edit(item)
  end,
})

-- live_grep: a dynamic source — `rg --vimgrep <query>` re-run per prompt edit, the
-- matcher bypassed; the superseded job is reaped via ctx.on_cancel.
nx.picker.source({
  name = "live_grep",
  dynamic = true,
  preview = "location", -- scroll the pane to the match and range-highlight it
  items = nx.async(function(ctx)
    if ctx.query == "" then
      return
    end
    local stream = nx.run_stream({
      cmd = "rg",
      args = { "--vimgrep", "--color=never", "--", ctx.query },
      cwd = ctx.cwd,
    })
    ctx.on_cancel(function()
      stream:kill()
    end)
    for batch in nx.await_each(stream) do
      for _, l in ipairs(batch) do
        -- file:line:col:text
        local file, lnum, col = l:match("^(.-):(%d+):(%d+):")
        if file then
          ctx.push({ text = l, path = file, row = tonumber(lnum), col = tonumber(col) })
        end
      end
    end
  end),
  confirm = function(item)
    nx.picker.edit(item)
  end,
})

-- buffers: a static, in-memory source — every open buffer, no process spawn. A
-- plain synchronous source: it pushes in a loop and returns (no promise needed —
-- returning nil settles the run). Names come from the authoritative buffer mirror
-- (`nx._bufs`); `nx.buf.name` short-circuits the *current* buffer to a separately-
-- tracked field that can lag, so reading the mirror lists every named buffer
-- including the focused one.
nx.picker.source({
  name = "buffers",
  preview = "file", -- preview the buffer's backing file (named buffers only)
  items = function(ctx)
    local bufs = nx._bufs or {}
    for _, b in ipairs(nx.buf.list()) do
      local entry = bufs[b]
      local name = (entry and entry.name) or nx.buf.name(b)
      if name and name ~= "" then
        ctx.push({ text = name, bufnr = b, path = name })
      end
    end
  end,
  confirm = function(item)
    vim.cmd("buffer " .. item.bufnr)
  end,
})
