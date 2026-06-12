-- nxvim Lua prelude — timers.
-- vim.defer_fn over the event-loop bridge. (The buffer-option proxy `nx.bo` /
-- `vim.bo` lives with the other option scopes in prelude/stdlib.lua.)
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- vim.defer_fn ----------------------------------------------------------
-- Wall-clock deferral rides the event-loop actor through the vim._timer_start /
-- vim._timer_stop bridge: a callback id is registered in vim._cb_fns, the actor
-- sleeps and fires LoopEvent::Timer, and the server runs the callback by id on its
-- thread. This is the same registry the keymap/schedule paths use.
vim._timer_active = vim._timer_active or {}

-- A minimal timer handle returned by vim.defer_fn, so a caller can :stop() the
-- deferral before it fires (neovim returns a uv timer; nxvim returns this). It is
-- NOT the libuv handle API — the `nx` timer surface is the supported one.
local defer_handle = {}
defer_handle.__index = defer_handle
function defer_handle:stop()
  vim._timer_active[self._id] = nil
  vim._timer_stop(self._id)
  vim._cb_fns[self._id] = nil
  return 0
end
function defer_handle:is_active() return vim._timer_active[self._id] == true end

-- vim.defer_fn(fn, timeout): run `fn` once, `timeout` ms from now, on the loop —
-- the off-tick deferral configs use for retry patterns. Returns a handle so the
-- caller can :stop() it before it fires.
function vim.defer_fn(fn, timeout)
  local id = vim._next_cb_id()
  vim._cb_fns[id] = fn
  vim._timer_active[id] = true -- armed; the returned handle's :is_active() reads this
  vim._timer_start(id, timeout or 0, 0) -- one-shot
  return setmetatable({ _id = id }, defer_handle)
end

-- vim.ui: the selection / input / open surface, driven through nxvim's own panel
-- and command line (Phase 8). `select` lists the choices in the panel and routes
-- the `<CR>` pick to `on_choice`; `input` opens a one-line command-line prompt and
-- hands the typed text (or nil on cancel) to `on_confirm`; `open` spawns the OS
-- file/URL opener via the async `vim.system`.
vim.ui = vim.ui or {}

-- vim.ui.select(items, opts, on_choice): present `items` for the user to pick one.
-- `opts.format_item(item)` renders each row (default `tostring`); `opts.prompt` is
-- the panel title. The chosen item and its 1-based index go to
-- `on_choice(item, index)` on `<CR>`. The panel's `on_select` callback drives it.
--
-- INCOMPLETE: dismissing the panel without a pick (`q`) does not deliver the
-- neovim `on_choice(nil)` cancel — the panel has no cancel event — so a caller
-- that must react to cancellation won't. A real pick is faithful. Faithful once
-- the panel emits a dismiss/cancel event.
function vim.ui.select(items, opts, on_choice)
  opts = opts or {}
  items = items or {}
  local format_item = opts.format_item or tostring
  local lines = {}
  for i, item in ipairs(items) do
    lines[i] = tostring(format_item(item))
  end
  vim.panel.open(opts.prompt or "Select one of:", lines, function(_line, idx)
    vim.panel.close()
    if on_choice then on_choice(items[idx], idx) end
  end)
end

-- vim.ui.input(opts, on_confirm): prompt for a line of text. `opts.prompt` is the
-- label shown ahead of the line, `opts.default` prefills it. The typed text reaches
-- `on_confirm(text)` on `<CR>`; cancelling (`<Esc>`) calls `on_confirm(nil)`,
-- matching neovim. Backed by nxvim's command line (a `CmdlineKind::Prompt`), so the
-- usual within-line editing (motion / backspace) applies. The callback fires
-- off-tick, when the server drains the prompt result.
-- INCOMPLETE: only one prompt is open at a time (a single command line) — if
-- several vim.ui.input calls are queued in one tick the last wins (a loud
-- single-prompt limitation, not a silent drop). Faithful once prompts can stack.
function vim.ui.input(opts, on_confirm)
  opts = opts or {}
  local cb = vim._next_cb_id()
  vim._cb_fns[cb] = function(text)
    if on_confirm then on_confirm(text) end
  end
  vim._ui_input(tostring(opts.prompt or "Input: "), tostring(opts.default or ""), cb)
end

-- vim.ui.open(path): open `path` (a file or URL) in the OS handler, asynchronously
-- via `vim.system` (Phase 8). The opener is `open` on macOS, `xdg-open` elsewhere
-- (`vim._ui_opener`). Returns the `vim.system` handle, matching neovim's
-- `(SystemObj, nil)` shape.
function vim.ui.open(path)
  if type(path) ~= "string" or path == "" then
    error("vim.ui.open: path must be a non-empty string", 2)
  end
  local cmd = vim._ui_opener()
  cmd[#cmd + 1] = path
  return vim.system(cmd)
end

-- vim.uri_to_bufnr(uri): in neovim, the (creating) buffer number for `uri`.
-- nxvim has no Lua-side buffer registry yet (Phase 6), so returning 0 would hand
-- a handler a wrong buffer; it raises via vim._notimpl instead.
function vim.uri_to_bufnr(_uri) vim._notimpl("vim.uri_to_bufnr") end

-- vim.validate / vim.deprecate: argument validation and deprecation notices in
-- neovim. Config files call them defensively; nxvim makes them no-ops (never
-- erroring) so a config that validates its opts loads unimpeded.
-- INCOMPLETE: vim.validate never actually validates — a config passing the wrong
-- arg type sails through instead of getting neovim's "expected X, got Y" error,
-- so bad opts surface later (or never) rather than at the validate call. A real
-- impl would parse the {name = {value, type/pred, optional}} spec and raise on
-- mismatch. vim.deprecate likewise swallows every deprecation notice silently.
function vim.validate() end

function vim.deprecate() end
