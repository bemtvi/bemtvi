-- nxvim Lua prelude — core standard library (pure helpers).
-- LuaJIT-compatible bit ops and the nx.tbl / nx.list / nx.str / nx.iter helpers
-- (with their vim.* aliases). No editor state lives here — the variable/option/
-- register stores moved to prelude/state.lua. Loaded first of the prelude chunks
-- by `LuaRuntime::new` (see runtime.rs).
local vim = vim
nx = nx or {}

-- ----- bit: LuaJIT-compatible bit ops on PUC Lua 5.4 ------------------------

-- neovim runs LuaJIT, which ships a global `bit` library; nxvim runs PUC Lua
-- 5.4, which has native bitwise *operators* but no `bit` *table* (nor 5.2's
-- `bit32`). Plugins reach for it as `bit or bit32` (catppuccin hashes its config
-- with djb2 + xor), so provide a faithful pure-Lua implementation with LuaJIT's
-- 32-bit two's-complement semantics: results are normalized to the signed
-- [-2^31, 2^31) range and shift counts are taken mod 32. Only installed when
-- absent (always, on PUC).
if not bit then
  local POW = {}
  for i = 0, 32 do
    POW[i] = 2 ^ i
  end
  local M32 = POW[32]

  -- Wrap to the unsigned 32-bit range [0, 2^32).
  local function u32(x)
    return x % M32
  end
  -- Wrap to LuaJIT's signed 32-bit result range.
  local function tobit(x)
    x = u32(x)
    if x >= POW[31] then
      x = x - M32
    end
    return x
  end

  -- Apply `f` (operating on single bits) across all 32 bit positions.
  local function bitwise(a, b, f)
    a, b = u32(a), u32(b)
    local r = 0
    for i = 0, 31 do
      local abit, bbit = a % 2, b % 2
      if f(abit, bbit) == 1 then
        r = r + POW[i]
      end
      a, b = (a - abit) / 2, (b - bbit) / 2
    end
    return tobit(r)
  end

  bit = {
    tobit = tobit,
    band = function(a, b)
      return bitwise(a, b, function(x, y)
        return x * y
      end)
    end,
    bor = function(a, b)
      return bitwise(a, b, function(x, y)
        return (x + y > 0) and 1 or 0
      end)
    end,
    bxor = function(a, b)
      return bitwise(a, b, function(x, y)
        return (x ~= y) and 1 or 0
      end)
    end,
    bnot = function(a)
      return tobit(-1 - u32(a))
    end,
    lshift = function(a, n)
      return tobit(u32(a) * POW[n % 32])
    end,
    rshift = function(a, n)
      return tobit(math.floor(u32(a) / POW[n % 32]))
    end,
    arshift = function(a, n)
      return tobit(math.floor(tobit(a) / POW[n % 32]))
    end,
  }
end

-- nx.str.* string helpers (aliases vim.fn.trim / str2list / nr2char / strchars /
-- strdisplaywidth / strcharpart / strtrans). nx.str.trim(text[, mask[, dir]]):
-- strip the characters in `mask` (default the whitespace set) from `text`. `dir` 0
-- trims both ends (default), 1 leading only, 2 trailing only. `mask` is a *set* of
-- characters, not a pattern. nvim-dap-python trims command output through this.
nx.str = nx.str or {}
function nx.str.trim(text, mask, dir)
  text = tostring(text or "")
  if mask == nil or mask == "" then
    mask = " \t\n\r\f\v"
  end
  dir = dir or 0
  local set = {}
  for i = 1, #mask do
    set[mask:sub(i, i)] = true
  end
  local from, to = 1, #text
  if dir == 0 or dir == 1 then
    while from <= to and set[text:sub(from, from)] do
      from = from + 1
    end
  end
  if dir == 0 or dir == 2 then
    while to >= from and set[text:sub(to, to)] do
      to = to - 1
    end
  end
  return text:sub(from, to)
end
vim.fn.trim = nx.str.trim

-- ----- table / list / string helpers ----------------------------------------

-- `nx.tbl.*` / `nx.list.*` are the canonical table/list helper namespaces; the
-- bare `vim.tbl_*` / `vim.list_*` names are thin aliases onto them.
nx.tbl = nx.tbl or {}
nx.list = nx.list or {}

-- nx.tbl.is_empty(t) [alias vim.tbl_isempty]: does `t` have no entries?
function nx.tbl.is_empty(t)
  return next(t) == nil
end
vim.tbl_isempty = nx.tbl.is_empty

-- nx.tbl.contains(t, value) [alias vim.tbl_contains]: is `value` one of `t`'s values?
function nx.tbl.contains(t, value)
  for _, v in pairs(t) do
    if v == value then
      return true
    end
  end
  return false
end
vim.tbl_contains = nx.tbl.contains

-- nx.tbl.keys(t) [alias vim.tbl_keys]: a list of `t`'s keys.
function nx.tbl.keys(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  return keys
end
vim.tbl_keys = nx.tbl.keys

-- nx.tbl.values(t) [alias vim.tbl_values]: a list of `t`'s values.
function nx.tbl.values(t)
  local values = {}
  for _, v in pairs(t) do
    values[#values + 1] = v
  end
  return values
end
vim.tbl_values = nx.tbl.values

-- nx.tbl.count(t) [alias vim.tbl_count]: number of entries in `t` (any keys, not just the sequence).
function nx.tbl.count(t)
  local n = 0
  for _ in pairs(t) do
    n = n + 1
  end
  return n
end
vim.tbl_count = nx.tbl.count

-- nx.tbl.deep_equal(a, b) [alias vim.deep_equal]: structural equality (recurses
-- into tables, comparing keys and values). A general config/plugin helper.
function nx.tbl.deep_equal(a, b)
  if a == b then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  for k, v in pairs(a) do
    if not nx.tbl.deep_equal(v, b[k]) then
      return false
    end
  end
  for k in pairs(b) do
    if a[k] == nil then
      return false
    end
  end
  return true
end
vim.deep_equal = nx.tbl.deep_equal

-- nx.npcall(fn, ...) [alias vim.npcall]: pcall that maps failure to nil — `select(2, pcall(...))`
-- on success, nil on error. A neovim helper kept for config/plugin convenience
-- (wrap a call that may raise and treat failure as "no value").
function nx.npcall(fn, ...)
  local ok, rv = pcall(fn, ...)
  if ok then
    return rv
  end
end
vim.npcall = nx.npcall

-- nx.nonnil(...) [alias vim.nonnil]: the first non-nil argument, or nil (verbatim from neovim's
-- vim/_core/shared.lua; the replacement for the deprecated vim.F.if_nil). A general
-- helper for defaulting an optional value.
function nx.nonnil(...)
  local nargs = select("#", ...)
  for i = 1, nargs do
    local v = select(i, ...)
    if v ~= nil then
      return v
    end
  end
  return nil
end
vim.nonnil = nx.nonnil

-- nx._tointeger / nx._assert_integer: integer coercion (verbatim from neovim's
-- vim/_core/shared.lua). vim.func._memoize uses them to parse a `concat-N` hash
-- spec; _assert_integer raises on a non-integer, _tointeger returns nil.
function nx._tointeger(x, base)
  local n = tonumber(x, base)
  if n and n == math.floor(n) then
    return n
  end
end

function nx._assert_integer(x, base)
  return nx._tointeger(x, base) or error(("Cannot convert %s to integer"):format(x))
end

-- nx.tbl.get(o, ...) [alias vim.tbl_get]: follow the `...` keys into nested table `o`, returning the
-- value reached or nil if any step is missing (or hits a non-table before the
-- last key). The safe nested access `lsp/<server>.lua` configs use to read deep
-- settings (e.g. rust_analyzer's `settings['rust-analyzer'].cargo.sysrootSrc`).
function nx.tbl.get(o, ...)
  local keys = { ... }
  if #keys == 0 then
    return nil
  end
  for _, k in ipairs(keys) do
    if type(o) ~= "table" then
      return nil
    end
    o = o[k]
    if o == nil then
      return nil
    end
  end
  return o
end
vim.tbl_get = nx.tbl.get

-- nx.tbl.filter(f, t) [alias vim.tbl_filter]: Iterates with `pairs` (not `ipairs`) to match neovim: callers filter
-- name-keyed maps too (a plugin manager filters its plugin set, keyed by plugin
-- name), not just arrays. The result is always a fresh array.
function nx.tbl.filter(f, t)
  local out = {}
  for _, v in pairs(t) do
    if f(v) then
      out[#out + 1] = v
    end
  end
  return out
end
vim.tbl_filter = nx.tbl.filter

-- nx.tbl.map(f, t) [alias vim.tbl_map]: apply `f` to each value, keeping keys.
function nx.tbl.map(f, t)
  local out = {}
  for k, v in pairs(t) do
    out[k] = f(v)
  end
  return out
end
vim.tbl_map = nx.tbl.map

-- nx.tbl.flatten(t) [alias vim.tbl_flatten]: a single list with every nested list flattened into it
-- (depth-first). Deprecated in neovim but still called by `lspconfig.util`.
function nx.tbl.flatten(t)
  local out = {}
  local function flatten(list)
    for _, v in ipairs(list) do
      if type(v) == "table" then
        flatten(v)
      else
        out[#out + 1] = v
      end
    end
  end
  flatten(t)
  return out
end
vim.tbl_flatten = nx.tbl.flatten

-- nx.tbl.deepcopy(orig) [alias vim.deepcopy]: a recursive copy of `orig` (metatables preserved).
function nx.tbl.deepcopy(orig)
  if type(orig) ~= "table" then
    return orig
  end
  local copy = {}
  for k, v in pairs(orig) do
    copy[nx.tbl.deepcopy(k)] = nx.tbl.deepcopy(v)
  end
  return setmetatable(copy, getmetatable(orig))
end
vim.deepcopy = nx.tbl.deepcopy

-- nx.tbl.deep_extend(behavior, ...) [alias vim.tbl_deep_extend]: Merge `...` maps into one. `behavior` is "force" | "keep" | "error". Nested
-- tables merge recursively; scalar conflicts resolve per `behavior`.
function nx.tbl.deep_extend(behavior, ...)
  local result = {}
  local function merge(dst, src)
    for k, v in pairs(src) do
      if type(v) == "table" and type(dst[k]) == "table" then
        merge(dst[k], v)
      elseif dst[k] == nil or behavior == "force" then
        dst[k] = nx.tbl.deepcopy(v)
      elseif behavior == "error" then
        error("key found in more than one map: " .. tostring(k))
      end -- "keep": leave dst[k] as-is
    end
  end
  for i = 1, select("#", ...) do
    merge(result, (select(i, ...)))
  end
  return result
end
vim.tbl_deep_extend = nx.tbl.deep_extend

-- nx.tbl.extend(behavior, ...) [alias vim.tbl_extend]: Shallow variant of nx.tbl.deep_extend.
function nx.tbl.extend(behavior, ...)
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
vim.tbl_extend = nx.tbl.extend

-- nx.list.extend(dst, src, start, finish) [alias vim.list_extend]: append `src[start..finish]` onto `dst`.
function nx.list.extend(dst, src, start, finish)
  start = start or 1
  finish = finish or #src
  for i = start, finish do
    dst[#dst + 1] = src[i]
  end
  return dst
end
vim.list_extend = nx.list.extend

-- nx.list.slice(list, start, finish) [alias vim.list_slice]: a copy of `list[start..finish]` (1-based,
-- inclusive; negative indices count from the end, as neovim). A completion plugin
-- caps its menu with `vim.list_slice(entries, 1, max_view_entries)`.
function nx.list.slice(list, start, finish)
  local n = #list
  start = start or 1
  finish = finish or n
  if start < 0 then
    start = n + start + 1
  end
  if finish < 0 then
    finish = n + finish + 1
  end
  local out = {}
  for i = start, finish do
    out[#out + 1] = list[i]
  end
  return out
end
vim.list_slice = nx.list.slice

-- nx.str.startswith(s, prefix) [alias vim.startswith]: does `s` begin with `prefix`?
function nx.str.startswith(s, prefix)
  return s:sub(1, #prefix) == prefix
end
vim.startswith = nx.str.startswith
-- nx.str.endswith(s, suffix) [alias vim.endswith]: does `s` end with `suffix`?
function nx.str.endswith(s, suffix)
  return suffix == "" or s:sub(-#suffix) == suffix
end
vim.endswith = nx.str.endswith

-- nx.str.split(s, sep, opts) [alias vim.split]: split `s` on `sep`.
function nx.str.split(s, sep, opts)
  -- Legacy positional form `vim.split(s, sep, plain)`: neovim keeps this
  -- backward-compat (a boolean third arg is the `plain` flag), and nvim-treesitter
  -- still calls `vim.split(path, '.', true)`. Without this it indexed a boolean as
  -- `opts.plain` and errored, breaking `require('nvim-treesitter').setup`.
  if type(opts) == "boolean" then
    opts = { plain = opts }
  end
  opts = opts or {}
  -- Empty separator: split into individual characters, matching neovim
  -- (`vim.split("nxso", "") == { "n", "x", "s", "o" }`, `vim.split("", "") == {}`)
  -- with no leading/trailing empty segment. `string.find(s, "", pos)` returns a
  -- zero-width match at `pos` (`from == pos`, `to == pos - 1`), so the generic
  -- loop below would leave `pos` unmoved and spin forever — a plugin hits this
  -- via `vim.split(modes, "")` (e.g. `"nxso"`). Handled up front; `trimempty` is a
  -- no-op here since single characters are never empty.
  if sep == "" then
    local parts = {}
    for i = 1, #s do
      parts[i] = string.sub(s, i, i)
    end
    return parts
  end
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
    while #parts > 0 and parts[#parts] == "" do
      parts[#parts] = nil
    end
    while #parts > 0 and parts[1] == "" do
      table.remove(parts, 1)
    end
  end
  return parts
end
vim.split = nx.str.split

-- ----- vim.fn string-width / character builtins ------------------------------
-- The display/character helpers a popup plugin calls to lay out its
-- grid over UTF-8 text. These decode UTF-8 by hand over Lua's byte strings (5.4
-- ships a `utf8` library, but these predate the bump and could later use it).
-- (vim.fn already exists — the Rust bridge created it before the prelude loads —
-- so these extend it.)

-- Decode the codepoint starting at byte index `i` (1-based) of `s`, returning
-- (codepoint, byte_length), or (nil, 0) past the end. A malformed / truncated
-- sequence is treated as a single 1-byte char so iteration always advances.
local function utf8_decode(s, i)
  local b = s:byte(i)
  if not b then
    return nil, 0
  end
  if b < 0x80 then
    return b, 1
  end
  if b >= 0xF0 then
    local b2, b3, b4 = s:byte(i + 1), s:byte(i + 2), s:byte(i + 3)
    if b2 and b3 and b4 then
      return (b % 0x08) * 0x40000 + (b2 % 0x40) * 0x1000 + (b3 % 0x40) * 0x40 + (b4 % 0x40), 4
    end
  elseif b >= 0xE0 then
    local b2, b3 = s:byte(i + 1), s:byte(i + 2)
    if b2 and b3 then
      return (b % 0x10) * 0x1000 + (b2 % 0x40) * 0x40 + (b3 % 0x40), 3
    end
  elseif b >= 0xC0 then
    local b2 = s:byte(i + 1)
    if b2 then
      return (b % 0x20) * 0x40 + (b2 % 0x40), 2
    end
  end
  return b, 1 -- ASCII control, stray continuation, or truncated lead byte
end

-- Display cells one codepoint occupies: 2 for the common East-Asian-wide and
-- emoji ranges, else 1. INCOMPLETE: a pragmatic range check, not the full
-- Unicode east-asian-width / emoji tables, and combining marks (which should be
-- width 0) count as 1 — close enough for popup grid layout, wrong for dense CJK
-- with combining marks. A real impl would consult a generated width table.
local function char_width(cp)
  if
    cp >= 0x1100
    and (
      cp <= 0x115F -- Hangul Jamo
      or (cp >= 0x2E80 and cp <= 0xA4CF and cp ~= 0x303F) -- CJK … Yi
      or (cp >= 0xAC00 and cp <= 0xD7A3) -- Hangul Syllables
      or (cp >= 0xF900 and cp <= 0xFAFF) -- CJK Compat Ideographs
      or (cp >= 0xFE30 and cp <= 0xFE4F) -- CJK Compat Forms
      or (cp >= 0xFF00 and cp <= 0xFF60) -- Fullwidth Forms
      or (cp >= 0xFFE0 and cp <= 0xFFE6) -- Fullwidth signs
      or (cp >= 0x1F300 and cp <= 0x1FAFF) -- emoji & pictographs
      or (cp >= 0x20000 and cp <= 0x3FFFD) -- CJK Ext B+
    )
  then
    return 2
  end
  return 1
end

-- Encode codepoint `cp` to its UTF-8 byte string. The inverse of `utf8_decode`,
-- backing `vim.fn.nr2char`. An out-of-range / negative value is clamped to U+FFFD
-- (the replacement char) so it always yields a valid string.
local function utf8_encode(cp)
  cp = math.floor(tonumber(cp) or 0)
  if cp < 0 or cp > 0x10FFFF then
    cp = 0xFFFD
  end
  if cp < 0x80 then
    return string.char(cp)
  elseif cp < 0x800 then
    return string.char(0xC0 + math.floor(cp / 0x40), 0x80 + cp % 0x40)
  elseif cp < 0x10000 then
    return string.char(
      0xE0 + math.floor(cp / 0x1000),
      0x80 + math.floor(cp / 0x40) % 0x40,
      0x80 + cp % 0x40
    )
  end
  return string.char(
    0xF0 + math.floor(cp / 0x40000),
    0x80 + math.floor(cp / 0x1000) % 0x40,
    0x80 + math.floor(cp / 0x40) % 0x40,
    0x80 + cp % 0x40
  )
end

-- nx.str.to_list(s[, utf8]) [alias vim.fn.str2list]: the codepoint of each character
-- in `s`, as a list of numbers (`str2list("AB") == { 65, 66 }`). nxvim is always
-- UTF-8, so the `utf8` flag is accepted and ignored (the result is the same either
-- way). A plugin's key parser round-trips a keymap's lhs through this and nr2char.
function nx.str.to_list(s, _utf8)
  s = tostring(s or "")
  local out, i = {}, 1
  while i <= #s do
    local cp, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    out[#out + 1] = cp
    i = i + len
  end
  return out
end
vim.fn.str2list = nx.str.to_list

-- nx.str.from_char(nr[, utf8]) [alias vim.fn.nr2char]: the string for codepoint `nr`
-- (`nr2char(65) == "A"`). The inverse of one nx.str.to_list element; nxvim is always
-- UTF-8 so `utf8` is accepted and ignored.
function nx.str.from_char(nr, _utf8)
  return utf8_encode(nr)
end
vim.fn.nr2char = nx.str.from_char

-- nx.str.chars(s[, skipcc]) [alias vim.fn.strchars]: number of characters
-- (codepoints) in `s`. INCOMPLETE: `skipcc` (skip composing characters) is ignored —
-- every codepoint counts, since nxvim doesn't classify combining marks.
function nx.str.chars(s, _skipcc)
  s = tostring(s or "")
  local i, n = 1, 0
  while i <= #s do
    local _, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    i, n = i + len, n + 1
  end
  return n
end
vim.fn.strchars = nx.str.chars

-- nx.str.displaywidth(s[, col]) [alias vim.fn.strdisplaywidth]: the display cells `s`
-- occupies, expanding tabs to the next tabstop boundary and counting wide chars as
-- two. `col` is the starting screen column used for tab-stop math (default 0); the
-- return value is the width of `s` itself (cells consumed beyond `col`). INCOMPLETE:
-- tabs expand on a fixed tabstop of 8, not the current buffer's 'tabstop'.
function nx.str.displaywidth(s, col)
  s = tostring(s or "")
  local ts, base = 8, col or 0
  local w, i = base, 1
  while i <= #s do
    local cp, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    if cp == 9 then
      w = w + (ts - (w % ts)) -- tab advances to the next tabstop
    else
      w = w + char_width(cp)
    end
    i = i + len
  end
  return w - base
end
vim.fn.strdisplaywidth = nx.str.displaywidth

-- nx.str.utfindex(s, [encoding,] index) [alias vim.str_utfindex]: convert a *byte* offset into `s` to a
-- UTF code-unit count, supporting both neovim signatures (a completion plugin probes
-- the version and uses whichever the running editor offers):
--   * pre-0.11  vim.str_utfindex(s [, byteidx])        -> utf32, utf16  (two values)
--   * 0.11+     vim.str_utfindex(s, encoding, byteidx) -> single index for encoding
-- `byteidx` defaults to #s (end of string) and is clamped into range. The count is
-- whole codepoints whose start byte falls at or before `byteidx`; a codepoint
-- outside the BMP (4-byte UTF-8) is one utf-32 unit but two utf-16 units.
local function utf_unit_counts(s, byteidx)
  byteidx = byteidx or #s
  if byteidx < 0 then
    byteidx = 0
  elseif byteidx > #s then
    byteidx = #s
  end
  local u32, u16, i = 0, 0, 1
  while i <= byteidx do
    local _, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    u32 = u32 + 1
    u16 = u16 + (len == 4 and 2 or 1)
    i = i + len
  end
  return u32, u16
end

function nx.str.utfindex(s, a, b)
  s = tostring(s or "")
  if type(a) == "string" then
    -- 0.11+ form: (s, encoding, index). utf-8 reports the codepoint count.
    local u32, u16 = utf_unit_counts(s, b)
    if a == "utf-16" then
      return u16
    end
    return u32
  end
  -- legacy form: (s [, index]) -> utf32, utf16.
  return utf_unit_counts(s, a)
end
vim.str_utfindex = nx.str.utfindex

-- nx.str.byteindex(s, [encoding,] index) [alias vim.str_byteindex]: the inverse — the byte offset of the
-- `index`-th UTF code unit. Mirrors str_utfindex's dual signature; the legacy form
-- counts utf-32 units (a 4-byte codepoint is one unit), the 0.11+ form honors the
-- requested encoding (utf-16 lets `index` land mid-astral, snapping to the
-- codepoint start). Clamps past-the-end indices to #s.
local function byteindex_for(s, index, utf16)
  if index == nil or index <= 0 then
    return 0
  end
  local i, units = 1, 0
  while i <= #s do
    local _, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    local step = (utf16 and len == 4) and 2 or 1
    if units + step > index then
      return i - 1
    end
    units = units + step
    i = i + len
    if units >= index then
      return i - 1
    end
  end
  return #s
end

function nx.str.byteindex(s, a, b)
  s = tostring(s or "")
  if type(a) == "string" then
    return byteindex_for(s, b, a == "utf-16")
  end
  return byteindex_for(s, a, false)
end
vim.str_byteindex = nx.str.byteindex

-- nx.str.charpart(s, start[, len]) [alias vim.fn.strcharpart]: the substring of `s`
-- starting at character index `start` (0-based), spanning `len` characters (default:
-- to the end). A negative `start` drops that many leading characters off the count
-- (vim's behavior) and clamps the start to 0.
function nx.str.charpart(s, start, len)
  s = tostring(s or "")
  start = start or 0
  if start < 0 then
    if len ~= nil then
      len = len + start
    end
    start = 0
  end
  if len ~= nil and len <= 0 then
    return ""
  end
  local out, idx, i = {}, 0, 1
  while i <= #s do
    local _, blen = utf8_decode(s, i)
    if blen == 0 then
      break
    end
    if idx >= start and (len == nil or idx < start + len) then
      out[#out + 1] = s:sub(i, i + blen - 1)
    end
    idx = idx + 1
    i = i + blen
  end
  return table.concat(out)
end
vim.fn.strcharpart = nx.str.charpart

-- nx.str.trans(s) [alias vim.fn.strtrans]: `s` with unprintable characters shown as
-- printable text — control chars 0x00–0x1F as ^@…^_, 0x7F as ^? — matching vim, so a
-- key label built from raw bytes displays readably. Multibyte UTF-8 is left intact.
function nx.str.trans(s)
  s = tostring(s or "")
  return (
    s:gsub("[%z\1-\31\127]", function(c)
      local b = c:byte()
      if b == 127 then
        return "^?"
      end
      return "^" .. string.char(b + 64)
    end)
  )
end
vim.fn.strtrans = nx.str.trans

-- nx.keytrans(s) [alias vim.fn.keytrans]: translate the internal form of a key
-- sequence to readable key notation (`<C-w>`, `<Space>`, …). nxvim represents keys
-- AS that notation throughout (parse_keys / nvim_feedkeys consume notation directly,
-- and nvim_replace_termcodes returns its input unchanged), so the internal form
-- already IS the notation — this returns `s` unchanged, the inverse of
-- nvim_replace_termcodes exactly as in vim.
function nx.keytrans(s)
  return tostring(s or "")
end
vim.fn.keytrans = nx.keytrans

-- nx.strwidth(text): the display cells `text` occupies (wide chars count as
-- two). Unlike strdisplaywidth it does not expand tabs — it measures the raw
-- string — matching neovim's API. Shares the char-width table above.
function nx.strwidth(text)
  text = tostring(text or "")
  local w, i = 0, 1
  while i <= #text do
    local cp, len = utf8_decode(text, i)
    if len == 0 then
      break
    end
    w = w + char_width(cp)
    i = i + len
  end
  return w
end
vim.api.nvim_strwidth = nx.strwidth

-- nx.tbl.spairs(t) [alias vim.spairs]: pairs() in sorted-key order. Neovim's stable-iteration helper —
-- a custom `'tabline'`/`str_join` uses it so output order is deterministic.
function nx.tbl.spairs(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  table.sort(keys)
  local i = 0
  return function()
    i = i + 1
    local k = keys[i]
    if k ~= nil then
      return k, t[k]
    end
  end
end
vim.spairs = nx.tbl.spairs

-- nx.print(...) [alias vim.print]: pretty-print each argument (via nx.inspect) on the message
-- line and return them unchanged, so it can wrap a value inline. Strings print
-- verbatim; tables are inspected.
function nx.print(...)
  local n = select("#", ...)
  local parts = {}
  for i = 1, n do
    local v = select(i, ...)
    parts[i] = type(v) == "string" and v or nx.inspect(v)
  end
  print(table.concat(parts, "\n"))
  return ...
end
vim.print = nx.print

-- ----- minimal vim.iter ------------------------------------------------------
-- A small chainable iterator over list-like tables: map / filter / each / fold
-- / totable, enough for what the colorscheme load path reaches for.
local Iter = {}
Iter.__index = Iter

-- nx.iter(src[, state, ctrl]) [alias vim.iter]: wrap a list-like table OR a Lua iterator triple
-- in a chainable iterator. The triple form is what `vim.iter(vim.fs.parents(p))`
-- passes — `vim.fs.parents` returns `(fn, state, start)`, which Lua spreads as
-- three args here — so the ancestors are drained eagerly into the item list.
function nx.iter(src, state, ctrl)
  local items = {}
  if type(src) == "function" then
    local var = ctrl
    while true do
      local v = src(state, var)
      if v == nil then
        break
      end
      var = v
      items[#items + 1] = v
    end
  elseif type(src) == "table" then
    for _, v in ipairs(src) do
      items[#items + 1] = v
    end
  end
  return setmetatable({ _items = items }, Iter)
end
vim.iter = nx.iter

-- Iter:find(pred): the first item for which `pred(item)` is truthy (or, when
-- `pred` is a plain value, the first item equal to it), else nil.
function Iter:find(pred)
  for _, v in ipairs(self._items) do
    if type(pred) == "function" then
      if pred(v) then
        return v
      end
    elseif v == pred then
      return v
    end
  end
  return nil
end

-- Iter:any(pred): true iff `pred(item)` is truthy for some item.
function Iter:any(pred)
  for _, v in ipairs(self._items) do
    if pred(v) then
      return true
    end
  end
  return false
end

-- Iter:flatten(): flatten one level of list-valued items into the stream.
function Iter:flatten()
  local out = {}
  for _, v in ipairs(self._items) do
    if type(v) == "table" then
      for _, inner in ipairs(v) do
        out[#out + 1] = inner
      end
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
    if r ~= nil then
      out[#out + 1] = r
    end
  end
  self._items = out
  return self
end

function Iter:filter(f)
  local out = {}
  for _, v in ipairs(self._items) do
    if f(v) then
      out[#out + 1] = v
    end
  end
  self._items = out
  return self
end

function Iter:each(f)
  for _, v in ipairs(self._items) do
    f(v)
  end
end

function Iter:fold(acc, f)
  for _, v in ipairs(self._items) do
    acc = f(acc, v)
  end
  return acc
end

function Iter:totable()
  return self._items
end

-- nx.str.substitute(str, pat, sub, flags) [alias vim.fn.substitute]: a real vim-regex
-- substitution, backed by the Rust engine (`nx._substitute`) so plugins that rely on
-- vim's magic dialect + replacement syntax (`\(\)`, `\{-}`, `&`, `\1`, `\U…\E`, …) get
-- the same result neovim gives. This is a DIFFERENT dialect from nxvim's `/` search
-- (canonical regex); the divergence is intentional and lives in the compat layer. An
-- invalid / unsupported pattern raises (fail loud).
function nx.str.substitute(str, pat, sub, flags)
  return nx._substitute(tostring(str), tostring(pat), tostring(sub or ""), tostring(flags or ""))
end
vim.fn.substitute = nx.str.substitute

-- vim.trim(s): aliases the canonical nx.str.trim (defined in stdlib.lua, a
-- superset accepting an optional mask/dir).
vim.trim = nx.str.trim

-- nx.list.is_list(t) [alias vim.islist]: true iff `t` is a list (a table whose
-- keys are exactly 1..#t).
nx.list = nx.list or {}
function nx.list.is_list(t)
  if type(t) ~= "table" then
    return false
  end
  local n = 0
  for _ in pairs(t) do
    n = n + 1
  end
  return n == #t
end
vim.islist = nx.list.is_list
vim.tbl_islist = nx.list.is_list -- the pre-0.10 name
