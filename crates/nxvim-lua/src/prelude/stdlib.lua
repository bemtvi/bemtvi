-- nxvim Lua prelude — core standard library.
-- LuaJIT-compatible bit ops, the option/variable stores (vim.g/o/opt/env/log), the table/list/string helpers, and the minimal chainable vim.iter.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

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

