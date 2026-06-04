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

function vim.iter(src)
  local items = {}
  if type(src) == "table" then
    for _, v in ipairs(src) do items[#items + 1] = v end
  end
  return setmetatable({ _items = items }, Iter)
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

-- No real event loop yet: run scheduled work immediately. Sufficient for the
-- colorscheme setup/apply path, which only defers to avoid reentrancy.
function vim.schedule(fn) fn() end

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
vim._cur_buf = vim._cur_buf or { bufnr = 0, name = "" }

function vim._set_cur_buf(bufnr, name)
  vim._cur_buf = { bufnr = bufnr or 0, name = name or "" }
end

function vim.api.nvim_create_user_command(name, command, _opts)
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
-- case `file` falls back to `pattern` (the old behavior).
function vim._fire(event, pattern, buf, file)
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
          cb({ id = au.id, event = event, match = pattern, buf = buf, file = file or pattern })
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
  local raw = vim.cmd
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

-- vim.keymap.set(mode, lhs, rhs, opts): map `lhs` to `rhs` in `mode`.
-- `rhs` is a function (stored in vim._keymap_fns) or a string (fed as keys).
-- Maps are non-recursive by default (the vim.keymap.set convention); pass
-- `opts.remap = true` for a recursive map whose RHS keys are re-fed through the
-- mapping layer (or, equivalently, `opts.noremap = false`). `opts.desc` is stored
-- but unused; `opts.buffer` / `opts.default` are recorded for the precedence
-- ladder the server applies (buffer-local maps and built-in defaults arrive in
-- later phases, but the fields ride along from day one).
function vim.keymap.set(mode, lhs, rhs, opts)
  opts = opts or {}
  keymap_seq = keymap_seq + 1
  local id = keymap_seq
  local rhs_data
  if type(rhs) == "function" then
    vim._keymap_fns[id] = rhs
    rhs_data = { kind = "lua", id = id }
  else
    rhs_data = { kind = "str", str = tostring(rhs) }
  end
  vim._keymaps[#vim._keymaps + 1] = {
    id = id,
    modes = keymap_modes(mode),
    lhs = keymap_expand_leader(lhs),
    rhs = rhs_data,
    -- noremap unless either `noremap = false` or `remap = true` is given.
    noremap = opts.noremap ~= false and not opts.remap,
    buffer = opts.buffer,
    desc = opts.desc,
    default = opts.default or false,
  }
  vim._keymaps_version = vim._keymaps_version + 1
end

-- Invoke the function RHS for entry `id` (called from Rust when a Lua-backed
-- mapping fires). A no-op if no function is registered under that id.
function vim._run_keymap(id)
  local fn = vim._keymap_fns[id]
  if fn then fn() end
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
