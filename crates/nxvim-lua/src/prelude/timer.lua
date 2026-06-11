-- nxvim Lua prelude — timers and vim.bo.
-- vim.defer_fn, vim.uv timers and vim.fn.timer_* over the event-loop bridge, plus the vim.bo buffer-option proxy.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- timers: vim.defer_fn / vim.uv timers / vim.fn.timer_* -----------------
-- All wall-clock timers ride the event-loop actor through the vim._timer_start /
-- vim._timer_stop bridge: a callback id is registered in vim._cb_fns, the actor
-- sleeps and fires LoopEvent::Timer, and the server runs the callback by id on its
-- thread. A repeating timer (repeat > 0) keeps its callback across fires; a
-- one-shot drops it. This is the same registry the keymap/schedule paths use.

-- A libuv-style timer handle: a table carrying its callback id, with the
-- start/stop/close/again methods plugins call. :start arms the actor timer;
-- :stop / :close cancel it (and :close drops the callback, freeing the registry).
-- Logical timer state, id-keyed, so :is_active() can answer faithfully. The
-- event-loop actor (Rust) *runs* the timer, but the armed/expired transition is
-- knowable Lua-side: a timer is active from :start until it is :stop/:close'd or
-- (one-shot only) fires. The lone edge the actor owns alone — a one-shot
-- auto-expiring with no Lua-side stop — is cleared at the single fire chokepoint
-- vim._run_cb (runtime.lua), which sees `keep == false` for a spent one-shot.
-- (Repeating timers stay active across fires until explicitly stopped.)
vim._timer_active = vim._timer_active or {}

local uv_timer = {}
uv_timer.__index = uv_timer
function uv_timer:start(timeout, rep, cb)
  if cb ~= nil then vim._cb_fns[self._id] = cb end
  self._repeat = rep or 0
  vim._timer_active[self._id] = true
  vim._timer_start(self._id, timeout or 0, self._repeat)
  return 0
end
function uv_timer:stop()
  vim._timer_active[self._id] = nil
  vim._timer_stop(self._id)
  return 0
end
function uv_timer:again()
  -- libuv: restart a repeating timer, using its stored repeat as the new delay.
  vim._timer_active[self._id] = true
  vim._timer_start(self._id, self._repeat, self._repeat)
  return 0
end
function uv_timer:close(cb)
  self._closing = true
  vim._timer_active[self._id] = nil
  vim._timer_stop(self._id)
  vim._cb_fns[self._id] = nil -- drop the callback so the registry can't leak
  vim._proc_pids[self._id] = nil
  if cb then cb() end
end
-- libuv semantics: is_active() is true while the timer is armed and will still
-- fire; is_closing() is true once :close() has begun tearing the handle down.
function uv_timer:is_closing() return self._closing == true end
function uv_timer:is_active() return vim._timer_active[self._id] == true end

-- vim.uv.new_timer_handle(id): wrap an existing callback id in a handle (used by
-- defer_fn, whose fn is already registered). vim.uv.new_timer(): a fresh handle.
-- vim.uv and vim.loop are the same table, so this lands on both.
function vim.uv.new_timer_handle(id) return setmetatable({ _id = id, _repeat = 0 }, uv_timer) end
function vim.uv.new_timer() return vim.uv.new_timer_handle(vim._next_cb_id()) end

-- luv's *function-form* timer API: uv.timer_start(handle, timeout, repeat, cb) /
-- uv.timer_stop(handle) / uv.timer_again(handle). luv exposes both the handle
-- methods (handle:start(...)) and these table-level functions taking the handle as
-- the first argument; some plugins use the latter (lualine's statusline refresh
-- timer is `vim.loop.timer_start(handle, …)` / `timer_stop(handle)`). vim.uv and
-- vim.loop are the same table, so these land on both. Each just delegates to the
-- handle method, so the event-loop bridge and no-leak guarantees are unchanged.
function vim.uv.timer_start(handle, timeout, rep, cb) return handle:start(timeout, rep, cb) end
function vim.uv.timer_stop(handle) return handle:stop() end
function vim.uv.timer_again(handle) return handle:again() end

-- ----- vim.uv.new_fs_event: filesystem change watcher ------------------------
-- A libuv-style fs-event handle: watch a path and fire callback(err, filename,
-- events) when it changes. nxvim backs this with a native watcher (inotify /
-- FSEvents / kqueue, via the `notify` crate) in the event-loop actor (evloop.rs).
-- :start arms the watch, :stop cancels it (the handle can be re-started), :close
-- cancels and drops the callback. `flags` is luv's { watch_entry, stat, recursive }
-- table; `recursive` (watch a subtree) is honored, the others are accepted for
-- call-compatibility (they don't change what's reported).
local uv_fs_event = {}
uv_fs_event.__index = uv_fs_event
function uv_fs_event:start(path, flags, cb)
  if type(path) ~= "string" or path == "" then
    error("fs_event:start: path must be a non-empty string", 2)
  end
  if type(cb) ~= "function" then error("fs_event:start: callback must be a function", 2) end
  vim._cb_fns[self._id] = cb
  self._path = path
  vim._fs_event_start(self._id, path, type(flags) == "table" and flags.recursive or false)
  return 0
end
function uv_fs_event:stop()
  vim._fs_event_stop(self._id)
  return 0
end
function uv_fs_event:getpath() return self._path end
function uv_fs_event:close(cb)
  vim._fs_event_stop(self._id)
  vim._cb_fns[self._id] = nil -- drop the callback so the registry can't leak
  if cb then cb() end
end

function vim.uv.new_fs_event() return setmetatable({ _id = vim._next_cb_id() }, uv_fs_event) end

-- vim.defer_fn(fn, timeout): run `fn` once, `timeout` ms from now, on the loop —
-- the off-tick deferral configs use for retry patterns. Returns a timer handle so
-- the caller can :stop() it before it fires (neovim returns a uv timer).
function vim.defer_fn(fn, timeout)
  local id = vim._next_cb_id()
  vim._cb_fns[id] = fn
  vim._timer_active[id] = true -- armed; the returned handle's :is_active() reads this
  vim._timer_start(id, timeout or 0, 0) -- one-shot
  return vim.uv.new_timer_handle(id)
end

-- vim.fn.timer_start(timeout, callback, opts): the vimscript timer. Returns a
-- timer id for timer_stop. `opts.repeat` is a *count* (-1 = forever, N = fire N
-- times, absent/0 = once); since the actor speaks intervals not counts, a finite
-- N>1 is honored by a wrapper that decrements and stops itself, so the count is
-- real rather than approximated. `callback` is called with the timer id (vim
-- passes the timer id as its argument).
function vim.fn.timer_start(timeout, callback, opts)
  opts = opts or {}
  local count = opts["repeat"] or 0
  local id = vim._next_cb_id()
  if count == 0 then
    vim._cb_fns[id] = function() callback(id) end
    vim._timer_start(id, timeout, 0)
  elseif count < 0 then
    vim._cb_fns[id] = function() callback(id) end
    vim._timer_start(id, timeout, timeout) -- forever, interval == timeout
  else
    local remaining = count
    vim._cb_fns[id] = function()
      callback(id)
      remaining = remaining - 1
      if remaining <= 0 then
        vim._timer_stop(id)
        vim._cb_fns[id] = nil
      end
    end
    vim._timer_start(id, timeout, timeout)
  end
  return id
end

-- vim.fn.timer_stop(id): cancel a timer started by timer_start and drop its fn.
function vim.fn.timer_stop(id)
  vim._timer_stop(id)
  vim._cb_fns[id] = nil
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

-- vim.bo: buffer-local options, indexed by bufnr (`vim.bo[buf].filetype`).
--
-- The indentation options nxvim's core honors — tabstop/shiftwidth/expandtab and
-- their `ts`/`sw`/`et` abbreviations — are *wired*: a write reaches the live
-- editor (it changes how the buffer renders tabs and indents on <Tab>), and a
-- read returns the core's current value (`vim._bo_mirror`, refreshed by the
-- server) — the option default until set, and a value set through the `:set`
-- ex-command path, not just one written from Lua.
--
-- `filetype`/`ft` stays authoritative from the current-buffer snapshot (it backs
-- the `root_dir` filetype checks configs do at load) unless a write overrode it.
-- Any other option falls back to the plain Lua store `vim._bo_store` (observable
-- read/write, but not yet driving editor behavior). A bare `vim.bo.<opt>` (no
-- bufnr) targets the current buffer.

-- Canonical name of a *wired* (core-honored) buffer option, or nil for the rest.
local BUF_OPT_CANON = {
  tabstop = "tabstop",
  ts = "tabstop",
  shiftwidth = "shiftwidth",
  sw = "shiftwidth",
  softtabstop = "softtabstop",
  sts = "softtabstop",
  expandtab = "expandtab",
  et = "expandtab",
  -- The buffer-local override of the global `regexsyntax` dialect for `/` and
  -- `:s`. `vim.bo.regexsyntax = "vim"` pins this buffer; reads return the
  -- *effective* dialect (the override resolved against the global).
  regexsyntax = "regexsyntax",
  rxs = "regexsyntax",
}
-- Core defaults, the safety net when the mirror hasn't been pushed for a buffer.
-- Match nxvim's core: tabstop 4, with shiftwidth/softtabstop following it via
-- their sentinels (0 = follow tabstop, -1 = follow shiftwidth); regexsyntax
-- "pcre" (the buffer follows the global, whose default is pcre).
local BUF_OPT_DEFAULT =
  { tabstop = 4, shiftwidth = 0, softtabstop = -1, expandtab = false, regexsyntax = "pcre" }

local function bo_get(bufnr, opt)
  local canon = BUF_OPT_CANON[opt]
  if canon then
    local mirror = vim._bo_mirror[bufnr]
    if mirror ~= nil and mirror[canon] ~= nil then return mirror[canon] end
    return BUF_OPT_DEFAULT[canon]
  end
  -- `modified` is read-only buffer *state* (not a settable option), mirrored by
  -- the server so a `'tabline'`/statusline label can read `vim.bo[n].modified`.
  if opt == "modified" or opt == "mod" then
    local mirror = vim._bo_mirror[bufnr]
    return (mirror ~= nil and mirror.modified) or false
  end
  local store = vim._bo_store[bufnr]
  if store ~= nil and store[opt] ~= nil then return store[opt] end
  if opt == "filetype" or opt == "ft" then return (vim._cur_buf or {}).filetype end
  return nil
end
local function bo_set(bufnr, opt, value)
  local canon = BUF_OPT_CANON[opt]
  if canon then
    -- Queue the change for the core and update the mirror so a read-after-write
    -- within this chunk is consistent (the server overwrites it on the next push).
    vim._buf_set_option(bufnr, canon, value)
    vim._bo_mirror[bufnr] = vim._bo_mirror[bufnr] or {}
    vim._bo_mirror[bufnr][canon] = value
    return
  end
  vim._bo_store[bufnr] = vim._bo_store[bufnr] or {}
  vim._bo_store[bufnr][opt] = value
end
local function bo_proxy(bufnr)
  bufnr = vim._resolve_bufnr(bufnr)
  return setmetatable({}, {
    __index = function(_, opt) return bo_get(bufnr, opt) end,
    __newindex = function(_, opt, value) bo_set(bufnr, opt, value) end,
  })
end
vim.bo = setmetatable({}, {
  __index = function(_, k)
    -- numeric key -> per-buffer proxy; option name -> current-buffer value.
    if type(k) == "number" then return bo_proxy(k) end
    return bo_get(vim._resolve_bufnr(0), k)
  end,
  __newindex = function(_, k, value) bo_set(vim._resolve_bufnr(0), k, value) end,
})

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
