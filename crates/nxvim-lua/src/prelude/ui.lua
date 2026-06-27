-- nxvim Lua prelude — the async UI surface (nx.ui).
-- The four small async UI primitives the native-plugin-API names (nx.ui.input /
-- select / confirm / float — docs/specs/2026-06-11-native-plugin-api.md). None
-- blocks (ADR 0002 rule 3): each returns at once and the result arrives on a later
-- tick. (The deferral primitives nx.schedule / nx.timer live in prelude/runtime.lua.)
--   nx.ui.input   — a one-line text prompt over the editor's command line.
--                   PROMISE-ONLY: nx.ui.input(opts) -> a promise of the text.
--   nx.ui.select  — a chooser over the floating selectable-list widget, one
--                   consumer of the server's shared float layer
--                   (docs/specs/2026-06-14-nx-ui-float-widget.md). PROMISE-ONLY:
--                   nx.ui.select(items, opts) -> a promise of the chosen item.
--   nx.ui.confirm — a yes/no confirmation over the command line (nx-native; neovim
--                   spells this blocking vim.fn.confirm, which the nx model omits).
--                   PROMISE-ONLY: nx.ui.confirm(message, opts) -> a promise of bool.
--   nx.ui.float   — the list-less content float (the widget's sibling, also the LSP
--                   hover surface). Fire-and-forget by default (no result); with
--                   `persist = true` it returns a resource HANDLE (:update/:close/
--                   :is_open), not an async result — so it stays non-promise.
--   nx.ui.open    — hand a path/URL to the OS opener (open / explorer / xdg-open).
--                   PROMISE-ONLY: nx.ui.open(uri) -> a promise of the run result.
-- `nx` async is promise-only (ADR 0002 / docs/plans/2026-06-16-nx-promise-only-async.md):
-- a one-shot async API returns a promise, never a callback. The callback shape lives
-- on the `vim.ui.*` muscle-memory aliases (the bounded compat layer), which adapt the
-- promise back to neovim's on_confirm / on_choice signatures. input and select map the
-- chosen value back through nx._cb_fns; confirm shares the command-line prompt plumbing
-- with input (one prompt open at a time). The nx.validate / nx.deprecate no-ops are not
-- part of nxvim's config API and remain intentionally absent.
local vim = vim
nx = nx or {}
nx.ui = nx.ui or {}
vim.ui = vim.ui or {}

-- ----- nx.ui.select [alias vim.ui.select] ------------------------------------
-- The shared core: open a floating selectable list and call cb(item, index) with
-- the chosen element and its 1-based index — or cb(nil, nil) on cancel. The server
-- owns the widget, its navigation, and the input grab; Lua only renders the display
-- labels (opts.format_item, default tostring) up front and maps the chosen index
-- back to the original item, so an arbitrary item table round-trips even though only
-- strings cross the bridge. Non-blocking (ADR 0002 rule 3): the menu opens at once
-- and cb fires on a later tick. Both the promise-shaped nx.ui.select and the
-- callback-shaped vim.ui.select alias build on this.
local function select_into(items, opts, cb)
  opts = opts or {}
  if type(items) ~= "table" then
    error("nx.ui.select: items must be a list", 2)
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
  local id = nx._next_cb_id()
  nx._cb_fns[id] = function(idx)
    -- idx: the 1-based chosen index, or nil on cancel.
    if idx == nil then
      return cb(nil, nil)
    end
    return cb(items[idx], idx)
  end
  nx._ui_select(labels, opts.prompt or "", id)
end

-- nx.ui.select(items, opts) -> a PROMISE that resolves to the chosen item, or to
-- nil on cancel (<Esc> / q). Promise-only: there is no on_choice argument (passing
-- one is the old callback shape and errors loudly). The 1-based index is dropped —
-- recover it from the item, or use the vim.ui.select alias, which keeps it. opts:
--   prompt      = the label drawn above the list (default none)
--   format_item = item -> display string (default tostring); the original item
--                 round-trips to the resolved value regardless.
function nx.ui.select(items, opts, on_choice)
  if on_choice ~= nil then
    error("nx.ui.select is promise-only: nx.ui.select(items, opts):next(fn)", 2)
  end
  if type(items) ~= "table" then
    error("nx.ui.select: items must be a list", 2)
  end
  return nx.promise.new(function(resolve)
    -- cb(item, index) -> resolve(item); the index falls away (resolve is 1-arg).
    select_into(items, opts, resolve)
  end)
end

-- vim.ui.select(items, opts, on_choice): neovim's callback-shaped alias (ADR 0002
-- whitelist) — on_choice(item, index), or on_choice(nil, nil) on cancel. Kept on the
-- compat layer so plugins (telescope, …) that pass a callback and read the index
-- still work; nx code uses the promise form above.
function vim.ui.select(items, opts, on_choice)
  select_into(items, opts, on_choice or function() end)
end

-- ----- rebindable select keys -----------------------------------------------
-- Like the picker, a promptless `nx.ui.select` list is driven through the keymap
-- engine, NOT a hardcoded grab: the server selects the `select` bucket while the
-- list owns input, so navigation / confirm / cancel are configurable with
-- `nx.keymap.set('select', '<key>', nx.ui.select_actions.<name>)`. Each action fires
-- through the engine (nx._select_action -> Editor::apply_select_action). A select
-- list has NO query, so there is no text fallthrough — an unmapped key is inert.
nx.ui.select_actions = nx.ui.select_actions or {}
for _, name in ipairs({ "next", "prev", "first", "last", "confirm", "cancel" }) do
  nx.ui.select_actions[name] = function()
    nx._select_action(name)
  end
end

-- The default select bindings — `default = true` so a user `nx.keymap.set('select',
-- …)` wins, and an empty-function map disables a key. `gg` is a two-key default map
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
  nx.keymap.set("select", m[1], nx.ui.select_actions[m[2]], { default = true, desc = m[3] })
end

-- ----- nx.ui.input [alias vim.ui.input] --------------------------------------
-- nx.ui.input(opts) -> a PROMISE that resolves to the entered string on <CR>, or
-- to nil on <Esc> (cancel). Promise-only: there is no on_confirm argument (passing
-- one is the old callback shape and errors loudly). opts:
--   prompt  = the label drawn ahead of the editable line (default "")
--   default = text prefilled into the line, cursor at its end (default "")
--   history = a namespace string enabling readline-style recall: `<Up>`/`<Down>`
--             (and `<C-p>`/`<C-n>`) browse the prompts submitted under this
--             namespace, and each non-empty submission is recorded into it. Each
--             namespace is an independent ring, so one plugin's REPL history is
--             separate from another's. Session-only for now. Absent ⇒ no history.
--   complete = a function `(line, col) -> candidates` driving `<Tab>` autocomplete
--             (the inline wildmenu above the prompt line — `<Tab>`/`<S-Tab>` cycle,
--             `<CR>` accepts). `candidates` is a `{ {label, insert?, doc?, start?,
--             length?}, … }` list (`insert` defaults to `label`), OR a PROMISE of one —
--             so an async source (e.g. a DAP `completions` request) works. The token
--             completed is the trailing identifier run before the cursor, UNLESS a
--             candidate supplies `start` (0-based char offset into the line) + `length`
--             (chars), an explicit replace span that overrides the token for that row.
--             `col` is the cursor's 0-based char offset.
--   complete_docs = show the side docs pane rendering each candidate's `doc` beside
--             the list (default true when `complete` is set; `false` suppresses it).
--   complete_debounce = ms to coalesce refresh queries (narrowing an open menu as you
--             type) so an async source isn't a wire round-trip per keystroke; the
--             initial `<Tab>` is always immediate (default 100; `0` disables it).
-- The server owns the prompt: it opens the editor's command line as a labelled
-- Prompt (Editor::open_prompt), and delivers the result to nx._cb_fns[id] through
-- the shared prompt_results channel. Non-blocking (ADR 0002 rule 3): the call
-- returns at once and the promise settles on a later tick. Note an empty submission
-- (<CR> on an empty line) resolves to "" (not nil) — only <Esc> cancels, matching
-- neovim's vim.ui.input.
function nx.ui.input(opts, on_confirm)
  if on_confirm ~= nil then
    error("nx.ui.input is promise-only: nx.ui.input(opts):next(fn)", 2)
  end
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.ui.input: opts must be a table", 2)
  end
  local history = opts.history
  if history ~= nil and type(history) ~= "string" then
    error("nx.ui.input: opts.history must be a string namespace", 2)
  end
  local complete = opts.complete
  if complete ~= nil and type(complete) ~= "function" then
    error("nx.ui.input: opts.complete must be a function", 2)
  end
  -- The side docs pane defaults ON when a completion source is given (so a candidate
  -- carrying a `doc` shows it); pass `complete_docs = false` to suppress it.
  local complete_docs = complete ~= nil and opts.complete_docs ~= false
  -- Refresh queries (narrowing an open menu as you type) coalesce through this debounce
  -- so an async source isn't a wire round-trip per keystroke; the initial <Tab> is
  -- always immediate. Default 100ms; `0` disables it (re-query every edit).
  local debounce_ms = opts.complete_debounce
  if debounce_ms ~= nil and (type(debounce_ms) ~= "number" or debounce_ms < 0) then
    error("nx.ui.input: opts.complete_debounce must be a non-negative number (ms)", 2)
  end
  debounce_ms = debounce_ms or 100
  return nx.promise.new(function(resolve)
    local id = nx._next_cb_id()
    -- text: the entered string ("" on an empty <CR>), or nil on cancel. Tear down the
    -- active completion source + its debounce on settle so neither leaks into a later
    -- prompt (only one prompt is open at a time).
    nx._cb_fns[id] = function(text)
      if nx._prompt_complete_debounced then
        nx._prompt_complete_debounced:cancel()
      end
      nx._active_prompt_complete = nil
      nx._prompt_complete_debounced = nil
      resolve(text)
    end
    nx._active_prompt_complete = complete
    -- A per-prompt debounced wrapper around the source (latest-args, trailing edge).
    -- nil when there's no source or the user disabled it (`complete_debounce = 0`).
    nx._prompt_complete_debounced = (complete and debounce_ms > 0)
        and nx.utils.debounce(function(line, col)
          nx._do_prompt_complete(complete, line, col)
        end, debounce_ms)
      or nil
    nx._ui_input(
      tostring(opts.prompt or ""),
      tostring(opts.default or ""),
      id,
      history,
      complete ~= nil,
      complete_docs
    )
  end)
end

-- The open prompt's `complete` source (`nx.ui.input{ complete = fn }`) and its
-- per-prompt debounced wrapper, or nil when the prompt opted into none / no debounce.
-- One prompt is open at a time, so single slots hold them; cleared when it settles.
nx._active_prompt_complete = nx._active_prompt_complete or nil
nx._prompt_complete_debounced = nx._prompt_complete_debounced or nil

-- Run the source once and pipe its candidates (a `{ {label, insert?, doc?}, … }` list
-- OR a promise of one — an async source like the DAP `completions` round-trip) into
-- the wildmenu via `nx._prompt_complete_show`. `nx.promise.resolve` adapts the sync
-- and async shapes uniformly; a nil result or an error resolves to an empty list (the
-- menu just closes — a completion hiccup must not break the prompt). Shared by the
-- immediate path and the debounced refresh path.
function nx._do_prompt_complete(fn, line, col)
  nx.promise.resolve(fn(line, col)):next(function(cands)
    nx._prompt_complete_show(cands or {})
  end, function()
    nx._prompt_complete_show({})
  end)
end

-- nx._run_prompt_complete(line, col, refresh): drive the open prompt's `complete`
-- source for the `<Tab>` wildmenu. The server calls this when core stamps a
-- prompt-completion request. The initial open (`refresh = false`) queries at once for
-- a snappy menu; an edit narrowing the open menu (`refresh = true`) coalesces through
-- the prompt's debounce so a rapid burst of keystrokes is one query, not one per key.
function nx._run_prompt_complete(line, col, refresh)
  local fn = nx._active_prompt_complete
  if not fn then
    nx._prompt_complete_show({})
    return
  end
  if refresh and nx._prompt_complete_debounced then
    nx._prompt_complete_debounced(line, col)
  else
    nx._do_prompt_complete(fn, line, col)
  end
end

-- vim.ui.input(opts, on_confirm): neovim's callback-shaped alias (ADR 0002
-- whitelist) — on_confirm(text) on <CR>, on_confirm(nil) on cancel. Kept on the
-- compat layer for plugins; nx code uses the promise form above.
function vim.ui.input(opts, on_confirm)
  on_confirm = on_confirm or function() end
  nx.ui.input(opts):next(on_confirm)
end

-- ----- nx.ui.confirm ---------------------------------------------------------
-- nx.ui.confirm(message, opts) -> a PROMISE that resolves to a boolean — true on
-- Yes, false on No or cancel (<Esc>). Promise-only: there is no on_choice argument
-- (the old callback forms — a third arg, or opts-as-function — error loudly). opts
-- (optional):
--   default = true | false  -- which button <CR> selects (default true = Yes)
-- nx-native (no vim.ui twin): neovim spells this blocking vim.fn.confirm, which the
-- nx model omits (rule 3). For an arbitrary multi-choice menu use nx.ui.select
-- instead — confirm is deliberately just yes/no. The server opens a single-keypress
-- Confirm dialog (Editor::open_confirm) sharing the prompt_results channel with
-- nx.ui.input (one prompt open at a time); the chosen 1-based button index arrives
-- as a string, which the wrapper folds to the boolean (1 = Yes; 2 = No; 0 = cancel).
function nx.ui.confirm(message, opts, on_choice)
  if type(opts) == "function" or on_choice ~= nil then
    error("nx.ui.confirm is promise-only: nx.ui.confirm(message, opts):next(fn)", 2)
  end
  opts = opts or {}
  if type(message) ~= "string" then
    error("nx.ui.confirm: message must be a string", 2)
  end
  -- Default to Yes (<CR> accepts), unless opts.default is explicitly false.
  local default_yes = opts.default ~= false
  -- A shell-style hint: the default button is upper-cased.
  local hint = default_yes and "[Y/n]" or "[y/N]"
  local label = message .. " " .. hint .. " "
  return nx.promise.new(function(resolve)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(idx_str)
      -- idx_str: the chosen 1-based button index as a string ("0" = Esc-cancel).
      resolve(tonumber(idx_str) == 1)
    end
    -- accelerators are matched lowercase against the keypress, in button order.
    nx._confirm(label, { "y", "n" }, default_yes and 1 or 2, id)
  end)
end

-- ----- nx.ui.float -----------------------------------------------------------
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
      error("nx.ui.float: a styled line must be a list of { text, hl_group? } chunks", 3)
    end
    if c[2] ~= nil and type(c[2]) ~= "string" then
      error("nx.ui.float: a chunk's hl_group must be a string", 3)
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
-- dropping a single trailing empty line so a markdown body ending in "\n" doesn't
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
    error("nx.ui.float: contents must be a string or a list of strings/chunk lines", 2)
  end
  if #lines > 1 and chunk_text(lines[#lines]) == "" then
    lines[#lines] = nil
  end
  return lines
end

-- The persistent-float handle returned by `nx.ui.float{ persist = true }`. It
-- owns a server-side content float by id: `:update` replaces its content in place
-- (same id), `:close` dismisses it, `:is_open` reports whether this handle still
-- owns the open float. `nx._float_open_id` tracks which persistent float the
-- server last opened, so a stale handle (its float replaced by a newer persistent
-- one) reports `is_open() == false` without a server round-trip.
nx._float_open_id = nx._float_open_id or nil
nx._next_float_id = nx._next_float_id or 0
local float_handle = {}
float_handle.__index = float_handle
function float_handle:update(contents, opts)
  opts = opts or {}
  local lines = float_lines(contents)
  if #lines == 0 then
    return self:close()
  end
  nx._float_open_id = self._id
  nx._ui_float(self._id, lines, opts.title, opts.border or "rounded", opts.relative or "cursor")
end
function float_handle:close()
  if nx._float_open_id == self._id then
    nx._float_open_id = nil
  end
  nx._ui_float_close(self._id)
end
function float_handle:is_open()
  return nx._float_open_id == self._id
end

-- nx.ui.float(contents, opts): open the list-less content float — the sibling of
-- the selectable-list widget (docs/specs/2026-06-14-nx-ui-float-widget.md, "What
-- stays out of this widget") — rendering content with no list / selection.
-- `contents` is a string (split on newlines), a list of line strings, or — for a
-- styled float (the "pretty" which-key) — a list where a row may be a CHUNK LIST
-- `{ {text, hl_group?}, … }` (neovim's virt_text shape): each chunk paints its
-- text in `hl_group`, so a row can colour its key one group and its description
-- another, or dim a whole row with a `Comment`/dim group. Plain and chunk rows mix
-- freely; a plain row is just one un-grouped chunk. `opts`:
--   border   = "none"|"single"|"rounded"|"double"|"solid"  (default "rounded")
--   title    = a string drawn on the top border (optional)
--   relative = "cursor" (default, anchors at the cursor) | "editor" (centered) |
--              "bottom" (pinned to the editor's bottom-right corner — the
--              which-key shape)
--   persist  = when truthy, the float survives keystrokes (it is not dismissed by
--              the next key) and nx.ui.float returns a HANDLE with :update(contents,
--              opts) / :close() / :is_open(). This is the surface a key-observer
--              plugin (e.g. which-key) renders through, refreshing it as keys arrive.
-- Without `persist` it is fire-and-forget: the server owns the float, its
-- geometry, and its dismissal (the next key closes it); returns nil. Empty
-- contents open nothing. LSP hover and signature help use the non-persistent form.
function nx.ui.float(contents, opts)
  opts = opts or {}
  local lines = float_lines(contents)
  if #lines == 0 then
    return
  end
  if opts.persist then
    nx._next_float_id = nx._next_float_id + 1
    local handle = setmetatable({ _id = nx._next_float_id }, float_handle)
    nx._float_open_id = handle._id
    nx._ui_float(handle._id, lines, opts.title, opts.border or "rounded", opts.relative or "cursor")
    return handle
  end
  -- Transient (id 0): dismissed by the next key, no handle.
  nx._ui_float(0, lines, opts.title, opts.border or "rounded", opts.relative or "cursor")
end

-- ----- nx.ui.open [alias vim.ui.open] ----------------------------------------
-- nx.ui.open(uri) -> a PROMISE of the opener's exit result { code, stdout, stderr }
-- (the nx.run shape). Hands `uri` — a file path or a URL — to the OS opener chosen
-- by platform (nx._ui_opener: `open` on macOS, `explorer` on Windows, `xdg-open`
-- elsewhere) and runs it off-tick. Like nx.run it RESOLVES rather than rejects: a
-- missing opener surfaces as code = -1 and a non-zero opener exit as that code —
-- the caller decides what to do with it. Promise-only (ADR 0002): no callback arg.
function nx.ui.open(uri)
  if type(uri) ~= "string" then
    error("nx.ui.open: uri must be a string", 2)
  end
  -- nx._ui_opener() returns a fresh argv prefix each call; the uri is the target.
  local argv = nx._ui_opener()
  argv[#argv + 1] = uri
  return nx.run({ cmd = argv })
end

-- vim.ui.open(path): neovim's opener alias (ADR 0002 whitelist). neovim returns a
-- blocking handle (SystemObj) plus an error string; nxvim has no blocking handle,
-- so this returns the async PROMISE instead — truthy on the optimistic path, the
-- closest faithful mapping. Callers that ignore the return (the common
-- `vim.ui.open(url)`) work unchanged.
function vim.ui.open(path)
  return nx.ui.open(path)
end
