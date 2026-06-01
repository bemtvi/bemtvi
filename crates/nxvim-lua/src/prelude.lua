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

function vim.api.nvim_create_user_command(name, command, _opts)
  vim._user_commands[name] = command
end

function vim.api.nvim_create_augroup(name, _opts)
  augroup_seq = augroup_seq + 1
  vim._augroups[name] = augroup_seq
  return augroup_seq
end

function vim.api.nvim_create_autocmd(event, opts)
  autocmd_seq = autocmd_seq + 1
  vim._autocmds[#vim._autocmds + 1] = { id = autocmd_seq, event = event, opts = opts or {} }
  return autocmd_seq
end

-- Fire the registered autocmds for `event` whose pattern matches `pattern`.
-- Called from Rust (LuaRuntime::fire_autocmd) when the editor triggers an event
-- — today only `ColorScheme`, after a theme loads. A function handler runs with
-- the usual callback args table; a string `command` is queued as an ex-command.
-- Match rules: event equals (or is in) the registered event; pattern is nil/"*",
-- equals `pattern`, or is in the registered pattern list.
function vim._fire(event, pattern)
  for _, au in ipairs(vim._autocmds) do
    local ev = au.event
    local ev_ok = ev == event or (type(ev) == "table" and vim.tbl_contains(ev, event))
    if ev_ok then
      local pat = au.opts.pattern
      local pat_ok = pat == nil or pat == "*" or pat == pattern
        or (type(pat) == "table" and vim.tbl_contains(pat, pattern))
      if pat_ok then
        local cb = au.opts.callback
        if type(cb) == "function" then
          cb({ id = au.id, event = event, match = pattern, file = pattern })
        elseif type(au.opts.command) == "string" then
          vim.cmd(au.opts.command)
        end
      end
    end
  end
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
