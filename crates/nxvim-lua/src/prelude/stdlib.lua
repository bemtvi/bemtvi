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
  for i = 0, 32 do
    POW[i] = 2 ^ i
  end
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
    band = function(a, b)
      return bitwise(a, b, function(x, y) return x * y end)
    end,
    bor = function(a, b)
      return bitwise(a, b, function(x, y) return (x + y > 0) and 1 or 0 end)
    end,
    bxor = function(a, b)
      return bitwise(a, b, function(x, y) return (x ~= y) and 1 or 0 end)
    end,
    bnot = function(a) return tobit(-1 - u32(a)) end,
    lshift = function(a, n) return tobit(u32(a) * POW[n % 32]) end,
    rshift = function(a, n) return tobit(math.floor(u32(a) / POW[n % 32])) end,
    arshift = function(a, n) return tobit(math.floor(tobit(a) / POW[n % 32])) end,
  }
end

-- ----- option / variable stores ---------------------------------------------

-- vim.g: global variables. Plain storage; reading an unset key yields nil.
vim.g = vim.g or {}

-- vim.w / vim.b: window- and buffer-scoped variables. In neovim each is indexed
-- first by a window/buffer handle (`vim.w[win].name`) and bare access targets the
-- *current* window/buffer (`vim.w.name`). nxvim backs them with a per-handle Lua
-- store rather than a core var dict — enough for plugins that stash a marker on a
-- window/buffer and read it back (trouble.nvim tags its own windows with
-- `vim.w[win].trouble` and skips them when picking a target window; a missing
-- `vim.w` made that an index-of-nil at setup). `vim.w[0]` / `vim.b[0]` resolve to
-- the current handle, like the rest of the API.
local function scoped_vars(store, current)
  return setmetatable({}, {
    __index = function(_, k)
      if type(k) == "number" then
        local h = (k == 0) and current() or k
        local t = store[h]
        if not t then
          t = {}
          store[h] = t
        end
        return t
      end
      -- bare `vim.w.name`: the current handle's var.
      local t = store[current()]
      return t and t[k]
    end,
    __newindex = function(_, k, v)
      if type(k) == "number" then
        error("vim.w/vim.b: assign fields on vim.w[handle], not the handle itself", 2)
      end
      local h = current()
      local t = store[h]
      if not t then
        t = {}
        store[h] = t
      end
      t[k] = v
    end,
  })
end
vim._w_vars = vim._w_vars or {}
vim._b_vars = vim._b_vars or {}
vim.w = scoped_vars(vim._w_vars, function() return vim.api.nvim_get_current_win() end)
vim.b = scoped_vars(vim._b_vars, function() return vim.api.nvim_get_current_buf() end)

-- vim.o: editor options with neovim's set-semantics — a write reaches the
-- option's real home and a read returns the core's current value (the default
-- until set, and a value set through the `:set` ex path, not just one written
-- from Lua). The wired options route to the scope their name implies:
--   * number / relativenumber       -> window-local (delegated to vim.wo)
--   * tabstop / shiftwidth /
--     softtabstop / expandtab       -> buffer-local (delegated to vim.bo)
--   * ignorecase / smartcase /
--     wrapscan / hlsearch /
--     incsearch / showtabline       -> global (vim._go_mirror + the
--                                      vim._set_global_option Rust bridge)
-- Any other option (termguicolors, background, winblend, pumblend, …) lands in
-- the plain Lua store `vim._o_store`: observable read/write, not yet honored.
--
-- vim.wo / vim.bo are defined in later prelude chunks; vim.o only touches them
-- from inside its metamethods, which run at config time once every chunk has
-- loaded, so the forward reference is fine.

-- Window- and buffer-local options vim.o forwards to vim.wo / vim.bo. Keyed by
-- both the full name and its abbreviation (the delegate canonicalizes again).
local O_WIN = { number = true, nu = true, relativenumber = true, rnu = true }
local O_BUF = {
  tabstop = true,
  ts = true,
  shiftwidth = true,
  sw = true,
  softtabstop = true,
  sts = true,
  expandtab = true,
  et = true,
}
-- Global (editor-wide) options: canonical name keyed by name and abbreviation.
local O_GLOBAL = {
  ignorecase = "ignorecase",
  ic = "ignorecase",
  smartcase = "smartcase",
  scs = "smartcase",
  wrapscan = "wrapscan",
  ws = "wrapscan",
  hlsearch = "hlsearch",
  hls = "hlsearch",
  incsearch = "incsearch",
  is = "incsearch",
  autoread = "autoread",
  ar = "autoread",
  showtabline = "showtabline",
  stal = "showtabline",
  laststatus = "laststatus",
  ls = "laststatus",
  statusline = "statusline",
  stl = "statusline",
  tabline = "tabline",
  tal = "tabline",
  guifont = "guifont",
  gfn = "guifont",
  regexsyntax = "regexsyntax",
  rxs = "regexsyntax",
  -- The editor screen extent (the server pushes the live size into the mirror);
  -- read-mostly here — a float-positioning plugin (telescope) reads them to
  -- center its windows, and `:set columns=` is not honored (the client owns the
  -- terminal size), but a write still lands in the mirror so a read-back agrees.
  columns = "columns",
  co = "columns",
  lines = "lines",
}
-- Core defaults, the safety net before the server has pushed the mirror.
local O_GLOBAL_DEFAULT = {
  ignorecase = false,
  smartcase = false,
  wrapscan = true,
  hlsearch = true,
  incsearch = true,
  autoread = true,
  showtabline = 1,
  laststatus = 2,
  statusline = "",
  tabline = "",
  guifont = "",
  regexsyntax = "pcre",
  columns = 80,
  lines = 24,
}

-- Rust→Lua mirror of the core's global option values, refreshed by the server
-- (vim._set_go_mirror) before any Lua that can read options. Authoritative for
-- the wired global options, so a read reflects the core default until set and a
-- value set through the `:set` ex path, not just one written from Lua.
vim._go_mirror = vim._go_mirror or {}
function vim._set_go_mirror(t) vim._go_mirror = t or {} end

-- Rust→Lua mirror of the core register file, refreshed by the server
-- (vim._set_reg_mirror) before any Lua that can read registers. Keyed by the
-- single-char register name -> { text, type } where type is "v" (charwise) or
-- "V" (linewise). Backs vim.fn.getreg / getregtype; vim.fn.setreg write-through
-- mutates it directly so a read-after-write within one chunk stays consistent
-- (core catches up when the server drains the queued RegisterSetOp).
vim._registers = vim._registers or {}
function vim._set_reg_mirror(t) vim._registers = t or {} end

-- Arbitrary (Lua-only) global options plugins set via vim.o; the wired options
-- live in their scope (vim.wo / vim.bo / vim._go_mirror) instead. Seeded with
-- the few defaults colorschemes read (termguicolors / background / *blend).
vim._o_store = vim._o_store
  or {
    background = "dark",
    termguicolors = false,
    winblend = 0,
    pumblend = 0,
    -- Read-mostly editor options plugins (telescope, plenary.popup) read to lay out
    -- floats and gate behavior. Observable defaults matching neovim's; not yet
    -- honored by the core (the client owns the cmdline / message regions), but a
    -- read returns a sane value instead of nil (which a `- cmdheight` arithmetic or
    -- a `.. report` concat would choke on).
    cmdheight = 1,
    report = 2,
    eventignore = "",
    ambiwidth = "single",
    helplang = "en",
    mouse = "",
    guicursor = "",
    shell = os.getenv("SHELL") or "/bin/sh",
    -- On by default in vim/neovim. Plugin managers gate their own startup on it
    -- (lazy.nvim bails out of setup() entirely when `not vim.go.loadplugins`), so a
    -- nil default would silently abort them before they ever run.
    loadplugins = true,
  }

local function o_get(k)
  if O_WIN[k] then return vim.wo[k] end
  if O_BUF[k] then return vim.bo[k] end
  local canon = O_GLOBAL[k]
  if canon then
    local v = vim._go_mirror[canon]
    if v ~= nil then return v end
    return O_GLOBAL_DEFAULT[canon]
  end
  return vim._o_store[k]
end
local function o_set(k, v)
  if O_WIN[k] then
    vim.wo[k] = v
    return
  end
  if O_BUF[k] then
    vim.bo[k] = v
    return
  end
  local canon = O_GLOBAL[k]
  if canon then
    -- Queue the change for the core and write through the mirror so a
    -- read-after-write within this chunk is consistent (the server overwrites it
    -- on the next push).
    vim._set_global_option(canon, v)
    vim._go_mirror[canon] = v
    return
  end
  vim._o_store[k] = v
end

vim.o = setmetatable({}, {
  __index = function(_, k) return o_get(k) end,
  __newindex = function(_, k, v) o_set(k, v) end,
})

-- An option name nxvim actually models (any scope): the routed window/buffer/
-- global options plus the read-mostly catch-all store. Used by vim.fn.exists to
-- answer the `&opt` / `+opt` probe honestly — 1 only for options we really have.
local function option_known(name)
  return O_WIN[name]
    or O_BUF[name]
    or O_GLOBAL[name] ~= nil
    or O_GLOBAL_DEFAULT[name] ~= nil
    or vim._o_store[name] ~= nil
end

-- vim.fn.exists(expr): does the vim entity named by `expr` exist? (1 / 0). nxvim
-- answers the forms it can verify and reports 0 for the rest (rather than a fake
-- 1) so feature-probing stays honest:
--   * '&opt' / '&l:opt' / '&g:opt' / '+opt'  -> an option nxvim models. nvim-cmp
--     gates every window-option write on `exists('+'..key)`, so an unknown option
--     is skipped instead of erroring the float setup.
--   * 'g:'/'b:'/'w:'/'t:'/'v:' prefixed name -> that scoped variable is set.
--   * everything else ('*func', ':Cmd', bare names) -> 0 (can't confirm).
function vim.fn.exists(expr)
  expr = tostring(expr or "")
  local lead = expr:sub(1, 1)
  if lead == "&" or lead == "+" then
    local name = expr:sub(2):gsub("^[gl]:", "")
    return option_known(name) and 1 or 0
  end
  local scope, name = expr:match("^([gbwtv]):(.+)$")
  if scope then
    local tbl = ({ g = vim.g, b = vim.b, w = vim.w, t = vim.t, v = vim.v })[scope]
    if tbl == nil then return 0 end
    local ok, val = pcall(function() return tbl[name] end)
    return (ok and val ~= nil) and 1 or 0
  end
  return 0
end

-- vim.fn.hlexists(name): is the highlight group `name` defined? (1 / 0). Backed by
-- the same `vim._hl_defs` registry nvim_get_hl reads (concrete groups and links
-- both count). LuaSnip probes this to drop ext-mark highlight groups that aren't
-- defined (`vim.fn.hlexists(group) == 1 and group or nil`), so a missing builtin
-- errored its setup; an undefined group correctly answers 0, leaving it unstyled.
function vim.fn.hlexists(name) return (vim._hl_defs or {})[name] ~= nil and 1 or 0 end

-- vim.fn.trim(text[, mask[, dir]]): strip the characters in `mask` (default the
-- whitespace set) from `text`. `dir` 0 trims both ends (default), 1 leading only,
-- 2 trailing only. `mask` is a *set* of characters, not a pattern. nvim-dap-python
-- trims interpreter-path command output through this at setup.
function vim.fn.trim(text, mask, dir)
  text = tostring(text or "")
  if mask == nil or mask == "" then mask = " \t\n\r\f\v" end
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

-- vim.opt: neovim's rich Option object. Reading a field yields an Option wrapping
-- the option's current value; the methods (:get / :append / :prepend / :remove)
-- and the +/-/^ operators mutate list / char-flag / key:val-map options the way
-- plugin configs (and plugin managers) expect, and a table assignment
-- (`vim.opt.rtp = { ... }`) encodes back to the option's comma string. Scope
-- routing is inherited from vim.o. For the runtimepath family a mutation also
-- feeds Lua's package.path, so a freshly-added plugin dir becomes require-able —
-- matching neovim, where runtimepath drives module search. (The earlier thin
-- scalar proxy sufficed for colorscheme get/set but broke `vim.opt.rtp:append`.)

-- Option "kinds": list (comma-separated <-> Lua array), map (comma-separated
-- key:val <-> Lua table), flag (concatenated single chars <-> char set). Keyed by
-- full name and abbreviation; everything else is a plain scalar.
local OPT_LIST = {
  runtimepath = true,
  rtp = true,
  packpath = true,
  pp = true,
  path = true,
  pa = true,
  tags = true,
  tag = true,
  wildignore = true,
  wig = true,
  backupdir = true,
  bdir = true,
  directory = true,
  dir = true,
  undodir = true,
  udir = true,
  diffopt = true,
  dip = true,
  completeopt = true,
  cot = true,
  sessionoptions = true,
  ssop = true,
  viewoptions = true,
  vop = true,
  switchbuf = true,
  swb = true,
  clipboard = true,
  cb = true,
  spelllang = true,
  spl = true,
  errorformat = true,
  efm = true,
  grepformat = true,
  gfm = true,
  comments = true,
  com = true,
  whichwrap = true,
  ww = true,
  virtualedit = true,
  ve = true,
  complete = true,
  cpt = true,
  wildmode = true,
  wim = true,
}
local OPT_MAP = {
  listchars = true,
  lcs = true,
  fillchars = true,
  fcs = true,
}
local OPT_FLAG = {
  shortmess = true,
  shm = true,
  formatoptions = true,
  fo = true,
  cpoptions = true,
  cpo = true,
  guioptions = true,
  go = true,
  mouse = true,
  concealcursor = true,
  cocu = true,
}

-- The kind of `name`. `assigning_table` biases an unknown option toward "list"
-- (a plugin passing a table almost always means a comma list); otherwise unknown
-- options are scalars.
local function opt_kind(name, assigning_table)
  if OPT_LIST[name] then return "list" end
  if OPT_MAP[name] then return "map" end
  if OPT_FLAG[name] then return "flag" end
  return assigning_table and "list" or "scalar"
end

local function opt_split_comma(raw)
  local out = {}
  for piece in tostring(raw or ""):gmatch("[^,]+") do
    out[#out + 1] = piece
  end
  return out
end

-- Decode the option's stored string form into its kind's Lua value.
local function opt_decode(kind, raw)
  if kind == "list" then
    return opt_split_comma(raw)
  elseif kind == "map" then
    local m = {}
    for _, piece in ipairs(opt_split_comma(raw)) do
      local key, val = piece:match("^(.-):(.*)$")
      if key then
        m[key] = val
      else
        m[piece] = true
      end
    end
    return m
  elseif kind == "flag" then
    local m, s = {}, tostring(raw or "")
    for i = 1, #s do
      m[s:sub(i, i)] = true
    end
    return m
  end
  return raw
end

-- Encode a kind's Lua value back to the option's string form.
local function opt_encode(kind, val)
  if kind == "list" then
    local parts = {}
    for _, v in ipairs(val) do
      parts[#parts + 1] = tostring(v)
    end
    return table.concat(parts, ",")
  elseif kind == "map" then
    local parts = {}
    for k, v in pairs(val) do
      if v == true then
        parts[#parts + 1] = k
      elseif v then
        parts[#parts + 1] = k .. ":" .. tostring(v)
      end
    end
    return table.concat(parts, ",")
  elseif kind == "flag" then
    local parts = {}
    if vim.islist(val) then
      for _, c in ipairs(val) do
        parts[#parts + 1] = c
      end
    else
      for k, v in pairs(val) do
        if v then parts[#parts + 1] = k end
      end
    end
    return table.concat(parts)
  end
  return val
end

-- Appending to the runtimepath family must make the new dir's lua/ require-able,
-- the way neovim drives module search off runtimepath. Mirror the pattern
-- seed_package_path uses on the host side.
local OPT_RTP = { runtimepath = true, rtp = true, packpath = true, pp = true }
local function opt_seed_require(name, entries)
  if not OPT_RTP[name] then return end
  for _, e in ipairs(entries) do
    e = tostring(e)
    package.path = package.path .. ";" .. e .. "/lua/?.lua;" .. e .. "/lua/?/init.lua"
  end
end

local Option = {}
Option.__index = Option

local function opt_new(name, kind, value)
  return setmetatable({ _name = name, _kind = kind, _value = value }, Option)
end

-- A scalar option being list-mutated (an unknown comma option) promotes to a list.
local function opt_promote(self)
  if self._kind == "scalar" then
    self._kind = "list"
    self._value = opt_split_comma(self._value)
  end
end

-- Apply op ∈ {append, prepend, remove} to `self._value`, writing through unless
-- `noflush` (the +/-/^ operators build a value that the assignment flushes).
local function opt_mutate(self, op, v, noflush)
  opt_promote(self)
  local kind = self._kind
  if kind == "flag" then
    local s = tostring(v)
    for i = 1, #s do
      self._value[s:sub(i, i)] = (op ~= "remove") or nil
    end
  elseif kind == "map" then
    if op == "remove" then
      local keys = type(v) == "table" and (vim.islist(v) and v or vim.tbl_keys(v)) or { v }
      for _, k in ipairs(keys) do
        self._value[k] = nil
      end
    else
      for k, val in pairs(v) do
        if op == "append" or self._value[k] == nil then self._value[k] = val end
      end
    end
  else -- list
    local items = {}
    if type(v) == "table" then
      for _, x in ipairs(v) do
        items[#items + 1] = x
      end
    else
      items[1] = v
    end
    if op == "remove" then
      local drop = {}
      for _, x in ipairs(items) do
        drop[x] = true
      end
      local keep = {}
      for _, x in ipairs(self._value) do
        if not drop[x] then keep[#keep + 1] = x end
      end
      self._value = keep
    elseif op == "prepend" then
      for i = #items, 1, -1 do
        table.insert(self._value, 1, items[i])
      end
      opt_seed_require(self._name, items)
    else -- append
      for _, x in ipairs(items) do
        self._value[#self._value + 1] = x
      end
      opt_seed_require(self._name, items)
    end
  end
  if not noflush then o_set(self._name, opt_encode(self._kind, self._value)) end
  return self
end

function Option:append(v) return opt_mutate(self, "append", v, false) end
function Option:prepend(v) return opt_mutate(self, "prepend", v, false) end
function Option:remove(v) return opt_mutate(self, "remove", v, false) end
function Option:get()
  if self._kind == "scalar" then return self._value end
  return vim.deepcopy(self._value)
end

local function opt_clone(self) return opt_new(self._name, self._kind, vim.deepcopy(self._value)) end
Option.__add = function(self, v) return opt_mutate(opt_clone(self), "append", v, true) end
Option.__pow = function(self, v) return opt_mutate(opt_clone(self), "prepend", v, true) end
Option.__sub = function(self, v) return opt_mutate(opt_clone(self), "remove", v, true) end
Option.__tostring = function(self) return tostring(opt_encode(self._kind, self._value)) end

local function opt_assign(name, v)
  if getmetatable(v) == Option then
    o_set(name, opt_encode(v._kind, v._value))
    if v._kind == "list" then opt_seed_require(name, v._value) end
  elseif type(v) == "table" then
    local kind = opt_kind(name, true)
    o_set(name, opt_encode(kind, v))
    if kind == "list" then opt_seed_require(name, v) end
  else
    o_set(name, v)
  end
end

vim.opt = setmetatable({}, {
  __index = function(_, k)
    local kind = opt_kind(k, false)
    return opt_new(k, kind, opt_decode(kind, o_get(k)))
  end,
  __newindex = function(_, k, v) opt_assign(k, v) end,
})
-- nxvim's vim.o already routes by scope, so opt_local / opt_global share the
-- same Option machinery (the forced-scope distinction neovim draws is collapsed).
vim.opt_local = vim.opt
vim.opt_global = vim.opt

-- vim.go: the *global* value of options (neovim's editor-wide scope). Unlike
-- vim.o it never delegates to the window/buffer scope — reading a window/buffer
-- option through vim.go yields its global default, matching neovim's "go is the
-- global option store" semantics. The wired global options reflect the core
-- (vim._go_mirror, the same home vim.o's global branch uses); any other option
-- lands in the plain vim._o_store (observable read/write, not yet honored).
local function go_get(k)
  local canon = O_GLOBAL[k]
  if canon then
    local v = vim._go_mirror[canon]
    if v ~= nil then return v end
    return O_GLOBAL_DEFAULT[canon]
  end
  return vim._o_store[k]
end
local function go_set(k, v)
  local canon = O_GLOBAL[k]
  if canon then
    vim._set_global_option(canon, v)
    vim._go_mirror[canon] = v
    return
  end
  vim._o_store[k] = v
end
vim.go = setmetatable({}, {
  __index = function(_, k) return go_get(k) end,
  __newindex = function(_, k, v) go_set(k, v) end,
})

-- vim.v: neovim's predefined `v:` variables. nxvim backs the few with a real
-- editor source from a Rust→Lua mirror (vim._v_mirror) the server refreshes
-- before any Lua that can read them:
--   * count    — the count accumulated for the pending command (0 when none)
--   * count1   — count, but at least 1 (v:count1)
--   * register — the register named by a leading `"x`, else `"` (the unnamed)
--   * operator — the pending operator char (`d`/`c`/`y`/…), "" when none
-- `vim_did_enter` is set to 1 once the startup VimEnter point passes (it is NOT
-- overwritten by the per-tick mirror refresh, so it stays sticky). `v:true` /
-- `v:false` are the boolean constants plugins compare against (reached via
-- `vim.v["true"]` since `true` is a Lua keyword). An unknown `v:` name reads
-- whatever was stored (nil if never set) rather than failing — `v:` is a
-- variable table, and many of neovim's predefined vars are legitimately empty.
vim._v_mirror = vim._v_mirror or { vim_did_enter = 0 }
-- Refresh the editor-sourced fields (count/register/operator); vim_did_enter and
-- any plugin-set var are preserved (the server pushes this every tick).
function vim._set_v_mirror(count, count1, register, operator)
  local m = vim._v_mirror
  m.count, m.count1, m.register, m.operator = count, count1, register, operator
end
function vim._set_vim_did_enter(v) vim._v_mirror.vim_did_enter = v and 1 or 0 end
vim.v = setmetatable({}, {
  __index = function(_, k)
    if k == "true" then return true end
    if k == "false" then return false end
    local m = vim._v_mirror
    if k == "count" then return m.count or 0 end
    if k == "count1" then return m.count1 or 1 end
    if k == "register" then return m.register or '"' end
    if k == "operator" then return m.operator or "" end
    if k == "vim_did_enter" then return m.vim_did_enter or 0 end
    -- `v:shell_error` is the exit status of the last `:!`/`system()` shell-out,
    -- 0 before any has run. `vim.fn.system`/`systemlist` write it; the lazy.nvim
    -- bootstrap branches on it (`if vim.v.shell_error ~= 0 then …`), so a `nil`
    -- default would read as "the clone failed" the very first time.
    if k == "shell_error" then return m.shell_error or 0 end
    -- `v:exiting` is `v:null` (→ vim.NIL in Lua) until the editor is actually
    -- exiting, when it becomes the exit code. Plugins gate async work on it —
    -- lazy.nvim's `Util.exiting()` is literally `vim.v.exiting ~= vim.NIL`, so a
    -- plain `nil` here reads as "already exiting" and the whole async runner
    -- (its git clone/install) silently refuses to start. Default to vim.NIL.
    if k == "exiting" then
      if m.exiting == nil then return vim.NIL end
      return m.exiting
    end
    return m[k]
  end,
  __newindex = function(_, k, v) vim._v_mirror[k] = v end,
})

-- vim.env: process environment, read through to the host; writes shadow locally
-- (a Lua-only override that wins over the host on the next read). nxvim ships its
-- runtime embedded in the binary rather than as an on-disk $VIMRUNTIME tree, but
-- plugins concatenate `vim.env.VIMRUNTIME .. "/..."` unconditionally (lazy.nvim
-- sources `$VIMRUNTIME/filetype.lua` at startup), so a nil there is a load-time
-- crash. Fall back to the data-dir runtime path: it need not be populated (nxvim
-- does its own filetype detection), and a `:source` of a missing file under it
-- fails soft.
vim._env_shadow = vim._env_shadow or {}
vim.env = setmetatable({}, {
  __index = function(_, k)
    if vim._env_shadow[k] ~= nil then return vim._env_shadow[k] end
    local v = os.getenv(k)
    if v ~= nil then return v end
    if k == "VIMRUNTIME" then return vim.fn.stdpath("data") .. "/runtime" end
    return nil
  end,
  __newindex = function(_, k, v) vim._env_shadow[k] = v end,
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
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  return keys
end

function vim.tbl_values(t)
  local values = {}
  for _, v in pairs(t) do
    values[#values + 1] = v
  end
  return values
end

-- vim.tbl_count(t): number of entries in `t` (any keys, not just the sequence).
function vim.tbl_count(t)
  local n = 0
  for _ in pairs(t) do
    n = n + 1
  end
  return n
end

-- vim.deep_equal(a, b): structural equality. Used by vim.treesitter.query to spot
-- specific directives (e.g. `#set! injection.combined`).
function vim.deep_equal(a, b)
  if a == b then return true end
  if type(a) ~= "table" or type(b) ~= "table" then return false end
  for k, v in pairs(a) do
    if not vim.deep_equal(v, b[k]) then return false end
  end
  for k in pairs(b) do
    if a[k] == nil then return false end
  end
  return true
end

-- vim.npcall(fn, ...): pcall that maps failure to nil — `select(2, pcall(...))`
-- on success, nil on error. A neovim helper kept for config/plugin convenience
-- (wrap a call that may raise and treat failure as "no value").
function vim.npcall(fn, ...)
  local ok, rv = pcall(fn, ...)
  if ok then return rv end
end

-- vim.nonnil(...): the first non-nil argument, or nil (verbatim from neovim's
-- vim/_core/shared.lua; the replacement for the deprecated vim.F.if_nil).
-- vim.treesitter.tree_for_range uses it to default `opts.ignore_injections`.
function vim.nonnil(...)
  local nargs = select("#", ...)
  for i = 1, nargs do
    local v = select(i, ...)
    if v ~= nil then return v end
  end
  return nil
end

-- vim._tointeger / vim._assert_integer: integer coercion (verbatim from neovim's
-- vim/_core/shared.lua). vim.func._memoize uses them to parse a `concat-N` hash
-- spec; _assert_integer raises on a non-integer, _tointeger returns nil.
function vim._tointeger(x, base)
  local nx = tonumber(x, base)
  if nx and nx == math.floor(nx) then return nx end
end

function vim._assert_integer(x, base)
  return vim._tointeger(x, base) or error(("Cannot convert %s to integer"):format(x))
end

-- vim.tbl_get(o, ...): follow the `...` keys into nested table `o`, returning the
-- value reached or nil if any step is missing (or hits a non-table before the
-- last key). The safe nested access `lsp/<server>.lua` configs use to read deep
-- settings (e.g. rust_analyzer's `settings['rust-analyzer'].cargo.sysrootSrc`).
function vim.tbl_get(o, ...)
  local keys = { ... }
  if #keys == 0 then return nil end
  for _, k in ipairs(keys) do
    if type(o) ~= "table" then return nil end
    o = o[k]
    if o == nil then return nil end
  end
  return o
end

-- Iterates with `pairs` (not `ipairs`) to match neovim: callers filter
-- name-keyed maps too (lazy.nvim filters `Config.plugins`, keyed by plugin
-- name), not just arrays. The result is always a fresh array.
function vim.tbl_filter(f, t)
  local out = {}
  for _, v in pairs(t) do
    if f(v) then out[#out + 1] = v end
  end
  return out
end

function vim.tbl_map(f, t)
  local out = {}
  for k, v in pairs(t) do
    out[k] = f(v)
  end
  return out
end

-- vim.tbl_flatten(t): a single list with every nested list flattened into it
-- (depth-first). Deprecated in neovim but still called by `lspconfig.util`.
function vim.tbl_flatten(t)
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

function vim.deepcopy(orig)
  if type(orig) ~= "table" then return orig end
  local copy = {}
  for k, v in pairs(orig) do
    copy[vim.deepcopy(k)] = vim.deepcopy(v)
  end
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
  for i = 1, select("#", ...) do
    merge(result, (select(i, ...)))
  end
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
  for i = start, finish do
    dst[#dst + 1] = src[i]
  end
  return dst
end

-- vim.list_slice(list, start, finish): a copy of `list[start..finish]` (1-based,
-- inclusive; negative indices count from the end, as neovim). nvim-cmp caps its
-- menu with `vim.list_slice(entries, 1, max_view_entries)`.
function vim.list_slice(list, start, finish)
  local n = #list
  start = start or 1
  finish = finish or n
  if start < 0 then start = n + start + 1 end
  if finish < 0 then finish = n + finish + 1 end
  local out = {}
  for i = start, finish do
    out[#out + 1] = list[i]
  end
  return out
end

function vim.startswith(s, prefix) return s:sub(1, #prefix) == prefix end
function vim.endswith(s, suffix) return suffix == "" or s:sub(-#suffix) == suffix end

function vim.split(s, sep, opts)
  -- Legacy positional form `vim.split(s, sep, plain)`: neovim keeps this
  -- backward-compat (a boolean third arg is the `plain` flag), and nvim-treesitter
  -- still calls `vim.split(path, '.', true)`. Without this it indexed a boolean as
  -- `opts.plain` and errored, breaking `require('nvim-treesitter').setup`.
  if type(opts) == "boolean" then opts = { plain = opts } end
  opts = opts or {}
  -- Empty separator: split into individual characters, matching neovim
  -- (`vim.split("nxso", "") == { "n", "x", "s", "o" }`, `vim.split("", "") == {}`)
  -- with no leading/trailing empty segment. `string.find(s, "", pos)` returns a
  -- zero-width match at `pos` (`from == pos`, `to == pos - 1`), so the generic
  -- loop below would leave `pos` unmoved and spin forever — which-key hits this
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

-- ----- vim.fn string-width / character builtins ------------------------------
-- The display/character helpers a popup plugin (which-key) calls to lay out its
-- grid over UTF-8 text. nxvim runs PUC Lua 5.1 (byte strings, no `utf8` library),
-- so these decode UTF-8 by hand. (vim.fn already exists — the Rust bridge created
-- it before the prelude loads — so these extend it.)

-- Decode the codepoint starting at byte index `i` (1-based) of `s`, returning
-- (codepoint, byte_length), or (nil, 0) past the end. A malformed / truncated
-- sequence is treated as a single 1-byte char so iteration always advances.
local function utf8_decode(s, i)
  local b = s:byte(i)
  if not b then return nil, 0 end
  if b < 0x80 then return b, 1 end
  if b >= 0xF0 then
    local b2, b3, b4 = s:byte(i + 1), s:byte(i + 2), s:byte(i + 3)
    if b2 and b3 and b4 then
      return (b % 0x08) * 0x40000 + (b2 % 0x40) * 0x1000 + (b3 % 0x40) * 0x40 + (b4 % 0x40), 4
    end
  elseif b >= 0xE0 then
    local b2, b3 = s:byte(i + 1), s:byte(i + 2)
    if b2 and b3 then return (b % 0x10) * 0x1000 + (b2 % 0x40) * 0x40 + (b3 % 0x40), 3 end
  elseif b >= 0xC0 then
    local b2 = s:byte(i + 1)
    if b2 then return (b % 0x20) * 0x40 + (b2 % 0x40), 2 end
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
  if cp < 0 or cp > 0x10FFFF then cp = 0xFFFD end
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

-- vim.fn.str2list(s[, utf8]): the codepoint of each character in `s`, as a list of
-- numbers (`str2list("AB") == { 65, 66 }`). nxvim is always UTF-8, so the `utf8`
-- flag is accepted and ignored (the result is the same either way). which-key's
-- key parser (`Util.keys`) round-trips a keymap's lhs through this and `nr2char`.
function vim.fn.str2list(s, _utf8)
  s = tostring(s or "")
  local out, i = {}, 1
  while i <= #s do
    local cp, len = utf8_decode(s, i)
    if len == 0 then break end
    out[#out + 1] = cp
    i = i + len
  end
  return out
end

-- vim.fn.nr2char(nr[, utf8]): the string for codepoint `nr` (`nr2char(65) == "A"`).
-- The inverse of one `str2list` element; nxvim is always UTF-8 so `utf8` is
-- accepted and ignored.
function vim.fn.nr2char(nr, _utf8) return utf8_encode(nr) end

-- vim.fn.strchars(s[, skipcc]): number of characters (codepoints) in `s`.
-- INCOMPLETE: `skipcc` (skip composing characters) is ignored — every codepoint
-- counts, since nxvim doesn't classify combining marks.
function vim.fn.strchars(s, _skipcc)
  s = tostring(s or "")
  local i, n = 1, 0
  while i <= #s do
    local _, len = utf8_decode(s, i)
    if len == 0 then break end
    i, n = i + len, n + 1
  end
  return n
end

-- vim.fn.strdisplaywidth(s[, col]): the display cells `s` occupies, expanding
-- tabs to the next tabstop boundary and counting wide chars as two. `col` is the
-- starting screen column used for tab-stop math (default 0); the return value is
-- the width of `s` itself (cells consumed beyond `col`). INCOMPLETE: tabs expand
-- on a fixed tabstop of 8, not the current buffer's 'tabstop'.
function vim.fn.strdisplaywidth(s, col)
  s = tostring(s or "")
  local ts, base = 8, col or 0
  local w, i = base, 1
  while i <= #s do
    local cp, len = utf8_decode(s, i)
    if len == 0 then break end
    if cp == 9 then
      w = w + (ts - (w % ts)) -- tab advances to the next tabstop
    else
      w = w + char_width(cp)
    end
    i = i + len
  end
  return w - base
end

-- vim.str_utfindex(s, [encoding,] index): convert a *byte* offset into `s` to a
-- UTF code-unit count, supporting both neovim signatures (nvim-cmp probes the
-- version and uses whichever the running editor offers):
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
    if len == 0 then break end
    u32 = u32 + 1
    u16 = u16 + (len == 4 and 2 or 1)
    i = i + len
  end
  return u32, u16
end

function vim.str_utfindex(s, a, b)
  s = tostring(s or "")
  if type(a) == "string" then
    -- 0.11+ form: (s, encoding, index). utf-8 reports the codepoint count.
    local u32, u16 = utf_unit_counts(s, b)
    if a == "utf-16" then return u16 end
    return u32
  end
  -- legacy form: (s [, index]) -> utf32, utf16.
  return utf_unit_counts(s, a)
end

-- vim.str_byteindex(s, [encoding,] index): the inverse — the byte offset of the
-- `index`-th UTF code unit. Mirrors str_utfindex's dual signature; the legacy form
-- counts utf-32 units (a 4-byte codepoint is one unit), the 0.11+ form honors the
-- requested encoding (utf-16 lets `index` land mid-astral, snapping to the
-- codepoint start). Clamps past-the-end indices to #s.
local function byteindex_for(s, index, utf16)
  if index == nil or index <= 0 then return 0 end
  local i, units = 1, 0
  while i <= #s do
    local _, len = utf8_decode(s, i)
    if len == 0 then break end
    local step = (utf16 and len == 4) and 2 or 1
    if units + step > index then return i - 1 end
    units = units + step
    i = i + len
    if units >= index then return i - 1 end
  end
  return #s
end

function vim.str_byteindex(s, a, b)
  s = tostring(s or "")
  if type(a) == "string" then return byteindex_for(s, b, a == "utf-16") end
  return byteindex_for(s, a, false)
end

-- vim.fn.strcharpart(s, start[, len]): the substring of `s` starting at character
-- index `start` (0-based), spanning `len` characters (default: to the end). A
-- negative `start` drops that many leading characters off the count (vim's
-- behavior) and clamps the start to 0.
function vim.fn.strcharpart(s, start, len)
  s = tostring(s or "")
  start = start or 0
  if start < 0 then
    if len ~= nil then len = len + start end
    start = 0
  end
  if len ~= nil and len <= 0 then return "" end
  local out, idx, i = {}, 0, 1
  while i <= #s do
    local _, blen = utf8_decode(s, i)
    if blen == 0 then break end
    if idx >= start and (len == nil or idx < start + len) then
      out[#out + 1] = s:sub(i, i + blen - 1)
    end
    idx = idx + 1
    i = i + blen
  end
  return table.concat(out)
end

-- vim.fn.strtrans(s): `s` with unprintable characters shown as printable text —
-- control chars 0x00–0x1F as ^@…^_, 0x7F as ^? — matching vim, so a key label
-- built from raw bytes displays readably. Multibyte UTF-8 is left intact.
function vim.fn.strtrans(s)
  s = tostring(s or "")
  return (
    s:gsub("[%z\1-\31\127]", function(c)
      local b = c:byte()
      if b == 127 then return "^?" end
      return "^" .. string.char(b + 64)
    end)
  )
end

-- vim.fn.keytrans(s): translate the internal form of a key sequence to readable
-- key notation (`<C-w>`, `<Space>`, …). nxvim represents keys AS that notation
-- throughout (parse_keys / nvim_feedkeys consume notation directly, and
-- nvim_replace_termcodes returns its input unchanged), so the internal form
-- already IS the notation — this returns `s` unchanged, the inverse of
-- nvim_replace_termcodes exactly as in vim.
function vim.fn.keytrans(s) return tostring(s or "") end

-- nvim_strwidth(text): the display cells `text` occupies (wide chars count as
-- two). Unlike strdisplaywidth it does not expand tabs — it measures the raw
-- string — matching neovim's API. Shares the char-width table above.
function vim.api.nvim_strwidth(text)
  text = tostring(text or "")
  local w, i = 0, 1
  while i <= #text do
    local cp, len = utf8_decode(text, i)
    if len == 0 then break end
    w = w + char_width(cp)
    i = i + len
  end
  return w
end

-- vim.fn.reg_recording() / reg_executing(): the register name of an in-progress
-- macro recording / replay, or "" when none. nxvim's core has no `q`-macro
-- recording yet, so both are always "" — an honest "nothing in progress" (the
-- value vim returns the vast majority of the time), not a faked recording state.
-- A statusline `%{reg_recording()}` recording indicator therefore stays blank.
function vim.fn.reg_recording() return "" end
function vim.fn.reg_executing() return "" end

-- vim.spairs(t): pairs() in sorted-key order. Neovim's stable-iteration helper —
-- a custom `'tabline'`/`str_join` uses it so output order is deterministic.
function vim.spairs(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  table.sort(keys)
  local i = 0
  return function()
    i = i + 1
    local k = keys[i]
    if k ~= nil then return k, t[k] end
  end
end

-- vim.print(...): pretty-print each argument (via vim.inspect) on the message
-- line and return them unchanged, so it can wrap a value inline. Strings print
-- verbatim; tables are inspected.
function vim.print(...)
  local n = select("#", ...)
  local parts = {}
  for i = 1, n do
    local v = select(i, ...)
    parts[i] = type(v) == "string" and v or vim.inspect(v)
  end
  print(table.concat(parts, "\n"))
  return ...
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
    for _, v in ipairs(src) do
      items[#items + 1] = v
    end
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

function Iter:totable() return self._items end
