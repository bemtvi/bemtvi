-- nxvim Lua prelude — keymaps.
-- The canonical nx.keymap.* natives over the nx._keymaps snapshot the server
-- compiles into per-mode tries: the ergonomic nx.keymap.set / nx.keymap.del
-- (vim.keymap aliases onto them) and the lower-level `:map`-family natives
-- (nx.keymap.raw_set / raw_buf_set / buf_del / get / buf_get) the muscle-memory
-- nvim_*_keymap names alias onto, in one block at the end of the file (ADR 0002).
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- keymaps ---------------------------------------------------------------
-- vim.keymap.set / .del store entries in a pure-Lua registry the server reads
-- back as data (unlike autocmds, whose *matching* stays in Lua); the server
-- compiles the snapshot into per-mode prefix tries and matches keystrokes there.
-- A function RHS is held in nx._keymap_fns keyed by the entry's stable id and
-- invoked from Rust via nx._run_keymap(id) — the run_user_command analogue.
-- Every mutation bumps nx._keymaps_version so the server rebuilds its tries
-- only when the registry actually changed (checked once per input batch).

nx._keymaps = nx._keymaps or {}
nx._keymap_fns = nx._keymap_fns or {}
nx._keymaps_version = nx._keymaps_version or 0
local keymap_seq = 0

nx.keymap = nx.keymap or {}
vim.keymap = nx.keymap

-- Normalize the `mode` argument to a list of mode codes. A bare string is one
-- mode (`'n'`, `'x'`, `''` = all); a list passes through unchanged. Each code's
-- expansion to the editor modes it covers (v/x → Visual+VisualLine, `''` → all)
-- is the server's job — it owns the per-mode tries.
local function keymap_modes(mode)
  if type(mode) == "table" then
    return mode
  end
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
  if leader == nil then
    leader = "\\"
  end
  local localleader = vim.g.maplocalleader
  if localleader == nil then
    localleader = "\\"
  end
  lhs = lhs:gsub("<[lL][eE][aA][dD][eE][rR]>", function()
    return leader
  end)
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
  if buffer == 0 then
    return nx._cur_buf and nx._cur_buf.bufnr or 0
  end
  return buffer
end

-- Does a mapping already exist for `lhs` overlapping any of `modes` at the given
-- `buffer` scope? Backs `<unique>` (opts.unique): vim errors (E227) rather than
-- overwrite. Compares the already-leader-expanded `lhs`/resolved `buffer` the
-- caller holds, and treats any mode overlap as a clash.
local function keymap_clashes(modes, lhs, buffer)
  local want = {}
  for _, m in ipairs(modes) do
    want[m] = true
  end
  for _, e in ipairs(nx._keymaps) do
    if e.lhs == lhs and e.buffer == buffer then
      for _, m in ipairs(e.modes) do
        if want[m] then
          return true
        end
      end
    end
  end
  return false
end

-- Register one keymap entry into nx._keymaps — the core of nx.keymap.set (which
-- the lower-level nvim_set_keymap / nvim_buf_set_keymap shim onto). `modes` is a
-- list of mode codes; `rhs` a function (stored in nx._keymap_fns) or a string (fed
-- as keys). `opts` is a normalized table the caller fills in: `noremap` (nx.keymap
-- defaults it true, the nvim_* family false — design D5), `buffer`, `desc`, `default`, and
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
    nx._keymap_fns[id] = rhs
    rhs_data = { kind = "lua", id = id }
  else
    -- <leader> is expanded in the string RHS too, not just the LHS, matching
    -- neovim — so a remap RHS can name another <leader> mapping.
    rhs_data = { kind = "str", str = keymap_expand_leader(tostring(rhs)) }
  end
  nx._keymaps[#nx._keymaps + 1] = {
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
  nx._keymaps_version = nx._keymaps_version + 1
end

-- Drop every buffer-local mapping bound to `bufnr` (and free any function RHS it
-- held). Called when a buffer is deleted so its maps don't outlive it and leak
-- onto a later buffer that reuses the bufnr. Bumps the version so the server
-- rebuilds its tries without the dropped entries.
function nx._purge_buf_keymaps(bufnr)
  local kept, dropped = {}, false
  for _, e in ipairs(nx._keymaps) do
    if e.buffer == bufnr then
      dropped = true
      if e.rhs.kind == "lua" then
        nx._keymap_fns[e.id] = nil
      end
    else
      kept[#kept + 1] = e
    end
  end
  if dropped then
    nx._keymaps = kept
    nx._keymaps_version = nx._keymaps_version + 1
  end
end

-- Remove the mappings for `lhs` in `modes` at the given `buffer` scope (nil for
-- global, a resolved number for buffer-local) — the core of nx.keymap.del (which
-- the nvim_*_del_keymap family shims onto). A matched entry loses only the requested
-- modes; it survives (with the rest) if it covered more, and is dropped — along
-- with any function RHS it held — only when no modes remain. Re-sourcing a config
-- that re-sets the same map therefore leaves exactly one mapping, so it can't
-- double-fire. Bumps the version so the server rebuilds its tries.
local function keymap_remove(modes, lhs, buffer)
  lhs = keymap_expand_leader(lhs)
  local want = {}
  for _, m in ipairs(modes) do
    want[m] = true
  end
  local kept = {}
  for _, e in ipairs(nx._keymaps) do
    if e.lhs == lhs and e.buffer == buffer then
      local remaining = {}
      for _, m in ipairs(e.modes) do
        if not want[m] then
          remaining[#remaining + 1] = m
        end
      end
      if #remaining > 0 then
        e.modes = remaining
        kept[#kept + 1] = e
      elseif e.rhs.kind == "lua" then
        nx._keymap_fns[e.id] = nil
      end
    else
      kept[#kept + 1] = e
    end
  end
  nx._keymaps = kept
  nx._keymaps_version = nx._keymaps_version + 1
end

-- nx.keymap.set(mode, lhs, rhs, opts): map `lhs` to `rhs` in `mode`.
-- `rhs` is a function (stored in nx._keymap_fns) or a string (fed as keys).
-- Maps are non-recursive by default (the nx.keymap.set convention); pass
-- `opts.remap = true` for a recursive map whose RHS keys are re-fed through the
-- mapping layer (or, equivalently, `opts.noremap = false`). `opts.desc` is stored
-- but unused; `opts.buffer` ties the map to one buffer (0 = current), `opts.default`
-- marks an overridable built-in — both feed the precedence ladder the server applies.
function nx.keymap.set(mode, lhs, rhs, opts)
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

-- nx.keymap.del(mode, lhs, opts): remove the mapping(s) for `lhs` in `mode`.
-- `opts.buffer` (0 = current) targets a buffer-local map; absent targets globals.
function nx.keymap.del(mode, lhs, opts)
  opts = opts or {}
  keymap_remove(keymap_modes(mode), lhs, keymap_resolve_buffer(opts.buffer))
end

-- The lower-level `:map`-family API as canonical `nx.keymap.*` natives (the
-- nvim_*_keymap names alias onto them at the end of the file). These are a thin
-- convention-shim over `nx.keymap.set` / `nx.keymap.del` above — there is no
-- second registration path. Only the calling convention differs and is normalized
-- here: a single-char `mode`, a function RHS carried on `opts.callback` (the API's
-- escape hatch) rather than as `rhs`, and the `:map` default of *remappable*
-- (design D5) — so `noremap` is on only when the caller passes
-- `opts.noremap = true`. `raw_buf_set` takes a leading `buffer` (0 = current);
-- `raw_set` is the global form (buffer `nil`).

-- nx.keymap.raw_buf_set(buffer, mode, lhs, rhs, opts) [alias nvim_buf_set_keymap].
function nx.keymap.raw_buf_set(buffer, mode, lhs, rhs, opts)
  opts = opts or {}
  nx.keymap.set(mode, lhs, opts.callback or rhs, {
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

-- nx.keymap.raw_set(mode, lhs, rhs, opts) [alias nvim_set_keymap]: the global form
-- of the raw setter (a buffer-less raw_buf_set).
function nx.keymap.raw_set(mode, lhs, rhs, opts)
  nx.keymap.raw_buf_set(nil, mode, lhs, rhs, opts)
end

-- nx.keymap.buf_del(buffer, mode, lhs) [alias nvim_buf_del_keymap]: remove a
-- buffer-local mapping. The global form, nvim_del_keymap, aliases nx.keymap.del
-- directly — its (mode, lhs) call already matches nx.keymap.del's signature.
function nx.keymap.buf_del(buffer, mode, lhs)
  nx.keymap.del(mode, lhs, { buffer = buffer })
end

-- Invoke the function RHS for entry `id` (called from Rust when a Lua-backed
-- mapping fires). A no-op if no function is registered under that id. A throwing
-- RHS propagates for the server to surface.
function nx._run_keymap(id)
  local fn = nx._keymap_fns[id]
  if fn then
    fn()
  end
end

-- Textlock for <expr> mappings. An <expr> RHS must *compute* the keys to feed and
-- not change editor state (vim's textlock); while this is set the mutation funnels
-- (currently vim.cmd) refuse. A simple, honest sandbox: the common offender raises
-- rather than silently no-ops, and the server additionally discards any effects an
-- <expr> RHS queued, so nothing it did leaks regardless.
nx._expr_lock = false

-- Run the <expr> function RHS for entry `id` and return the keys it produced (its
-- return value coerced to a string; nil/false → ""). Runs under nx._expr_lock so
-- vim.cmd refuses; pcall guarantees the lock is cleared even if the RHS throws,
-- after which the error is re-raised for Rust to surface (the mapping then feeds
-- nothing). A no-op id yields "".
function nx._run_keymap_expr(id)
  local fn = nx._keymap_fns[id]
  if not fn then
    return ""
  end
  nx._expr_lock = true
  local ok, result = pcall(fn)
  nx._expr_lock = false
  if not ok then
    error(result, 0)
  end
  if result == nil or result == false then
    return ""
  end
  return tostring(result)
end

-- ----- reading mappings back: nvim_get_keymap / maparg ----------------------
-- The introspection side of the registry: where the matcher *consumes*
-- nx._keymaps (compiled to tries server-side), these read it back as the maparg
-- dict shape neovim hands plugins. A plugin that builds an overlay of your
-- mappings walks these to discover what is bound.

-- Whether a registry entry's declared modes cover the single mode code `want`.
-- `""` (vim's :map) covers everything; `x`/`v` are treated as interchangeable
-- (nxvim has no separate Select mode, and visual maps may be declared either way).
local function keymap_mode_match(modes, want)
  for _, m in ipairs(modes) do
    if m == want or m == "" then
      return true
    end
    if (want == "x" or want == "v") and (m == "x" or m == "v") then
      return true
    end
  end
  return false
end

-- Render one registry entry as neovim's maparg(..., {dict}) table for mode `mode`.
-- A function RHS reports `rhs = ""` with the function on `callback` (matching
-- neovim, which carries a Lua RHS out-of-band); a string RHS reports the keys.
local function keymap_dict(e, mode)
  local d = {
    lhs = e.lhs,
    lhsraw = e.lhs,
    mode = mode,
    noremap = e.noremap and 1 or 0,
    script = 0,
    expr = e.expr and 1 or 0,
    silent = e.silent and 1 or 0,
    nowait = e.nowait and 1 or 0,
    buffer = e.buffer or 0,
    sid = 0,
    lnum = 0,
    desc = e.desc,
  }
  if e.rhs.kind == "lua" then
    d.rhs = ""
    d.callback = nx._keymap_fns[e.id]
  else
    d.rhs = e.rhs.str
  end
  return d
end

-- nx.keymap.get(mode) [alias nvim_get_keymap]: every GLOBAL mapping that applies
-- in `mode`, as maparg dicts. Buffer-local maps are excluded (nx.keymap.buf_get
-- returns those).
function nx.keymap.get(mode)
  local out = {}
  for _, e in ipairs(nx._keymaps) do
    if (e.buffer == nil or e.buffer == 0) and keymap_mode_match(e.modes, mode) then
      out[#out + 1] = keymap_dict(e, mode)
    end
  end
  return out
end

-- nx.keymap.buf_get(buffer, mode) [alias nvim_buf_get_keymap]: the BUFFER-LOCAL
-- mappings of `buffer` (0 = current) that apply in `mode`, as maparg dicts.
function nx.keymap.buf_get(buffer, mode)
  buffer = nx._resolve_bufnr(buffer)
  local out = {}
  for _, e in ipairs(nx._keymaps) do
    if e.buffer == buffer and keymap_mode_match(e.modes, mode) then
      out[#out + 1] = keymap_dict(e, mode)
    end
  end
  return out
end

-- nx.keymap.arg(name, mode, abbr, dict) [alias vim.fn.maparg]: the mapping bound to
-- `name` in `mode`. With `dict` truthy returns the full maparg dict (or `{}` when
-- unmapped); else the rhs string ("" when unmapped, or for a function RHS). `abbr`
-- (abbreviation lookup) is accepted and ignored — nxvim has no abbreviations. A
-- buffer-local map for the current buffer shadows a global one at the same lhs.
function nx.keymap.arg(name, mode, _abbr, dict)
  if mode == nil or mode == "" then
    mode = nx._cur_mode or "n"
  end
  local cur = nx._resolve_bufnr(0)
  local best
  for _, e in ipairs(nx._keymaps) do
    if e.lhs == name and keymap_mode_match(e.modes, mode) then
      if e.buffer == cur then
        best = e -- buffer-local for the current buffer always wins
      elseif (e.buffer == nil or e.buffer == 0) and (best == nil or best.buffer ~= cur) then
        best = e -- a global match (kept unless a buffer-local one is found)
      end
    end
  end
  if not best then
    return dict and {} or ""
  end
  if dict then
    return keymap_dict(best, mode)
  end
  return best.rhs.kind == "lua" and "" or best.rhs.str
end
vim.fn.maparg = nx.keymap.arg

-- The `:ls` panel's <CR> handler: jump to the buffer whose number leads the
-- selected listing line (`"  2 %a "name" line 1"`), then dismiss the list. The
-- core installs this via `vim.panel.on_select` when `:ls` opens its panel, so
-- the buffer list rides the same scripting select path a plugin would use.
function nx._panel_select_buffer(line)
  local n = tonumber(line:match("^%s*(%d+)"))
  if n then
    vim.panel.close()
    vim.cmd("buffer " .. n)
  end
end

-- ----- rebindable panel keys ------------------------------------------------
-- The bottom message / quickfix / location panel is driven through the keymap
-- engine, NOT a hardcoded grab: the server selects the `panel` bucket while the
-- panel owns input, so navigation / confirm / close are configurable with
-- `nx.keymap.set('panel', '<key>', nx.panel.actions.<name>)`. Each action fires
-- through the engine (nx._panel_action -> Editor::apply_panel_action). A panel has
-- NO query, so there is no text fallthrough — an unmapped key is inert.
nx.panel = nx.panel or {}
nx.panel.actions = nx.panel.actions or {}
for _, name in ipairs({
  "next",
  "prev",
  "first",
  "last",
  "half_down",
  "half_up",
  "confirm",
  "close",
}) do
  nx.panel.actions[name] = function()
    nx._panel_action(name)
  end
end

-- The default panel bindings — `default = true` so a user `nx.keymap.set('panel',
-- …)` wins, and an empty-function map disables a key. `gg` is a two-key default map.
-- These mirror the vim-style keys the panel used to hardcode.
for _, m in ipairs({
  { "j", "next", "Next line" },
  { "<Down>", "next", "Next line" },
  { "k", "prev", "Previous line" },
  { "<Up>", "prev", "Previous line" },
  { "gg", "first", "First line" },
  { "<Home>", "first", "First line" },
  { "G", "last", "Last line" },
  { "<End>", "last", "Last line" },
  { "<C-d>", "half_down", "Half-page down" },
  { "<C-u>", "half_up", "Half-page up" },
  { "<CR>", "confirm", "Select / jump" },
  { "q", "close", "Close" },
  { "Q", "close", "Close" },
  { "<Esc>", "close", "Close" },
}) do
  nx.keymap.set("panel", m[1], nx.panel.actions[m[2]], { default = true, desc = m[3] })
end

-- ----- rebindable explorer keys ---------------------------------------------
-- The file-explorer (directory-listing) buffer is driven through the keymap
-- engine, NOT a hardcoded grab: the server selects the `explorer` bucket while a
-- listing is focused in normal mode, so navigation / open / up are configurable
-- with `nx.keymap.set('explorer', '<key>', nx.explorer.actions.<name>)`. Each
-- action fires through the engine (nx._explorer_action ->
-- Editor::apply_explorer_action). The listing's ONE residual non-map key is
-- `:`/`/`/`?` (it falls through to the command line); every other unmapped key is
-- inert, so the listing can't be corrupted.
nx.explorer = nx.explorer or {}
nx.explorer.actions = nx.explorer.actions or {}
for _, name in ipairs({
  "open",
  "up",
  "next",
  "prev",
  "first",
  "last",
  "half_down",
  "half_up",
  "page_down",
  "page_up",
}) do
  nx.explorer.actions[name] = function()
    nx._explorer_action(name)
  end
end

-- The default explorer bindings — `default = true`, so a user override wins and an
-- empty-function map disables a key. `gg` is a two-key default map. These mirror
-- the keys the explorer used to hardcode.
for _, m in ipairs({
  { "<CR>", "open", "Open entry" },
  { "-", "up", "Parent directory" },
  { "j", "next", "Next entry" },
  { "<Down>", "next", "Next entry" },
  { "k", "prev", "Previous entry" },
  { "<Up>", "prev", "Previous entry" },
  { "gg", "first", "First entry" },
  { "<Home>", "first", "First entry" },
  { "G", "last", "Last entry" },
  { "<End>", "last", "Last entry" },
  { "<C-d>", "half_down", "Half-page down" },
  { "<C-u>", "half_up", "Half-page up" },
  { "<C-f>", "page_down", "Page down" },
  { "<PageDown>", "page_down", "Page down" },
  { "<C-b>", "page_up", "Page up" },
  { "<PageUp>", "page_up", "Page up" },
}) do
  nx.keymap.set("explorer", m[1], nx.explorer.actions[m[2]], { default = true, desc = m[3] })
end

-- ----- rebindable command-line keys -----------------------------------------
-- The command line (`:` ex, `/`,`?` search, `vim.ui.input` prompt) is driven
-- through the keymap engine, NOT a hardcoded grab. It reuses the command-mode `c`
-- bucket (`mode = "cmdline"` is the readable alias), so every control key is
-- configurable with `nx.keymap.set("cmdline", "<key>", nx.cmdline.actions.<name>)`
-- (or the muscle-memory `nx.keymap.set("c", …)`). Each action fires through the
-- engine (nx._cmdline_action -> Editor::apply_cmdline_action). Typed TEXT is the
-- residual fallthrough — an unmapped printable inserts into the line — so the hot
-- path stays core-direct; only the control keys round-trip Lua. The `<C-r>{reg}`
-- register name and a `vim.fn.confirm` answer are fixed grammars read raw, NOT
-- keymaps (so they are not listed here).
nx.cmdline = nx.cmdline or {}
nx.cmdline.actions = nx.cmdline.actions or {}
for _, name in ipairs({
  "cancel",
  "submit",
  "backspace",
  "delete",
  "left",
  "right",
  "to_start",
  "to_end",
  "history_prev",
  "history_next",
  "insert_register",
}) do
  nx.cmdline.actions[name] = function()
    nx._cmdline_action(name)
  end
end

-- The default command-line bindings — `default = true`, so a user override wins
-- and an empty-function map disables a key. These mirror the keys handle_command
-- used to hardcode.
for _, m in ipairs({
  { "<Esc>", "cancel", "Cancel" },
  { "<CR>", "submit", "Run the command line" },
  { "<BS>", "backspace", "Delete char before cursor" },
  { "<Del>", "delete", "Delete char under cursor" },
  { "<Left>", "left", "Cursor left" },
  { "<Right>", "right", "Cursor right" },
  { "<Home>", "to_start", "Cursor to start" },
  { "<End>", "to_end", "Cursor to end" },
  { "<C-b>", "to_start", "Cursor to start" },
  { "<C-e>", "to_end", "Cursor to end" },
  { "<Up>", "history_prev", "Older history entry" },
  { "<C-p>", "history_prev", "Older history entry" },
  { "<Down>", "history_next", "Newer history entry" },
  { "<C-n>", "history_next", "Newer history entry" },
  { "<C-r>", "insert_register", "Insert register" },
}) do
  nx.keymap.set("cmdline", m[1], nx.cmdline.actions[m[2]], { default = true, desc = m[3] })
end

-- ----- the pending-key event (which-key / showcmd) -------------------------
-- nx.on_key_pending(fn): subscribe to the engine-computed pending-key signal. The
-- server fires it whenever the *pending key-context changes* — a mapped prefix
-- grows (you type <leader>, then w) or clears (the sequence completed, broke, or
-- the idle flush resolved it) — NOT per keystroke (ADR 0002 rule 4: no per-key
-- Lua). The handler receives
--   ctx = { mode, keys, continuations = { { key, desc, kind = "map"|"group" }, … } }
-- where `keys` is the withheld prefix in vim notation ("" when the context cleared,
-- which a which-key popup treats as "close") and each continuation is one key that
-- extends the prefix — `kind = "map"` completes a mapping (carrying its `desc`),
-- `kind = "group"` only leads on to longer mappings.
--
-- `ctx.label` (a string, or nil) is the **source B** channel: the built-in command
-- grammar's open pending states — `f`/`t`/`F`/`T` find-char, `r` replace, `i`/`a`
-- text-object, marks, registers, `<C-w>` — have no finite key list to enumerate, so
-- they arrive with `continuations = {}` and a human label instead (`"Find
-- character"`). A which-key renders the label as a hint card when `continuations` is
-- empty; sources A/C (mapped prefixes) leave `label` nil and list keys.
--
-- This is the render-time oracle a native which-key plugin debounces
-- (nx.utils.debounce) and draws as a persistent nx.ui.float. Continuations come from
-- the mapped-prefix trie (user + native-default maps, sources A/C); the built-in
-- grammar is surfaced via `label` (source B).
nx._on_key_pending = nx._on_key_pending or {}

function nx.on_key_pending(fn)
  if type(fn) ~= "function" then
    error("nx.on_key_pending: expects a function", 2)
  end
  nx._on_key_pending[#nx._on_key_pending + 1] = fn
  -- Tell the server a listener exists so it starts computing + pushing the signal;
  -- it skips the whole path while none are registered.
  nx._key_pending_register()
end

-- nx._key_pending_dispatch(ctx): fan one pending-key event out to every registered
-- handler (called from Rust on each pending-context change). A throwing handler is
-- isolated (surfaced via nx.notify) so one bad listener can't starve the others.
function nx._key_pending_dispatch(ctx)
  for _, fn in ipairs(nx._on_key_pending) do
    local ok, err = pcall(fn, ctx)
    if not ok then
      nx.notify("nx.on_key_pending handler error: " .. tostring(err), "error")
    end
  end
end

-- ----- vim.api.nvim_*_keymap compatibility aliases -------------------------
-- The muscle-memory `vim.api.nvim_*` names for the keymap natives above, each
-- forwarding to the canonical `nx.keymap.*` (same function object, same
-- signature). nvim_del_keymap aliases the ergonomic nx.keymap.del directly — its
-- (mode, lhs) call already matches that signature with `opts` left nil.
vim.api.nvim_set_keymap = nx.keymap.raw_set
vim.api.nvim_buf_set_keymap = nx.keymap.raw_buf_set
vim.api.nvim_del_keymap = nx.keymap.del
vim.api.nvim_buf_del_keymap = nx.keymap.buf_del
vim.api.nvim_get_keymap = nx.keymap.get
vim.api.nvim_buf_get_keymap = nx.keymap.buf_get
