-- nxvim Lua prelude — timers + the async UI surface (nx.ui).
-- nx.timer (alias vim.defer_fn) over the event-loop bridge, plus the four small
-- async UI primitives the native-plugin-API names (nx.ui.input / select / confirm
-- / float — docs/specs/2026-06-11-native-plugin-api.md). None blocks (ADR 0002
-- rule 3): each returns at once and fires its callback on a later tick.
--   nx.ui.input   — a one-line text prompt over the editor's command line
--                   (alias vim.ui.input per ADR 0002's whitelist).
--   nx.ui.select  — the callback-shaped chooser over the floating selectable-list
--                   widget (alias vim.ui.select), one consumer of the server's
--                   shared float layer (docs/specs/2026-06-14-nx-ui-float-widget.md).
--   nx.ui.confirm — a yes/no confirmation over the command line (nx-native; neovim
--                   spells this blocking vim.fn.confirm, which the nx model omits).
--   nx.ui.float   — the list-less content float (the widget's sibling, also the LSP
--                   hover surface).
-- input and select map the chosen value back through nx._cb_fns; confirm shares the
-- command-line prompt plumbing with input (one prompt open at a time). The
-- nx.validate / nx.deprecate no-ops are not part of nxvim's config API and remain
-- intentionally absent.
local vim = vim
nx = nx or {}
nx.ui = nx.ui or {}
vim.ui = vim.ui or {}

-- ----- nx.timer [alias vim.defer_fn] -----------------------------------------
-- Wall-clock deferral rides the event-loop actor through the nx._timer_start /
-- nx._timer_stop bridge: a callback id is registered in nx._cb_fns, the actor
-- sleeps and fires LoopEvent::Timer, and the server runs the callback by id on its
-- thread. This is the same registry the keymap/schedule paths use.
nx._timer_active = nx._timer_active or {}

-- A minimal timer handle returned by nx.timer, so a caller can :stop() the
-- deferral before it fires (neovim returns a uv timer; nxvim returns this). It is
-- NOT the libuv handle API — the `nx` timer surface is the supported one.
local defer_handle = {}
defer_handle.__index = defer_handle
function defer_handle:stop()
  nx._timer_active[self._id] = nil
  nx._timer_stop(self._id)
  nx._cb_fns[self._id] = nil
  return 0
end
function defer_handle:is_active()
  return nx._timer_active[self._id] == true
end

-- nx.timer(fn, timeout): the canonical timer / defer primitive (aliased by
-- vim.defer_fn) — run `fn` once, `timeout` ms from now, on the loop — the
-- off-tick deferral configs use for retry patterns. Returns a handle so the
-- caller can :stop() it before it fires.
function nx.timer(fn, timeout)
  local id = nx._next_cb_id()
  nx._cb_fns[id] = fn
  nx._timer_active[id] = true -- armed; the returned handle's :is_active() reads this
  nx._timer_start(id, timeout or 0, 0) -- one-shot
  return setmetatable({ _id = id }, defer_handle)
end
vim.defer_fn = nx.timer

-- ----- nx.ui.select [alias vim.ui.select] ------------------------------------
-- nx.ui.select(items, opts, on_choice): open a floating selectable list and call
-- on_choice(item, index) with the chosen element and its 1-based index — or
-- on_choice(nil, nil) on cancel. The server owns the widget, its navigation, and
-- the input grab; Lua only renders the display labels (opts.format_item, default
-- tostring) up front and maps the chosen index back to the original item, so an
-- arbitrary item table round-trips even though only strings cross the bridge.
-- Non-blocking and callback-shaped (ADR 0002 rule 3): the call returns at once
-- and on_choice fires on a later tick.
function nx.ui.select(items, opts, on_choice)
  opts = opts or {}
  on_choice = on_choice or function() end
  if type(items) ~= "table" then
    error("nx.ui.select: items must be a list", 2)
  end
  local format_item = opts.format_item or tostring
  local labels = {}
  for i, item in ipairs(items) do
    labels[i] = tostring(format_item(item))
  end
  -- An empty list has nothing to choose: resolve to cancel without a menu.
  if #labels == 0 then
    on_choice(nil, nil)
    return
  end
  local id = nx._next_cb_id()
  nx._cb_fns[id] = function(idx)
    -- idx: the 1-based chosen index, or nil on cancel.
    if idx == nil then
      return on_choice(nil, nil)
    end
    return on_choice(items[idx], idx)
  end
  nx._ui_select(labels, opts.prompt or "", id)
end
vim.ui.select = nx.ui.select

-- ----- nx.ui.input [alias vim.ui.input] --------------------------------------
-- nx.ui.input(opts, on_confirm): open a one-line text prompt and call
-- on_confirm(text) with the entered string on <CR>, or on_confirm(nil) on <Esc>
-- (cancel). opts:
--   prompt  = the label drawn ahead of the editable line (default "")
--   default = text prefilled into the line, cursor at its end (default "")
-- The server owns the prompt: it opens the editor's command line as a labelled
-- Prompt (Editor::open_prompt), and delivers the result to nx._cb_fns[id] through
-- the shared prompt_results channel. Non-blocking and callback-shaped (ADR 0002
-- rule 3): the call returns at once and on_confirm fires on a later tick. Note an
-- empty submission (<CR> on an empty line) is "" (not nil) — only <Esc> cancels,
-- matching neovim's vim.ui.input.
function nx.ui.input(opts, on_confirm)
  opts = opts or {}
  on_confirm = on_confirm or function() end
  if type(opts) ~= "table" then
    error("nx.ui.input: opts must be a table", 2)
  end
  local id = nx._next_cb_id()
  nx._cb_fns[id] = function(text)
    -- text: the entered string ("" on an empty <CR>), or nil on cancel.
    return on_confirm(text)
  end
  nx._ui_input(tostring(opts.prompt or ""), tostring(opts.default or ""), id)
end
vim.ui.input = nx.ui.input

-- ----- nx.ui.confirm ---------------------------------------------------------
-- nx.ui.confirm(message, opts, on_choice) — or nx.ui.confirm(message, on_choice):
-- a yes/no confirmation over the command line. on_choice(confirmed) gets a boolean
-- — true on Yes, false on No or cancel (<Esc>). opts (optional):
--   default = true | false  -- which button <CR> selects (default true = Yes)
-- nx-native: neovim spells this blocking vim.fn.confirm, which the nx model omits
-- (rule 3). For an arbitrary multi-choice menu use nx.ui.select instead — confirm
-- is deliberately just yes/no. The server opens a single-keypress Confirm dialog
-- (Editor::open_confirm) sharing the prompt_results channel with nx.ui.input (one
-- prompt open at a time); the chosen 1-based button index arrives as a string,
-- which the wrapper folds to the boolean (button 1 = Yes; 2 = No; 0 = cancel).
function nx.ui.confirm(message, opts, on_choice)
  -- The 2-arg form nx.ui.confirm(message, on_choice): opts omitted.
  if type(opts) == "function" then
    on_choice = opts
    opts = nil
  end
  opts = opts or {}
  on_choice = on_choice or function() end
  if type(message) ~= "string" then
    error("nx.ui.confirm: message must be a string", 2)
  end
  -- Default to Yes (<CR> accepts), unless opts.default is explicitly false.
  local default_yes = opts.default ~= false
  -- A shell-style hint: the default button is upper-cased.
  local hint = default_yes and "[Y/n]" or "[y/N]"
  local label = message .. " " .. hint .. " "
  local id = nx._next_cb_id()
  nx._cb_fns[id] = function(idx_str)
    -- idx_str: the chosen 1-based button index as a string ("0" = Esc-cancel).
    return on_choice(tonumber(idx_str) == 1)
  end
  -- accelerators are matched lowercase against the keypress, in button order.
  nx._confirm(label, { "y", "n" }, default_yes and 1 or 2, id)
end

-- ----- nx.ui.float -----------------------------------------------------------
-- Normalize `contents` (a string split on newlines, or a list of line strings)
-- into a line list, dropping a single trailing empty line so a markdown body
-- ending in "\n" doesn't render a blank last row. Errors loud on a bad type.
local function float_lines(contents)
  local lines
  if type(contents) == "string" then
    lines = vim.split(contents, "\n", { plain = true })
  elseif type(contents) == "table" then
    lines = {}
    for i, l in ipairs(contents) do
      lines[i] = tostring(l)
    end
  else
    error("nx.ui.float: contents must be a string or a list of strings", 2)
  end
  if #lines > 1 and lines[#lines] == "" then
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
  nx._ui_float(self._id, lines, opts.title, opts.border or "rounded", opts.relative == "editor")
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
-- stays out of this widget") — rendering plain content with no list / selection.
-- `contents` is a string (split on newlines) or a list of line strings. `opts`:
--   border   = "none"|"single"|"rounded"|"double"|"solid"  (default "rounded")
--   title    = a string drawn on the top border (optional)
--   relative = "cursor" (default, anchors at the cursor) | "editor" (centered)
--   persist  = when truthy, the float survives keystrokes (it is not dismissed by
--              the next key) and nx.ui.float returns a HANDLE with :update(contents,
--              opts) / :close() / :is_open(). This is the surface an observer plugin
--              (e.g. which-key, driving it from nx.on_key) renders through.
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
    nx._ui_float(handle._id, lines, opts.title, opts.border or "rounded", opts.relative == "editor")
    return handle
  end
  -- Transient (id 0): dismissed by the next key, no handle.
  nx._ui_float(0, lines, opts.title, opts.border or "rounded", opts.relative == "editor")
end
