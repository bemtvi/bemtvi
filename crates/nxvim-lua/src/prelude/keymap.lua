-- nxvim Lua prelude — keymaps.
-- vim.keymap.set / vim.keymap.del over the vim._keymaps snapshot the server compiles into per-mode tries.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

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

