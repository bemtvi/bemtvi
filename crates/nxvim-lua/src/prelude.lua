-- nxvim Lua prelude: the pure-Lua part of the `vim.*` standard library, loaded
-- once at VM init right after the Rust-backed bridge. It mirrors neovim's
-- runtime/lua/vim/shared.lua for the subset real plugins (catppuccin first)
-- depend on. The editor-touching functions — vim.cmd, vim.api.nvim_command /
-- nvim_echo, vim.fn.* — are installed from Rust; everything here is plain Lua
-- layered on top of them. Tables prefixed `_` (vim._user_commands, …) are
-- nxvim-internal registries the server reads back.

local vim = vim

-- ----- bit: LuaJIT-compatible bit ops on PUC Lua 5.1 ------------------------

-- neovim runs LuaJIT, which ships a global `bit` library; nxvim runs PUC Lua
-- 5.1, which has neither `bit` nor 5.2's `bit32`. Plugins reach for it as
-- `bit or bit32` (catppuccin hashes its config with djb2 + xor), so provide a
-- faithful pure-Lua implementation with LuaJIT's 32-bit two's-complement
-- semantics: results are normalized to the signed [-2^31, 2^31) range and shift
-- counts are taken mod 32. Only installed when absent, so a real LuaJIT host
-- keeps its native (faster) version.
if not bit then
  local POW = {}
  for i = 0, 32 do POW[i] = 2 ^ i end
  local M32 = POW[32]

  -- Wrap to the unsigned 32-bit range [0, 2^32).
  local function u32(x) return x % M32 end
  -- Wrap to LuaJIT's signed 32-bit result range.
  local function tobit(x)
    x = u32(x)
    if x >= POW[31] then x = x - M32 end
    return x
  end

  -- Apply `f` (operating on single bits) across all 32 bit positions.
  local function bitwise(a, b, f)
    a, b = u32(a), u32(b)
    local r = 0
    for i = 0, 31 do
      local abit, bbit = a % 2, b % 2
      if f(abit, bbit) == 1 then r = r + POW[i] end
      a, b = (a - abit) / 2, (b - bbit) / 2
    end
    return tobit(r)
  end

  bit = {
    tobit = tobit,
    band = function(a, b) return bitwise(a, b, function(x, y) return x * y end) end,
    bor = function(a, b) return bitwise(a, b, function(x, y) return (x + y > 0) and 1 or 0 end) end,
    bxor = function(a, b) return bitwise(a, b, function(x, y) return (x ~= y) and 1 or 0 end) end,
    bnot = function(a) return tobit(-1 - u32(a)) end,
    lshift = function(a, n) return tobit(u32(a) * POW[n % 32]) end,
    rshift = function(a, n) return tobit(math.floor(u32(a) / POW[n % 32])) end,
    arshift = function(a, n) return tobit(math.floor(tobit(a) / POW[n % 32])) end,
  }
end

-- ----- option / variable stores ---------------------------------------------

-- vim.g: global variables. Plain storage; reading an unset key yields nil.
vim.g = vim.g or {}

-- vim.o: editor options. Only a few are meaningful today (background,
-- termguicolors); the rest are stored so plugins can set/read them freely.
vim.o = vim.o or {
  background = "dark",
  termguicolors = false,
  winblend = 0,
  pumblend = 0,
}

-- vim.opt: in neovim each field is a rich Option object, but the colorscheme
-- load path only uses scalar get/set, so a thin proxy over vim.o suffices.
vim.opt = setmetatable({}, {
  __index = function(_, k) return vim.o[k] end,
  __newindex = function(_, k, v) vim.o[k] = v end,
})

-- vim.env: process environment, read through to the host (writes shadow locally).
vim.env = setmetatable({}, {
  __index = function(_, k) return os.getenv(k) end,
})

vim.log = { levels = { TRACE = 0, DEBUG = 1, INFO = 2, WARN = 3, ERROR = 4, OFF = 5 } }

-- ----- table / list / string helpers ----------------------------------------

function vim.tbl_isempty(t) return next(t) == nil end

function vim.tbl_contains(t, value)
  for _, v in pairs(t) do
    if v == value then return true end
  end
  return false
end

function vim.tbl_keys(t)
  local keys = {}
  for k in pairs(t) do keys[#keys + 1] = k end
  return keys
end

function vim.tbl_values(t)
  local values = {}
  for _, v in pairs(t) do values[#values + 1] = v end
  return values
end

-- vim.tbl_get(o, ...): follow the `...` keys into nested table `o`, returning the
-- value reached or nil if any step is missing (or hits a non-table before the
-- last key). The safe nested access `lsp/<server>.lua` configs use to read deep
-- settings (e.g. rust_analyzer's `settings['rust-analyzer'].cargo.sysrootSrc`).
function vim.tbl_get(o, ...)
  local keys = { ... }
  if #keys == 0 then return nil end
  for i, k in ipairs(keys) do
    if type(o) ~= "table" then return nil end
    o = o[k]
    if o == nil then return nil end
  end
  return o
end

function vim.tbl_filter(f, t)
  local out = {}
  for _, v in ipairs(t) do
    if f(v) then out[#out + 1] = v end
  end
  return out
end

function vim.tbl_map(f, t)
  local out = {}
  for k, v in pairs(t) do out[k] = f(v) end
  return out
end

-- vim.tbl_flatten(t): a single list with every nested list flattened into it
-- (depth-first). Deprecated in neovim but still called by `lspconfig.util`.
function vim.tbl_flatten(t)
  local out = {}
  local function flatten(list)
    for _, v in ipairs(list) do
      if type(v) == "table" then flatten(v) else out[#out + 1] = v end
    end
  end
  flatten(t)
  return out
end

function vim.deepcopy(orig)
  if type(orig) ~= "table" then return orig end
  local copy = {}
  for k, v in pairs(orig) do copy[vim.deepcopy(k)] = vim.deepcopy(v) end
  return setmetatable(copy, getmetatable(orig))
end

-- Merge `...` maps into one. `behavior` is "force" | "keep" | "error". Nested
-- tables merge recursively; scalar conflicts resolve per `behavior`.
function vim.tbl_deep_extend(behavior, ...)
  local result = {}
  local function merge(dst, src)
    for k, v in pairs(src) do
      if type(v) == "table" and type(dst[k]) == "table" then
        merge(dst[k], v)
      elseif dst[k] == nil or behavior == "force" then
        dst[k] = vim.deepcopy(v)
      elseif behavior == "error" then
        error("key found in more than one map: " .. tostring(k))
      end -- "keep": leave dst[k] as-is
    end
  end
  for i = 1, select("#", ...) do merge(result, (select(i, ...))) end
  return result
end

-- Shallow variant of tbl_deep_extend.
function vim.tbl_extend(behavior, ...)
  local result = {}
  for i = 1, select("#", ...) do
    for k, v in pairs((select(i, ...))) do
      if result[k] == nil or behavior == "force" then
        result[k] = v
      elseif behavior == "error" then
        error("key found in more than one map: " .. tostring(k))
      end
    end
  end
  return result
end

function vim.list_extend(dst, src, start, finish)
  start = start or 1
  finish = finish or #src
  for i = start, finish do dst[#dst + 1] = src[i] end
  return dst
end

function vim.startswith(s, prefix) return s:sub(1, #prefix) == prefix end
function vim.endswith(s, suffix) return suffix == "" or s:sub(-#suffix) == suffix end

function vim.split(s, sep, opts)
  opts = opts or {}
  local parts, pos = {}, 1
  while true do
    local from, to = string.find(s, sep, pos, opts.plain)
    if not from then
      parts[#parts + 1] = string.sub(s, pos)
      break
    end
    parts[#parts + 1] = string.sub(s, pos, from - 1)
    pos = to + 1
  end
  if opts.trimempty then
    while #parts > 0 and parts[#parts] == "" do parts[#parts] = nil end
    while #parts > 0 and parts[1] == "" do table.remove(parts, 1) end
  end
  return parts
end

-- ----- minimal vim.iter ------------------------------------------------------
-- A small chainable iterator over list-like tables: map / filter / each / fold
-- / totable, enough for what the colorscheme load path reaches for.
local Iter = {}
Iter.__index = Iter

-- vim.iter(src[, state, ctrl]): wrap a list-like table OR a Lua iterator triple
-- in a chainable iterator. The triple form is what `vim.iter(vim.fs.parents(p))`
-- passes — `vim.fs.parents` returns `(fn, state, start)`, which Lua spreads as
-- three args here — so the ancestors are drained eagerly into the item list.
function vim.iter(src, state, ctrl)
  local items = {}
  if type(src) == "function" then
    local var = ctrl
    while true do
      local v = src(state, var)
      if v == nil then break end
      var = v
      items[#items + 1] = v
    end
  elseif type(src) == "table" then
    for _, v in ipairs(src) do items[#items + 1] = v end
  end
  return setmetatable({ _items = items }, Iter)
end

-- Iter:find(pred): the first item for which `pred(item)` is truthy (or, when
-- `pred` is a plain value, the first item equal to it), else nil.
function Iter:find(pred)
  for _, v in ipairs(self._items) do
    if type(pred) == "function" then
      if pred(v) then return v end
    elseif v == pred then
      return v
    end
  end
  return nil
end

-- Iter:any(pred): true iff `pred(item)` is truthy for some item.
function Iter:any(pred)
  for _, v in ipairs(self._items) do
    if pred(v) then return true end
  end
  return false
end

-- Iter:flatten(): flatten one level of list-valued items into the stream.
function Iter:flatten()
  local out = {}
  for _, v in ipairs(self._items) do
    if type(v) == "table" then
      for _, inner in ipairs(v) do out[#out + 1] = inner end
    else
      out[#out + 1] = v
    end
  end
  self._items = out
  return self
end

function Iter:map(f)
  local out = {}
  for _, v in ipairs(self._items) do
    local r = f(v)
    if r ~= nil then out[#out + 1] = r end
  end
  self._items = out
  return self
end

function Iter:filter(f)
  local out = {}
  for _, v in ipairs(self._items) do
    if f(v) then out[#out + 1] = v end
  end
  self._items = out
  return self
end

function Iter:each(f)
  for _, v in ipairs(self._items) do f(v) end
end

function Iter:fold(acc, f)
  for _, v in ipairs(self._items) do acc = f(acc, v) end
  return acc
end

function Iter:totable() return self._items end

-- ----- misc ------------------------------------------------------------------

-- vim._notimpl(name): the loud-failure funnel for not-yet-implemented surface.
-- Records `name` into vim._notimpl_hits (a set, so a future `:checkhealth` /
-- `vim.lsp._report` can enumerate which gaps a real config actually hit) and
-- raises a named error. A stub that quietly returns a fake/empty value makes a
-- broken server look configured; routing every hollow stub through here turns
-- "we think it works" into a concrete, trackable list of what to build (the
-- guiding principle of docs/lsp-completion-plan.md). `level` (default 2) blames
-- the stub's call site in the error position; the message names the function.
vim._notimpl_hits = vim._notimpl_hits or {}
function vim._notimpl(name, level)
  vim._notimpl_hits[name] = true
  error("nxvim: not implemented: " .. name, level or 2)
end

-- ----- the async runtime: the deferred-callback registry ---------------------
-- The spine of nxvim's event loop. A deferred function (vim.schedule, defer_fn,
-- a vim.uv timer, a vim.system on_exit) is stored by integer id in vim._cb_fns
-- and run *later*, by id, from Rust — the vim._keymap_fns / vim._run_keymap shape
-- applied to async work. vim._next_cb_id() allocates a fresh id; vim._run_cb runs
-- one and (unless `keep`) drops it so the registry can't grow unbounded.
vim._cb_fns = vim._cb_fns or {}
vim._cb_seq = vim._cb_seq or 0
function vim._next_cb_id()
  vim._cb_seq = vim._cb_seq + 1
  return vim._cb_seq
end

-- Run the callback registered under `id`, forwarding any extra args. `keep` is
-- false for one-shots (vim.schedule, defer_fn, a system on_exit) — the entry is
-- dropped *before* the call so a throwing or re-scheduling callback still leaves
-- the registry clean — and true for a repeating timer, whose fn is retained
-- across fires (its :stop()/:close() drops it). A nil id (already stopped) is a
-- silent no-op. The return value is forwarded so an <expr>-like caller could read
-- it; current callers ignore it.
function vim._run_cb(id, keep, ...)
  local fn = vim._cb_fns[id]
  if not keep then vim._cb_fns[id] = nil end
  if fn then return fn(...) end
end

-- vim.schedule(fn): defer `fn` to the end of the current convergence — it runs
-- after the work that scheduled it settles, no longer nested in the caller's
-- stack frame (the strict improvement over the old inline `fn()`), but still
-- within the same input tick (not a later wall-clock turn; that is defer_fn).
-- This is exactly what the colorscheme's "defer to avoid reentrancy" wants.
function vim.schedule(fn)
  local id = vim._next_cb_id()
  vim._cb_fns[id] = fn
  vim._schedule(id) -- Rust bridge: push LoopOp::Schedule{id} onto Shared.loop_ops
end

-- vim.schedule_wrap(fn): return a function that, when called, schedules `fn` with
-- whatever arguments it was given — a common plugin idiom for "run this callback
-- safely on the loop". The captured args ride into the deferred call via a closure.
function vim.schedule_wrap(fn)
  return function(...)
    local args = { ... }
    local n = select("#", ...)
    vim.schedule(function() fn(table.unpack and table.unpack(args, 1, n) or unpack(args, 1, n)) end)
  end
end

-- pid registry for async vim.system handles. The event-loop actor reports a
-- spawned child's OS pid back to the server, which records it here keyed by the
-- handle's callback id; the handle's `.pid` reads through this table (nil until
-- the spawn lands, since it can't be known synchronously on a single thread).
vim._proc_pids = vim._proc_pids or {}
function vim._set_proc_pid(id, pid) vim._proc_pids[id] = pid end

function vim.notify(msg, _level, _opts)
  if type(msg) == "table" then msg = table.concat(msg, "\n") end
  print(msg)
end

-- vim.notify_once: in neovim this dedups by message; we have no message history
-- to dedup against during a one-shot colorscheme load, so route to notify.
function vim.notify_once(msg, level, opts) return vim.notify(msg, level, opts) end

-- vim.treesitter: nxvim runs its own out-of-process treesitter highlighter, not
-- neovim's in-VM one, so this namespace is otherwise absent. catppuccin probes
-- `vim.treesitter.highlighter.hl_map` purely to detect ancient neovim 0.7; an
-- empty `highlighter` makes that field nil, so the modern path is taken.
vim.treesitter = vim.treesitter or { highlighter = {} }

function vim.inspect(value)
  local function ins(v, indent)
    if type(v) ~= "table" then
      if type(v) == "string" then return string.format("%q", v) end
      return tostring(v)
    end
    local parts = {}
    for k, val in pairs(v) do
      parts[#parts + 1] = indent .. "  " .. tostring(k) .. " = " .. ins(val, indent .. "  ")
    end
    return "{\n" .. table.concat(parts, ",\n") .. "\n" .. indent .. "}"
  end
  return ins(value, "")
end

-- ----- API surface stored purely in Lua --------------------------------------
-- Registration that needn't touch the editor lives in Lua tables; the server
-- reads them when it must (e.g. dispatching a user command typed as `:Foo`).

vim._user_commands = vim._user_commands or {}
vim._autocmds = vim._autocmds or {}
vim._augroups = vim._augroups or {}
local augroup_seq, autocmd_seq = 0, 0

-- vim._cur_buf: the current-buffer snapshot the server refreshes (via
-- vim._set_cur_buf) immediately before firing a buffer/mode autocmd, so a
-- callback can resolve "the buffer that fired" — nvim_buf_get_name(0) and
-- expand('%') read it. An interim until a real per-bufnr registry exists; with
-- the core single-message-at-a-time it can't go stale mid-dispatch.
vim._cur_buf = vim._cur_buf or { bufnr = 0, name = "", filetype = "" }

function vim._set_cur_buf(bufnr, name, filetype)
  vim._cur_buf = { bufnr = bufnr or 0, name = name or "", filetype = filetype or "" }
end

-- vim._bufs / vim._cur_cursor / vim._cur_win: the Rust→Lua buffer mirror the
-- buffer-read API (Phase 6) resolves against. The server refreshes it via
-- vim._set_buf_mirror before running any Lua that can read buffer or cursor
-- state, so nvim_buf_get_lines / nvim_win_get_cursor / nvim_buf_is_loaded read
-- live data without reaching the Server. vim._bufs[bufnr] = { lines, name,
-- loaded }; nvim_buf_set_lines write-through mutates `lines` here directly so a
-- read-after-write within one chunk stays consistent (the real buffer catches up
-- when the server drains the queued BufOp).
vim._bufs = vim._bufs or {}
vim._cur_cursor = vim._cur_cursor or { row = 1, col = 0 }
vim._cur_win = vim._cur_win or 1000
-- Per-buffer option store backing vim.bo / nvim_set_option_value (Phase 6); the
-- table is created here so the earlier-defined setter can index it safely.
vim._bo_store = vim._bo_store or {}

function vim._set_buf_mirror(entries, row, col, win)
  -- The server omits `lines` for a buffer whose changedtick is unchanged (the
  -- cheap cursor-moved-no-edit path); keep the prior `lines` in that case.
  for bufnr, entry in pairs(entries) do
    if entry.lines == nil then
      local prev = vim._bufs[bufnr]
      if prev then entry.lines = prev.lines end
    end
    entry.loaded = true
  end
  vim._bufs = entries
  vim._cur_cursor = { row = row or 1, col = col or 0 }
  vim._cur_win = win or 1000
end

-- Resolve a buffer handle to a concrete bufnr (0 / nil -> current buffer), the
-- one place the buffer-read API maps neovim's "0 means current" convention.
function vim._resolve_bufnr(bufnr)
  if bufnr == nil or bufnr == 0 then return (vim._cur_buf or {}).bufnr or 0 end
  return bufnr
end

-- Normalize a neovim line index against a buffer of `n` real lines, shared by
-- nvim_buf_get_lines and nvim_buf_set_lines (and mirrored on the Rust side so the
-- write-through and the real apply can't disagree): negatives count from the end
-- (`-1` == one past the last line), then clamp into [0, n]. `strict` raises on an
-- out-of-range index instead of clamping (neovim's strict_indexing).
function vim._norm_line_index(i, n, strict)
  local orig = i
  if i < 0 then i = n + i + 1 end
  if strict and (orig > n or i < 0) then
    error("Index out of bounds", 3)
  end
  if i < 0 then i = 0 elseif i > n then i = n end
  return i
end

function vim.api.nvim_create_user_command(name, command, _opts)
  vim._user_commands[name] = command
end

-- nvim_buf_create_user_command(buffer, name, command, opts): in neovim this
-- registers a *buffer-local* command; nxvim has no per-buffer command registry
-- yet, so it registers globally (the buffer scope is ignored). Enough for an
-- `on_attach` that defines a convenience command (e.g. rust_analyzer's
-- `:LspCargoReload`) to load without error.
function vim.api.nvim_buf_create_user_command(_buffer, name, command, _opts)
  vim._user_commands[name] = command
end

-- nvim_create_augroup(name[, {clear=…}]): define (or look up) an augroup. When
-- the group already exists and `clear` is set (the default), its autocmds are
-- removed first — so re-sourcing a config that recreates its groups doesn't
-- double-register. The group id is stable across recreation (callers store it
-- and pass it as `opts.group` to nvim_create_autocmd).
function vim.api.nvim_create_augroup(name, opts)
  opts = opts or {}
  local clear = opts.clear ~= false -- absent → clear, matching neovim's default
  local id = vim._augroups[name]
  if id and clear then
    vim._autocmds = vim.tbl_filter(function(au) return au.group ~= id end, vim._autocmds)
  end
  if not id then
    augroup_seq = augroup_seq + 1
    id = augroup_seq
    vim._augroups[name] = id
  end
  return id
end

-- nvim_create_autocmd(event, opts): register a callback/command for `event`.
-- `opts.group` (numeric id or augroup name) ties it to a group so a later
-- `clear` can drop it; `opts.buffer` makes it buffer-local (only fires for that
-- buffer; 0 resolves to the current snapshot buffer at registration time).
function vim.api.nvim_create_autocmd(event, opts)
  opts = opts or {}
  autocmd_seq = autocmd_seq + 1
  local group = opts.group
  if type(group) == "string" then group = vim._augroups[group] end
  local buffer = opts.buffer
  if buffer == 0 then buffer = vim._cur_buf and vim._cur_buf.bufnr or 0 end
  vim._autocmds[#vim._autocmds + 1] =
    { id = autocmd_seq, event = event, opts = opts, group = group, buffer = buffer }
  return autocmd_seq
end

-- nvim_del_autocmd(id): remove the autocmd with this id, so it stops firing.
function vim.api.nvim_del_autocmd(id)
  vim._autocmds = vim.tbl_filter(function(au) return au.id ~= id end, vim._autocmds)
end

-- Fire the registered autocmds for `event` whose pattern matches `pattern`,
-- with optional buffer context. Called from Rust (LuaRuntime::fire_autocmd*)
-- when the editor triggers an event, and from nvim_exec_autocmds. A function
-- handler runs with the callback args table `{id, event, match, buf, file}`; a
-- string `command` is queued as an ex-command. Match rules: event equals (or is
-- in) the registered event; pattern is nil/"*", equals `pattern`, or is in the
-- registered pattern list; a buffer-local autocmd only fires for its `buffer`.
-- `buf`/`file` are nil for back-compat callers (e.g. ColorScheme), in which
-- case `file` falls back to `pattern` (the old behavior). `data` is the optional
-- `args.data` payload (LspAttach/LspDetach carry `{ client_id = … }`); nil otherwise.
function vim._fire(event, pattern, buf, file, data)
  for _, au in ipairs(vim._autocmds) do
    local ev = au.event
    local ev_ok = ev == event or (type(ev) == "table" and vim.tbl_contains(ev, event))
    if ev_ok then
      local pat = au.opts.pattern
      local pat_ok = pat == nil or pat == "*" or pat == pattern
        or (type(pat) == "table" and vim.tbl_contains(pat, pattern))
      local buf_ok = au.buffer == nil or au.buffer == buf
      if pat_ok and buf_ok then
        local cb = au.opts.callback
        if type(cb) == "function" then
          cb({ id = au.id, event = event, match = pattern, buf = buf, file = file or pattern, data = data })
        elseif type(au.opts.command) == "string" then
          vim.cmd(au.opts.command)
        end
      end
    end
  end
end

-- nvim_exec_autocmds(event, opts): fire `event` (or a list of events) manually.
-- `opts.pattern` (string or list) is matched as in registration; `opts.buffer`
-- supplies the buffer context (defaulting to the current snapshot buffer), and
-- the callback's `args.file` is the snapshot name when firing for it.
function vim.api.nvim_exec_autocmds(event, opts)
  opts = opts or {}
  local events = type(event) == "table" and event or { event }
  local buf = opts.buffer
  if buf == nil then buf = vim._cur_buf and vim._cur_buf.bufnr or nil end
  local file
  if vim._cur_buf and buf == vim._cur_buf.bufnr then file = vim._cur_buf.name end
  local patterns = opts.pattern
  for _, ev in ipairs(events) do
    if type(patterns) == "table" then
      for _, p in ipairs(patterns) do vim._fire(ev, p, buf, file) end
    else
      vim._fire(ev, patterns, buf, file)
    end
  end
end

-- nvim_get_autocmds(opts): introspect the registered autocmds — a debugging
-- affordance for confirming what `clear`/`del` left behind. Returns a list of
-- `{id, event, group, group_name, pattern, buffer, command}` entries, optionally
-- filtered by `opts.event` (string or list) and `opts.group` (id or name). Run
-- it interactively as `:lua print(vim.inspect(vim.api.nvim_get_autocmds({})))`.
function vim.api.nvim_get_autocmds(opts)
  opts = opts or {}
  local want_events = opts.event and (type(opts.event) == "table" and opts.event or { opts.event })
  local want_group = opts.group
  if type(want_group) == "string" then want_group = vim._augroups[want_group] end
  -- reverse map: group id → its registered name, for human-readable output
  local group_name = {}
  for nm, id in pairs(vim._augroups) do group_name[id] = nm end
  local out = {}
  for _, au in ipairs(vim._autocmds) do
    -- match if any requested event is among the autocmd's events
    local ev_ok = true
    if want_events then
      ev_ok = false
      local evs = type(au.event) == "table" and au.event or { au.event }
      for _, w in ipairs(want_events) do
        if vim.tbl_contains(evs, w) then ev_ok = true break end
      end
    end
    local group_ok = want_group == nil or au.group == want_group
    if ev_ok and group_ok then
      out[#out + 1] = {
        id = au.id,
        event = au.event,
        group = au.group,
        group_name = au.group and group_name[au.group] or nil,
        pattern = au.opts.pattern,
        buffer = au.buffer,
        command = type(au.opts.command) == "string" and au.opts.command or nil,
      }
    end
  end
  return out
end

-- nvim_buf_get_name(bufnr): the snapshot buffer's name when `bufnr` is 0/nil or
-- matches the snapshot, else "". Snapshot-backed (vim._cur_buf) as an interim
-- until a real per-bufnr registry exists. (A separate, core-backed
-- nvim_buf_get_name *RPC* method serves remote clients; this is the in-VM Lua
-- binding autocmd callbacks reach for.)
function vim.api.nvim_buf_get_name(bufnr)
  local cur = vim._cur_buf or { bufnr = 0, name = "" }
  if bufnr == nil or bufnr == 0 or bufnr == cur.bufnr then return cur.name end
  return ""
end

-- A few more vim.api the configs touch. `nvim_get_current_buf` resolves against
-- the single-buffer snapshot (faithful: it returns the real current buffer). The
-- window/cursor/line-access getters (Phase 6) read the `vim._bufs` / `vim._cur_*`
-- mirror the server refreshes before running Lua, so they return live state, and
-- `nvim_buf_set_lines` write-through updates the mirror then queues the real edit.
-- (`nvim_create_augroup`/`_autocmd`/`nvim_buf_get_name`/`nvim_echo` are the real,
-- behavior-carrying ones, defined elsewhere.)
function vim.api.nvim_get_current_buf() return (vim._cur_buf or {}).bufnr or 0 end

-- Single-window nxvim: one window handle, and the cursor is the editor cursor.
function vim.api.nvim_get_current_win() return vim._cur_win or 1000 end
function vim.api.nvim_win_get_cursor(_win)
  local c = vim._cur_cursor or { row = 1, col = 0 }
  return { c.row, c.col }
end

function vim.api.nvim_buf_is_loaded(bufnr)
  return vim._bufs[vim._resolve_bufnr(bufnr)] ~= nil
end

function vim.api.nvim_buf_get_lines(bufnr, start, end_, strict)
  local buf = vim._bufs[vim._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then
    if strict then error("Invalid buffer id", 2) end
    return {}
  end
  local lines = buf.lines
  local n = #lines
  local s = vim._norm_line_index(start, n, strict)
  local e = vim._norm_line_index(end_, n, strict)
  if e < s then e = s end
  local out = {}
  for i = s + 1, e do
    out[#out + 1] = lines[i]
  end
  return out
end

function vim.api.nvim_buf_set_lines(bufnr, start, end_, strict, repl)
  local id = vim._resolve_bufnr(bufnr)
  local buf = vim._bufs[id]
  if not buf or not buf.lines then
    if strict then error("Invalid buffer id", 2) end
    return
  end
  local lines = buf.lines
  local n = #lines
  local s = vim._norm_line_index(start, n, strict)
  local e = vim._norm_line_index(end_, n, strict)
  if e < s then e = s end
  -- Write-through: splice the mirror so a read-after-write within this chunk is
  -- consistent, then queue the real edit (the server re-derives the byte range).
  local updated = {}
  for i = 1, s do updated[#updated + 1] = lines[i] end
  for i = 1, #repl do updated[#updated + 1] = repl[i] end
  for i = e + 1, n do updated[#updated + 1] = lines[i] end
  buf.lines = updated
  vim._buf_set_lines(id, start, end_, repl)
end

-- nvim_set_option_value(name, value, opts): set a (buffer-local) option. opts.buf
-- targets a specific buffer; otherwise the current buffer. Backed by the same
-- per-buffer store as vim.bo (observable, see the note there).
function vim.api.nvim_set_option_value(name, value, opts)
  opts = opts or {}
  local buf = opts.buf and vim._resolve_bufnr(opts.buf) or vim._resolve_bufnr(0)
  vim._bo_store[buf] = vim._bo_store[buf] or {}
  vim._bo_store[buf][name] = value
end

-- vim.fn.expand: the `%` (current file) forms autocmd callbacks use to resolve
-- paths, backed by the snapshot. Supports `%`, `%:p` (absolute — for the first
-- cut the stored path is taken as-is), `%:h` (head/dir), `%:t` (tail/basename),
-- and `%:p:h`. Unknown expressions return the stored name unchanged.
function vim.fn.expand(expr)
  local cur = vim._cur_buf or { bufnr = 0, name = "" }
  local name = cur.name or ""
  if expr == "%" or expr == "%:p" then
    return name
  elseif expr == "%:h" or expr == "%:p:h" then
    return name:match("^(.*)/[^/]*$") or ""
  elseif expr == "%:t" then
    return name:match("[^/]*$") or name
  end
  return name
end

-- vim.api.nvim_set_hl is installed from Rust (it captures the group definition
-- for the server to fold into the core highlight registry), so it is not
-- (re)defined here — doing so would shadow the Rust-backed version.

-- ----- vim.cmd: callable AND indexable ---------------------------------------
-- vim.cmd("…") queues a raw ex-command (the Rust function installed earlier);
-- vim.cmd.colorscheme("x") / vim.cmd.set("number") build "<name> <args…>".
do
  local raw_cmd = vim.cmd
  -- An <expr> mapping RHS must not change editor state (textlock): while
  -- vim._expr_lock is set, running an ex-command raises instead of mutating.
  local function raw(c)
    if vim._expr_lock then
      error("E5555: <expr> mapping must not change the editor (vim.cmd is blocked)", 0)
    end
    return raw_cmd(c)
  end
  local function build(name, ...)
    local first = ...
    if type(first) == "table" then
      local s = name
      if first.bang then s = s .. "!" end
      if first.args then s = s .. " " .. table.concat(first.args, " ") end
      return raw(s)
    end
    local parts = {}
    for i = 1, select("#", ...) do parts[i] = tostring((select(i, ...))) end
    local s = name
    if #parts > 0 then s = s .. " " .. table.concat(parts, " ") end
    return raw(s)
  end
  vim.cmd = setmetatable({}, {
    __call = function(_, c) return raw(c) end,
    __index = function(_, name)
      return function(...) return build(name, ...) end
    end,
  })
end

-- ----- keymaps ---------------------------------------------------------------
-- vim.keymap.set / .del store entries in a pure-Lua registry the server reads
-- back as data (unlike autocmds, whose *matching* stays in Lua); the server
-- compiles the snapshot into per-mode prefix tries and matches keystrokes there.
-- A function RHS is held in vim._keymap_fns keyed by the entry's stable id and
-- invoked from Rust via vim._run_keymap(id) — the run_user_command analogue.
-- Every mutation bumps vim._keymaps_version so the server rebuilds its tries
-- only when the registry actually changed (checked once per input batch).

vim._keymaps = vim._keymaps or {}
vim._keymap_fns = vim._keymap_fns or {}
vim._keymaps_version = vim._keymaps_version or 0
local keymap_seq = 0

vim.keymap = vim.keymap or {}

-- Normalize the `mode` argument to a list of mode codes. A bare string is one
-- mode (`'n'`, `'x'`, `''` = all); a list passes through unchanged. Each code's
-- expansion to the editor modes it covers (v/x → Visual+VisualLine, `''` → all)
-- is the server's job — it owns the per-mode tries.
local function keymap_modes(mode)
  if type(mode) == "table" then return mode end
  return { mode }
end

-- Expand <leader>/<localleader> in an LHS to the current mapleader/maplocalleader
-- (vim.g.mapleader / vim.g.maplocalleader, each defaulting to "\" as in vim),
-- matching neovim's *set-time* expansion: the leader in force when the map is
-- defined is baked in, so a later mapleader change doesn't retroactively move it.
-- The notation names match case-insensitively (`<Leader>` == `<leader>`). The
-- replacement is returned from a function so gsub takes it literally (a leader
-- like "%" or "\" is not reinterpreted as a pattern/replacement metacharacter).
local function keymap_expand_leader(lhs)
  local leader = vim.g.mapleader
  if leader == nil then leader = "\\" end
  local localleader = vim.g.maplocalleader
  if localleader == nil then localleader = "\\" end
  lhs = lhs:gsub("<[lL][eE][aA][dD][eE][rR]>", function() return leader end)
  lhs = lhs:gsub("<[lL][oO][cC][aA][lL][lL][eE][aA][dD][eE][rR]>", function()
    return localleader
  end)
  return lhs
end

-- Resolve a `buffer` opt to a concrete buffer number: 0 means "the current
-- buffer", resolved at call-time against the snapshot the server refreshes (the
-- same convention nvim_create_autocmd uses), so a buffer-local map declared with
-- `buffer = 0` is pinned to the buffer that was current when it was set.
local function keymap_resolve_buffer(buffer)
  if buffer == 0 then return vim._cur_buf and vim._cur_buf.bufnr or 0 end
  return buffer
end

-- Does a mapping already exist for `lhs` overlapping any of `modes` at the given
-- `buffer` scope? Backs `<unique>` (opts.unique): vim errors (E227) rather than
-- overwrite. Compares the already-leader-expanded `lhs`/resolved `buffer` the
-- caller holds, and treats any mode overlap as a clash.
local function keymap_clashes(modes, lhs, buffer)
  local want = {}
  for _, m in ipairs(modes) do want[m] = true end
  for _, e in ipairs(vim._keymaps) do
    if e.lhs == lhs and e.buffer == buffer then
      for _, m in ipairs(e.modes) do
        if want[m] then return true end
      end
    end
  end
  return false
end

-- Register one keymap entry into vim._keymaps — the shared core of vim.keymap.set
-- and the lower-level nvim_set_keymap / nvim_buf_set_keymap. `modes` is a list of
-- mode codes; `rhs` a function (stored in vim._keymap_fns) or a string (fed as
-- keys). `opts` is a normalized table the callers fill in: `noremap` (set defaults
-- it true, the nvim_* family false — design D5), `buffer`, `desc`, `default`, and
-- the Phase-4 flags `nowait` / `silent` / `expr` (read by the matcher / fire path)
-- and `unique` (a set-time check, never stored). `<leader>` is expanded in both LHS
-- and a string RHS at set-time, matching neovim. Bumps the version so the server
-- rebuilds its tries.
local function keymap_register(modes, lhs, rhs, opts)
  lhs = keymap_expand_leader(lhs)
  local buffer = keymap_resolve_buffer(opts.buffer)
  if opts.unique and keymap_clashes(modes, lhs, buffer) then
    error("E227: mapping already exists for " .. lhs, 0)
  end
  keymap_seq = keymap_seq + 1
  local id = keymap_seq
  local rhs_data
  if type(rhs) == "function" then
    vim._keymap_fns[id] = rhs
    rhs_data = { kind = "lua", id = id }
  else
    -- <leader> is expanded in the string RHS too, not just the LHS, matching
    -- neovim — so a remap RHS can name another <leader> mapping.
    rhs_data = { kind = "str", str = keymap_expand_leader(tostring(rhs)) }
  end
  vim._keymaps[#vim._keymaps + 1] = {
    id = id,
    modes = modes,
    lhs = lhs,
    rhs = rhs_data,
    noremap = opts.noremap,
    buffer = buffer,
    desc = opts.desc,
    nowait = opts.nowait or false,
    silent = opts.silent or false,
    expr = opts.expr or false,
    default = opts.default or false,
  }
  vim._keymaps_version = vim._keymaps_version + 1
end

-- Remove the mappings for `lhs` in `modes` at the given `buffer` scope (nil for
-- global, a resolved number for buffer-local) — the shared core of vim.keymap.del
-- and the nvim_*_del_keymap family. A matched entry loses only the requested
-- modes; it survives (with the rest) if it covered more, and is dropped — along
-- with any function RHS it held — only when no modes remain. Re-sourcing a config
-- that re-sets the same map therefore leaves exactly one mapping, so it can't
-- double-fire. Bumps the version so the server rebuilds its tries.
local function keymap_remove(modes, lhs, buffer)
  lhs = keymap_expand_leader(lhs)
  local want = {}
  for _, m in ipairs(modes) do want[m] = true end
  local kept = {}
  for _, e in ipairs(vim._keymaps) do
    if e.lhs == lhs and e.buffer == buffer then
      local remaining = {}
      for _, m in ipairs(e.modes) do
        if not want[m] then remaining[#remaining + 1] = m end
      end
      if #remaining > 0 then
        e.modes = remaining
        kept[#kept + 1] = e
      elseif e.rhs.kind == "lua" then
        vim._keymap_fns[e.id] = nil
      end
    else
      kept[#kept + 1] = e
    end
  end
  vim._keymaps = kept
  vim._keymaps_version = vim._keymaps_version + 1
end

-- vim.keymap.set(mode, lhs, rhs, opts): map `lhs` to `rhs` in `mode`.
-- `rhs` is a function (stored in vim._keymap_fns) or a string (fed as keys).
-- Maps are non-recursive by default (the vim.keymap.set convention); pass
-- `opts.remap = true` for a recursive map whose RHS keys are re-fed through the
-- mapping layer (or, equivalently, `opts.noremap = false`). `opts.desc` is stored
-- but unused; `opts.buffer` ties the map to one buffer (0 = current), `opts.default`
-- marks an overridable built-in — both feed the precedence ladder the server applies.
function vim.keymap.set(mode, lhs, rhs, opts)
  opts = opts or {}
  -- noremap unless either `noremap = false` or `remap = true` is given.
  local noremap = opts.noremap ~= false and not opts.remap
  keymap_register(keymap_modes(mode), lhs, rhs, {
    noremap = noremap,
    buffer = opts.buffer,
    desc = opts.desc,
    default = opts.default,
    nowait = opts.nowait,
    silent = opts.silent,
    expr = opts.expr,
    unique = opts.unique,
  })
end

-- vim.keymap.del(mode, lhs, opts): remove the mapping(s) for `lhs` in `mode`.
-- `opts.buffer` (0 = current) targets a buffer-local map; absent targets globals.
function vim.keymap.del(mode, lhs, opts)
  opts = opts or {}
  keymap_remove(keymap_modes(mode), lhs, keymap_resolve_buffer(opts.buffer))
end

-- The lower-level nvim_set_keymap / nvim_buf_set_keymap (+ their del partners)
-- that vim.keymap.set normalizes onto: single-char `mode`, and — matching the
-- `:map`-family default (design D5) — *remappable* unless `opts.noremap` is set.
-- A function RHS rides `opts.callback` (the API's escape hatch), else `rhs` is the
-- key string. nvim_buf_*_keymap take a leading `buffer` (0 = current).
function vim.api.nvim_set_keymap(mode, lhs, rhs, opts)
  opts = opts or {}
  keymap_register({ mode }, lhs, opts.callback or rhs, {
    noremap = opts.noremap == true,
    buffer = nil,
    desc = opts.desc,
    default = opts.default,
    nowait = opts.nowait,
    silent = opts.silent,
    expr = opts.expr,
    unique = opts.unique,
  })
end

function vim.api.nvim_buf_set_keymap(buffer, mode, lhs, rhs, opts)
  opts = opts or {}
  keymap_register({ mode }, lhs, opts.callback or rhs, {
    noremap = opts.noremap == true,
    buffer = buffer,
    desc = opts.desc,
    default = opts.default,
    nowait = opts.nowait,
    silent = opts.silent,
    expr = opts.expr,
    unique = opts.unique,
  })
end

function vim.api.nvim_del_keymap(mode, lhs)
  keymap_remove({ mode }, lhs, nil)
end

function vim.api.nvim_buf_del_keymap(buffer, mode, lhs)
  keymap_remove({ mode }, lhs, keymap_resolve_buffer(buffer))
end

-- Invoke the function RHS for entry `id` (called from Rust when a Lua-backed
-- mapping fires). A no-op if no function is registered under that id.
function vim._run_keymap(id)
  local fn = vim._keymap_fns[id]
  if fn then fn() end
end

-- Textlock for <expr> mappings. An <expr> RHS must *compute* the keys to feed and
-- not change editor state (vim's textlock); while this is set the mutation funnels
-- (currently vim.cmd) refuse. A simple, honest sandbox: the common offender raises
-- rather than silently no-ops, and the server additionally discards any effects an
-- <expr> RHS queued, so nothing it did leaks regardless.
vim._expr_lock = false

-- Run the <expr> function RHS for entry `id` and return the keys it produced (its
-- return value coerced to a string; nil/false → ""). Runs under vim._expr_lock so
-- vim.cmd refuses; pcall guarantees the lock is cleared even if the RHS throws,
-- after which the error is re-raised for Rust to surface (the mapping then feeds
-- nothing). A no-op id yields "".
function vim._run_keymap_expr(id)
  local fn = vim._keymap_fns[id]
  if not fn then return "" end
  vim._expr_lock = true
  local ok, result = pcall(fn)
  vim._expr_lock = false
  if not ok then
    error(result, 0)
  end
  if result == nil or result == false then return "" end
  return tostring(result)
end

-- The `:ls` panel's <CR> handler: jump to the buffer whose number leads the
-- selected listing line (`"  2 %a "name" line 1"`), then dismiss the list. The
-- core installs this via `vim.panel.on_select` when `:ls` opens its panel, so
-- the buffer list rides the same scripting select path a plugin would use.
function vim._panel_select_buffer(line)
  local n = tonumber(line:match("^%s*(%d+)"))
  if n then
    vim.panel.close()
    vim.cmd("buffer " .. n)
  end
end

-- ----- vim.fs: path helpers --------------------------------------------------
-- The subset of neovim's `vim.fs` the real `lsp/<server>.lua` config files reach
-- for to resolve a workspace root. Pure string/path math layered over the
-- Rust-backed `vim._readdir` / `vim.fn.getftime` / `vim.fn.getcwd` primitives.

vim.fs = vim.fs or {}

-- Join path segments with `/`, collapsing duplicate separators.
function vim.fs.joinpath(...)
  return (table.concat({ ... }, "/"):gsub("//+", "/"))
end

-- Expand a leading `~` and collapse duplicate / trailing slashes. (Minimal: no
-- `..` resolution — the config files don't need it.)
function vim.fs.normalize(path, _opts)
  if type(path) ~= "string" then return path end
  if path == "~" or vim.startswith(path, "~/") then
    path = (os.getenv("HOME") or "") .. path:sub(2)
  end
  path = path:gsub("//+", "/")
  if #path > 1 then path = (path:gsub("/$", "")) end
  return path
end

-- The directory part of `path` ("." when there is none, "/" at the root).
function vim.fs.dirname(path)
  if not path or path == "" then return "." end
  path = path:gsub("/+$", "")
  -- All slashes (the root "/") stripped to "": root's parent is itself, so the
  -- upward walks in vim.fs.root / vim.fs.parents terminate at "/" instead of
  -- escaping to "." (the cwd).
  if path == "" then return "/" end
  local dir = path:match("^(.*)/[^/]*$")
  if dir == nil then return "." end
  if dir == "" then return "/" end
  return dir
end

-- The final component of `path`.
function vim.fs.basename(path)
  if not path then return nil end
  return (path:gsub("/+$", ""):match("[^/]*$"))
end

-- Iterate the ancestors of `start` (each parent in turn, excluding `start`),
-- usable as `for dir in vim.fs.parents(path) do … end`.
function vim.fs.parents(start)
  return function(_, dir)
    local parent = vim.fs.dirname(dir)
    if parent == dir then return nil end
    return parent
  end, nil, start
end

-- Does `path` exist on disk (file or directory)? `getftime` stats both and
-- returns -1 only when the path can't be stat'd.
local function fs_exists(path)
  return vim.fn.getftime(path) ~= -1
end

-- vim.fs.find(names, opts): find paths matching `names` (a name, list of names,
-- or `function(name, path)` predicate). `opts.upward` walks ancestors of
-- `opts.path` (default cwd); otherwise it descends breadth-first. `opts.limit`
-- caps results (default 1). Enough for the root_dir helpers configs use.
function vim.fs.find(names, opts)
  opts = opts or {}
  local matches
  if type(names) == "function" then
    matches = names
  else
    local list = type(names) == "table" and names or { names }
    matches = function(n) return vim.tbl_contains(list, n) end
  end
  local path = opts.path or vim.fn.getcwd()
  local limit = opts.limit or 1
  local results = {}
  local function consider(dir, entry)
    if matches(entry, dir) then
      results[#results + 1] = vim.fs.joinpath(dir, entry)
    end
  end
  if opts.upward then
    local dir = path
    while dir do
      for _, entry in ipairs(vim._readdir(dir)) do
        consider(dir, entry)
        if #results >= limit then return results end
      end
      local parent = vim.fs.dirname(dir)
      if parent == dir then break end
      dir = parent
    end
  else
    local queue, scanned = { path }, 0
    while #queue > 0 and scanned < 4096 do
      local dir = table.remove(queue, 1)
      scanned = scanned + 1
      for _, entry in ipairs(vim._readdir(dir)) do
        local full = vim.fs.joinpath(dir, entry)
        consider(dir, entry)
        if #results >= limit then return results end
        if vim.fn.isdirectory(full) == 1 then queue[#queue + 1] = full end
      end
    end
  end
  return results
end

-- vim.fs.root(source, marker): the nearest ancestor of `source` (a path, or a
-- bufnr — 0/snapshot resolves to the current buffer's name, else cwd) that holds
-- `marker`. `marker` is a filename, a `function(name, path)` predicate, or a
-- list. A LIST is an ordered priority chain (neovim 0.11): each element is a
-- *tier* tried in turn — the highest-priority tier with a match anywhere up the
-- tree wins, regardless of depth. A tier that is itself a list groups names of
-- EQUAL priority (closest ancestor with any of them wins). So
-- `{ 'a', { 'b', 'c' }, 'd' }` means: prefer 'a'; else 'b'-or-'c'; else 'd'.
-- Returns nil if none match. This is what the vendored `lsp/<server>.lua` files
-- call to compute their `root_dir`.
function vim.fs.root(source, marker)
  local path
  if type(source) == "number" then
    path = vim.api.nvim_buf_get_name(source)
    if path == nil or path == "" then path = vim.fn.getcwd() end
  else
    path = source
  end
  path = vim.fs.normalize(path)
  -- Start at the path's directory when it is a file.
  local start = path
  if vim.fn.isdirectory(path) == 0 then start = vim.fs.dirname(path) end
  -- Normalize `marker` into the ordered list of tiers; each tier is a list of
  -- equal-priority names (or predicates). A bare string/function is one tier; a
  -- list marker is one tier per element (an element that is itself a list is a
  -- single equal-priority tier).
  local tiers
  if type(marker) == "table" then
    tiers = {}
    for _, m in ipairs(marker) do
      tiers[#tiers + 1] = type(m) == "table" and m or { m }
    end
  else
    tiers = { { marker } }
  end
  for _, names in ipairs(tiers) do
    local dir = start
    while dir do
      for _, m in ipairs(names) do
        if type(m) == "function" then
          for _, entry in ipairs(vim._readdir(dir)) do
            if m(entry, dir) then return dir end
          end
        elseif fs_exists(vim.fs.joinpath(dir, m)) then
          return dir
        end
      end
      local parent = vim.fs.dirname(dir)
      if parent == dir then break end
      dir = parent
    end
  end
  return nil
end

-- vim.fs.relpath(base, target): `target` expressed relative to `base`, or nil
-- when `base` is not an ancestor of `target` (the two are compared on a path
-- *segment* boundary, so "/a/b" is not an ancestor of "/a/bc"). Equal paths give
-- ".". Both are normalized first. rust_analyzer's `root_dir` uses it to decide
-- whether a file lives under a toolchain/registry/sysroot directory.
function vim.fs.relpath(base, target, _opts)
  base = vim.fs.normalize(base)
  target = vim.fs.normalize(target)
  if base == target then return "." end
  -- A trailing "/" makes the comparison segment-aligned; normalize strips it
  -- from "/a/b" (len > 1) but leaves the root "/" as-is, which already ends in /.
  local prefix = base
  if prefix:sub(-1) ~= "/" then prefix = prefix .. "/" end
  if target:sub(1, #prefix) == prefix then return target:sub(#prefix + 1) end
  return nil
end

-- ----- vim.uri ---------------------------------------------------------------
-- Minimal `file://` URI conversion. (The server does its own, encoding-aware URI
-- handling for actual LSP traffic; these back config-file path computations.)

function vim.uri_from_fname(path)
  path = vim.fs.normalize(path)
  if path:sub(1, 1) ~= "/" then path = "/" .. path end
  return "file://" .. path
end

function vim.uri_to_fname(uri)
  local path = (uri:gsub("^file://", ""))
  return (path:gsub("%%(%x%x)", function(h) return string.char(tonumber(h, 16)) end))
end

function vim.uri_from_bufnr(bufnr)
  return vim.uri_from_fname(vim.api.nvim_buf_get_name(bufnr))
end

-- ----- additional vim.fn -----------------------------------------------------

-- vim.fn.bufname(bufnr): the buffer's name, snapshot-backed via nvim_buf_get_name.
function vim.fn.bufname(bufnr) return vim.api.nvim_buf_get_name(bufnr or 0) end

-- vim.fn.fnamemodify(fname, mods): apply the `:p`/`:h`/`:t`/`:r`/`:e` filename
-- modifiers (left to right) configs use. `:p` absolutizes against cwd.
function vim.fn.fnamemodify(fname, mods)
  local result = fname or ""
  local i = 1
  while i <= #(mods or "") do
    if mods:sub(i, i) == ":" then
      local m = mods:sub(i + 1, i + 1)
      if m == "p" then
        if result:sub(1, 1) ~= "/" then result = vim.fs.joinpath(vim.fn.getcwd(), result) end
      elseif m == "h" then
        result = vim.fs.dirname(result)
      elseif m == "t" then
        result = vim.fs.basename(result)
      elseif m == "r" then
        result = (result:gsub("%.[^./]*$", ""))
      elseif m == "e" then
        result = result:match("%.([^./]*)$") or ""
      end
      i = i + 2
    else
      i = i + 1
    end
  end
  return result
end

-- A few more vim.fn used only inside deferred callbacks (handlers / user
-- commands) nxvim doesn't drive yet. `finddir` faithfully reuses the Rust-backed
-- directory search via vim.fs, so it stays. `bufnr` resolves against the Phase-6
-- buffer mirror; the register/quickfix/prompt ones can't be honored without those
-- UIs, so they raise via vim._notimpl rather than silently dropping the write.
function vim.fn.finddir(name, path)
  local hit = vim.fs.find(name, { path = path or vim.fn.getcwd(), upward = true, type = "directory" })[1]
  return hit or ""
end

-- vim.fn.bufnr(expr): the buffer number for `expr`. "" / "%" / nil / 0 -> current
-- buffer; a string -> the loaded buffer whose name matches (exact, else suffix),
-- -1 when none. Backed by the Phase-6 `vim._bufs` mirror.
function vim.fn.bufnr(expr)
  if expr == nil or expr == 0 or expr == "" or expr == "%" then
    return (vim._cur_buf or {}).bufnr or 0
  end
  if type(expr) == "number" then
    return vim._bufs[expr] and expr or -1
  end
  for bufnr, buf in pairs(vim._bufs) do
    local name = buf.name or ""
    if name == expr or name:sub(-#expr) == expr then return bufnr end
  end
  return -1
end

-- vim.fn.substitute(str, pat, sub, flags): vim-regex substitution. nxvim has no
-- vim-regex engine; the only caller is `lspconfig.util.strip_archive_subpath`,
-- which transforms `zipfile:`/`tarfile:` virtual paths and leaves ordinary paths
-- untouched. Returning `str` unchanged is therefore correct for every real file
-- path (archive-buffer paths, which nxvim doesn't produce, pass through as-is).
function vim.fn.substitute(str, _pat, _sub, _flags) return str end
function vim.fn.setreg(_name, _value, _opts) vim._notimpl("vim.fn.setreg") end
function vim.fn.setqflist(_list, _action, _what) vim._notimpl("vim.fn.setqflist") end
function vim.fn.confirm(_msg, _choices, _default, _type) vim._notimpl("vim.fn.confirm") end

-- ----- vim.system / vim.json -------------------------------------------------

-- vim.system(cmd, opts, on_exit): run `cmd` (an argv list) and return a handle.
-- `opts` may carry `cwd`, `env` (a {VAR=value} dict layered on the inherited
-- environment), and `text` (accepted; output is always returned as a string).
--
-- Two modes, split on whether an `on_exit` is given (the pragmatic
-- approximation of neovim's loop-pumping `:wait()`, which a single thread can't
-- replicate; see docs/async-lua-runtime-plan.md):
--   * `on_exit` given  → ASYNC. The child runs in the event-loop actor (off the
--     server thread); `on_exit` fires on a later tick with { code, stdout, stderr }.
--     The handle exposes a real `pid` (filled once the spawn lands) and a working
--     `kill`. `:wait()` is unavailable on this handle (it would need to pump the
--     loop) and raises, pointing the caller at the synchronous form.
--   * no `on_exit`     → SYNCHRONOUS. The child runs to completion inline and
--     `:wait()` returns the already-complete result. This is what an
--     `lsp/<server>.lua` `root_dir` that shells out (rust_analyzer's `cargo
--     metadata` / `rustc --print sysroot`) needs — short, blocking, resolved
--     during `vim.lsp.enable`.
function vim.system(cmd, opts, on_exit)
  if type(opts) == "function" then
    on_exit, opts = opts, nil
  end
  opts = opts or {}
  if on_exit then
    local id = vim._next_cb_id()
    vim._cb_fns[id] = on_exit
    vim._system_async(id, cmd, opts.cwd, opts.env)
    return setmetatable({}, {
      __index = function(_, key)
        if key == "pid" then
          return vim._proc_pids[id]
        elseif key == "kill" then
          return function(_, signal) vim._system_kill(id, signal) end
        elseif key == "wait" then
          return function()
            error("nxvim: vim.system():wait() is unavailable on a handle spawned "
              .. "with on_exit; call vim.system without on_exit for a synchronous result", 2)
          end
        end
        return nil
      end,
    })
  end
  local result = vim._system(cmd, opts.cwd, opts.env, opts.text ~= false)
  return setmetatable({ pid = result.pid }, {
    __index = {
      wait = function() return result end,
      kill = function() end, -- already exited; nothing to signal
    },
  })
end

-- vim.json.encode/decode: JSON (de)serialization, backed by the Rust serde_json
-- bridge. `decode` maps objects to string-keyed tables, arrays to sequences, and
-- `null` to nil; `encode` treats a `1..n` table as an array and any other as an
-- object. `decode` raises on malformed input (neovim parity).
vim.json = vim.json or {}
function vim.json.encode(value) return vim._json_encode(value) end
function vim.json.decode(str, _opts) return vim._json_decode(str) end

-- ----- misc vim.* the configs reach for --------------------------------------

-- vim.NIL: the sentinel for JSON null (a value that survives table storage where
-- a literal nil would simply drop the key). Configs store it in init_options /
-- capabilities; nxvim doesn't yet forward those to a server, so it only needs to
-- be a distinct, stringifiable value. `vim.json.encode` maps it to JSON null.
vim.NIL = setmetatable({}, { __tostring = function() return "vim.NIL" end })

-- vim.empty_dict(): a fresh table that JSON-encodes as `{}` (an object), never
-- `[]`. nxvim's encoder already emits `{}` for an empty table, so a plain table
-- suffices.
function vim.empty_dict() return {} end

-- vim.trim(s): `s` with leading/trailing whitespace removed.
function vim.trim(s) return (tostring(s):gsub("^%s+", ""):gsub("%s+$", "")) end

-- vim.islist(t): true iff `t` is a list (a table whose keys are exactly 1..#t).
function vim.islist(t)
  if type(t) ~= "table" then return false end
  local n = 0
  for _ in pairs(t) do n = n + 1 end
  return n == #t
end
vim.tbl_islist = vim.islist -- the pre-0.10 name

-- vim.version: callable (returns nxvim's emulated neovim version, stringifiable
-- as "0.11.0" — configs report it to the server) and a table of semver helpers.
-- nxvim targets the neovim 0.11 Lua surface, so that is what it reports.
local NVIM_VERSION = { major = 0, minor = 11, patch = 0 }
local function version_tbl(t)
  return setmetatable(t, {
    __tostring = function(v) return v.major .. "." .. v.minor .. "." .. v.patch end,
  })
end
vim.version = setmetatable({
  -- vim.version.parse("1.2.3"): a {major,minor,patch} table, or nil.
  parse = function(s)
    local a, b, c = tostring(s):match("v?(%d+)%.(%d+)%.?(%d*)")
    if not a then return nil end
    return version_tbl({ major = tonumber(a), minor = tonumber(b), patch = tonumber(c) or 0 })
  end,
  -- vim.version.cmp(a,b): -1 / 0 / 1. Accepts version tables or "x.y.z" strings.
  cmp = function(a, b)
    if type(a) == "string" then a = vim.version.parse(a) end
    if type(b) == "string" then b = vim.version.parse(b) end
    for _, k in ipairs({ "major", "minor", "patch" }) do
      if (a[k] or 0) ~= (b[k] or 0) then return (a[k] or 0) < (b[k] or 0) and -1 or 1 end
    end
    return 0
  end,
}, {
  __call = function() return version_tbl({ major = NVIM_VERSION.major, minor = NVIM_VERSION.minor, patch = NVIM_VERSION.patch }) end,
})
vim.version.lt = function(a, b) return vim.version.cmp(a, b) < 0 end
vim.version.gt = function(a, b) return vim.version.cmp(a, b) > 0 end
vim.version.eq = function(a, b) return vim.version.cmp(a, b) == 0 end
vim.version.ge = function(a, b) return vim.version.cmp(a, b) >= 0 end
vim.version.le = function(a, b) return vim.version.cmp(a, b) <= 0 end

-- ----- timers: vim.defer_fn / vim.uv timers / vim.fn.timer_* -----------------
-- All wall-clock timers ride the event-loop actor through the vim._timer_start /
-- vim._timer_stop bridge: a callback id is registered in vim._cb_fns, the actor
-- sleeps and fires LoopEvent::Timer, and the server runs the callback by id on its
-- thread. A repeating timer (repeat > 0) keeps its callback across fires; a
-- one-shot drops it. This is the same registry the keymap/schedule paths use.

-- A libuv-style timer handle: a table carrying its callback id, with the
-- start/stop/close/again methods plugins call. :start arms the actor timer;
-- :stop / :close cancel it (and :close drops the callback, freeing the registry).
local uv_timer = {}
uv_timer.__index = uv_timer
function uv_timer:start(timeout, rep, cb)
  if cb ~= nil then vim._cb_fns[self._id] = cb end
  self._repeat = rep or 0
  vim._timer_start(self._id, timeout or 0, self._repeat)
  return 0
end
function uv_timer:stop()
  vim._timer_stop(self._id)
  return 0
end
function uv_timer:again()
  -- libuv: restart a repeating timer, using its stored repeat as the new delay.
  vim._timer_start(self._id, self._repeat, self._repeat)
  return 0
end
function uv_timer:close(cb)
  vim._timer_stop(self._id)
  vim._cb_fns[self._id] = nil -- drop the callback so the registry can't leak
  vim._proc_pids[self._id] = nil
  if cb then cb() end
end
function uv_timer:is_closing() return false end
function uv_timer:is_active() return true end

-- vim.uv.new_timer_handle(id): wrap an existing callback id in a handle (used by
-- defer_fn, whose fn is already registered). vim.uv.new_timer(): a fresh handle.
-- vim.uv and vim.loop are the same table, so this lands on both.
function vim.uv.new_timer_handle(id)
  return setmetatable({ _id = id, _repeat = 0 }, uv_timer)
end
function vim.uv.new_timer()
  return vim.uv.new_timer_handle(vim._next_cb_id())
end

-- vim.defer_fn(fn, timeout): run `fn` once, `timeout` ms from now, on the loop —
-- the off-tick deferral configs use for retry patterns. Returns a timer handle so
-- the caller can :stop() it before it fires (neovim returns a uv timer).
function vim.defer_fn(fn, timeout)
  local id = vim._next_cb_id()
  vim._cb_fns[id] = fn
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
      if remaining <= 0 then vim._timer_stop(id); vim._cb_fns[id] = nil end
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

-- vim.ui: the selection/input/open hooks. With no UI layer wired (Phase 8),
-- calling `select`/`input` with a fake cancellation (on_choice(nil)) would make a
-- code-action picker look like the user dismissed it; `open` silently doing
-- nothing hides the gap. All three raise via vim._notimpl instead.
vim.ui = vim.ui or {}
function vim.ui.select(_items, _opts, _on_choice) vim._notimpl("vim.ui.select") end
function vim.ui.input(_opts, _on_confirm) vim._notimpl("vim.ui.input") end
function vim.ui.open(_path) vim._notimpl("vim.ui.open") end

-- vim.bo: buffer-local options, indexed by bufnr (`vim.bo[buf].filetype`), backed
-- by a per-buffer store (Phase 6). Writes record; reads return the stored value
-- (else nil — neovim's option default isn't modeled). `filetype`/`ft` stays
-- authoritative from the current-buffer snapshot (it backs the `root_dir`
-- filetype checks configs do at load) unless a write explicitly overrode it. The
-- store is *observable* but not yet wired to editor behavior — see the doc's
-- known-approximations list. A bare `vim.bo.<opt>` (no bufnr) targets the current
-- buffer. The `vim._bo_store` table is initialized with the other Phase-6 mirror
-- state above.
local function bo_get(bufnr, opt)
  local store = vim._bo_store[bufnr]
  if store ~= nil and store[opt] ~= nil then return store[opt] end
  if opt == "filetype" or opt == "ft" then return (vim._cur_buf or {}).filetype end
  return nil
end
local function bo_set(bufnr, opt, value)
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
function vim.validate(...) end

function vim.deprecate(...) end

-- ----- vim.lsp: the config framework (Neovim 0.11 core) ----------------------
-- nxvim's LSP machinery (the nxvim-lsp client + server-side document sync) is
-- driven entirely from this Lua surface, exactly like neovim 0.11: a user calls
-- `vim.lsp.config(name, …)` / `vim.lsp.enable(name)` (or drops an
-- `lsp/<name>.lua` on the runtimepath), and an opened file of a matching
-- filetype starts the server. There is no built-in server table — zero config
-- means no LSP. `vim.lsp.start` queues an `LspOp` (Rust `vim._lsp_start`) the
-- server drains into its `LspManager`.

vim.lsp = vim.lsp or {}
vim.lsp.protocol = vim.lsp.protocol or {}

-- vim.lsp.commands: the registry a server's workspace/executeCommand handlers map
-- into (a config's `before_init` may populate it, e.g. rust_analyzer's
-- `rust-analyzer.runSingle`). nxvim doesn't dispatch through it yet, but it must
-- exist so assigning into it doesn't index nil.
vim.lsp.commands = vim.lsp.commands or {}

-- vim.lsp.handlers: the default response-handler registry, keyed by LSP method
-- (`handler(err, result, ctx)`). A `client:request` with no explicit handler falls
-- back to the config's `handlers[method]`, then this global table (Phase 5). A
-- config may register a global default handler here; the per-config layer wins.
vim.lsp.handlers = vim.lsp.handlers or {}

-- Client capabilities are owned and advertised by the Rust client at
-- `initialize`; this stub lets a config that merges into them run without error.
function vim.lsp.protocol.make_client_capabilities() return {} end

-- vim.lsp.protocol.MessageType: the window/logMessage severity enum. A config
-- may name it as a literal value (smithy_ls sets `message_level =
-- vim.lsp.protocol.MessageType.Log`), so the table must exist at load time.
vim.lsp.protocol.MessageType = {
  Error = 1, Warning = 2, Info = 3, Log = 4, Debug = 5,
  [1] = "Error", [2] = "Warning", [3] = "Info", [4] = "Log", [5] = "Debug",
}

-- vim.lsp.protocol.Methods: the request/notification method-name table. Real
-- neovim maps e.g. `textDocument_diagnostic` -> "textDocument/diagnostic"; the
-- metatable reproduces that (first underscore -> slash) for any key, so a config
-- that names a method (in a deferred handler) gets the wire string, not nil.
vim.lsp.protocol.Methods = setmetatable({}, {
  __index = function(_, k) return (tostring(k):gsub("_", "/", 1)) end,
})

-- vim.lsp.rpc: the transport entry points a config's `cmd` builder calls.
-- nxvim does its own (stdio) process spawning in Rust, so it does not need
-- neovim's RPC client — it only needs the argv. `start(cmd, dispatchers, extra)`
-- therefore returns `cmd` unchanged: a `cmd = function(d, c) … return
-- vim.lsp.rpc.start({argv}, d) end` builder (ts_ls, eslint, jsonls, html, biome,
-- tailwindcss, … — 20-plus servers) resolves straight to its argv. `connect`
-- (a TCP transport, e.g. gdscript) can't be driven by the stdio spawner; it
-- raises (see below), so a config that builds its cmd through it surfaces as a
-- load error (vim._lsp_load_errors) rather than crashing `enable`.
vim.lsp.rpc = vim.lsp.rpc or {}
function vim.lsp.rpc.start(cmd, _dispatchers, _extra) return cmd end
-- `connect` is a TCP transport (e.g. gdscript). nxvim's spawner is stdio-only,
-- so there is no argv to hand back — returning a sentinel let the gap pass
-- silently. It raises via vim._notimpl: a config that calls it at load (gdscript)
-- surfaces as a real, allowlisted gap (TCP transport) rather than a "skip".
function vim.lsp.rpc.connect(_host, _port) vim._notimpl("vim.lsp.rpc.connect") end

-- vim.lsp.util: helpers a config reaches for inside on_attach / command / handler
-- callbacks (Phase 7). nxvim drives its core LSP features natively (vim.lsp.buf.*);
-- these compute LSP params from the real cursor/buffer state (the Phase-6 mirror)
-- and drive nxvim's own surfaces — the panel for previews, and the native
-- workspace-edit / single-location goto paths (queued as LspOps) for edits and
-- navigation. The Phase-0 vim._notimpl raises are gone.
vim.lsp.util = vim.lsp.util or {}

-- Convert a 0-based *byte* column on `line` to a position character in the LSP
-- `encoding` (utf-16 default; utf-8 → the byte index unchanged; utf-32 →
-- codepoints). nxvim stores text as UTF-8 bytes, so this walks the prefix
-- [0, byte_col) one UTF-8 lead byte at a time, counting code units (a 4-byte
-- char is a surrogate pair — 2 units — under utf-16, 1 under utf-32).
function vim._byte_to_position_char(line, byte_col, encoding)
  if encoding == nil or encoding == "utf-8" then return byte_col end
  local utf16 = encoding ~= "utf-32"
  local count, i = 0, 1
  local limit = math.min(byte_col, #line)
  while i <= limit do
    local b = string.byte(line, i)
    local size, units
    if b < 0x80 then size, units = 1, 1
    elseif b < 0xE0 then size, units = 2, 1
    elseif b < 0xF0 then size, units = 3, 1
    else size, units = 4, utf16 and 2 or 1 end
    count = count + units
    i = i + size
  end
  return count
end

-- The inverse: a position `character` in `encoding` back to a 0-based byte column
-- on `line` (used to address loclist columns by byte). Clamps at end-of-line.
function vim._position_char_to_byte(line, character, encoding)
  if encoding == nil or encoding == "utf-8" then return math.min(character, #line) end
  local utf16 = encoding ~= "utf-32"
  local count, i = 0, 1
  while i <= #line and count < character do
    local b = string.byte(line, i)
    local size, units
    if b < 0x80 then size, units = 1, 1
    elseif b < 0xE0 then size, units = 2, 1
    elseif b < 0xF0 then size, units = 3, 1
    else size, units = 4, utf16 and 2 or 1 end
    count = count + units
    i = i + size
  end
  return i - 1
end

-- The text of 0-based `row` in the loaded buffer whose name maps to `uri`, or nil
-- when no open buffer backs it. Scans the Phase-6 mirror (`vim._bufs`, which
-- carries each buffer's name and line array) — the loclist `text` field for a
-- location in an unopened file is left empty rather than read off disk.
function vim._line_text_for_uri(uri, row)
  local fname = vim.uri_to_fname(uri)
  for _, buf in pairs(vim._bufs) do
    if buf.lines and buf.name == fname then
      return buf.lines[row + 1]
    end
  end
  return nil
end

-- make_text_document_params(bufnr): the `{ uri }` a request's `textDocument` field
-- carries, from the buffer's file path.
function vim.lsp.util.make_text_document_params(bufnr)
  return { uri = vim.uri_from_bufnr(bufnr or 0) }
end

-- make_position_params(window, encoding): the `{ textDocument, position }` a
-- cursor-relative request (definition, hover, …) carries. The cursor comes from
-- the real editor (Phase-6 mirror); its byte column is converted to `encoding`
-- (utf-16 default). `window` is ignored — single-window nxvim.
function vim.lsp.util.make_position_params(_window, encoding)
  encoding = encoding or "utf-16"
  local bufnr = vim.api.nvim_get_current_buf()
  local c = vim.api.nvim_win_get_cursor(0) -- { row (1-based), col (0-based byte) }
  local line = vim.api.nvim_buf_get_lines(bufnr, c[1] - 1, c[1], false)[1] or ""
  return {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
    position = { line = c[1] - 1, character = vim._byte_to_position_char(line, c[2], encoding) },
  }
end

-- make_given_range_params(start_pos, end_pos, bufnr, encoding): the
-- `{ textDocument, range }` a range request (range formatting, range code action)
-- carries. `start_pos`/`end_pos` are `{ row (1-based), col (0-based byte) }` (the
-- neovim mark shape); the columns convert to `encoding` and the end is made
-- exclusive (marks are inclusive), matching neovim.
function vim.lsp.util.make_given_range_params(start_pos, end_pos, bufnr, encoding)
  encoding = encoding or "utf-16"
  bufnr = vim._resolve_bufnr(bufnr or 0)
  local function pos_at(p)
    local row = p[1] - 1
    local line = vim.api.nvim_buf_get_lines(bufnr, row, row + 1, false)[1] or ""
    return { line = row, character = vim._byte_to_position_char(line, p[2], encoding) }
  end
  local s = pos_at(start_pos)
  local e = pos_at(end_pos)
  e.character = e.character + 1 -- inclusive mark → exclusive LSP range end
  return {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
    range = { start = s, ["end"] = e },
  }
end

-- locations_to_items(locations, encoding): turn LSP `Location` / `LocationLink`s
-- into loclist items (`{ filename, lnum, col, text }`), sorted by file then
-- position. The byte `col` and the `text` come from the open buffer backing each
-- location (empty `text` for an unopened file). `user_data` keeps the raw location.
function vim.lsp.util.locations_to_items(locations, encoding)
  encoding = encoding or "utf-16"
  local items = {}
  for _, loc in ipairs(locations or {}) do
    local uri = loc.uri or loc.targetUri
    local range = loc.range or loc.targetRange
    if uri and range then
      local row = range.start.line
      local text = vim._line_text_for_uri(uri, row) or ""
      items[#items + 1] = {
        filename = vim.uri_to_fname(uri),
        lnum = row + 1,
        col = vim._position_char_to_byte(text, range.start.character, encoding) + 1,
        text = text,
        user_data = loc,
      }
    end
  end
  table.sort(items, function(a, b)
    if a.filename ~= b.filename then return a.filename < b.filename end
    if a.lnum ~= b.lnum then return a.lnum < b.lnum end
    return a.col < b.col
  end)
  return items
end

-- get_effective_tabstop(bufnr): the indent width for the buffer — `shiftwidth`
-- when set (> 0), else `tabstop` (default 8), read from the vim.bo store (Phase 6).
function vim.lsp.util.get_effective_tabstop(bufnr)
  bufnr = vim._resolve_bufnr(bufnr or 0)
  local store = vim._bo_store[bufnr] or {}
  local sw = store.shiftwidth or 0
  if sw > 0 then return sw end
  return store.tabstop or 8
end

-- open_floating_preview(contents, syntax, opts): show `contents` (a list of lines)
-- in nxvim's panel — the surface that stands in for neovim's floating window.
-- neovim returns `(float_bufnr, win_id)`; nxvim has one panel and no per-float
-- handle, so it returns `0` and the current window handle for call-site shape.
function vim.lsp.util.open_floating_preview(contents, _syntax, opts)
  opts = opts or {}
  local lines = type(contents) == "table" and contents or { tostring(contents) }
  vim.panel.open(opts.title or "Preview", lines)
  return 0, vim.api.nvim_get_current_win()
end

-- apply_workspace_edit(workspace_edit, encoding): apply a `WorkspaceEdit` across
-- the open buffers it names, reusing the native rename / code-action path (queued
-- as an LspOp the server normalizes and applies). Edits to unopened files are a
-- follow-up (the native path edits open buffers only); `encoding` is carried by
-- the edit's positions and resolved server-side, so the arg is accepted here.
function vim.lsp.util.apply_workspace_edit(workspace_edit, _encoding)
  vim._lsp_apply_workspace_edit(workspace_edit or {})
end

-- show_document(location, encoding, opts): jump the cursor to an LSP location
-- (`Location` or `LocationLink`), opening the file if needed — the native
-- single-location goto, queued as an LspOp. An `external = true` location (open in
-- a browser/program) has no nxvim surface, so it raises rather than no-op.
function vim.lsp.util.show_document(location, encoding, _opts)
  if type(location) ~= "table" then return false end
  if location.external then vim._notimpl("vim.lsp.util.show_document (external)") end
  local uri = location.uri or location.targetUri
  local range = location.range or location.targetRange
  if not uri then return false end
  local line = range and range.start.line or 0
  local character = range and range.start.character or 0
  vim._lsp_show_document(uri, line, character, encoding or "utf-16")
  return true
end

-- vim.lsp.omnifunc: the i_CTRL-X_CTRL-O completion entry point. nxvim has no
-- omni-completion path yet; returning -1 ("no completion") masked the gap, so it
-- raises via vim._notimpl.
function vim.lsp.omnifunc(_findstart, _base) vim._notimpl("vim.lsp.omnifunc") end

vim._lsp_user_config = vim._lsp_user_config or {} -- name -> user override layer
vim._lsp_base_cache = vim._lsp_base_cache or {}   -- name -> lsp/<name>.lua result (false = none)
vim._lsp_enabled = vim._lsp_enabled or {}         -- name -> enabled?

-- Phase 1 visibility surfaces: a config that errors at load, and a server skipped
-- at start, are recorded here (keyed by name, so a re-resolve never duplicates)
-- instead of silently degrading to `{}` / a bare `return`. `vim.lsp._report`
-- reads them back. See docs/lsp-completion-plan.md (Phase 1).
vim._lsp_load_errors = vim._lsp_load_errors or {} -- name -> load error message
vim._lsp_skipped = vim._lsp_skipped or {}         -- name -> skip reason

-- Record that `name`'s lsp/<name>.lua failed to load, and echo a one-line
-- warning. One broken config must not wedge startup — the editor keeps running
-- and the other servers still start — but the failure is loud, not swallowed into
-- an empty config. Idempotent (lsp_base_config caches, so it records once).
local function lsp_record_load_error(name, err)
  vim._lsp_load_errors[name] = err
  vim.api.nvim_echo("nxvim LSP: config '" .. name .. "' failed to load: " .. err)
end

-- Record that `name` was skipped at start with `reason` (its cmd didn't resolve
-- to a spawnable stdio argv), and echo a one-line warning. Deduped on the reason
-- so a server that skips on every buffer open doesn't spam the panel.
local function lsp_record_skip(name, reason)
  if vim._lsp_skipped[name] == reason then return end
  vim._lsp_skipped[name] = reason
  vim.api.nvim_echo("nxvim LSP: server '" .. name .. "' skipped: " .. reason)
end

-- Errors raised inside a config's lifecycle hook (`before_init` / `on_init` /
-- `on_exit`), keyed by "name:hook" → message. A hook that throws (e.g. one that
-- reaches a Phase-0 gap like `vim.uv`) must not wedge the start/exit path, but the
-- failure is recorded and echoed, never swallowed. Surfaced by vim.lsp._report.
vim._lsp_hook_errors = vim._lsp_hook_errors or {}
local function lsp_record_hook_error(name, hook, err)
  local key = (name or "?") .. ":" .. hook
  vim._lsp_hook_errors[key] = tostring(err)
  vim.api.nvim_echo("nxvim LSP: " .. hook .. " for '" .. (name or "?") .. "' errored: " .. tostring(err))
end

-- The client registry: id -> { id, name, server_capabilities }, mirrored from
-- Rust (`LuaRuntime::set_lsp_client`) when a server finishes `initialize`. The
-- handle `LspAttach`'s `args.data.client_id` resolves through `get_client_by_id`.
vim.lsp._clients = vim.lsp._clients or {}

-- The handler for a `client:request` reply on `method`: the config's
-- `handlers[method]`, else the global `vim.lsp.handlers[method]`, else nil (the
-- reply is discarded after firing). The per-config layer wins (Phase 5).
function vim.lsp._resolve_handler(name, method)
  local cfg = name and vim.lsp.config[name]
  if cfg and type(cfg.handlers) == "table" and cfg.handlers[method] ~= nil then
    return cfg.handlers[method]
  end
  return vim.lsp.handlers[method]
end

-- client:request(method, params, handler, bufnr): issue a generic LSP request to
-- this client's server and route the reply to `handler(err, result, ctx)` when it
-- lands off-tick (Phase 5). With no handler, falls back to the config's
-- `handlers[method]` then `vim.lsp.handlers[method]`. The handler is registered in
-- the deferred-callback registry (`vim._cb_fns`), dropped after one fire (no leak).
-- Returns `true, request_id`; the reply won't arrive if the server exits first
-- (the same liveness caveat neovim has).
function vim.lsp._client_request(self, method, params, handler, bufnr)
  if type(method) ~= "string" then error("client:request: method must be a string", 2) end
  handler = handler or vim.lsp._resolve_handler(self.name, method)
  local cb = vim._next_cb_id()
  local client_id, client_name = self.id, self.name
  vim._cb_fns[cb] = function(err, result)
    if handler then
      handler(err, result, {
        method = method, client_id = client_id, client_name = client_name, bufnr = bufnr,
      })
    end
  end
  vim._lsp_client_request(self.id, method, params, cb)
  return true, cb
end

-- client:notify(method, params): fire-and-forget a generic LSP notification to
-- this client's server (Phase 5). Returns true (queued).
function vim.lsp._client_notify(self, method, params)
  if type(method) ~= "string" then error("client:notify: method must be a string", 2) end
  vim._lsp_client_notify(self.id, method, params)
  return true
end

-- Build a client table carrying the real request/notify methods. Shared by
-- `_set_client` (the entry `get_client_by_id`/`on_attach` resolve) and
-- `get_clients`, so `client:request`/`client:notify` work from every call site.
function vim.lsp._make_client(id, name, server_capabilities)
  return {
    id = id,
    name = name,
    server_capabilities = server_capabilities or {},
    request = vim.lsp._client_request,
    notify = vim.lsp._client_notify,
  }
end

function vim.lsp._set_client(id, name, server_capabilities)
  vim.lsp._clients[id] = vim.lsp._make_client(id, name, server_capabilities)
end
function vim.lsp._remove_client(id) vim.lsp._clients[id] = nil end

-- vim.lsp.get_client_by_id(id): the registered client table (with `name` and
-- `server_capabilities`), or nil once its server has exited.
function vim.lsp.get_client_by_id(id) return vim.lsp._clients[id] end

-- vim.lsp._run_on_init(id, result): call the config's `on_init(client, result)`
-- hook (Phase 3), invoked from Rust (`LuaRuntime::run_lsp_on_init`) right after the
-- client is mirrored on `initialize`. `result` is the raw `initialize` result. A
-- throwing hook is recorded, never fatal. No-op if the client/hook is absent.
function vim.lsp._run_on_init(id, result)
  local client = vim.lsp._clients[id]
  if not client then return end
  local cfg = vim.lsp.config[client.name]
  if cfg and type(cfg.on_init) == "function" then
    local ok, err = pcall(cfg.on_init, client, result)
    if not ok then lsp_record_hook_error(client.name, "on_init", err) end
  end
end

-- vim.lsp._run_on_exit(id, code, signal): call the config's
-- `on_exit(code, signal, client)` hook (Phase 3), invoked from Rust
-- (`LuaRuntime::run_lsp_on_exit`) when the server exits, while the client is still
-- registered (before it is removed). A throwing hook is recorded, never fatal.
function vim.lsp._run_on_exit(id, code, signal)
  local client = vim.lsp._clients[id]
  if not client then return end
  local cfg = vim.lsp.config[client.name]
  if cfg and type(cfg.on_exit) == "function" then
    local ok, err = pcall(cfg.on_exit, code, signal, client)
    if not ok then lsp_record_hook_error(client.name, "on_exit", err) end
  end
end

-- vim.lsp.get_clients(filter): the list of active clients, each a
-- `{ id, name, server_capabilities, config, request, notify }` table. `filter`
-- narrows by `id` and/or `name`; a `bufnr` filter is accepted but not honored —
-- nxvim has no Lua-side buffer->client map yet, so it returns the name/id matches
-- across all buffers. `config` is the resolved `vim.lsp.config[name]`; `request` /
-- `notify` are the real Phase-5 client methods (a server-specific command like
-- rust_analyzer's `:LspCargoReload` issues `client:request` through them).
-- `get_active_clients` is the deprecated neovim alias, kept for configs that still
-- call it.
function vim.lsp.get_clients(filter)
  filter = filter or {}
  local out = {}
  for id, c in pairs(vim.lsp._clients) do
    if (filter.id == nil or filter.id == id) and (filter.name == nil or filter.name == c.name) then
      local client = vim.lsp._make_client(c.id, c.name, c.server_capabilities)
      client.config = vim.lsp.config[c.name]
      out[#out + 1] = client
    end
  end
  return out
end

vim.lsp.get_active_clients = vim.lsp.get_clients

-- vim.lsp._report(): the Phase-1 scoreboard — a snapshot of what the LSP layer is
-- doing and where it fell short, so no failure stays silent. `enabled` lists the
-- configs marked for auto-activation; `started` the servers that reached
-- `initialize` (the live clients); `load_errors` the configs that failed to load
-- (name -> message); `skipped` the servers whose cmd didn't resolve to a
-- spawnable argv (name -> reason); `notimpl_hits` the not-implemented functions a
-- real config actually called (the Phase-0 set). A `:LspInfo`-style command can
-- render this later; for now it backs `:lua print(vim.inspect(vim.lsp._report()))`
-- and the tests.
function vim.lsp._report()
  local enabled = {}
  for name, on in pairs(vim._lsp_enabled) do
    if on then enabled[#enabled + 1] = name end
  end
  table.sort(enabled)
  local started = {}
  for _, c in pairs(vim.lsp._clients) do started[#started + 1] = c.name end
  table.sort(started)
  local notimpl = vim.tbl_keys(vim._notimpl_hits)
  table.sort(notimpl)
  return {
    enabled = enabled,
    started = started,
    load_errors = vim._lsp_load_errors,
    skipped = vim._lsp_skipped,
    hook_errors = vim._lsp_hook_errors,
    notimpl_hits = notimpl,
  }
end

-- Load and cache `lsp/<name>.lua` off the runtimepath (the base config layer).
-- Returns its returned table, or nil when the file is simply absent. A file that
-- IS present but fails — unreadable, a parse error, a runtime error (now possible
-- since Phase 0 made gaps raise), or one that doesn't return a table — is no
-- longer swallowed into an empty config: it is recorded in vim._lsp_load_errors
-- and echoed (lsp_record_load_error). The result is still cached (`false`) so the
-- load is attempted — and reported — only once.
local function lsp_base_config(name)
  local cached = vim._lsp_base_cache[name]
  if cached ~= nil then return cached or nil end
  local cfg = false
  local files = vim.api.nvim_get_runtime_file("lsp/" .. name .. ".lua", false)
  if files and files[1] then
    local file = files[1]
    local src = vim._read_file(file)
    if src == nil then
      lsp_record_load_error(name, "could not read " .. file)
    else
      local chunk, perr = loadstring(src, "@" .. file)
      if not chunk then
        lsp_record_load_error(name, "parse: " .. tostring(perr))
      else
        local ok, ret = pcall(chunk)
        if not ok then
          lsp_record_load_error(name, tostring(ret))
        elseif type(ret) ~= "table" then
          lsp_record_load_error(name, "config did not return a table (got " .. type(ret) .. ")")
        else
          cfg = ret
        end
      end
    end
  end
  vim._lsp_base_cache[name] = cfg
  return cfg or nil
end

-- The resolved config for `name`: the `'*'` wildcard layer, then the
-- `lsp/<name>.lua` runtimepath base, then the user override — deep-merged with
-- the rightmost winning (neovim's `vim.lsp.config[name]` chain).
local function lsp_resolve(name)
  return vim.tbl_deep_extend(
    "force",
    vim._lsp_user_config["*"] or {},
    lsp_base_config(name) or {},
    vim._lsp_user_config[name] or {}
  )
end

-- vim.lsp.config: callable to merge an override (`vim.lsp.config(name, opts)` —
-- `'*'` is the all-clients layer), indexable for the resolved config
-- (`vim.lsp.config[name]`), and assignable to redefine (`vim.lsp.config[name] =
-- opts`, which replaces the override layer and drops the runtimepath base).
vim.lsp.config = setmetatable({}, {
  __call = function(_, name, opts)
    if type(name) ~= "string" then error("vim.lsp.config: name must be a string") end
    local prev = vim._lsp_user_config[name] or {}
    vim._lsp_user_config[name] = vim.tbl_deep_extend("force", prev, opts or {})
  end,
  __index = function(_, name) return lsp_resolve(name) end,
  __newindex = function(_, name, opts)
    vim._lsp_user_config[name] = opts or {}
    vim._lsp_base_cache[name] = false -- a redefine overrides the resolved chain
  end,
})

-- Is `cmd` a usable argv — a non-empty list of strings? Guards the start queue
-- against the config shapes nxvim can't spawn: an empty/nil cmd or a builder that
-- failed. Those skip the start rather than erroring at the Rust boundary.
local function lsp_is_argv(cmd)
  if type(cmd) ~= "table" or #cmd == 0 then return false end
  for _, a in ipairs(cmd) do
    if type(a) ~= "string" then return false end
  end
  return true
end

-- `t` if it is a non-empty table, else nil. Guards the config payloads
-- (`settings` / `init_options` / `capabilities`) threaded to `vim._lsp_start`: an
-- absent or empty table becomes nil → the server forwards nothing, rather than an
-- empty `{}` that the lua_to_json bridge would emit as `[]`.
local function lsp_nonempty(t)
  if type(t) == "table" and next(t) ~= nil then return t end
  return nil
end

-- Why `cmd` is not a spawnable argv — the human-readable reason recorded in
-- vim._lsp_skipped so a skipped server isn't a silent mystery.
local function lsp_argv_reason(cmd)
  if cmd == nil then return "cmd did not resolve (nil)" end
  if type(cmd) ~= "table" then return "cmd is not an argv list (got " .. type(cmd) .. ")" end
  if #cmd == 0 then return "cmd is an empty argv list" end
  return "cmd has a non-string element"
end

-- Resolve a config's `cmd` to an argv list. A function `cmd` is neovim's
-- `cmd(dispatchers, config)` builder: nxvim does its own (stdio) spawning, so the
-- dispatchers are a stub and `vim.lsp.rpc.start` returns the argv it was given
-- (see its shim) — letting the many `node_modules/.bin` resolvers run unchanged.
-- Run the config's `before_init(init_params, config)` hook (Phase 3) if present,
-- and return the `(init_options, settings, capabilities)` to forward — honoring
-- whatever the hook left in `init_params.initializationOptions` / `.capabilities`
-- and any mutation of `config.settings` (rust_analyzer copies
-- `settings['rust-analyzer'] → init_params.initializationOptions`; eslint mutates
-- `config.settings`). nxvim runs this synchronously on the editor thread just
-- before the start is queued (no event loop needed), so the mutations are baked
-- into the `initialize` Phase 2 forwards. A throwing hook is recorded (not fatal)
-- and the pre-hook values are forwarded unchanged. `init_params` is the minimal
-- shape the common hooks touch; a `config.cmd` mutation here is too late (the cmd
-- is already resolved) and is not honored — a documented approximation.
local function lsp_before_init(config)
  local init_options, settings, capabilities =
    config.init_options, config.settings, config.capabilities
  if type(config.before_init) == "function" then
    local init_params = {
      initializationOptions = init_options or settings,
      capabilities = capabilities or {},
    }
    local ok, err = pcall(config.before_init, init_params, config)
    if ok then
      init_options = init_params.initializationOptions
      capabilities = init_params.capabilities
      settings = config.settings -- the hook may have mutated it in place
    else
      lsp_record_hook_error(config.name, "before_init", err)
    end
  end
  return init_options, settings, capabilities
end

-- The builder gets the resolved config with `root_dir` filled in (the field those
-- resolvers read). A throwing builder yields `nil, reason` so the caller can
-- record exactly why the server was skipped (instead of a bare nil that looks the
-- same as "no cmd").
local function lsp_resolve_cmd(cfg, root)
  local cmd = cfg.cmd
  if type(cmd) == "function" then
    -- Shallow-copy and set root_dir to the *resolved* root. A direct assignment
    -- (not tbl_extend) so a nil root CLEARS the field rather than leaving cfg's
    -- root_dir function in place — otherwise a builder that does
    -- `joinpath(config.root_dir, …)` would join against a function. With it nil,
    -- those builders fall back to the global binary, which is correct.
    local config = {}
    for k, v in pairs(cfg) do config[k] = v end
    config.root_dir = root
    local ok, result = pcall(cmd, {}, config)
    if not ok then return nil, "cmd builder errored: " .. tostring(result) end
    cmd = result
  end
  return cmd
end

-- Queue a start for `bufnr` from a fully-resolved config (root already computed).
-- When the cmd doesn't resolve to a spawnable argv, the server is recorded in
-- vim._lsp_skipped with the reason (and a warning echoed) rather than vanishing —
-- so enabling a server whose binary/transport nxvim can't drive is visible, not a
-- silent no-op, and still never errors the whole enable.
local function lsp_start_resolved(name, cfg, bufnr, ft, root)
  local cmd, reason = lsp_resolve_cmd(cfg, root)
  if not lsp_is_argv(cmd) then
    lsp_record_skip(name, reason or lsp_argv_reason(cmd))
    return
  end
  vim.lsp.start(
    {
      name = name,
      cmd = cmd,
      root_dir = root,
      filetypes = cfg.filetypes,
      -- Carry what the config configures so the server runs configured, not on
      -- defaults (Phase 2): vim.lsp.start reads these and forwards them to Rust.
      settings = cfg.settings,
      init_options = cfg.init_options,
      capabilities = cfg.capabilities,
      -- The lifecycle hook run just before initialize (Phase 3).
      before_init = cfg.before_init,
    },
    { bufnr = bufnr, filetype = ft }
  )
end

-- Resolve `cfg`'s root_dir (string | `function(bufnr, on_dir)` | `root_markers`
-- upward search) and start the server. A function root_dir drives the start
-- through its `on_dir` callback, so it can decline (never calling it) to skip a
-- buffer — the mechanism `vim.lsp.enable`'s docs describe.
local function lsp_start_for(name, cfg, bufnr, ft)
  local rd = cfg.root_dir
  if type(rd) == "function" then
    rd(bufnr, function(root) lsp_start_resolved(name, cfg, bufnr, ft, root) end)
  elseif type(rd) == "string" then
    lsp_start_resolved(name, cfg, bufnr, ft, rd)
  elseif cfg.root_markers then
    lsp_start_resolved(name, cfg, bufnr, ft, vim.fs.root(bufnr, cfg.root_markers))
  else
    lsp_start_resolved(name, cfg, bufnr, ft, nil)
  end
end

-- The shared FileType dispatcher body: for every enabled config whose resolved
-- `filetypes` includes `ft`, resolve the root and start the server for `bufnr`.
function vim.lsp._on_filetype(bufnr, ft)
  if not ft or ft == "" then return end
  for name, on in pairs(vim._lsp_enabled) do
    if on then
      local cfg = vim.lsp.config[name]
      if cfg.filetypes and vim.tbl_contains(cfg.filetypes, ft) then
        lsp_start_for(name, cfg, bufnr, ft)
      end
    end
  end
end

-- Install the single shared FileType autocmd that drives all enabled configs
-- (idempotent — `vim.lsp.enable` may be called many times).
local function lsp_ensure_dispatcher()
  if vim._lsp_dispatcher_installed then return end
  vim._lsp_dispatcher_installed = true
  local group = vim.api.nvim_create_augroup("nxvim.lsp.enable", { clear = true })
  vim.api.nvim_create_autocmd("FileType", {
    group = group,
    callback = function(args) vim.lsp._on_filetype(args.buf, args.match) end,
  })
  -- The attach hook: when the server bound to a buffer finishes its first
  -- `didOpen`, the server fires `LspAttach` with `data.client_id`; resolve the
  -- client and run its config's `on_attach(client, bufnr)` — the call site that
  -- lets a config set buffer-local LSP keymaps (`vim.keymap.set('n','gd',
  -- vim.lsp.buf.definition, {buffer=args.buf})`).
  vim.api.nvim_create_autocmd("LspAttach", {
    group = group,
    callback = function(args)
      local client = vim.lsp.get_client_by_id(args.data and args.data.client_id)
      if not client then return end
      local cfg = vim.lsp.config[client.name]
      if cfg and type(cfg.on_attach) == "function" then
        cfg.on_attach(client, args.buf)
      end
    end,
  })
end

-- vim.lsp.enable(name|list[, enable]): mark configs for auto-activation (on
-- current and future buffers) and install the FileType dispatcher. `enable=false`
-- turns a config off (future buffers won't start it). `'*'` is not a valid name.
function vim.lsp.enable(name, enable)
  local names = type(name) == "table" and name or { name }
  local on = enable ~= false
  for _, n in ipairs(names) do
    if n == "*" then error("vim.lsp.enable: '*' is not a valid LSP config name") end
    vim._lsp_enabled[n] = on
  end
  lsp_ensure_dispatcher()
  -- Process the already-open current buffer on the spot (neovim parity): its
  -- `FileType` has already fired, so the dispatcher just installed won't catch
  -- it, and an interactive `vim.lsp.enable(...)` would otherwise be a no-op until
  -- the next file opened. A start is idempotent server-side, so the overlap with
  -- the startup `FileType` (when this runs from `init.lua`) is harmless. Only on
  -- an *enable* — a disable must not start anything.
  if on then
    local cur = vim._cur_buf
    if cur and cur.filetype and cur.filetype ~= "" then
      vim.lsp._on_filetype(cur.bufnr, cur.filetype)
    end
  end
end

-- vim.lsp.start(config[, opts]): start (or reuse) the server for `config`
-- (`{name, cmd, root_dir}`) and attach a buffer (`opts.bufnr`, default the
-- snapshot buffer). `opts.filetype` is the buffer's filetype (the LSP
-- languageId). Reuse on `(name, root)` is the server's job; here it just queues.
function vim.lsp.start(config, opts)
  opts = opts or {}
  local bufnr = opts.bufnr or (vim._cur_buf and vim._cur_buf.bufnr) or 0
  local cmd, reason = config.cmd or {}, nil
  if type(cmd) == "function" then cmd, reason = lsp_resolve_cmd(config, config.root_dir) end
  -- Only queue a spawnable argv (see lsp_is_argv): a non-stdio/empty cmd would
  -- otherwise fail at the Rust `vim._lsp_start` boundary. A skip is recorded
  -- (vim._lsp_skipped) rather than returning silently.
  if not lsp_is_argv(cmd) then
    lsp_record_skip(config.name or "?", reason or lsp_argv_reason(cmd))
    return
  end
  -- Run before_init (Phase 3) and forward the (possibly hook-mutated)
  -- init_options / settings / capabilities the server applies at initialize.
  local init_options, settings, capabilities = lsp_before_init(config)
  vim._lsp_start(
    config.name, cmd, config.root_dir, opts.filetype or "", bufnr,
    lsp_nonempty(init_options),
    lsp_nonempty(settings),
    lsp_nonempty(capabilities)
  )
end

-- ----- vim.lsp.buf: Lua entry points to the native features -------------------
-- Each function enqueues an `LspOp` (Rust `vim._lsp_buf*`) that the server drains
-- on the same input tick and routes into the existing `request_lsp*` paths — so
-- the request reads the cursor where the key fired. The functions are *bare*
-- (no implicit args) so `vim.keymap.set('n', 'gd', vim.lsp.buf.definition)` works:
-- the keymap RHS is called with no arguments and just queues the op.
--
-- `kind` ints mirror `LspReqKind::as_u16` (Rust); keep the two in lockstep.
vim.lsp.buf = vim.lsp.buf or {}

function vim.lsp.buf.definition() vim._lsp_buf(0) end
function vim.lsp.buf.declaration() vim._lsp_buf(1) end
function vim.lsp.buf.type_definition() vim._lsp_buf(2) end
function vim.lsp.buf.implementation() vim._lsp_buf(3) end
function vim.lsp.buf.references() vim._lsp_buf(4) end
function vim.lsp.buf.hover() vim._lsp_buf(5) end
function vim.lsp.buf.signature_help() vim._lsp_buf(6) end

-- format()/code_action() take an options table in neovim (async, range, filter,
-- …); none have behavior in nxvim yet (the request is synchronous-issue,
-- async-reply), so the argument is accepted and ignored for call-site
-- compatibility — see the Phase 7b follow-ups.
function vim.lsp.buf.format(_opts) vim._lsp_buf_format() end
function vim.lsp.buf.code_action(_opts) vim._lsp_buf_code_action() end

-- rename(new_name): the name is required (nxvim has no prompt UI yet). A nil/empty
-- name echoes E471 rather than prompting, matching `:LspRename`.
function vim.lsp.buf.rename(new_name)
  if type(new_name) ~= "string" or new_name == "" then
    vim.api.nvim_echo("E471: Argument required: vim.lsp.buf.rename(name)")
    return
  end
  vim._lsp_buf_rename(new_name)
end

-- ----- vim.diagnostic: the Lua diagnostics surface ---------------------------
-- `get` reads the Rust→Lua mirror (`vim._diagnostics`, keyed by bufnr, refreshed
-- on every publishDiagnostics via `vim._set_diagnostics`); the actions
-- (`goto_next`/`goto_prev`/`setloclist`) and `config` enqueue an `LspOp` the
-- server applies, reusing the native cursor-move / panel / underline paths.
vim.diagnostic = vim.diagnostic or {}

-- Severity is numbered 1=ERROR…4=HINT (neovim), and the table reverse-maps the
-- number back to its name (`vim.diagnostic.severity[1] == "ERROR"`).
vim.diagnostic.severity = {
  ERROR = 1, WARN = 2, INFO = 3, HINT = 4,
  [1] = "ERROR", [2] = "WARN", [3] = "INFO", [4] = "HINT",
}

-- The mirror the server pushes into; keyed by bufnr → list of diagnostic tables.
vim._diagnostics = vim._diagnostics or {}
function vim._set_diagnostics(bufnr, list)
  vim._diagnostics[bufnr or 0] = list or {}
end

local function diag_current_bufnr()
  return vim._cur_buf and vim._cur_buf.bufnr or 0
end

-- vim.diagnostic.get([bufnr, [opts]]): diagnostics as plain tables. `nil` bufnr →
-- every buffer; `0` → the current one. `opts.severity` (a number) filters. The
-- entries are copied out (callers must not mutate the mirror), each tagged with
-- its `bufnr`, matching neovim's shape.
function vim.diagnostic.get(bufnr, opts)
  opts = opts or {}
  local out = {}
  local function collect(b)
    for _, d in ipairs(vim._diagnostics[b] or {}) do
      if opts.severity == nil or d.severity == opts.severity then
        local copy = { bufnr = b }
        for k, v in pairs(d) do copy[k] = v end
        out[#out + 1] = copy
      end
    end
  end
  if bufnr == nil then
    for b in pairs(vim._diagnostics) do collect(b) end
  else
    if bufnr == 0 then bufnr = diag_current_bufnr() end
    collect(bufnr)
  end
  return out
end

function vim.diagnostic.goto_next(opts)
  opts = opts or {}
  vim._diagnostic_goto(true, opts.severity)
end

function vim.diagnostic.goto_prev(opts)
  opts = opts or {}
  vim._diagnostic_goto(false, opts.severity)
end

function vim.diagnostic.setloclist(_opts)
  vim._diagnostic_setloclist()
end

-- vim.diagnostic.config([opts]): merge `opts` into the stored config and return
-- the merged table when called bare. nxvim has one diagnostic surface — the
-- underline spans — so the `underline` key is honored (false hides the
-- squiggles); virt-text/signs and the rest are stored without behavior until a
-- surface exists.
vim.diagnostic._config = { underline = true }
function vim.diagnostic.config(opts, _namespace)
  if opts == nil then return vim.diagnostic._config end
  for k, v in pairs(opts) do vim.diagnostic._config[k] = v end
  -- `underline` is true/false/table (a table is an enabled, filtered form);
  -- only an explicit `false` disables.
  vim._diagnostic_config(vim.diagnostic._config.underline ~= false)
end
