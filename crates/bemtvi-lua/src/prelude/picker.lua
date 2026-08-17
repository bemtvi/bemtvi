-- btv.picker: the native fuzzy finder over the unified float-list widget
-- (docs/specs/2026-06-14-btv-ui-float-widget.md, Phase 2). A picker is a centered
-- float with a prompt that grabs input; the server owns the prompt, the Rust
-- fuzzy matcher, navigation, and the generation token, so Lua only ever sees
-- "open", "run the source for this query" and "confirm". Sources are thin Lua
-- drivers: they stream candidates in via `ctx.push`, and handle `confirm(item)`.
--
-- The full item tables stay Lua-side (`btv._picker.active.items`); only a display
-- label + an integer key cross the bridge, exactly like btv.ui.select — so an
-- item's arbitrary fields (path/row/col) never need to serialize.

btv.picker = btv.picker or {}
btv.picker._sources = btv.picker._sources or {}

-- btv._picker holds the *active* picker: the running source, its per-generation
-- item array (keyed by the integer `key` pushed to the widget), the live
-- generation, and the current run's `on_cancel` (a superseded dynamic query runs
-- it to reap its job).
btv._picker = btv._picker or nil

-- The default debounce (ms) before a DYNAMIC source re-runs on a query edit — the
-- global knob. Override it here (`btv.picker.debounce = 400`), per source
-- (`debounce = N` on the source), or per open (`btv.picker.open(name, {debounce=N})`);
-- the more specific wins. `0` disables the debounce (re-run on every keystroke).
btv.picker.debounce = btv.picker.debounce or 250

-- Char length of `s` — the unit the widget's row offsets (`head` / `match`) are in.
-- Falls back to the byte length when the string isn't valid UTF-8 (a grep hit in a
-- mis-encoded file); a column off by a byte beats a nil arithmetic error.
local function charlen(s)
  return utf8.len(s) or #s
end

-- ----- rebindable picker keys -----------------------------------------------
-- Every picker key is an ordinary `picker`-mode keymap, NOT a hardcoded grab: the
-- server selects the `picker` bucket while a picker owns input, so navigation /
-- confirm / cancel / preview-scroll / query-edit are all configurable with
-- `btv.keymap.set('picker', '<key>', btv.picker.actions.<name>)` exactly like any
-- other mode. `btv.picker.actions.<name>` fires the named action through the keymap
-- engine (btv._picker_action -> Editor::apply_picker_action). The only key NOT a map
-- is an arbitrary printable char — there is no way to enumerate every char, so an
-- unmapped printable simply inserts into the query (the picker's text fallthrough).
btv.picker.actions = btv.picker.actions or {}
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
  "toggle_filters",
  "next_field",
  "history_prev",
  "history_next",
}) do
  btv.picker.actions[name] = function()
    btv._picker_action(name)
  end
end

-- The default picker bindings — `default = true` so a user `btv.keymap.set('picker', …)`
-- for the same key wins by the precedence ladder; binding a key to an empty
-- function (`btv.keymap.set('picker', '<C-n>', function() end)`) disables it. These
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
  -- The include/exclude filter boxes. `<C-g>` reveals them and steps through the
  -- three lines; `<Tab>` is already multi-select, so it is deliberately not reused.
  { "<C-g>", "next_field", "Cycle query / include / exclude" },
  -- Filter-line history, the cmdline-history keys one modifier over: `<Up>`/`<Down>`
  -- already move the selection, and they keep doing so while a box has focus.
  { "<C-Up>", "history_prev", "Recall an older filter line" },
  { "<C-Down>", "history_next", "Recall a newer filter line" },
}) do
  btv.keymap.set("picker", m[1], btv.picker.actions[m[2]], { default = true, desc = m[3] })
end

-- ----- include / exclude filters ---------------------------------------------
-- The picker's two glob boxes (VSCode's "files to include" / "files to exclude").
-- A filterable source gets them for free: the patterns are compiled once per run and
-- tested against every candidate's `path` in the sink below, so a source only has to
-- declare `filter = true` — and, if it shells out to a tool that can prune for
-- itself, splice `ctx.rg_globs` into its argv.

-- Expand one typed pattern into the glob(s) that match what a person means by it.
--
-- Two rules, and both exist to make ONE pattern mean the same thing to `btv.glob` and
-- to ripgrep's `-g`, which read a bare name differently:
--
--   * **No `/` ⇒ any depth.** `*.lock` becomes `**/*.lock`. ripgrep applies
--     gitignore's basename rule (a slash-less pattern matches at any depth) while
--     `btv.glob`'s `*` stops at `/`, so an un-anchored pattern would prune different
--     sets on the `rg` leg and the walk legs. Anchoring it here makes them identical.
--   * **A plain name is also a directory.** `target` becomes `**/target` *and*
--     `**/target/**` — candidates are files, so excluding only the directory entry
--     would leave every file under it. A pattern that already carries a glob
--     metacharacter (`src/**`, `*.lock`) is taken as written, since the user has said
--     what they mean.
--
-- A trailing `/` (`vendor/`) is dropped first — it only ever meant "the directory".
local function expand_pattern(pat)
  pat = pat:gsub("/+$", "")
  if pat == "" then
    return {}
  end
  local anchored = pat:find("/", 1, true) and pat or ("**/" .. pat)
  if btv.glob.is_glob(pat) then
    return { anchored }
  end
  return { anchored, anchored .. "/**" }
end

-- `btv.picker.patterns(line)` -> the glob patterns one filter box's text expands to.
-- `line` is the raw comma-separated text (`btv.glob.split` splits it, so a `{a,b}`
-- alternation stays one pattern). A list is accepted too, so a caller passing
-- `exclude = { "target/", "*.lock" }` need not join it first. Returns an empty list
-- for empty text — i.e. "no filter".
--
-- Each entry is expanded so that one typed pattern means the same set to `btv.glob`
-- and to ripgrep's `-g`, which read a bare name differently:
--
-- ```
-- *.lock     ->  **/*.lock                 no `/` ⇒ matches at any depth
-- target     ->  **/target, **/target/**   a plain name is also a directory
-- vendor/    ->  **/vendor, **/vendor/**   a trailing `/` only meant "directory"
-- src/**     ->  src/**                    already a glob ⇒ taken as written
-- ```
--
-- ```lua
-- btv.picker.patterns("target/, *.lock")
-- --> { "**/target", "**/target/**", "**/*.lock" }
-- ```
function btv.picker.patterns(line)
  local out = {}
  local entries
  if type(line) == "table" then
    entries = {}
    for _, v in ipairs(line) do
      for _, e in ipairs(btv.glob.split(tostring(v))) do
        entries[#entries + 1] = e
      end
    end
  else
    entries = btv.glob.split(tostring(line or ""))
  end
  for _, e in ipairs(entries) do
    for _, g in ipairs(expand_pattern(e)) do
      out[#out + 1] = g
    end
  end
  return out
end

-- Coerce a user-supplied `include` / `exclude` option into the ONE comma-separated
-- line the box holds. A list joins with ", " so it reads back the way it would have
-- been typed; a string passes through.
local function filter_line(value, what)
  if value == nil then
    return nil
  end
  if type(value) == "string" then
    return value
  end
  if type(value) == "table" then
    return table.concat(value, ", ")
  end
  error("btv.picker: " .. what .. " must be a string or a list of strings", 3)
end

-- The `-g` argument pairs that hand `patterns` to a ripgrep-compatible tool.
-- `negate` emits `!pat` (rg's exclusion form). Pruning at the tool is what keeps a
-- `node_modules`-heavy tree from burning the result cap on paths we would throw away
-- a moment later — the sink still tests every candidate, so this can only ever remove
-- what the sink would have removed anyway.
local function rg_glob_args(patterns, negate)
  local args = {}
  for _, p in ipairs(patterns) do
    args[#args + 1] = "-g"
    args[#args + 1] = negate and ("!" .. p) or p
  end
  return args
end

-- ----- filter defaults and history -------------------------------------------

-- The `btv.picker.setup` config. Kept as a plain table (not a source spec) because
-- these are cross-picker defaults, not any one source's business.
btv.picker._config = btv.picker._config or { include = nil, exclude = nil, history = 20 }

-- The persisted store. The prelude attributes to no runtimepath entry, so the
-- namespace is passed explicitly — `"picker"` is reserved for exactly this. Opened
-- lazily so a session that never opens a filterable picker never touches shada.
local function history_store()
  return btv.shada.plugin("picker")
end

-- One box's recallable lines, most recent first. Missing (or a store holding
-- something other than a list of strings — a hand-edited shada, an older layout)
-- reads as no history rather than raising: a bad recall list must never stop a
-- picker from opening.
local function history_get(field)
  local ok, list = pcall(function()
    return history_store():get("filter_history_" .. field)
  end)
  if not ok or type(list) ~= "table" then
    return {}
  end
  local out = {}
  for _, v in ipairs(list) do
    if type(v) == "string" and v ~= "" then
      out[#out + 1] = v
    end
  end
  return out
end

-- `btv.picker.setup(opts)`: cross-picker defaults for the include/exclude boxes.
--
--   * `include` / `exclude` — the line every filterable picker opens with, as a
--     comma-separated string or a list of globs. This is the "stop showing me
--     `target/`" knob: set it once and every `files` / `live_grep` opens narrowed.
--   * `history` — how many past lines each box keeps for `<C-Up>` / `<C-Down>`
--     (default 20). `0` disables the history *and* its persistence.
--
-- These are the LOW end of the precedence ladder — a line you actually typed
-- (recalled from history) and an explicit `btv.picker.open{ include = … }` both win,
-- so a default never overrides an intent expressed later. Calling it again replaces
-- only the keys given.
--
-- ```lua
-- btv.picker.setup({
--   exclude = { "target/", "node_modules/", "*.min.js" },
-- })
-- ```
function btv.picker.setup(opts)
  if opts ~= nil and type(opts) ~= "table" then
    error("btv.picker.setup: opts must be a table", 2)
  end
  opts = opts or {}
  if opts.include ~= nil then
    btv.picker._config.include = filter_line(opts.include, "include")
  end
  if opts.exclude ~= nil then
    btv.picker._config.exclude = filter_line(opts.exclude, "exclude")
  end
  if opts.history ~= nil then
    if type(opts.history) ~= "number" or opts.history < 0 then
      error("btv.picker.setup: history must be a non-negative number", 2)
    end
    btv.picker._config.history = math.floor(opts.history)
  end
end

-- btv.picker._history_record(include, exclude): fold a just-closed picker's filter
-- lines into the persisted history. Called by the server with the lines the boxes
-- held **at close** (not at the last source run — a dynamic source's re-run is
-- debounced and can lag the final keystroke).
--
-- A recorded line moves to the front rather than being appended again, so cycling
-- walks distinct patterns instead of a run of duplicates, and the most recent is
-- always first (which is what a picker pre-fills from). An empty line records
-- nothing — clearing a box is not a pattern worth recalling.
function btv.picker._history_record(include, exclude)
  local cap = btv.picker._config.history or 20
  if cap <= 0 then
    return
  end
  local store = history_store()
  for field, line in pairs({ include = include, exclude = exclude }) do
    if type(line) == "string" and line ~= "" then
      local list = history_get(field)
      local out = { line }
      for _, v in ipairs(list) do
        if v ~= line and #out < cap then
          out[#out + 1] = v
        end
      end
      store:set("filter_history_" .. field, out)
    end
  end
end

-- `btv.picker.history(field)` -> the recallable lines for `"include"` / `"exclude"`,
-- most recent first. The read side of what `<C-Up>` / `<C-Down>` cycle; useful for a
-- config that wants to seed a picker from a recent filter, or just to see what is
-- stored.
--
-- ```lua
-- local recent = btv.picker.history("exclude")[1]   -- the last exclude line used
-- ```
function btv.picker.history(field)
  if field ~= "include" and field ~= "exclude" then
    error('btv.picker.history: field must be "include" or "exclude"', 2)
  end
  return history_get(field)
end

-- `btv.picker.forget_history()`: drop every stored filter line (both boxes). The
-- escape hatch for a history that has accumulated patterns you don't want back.
function btv.picker.forget_history()
  local store = history_store()
  store:delete("filter_history_include")
  store:delete("filter_history_exclude")
end

-- btv.picker.source { name, items = function(ctx), dynamic, confirm, preview }:
-- register a source. `items(ctx)` streams candidates: it calls `ctx.push(item)` per
-- result (an item is a table with a `text` display field, plus any data `confirm` or
-- the preview needs — e.g. `path` / `row` / `col`) and signals completion by
-- *returning* — a synchronous source just returns when
-- its loop ends, an asynchronous one is wrapped in `btv.async` and returns the
-- promise (the engine awaits it; btv is promise-only, so there is no `done`
-- callback). A streaming source consumes a `btv.run_stream` with `btv.await_each`,
-- and reaps its job on close via `ctx.on_cancel`. `dynamic = true` re-runs `items`
-- on every prompt edit (live grep — the matcher is bypassed), reading the live
-- prompt from `ctx.query` and the working directory from `ctx.cwd`; the default is a
-- static source matched locally in Rust as you type. `confirm(item)` acts on the
-- chosen item. Optional `preview` adds a side pane for the highlighted item:
-- `"file"` shows the head of `item.path`, `"location"` shows `item.path` positioned
-- at `item.row` / `item.col` (1-based). Omitted ⇒ no preview pane; per-open
-- overridable via `btv.picker.open(name, { preview = … })`. Optional `width` /
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
-- in-flight search. It defaults to the global `btv.picker.debounce` (250), and is
-- also overridable per open via `btv.picker.open(name, { debounce = N })`; `0`
-- disables it. While that search runs, the PREVIOUS results stay
-- on screen (the list never flashes empty); they are swapped out only when the new
-- search's first result arrives, or cleared if it matched nothing. The widget
-- windows its rendering and matches incrementally, so a source can stream 100k+
-- candidates and stay fast; `max_results` (default 100000) is only a runaway-source
-- safety bound.
--
-- An item may also be a **two-column** row — a location column plus content, as
-- `live_grep`'s `path:line:col: <the matched line>` is:
--
-- ```lua
-- ctx.push({
--   head = "src/main.rs:12:5: ",  -- the location column
--   text = "let x = compute()",   -- the body (the row's label is `head .. text`)
--   match = { 9, 15 },            -- 1-based INCLUSIVE char range of the hit in `text`
-- })
-- ```
--
-- The widget then fits the two columns separately: the head keeps at least 40% of the
-- row (a long line can never squeeze the file name off), and the body is windowed
-- around `match` so the hit stays on screen instead of scrolling off the right edge.
-- `match` also highlights, which is what a `dynamic` source wants — it bypasses the
-- fuzzy matcher, so its own match is the only one to show. Every field is optional:
-- a plain `text`-only item is a single-column row that truncates path-tail-first.
--
-- A row may also lead with a short **tag** — a classification the head must never lose
-- — and declare its own **color**, the highlight group it is painted with:
--
-- ```lua
-- ctx.push({
--   tag = "E",                     -- pinned: the elision happens after it
--   head = "src/main.rs:12 ",      -- the location column
--   text = "unused variable",      -- the body
--   hl = "DiagnosticError",        -- the group the row paints with
-- })
-- ```
--
-- The tag is prepended to the head (`"E src/main.rs:12 "`) and *pinned*: a head too
-- narrow for its path elides around the tag rather than dropping it, so the letter that
-- says what the row IS survives at any width. Without it the head elides tail-first,
-- which is right for a pure location (live_grep keeps `file:line`) and wrong for a
-- classified one.
--
-- `hl` is an ordinary highlight-group name, resolved against the live colorscheme each
-- frame (a group the scheme leaves undefined simply doesn't paint — never an error, so
-- a source may name groups a minimal theme lacks). It colors the row's **head** column
-- when the row declares one, and the whole label when it doesn't: the head is the part
-- that classifies the row, and leaving the body alone keeps the fuzzy-match highlight
-- readable over it. The selected row keeps its selection background either way.
--
-- `filter = true` gives the picker the include/exclude glob boxes (`<C-g>`), and is
-- what every candidate's `path` is then tested against — declare it on any source
-- whose items are paths. A source that shells out to `rg` should also splice
-- `ctx.rg_globs` into its argv so the tool prunes the tree instead of streaming paths
-- the sink is about to drop. Without it the filter keys are an echoed no-op, since
-- there would be nothing to filter on.
--
-- `resumable = false` opts the source out of `btv.picker.resume()` (`<leader>fr`):
-- opening it never overwrites the resume slot, so a transient internal picker (the
-- cmdline file completer) can't shadow the last user-facing one. Defaults to true.
function btv.picker.source(spec)
  if type(spec) ~= "table" or type(spec.name) ~= "string" then
    error("btv.picker.source: requires a { name = <string>, items = <fn> } table", 2)
  end
  if type(spec.items) ~= "function" then
    error("btv.picker.source('" .. spec.name .. "'): items must be a function", 2)
  end
  if spec.preview ~= nil and spec.preview ~= "file" and spec.preview ~= "location" then
    error("btv.picker.source('" .. spec.name .. '\'): preview must be "file" or "location"', 2)
  end
  if spec.layer ~= nil and spec.layer ~= "main" and spec.layer ~= "active" then
    error("btv.picker.source('" .. spec.name .. '\'): layer must be "main" or "active"', 2)
  end
  if spec.filter ~= nil and type(spec.filter) ~= "boolean" then
    error("btv.picker.source('" .. spec.name .. "'): filter must be a boolean", 2)
  end
  btv.picker._sources[spec.name] = spec
end

-- btv.picker.open(name[, opts]): open the picker for the registered source `name`.
-- Each `opts` field overrides the matching field on the source (which in turn
-- overrides the picker default):
--   * `width` / `height` — a FIXED box size: a cell count (e.g. 100) or a CSS-style
--     viewport fraction string (`"80vw"` / `"60vh"` / `"50%"`). The picker is never
--     content-sized (a content-hugging box looks ragged).
--   * `align` + `margin` — placement, like a float (see `btv.picker.source`).
--   * `preview` — `"file"` / `"location"` / nil (no pane).
--   * `prompt_pos` — `"top"` (default) / `"bottom"`.
--   * `query` — initial prompt text: the picker opens already filtered against it
--     (the gen-0 run uses it instead of `""`), caret at its end. Default `""`.
--   * `title` — a title centered on the box's top border (e.g. `"Find Files"`); nil
--     ⇒ no title. The shipped sources set their own (`"Find Files"`/`"Live Grep"`/…).
--   * `multiselect` — whether `<Tab>` marks rows for a batch action (default true);
--     `false` is a single-choice picker (no marking).
--   * `debounce` — ms before a `dynamic` source re-runs on a query edit; `0` off.
--   * `include` / `exclude` — pre-fill the glob filter boxes, so a keymap can open a
--     picker already scoped (`btv.picker.open("files", { include = "src/**" })`). Each
--     takes a comma-separated string or a list of patterns, and is a *seed*: the boxes
--     stay editable. Only meaningful on a source with `filter = true`.
--   * `filters` — `"open"` reveals the filter rows immediately (what a pre-filtered
--     picker usually wants), `"collapsed"` (the default) keeps the picker's ordinary
--     single-line shape with the active patterns shown as a badge.
--   * `layer` — where a confirmed item opens: `"main"` crosses back to the main editor
--     area first (so a file picked while focused in a dock lands in the editor, not
--     the sidebar), `"active"` opens in the focused layer. Defaults to `"active"`; the
--     shipped `files`/`live_grep` sources set `"main"`, `buffers` stays `"active"`.
function btv.picker.open(name, opts)
  local source = btv.picker._sources[name]
  if not source then
    error("btv.picker.open: no such source '" .. tostring(name) .. "'", 2)
  end
  opts = opts or {}
  -- Resolve the preview kind: per-open overrides per-source. nil ⇒ no preview pane.
  local preview = opts.preview
  if preview == nil then
    preview = source.preview
  end
  if preview ~= nil and preview ~= "file" and preview ~= "location" then
    error('btv.picker.open: preview must be "file" or "location"', 2)
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
    error('btv.picker.open: layer must be "main" or "active"', 2)
  end
  btv._picker =
    { source = source, items = {}, gen = 0, on_cancel = nil, preview = preview, layer = layer }
  local width = btv._geom.size(opts.width ~= nil and opts.width or source.width)
  local height = btv._geom.size(opts.height ~= nil and opts.height or source.height)
  -- Placement: `align` (a 9-grid word, default centered) + `margin` (a gap from the
  -- editor edges), each per-open overriding per-source. The picker box used to be
  -- centered-only; now it can sit in any corner with a margin, like a float.
  local align = btv._geom.align(opts.align ~= nil and opts.align or source.align)
  local margin = btv._geom.margin(opts.margin ~= nil and opts.margin or source.margin)
  -- Resolve the debounce: per-open overrides per-source overrides the global
  -- default. `0` is a valid (truthy) value — disable — so test for `nil`, not `or`.
  local debounce = opts.debounce
  if debounce == nil then
    debounce = source.debounce
  end
  if debounce == nil then
    debounce = btv.picker.debounce
  end
  btv._picker.debounce_ms = debounce or 250
  -- Prompt position: per-open overrides per-source overrides the default ("top").
  -- "bottom" puts the input under the results (telescope-style); anything else is
  -- top. Resolved to a bool for the bridge.
  local prompt_pos = opts.prompt_pos
  if prompt_pos == nil then
    prompt_pos = source.prompt_pos
  end
  local prompt_bottom = prompt_pos == "bottom"
  -- The initial prompt text: `btv.picker.open(name, { query = "src/ed" })` opens
  -- the picker already filtered against `query` (the gen-0 run uses it instead of
  -- ""), with the caret at its end so the user keeps typing. Defaults to "" — the
  -- historical empty-prompt open.
  local query = opts.query
  if query == nil then
    query = ""
  end
  if type(query) ~= "string" then
    error("btv.picker.open: query must be a string", 2)
  end
  -- An optional title for the picker box's top border (`title = "Select file"`).
  -- per-open overrides per-source; nil ⇒ no title.
  local title = opts.title
  if title == nil then
    title = source.title
  end
  if title ~= nil and type(title) ~= "string" then
    error("btv.picker.open: title must be a string", 2)
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
  -- The resume slot (`btv.picker.resume` / `<leader>fr`). The reopen replays a frozen
  -- snapshot the *server* holds (the displayed rows, cursor, marks, query) — a
  -- live-grep order isn't reproducible, so we never re-run the source. Lua's only job
  -- is to keep `btv._picker` (the source + the window's item tables) alive for
  -- `confirm` and future query edits; `btv._picker_save_resume` fills `last.picker` at
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
    btv.picker._last = last
    btv._picker._last = last
  end
  -- The include/exclude boxes. Only a source that declared `filter = true` has them;
  -- for anything else the seeds stay empty and the filter keys echo a no-op.
  --
  -- Precedence, low to high: the source's own default, then `btv.picker.setup`, then
  -- the most recent persisted line, then this call's options. History sits above the
  -- configured defaults because a line you actually typed is a stronger statement
  -- than one you configured months ago; the explicit `open` option sits above
  -- everything so a picker asked for a scope gets exactly that scope and is never
  -- surprised by a stale box. All of them are seeds — the boxes stay editable.
  local filterable = source.filter == true
  local hist_include, hist_exclude = {}, {}
  if filterable and (btv.picker._config.history or 20) > 0 then
    hist_include, hist_exclude = history_get("include"), history_get("exclude")
  end
  local include = filter_line(opts.include, "include")
    or hist_include[1]
    or btv.picker._config.include
    or filter_line(source.include, "include")
    or ""
  local exclude = filter_line(opts.exclude, "exclude")
    or hist_exclude[1]
    or btv.picker._config.exclude
    or filter_line(source.exclude, "exclude")
    or ""
  if opts.filters ~= nil and opts.filters ~= "open" and opts.filters ~= "collapsed" then
    error('btv.picker.open: filters must be "open" or "collapsed"', 2)
  end
  local filters_open = opts.filters == "open"
  -- Filters are the source's to honor: they reach it as `ctx.include` / `ctx.exclude`
  -- and are applied to every pushed path, so remember them for each run.
  btv._picker.include = include
  btv._picker.exclude = exclude
  -- The server opens the aligned widget and kicks the initial run (gen 0, query);
  -- `resumable` tells it whether to snapshot this picker when it closes.
  btv._picker_open(
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
    source.resumable ~= false,
    -- The boxes travel as one table: six values that grow together, against mlua's
    -- 16-argument ceiling on the bridge.
    {
      on = filterable,
      include = include,
      exclude = exclude,
      open = filters_open,
      include_history = hist_include,
      exclude_history = hist_exclude,
    }
  )
end

-- btv.picker.resume(): reopen the most-recently-closed picker (telescope's `resume`),
-- restored to exactly where the user left off — the displayed rows, prompt text,
-- highlighted row, and multi-select marks. The server replays a frozen snapshot it
-- captured at close (bounded to a window around the cursor), so a live-grep picker
-- comes back with its *actual* previous results, not a fresh (differently-ordered)
-- search. Re-installs `btv._picker` so `confirm` works and a later query edit re-runs
-- the source. No-op (a gentle notice) before any resumable picker has closed.
function btv.picker.resume()
  local last = btv.picker._last
  if not (last and last.picker) then
    btv.notify("btv.picker: no picker to resume", "info")
    return
  end
  -- Re-arm the Lua-side runtime (a fresh shallow copy each resume so re-closing
  -- snapshots cleanly), then let the server replay its frozen menu.
  local saved = last.picker
  btv._picker = {
    source = saved.source,
    items = saved.items,
    preview = saved.preview,
    layer = saved.layer,
    gen = saved.gen,
    nitems = saved.nitems,
    debounce_ms = saved.debounce_ms,
    _last = last,
  }
  btv._picker_resume()
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

-- btv._picker_run(gen, query, include, exclude): (re-)run the active source for `query`
-- under `gen`, filtered by the two glob boxes. Called by the server on open (gen 0)
-- and on each prompt edit that needs a re-run — a query edit on a **dynamic** source,
-- or an include/exclude edit on any filterable one (the set of paths that exist has
-- changed, which no local re-ranking can produce).
--
-- A **dynamic** source is DEBOUNCED — a query edit cancels the in-flight job and any
-- pending run, then schedules the search `debounce` ms later, so a fast typist
-- spawns one process per pause, not one per keystroke. Static / the initial run
-- start immediately (no process churn to debounce).
function btv._picker_run(gen, query, include, exclude)
  local p = btv._picker
  if not p then
    return
  end
  -- The boxes are authoritative once the user edits them; the server sends their live
  -- text with every run, so the seeds recorded at open are only the gen-0 value.
  -- Whether they *changed* decides the debounce below, so compare before overwriting.
  local filters_changed = (include ~= nil and include ~= p.include)
    or (exclude ~= nil and exclude ~= p.exclude)
  p.include = include or p.include or ""
  p.exclude = exclude or p.exclude or ""
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
    if btv._picker ~= p or p.gen ~= gen then
      return
    end

    -- The filter boxes, expanded once per run: `include` / `exclude` are the compiled
    -- pattern lists a source may hand to its own tool, `rg_globs` the ready-to-splice
    -- `-g` argv for a ripgrep-compatible one, and the two globsets below are what the
    -- sink tests every candidate against.
    local include_pats = btv.picker.patterns(p.include)
    local exclude_pats = btv.picker.patterns(p.exclude)
    local include_set = #include_pats > 0 and btv.glob.set(include_pats) or nil
    local exclude_set = #exclude_pats > 0 and btv.glob.set(exclude_pats) or nil
    local rg_globs = rg_glob_args(include_pats, false)
    for _, a in ipairs(rg_glob_args(exclude_pats, true)) do
      rg_globs[#rg_globs + 1] = a
    end

    local ctx = {
      query = query,
      cwd = vim.fn.getcwd(),
      gen = gen,
      include = include_pats,
      exclude = exclude_pats,
      rg_globs = rg_globs,
      -- A source registers a reaper for its in-flight job; the next run (or close)
      -- invokes it. Only the *current* run of the *active* picker registers — the
      -- identity check (`btv._picker == p`) drops a registration from a run whose
      -- picker has since closed (a new picker reuses generation 0).
      on_cancel = function(fn)
        if btv._picker == p and p.gen == gen then
          p.on_cancel = fn
        end
      end,
    }

    -- Candidates are buffered and crossed to the server in batches (one bridge call
    -- per ~`FLUSH_N` items, not per item) — the key to streaming 100k results fast.
    -- When the picker carries a preview pane, the per-item target travels in parallel
    -- arrays: `paths` (both kinds; "" ⇒ that row has no target) and, for the
    -- "location" kind, 0-based `rows` / `cols`. nil arrays ⇒ no preview (the common
    -- btv.ui.select / preview-less picker path is unchanged).
    local pv = p.preview -- nil | "file" | "location"
    local labels, keys, batched = {}, {}, 0
    local paths = pv and {} or nil
    local rows = pv == "location" and {} or nil
    local cols = pv == "location" and {} or nil
    -- Two-column rows (`push { head = … }`): a flat run of three char offsets per
    -- item — head length, match start, match end. nil until a row declares one, so a
    -- plain source's batch is exactly as before.
    local layouts = nil
    -- Per-row highlight groups (`push { hl = … }`): one name per item, `""` for an
    -- unpainted row. nil until a row declares one, exactly like `layouts`.
    local hls = nil
    local pushed = 0 -- this run's result count, for the cap (p.items is session-wide)
    local function flush()
      if batched > 0 then
        btv._picker_push(gen, labels, keys, paths, rows, cols, layouts, hls)
        labels, keys, batched = {}, {}, 0
        if paths then
          paths = {}
        end
        if rows then
          rows, cols = {}, {}
        end
        layouts = nil
        hls = nil
      end
    end
    local function push(item)
      -- Drop a push from a run the user has typed past (`p.gen ~= gen`) OR from a
      -- run whose picker has already closed (`btv._picker ~= p`). The identity check
      -- is essential: generation resets to 0 on every open, so a stale gen-0 push
      -- from a closed picker's orphaned job would otherwise collide with a freshly
      -- opened picker (also gen 0).
      if btv._picker ~= p or p.gen ~= gen then
        return
      end
      local entry = item
      if type(entry) ~= "table" then
        entry = { text = tostring(entry) }
      end
      -- The include/exclude boxes, applied at the ONE point every candidate crosses —
      -- so every filterable source is filtered identically, whether its paths came
      -- from `rg`, from `find`, or from an `btv.fs` walk, and a source that prunes at
      -- its tool and one that cannot still enumerate the same set. Dropped candidates
      -- are dropped *before* the cap below, so a filtered-out `node_modules` can never
      -- consume the budget the results you asked for need.
      local path = entry.path
      if path and path ~= "" then
        if include_set and not include_set:test(path) then
          return
        end
        if exclude_set and exclude_set:test(path) then
          return
        end
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
      -- `p.nitems` is an O(1) running count (no `#p.items` border-search per item),
      -- and the absolute key into the session-wide `p.items`.
      p.nitems = (p.nitems or 0) + 1
      p.items[p.nitems] = entry
      batched = batched + 1
      local text = entry.text or tostring(entry)
      -- A two-column row (`head`): the label is `head .. text`, and the widget gets the
      -- head's char length plus the source's own match range (`entry.match`, 1-based
      -- inclusive char offsets into `text`) so it can fit the two columns separately and
      -- highlight the hit. A plain row ships the all-zero sentinel when some *other* row
      -- in this batch declared a layout — the array stays dense and parallel.
      if entry.head then
        -- A `tag` leads the head, separated by one space, and its width travels so the
        -- widget can pin it through an elision (see `btv.picker.source`).
        local tagw = entry.tag and (charlen(entry.tag) + 1) or 0
        local head = tagw > 0 and (entry.tag .. " " .. entry.head) or entry.head
        local h = charlen(head)
        labels[batched] = head .. text
        if not layouts then
          -- Backfill the plain rows already batched, so entry `i` stays at `i*4`.
          layouts = {}
          for _ = 1, (batched - 1) * 4 do
            layouts[#layouts + 1] = 0
          end
        end
        local m = entry.match
        layouts[#layouts + 1] = h
        layouts[#layouts + 1] = h + (m and math.max(0, m[1] - 1) or 0)
        layouts[#layouts + 1] = h + (m and math.max(0, m[2]) or 0)
        layouts[#layouts + 1] = tagw
      else
        labels[batched] = text
        if layouts then
          layouts[#layouts + 1], layouts[#layouts + 2] = 0, 0
          layouts[#layouts + 1], layouts[#layouts + 2] = 0, 0
        end
      end
      -- The row's own highlight group. Like `layouts`, the array materializes only
      -- once some row declares one, and backfills the plain rows already batched so
      -- entry `i` stays at index `i`.
      if entry.hl then
        if not hls then
          hls = {}
          for j = 1, batched - 1 do
            hls[j] = ""
          end
        end
        hls[batched] = tostring(entry.hl)
      elseif hls then
        hls[batched] = ""
      end
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
      if btv._picker == p and p.gen == gen then
        btv._picker_finish(gen)
      end
    end
    -- The source emits through `ctx.push` (the sink) and signals completion by
    -- *returning* (a promise from btv.async, or nothing for a synchronous source) —
    -- btv is promise-only, so there is no `done` callback passed in.
    ctx.push = push

    -- Drive the source's completion. btv.promise.try unifies a synchronous source
    -- (returns nil ⇒ already done) and an async one (returns a promise that settles
    -- when its coroutine finishes), AND folds a synchronous throw into the same
    -- rejection path: notify on either (`:catch`), then `done()` exactly once
    -- whichever way it goes (`:finally`) — never a wedged picker.
    btv.promise
      .try(p.source.items, ctx)
      :catch(function(err)
        btv.notify("btv.picker: source '" .. p.source.name .. "' error: " .. tostring(err), "error")
      end)
      :finally(done)
  end

  -- Debounce anything that RE-RUNS the source per keystroke, so a fast typist spawns
  -- one search per pause rather than one per character.
  --
  -- A dynamic source is the original case (every query edit re-runs it). A **filter**
  -- edit re-runs the source too — for a static source as much as a dynamic one, since
  -- changing the patterns changes which paths exist — and that re-run is a full tree
  -- walk: undebounced, typing `node_modules` into the exclude box would spawn a dozen
  -- `rg` scans of the whole repo, one per character. The initial run (gen 0) is never
  -- debounced; there is nothing to coalesce yet and the list would just open late.
  local delay = p.debounce_ms or 0
  if gen > 0 and delay > 0 and (p.source.dynamic or filters_changed) then
    p.debounce = btv.timer(start, delay) -- trailing debounce; a new edit reschedules
  else
    start()
  end
end

-- Save a closing picker's runtime onto its `_last` slot so `btv.picker.resume()` can
-- re-arm it: the source (for `confirm` + future query edits) and the item tables for
-- the snapshot window the server kept (`resume_keys`, in display order). Trimming
-- `items` to the window bounds retained memory the same way the server bounds its
-- snapshot. Tied to the *closing* picker's own `_last` (`p._last`) so a stale close
-- never clobbers a newer picker's slot; a `resumable = false` source has no `_last`.
function btv._picker_save_resume(p, resume_keys)
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

-- btv._picker_result(key): the picker resolved. `key` (an integer) confirms the
-- item under that key for the current generation; `nil` cancels. Either way the
-- active picker is cleared (and a pending job reaped).
-- `mode` is the confirm gesture's open mode — `"current"` (the focused window) or
-- `"tab"` (the default `<C-t>` ⇒ a new tab) — forwarded to
-- `source.confirm(item, mode, layer)`. `layer` is the resolved confirm target
-- (`"main"`/`"active"`); built-in sources honor both (see `btv.picker.edit`).
-- `resume_keys` (the snapshot window's item keys) lets `btv.picker.resume()` re-arm this
-- picker — see `btv._picker_save_resume`.
function btv._picker_result(key, mode, resume_keys)
  local p = btv._picker
  btv._picker = nil
  btv._picker_save_resume(p, resume_keys)
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
      btv.notify("btv.picker: confirm error: " .. tostring(err), "error")
    end
  end
end

-- A picker item -> a location-list entry. Only items carrying a `path` make sense
-- in a list; `row`/`col` are 1-based (the item convention), defaulting to the file
-- head. `text` is the item's display text — for a two-column row (`head`) that is the
-- content column alone (`live_grep`'s matched line), which is exactly what a list wants
-- beside its own `filename` / `lnum` / `col`.
local function picker_item_to_qf(item)
  return {
    filename = item.path,
    lnum = item.row or 1,
    col = item.col or 1,
    text = item.text or "",
  }
end

-- btv._picker_send(keys, resume_keys, query): the "send results to a list" outcome
-- (the picker's `<C-q>`). `keys` are the matched item keys in display order — the
-- *filtered* result set the server captured before closing the picker; `query` is the
-- live prompt text. Map the keys back to their source item tables, keep the ones with
-- a target file, and stash them in a **named list** keyed `<picker>:<query>` — so each
-- distinct search is its own persistent dock tab (re-running the same search updates
-- it in place), independent of the global quickfix and of any window. Deferred with
-- `btv.schedule` so the picker float has closed and focus is back in the main layer
-- before the tab opens.
function btv._picker_send(keys, resume_keys, query)
  local p = btv._picker
  btv._picker = nil
  -- Keep the resume slot current even when the picker closed via a send.
  btv._picker_save_resume(p, resume_keys)
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
  btv.schedule(function()
    btv.qf.list(name, items, { title = name })
    btv.qf.show(name)
  end)
end

-- btv.picker.edit(item): the common confirm action — open `item.path`, and if the
-- item carries a 1-based `row` (and optional 1-based `col`, as live_grep / LSP
-- location items do), jump the cursor there.
--
-- A *located* jump (`item.row` set) goes through the `btv._jump_to` bridge, NOT
-- `:edit`: a jump must navigate, never reload. `:edit`-ing the location would (a)
-- error with E37 when the target is the *current* modified buffer (the LSP hands
-- back an absolute path, but the open buffer may be relatively named) and (b)
-- strand a duplicate buffer when that absolute path doesn't string-match the
-- relative one. `btv._jump_to` reuses the open buffer cwd-aware and skips the
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
function btv.picker.edit(item, mode, layer)
  local col = math.max(0, (item.col or 1) - 1)
  local to_main = layer == "main"
  if mode == "tab" or mode == "split" or mode == "vsplit" then
    -- A fresh tab / split for the file; located items land the cursor, plain opens
    -- start at the top.
    btv._jump_to(item.path, item.row and (item.row - 1) or 0, item.row and col or 0, mode, to_main)
  elseif item.row then
    btv._jump_to(item.path, item.row - 1, col, nil, to_main)
  else
    -- Open honoring 'switchbuf' (a file already shown in another tab is focused
    -- there under the default `usetab`), not a plain `:edit` into this window.
    -- Distinct bridge from btv.open's `btv._open` (the `:edit`-like layer open).
    btv._open_switchbuf(item.path, to_main)
  end
end

-- ----- built-in sources ------------------------------------------------------
-- Shipped defaults exercising the three source shapes; a config can register more.

-- files: a static source — file paths streamed in, fuzzy-matched locally. An
-- btv.async source: iterate the run_stream's batches with btv.await_each, pushing each
-- path; returning ends the run. The stream is reaped on close via on_cancel.
--
-- Enumeration falls back through a chain so the picker lists files in every mode:
-- `rg --files` (fast) → `find` (any real shell lacking rg) → a transport-agnostic
-- `btv.fs` walk. The binaries need a real shell, so the pure web client — where a
-- hostless spawn fails loud with code -1 — lands on the btv.fs walk (which rides the
-- off-tick seam to OPFS / the daemon). Each step runs only when the previous one
-- could not RUN (`stream:exit().code == -1`), never merely because it listed nothing.
--
-- The search is **unrestricted by default** (`rg -uu` = `--no-ignore --hidden`): a
-- file you can't find isn't a file, so ignore rules and dotfiles don't hide it. Only
-- `.git` is excluded (its object store is noise, not source), matching what the
-- `find` / `btv.fs` steps already do. Narrowing is the **exclude box**'s job (`<C-g>`)
-- rather than a different default: it is per-search, so hiding `target/` this time
-- never makes a file unfindable the next.
btv.picker.source({
  name = "files",
  title = "Find Files",
  layer = "main", -- a picked file opens in the main editor, never a focused dock
  preview = "file", -- the preview pane shows the file's head
  filter = true, -- items are paths: give it the include/exclude boxes
  items = btv.async(function(ctx)
    local cancelled = false
    -- Stream a listing command's stdout as candidates. Returns whether the chain is
    -- SETTLED here — the tool ran (whatever it listed), or the run was superseded.
    -- `strip` removes `find`'s leading "./" so its paths match rg's relative style.
    local function run(cmd, args, strip)
      local stream = btv.run_stream({ cmd = cmd, args = args, cwd = ctx.cwd })
      -- Reap the job when the picker closes, so a confirmed/cancelled picker doesn't
      -- leave a process streaming paths into the void. Sequential steps each arm this
      -- for the only stream that can still be running.
      ctx.on_cancel(function()
        cancelled = true
        stream:kill()
      end)
      for batch in btv.await_each(stream) do
        for _, l in ipairs(batch) do
          if strip then
            l = l:gsub("^%./", "")
          end
          if l ~= "" then
            ctx.push({ text = l, path = l })
          end
        end
      end
      -- The child's exit status is the canonical "did this tool exist" signal:
      -- `-1` is a spawn failure (no such binary) or our own `:kill()`. Anything
      -- else means it ran, so an empty listing is its ANSWER and the chain stops.
      local exit = stream:exit()
      return cancelled or exit == nil or exit.code ~= -1
    end
    -- The filter boxes ride along as `-g` arguments so rg prunes the tree rather than
    -- streaming paths the sink would drop — on a `node_modules`-sized directory that
    -- is the difference between the results arriving and the cap filling with noise.
    -- The sink still tests every path, so this only ever removes what it would too.
    local files_args = { "--files", "--color=never", "-uu", "-g", "!.git" }
    for _, a in ipairs(ctx.rg_globs) do
      files_args[#files_args + 1] = a
    end
    if run("rg", files_args) then
      return
    end
    if run("find", { ".", "-type", "f", "-not", "-path", "*/.git/*" }, true) then
      return
    end
    -- No shell / no rg / no find (the pure web client): walk the tree over btv.fs.
    -- `hidden = true` keeps this leg unrestricted like the two above (the walk's
    -- default `skip` already prunes `.git`).
    local ok, files = pcall(btv.await, btv.fs.walk(ctx.cwd, { hidden = true }))
    if ok then
      for _, f in ipairs(files) do
        ctx.push({ text = f, path = f })
      end
    end
  end),
  confirm = function(item, mode, layer)
    btv.picker.edit(item, mode, layer)
  end,
})

-- live_grep: a dynamic source — re-run per prompt edit, the matcher bypassed. Search
-- falls back through a chain so it works in every mode: `rg --vimgrep` (fast) →
-- `grep -rn` (any real shell lacking rg) → a transport-agnostic btv.fs walk + in-Lua
-- substring match. The binaries need a real shell, so the pure web client — where a
-- hostless spawn fails loud with code -1 — lands on the btv.fs match (which rides the
-- off-tick seam to OPFS). Each step runs only when the previous one could not RUN
-- (`stream:exit().code == -1`): zero matches is a legitimate ANSWER, not a missing
-- binary, and re-searching the tree for it would leave the previous query's rows on
-- screen for as long as the pointless re-searches took. The superseded job is reaped
-- via ctx.on_cancel, which also stops the chain dead.
--
-- Unrestricted by default, exactly like `files` above: `rg -uu` (`--no-ignore
-- --hidden`) minus `.git`, so an ignored or dotted file still matches. rg still skips
-- binaries (that needs a third `u`), and the `grep -rnI` step keeps the same shape.
--
-- Rows are **two-column** (`push { head = … }`): the location (`path:line:col: `)
-- and the matched line as the body, so the widget can keep the file name on screen
-- (it never drops below 40% of the list) and window the body around the hit instead
-- of showing a long line's head and nothing else. The body's leading indentation is
-- stripped — a deeply-indented hit would otherwise spend the row on whitespace —
-- and `match` carries the hit's char range so it highlights like a `files` fuzzy hit.
-- The include/exclude boxes (`<C-g>`) narrow a single search without changing that.
btv.picker.source({
  name = "live_grep",
  title = "Live Grep",
  layer = "main", -- a grep hit opens in the main editor, never a focused dock
  dynamic = true,
  filter = true, -- hits carry a path: give it the include/exclude boxes
  preview = "location", -- scroll the pane to the match and range-highlight it
  items = btv.async(function(ctx)
    if ctx.query == "" then
      return
    end
    local q = ctx.query
    local cancelled = false

    -- How far each hit RUNS: every leg reports where a match starts, none reports
    -- where it ends, and the row wants the whole hit highlighted — so the query is
    -- re-matched against the line to measure it. Which dialect depends on the leg, and
    -- a mismatched dialect could mark the wrong text: `rg` searches with the Rust
    -- `regex` crate, so its hits are measured with the same PCRE (`re`); `grep` (a BRE)
    -- and the `btv.fs` walk (a literal substring) are measured literally (`lit`), which
    -- agrees with them for any query without metacharacters and simply finds nothing —
    -- leaving the row unhighlighted rather than mismarked — for one with. Either is nil
    -- when the query doesn't compile (rg then errored out too).
    local function matcher(opts)
      local ok, r = pcall(btv.regex, q, opts)
      return ok and r or nil
    end
    local re, lit = matcher(), matcher({ plain = true })

    -- One parsed hit -> a two-column item: the `path:line:col: ` head, the matched
    -- line (leading indentation stripped) as the body, and the hit's char range within
    -- that body, measured with the leg's `find`er. `col` is 1-based and BYTE-based
    -- (rg's convention) and stays untouched on the item — it is what `confirm` jumps
    -- to; only the display body is trimmed.
    local function emit(find, file, lnum, col, line)
      local body = line:gsub("^%s+", "")
      local indent = #line - #body
      local m
      if find then
        -- Search from the reported column so a line with several hits highlights the
        -- one rg pointed at; a pattern that only matches from the line start (`^…`)
        -- falls back to a search from the beginning rather than losing its highlight.
        local at = math.max(1, (col or 1) - indent)
        local s, e = find:find(body, at)
        if not s and at > 1 then
          s, e = find:find(body)
        end
        if s then
          m = { charlen(body:sub(1, s - 1)) + 1, charlen(body:sub(1, e)) }
        end
      end
      ctx.push({
        head = file .. ":" .. lnum .. ":" .. (col or 1) .. ": ",
        text = body,
        match = m,
        path = file,
        row = tonumber(lnum),
        col = tonumber(col),
      })
    end

    -- Stream a grep-like command, parsing `file:lnum[:col]:text` per line; `has_col` for
    -- rg's `--vimgrep` column. `strip` drops grep's leading "./", and `find` is the
    -- matcher this tool's dialect is measured with. Returns whether the chain is SETTLED
    -- here — the tool ran (matches or not), or the run was superseded.
    local function run(cmd, args, has_col, strip, find)
      local stream = btv.run_stream({ cmd = cmd, args = args, cwd = ctx.cwd })
      ctx.on_cancel(function()
        cancelled = true
        stream:kill()
      end)
      for batch in btv.await_each(stream) do
        for _, l in ipairs(batch) do
          if strip then
            l = l:gsub("^%./", "")
          end
          local file, lnum, col, body
          if has_col then
            file, lnum, col, body = l:match("^(.-):(%d+):(%d+):(.*)$")
          else
            file, lnum, body = l:match("^(.-):(%d+):(.*)$")
            col = 1
          end
          if file then
            emit(find, file, lnum, tonumber(col), body)
          end
        end
      end
      -- The child's exit status is the canonical "did this tool exist" signal: `-1`
      -- is a spawn failure (no such binary) or our own `:kill()`. rg/grep exit `1`
      -- on zero matches — a real answer, so the chain stops and the picker settles
      -- empty instead of grinding the whole tree twice more.
      local exit = stream:exit()
      return cancelled or exit == nil or exit.code ~= -1
    end

    -- As in `files`: the boxes prune at rg, and the sink filters every leg's output.
    local grep_args = { "--vimgrep", "--color=never", "-uu", "-g", "!.git" }
    for _, a in ipairs(ctx.rg_globs) do
      grep_args[#grep_args + 1] = a
    end
    grep_args[#grep_args + 1] = "--"
    grep_args[#grep_args + 1] = q
    if run("rg", grep_args, true, false, re) then
      return
    end
    if run("grep", { "-rnI", "--exclude-dir=.git", "--", q, "." }, false, true, lit) then
      return
    end

    -- No shell / no rg / no grep (the pure web client): a transport-agnostic btv.fs walk
    -- + in-Lua substring match. Performance isn't a concern — the pure-client trees are
    -- small and the picker caps results; pushes for a superseded query are dropped by the
    -- sink.
    local ok, matches = pcall(btv.await, btv.fs.grep(ctx.cwd, q, { hidden = true }))
    if ok then
      for _, m in ipairs(matches) do
        emit(lit, m.path, m.row, m.col, m.text)
      end
    end
  end),
  confirm = function(item, mode, layer)
    btv.picker.edit(item, mode, layer)
  end,
})

-- buffers: a static, in-memory source — the focused layer's open buffers, no
-- process spawn. A plain synchronous source: it pushes in a loop and returns (no
-- promise needed — returning nil settles the run). Scoped to the focused layer
-- with `{ focused = true }`, exactly like `:ls`: the main area and each dock keep
-- disjoint buffer lists, so picking a buffer never yanks a document into a dock
-- (or vice versa). Names come from the authoritative buffer mirror (`btv._bufs`);
-- `btv.buf.name` short-circuits the *current* buffer to a separately-tracked field
-- that can lag, so reading the mirror lists every buffer including the focused one.
--
-- The listing matches `:ls` buffer for buffer — an unnamed one is a row too
-- (`[No Name]`, no path, so no preview), because a scratch buffer you can see in
-- `:ls` but can't pick is a hole, not a tidy list. Each row carries the same facts
-- `:ls` prints — the buffer number, the `%` current / `#` alternate flag, `a`ctive /
-- `h`idden, `+` modified, and the buffer's last cursor line — as a fixed-width
-- **head** column ahead of the name:
--
-- ```
--   3 %a + 12  /home/me/src/main.rs
--   2 #h    3  /home/me/README.md
--   1  h    1  [No Name]
-- ```
--
-- Laid out as a leading column rather than `:ls`'s trailing `"name" line N` for two
-- reasons: the widget aligns and always shows a declared head (a narrow list elides
-- the *body*, so the flags can't be the thing that scrolls off), and the label is
-- what the fuzzy matcher scores — a trailing `line 12` would put the letters of
-- "line" in every row, so typing `in` would match the whole buffer list. The row
-- also declares an empty `match` at the file's base name, which windows an
-- over-long path so the name stays visible without highlighting anything.
btv.picker.source({
  name = "buffers",
  title = "Buffers",
  layer = "active", -- scoped to the focused layer, so a pick stays in that layer
  preview = "file", -- preview the buffer's backing file (named buffers only)
  items = function(ctx)
    local bufs = btv._bufs or {}
    local cur, alt = btv.buf.current(), btv.buf.alternate()
    -- Collect first: the line column is sized to the widest line number in the list,
    -- so every name starts at the same cell (a 5-wide column for a 12-line file
    -- would be dead space in a narrow picker).
    local rows, lnum_w = {}, 1
    for _, b in ipairs(btv.buf.list({ focused = true })) do
      local entry = bufs[b]
      local name = (entry and entry.name) or btv.buf.name(b)
      local lnum = (entry and entry.lnum) or 1
      -- An unnamed buffer (`:enew`, the startup scratch) is listed as `[No Name]`,
      -- the way `:ls` lists it — the two surfaces show the same buffers, and a
      -- scratch buffer stays reachable from the picker. It carries no `path`, so the
      -- preview pane says "no preview" instead of reading a file that isn't there.
      local named = name ~= nil and name ~= ""
      rows[#rows + 1] = {
        bufnr = b,
        label = named and name or "[No Name]",
        path = named and name or nil,
        lnum = lnum,
      }
      lnum_w = math.max(lnum_w, #tostring(lnum))
    end
    for _, r in ipairs(rows) do
      local flag = (r.bufnr == cur and "%") or (r.bufnr == alt and "#") or " "
      -- `a`ctive is the current buffer, everything else `h`idden — the same rule the
      -- core's `:ls` applies, so the two listings never disagree about a row.
      local active = r.bufnr == cur and "a" or "h"
      local modified = btv.bo[r.bufnr].modified and "+" or " "
      -- The base name's 1-based CHAR offset, pushed as an empty `match` range: "the
      -- interesting part starts here" without a highlight, so a path too long for the
      -- list column keeps its tail instead of its leading directories.
      local at = charlen(r.label:match("^.*/") or "") + 1
      ctx.push({
        head = string.format(
          "%3d %s%s %s %" .. lnum_w .. "d  ",
          r.bufnr,
          flag,
          active,
          modified,
          r.lnum
        ),
        text = r.label,
        match = { at, at - 1 },
        bufnr = r.bufnr,
        path = r.path,
      })
    end
  end,
  confirm = function(item, mode, layer)
    -- The mode rides the bridge: "current" honors 'switchbuf' (a buffer shown in
    -- another tab is focused there under the default `usetab`); "tab"/"split"/
    -- "vsplit" (`<C-t>`/`<C-x>`/`<C-v>`) open it in a new tab / split regardless.
    -- `layer == "main"` would cross out of a dock first; this source is "active", so
    -- a pick stays in the focused layer — but an override is honored for symmetry.
    btv._buf_switch(item.bufnr, mode, layer == "main")
  end,
})

-- curbuf: fuzzy-find a line in the *current* buffer (telescope's
-- `current_buffer_fuzzy_find`). A static, in-memory source over the focused
-- buffer's lines (read from the `btv.buf` mirror, never live state); each item
-- carries its 1-based `row` and confirm just moves the cursor there — no path, so
-- it works for an unnamed buffer too, and there is no preview pane (the line is
-- already on screen). Read `btv.buf.current()` at open, before the picker float
-- grabs input, so it is the underlying buffer, not the prompt.
btv.picker.source({
  name = "curbuf",
  title = "Buffer Lines",
  layer = "main",
  items = function(ctx)
    for i, line in ipairs(btv.buf.lines(btv.buf.current(), 0, -1)) do
      -- A real two-column row rather than a padded prefix inside one string: the
      -- widget aligns the line-number column itself (to the widest number actually
      -- listed) and keeps it when a long line overflows, instead of the whole row
      -- eliding tail-first and leaving a fragment of text with no line number.
      ctx.push({ head = string.format("%d: ", i), text = line, row = i })
    end
  end,
  confirm = function(item)
    btv.pos.set(".", { 0, item.row, 1 })
  end,
})

-- Relativize `path` against the cwd for a compact diagnostics label; the cwd is
-- escaped so a path-magic char in it (`.`, `-`, `(`, …) can't corrupt the anchor.
local function relpath(path)
  local cwd = vim.fn.getcwd()
  local anchor = cwd:gsub("[%(%)%.%%%+%-%*%?%[%]%^%$]", "%%%1")
  return (path or ""):gsub("^" .. anchor .. "/", "")
end

-- The one-letter tag, the standard highlight group, and the private fallback group
-- each severity renders with, indexed by the numeric severity (1=ERROR … 4=HINT). The
-- letter keeps the classification in a fixed-width column so the paths below it line
-- up.
--
-- Two group names because the standard `Diagnostic*` ones are a *colorscheme's* to
-- define: a theme that styles them wins (as it should), but a bare session with no
-- colorscheme defines none of them, and a severity column that paints nothing is the
-- one thing this row must not be. So each severity also has a picker-private group
-- filled from `btv.hl.palette()` — the active theme's own hues, whatever it is — used
-- only when the standard name is undefined.
local SEVERITY_ROW = {
  [1] = { "E", "DiagnosticError", "BtvPickerDiagError", "red" },
  [2] = { "W", "DiagnosticWarn", "BtvPickerDiagWarn", "yellow" },
  [3] = { "I", "DiagnosticInfo", "BtvPickerDiagInfo", "blue" },
  [4] = { "H", "DiagnosticHint", "BtvPickerDiagHint", "cyan" },
}

-- (Re)derive the private severity colors from the running colorscheme. Called once per
-- picker run rather than wired to `ColorScheme` — the source re-runs on every open, so
-- a re-derivation there always reflects the theme in force, with no subscription to
-- keep in step (and no `btv.on` at prelude-load time, which this chunk predates).
-- `btv.hl.fallback` yields to any theme or user definition of these names, and the
-- private names are deliberately absent from `btv.hl.palette`'s own lookup chains —
-- deriving a default from a group the palette reads back would feed the previous
-- theme's color into the next one.
local function paint_severity_fallbacks()
  local p = btv.hl.palette()
  for _, sev in ipairs(SEVERITY_ROW) do
    btv.hl.fallback(sev[3], { fg = p[sev[4]] })
  end
end

-- The group a severity's row paints with: the colorscheme's `Diagnostic*` when it
-- defines one, else this picker's theme-derived stand-in.
local function severity_hl(sev)
  if next(btv.hl.get(0, { name = sev[2] })) ~= nil then
    return sev[2]
  end
  return sev[3]
end

-- Collapse a diagnostic message onto ONE line: a compiler's message is routinely
-- multi-line (rustc's "expected X, found Y" continuations) and a row is one line, so
-- an embedded newline would otherwise render as a stray glyph mid-row and push the
-- rest of the text off. Runs of whitespace collapse to a single space, ends trimmed.
local function one_line(msg)
  return (tostring(msg or ""):gsub("%s+", " "):gsub("^ ", ""):gsub(" $", ""))
end

-- diagnostics: every diagnostic across all buffers (telescope's `diagnostics`), the
-- merged `btv.diagnostic.get()` set — LSP-pushed plus every client namespace. Static
-- and in-memory; `location` preview scrolls to and highlights the match, confirm
-- jumps via `btv.picker.edit`. Diagnostic records are 0-based (`lnum`/`col`), so the
-- pushed item's `row`/`col` add 1 to reach the picker's 1-based convention.
--
-- Rows are **two-column** and severity-colored: the head is the classification (a
-- pinned `tag`) plus the location (`E src/main.rs:12:5 `), and the body is the message,
-- prefixed with the diagnostic's `source` (`ty`, `rustc`, `eslint`) when there is one. The
-- head is what a client keeps and aligns when the row overflows, so a long message can
-- no longer crop the row down to a floating fragment of text with no severity, file or
-- line in front of it; `hl` paints that head in the severity's color.
--
-- The list is ordered errors-first, then by file and line. `btv.diagnostic.get()`
-- walks a bufnr-keyed table, so without this the rows arrive in an arbitrary (and
-- run-to-run unstable) order — and the diagnostics worth jumping to first are the
-- errors.
btv.picker.source({
  name = "diagnostics",
  title = "Diagnostics",
  layer = "main",
  preview = "location",
  items = function(ctx)
    paint_severity_fallbacks()
    local all = btv.diagnostic.get()
    for _, d in ipairs(all) do
      d._name = btv.buf.name(d.bufnr) or ""
    end
    table.sort(all, function(a, b)
      if a.severity ~= b.severity then
        return (a.severity or 9) < (b.severity or 9)
      end
      if a._name ~= b._name then
        return a._name < b._name
      end
      if a.lnum ~= b.lnum then
        return (a.lnum or 0) < (b.lnum or 0)
      end
      return (a.col or 0) < (b.col or 0)
    end)
    for _, d in ipairs(all) do
      local sev = SEVERITY_ROW[d.severity]
      -- An unnamed buffer (a plugin's scratch surface carrying client-set
      -- diagnostics) has no path to relativize — name it by its number rather than
      -- leaving the location column blank.
      local where = d._name ~= "" and relpath(d._name) or ("[buf " .. d.bufnr .. "]")
      local msg = one_line(d.message)
      -- 0-based on the record, 1-based in the row and in what `confirm` jumps to.
      local row, col = (d.lnum or 0) + 1, (d.col or 0) + 1
      ctx.push({
        tag = sev and sev[1] or "?",
        head = string.format("%s:%d:%d ", where, row, col),
        text = d.source and (d.source .. ": " .. msg) or msg,
        hl = sev and severity_hl(sev) or nil,
        path = d._name,
        row = row,
        col = col,
      })
    end
  end,
  confirm = function(item, mode, layer)
    btv.picker.edit(item, mode, layer)
  end,
})

-- keymaps: every mapping that applies right now, across normal / visual / insert
-- mode (telescope's `keymaps`). This lists BOTH the current buffer's buffer-local
-- maps AND the global ones — so a plugin's on-buffer bindings are discoverable while
-- its buffer is focused (e.g. focus the bemtvi-tree sidebar and `a` shows as
-- "bemtvi-tree: Create a file"). A buffer-local map shadows a global one at the same
-- lhs+mode, so only the binding that would actually fire is shown; buffer-local rows
-- are flagged with a `@` marker in the mode column.
--
-- Filter-by-plugin is a `desc` convention rather than a new registry field: a plugin
-- prefixes each description with `"<plugin>: "` (`bemtvi-tree`, `btv.complete`, …), so
-- typing the plugin name in the prompt narrows the list to its maps — the prefix is
-- part of the fuzzy-matched row text.
--
-- Each row is `<mode><@> <lhs>  <description>` in real columns: the mode (with the
-- buffer-local `@` marker) is the pinned `tag`, the `lhs` the aligned `head`, and the
-- description the body. A mapping with an unusually long `lhs` therefore can't push
-- every other row's description out of line, which a padded prefix could not prevent.
--
-- The displayed `lhs` runs through `btv.keytrans` so special keys read as notation (a
-- space leader shows `<Space>`, not a hard-to-see literal blank; likewise `<Tab>`,
-- `<C-x>`, …); the raw `lhs` is kept for the confirm feed. Confirm re-feeds it with
-- remapping on, so picking a mapping *runs* it, exactly like telescope.
btv.picker.source({
  name = "keymaps",
  title = "Keymaps",
  layer = "main",
  items = function(ctx)
    local buf = btv._resolve_bufnr(0)
    local function push(mode, k, local_marker)
      -- The mode (plus the `@` buffer-local marker) is the row's `tag` — pinned, so
      -- the one thing that says WHEN a mapping applies can't elide — and the `lhs` is
      -- the head column the widget aligns. The old `%-16s` padded the lhs but never
      -- truncated it, so one long mapping knocked every description out of line.
      ctx.push({
        tag = mode .. local_marker,
        head = btv.keytrans(k.lhs) .. " ",
        text = k.desc or "",
        lhs = k.lhs,
      })
    end
    for _, mode in ipairs({ "n", "v", "i" }) do
      -- Buffer-local first, recording each lhs so the matching global is skipped.
      local shadowed = {}
      for _, k in ipairs(btv.keymap.buf_get(buf, mode)) do
        shadowed[k.lhs] = true
        push(mode, k, "@")
      end
      for _, k in ipairs(btv.keymap.get(mode)) do
        if not shadowed[k.lhs] then
          push(mode, k, " ")
        end
      end
    end
  end,
  confirm = function(item)
    btv._feedkeys(item.lhs, true, false)
  end,
})

-- pickers: a picker of pickers (telescope's `builtin`) — every registered source
-- name, confirm opens the chosen one. Opening a picker from inside a `confirm` has
-- to wait for this picker to tear down first, so it defers to `btv.on_next_tick`.
btv.picker.source({
  name = "pickers",
  title = "Pickers",
  layer = "main",
  items = function(ctx)
    local names = {}
    for name in pairs(btv.picker._sources) do
      names[#names + 1] = name
    end
    table.sort(names)
    for _, name in ipairs(names) do
      ctx.push({ text = name, source = name })
    end
  end,
  confirm = function(item)
    btv.on_next_tick(function()
      btv.picker.open(item.source)
    end)
  end,
})

-- marks: the set marks (telescope's `marks`), read from `btv.mark.list` — the
-- current buffer's specials + `a`–`z`, the globals `A`–`Z`, then the numbered
-- `0`–`9`. `location` preview scrolls to the mark; confirm jumps via
-- `btv.picker.edit` when the mark names a file, or moves the cursor directly for a
-- mark in an unnamed current buffer (no path to open). Mirror positions are 0-based,
-- so the pushed item's `row`/`col` add 1.
--
-- Each row is `<mark> <line>:<col>  <the line's text>` in real columns: the mark name
-- is the pinned `tag` (the one character you scan the list by), the position is the
-- aligned `head`, and the text is the body — so a long line crops the TEXT, never the
-- mark it belongs to.
btv.picker.source({
  name = "marks",
  title = "Marks",
  layer = "main",
  preview = "location",
  items = function(ctx)
    for _, m in ipairs(btv.mark.list()) do
      -- The mark's name is its `tag` — the one character the row is *scanned* by, so
      -- it is pinned through any elision — and the location is the head the widget
      -- aligns; the line's text is the body, windowed like any other content column.
      ctx.push({
        tag = m.name,
        head = string.format("%d:%d ", m.line + 1, m.col),
        text = m.text,
        path = m.path,
        row = m.line + 1,
        col = m.col + 1,
      })
    end
  end,
  confirm = function(item, mode, layer)
    if item.path ~= "" then
      btv.picker.edit(item, mode, layer)
    else
      btv.pos.set(".", { 0, item.row, item.col })
    end
  end,
})

-- jumplist: the focused window's jump history (telescope's `jumplist`), read from
-- `btv.jumplist.get` — the same `<C-o>`/`<C-i>` list `:jumps` shows. Listed
-- newest-first (the freshest jump on top, as telescope does), so item 1 is where a
-- single `<C-o>` would take you. Like `:jumps`, an entry in the *current* buffer
-- shows its line's text; one in another buffer shows the file name (arbitrary
-- buffers' lines aren't mirrored). `location` preview scrolls to the entry; confirm
-- jumps via `btv.picker.edit` when the entry names a file, else moves the cursor
-- directly for a mark in an unnamed current buffer. Mirror entries are 1-based
-- `lnum` / 0-based `col`, so the pushed item's `col` adds 1.
--
-- The `<line>:<col> ` position is a declared `head` column, aligned by the widget and
-- kept when the detail overflows — not a padded prefix inside one label.
btv.picker.source({
  name = "jumplist",
  title = "Jumplist",
  layer = "main",
  preview = "location",
  items = function(ctx)
    local cur = btv.buf.current()
    local curlines = btv.buf.lines(cur, 0, -1)
    local list = btv.jumplist.get()[1]
    -- Newest-first: walk the oldest-first mirror in reverse.
    for i = #list, 1, -1 do
      local e = list[i]
      local path = btv.buf.name(e.bufnr)
      local detail
      if e.bufnr == cur then
        detail = (curlines[e.lnum] or ""):gsub("%s+$", "")
      else
        detail = path ~= "" and path or "[No Name]"
      end
      -- The location is a real head column (aligned by the widget, kept when the
      -- detail overflows), not a padded prefix inside the label.
      ctx.push({
        head = string.format("%d:%d ", e.lnum, e.col),
        text = detail,
        path = path,
        row = e.lnum,
        col = e.col + 1,
      })
    end
  end,
  confirm = function(item, mode, layer)
    if item.path ~= "" then
      btv.picker.edit(item, mode, layer)
    else
      btv.pos.set(".", { 0, item.row, item.col })
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
-- Use `btv.autocmd.create` directly, not the `btv.on` sugar: this module loads
-- before btv.lua (where `btv.on` is defined), but autocmd.lua is already in.
btv.autocmd.create("VimEnter", {
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
      btv.keymap.set("n", m[1], function()
        btv.picker.open(source)
      end, { default = true, desc = m[3] })
    end
    -- `<leader>fr` reopens the last picker where you left off (telescope's `resume`).
    btv.keymap.set("n", "<leader>fr", function()
      btv.picker.resume()
    end, { default = true, desc = "Resume last picker" })
  end,
})

-- `btv.picker.scorer(src)`: install a **re-ranker** over a picker's surviving
-- rows, or clear it with `nil`.
--
-- `src` is a string of Lua *source* — an expression, not a function value —
-- because the re-ranker runs in the bounded compute sandbox: a second, pure VM
-- with a wall-clock deadline, no editor state and no `btv.*`. A closure cannot
-- cross between VMs, so the source crosses instead and is compiled there.
--
-- Three names are in scope, and the expression returns a number — the new sort
-- key, **higher first**:
--
-- ```
-- label   the row's text
-- query   the active query
-- score   the native fuzzy score this row already earned
-- ```
--
--
-- The sandbox is **stateless**: nothing carries from one call to the next, and
-- assigning a global raises. That is deliberate — no call shape is a clean
-- once-per-item traversal (`:s` re-runs on every keystroke of the live preview,
-- a foldexpr sees only the rows an edit touched, the picker scorer only the top
-- survivors, `foldtext` is memoized), so an accumulator would be quietly wrong.
-- Because `score` is handed in, a scorer *nudges* the native order rather than
-- reinventing matching:
--
-- ```lua
-- -- push test files down, keep everything else as the matcher ranked it
-- btv.picker.scorer([[ score - (label:find("/test") and 50 or 0) ]])
--
-- btv.picker.scorer(nil)   -- back to pure fuzzy order
-- ```
--
-- It is applied to the **filtered** rows only, never to every candidate, and to
-- at most the top 1000 of them — a picker streams 100k+ candidates and scoring
-- them all would freeze the editor. Rows past that keep native order. The
-- scorer also runs at most once per repaint, not once per streamed batch.
--
-- A scorer that errors, exceeds its deadline, or returns a non-number reports
-- once and is then uninstalled, rather than repeating the error every frame.
function btv.picker.scorer(src)
  if src ~= nil and type(src) ~= "string" then
    error("btv.picker.scorer: expected a string of Lua source (or nil), got " .. type(src), 2)
  end
  btv._picker_set_scorer(src)
end
