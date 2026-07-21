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
  "confirm_tab",
  "confirm_split",
  "confirm_vsplit",
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
  "send_to_list",
  "toggle_select",
  "clear_select",
}) do
  nx.picker.actions[name] = function()
    nx._picker_action(name)
  end
end

-- The default picker bindings — `default = true` so a user `nx.keymap.set('picker', …)`
-- for the same key wins by the precedence ladder; binding a key to an empty
-- function (`nx.keymap.set('picker', '<C-n>', function() end)`) disables it. These
-- mirror the keys the picker used to hardcode.
for _, m in ipairs({
  { "<C-n>", "next", "Next item" },
  { "<Down>", "next", "Next item" },
  { "<C-p>", "prev", "Previous item" },
  { "<Up>", "prev", "Previous item" },
  { "<CR>", "confirm", "Confirm selection" },
  { "<C-t>", "confirm_tab", "Open in a new tab" },
  { "<C-x>", "confirm_split", "Open in a horizontal split" },
  { "<C-v>", "confirm_vsplit", "Open in a vertical split" },
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
  { "<C-q>", "send_to_list", "Send results to a named list" },
  { "<Tab>", "toggle_select", "Toggle multi-select on this row" },
  { "<S-Tab>", "toggle_select", "Toggle multi-select on this row" },
}) do
  nx.keymap.set("picker", m[1], nx.picker.actions[m[2]], { default = true, desc = m[3] })
end

-- nx.picker.source { name, items = function(ctx), dynamic, confirm, preview }:
-- register a source. `items(ctx)` streams candidates: it calls `ctx.push(item)` per
-- result (an item is a table with a `text` display field, plus any data `confirm` or
-- the preview needs — e.g. `path` / `row` / `col`) and signals completion by
-- *returning* — a synchronous source just returns when
-- its loop ends, an asynchronous one is wrapped in `nx.async` and returns the
-- promise (the engine awaits it; nx is promise-only, so there is no `done`
-- callback). A streaming source consumes a `nx.run_stream` with `nx.await_each`,
-- and reaps its job on close via `ctx.on_cancel`. `dynamic = true` re-runs `items`
-- on every prompt edit (live grep — the matcher is bypassed), reading the live
-- prompt from `ctx.query` and the working directory from `ctx.cwd`; the default is a
-- static source matched locally in Rust as you type. `confirm(item)` acts on the
-- chosen item. Optional `preview` adds a side pane for the highlighted item:
-- `"file"` shows the head of `item.path`, `"location"` shows `item.path` positioned
-- at `item.row` / `item.col` (1-based). Omitted ⇒ no preview pane; per-open
-- overridable via `nx.picker.open(name, { preview = … })`. Optional `width` /
-- `height` fix the box size — a cell count
-- (number) or a CSS-style viewport fraction string (`"80vw"` / `"60vh"` / `"50%"`);
-- omitted ⇒ the default (~80vw x 60vh). The picker is never content-sized.
-- Optional `align` (`"top-left"`…`"center"`…`"bottom-right"`, default centered) +
-- `margin` (a gap from the editor edges: a number — the vertical gap, horizontal
-- sides 2x to look even — or {vertical, horizontal} / {top, right, bottom, left} /
-- {top=, …}) place the box like a float. Optional
-- `prompt_pos` = `"top"` (default) or `"bottom"` places the input above or below the
-- results list.
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
--
-- `resumable = false` opts the source out of `nx.picker.resume()` (`<leader>fr`):
-- opening it never overwrites the resume slot, so a transient internal picker (the
-- cmdline file completer) can't shadow the last user-facing one. Defaults to true.
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
  if spec.layer ~= nil and spec.layer ~= "main" and spec.layer ~= "active" then
    error("nx.picker.source('" .. spec.name .. '\'): layer must be "main" or "active"', 2)
  end
  nx.picker._sources[spec.name] = spec
end

-- nx.picker.open(name[, opts]): open the picker for the registered source `name`.
-- Each `opts` field overrides the matching field on the source (which in turn
-- overrides the picker default):
--   * `width` / `height` — a FIXED box size: a cell count (e.g. 100) or a CSS-style
--     viewport fraction string (`"80vw"` / `"60vh"` / `"50%"`). The picker is never
--     content-sized (a content-hugging box looks ragged).
--   * `align` + `margin` — placement, like a float (see `nx.picker.source`).
--   * `preview` — `"file"` / `"location"` / nil (no pane).
--   * `prompt_pos` — `"top"` (default) / `"bottom"`.
--   * `query` — initial prompt text: the picker opens already filtered against it
--     (the gen-0 run uses it instead of `""`), caret at its end. Default `""`.
--   * `title` — a title centered on the box's top border (e.g. `"Find Files"`); nil
--     ⇒ no title. The shipped sources set their own (`"Find Files"`/`"Live Grep"`/…).
--   * `multiselect` — whether `<Tab>` marks rows for a batch action (default true);
--     `false` is a single-choice picker (no marking).
--   * `debounce` — ms before a `dynamic` source re-runs on a query edit; `0` off.
--   * `layer` — where a confirmed item opens: `"main"` crosses back to the main editor
--     area first (so a file picked while focused in a dock lands in the editor, not
--     the sidebar), `"active"` opens in the focused layer. Defaults to `"active"`; the
--     shipped `files`/`live_grep` sources set `"main"`, `buffers` stays `"active"`.
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
  -- Resolve the confirm target layer: per-open overrides per-source, default "active".
  local layer = opts.layer
  if layer == nil then
    layer = source.layer
  end
  if layer == nil then
    layer = "active"
  end
  if layer ~= "main" and layer ~= "active" then
    error('nx.picker.open: layer must be "main" or "active"', 2)
  end
  nx._picker =
    { source = source, items = {}, gen = 0, on_cancel = nil, preview = preview, layer = layer }
  local width = nx._geom.size(opts.width ~= nil and opts.width or source.width)
  local height = nx._geom.size(opts.height ~= nil and opts.height or source.height)
  -- Placement: `align` (a 9-grid word, default centered) + `margin` (a gap from the
  -- editor edges), each per-open overriding per-source. The picker box used to be
  -- centered-only; now it can sit in any corner with a margin, like a float.
  local align = nx._geom.align(opts.align ~= nil and opts.align or source.align)
  local margin = nx._geom.margin(opts.margin ~= nil and opts.margin or source.margin)
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
  -- The initial prompt text: `nx.picker.open(name, { query = "src/ed" })` opens
  -- the picker already filtered against `query` (the gen-0 run uses it instead of
  -- ""), with the caret at its end so the user keeps typing. Defaults to "" — the
  -- historical empty-prompt open.
  local query = opts.query
  if query == nil then
    query = ""
  end
  if type(query) ~= "string" then
    error("nx.picker.open: query must be a string", 2)
  end
  -- An optional title for the picker box's top border (`title = "Select file"`).
  -- per-open overrides per-source; nil ⇒ no title.
  local title = opts.title
  if title == nil then
    title = source.title
  end
  if title ~= nil and type(title) ~= "string" then
    error("nx.picker.open: title must be a string", 2)
  end
  -- Whether `<Tab>` multi-selects (marks) rows; per-open overrides per-source,
  -- default true. `false` is a single-choice picker (e.g. the cmdline file completer).
  local multiselect = opts.multiselect
  if multiselect == nil then
    multiselect = source.multiselect
  end
  if multiselect == nil then
    multiselect = true
  end
  -- The resume slot (`nx.picker.resume` / `<leader>fr`). The reopen replays a frozen
  -- snapshot the *server* holds (the displayed rows, cursor, marks, query) — a
  -- live-grep order isn't reproducible, so we never re-run the source. Lua's only job
  -- is to keep `nx._picker` (the source + the window's item tables) alive for
  -- `confirm` and future query edits; `nx._picker_save_resume` fills `last.picker` at
  -- close. Linked onto the active picker (`_last`) so the close handler updates the
  -- right record.
  --
  -- A source can opt out with `resumable = false` (the cmdline file completer does):
  -- it is a transient internal picker whose confirm acts on the open command line, so
  -- replaying it standalone from `<leader>fr` makes no sense. Such a picker leaves the
  -- slot pointing at the last *real* picker (server-side too — see `resumable`), so
  -- resume skips it.
  if source.resumable ~= false then
    local last = { name = name }
    nx.picker._last = last
    nx._picker._last = last
  end
  -- The server opens the aligned widget and kicks the initial run (gen 0, query);
  -- `resumable` tells it whether to snapshot this picker when it closes.
  nx._picker_open(
    source.dynamic == true,
    width,
    height,
    align,
    margin,
    prompt_bottom,
    preview ~= nil,
    query,
    title,
    multiselect == true,
    source.resumable ~= false
  )
end

-- nx.picker.resume(): reopen the most-recently-closed picker (telescope's `resume`),
-- restored to exactly where the user left off — the displayed rows, prompt text,
-- highlighted row, and multi-select marks. The server replays a frozen snapshot it
-- captured at close (bounded to a window around the cursor), so a live-grep picker
-- comes back with its *actual* previous results, not a fresh (differently-ordered)
-- search. Re-installs `nx._picker` so `confirm` works and a later query edit re-runs
-- the source. No-op (a gentle notice) before any resumable picker has closed.
function nx.picker.resume()
  local last = nx.picker._last
  if not (last and last.picker) then
    nx.notify("nx.picker: no picker to resume", "info")
    return
  end
  -- Re-arm the Lua-side runtime (a fresh shallow copy each resume so re-closing
  -- snapshots cleanly), then let the server replay its frozen menu.
  local saved = last.picker
  nx._picker = {
    source = saved.source,
    items = saved.items,
    preview = saved.preview,
    layer = saved.layer,
    gen = saved.gen,
    nitems = saved.nitems,
    debounce_ms = saved.debounce_ms,
    _last = last,
  }
  nx._picker_resume()
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
-- Called by the server on open (gen 0, `""`) and on each dynamic query edit. A
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

-- Save a closing picker's runtime onto its `_last` slot so `nx.picker.resume()` can
-- re-arm it: the source (for `confirm` + future query edits) and the item tables for
-- the snapshot window the server kept (`resume_keys`, in display order). Trimming
-- `items` to the window bounds retained memory the same way the server bounds its
-- snapshot. Tied to the *closing* picker's own `_last` (`p._last`) so a stale close
-- never clobbers a newer picker's slot; a `resumable = false` source has no `_last`.
function nx._picker_save_resume(p, resume_keys)
  if not (p and p._last and resume_keys) then
    return
  end
  local items = {}
  for _, key in ipairs(resume_keys) do
    items[key] = p.items[key]
  end
  p._last.picker = {
    source = p.source,
    items = items,
    preview = p.preview,
    layer = p.layer,
    gen = p.gen,
    nitems = p.nitems,
    debounce_ms = p.debounce_ms,
  }
end

-- nx._picker_result(key): the picker resolved. `key` (an integer) confirms the
-- item under that key for the current generation; `nil` cancels. Either way the
-- active picker is cleared (and a pending job reaped).
-- `mode` is the confirm gesture's open mode — `"current"` (the focused window) or
-- `"tab"` (the default `<C-t>` ⇒ a new tab) — forwarded to
-- `source.confirm(item, mode, layer)`. `layer` is the resolved confirm target
-- (`"main"`/`"active"`); built-in sources honor both (see `nx.picker.edit`).
-- `resume_keys` (the snapshot window's item keys) lets `nx.picker.resume()` re-arm this
-- picker — see `nx._picker_save_resume`.
function nx._picker_result(key, mode, resume_keys)
  local p = nx._picker
  nx._picker = nil
  nx._picker_save_resume(p, resume_keys)
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
    local ok, err = pcall(p.source.confirm, item, mode, p.layer)
    if not ok then
      nx.notify("nx.picker: confirm error: " .. tostring(err), "error")
    end
  end
end

-- A picker item -> a location-list entry. Only items carrying a `path` make sense
-- in a list; `row`/`col` are 1-based (the item convention), defaulting to the file
-- head. `text` is the item's display label (e.g. live_grep's `file:line:col:text`).
local function picker_item_to_qf(item)
  return {
    filename = item.path,
    lnum = item.row or 1,
    col = item.col or 1,
    text = item.text or "",
  }
end

-- nx._picker_send(keys, resume_keys, query): the "send results to a list" outcome
-- (the picker's `<C-q>`). `keys` are the matched item keys in display order — the
-- *filtered* result set the server captured before closing the picker; `query` is the
-- live prompt text. Map the keys back to their source item tables, keep the ones with
-- a target file, and stash them in a **named list** keyed `<picker>:<query>` — so each
-- distinct search is its own persistent dock tab (re-running the same search updates
-- it in place), independent of the global quickfix and of any window. Deferred with
-- `nx.schedule` so the picker float has closed and focus is back in the main layer
-- before the tab opens.
function nx._picker_send(keys, resume_keys, query)
  local p = nx._picker
  nx._picker = nil
  -- Keep the resume slot current even when the picker closed via a send.
  nx._picker_save_resume(p, resume_keys)
  if not p then
    return
  end
  picker_cancel_inflight(p)
  local items = {}
  for _, key in ipairs(keys) do
    local it = p.items[key]
    if it and it.path and it.path ~= "" then
      items[#items + 1] = picker_item_to_qf(it)
    end
  end
  local picker_name = (p.source and p.source.name) or "picker"
  local name = picker_name .. ":" .. (query or "")
  nx.schedule(function()
    nx.qf.list(name, items, { title = name })
    nx.qf.show(name)
  end)
end

-- nx.picker.edit(item): the common confirm action — open `item.path`, and if the
-- item carries a 1-based `row` (and optional 1-based `col`, as live_grep / LSP
-- location items do), jump the cursor there.
--
-- A *located* jump (`item.row` set) goes through the `nx._jump_to` bridge, NOT
-- `:edit`: a jump must navigate, never reload. `:edit`-ing the location would (a)
-- error with E37 when the target is the *current* modified buffer (the LSP hands
-- back an absolute path, but the open buffer may be relatively named) and (b)
-- strand a duplicate buffer when that absolute path doesn't string-match the
-- relative one. `nx._jump_to` reuses the open buffer cwd-aware and skips the
-- modified guard, so selecting a symbol in the file you're editing just moves the
-- cursor. A location-less item (the `files` source) is a plain open instead.
-- `mode` is the confirm gesture: `"tab"`/`"split"`/`"vsplit"` (`<C-t>`/`<C-x>`/`<C-v>`)
-- open in a NEW tab / split regardless of `'switchbuf'` (an explicit gesture);
-- `"current"` (or nil) honors `'switchbuf'`.
--
-- `layer` is the confirm target the picker resolved (`"main"`/`"active"`), forwarded to
-- `confirm` and on to here. `"main"` crosses back to the main editor layer before
-- opening, so a file picked while focused in a dock lands in the editor rather than
-- the sidebar; `"active"` (or nil) opens in the focused layer.
function nx.picker.edit(item, mode, layer)
  local col = math.max(0, (item.col or 1) - 1)
  local to_main = layer == "main"
  if mode == "tab" or mode == "split" or mode == "vsplit" then
    -- A fresh tab / split for the file; located items land the cursor, plain opens
    -- start at the top.
    nx._jump_to(item.path, item.row and (item.row - 1) or 0, item.row and col or 0, mode, to_main)
  elseif item.row then
    nx._jump_to(item.path, item.row - 1, col, nil, to_main)
  else
    -- Open honoring 'switchbuf' (a file already shown in another tab is focused
    -- there under the default `usetab`), not a plain `:edit` into this window.
    -- Distinct bridge from nx.open's `nx._open` (the `:edit`-like layer open).
    nx._open_switchbuf(item.path, to_main)
  end
end

-- ----- built-in sources ------------------------------------------------------
-- Shipped defaults exercising the three source shapes; a config can register more.

-- files: a static source — file paths streamed in, fuzzy-matched locally. An
-- nx.async source: iterate the run_stream's batches with nx.await_each, pushing each
-- path; returning ends the run. The stream is reaped on close via on_cancel.
--
-- Enumeration falls back through a chain so the picker lists files in every mode:
-- `rg --files` (fast, .gitignore-aware) → `find` (any real shell lacking rg) → a
-- transport-agnostic `nx.fs` walk. The binaries need a real shell, so the pure web
-- client — where a hostless spawn fails loud with code -1 — lands on the nx.fs walk
-- (which rides the off-tick seam to OPFS / the daemon). Each step runs only when the
-- previous produced nothing.
nx.picker.source({
  name = "files",
  title = "Find Files",
  layer = "main", -- a picked file opens in the main editor, never a focused dock
  preview = "file", -- the preview pane shows the file's head
  items = nx.async(function(ctx)
    -- Stream a listing command's stdout as candidates; returns whether any landed.
    -- `strip` removes `find`'s leading "./" so its paths match rg's relative style.
    local function run(cmd, args, strip)
      local pushed = false
      local stream = nx.run_stream({ cmd = cmd, args = args, cwd = ctx.cwd })
      -- Reap the job when the picker closes, so a confirmed/cancelled picker doesn't
      -- leave a process streaming paths into the void. Sequential steps each arm this
      -- for the only stream that can still be running.
      ctx.on_cancel(function()
        stream:kill()
      end)
      for batch in nx.await_each(stream) do
        for _, l in ipairs(batch) do
          if strip then
            l = l:gsub("^%./", "")
          end
          if l ~= "" then
            pushed = true
            ctx.push({ text = l, path = l })
          end
        end
      end
      return pushed
    end
    if run("rg", { "--files", "--color=never" }) then
      return
    end
    if run("find", { ".", "-type", "f", "-not", "-path", "*/.git/*" }, true) then
      return
    end
    -- No shell / no rg / no find (the pure web client): walk the tree over nx.fs.
    local ok, files = pcall(nx.await, nx.fs.walk(ctx.cwd))
    if ok then
      for _, f in ipairs(files) do
        ctx.push({ text = f, path = f })
      end
    end
  end),
  confirm = function(item, mode, layer)
    nx.picker.edit(item, mode, layer)
  end,
})

-- live_grep: a dynamic source — re-run per prompt edit, the matcher bypassed. Search
-- falls back through a chain so it works in every mode: `rg --vimgrep` (fast,
-- .gitignore-aware) → `grep -rn` (any real shell lacking rg) → a transport-agnostic
-- nx.fs walk + in-Lua substring match. The binaries need a real shell, so the pure web
-- client — where a hostless spawn fails loud with code -1 — lands on the nx.fs match
-- (which rides the off-tick seam to OPFS). Each step runs only when the previous found
-- nothing; the superseded job is reaped via ctx.on_cancel.
nx.picker.source({
  name = "live_grep",
  title = "Live Grep",
  layer = "main", -- a grep hit opens in the main editor, never a focused dock
  dynamic = true,
  preview = "location", -- scroll the pane to the match and range-highlight it
  items = nx.async(function(ctx)
    if ctx.query == "" then
      return
    end
    local q = ctx.query

    -- Stream a grep-like command, parsing `file:lnum[:col]:text` per line; `has_col` for
    -- rg's `--vimgrep` column. `strip` drops grep's leading "./". Returns whether any
    -- match landed.
    local function run(cmd, args, has_col, strip)
      local pushed = false
      local stream = nx.run_stream({ cmd = cmd, args = args, cwd = ctx.cwd })
      ctx.on_cancel(function()
        stream:kill()
      end)
      for batch in nx.await_each(stream) do
        for _, l in ipairs(batch) do
          if strip then
            l = l:gsub("^%./", "")
          end
          local file, lnum, col
          if has_col then
            file, lnum, col = l:match("^(.-):(%d+):(%d+):")
          else
            file, lnum = l:match("^(.-):(%d+):")
            col = 1
          end
          if file then
            pushed = true
            ctx.push({ text = l, path = file, row = tonumber(lnum), col = tonumber(col) })
          end
        end
      end
      return pushed
    end

    if run("rg", { "--vimgrep", "--color=never", "--", q }, true, false) then
      return
    end
    if run("grep", { "-rnI", "--exclude-dir=.git", "--", q, "." }, false, true) then
      return
    end

    -- No shell / no rg / no grep (the pure web client): a transport-agnostic nx.fs walk
    -- + in-Lua substring match. Performance isn't a concern — the pure-client trees are
    -- small and the picker caps results; pushes for a superseded query are dropped by the
    -- sink.
    local ok, matches = pcall(nx.await, nx.fs.grep(ctx.cwd, q))
    if ok then
      for _, m in ipairs(matches) do
        ctx.push({
          text = m.path .. ":" .. m.row .. ":" .. m.text,
          path = m.path,
          row = m.row,
          col = m.col,
        })
      end
    end
  end),
  confirm = function(item, mode, layer)
    nx.picker.edit(item, mode, layer)
  end,
})

-- buffers: a static, in-memory source — the focused layer's open buffers, no
-- process spawn. A plain synchronous source: it pushes in a loop and returns (no
-- promise needed — returning nil settles the run). Scoped to the focused layer
-- with `{ focused = true }`, exactly like `:ls`: the main area and each dock keep
-- disjoint buffer lists, so picking a buffer never yanks a document into a dock
-- (or vice versa). Names come from the authoritative buffer mirror (`nx._bufs`);
-- `nx.buf.name` short-circuits the *current* buffer to a separately-tracked field
-- that can lag, so reading the mirror lists every named buffer including the
-- focused one.
nx.picker.source({
  name = "buffers",
  title = "Buffers",
  layer = "active", -- scoped to the focused layer, so a pick stays in that layer
  preview = "file", -- preview the buffer's backing file (named buffers only)
  items = function(ctx)
    local bufs = nx._bufs or {}
    for _, b in ipairs(nx.buf.list({ focused = true })) do
      local entry = bufs[b]
      local name = (entry and entry.name) or nx.buf.name(b)
      if name and name ~= "" then
        ctx.push({ text = name, bufnr = b, path = name })
      end
    end
  end,
  confirm = function(item, mode, layer)
    -- The mode rides the bridge: "current" honors 'switchbuf' (a buffer shown in
    -- another tab is focused there under the default `usetab`); "tab"/"split"/
    -- "vsplit" (`<C-t>`/`<C-x>`/`<C-v>`) open it in a new tab / split regardless.
    -- `layer == "main"` would cross out of a dock first; this source is "active", so
    -- a pick stays in the focused layer — but an override is honored for symmetry.
    nx._buf_switch(item.bufnr, mode, layer == "main")
  end,
})

-- curbuf: fuzzy-find a line in the *current* buffer (telescope's
-- `current_buffer_fuzzy_find`). A static, in-memory source over the focused
-- buffer's lines (read from the `nx.buf` mirror, never live state); each item
-- carries its 1-based `row` and confirm just moves the cursor there — no path, so
-- it works for an unnamed buffer too, and there is no preview pane (the line is
-- already on screen). Read `nx.buf.current()` at open, before the picker float
-- grabs input, so it is the underlying buffer, not the prompt.
nx.picker.source({
  name = "curbuf",
  title = "Buffer Lines",
  layer = "main",
  items = function(ctx)
    for i, line in ipairs(nx.buf.lines(nx.buf.current(), 0, -1)) do
      -- Right-align the line number in a 6-wide field so the text starts at the
      -- same column for any file up to 999999 lines; past that it just wraps wider.
      ctx.push({ text = string.format("%6d: %s", i, line), row = i })
    end
  end,
  confirm = function(item)
    nx.pos.set(".", { 0, item.row, 1 })
  end,
})

-- Relativize `path` against the cwd for a compact diagnostics label; the cwd is
-- escaped so a path-magic char in it (`.`, `-`, `(`, …) can't corrupt the anchor.
local function relpath(path)
  local cwd = vim.fn.getcwd()
  local anchor = cwd:gsub("[%(%)%.%%%+%-%*%?%[%]%^%$]", "%%%1")
  return (path or ""):gsub("^" .. anchor .. "/", "")
end

-- diagnostics: every diagnostic across all buffers (telescope's `diagnostics`), the
-- merged `nx.diagnostic.get()` set — LSP-pushed plus every client namespace. Static
-- and in-memory; `location` preview scrolls to and highlights the match, confirm
-- jumps via `nx.picker.edit`. Diagnostic records are 0-based (`lnum`/`col`), so the
-- pushed item's `row`/`col` add 1 to reach the picker's 1-based convention.
nx.picker.source({
  name = "diagnostics",
  title = "Diagnostics",
  layer = "main",
  preview = "location",
  items = function(ctx)
    for _, d in ipairs(nx.diagnostic.get()) do
      local name = nx.buf.name(d.bufnr)
      local sev = nx.diagnostic.severity[d.severity] or "?"
      -- Lead with the severity left-padded to 5 (the widest, `ERROR`) so the
      -- `file:line` column lines up; the message trails the (variable-width) path.
      ctx.push({
        text = string.format("%-5s %s:%d  %s", sev, relpath(name), d.lnum + 1, d.message),
        path = name,
        row = d.lnum + 1,
        col = d.col + 1,
      })
    end
  end,
  confirm = function(item, mode, layer)
    nx.picker.edit(item, mode, layer)
  end,
})

-- keymaps: every global mapping in normal / visual / insert mode (telescope's
-- `keymaps`), read from `nx.keymap.get`. The displayed `lhs` runs through
-- `nx.keytrans` so special keys read as notation (a space leader shows `<Space>`,
-- not a hard-to-see literal blank; likewise `<Tab>`, `<C-x>`, …); the raw `lhs` is
-- kept for the confirm feed. Confirm re-feeds it with remapping on, so picking a
-- mapping *runs* it, exactly like telescope.
nx.picker.source({
  name = "keymaps",
  title = "Keymaps",
  layer = "main",
  items = function(ctx)
    for _, mode in ipairs({ "n", "v", "i" }) do
      for _, k in ipairs(nx.keymap.get(mode)) do
        ctx.push({
          text = string.format("%s  %-16s %s", mode, nx.keytrans(k.lhs), k.desc or ""),
          lhs = k.lhs,
        })
      end
    end
  end,
  confirm = function(item)
    nx._feedkeys(item.lhs, true, false)
  end,
})

-- pickers: a picker of pickers (telescope's `builtin`) — every registered source
-- name, confirm opens the chosen one. Opening a picker from inside a `confirm` has
-- to wait for this picker to tear down first, so it defers to `nx.on_next_tick`.
nx.picker.source({
  name = "pickers",
  title = "Pickers",
  layer = "main",
  items = function(ctx)
    local names = {}
    for name in pairs(nx.picker._sources) do
      names[#names + 1] = name
    end
    table.sort(names)
    for _, name in ipairs(names) do
      ctx.push({ text = name, source = name })
    end
  end,
  confirm = function(item)
    nx.on_next_tick(function()
      nx.picker.open(item.source)
    end)
  end,
})

-- marks: the set marks (telescope's `marks`), read from `nx.mark.list` — the
-- current buffer's specials + `a`–`z`, the globals `A`–`Z`, then the numbered
-- `0`–`9`. `location` preview scrolls to the mark; confirm jumps via
-- `nx.picker.edit` when the mark names a file, or moves the cursor directly for a
-- mark in an unnamed current buffer (no path to open). Mirror positions are 0-based,
-- so the pushed item's `row`/`col` add 1.
nx.picker.source({
  name = "marks",
  title = "Marks",
  layer = "main",
  preview = "location",
  items = function(ctx)
    for _, m in ipairs(nx.mark.list()) do
      -- Fixed-width `name  line:col` prefix so the detail text lines up: the mark
      -- name is always one char, the line right-aligned to 6 (files up to 999999
      -- lines, matching `curbuf`), the col left-padded to 4.
      ctx.push({
        text = string.format("%s  %6d:%-4d %s", m.name, m.line + 1, m.col, m.text),
        path = m.path,
        row = m.line + 1,
        col = m.col + 1,
      })
    end
  end,
  confirm = function(item, mode, layer)
    if item.path ~= "" then
      nx.picker.edit(item, mode, layer)
    else
      nx.pos.set(".", { 0, item.row, item.col })
    end
  end,
})

-- jumplist: the focused window's jump history (telescope's `jumplist`), read from
-- `nx.jumplist.get` — the same `<C-o>`/`<C-i>` list `:jumps` shows. Listed
-- newest-first (the freshest jump on top, as telescope does), so item 1 is where a
-- single `<C-o>` would take you. Like `:jumps`, an entry in the *current* buffer
-- shows its line's text; one in another buffer shows the file name (arbitrary
-- buffers' lines aren't mirrored). `location` preview scrolls to the entry; confirm
-- jumps via `nx.picker.edit` when the entry names a file, else moves the cursor
-- directly for a mark in an unnamed current buffer. Mirror entries are 1-based
-- `lnum` / 0-based `col`, so the pushed item's `col` adds 1.
nx.picker.source({
  name = "jumplist",
  title = "Jumplist",
  layer = "main",
  preview = "location",
  items = function(ctx)
    local cur = nx.buf.current()
    local curlines = nx.buf.lines(cur, 0, -1)
    local list = nx.jumplist.get()[1]
    -- Newest-first: walk the oldest-first mirror in reverse.
    for i = #list, 1, -1 do
      local e = list[i]
      local path = nx.buf.name(e.bufnr)
      local detail
      if e.bufnr == cur then
        detail = (curlines[e.lnum] or ""):gsub("%s+$", "")
      else
        detail = path ~= "" and path or "[No Name]"
      end
      -- Fixed-width `line:col` prefix so the detail text lines up (line right-aligned
      -- to 6, matching `curbuf`/`marks`; col left-padded to 4).
      ctx.push({
        text = string.format("%6d:%-4d %s", e.lnum, e.col, detail),
        path = path,
        row = e.lnum,
        col = e.col + 1,
      })
    end
  end,
  confirm = function(item, mode, layer)
    if item.path ~= "" then
      nx.picker.edit(item, mode, layer)
    else
      nx.pos.set(".", { 0, item.row, item.col })
    end
  end,
})

-- ----- default leader maps ---------------------------------------------------
-- Bind the three shipped sources to `<leader>f{f,g,b}` out of the box. Registered
-- on VimEnter (after `init.lua` runs) so `<leader>` expands with the user's
-- `mapleader` — not the default `\` it would carry if set at prelude-load, before
-- the config sets mapleader. `default = true` puts each at the overridable rung, so
-- a user's own map for the same lhs wins regardless of order (bind to an empty
-- function to disable). Hermetic: the test harness fires VimEnter too, but these
-- only register maps — nothing spawns until a key is actually pressed.
-- Use `nx.autocmd.create` directly, not the `nx.on` sugar: this module loads
-- before nx.lua (where `nx.on` is defined), but autocmd.lua is already in.
nx.autocmd.create("VimEnter", {
  callback = function()
    for _, m in ipairs({
      { "<leader>ff", "files", "Find files" },
      { "<leader>fg", "live_grep", "Live grep" },
      { "<leader>fb", "buffers", "Find buffers" },
      { "<leader>fk", "keymaps", "Find keymaps" },
      { "<leader>fd", "diagnostics", "Find diagnostics" },
      { "<leader>fi", "pickers", "Find pickers" },
      { "<leader>fm", "marks", "Find marks" },
      { "<leader>fj", "jumplist", "Find jumplist" },
      { "<leader>f/", "curbuf", "Fuzzy find in current buffer" },
    }) do
      local source = m[2]
      nx.keymap.set("n", m[1], function()
        nx.picker.open(source)
      end, { default = true, desc = m[3] })
    end
    -- `<leader>fr` reopens the last picker where you left off (telescope's `resume`).
    nx.keymap.set("n", "<leader>fr", function()
      nx.picker.resume()
    end, { default = true, desc = "Resume last picker" })
  end,
})
