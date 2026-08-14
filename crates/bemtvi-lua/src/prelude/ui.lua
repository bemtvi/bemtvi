-- bemtvi Lua prelude — the async UI surface (`btv.ui`).
-- The four small async UI primitives the native-plugin-API names (`btv.ui.input` /
-- select / confirm / float — docs/specs/2026-06-11-native-plugin-api.md). None
-- blocks (ADR 0002 rule 3): each returns at once and the result arrives on a later
-- tick. (The deferral primitives `btv.schedule` / `btv.timer` live in prelude/runtime.lua.)
--   `btv.ui.input`   — a one-line text prompt over the editor's command line.
--                   PROMISE-ONLY: `btv.ui.input(opts)` -> a promise of the text.
--   `btv.ui.select`  — a chooser over the floating selectable-list widget, one
--                   consumer of the server's shared float layer
--                   (docs/specs/2026-06-14-btv-ui-float-widget.md). PROMISE-ONLY:
--                   `btv.ui.select(items, opts)` -> a promise of the chosen item.
--   `btv.ui.confirm` — a yes/no confirmation over the command line (btv-native; neovim
--                   spells this blocking `vim.fn.confirm`, which the btv model omits).
--                   PROMISE-ONLY: `btv.ui.confirm(message, opts)` -> a promise of bool.
--   `btv.ui.float`   — the list-less content float (the widget's sibling, also the LSP
--                   hover surface). Fire-and-forget by default (no result); with
--                   `persist = true` it returns a resource HANDLE (`:update`/`:close`/
--                   `:is_open`), not an async result — so it stays non-promise.
--   `btv.ui.open`    — hand a path/URL to the OS opener (`open` / `explorer` / `xdg-open`).
--                   PROMISE-ONLY: `btv.ui.open(uri)` -> a promise of the run result.
-- `btv` async is promise-only (ADR 0002 / docs/plans/2026-06-16-btv-promise-only-async.md):
-- a one-shot async API returns a promise, never a callback. The callback shape lives
-- on the `vim.ui.*` muscle-memory aliases (the bounded compat layer), which adapt the
-- promise back to neovim's `on_confirm` / `on_choice` signatures. input and select map the
-- chosen value back through `btv._cb_fns`; confirm shares the command-line prompt plumbing
-- with input (one prompt open at a time). The `btv.validate` / `btv.deprecate` no-ops are not
-- part of bemtvi's config API and remain intentionally absent.
local vim = vim
btv = btv or {}
btv.ui = btv.ui or {}
vim.ui = vim.ui or {}

-- ----- btv.ui.select [alias vim.ui.select] ------------------------------------
-- The shared core: open a floating selectable list and call cb(item, index) with
-- the chosen element and its 1-based index — or cb(nil, nil) on cancel. The server
-- owns the widget, its navigation, and the input grab; Lua only renders the display
-- labels (opts.format_item, default tostring) up front and maps the chosen index
-- back to the original item, so an arbitrary item table round-trips even though only
-- strings cross the bridge. Non-blocking (ADR 0002 rule 3): the menu opens at once
-- and cb fires on a later tick. Both the promise-shaped btv.ui.select and the
-- callback-shaped vim.ui.select alias build on this.
local function select_into(items, opts, cb)
  opts = opts or {}
  if type(items) ~= "table" then
    error("btv.ui.select: items must be a list", 2)
  end
  local format_item = opts.format_item or tostring
  local labels = {}
  for i, item in ipairs(items) do
    labels[i] = tostring(format_item(item))
  end
  -- An empty list has nothing to choose: cancel without opening a menu.
  if #labels == 0 then
    return cb(nil, nil)
  end
  local id = btv._next_cb_id()
  btv._cb_fns[id] = function(idx)
    -- idx: the 1-based chosen index, or nil on cancel.
    if idx == nil then
      return cb(nil, nil)
    end
    return cb(items[idx], idx)
  end
  btv._bridge(id, function()
    btv._ui_select(labels, opts.prompt or "", id)
  end)
end

-- `btv.ui.select(items, opts)` -> a PROMISE that resolves to the chosen item, or to
-- nil on cancel (<Esc> / q). Promise-only: there is no `on_choice` argument (passing
-- one is the old callback shape and errors loudly). The 1-based index is dropped —
-- recover it from the item, or use the `vim.ui.select` alias, which keeps it.
--
-- opts:
--   * `prompt` — the label drawn above the list (default none).
--   * `format_item` — an `item -> display string` mapper (default `tostring`); the
--     original item round-trips to the resolved value regardless.
function btv.ui.select(items, opts, on_choice)
  if on_choice ~= nil then
    error("btv.ui.select is promise-only: btv.ui.select(items, opts):next(fn)", 2)
  end
  if type(items) ~= "table" then
    error("btv.ui.select: items must be a list", 2)
  end
  return btv.promise.new(function(resolve)
    -- cb(item, index) -> resolve(item); the index falls away (resolve is 1-arg).
    select_into(items, opts, resolve)
  end)
end

-- `vim.ui.select(items, opts, on_choice)`: neovim's callback-shaped alias (ADR 0002
-- whitelist) — `on_choice(item, index)`, or `on_choice(nil, nil)` on cancel. Kept on the
-- compat layer so plugins (telescope, …) that pass a callback and read the index
-- still work; btv code uses the promise form above.
function vim.ui.select(items, opts, on_choice)
  select_into(items, opts, on_choice or function() end)
end

-- ----- rebindable select keys -----------------------------------------------
-- Like the picker, a promptless `btv.ui.select` list is driven through the keymap
-- engine, NOT a hardcoded grab: the server selects the `select` bucket while the
-- list owns input, so navigation / confirm / cancel are configurable with
-- `btv.keymap.set('select', '<key>', btv.ui.select_actions.<name>)`. Each action fires
-- through the engine (`btv._select_action` -> `Editor::apply_select_action`). A select
-- list has NO query, so there is no text fallthrough — an unmapped key is inert.
btv.ui.select_actions = btv.ui.select_actions or {}
for _, name in ipairs({ "next", "prev", "first", "last", "confirm", "cancel" }) do
  btv.ui.select_actions[name] = function()
    btv._select_action(name)
  end
end

-- The default select bindings — `default = true` so a user `btv.keymap.set('select', …)`
-- wins, and an empty-function map disables a key. `gg` is a two-key default map
-- (the multi-key widget map the same trie handles); these mirror the vim-style list
-- keys select used to hardcode.
for _, m in ipairs({
  { "<C-n>", "next", "Next item" },
  { "<Down>", "next", "Next item" },
  { "j", "next", "Next item" },
  { "<C-p>", "prev", "Previous item" },
  { "<Up>", "prev", "Previous item" },
  { "k", "prev", "Previous item" },
  { "gg", "first", "First item" },
  { "<Home>", "first", "First item" },
  { "G", "last", "Last item" },
  { "<End>", "last", "Last item" },
  { "<CR>", "confirm", "Confirm selection" },
  { "<Esc>", "cancel", "Cancel" },
  { "q", "cancel", "Cancel" },
}) do
  btv.keymap.set("select", m[1], btv.ui.select_actions[m[2]], { default = true, desc = m[3] })
end

-- ----- btv.ui.input [alias vim.ui.input] --------------------------------------
-- `btv.ui.input(opts)` -> a PROMISE that resolves to the entered string on <CR>, or
-- to nil on <Esc> (cancel). Promise-only: there is no `on_confirm` argument (passing
-- one is the old callback shape and errors loudly).
--
-- opts:
--   * `prompt` — the label drawn ahead of the editable line (default `""`).
--   * `default` — text prefilled into the line, cursor at its end (default `""`).
--   * `history` — a namespace string enabling readline-style recall: `<Up>`/`<Down>`
--     (and `<C-p>`/`<C-n>`) browse the prompts submitted under this namespace, and each
--     non-empty submission is recorded into it. Each namespace is an independent ring,
--     so one plugin's REPL history is separate from another's. Session-only for now.
--     Absent ⇒ no history.
--   * `complete` — a function `(line, col) -> candidates` driving `<Tab>` autocomplete
--     (the inline wildmenu above the prompt line — `<Tab>`/`<S-Tab>` cycle, `<CR>`
--     accepts). `candidates` is a `{ {label, insert?, doc?, start?, length?}, … }` list
--     (`insert` defaults to `label`), OR a PROMISE of one — so an async source (e.g. a
--     DAP `completions` request) works. The token completed is the trailing identifier
--     run before the cursor, UNLESS a candidate supplies `start` (0-based char offset
--     into the line) + `length` (chars), an explicit replace span that overrides the
--     token for that row. `col` is the cursor's 0-based char offset.
--   * `complete_docs` — show the side docs pane rendering each candidate's `doc` beside
--     the list (default true when `complete` is set; `false` suppresses it).
--   * `complete_debounce` — ms to coalesce refresh queries (narrowing an open menu as
--     you type) so an async source isn't a wire round-trip per keystroke; the initial
--     `<Tab>` is always immediate (default 100; `0` disables it).
--
-- The server owns the prompt: it opens the editor's command line as a labelled
-- Prompt (`Editor::open_prompt`), and delivers the result to `btv._cb_fns[id]` through
-- the shared `prompt_results` channel. Non-blocking (ADR 0002 rule 3): the call
-- returns at once and the promise settles on a later tick. Note an empty submission
-- (<CR> on an empty line) resolves to `""` (not nil) — only <Esc> cancels, matching
-- neovim's `vim.ui.input`.
function btv.ui.input(opts, on_confirm)
  if on_confirm ~= nil then
    error("btv.ui.input is promise-only: btv.ui.input(opts):next(fn)", 2)
  end
  opts = opts or {}
  if type(opts) ~= "table" then
    error("btv.ui.input: opts must be a table", 2)
  end
  local history = opts.history
  if history ~= nil and type(history) ~= "string" then
    error("btv.ui.input: opts.history must be a string namespace", 2)
  end
  local complete = opts.complete
  if complete ~= nil and type(complete) ~= "function" then
    error("btv.ui.input: opts.complete must be a function", 2)
  end
  -- The side docs pane defaults ON when a completion source is given (so a candidate
  -- carrying a `doc` shows it); pass `complete_docs = false` to suppress it.
  local complete_docs = complete ~= nil and opts.complete_docs ~= false
  -- Refresh queries (narrowing an open menu as you type) coalesce through this debounce
  -- so an async source isn't a wire round-trip per keystroke; the initial <Tab> is
  -- always immediate. Default 100ms; `0` disables it (re-query every edit).
  local debounce_ms = opts.complete_debounce
  if debounce_ms ~= nil and (type(debounce_ms) ~= "number" or debounce_ms < 0) then
    error("btv.ui.input: opts.complete_debounce must be a non-negative number (ms)", 2)
  end
  debounce_ms = debounce_ms or 100
  return btv.promise.new(function(resolve)
    local id = btv._next_cb_id()
    -- text: the entered string ("" on an empty <CR>), or nil on cancel. Tear down the
    -- active completion source + its debounce on settle so neither leaks into a later
    -- prompt (only one prompt is open at a time).
    btv._cb_fns[id] = function(text)
      if btv._prompt_complete_debounced then
        btv._prompt_complete_debounced:cancel()
      end
      btv._active_prompt_complete = nil
      btv._prompt_complete_debounced = nil
      resolve(text)
    end
    btv._active_prompt_complete = complete
    -- A per-prompt debounced wrapper around the source (latest-args, trailing edge).
    -- nil when there's no source or the user disabled it (`complete_debounce = 0`).
    btv._prompt_complete_debounced = (complete and debounce_ms > 0)
        and btv.utils.debounce(function(line, col)
          btv._do_prompt_complete(complete, line, col)
        end, debounce_ms)
      or nil
    btv._bridge(id, function()
      btv._ui_input(
        tostring(opts.prompt or ""),
        tostring(opts.default or ""),
        id,
        history,
        complete ~= nil,
        complete_docs
      )
    end)
  end)
end

-- The open prompt's `complete` source (`btv.ui.input{ complete = fn }`) and its
-- per-prompt debounced wrapper, or nil when the prompt opted into none / no debounce.
-- One prompt is open at a time, so single slots hold them; cleared when it settles.
btv._active_prompt_complete = btv._active_prompt_complete or nil
btv._prompt_complete_debounced = btv._prompt_complete_debounced or nil

-- Run the source once and pipe its candidates (a `{ {label, insert?, doc?}, … }` list
-- OR a promise of one — an async source like the DAP `completions` round-trip) into
-- the wildmenu via `btv._prompt_complete_show`. `btv.promise.resolve` adapts the sync
-- and async shapes uniformly; a nil result or an error resolves to an empty list (the
-- menu just closes — a completion hiccup must not break the prompt). Shared by the
-- immediate path and the debounced refresh path.
function btv._do_prompt_complete(fn, line, col)
  -- `try` folds a synchronous throw from `fn` into a rejection, so the error
  -- handler below still closes the menu (a completion hiccup must not break the
  -- prompt) instead of the throw escaping the callback uncaught.
  btv.promise.try(fn, line, col):next(function(cands)
    btv._prompt_complete_show(cands or {})
  end, function()
    btv._prompt_complete_show({})
  end)
end

-- `btv._run_prompt_complete(line, col, refresh)`: drive the open prompt's `complete`
-- source for the `<Tab>` wildmenu. The server calls this when core stamps a
-- prompt-completion request. The initial open (`refresh = false`) queries at once for
-- a snappy menu; an edit narrowing the open menu (`refresh = true`) coalesces through
-- the prompt's debounce so a rapid burst of keystrokes is one query, not one per key.
function btv._run_prompt_complete(line, col, refresh)
  local fn = btv._active_prompt_complete
  if not fn then
    btv._prompt_complete_show({})
    return
  end
  if refresh and btv._prompt_complete_debounced then
    btv._prompt_complete_debounced(line, col)
  else
    btv._do_prompt_complete(fn, line, col)
  end
end

-- `vim.ui.input(opts, on_confirm)`: neovim's callback-shaped alias (ADR 0002
-- whitelist) — `on_confirm(text)` on <CR>, `on_confirm(nil)` on cancel. Kept on the
-- compat layer for plugins; btv code uses the promise form above.
function vim.ui.input(opts, on_confirm)
  on_confirm = on_confirm or function() end
  btv.ui.input(opts):next(on_confirm)
end

-- ----- btv.ui.confirm ---------------------------------------------------------
-- `btv.ui.confirm(message, opts)` -> a PROMISE that resolves to a boolean — true on
-- Yes, false on No or cancel (<Esc>). Promise-only: there is no `on_choice` argument
-- (the old callback forms — a third arg, or opts-as-function — error loudly).
--
-- opts (optional):
--   * `default` — `true` | `false`, which button `<CR>` selects (default `true` = Yes).
--
-- btv-native (no `vim.ui` twin): neovim spells this blocking `vim.fn.confirm`, which the
-- btv model omits (rule 3). For an arbitrary multi-choice menu use `btv.ui.select`
-- instead — confirm is deliberately just yes/no. The server opens a single-keypress
-- Confirm dialog (`Editor::open_confirm`) sharing the `prompt_results` channel with
-- `btv.ui.input` (one prompt open at a time); the chosen 1-based button index arrives
-- as a string, which the wrapper folds to the boolean (1 = Yes; 2 = No; 0 = cancel).
function btv.ui.confirm(message, opts, on_choice)
  if type(opts) == "function" or on_choice ~= nil then
    error("btv.ui.confirm is promise-only: btv.ui.confirm(message, opts):next(fn)", 2)
  end
  opts = opts or {}
  if type(message) ~= "string" then
    error("btv.ui.confirm: message must be a string", 2)
  end
  -- Default to Yes (<CR> accepts), unless opts.default is explicitly false.
  local default_yes = opts.default ~= false
  -- A shell-style hint: the default button is upper-cased.
  local hint = default_yes and "[Y/n]" or "[y/N]"
  local label = message .. " " .. hint .. " "
  return btv.promise.new(function(resolve)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(idx_str)
      -- idx_str: the chosen 1-based button index as a string ("0" = Esc-cancel).
      resolve(tonumber(idx_str) == 1)
    end
    -- accelerators are matched lowercase against the keypress, in button order.
    btv._bridge(id, function()
      btv._confirm(label, { "y", "n" }, default_yes and 1 or 2, id)
    end)
  end)
end

-- ----- btv.ui.float -----------------------------------------------------------
-- The "chunk" form a styled line is built from: `{ text, hl_group? }`, exactly
-- neovim's virt_text / nvim_echo chunk (a string + an optional highlight group).
-- A line is a LIST of these chunks, so one row can colour the key one group and
-- its description another. A plain string row is normalized to a single un-grouped
-- chunk, so a renderer that ignores styling still shows the text.
--
-- Detect a chunk list (`{ {text, hl}, … }`) vs. a plain row: a chunk list's first
-- element is itself a table whose [1] is a string. An empty table is treated as a
-- (blank) chunk list — it renders nothing either way.
local function is_chunk_list(l)
  if type(l) ~= "table" then
    return false
  end
  if l[1] == nil then
    return true
  end
  return type(l[1]) == "table"
end

-- Normalize one chunk list into `{ {text, hl}, … }` with string text and a string
-- (or nil) group, erroring loud on a malformed chunk — the server parses the same
-- shape as extmark virt_text.
local function normalize_chunks(l)
  local chunks = {}
  for i, c in ipairs(l) do
    if type(c) ~= "table" or type(c[1]) ~= "string" then
      error("btv.ui.float: a styled line must be a list of { text, hl_group? } chunks", 3)
    end
    if c[2] ~= nil and type(c[2]) ~= "string" then
      error("btv.ui.float: a chunk's hl_group must be a string", 3)
    end
    chunks[i] = { c[1], c[2] }
  end
  return chunks
end

-- The concatenated plain text of a normalized chunk line (for the trailing-blank
-- drop below).
local function chunk_text(chunks)
  local parts = {}
  for i, c in ipairs(chunks) do
    parts[i] = c[1]
  end
  return table.concat(parts)
end

-- Normalize `contents` into a list of chunk LINES (each `{ {text, hl}, … }`),
-- dropping a single trailing empty line so a markdown body ending in `"\n"` doesn't
-- render a blank last row. Accepts:
--   * a string                  → split on newlines, each a single plain chunk
--   * a list of strings         → each a single plain chunk
--   * a list of chunk lists     → styled rows, passed through (key/desc colours)
--   * a mix of the two row forms
-- Errors loud on a bad type.
local function float_lines(contents)
  local lines
  if type(contents) == "string" then
    lines = {}
    for i, s in ipairs(vim.split(contents, "\n", { plain = true })) do
      lines[i] = { { s } }
    end
  elseif type(contents) == "table" then
    lines = {}
    for i, l in ipairs(contents) do
      if type(l) == "string" then
        lines[i] = { { l } }
      elseif is_chunk_list(l) then
        lines[i] = normalize_chunks(l)
      else
        lines[i] = { { tostring(l) } }
      end
    end
  else
    error("btv.ui.float: contents must be a string or a list of strings/chunk lines", 2)
  end
  if #lines > 1 and chunk_text(lines[#lines]) == "" then
    lines[#lines] = nil
  end
  return lines
end

-- The persistent-float handle returned by `btv.ui.float{ persist = true }`. It
-- owns a server-side content float by id: `:update` replaces its content in place
-- (same id), `:close` dismisses it, `:is_open` reports whether this handle still
-- owns the open float. `btv._float_open_id` tracks which persistent float the
-- server last opened, so a stale handle (its float replaced by a newer persistent
-- one) reports `is_open() == false` without a server round-trip.
btv._float_open_id = btv._float_open_id or nil
btv._next_float_id = btv._next_float_id or 0
local float_handle = {}
float_handle.__index = float_handle
function float_handle:update(contents, opts)
  opts = opts or {}
  local lines = float_lines(contents)
  if #lines == 0 then
    return self:close()
  end
  btv._float_open_id = self._id
  btv._ui_float(self._id, lines, opts.title, opts.border or "rounded", opts.relative or "cursor")
end
function float_handle:close()
  if btv._float_open_id == self._id then
    btv._float_open_id = nil
  end
  btv._ui_float_close(self._id)
end
function float_handle:is_open()
  return btv._float_open_id == self._id
end

-- `btv.ui.float(contents, opts)`: open the list-less content float — the sibling of
-- the selectable-list widget (docs/specs/2026-06-14-btv-ui-float-widget.md, "What
-- stays out of this widget") — rendering content with no list / selection.
-- `contents` is a string (split on newlines), a list of line strings, or — for a
-- styled float (the "pretty" which-key) — a list where a row may be a CHUNK LIST
-- `{ {text, hl_group?}, … }` (neovim's virt_text shape): each chunk paints its
-- text in `hl_group`, so a row can colour its key one group and its description
-- another, or dim a whole row with a `Comment`/dim group. Plain and chunk rows mix
-- freely; a plain row is just one un-grouped chunk.
--
-- opts:
--   * `border` — `"none"|"single"|"rounded"|"double"|"solid"` (default `"rounded"`).
--   * `title` — a string drawn on the top border (optional).
--   * `relative` — `"cursor"` (default, anchors at the cursor) | `"editor"` (centered)
--     | `"bottom"` (pinned to the editor's bottom-right corner — the which-key shape).
--   * `persist` — when truthy, the float survives keystrokes (it is not dismissed by
--     the next key) and `btv.ui.float` returns a HANDLE with `:update(contents, opts)` /
--     `:close()` / `:is_open()`. This is the surface a key-observer plugin (e.g.
--     which-key) renders through, refreshing it as keys arrive.
-- Without `persist` it is fire-and-forget: the server owns the float, its
-- geometry, and its dismissal (the next key closes it); returns nil. Empty
-- contents open nothing. LSP hover and signature help use the non-persistent form.
function btv.ui.float(contents, opts)
  opts = opts or {}
  local lines = float_lines(contents)
  if #lines == 0 then
    return
  end
  if opts.persist then
    btv._next_float_id = btv._next_float_id + 1
    local handle = setmetatable({ _id = btv._next_float_id }, float_handle)
    btv._float_open_id = handle._id
    btv._ui_float(
      handle._id,
      lines,
      opts.title,
      opts.border or "rounded",
      opts.relative or "cursor"
    )
    return handle
  end
  -- Transient (id 0): dismissed by the next key, no handle.
  btv._ui_float(0, lines, opts.title, opts.border or "rounded", opts.relative or "cursor")
end

-- ----- btv.ui.open [alias vim.ui.open] ----------------------------------------
-- `btv.ui.open(uri)` -> a PROMISE of the opener's exit result `{ code, stdout, stderr }`
-- (the `btv.run` shape). Hands `uri` — a file path or a URL — to the OS opener chosen
-- by platform (`btv._ui_opener`: `open` on macOS, `explorer` on Windows, `xdg-open`
-- elsewhere) and runs it off-tick. Like `btv.run` it RESOLVES rather than rejects: a
-- missing opener surfaces as `code = -1` and a non-zero opener exit as that code —
-- the caller decides what to do with it. Promise-only (ADR 0002): no callback arg.
function btv.ui.open(uri)
  if type(uri) ~= "string" then
    error("btv.ui.open: uri must be a string", 2)
  end
  -- btv._ui_opener() returns a fresh argv prefix each call; the uri is the target.
  local argv = btv._ui_opener()
  argv[#argv + 1] = uri
  return btv.run({ cmd = argv })
end

-- `vim.ui.open(path)`: neovim's opener alias (ADR 0002 whitelist). neovim returns a
-- blocking handle (`SystemObj`) plus an error string; bemtvi has no blocking handle,
-- so this returns the async PROMISE instead — truthy on the optimistic path, the
-- closest faithful mapping. Callers that ignore the return (the common
-- `vim.ui.open(url)`) work unchanged.
function vim.ui.open(path)
  return btv.ui.open(path)
end

-- ----- btv.ui.caps ------------------------------------------------------------
-- What the ATTACHED CLIENT reported about its terminal, mirrored from the
-- capabilities map it sent at attach. All false until a UI attaches (a headless
-- server has no client to ask), so read it from a `UIEnter` handler — the event
-- fires right after this mirror is refreshed.
btv._ui_caps = btv._ui_caps
  or { keyboard_protocol = false, truecolor = false, osc52 = false, key_labels = {} }

-- Server-called: refresh the mirror from the attaching client's capabilities map.
-- `key_labels` arrives as a flat `{ chord, label, chord, label, … }` list (the wire
-- carries pairs, not a map) and is folded back into a table here.
function btv._set_ui_caps(keyboard_protocol, truecolor, osc52, key_labels)
  btv._ui_caps.keyboard_protocol = keyboard_protocol and true or false
  btv._ui_caps.truecolor = truecolor and true or false
  btv._ui_caps.osc52 = osc52 and true or false
  local labels = {}
  if type(key_labels) == "table" then
    for i = 1, #key_labels - 1, 2 do
      labels[key_labels[i]] = key_labels[i + 1]
    end
  end
  btv._ui_caps.key_labels = labels
end

-- `btv.ui.caps()` -> a fresh table of the attached client's terminal capabilities:
--
-- ```lua
-- {
--   keyboard_protocol = false, -- the kitty keyboard protocol is on
--   truecolor         = false, -- the terminal can show 24-bit color
--   osc52             = false, -- the terminal accepts OSC 52 clipboard writes
--   key_labels        = {},    -- chords this client can only deliver via another chord
-- }
-- ```
--
-- `keyboard_protocol` is the one a keymap cares about. Without it the terminal
-- cannot tell `<C-i>` / `<C-m>` / `<C-[>` / `<C-h>` apart from `<Tab>` / `<CR>` /
-- `<Esc>` / `<BS>`, and bemtvi folds each onto the named key — on BOTH sides, so
-- mapping `<C-h>` there really maps `<BS>`. A plugin that wants one of those four
-- chords should install it only when this is true:
--
-- ```lua
-- btv.on("UIEnter", {}, function()
--   if btv.ui.caps().keyboard_protocol then
--     btv.keymap.set("i", "<C-h>", my_action)
--   end
-- end)
-- ```
--
-- `key_labels` maps a chord the editor sees onto the chord the user must actually
-- PRESS to send it, for the clients that cannot deliver it directly. The browser is
-- the case that forced it: Chrome and Edge handle `<C-w>` / `<C-t>` / `<C-n>` /
-- `<C-1>`..`<C-9>` themselves on Windows and Linux, so the page never sees them and
-- the web client substitutes Alt — a keypress of `Alt+w` arrives as `<C-w>`. Anything
-- that DISPLAYS a key (a which-key popup, a cheat sheet, a `desc` listing) should
-- render through this so it names a chord the user can press; anything that MATCHES
-- on keys must not, since the editor still sees the canonical notation:
--
-- ```lua
-- local labels = btv.ui.caps().key_labels
-- local shown = labels["<C-w>"] or "<C-w>" -- "<A-w>" in a browser, "<C-w>" elsewhere
-- ```
--
-- It is empty on a client with nothing to substitute (every terminal, and a browser
-- on macOS, where those shortcuts hang off Cmd and Ctrl arrives untouched).
--
-- Every field is false/empty before a client attaches, so check it from `UIEnter`
-- rather than at config time: the config is sourced (and `VimEnter` fires) before
-- the first client attaches.
function btv.ui.caps()
  local labels = {}
  for chord, label in pairs(btv._ui_caps.key_labels) do
    labels[chord] = label
  end
  return {
    keyboard_protocol = btv._ui_caps.keyboard_protocol,
    truecolor = btv._ui_caps.truecolor,
    osc52 = btv._ui_caps.osc52,
    key_labels = labels,
  }
end
