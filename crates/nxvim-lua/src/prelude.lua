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
vim._cur_buf = vim._cur_buf or { bufnr = 0, name = "", filetype = "" }

function vim._set_cur_buf(bufnr, name, filetype)
  vim._cur_buf = { bufnr = bufnr or 0, name = name or "", filetype = filetype or "" }
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
-- `marker`. `marker` is a filename, a `function(name, path)` predicate, a list
-- (equal priority — any present matches), or a list of such groups tried in
-- order ("a then b" priority). Returns nil if none match. This is what the
-- vendored `lsp/<server>.lua` files call to compute their `root_dir`.
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
  -- Normalize `marker` to an ordered list of equal-priority groups.
  local groups
  if type(marker) == "table" and type(marker[1]) == "table" then
    groups = marker
  else
    groups = { marker }
  end
  for _, group in ipairs(groups) do
    local names = type(group) == "table" and group or { group }
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

-- Client capabilities are owned and advertised by the Rust client at
-- `initialize`; this stub lets a config that merges into them run without error.
function vim.lsp.protocol.make_client_capabilities() return {} end

vim._lsp_user_config = vim._lsp_user_config or {} -- name -> user override layer
vim._lsp_base_cache = vim._lsp_base_cache or {}   -- name -> lsp/<name>.lua result (false = none)
vim._lsp_enabled = vim._lsp_enabled or {}         -- name -> enabled?

-- Load and cache `lsp/<name>.lua` off the runtimepath (the base config layer).
-- Returns its returned table, or nil when absent / not a table.
local function lsp_base_config(name)
  local cached = vim._lsp_base_cache[name]
  if cached ~= nil then return cached or nil end
  local cfg = false
  local files = vim.api.nvim_get_runtime_file("lsp/" .. name .. ".lua", false)
  if files and files[1] then
    local src = vim._read_file(files[1])
    if src then
      local chunk = loadstring(src, "@" .. files[1])
      if chunk then
        local ok, ret = pcall(chunk)
        if ok and type(ret) == "table" then cfg = ret end
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

-- Queue a start for `bufnr` from a fully-resolved config (root already computed).
local function lsp_start_resolved(name, cfg, bufnr, ft, root)
  local cmd = cfg.cmd
  if type(cmd) == "function" then cmd = cmd() end
  vim.lsp.start(
    { name = name, cmd = cmd or {}, root_dir = root, filetypes = cfg.filetypes },
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
  local cmd = config.cmd or {}
  if type(cmd) == "function" then cmd = cmd() end
  vim._lsp_start(config.name, cmd, config.root_dir, opts.filetype or "", bufnr)
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
