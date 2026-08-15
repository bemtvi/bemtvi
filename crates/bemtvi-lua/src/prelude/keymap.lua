-- bemtvi Lua prelude — keymaps.
-- The canonical `btv.keymap`.* natives over the `btv._keymaps` snapshot the server
-- compiles into per-mode tries: the ergonomic `btv.keymap.set` / `btv.keymap.del`
-- (`vim.keymap` aliases onto them) and the lower-level `:map`-family natives
-- (`btv.keymap.raw_set` / raw_buf_set / buf_del / get / buf_get) the muscle-memory
-- nvim_*_keymap names alias onto, in one block at the end of the file (ADR 0002).
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- keymaps ---------------------------------------------------------------
-- `vim.keymap.set` / .del store entries in a pure-Lua registry the server reads
-- back as data (unlike autocmds, whose *matching* stays in Lua); the server
-- compiles the snapshot into per-mode prefix tries and matches keystrokes there.
-- A function RHS is held in `btv._keymap_fns` keyed by the entry's stable id and
-- invoked from Rust via `btv._run_keymap`(id) — the run_user_command analogue.
-- Every mutation bumps `btv._keymaps_version` so the server rebuilds its tries
-- only when the registry actually changed (checked once per input batch).

btv._keymaps = btv._keymaps or {}
btv._keymap_fns = btv._keymap_fns or {}
btv._keymaps_version = btv._keymaps_version or 0
local keymap_seq = 0

btv.keymap = btv.keymap or {}
vim.keymap = btv.keymap

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
-- (`vim.g.mapleader` / `vim.g.maplocalleader`, each defaulting to `"\"` as in vim),
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
-- same convention `nvim_create_autocmd` uses), so a buffer-local map declared with
-- `buffer = 0` is pinned to the buffer that was current when it was set.
local function keymap_resolve_buffer(buffer)
  if buffer == 0 then
    return btv._cur_buf and btv._cur_buf.bufnr or 0
  end
  return buffer
end

-- Does a mapping already exist for `lhs` overlapping any of `modes` at the given
-- `buffer` scope? Backs `<unique>` (`opts.unique`): vim errors (E227) rather than
-- overwrite. Compares the already-leader-expanded `lhs`/resolved `buffer` the
-- caller holds, and treats any mode overlap as a clash.
local function keymap_clashes(modes, lhs, buffer)
  local want = {}
  for _, m in ipairs(modes) do
    want[m] = true
  end
  for _, e in ipairs(btv._keymaps) do
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

-- Register one keymap entry into `btv._keymaps` — the core of `btv.keymap.set` (which
-- the lower-level `nvim_set_keymap` / `nvim_buf_set_keymap` shim onto). `modes` is a
-- list of mode codes; `rhs` a function (stored in `btv._keymap_fns`) or a string (fed
-- as keys). `opts` is a normalized table the caller fills in: `noremap` (`btv.keymap`
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
    btv._keymap_fns[id] = rhs
    rhs_data = { kind = "lua", id = id }
  else
    -- <leader> is expanded in the string RHS too, not just the LHS, matching
    -- neovim — so a remap RHS can name another <leader> mapping.
    rhs_data = { kind = "str", str = keymap_expand_leader(tostring(rhs)) }
  end
  btv._keymaps[#btv._keymaps + 1] = {
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
  btv._keymaps_version = btv._keymaps_version + 1
end

-- Drop every buffer-local mapping bound to `bufnr` (and free any function RHS it
-- held). Called when a buffer is deleted so its maps don't outlive it and leak
-- onto a later buffer that reuses the bufnr. Bumps the version so the server
-- rebuilds its tries without the dropped entries.
function btv._purge_buf_keymaps(bufnr)
  local kept, dropped = {}, false
  for _, e in ipairs(btv._keymaps) do
    if e.buffer == bufnr then
      dropped = true
      if e.rhs.kind == "lua" then
        btv._keymap_fns[e.id] = nil
      end
    else
      kept[#kept + 1] = e
    end
  end
  if dropped then
    btv._keymaps = kept
    btv._keymaps_version = btv._keymaps_version + 1
  end
end

-- Remove the mappings for `lhs` in `modes` at the given `buffer` scope (nil for
-- global, a resolved number for buffer-local) — the core of `btv.keymap.del` (which
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
  for _, e in ipairs(btv._keymaps) do
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
        btv._keymap_fns[e.id] = nil
      end
    else
      kept[#kept + 1] = e
    end
  end
  btv._keymaps = kept
  btv._keymaps_version = btv._keymaps_version + 1
end

-- `btv.keymap.set`(mode, lhs, rhs, opts): map `lhs` to `rhs` in `mode`.
-- `rhs` is a function (stored in `btv._keymap_fns`) or a string (fed as keys).
-- Maps are non-recursive by default (the `btv.keymap.set` convention); pass
-- `opts.remap = true` for a recursive map whose RHS keys are re-fed through the
-- mapping layer (or, equivalently, `opts.noremap = false`). `opts.desc` is stored
-- but unused; `opts.buffer` ties the map to one buffer (0 = current), `opts.default`
-- marks an overridable built-in — both feed the precedence ladder the server applies.
function btv.keymap.set(mode, lhs, rhs, opts)
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

-- `btv.keymap.expand_leader`(lhs): the `lhs` with `<leader>` / `<localleader>` replaced
-- by the CURRENT `vim.g.mapleader` / `vim.g.maplocalleader` (each `"\"` when unset),
-- matching the set-time expansion `btv.keymap.set` performs on every LHS it registers.
--
-- Call this when you need the concrete key sequence a map is registered under — most
-- often to feed it back through `btv._feedkeys`, which parses raw vim key-notation and
-- does NOT know `<leader>`. Expand once, at the moment you set the map, and reuse that
-- string: the leader in force then is the one baked into the registration.
function btv.keymap.expand_leader(lhs)
  return keymap_expand_leader(lhs)
end

-- `btv.keymap.del`(mode, lhs, opts): remove the mapping(s) for `lhs` in `mode`.
-- `opts.buffer` (0 = current) targets a buffer-local map; absent targets globals.
function btv.keymap.del(mode, lhs, opts)
  opts = opts or {}
  keymap_remove(keymap_modes(mode), lhs, keymap_resolve_buffer(opts.buffer))
end

-- The lower-level `:map`-family API as canonical `btv.keymap.*` natives (the
-- nvim_*_keymap names alias onto them at the end of the file). These are a thin
-- convention-shim over `btv.keymap.set` / `btv.keymap.del` above — there is no
-- second registration path. Only the calling convention differs and is normalized
-- here: a single-char `mode`, a function RHS carried on `opts.callback` (the API's
-- escape hatch) rather than as `rhs`, and the `:map` default of *remappable*
-- (design D5) — so `noremap` is on only when the caller passes
-- `opts.noremap = true`. `raw_buf_set` takes a leading `buffer` (0 = current);
-- `raw_set` is the global form (buffer `nil`).

-- `btv.keymap.raw_buf_set`(buffer, mode, lhs, rhs, opts) [alias `nvim_buf_set_keymap`].
function btv.keymap.raw_buf_set(buffer, mode, lhs, rhs, opts)
  opts = opts or {}
  btv.keymap.set(mode, lhs, opts.callback or rhs, {
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

-- `btv.keymap.raw_set`(mode, lhs, rhs, opts) [alias `nvim_set_keymap`]: the global form
-- of the raw setter (a buffer-less raw_buf_set).
function btv.keymap.raw_set(mode, lhs, rhs, opts)
  btv.keymap.raw_buf_set(nil, mode, lhs, rhs, opts)
end

-- `btv.keymap.buf_del`(buffer, mode, lhs) [alias `nvim_buf_del_keymap`]: remove a
-- buffer-local mapping. The global form, `nvim_del_keymap`, aliases `btv.keymap.del`
-- directly — its (mode, lhs) call already matches `btv.keymap.del`'s signature.
function btv.keymap.buf_del(buffer, mode, lhs)
  btv.keymap.del(mode, lhs, { buffer = buffer })
end

-- Invoke the function RHS for entry `id` (called from Rust when a Lua-backed
-- mapping fires). A no-op if no function is registered under that id. A throwing
-- RHS propagates for the server to surface.
function btv._run_keymap(id)
  local fn = btv._keymap_fns[id]
  if fn then
    fn()
  end
end

-- Textlock for <expr> mappings. An <expr> RHS must *compute* the keys to feed and
-- not change editor state (vim's textlock); while this is set the mutation funnels
-- (currently `vim.cmd`) refuse. A simple, honest sandbox: the common offender raises
-- rather than silently no-ops, and the server additionally discards any effects an
-- <expr> RHS queued, so nothing it did leaks regardless.
btv._expr_lock = false

-- Run the <expr> function RHS for entry `id` and return the keys it produced (its
-- return value coerced to a string; nil/false → `""`). Runs under `btv._expr_lock` so
-- `vim.cmd` refuses; pcall guarantees the lock is cleared even if the RHS throws,
-- after which the error is re-raised for Rust to surface (the mapping then feeds
-- nothing). A no-op id yields "".
function btv._run_keymap_expr(id)
  local fn = btv._keymap_fns[id]
  if not fn then
    return ""
  end
  btv._expr_lock = true
  local ok, result = pcall(fn)
  btv._expr_lock = false
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
-- `btv._keymaps` (compiled to tries server-side), these read it back as the maparg
-- dict shape neovim hands plugins. A plugin that builds an overlay of your
-- mappings walks these to discover what is bound.

-- Whether a registry entry's declared modes cover the single mode code `want`.
-- `""` (vim's :map) covers everything; `x`/`v` are treated as interchangeable
-- (bemtvi has no separate Select mode, and visual maps may be declared either way).
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
    d.callback = btv._keymap_fns[e.id]
  else
    d.rhs = e.rhs.str
  end
  return d
end

-- `btv.keymap.get`(mode) [alias `nvim_get_keymap`]: every GLOBAL mapping that applies
-- in `mode`, as maparg dicts. Buffer-local maps are excluded (`btv.keymap.buf_get`
-- returns those).
function btv.keymap.get(mode)
  local out = {}
  for _, e in ipairs(btv._keymaps) do
    if (e.buffer == nil or e.buffer == 0) and keymap_mode_match(e.modes, mode) then
      out[#out + 1] = keymap_dict(e, mode)
    end
  end
  return out
end

-- `btv.keymap.buf_get`(buffer, mode) [alias `nvim_buf_get_keymap`]: the BUFFER-LOCAL
-- mappings of `buffer` (0 = current) that apply in `mode`, as maparg dicts.
function btv.keymap.buf_get(buffer, mode)
  buffer = btv._resolve_bufnr(buffer)
  local out = {}
  for _, e in ipairs(btv._keymaps) do
    if e.buffer == buffer and keymap_mode_match(e.modes, mode) then
      out[#out + 1] = keymap_dict(e, mode)
    end
  end
  return out
end

-- `btv.keymap.arg`(name, mode, abbr, dict) [alias `vim.fn.maparg`]: the mapping bound to
-- `name` in `mode`. With `dict` truthy returns the full maparg dict (or `{}` when
-- unmapped); else the rhs string ("" when unmapped, or for a function RHS). `abbr`
-- (abbreviation lookup) is accepted and ignored — bemtvi has no abbreviations. A
-- buffer-local map for the current buffer shadows a global one at the same lhs.
function btv.keymap.arg(name, mode, _abbr, dict)
  if mode == nil or mode == "" then
    mode = btv._cur_mode or "n"
  end
  local cur = btv._resolve_bufnr(0)
  local best
  for _, e in ipairs(btv._keymaps) do
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
vim.fn.maparg = btv.keymap.arg

-- ----- the explorer / quickfix activation maps (vim's ftplugin model) -------
-- The file-explorer listing (`filetype=btvdir`) and the quickfix / loclist display
-- (`filetype=qf`) are ordinary `nomodifiable` buffers in a window: `j`/`k`/`gg`/`G`/
-- `/`/`:`/`<C-w>` are plain normal-mode motions on them (edits refused at the
-- `modifiable()` chokepoints). Only their *activation* keys are special, and those
-- are ordinary BUFFER-LOCAL default keymaps installed by a `FileType` autocmd — vim's
-- ftplugin model — overridable the standard way with
-- `btv.keymap.set('n', lhs, rhs, { buffer = 0 })` (or per-buffer in your own FileType
-- autocmd). The explorer's actions are pure Lua (`btv.explorer._open` / `_up`, in
-- prelude/explorer.lua — the netrw-as-plugin); the quickfix one fires the `btv._qf_action`
-- bridge. (`btv.view`'s `<CR>` is installed the same way, but at create time — see
-- prelude/view.lua.) See docs/plans/2026-06-16-unify-special-buffer-kinds.md.
btv.explorer = btv.explorer or {}

btv.qf = btv.qf or {}
btv.qf.actions = btv.qf.actions or {}
-- `btv.qf.actions.jump`(): the default `<CR>` action in the quickfix window — jump to
-- the entry on the cursor's line. Bound buffer-locally as a `default` map by the
-- `FileType qf` autocmd, so a user `<CR>` map overrides it (the rebindable
-- panel-action pattern; cf. `btv.buffers.actions.open`).
btv.qf.actions.jump = function()
  btv._qf_action("jump")
end

-- `default = true` so a user/plugin map on the same key wins; `buffer = args.buf`
-- scopes each map to the just-typed special buffer only. The explorer listing buffer
-- (`btvdir`) is filled by the `BufReadCmd` handler in prelude/explorer.lua, which sets
-- this filetype — so these maps land on the listing the moment it exists.
btv.autocmd.create("FileType", {
  pattern = "btvdir",
  callback = function(args)
    local buf = args.buf
    local open = btv.explorer.actions.open
    btv.keymap.set("n", "<CR>", open, { buffer = buf, default = true, desc = "Open entry" })
    btv.keymap.set(
      "n",
      "-",
      btv.explorer.actions.up,
      { buffer = buf, default = true, desc = "Parent directory" }
    )
    -- A double-click opens the entry under the pointer (netrw), the mouse form of
    -- `<CR>`. `<2-LeftMouse>` is an ordinary mappable key (the general mouse-mapping
    -- primitive): the single click placed the cursor on the row, the double fires this.
    btv.keymap.set(
      "n",
      "<2-LeftMouse>",
      open,
      { buffer = buf, default = true, desc = "Open entry" }
    )
  end,
})

btv.autocmd.create("FileType", {
  pattern = "qf",
  callback = function(args)
    btv.keymap.set(
      "n",
      "<CR>",
      btv.qf.actions.jump,
      { buffer = args.buf, default = true, desc = "Jump to entry" }
    )
  end,
})

-- `:ls`/`:buffers` opens a `btvbuffers` panel whose rows begin with the buffer number
-- (`{:>3}` column). `<CR>` parses that number off the cursor line, dismisses the panel,
-- and switches to that buffer — the buffer-list analogue of quickfix's `<CR>`, the vim
-- ftplugin model (ordinary buffer-local map), not a bespoke panel `on_select`. The switch
-- is `btv.schedule`d so it runs *after* the panel close has restored focus to the main
-- window (closing is a queued panel op; the switch must land in that window, not the
-- about-to-be-removed panel one).
btv.buffers = btv.buffers or {}
btv.buffers.actions = btv.buffers.actions or {}
-- `btv.buffers.actions.open`(): the default `<CR>` action inside the `:ls` / `:buffers`
-- list panel (filetype `btvbuffers`). Reads the buffer number off the start of the
-- cursor's row, closes the panel, and switches to that buffer — scheduled so the
-- switch lands in the main window after the panel close restores focus. Bound
-- buffer-locally as a `default` map (a user `<CR>` map overrides it); rebindable like
-- the other panel actions (`btv.qf.actions.jump`, `btv.panels.actions.open`). Note this
-- is the buffer-list panel's key action, distinct from the `btv.buf.*` buffer API.
btv.buffers.actions.open = function()
  local n = tostring(btv.current_line()):match("^%s*(%d+)")
  if n then
    btv.panel.close()
    btv.schedule(function()
      vim.cmd("buffer " .. n)
    end)
  end
end

btv.autocmd.create("FileType", {
  pattern = "btvbuffers",
  callback = function(args)
    btv.keymap.set(
      "n",
      "<CR>",
      btv.buffers.actions.open,
      { buffer = args.buf, default = true, desc = "Open buffer" }
    )
  end,
})

btv.panels = btv.panels or {}
btv.panels.actions = btv.panels.actions or {}
-- `:lspanels` opens a `btvpanels` panel listing the named panels (rows begin with the panel
-- buffer's number). `<CR>` parses that number and `:b`-switches to it — but the current
-- window IS the panel window, so `switch_buffer` swaps it *in place*, showing that panel's
-- last content (no close, no regenerating command). Contrast `btvbuffers`, which targets a
-- document and so closes the panel and lands the switch in the main window.
btv.panels.actions.open = function()
  local n = tostring(btv.current_line()):match("^%s*(%d+)")
  if n then
    vim.cmd("buffer " .. n)
  end
end

btv.autocmd.create("FileType", {
  pattern = "btvpanels",
  callback = function(args)
    btv.keymap.set(
      "n",
      "<CR>",
      btv.panels.actions.open,
      { buffer = args.buf, default = true, desc = "Open panel" }
    )
  end,
})

-- `:messages` opens a `btvmessages` panel holding the recorded history. `C` clears the
-- log — `:messages clear`, which re-renders the (now empty) panel in place, so the key
-- reads as "wipe this". Buffer-local and `default`, like every other panel action, so a
-- user `C` map wins and the key never bleeds onto ordinary buffers.
btv.messages = btv.messages or {}
btv.messages.actions = btv.messages.actions or {}
-- `btv.messages.actions.clear`(): the default `C` action inside the `:messages` panel
-- (filetype `btvmessages`). Clears the recorded message history and re-renders the panel
-- in place. Rebindable like the other panel actions (`btv.qf.actions.jump`,
-- `btv.buffers.actions.open`).
btv.messages.actions.clear = function()
  vim.cmd("messages clear")
end

btv.autocmd.create("FileType", {
  pattern = "btvmessages",
  callback = function(args)
    btv.keymap.set(
      "n",
      "C",
      btv.messages.actions.clear,
      { buffer = args.buf, default = true, desc = "Clear the message log" }
    )
  end,
})

-- Every panel listing (the built-in text listings `btvlisting`, the buffer list
-- `btvbuffers`, the panel list `btvpanels`, the message log `btvmessages`, and scripted
-- `btv.panel.open` panels defaulting to `btvpanel`) dismisses on `q` / `<Esc>` — the focus-locked overlay's "you're done here, go
-- back" key. An ordinary buffer-local default map, so motions / `<CR>` / a plugin's own
-- keys coexist and a user map wins. A scripted panel with its *own* filetype opts out and
-- wires its own dismiss.
btv.autocmd.create("FileType", {
  pattern = { "btvlisting", "btvbuffers", "btvpanels", "btvmessages", "btvpanel" },
  callback = function(args)
    for _, key in ipairs({ "q", "<Esc>" }) do
      btv.keymap.set(
        "n",
        key,
        btv.panel.close,
        { buffer = args.buf, default = true, desc = "Close panel" }
      )
    end
  end,
})

-- ----- rebindable command-line keys -----------------------------------------
-- The command line (`:` ex, `/`,`?` search, `vim.ui.input` prompt) is driven
-- through the keymap engine, NOT a hardcoded grab. It reuses the command-mode `c`
-- bucket (`mode = "cmdline"` is the readable alias), so every control key is
-- configurable with `btv.keymap.set("cmdline", "<key>", btv.cmdline.actions.<name>)`
-- (or the muscle-memory `btv.keymap.set("c", …)`). Each action fires through the
-- engine (`btv._cmdline_action` -> `Editor::apply_cmdline_action`). Typed TEXT is the
-- residual fallthrough — an unmapped printable inserts into the line — so the hot
-- path stays core-direct; only the control keys round-trip Lua. The `<C-r>{reg}`
-- register name and a `vim.fn.confirm` answer are fixed grammars read raw, NOT
-- keymaps (so they are not listed here).
btv.cmdline = btv.cmdline or {}
btv.cmdline.actions = btv.cmdline.actions or {}
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
  -- `<Tab>` / `<S-Tab>`: open the command-line completion popup (`btv.cmdline_complete`,
  -- the float-list widget's fifth orchestration), then cycle the selection forward /
  -- backward while it is open. The other navigation + accept keys overload the
  -- existing cmdline keys when the popup is open — `<CR>` (submit) accepts the
  -- highlighted row then executes, `<C-n>`/`<C-p>` (history) cycle the selection, and
  -- `<Esc>` (cancel) closes the popup before it cancels the line.
  "complete",
  "complete_prev",
}) do
  btv.cmdline.actions[name] = function()
    btv._cmdline_action(name)
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
  { "<Tab>", "complete", "Command completion" },
  { "<S-Tab>", "complete_prev", "Command completion (previous)" },
}) do
  btv.keymap.set("cmdline", m[1], btv.cmdline.actions[m[2]], { default = true, desc = m[3] })
end

-- ----- the built-in clipboard chords (Ctrl+C / Ctrl+V) ---------------------
-- The desktop copy/paste keys, on the SYSTEM clipboard (`"+`) rather than the
-- unnamed register: `<C-c>` copies the visual selection, `<C-v>` pastes — at the
-- cursor in normal mode, at the caret in insert mode, and into the line being typed
-- on the command line. Paste is `P`, not `p` — the text lands at the
-- cursor (charwise) / above the line (linewise), which is where a non-modal editor
-- puts it; vim's own mswin.vim maps the same chord to `"+gP` for that reason.
--
-- Each has a `<C-S-…>` twin. A terminal without the kitty keyboard protocol
-- collapses Ctrl+Shift+C onto the same control byte as Ctrl+C, so the twin costs
-- nothing there — but a GUI, a browser client, or a kitty-protocol terminal reports
-- it as its own chord, and without the map it would do nothing on exactly the
-- clients that can tell the two apart.
--
-- None of these shadows a vim binding bemtvi has: there is no blockwise visual mode
-- for `<C-v>` to open and no `i_CTRL-V` literal insert, and `<C-c>` reaches no
-- handler at all (`<Esc>` is how you leave visual). The picker's own `<C-v>` (open
-- in a vertical split) lives in the widget bucket and is untouched.
--
-- One chord never gets here: the GUI claims Ctrl+Shift+V (with Cmd+V and
-- Shift+Insert) client-side as a paste gesture — it reads the OS clipboard itself
-- and feeds it as a bracketed paste (`bemtvi-gui`'s `is_paste`), which is strictly
-- better in insert mode. Same outcome, so the map below is simply what every other
-- client does with the chord.
--
-- `default = true`, so a config that maps any of them wins, and an empty function
-- turns one off:  `btv.keymap.set("n", "<C-v>", function() end)`.
for _, m in ipairs({
  { "v", "<C-c>", '"+y', "Copy the selection to the system clipboard" },
  { "v", "<C-S-c>", '"+y', "Copy the selection to the system clipboard" },
  { "n", "<C-v>", '"+P', "Paste the system clipboard at the cursor" },
  { "n", "<C-S-v>", '"+P', "Paste the system clipboard at the cursor" },
  -- Insert mode inserts the register at the caret (`i_CTRL-R`) and stays in insert,
  -- so typing continues after the pasted text.
  { "i", "<C-v>", "<C-r>+", "Paste the system clipboard" },
  { "i", "<C-S-v>", "<C-r>+", "Paste the system clipboard" },
  -- The command line takes the same RHS: `<C-r>` is a cmdline default of its own
  -- (insert-register), and a `noremap` RHS still reaches the built-ins, so the fed
  -- `<C-r>` fires it and `+` is read as the register name (a raw grammar, not a map).
  { "cmdline", "<C-v>", "<C-r>+", "Paste the system clipboard" },
  { "cmdline", "<C-S-v>", "<C-r>+", "Paste the system clipboard" },
}) do
  btv.keymap.set(m[1], m[2], m[3], { default = true, desc = m[4] })
end

-- ----- the pending-key event (which-key / showcmd) -------------------------
btv._on_key_pending = btv._on_key_pending or {}
-- `btv.on_key_pending`(fn): subscribe to the engine-computed pending-key signal. The
-- server fires it whenever the *pending key-context changes* — a mapped prefix
-- grows (you type <leader>, then w) or clears (the sequence completed, broke, or
-- the idle flush resolved it) — NOT per keystroke (ADR 0002 rule 4: no per-key
-- Lua). The handler receives
--   ctx = { mode, keys, continuations = { { key, desc, kind = "map"|"group" }, … } }
-- where `keys` is the withheld prefix in vim notation ("" when the context cleared,
-- which a which-key popup treats as "close") and each continuation is one key that
-- extends the prefix — `kind = "map"` completes a mapping (carrying its `desc`),
-- `kind = "group"` only leads on to longer mappings. The finite built-in prefixes
-- (`g` -> gg/gt/.., `z` -> zt/zz/.., `<C-w>` -> window commands) enumerate
-- continuations too, merged under one prefix with any maps that share it (`g` + the
-- LSP gd/gD/gr).
--
-- `available` (boolean) is `true` for every reachable continuation. It is `false`
-- only for a continuation kept visible but no longer firable: a mapped `g`-prefix
-- (gd/gD/gr) surfaced AFTER the leader timeout committed `g` to the built-in grammar
-- -- the maps need a faster sequence to fire, so which-key shows them dimmed / cued
-- rather than dropping them mid-popup. (A future per-line float highlight will let a
-- which-key gray these properly; see docs/plans/2026-06-16-keypending-source-b.md.)
--
-- `ctx.label` (a string, or nil) is the **source B** channel: the built-in command
-- grammar's open pending states — `f`/`t`/`F`/`T` find-char, `r` replace, `i`/`a`
-- text-object, marks, registers, `<C-w>` — have no finite key list to enumerate, so
-- they arrive with `continuations = {}` and a human label instead
-- (`"Find character"`). A which-key renders the label as a hint card when
-- `continuations` is empty; sources A/C (mapped prefixes) leave `label` nil and list keys.
--
-- This is the render-time oracle a native which-key plugin debounces
-- (`btv.utils.debounce`) and draws as a persistent `btv.ui.float`. Continuations come from
-- the mapped-prefix trie (user + native-default maps, sources A/C) AND the built-in
-- command grammar's finite prefixes (source B); the open built-in states are
-- surfaced via `label` instead.
function btv.on_key_pending(fn)
  if type(fn) ~= "function" then
    error("btv.on_key_pending: expects a function", 2)
  end
  btv._on_key_pending[#btv._on_key_pending + 1] = fn
  -- Tell the server a listener exists so it starts computing + pushing the signal;
  -- it skips the whole path while none are registered.
  btv._key_pending_register()
end

-- `btv._key_pending_dispatch`(ctx): fan one pending-key event out to every registered
-- handler (called from Rust on each pending-context change). A throwing handler is
-- isolated (surfaced via `btv.notify`) so one bad listener can't starve the others.
function btv._key_pending_dispatch(ctx)
  for _, fn in ipairs(btv._on_key_pending) do
    local ok, err = pcall(fn, ctx)
    if not ok then
      btv.notify("btv.on_key_pending handler error: " .. tostring(err), "error")
    end
  end
end

-- ----- vim.api.nvim_*_keymap compatibility aliases -------------------------
-- The muscle-memory `vim.api.nvim_*` names for the keymap natives above, each
-- forwarding to the canonical `btv.keymap.*` (same function object, same
-- signature). `nvim_del_keymap` aliases the ergonomic `btv.keymap.del` directly — its
-- (mode, lhs) call already matches that signature with `opts` left nil.
vim.api.nvim_set_keymap = btv.keymap.raw_set
vim.api.nvim_buf_set_keymap = btv.keymap.raw_buf_set
vim.api.nvim_del_keymap = btv.keymap.del
vim.api.nvim_buf_del_keymap = btv.keymap.buf_del
vim.api.nvim_get_keymap = btv.keymap.get
vim.api.nvim_buf_get_keymap = btv.keymap.buf_get
