-- nx.view — plugin-owned, read-only content surfaces.
--
-- A view is the dockable, mountable generalization of the bottom panel: an
-- ordinary editor buffer whose lines a plugin replaces wholesale, whose `<CR>`
-- dispatches to an `on_select` callback, and which the editing grammar treats as
-- inert (navigation works, text-mutating keys don't). It is the content surface a
-- pure-Lua file tree / symbol list / any line-oriented widget mounts in a dock or a
-- split.
--
-- `nx.view.create{...}` returns a handle whose methods queue the native ops (the
-- `nx.view._*` Rust bridges) and whose Lua-side state — the per-line `userdata` and
-- the `on_select` callback — lives in the handle. The backing buffer number and the
-- view's cursor line arrive each tick via the `nx._view_buf` / `nx._view_line`
-- mirror, so `:set_decor` / `:bufnr` / `:line` read live editor state with no
-- round-trip. Navigation is plain normal-mode motion on the nomodifiable view
-- buffer; the one special key, `<CR>` → `confirm`, is a buffer-local default map
-- installed at create (`nx._install_view_keymaps`) and lands here through
-- `nx._view_select`.

nx.view = nx.view or {}
nx._views = nx._views or {} -- id -> handle
nx._view_next_id = nx._view_next_id or 0

-- The view's one activation action: `<CR>` → confirm → the handle's `on_select`. It
-- fires the native bridge (nx._view_action -> Editor::apply_view_action). Navigation
-- is plain normal-mode motion on the nomodifiable view buffer, so this is the only
-- view action.
nx.view.actions = nx.view.actions or {}
nx.view.actions.confirm = function()
  nx._view_action("confirm")
end

-- nx._install_view_keymaps(buf) — install the view's buffer-local default activation
-- map. Called by the server right after the view's backing buffer is created (the
-- bufnr is known synchronously in core, ahead of the next-tick `nx._view_buf`
-- mirror), so the `<CR>` → `on_select` map exists immediately. `default = true` lets a
-- plugin override `<CR>` with its own `{ buffer = buf }` map. A view is an ordinary
-- `nomodifiable` buffer otherwise, so this is its only special key. (The explorer /
-- quickfix install their maps off a `FileType` autocmd instead — see
-- prelude/keymap.lua — but a view's filetype is content-semantic and it may never be
-- the current buffer when `FileType` would fire, so it installs at create time.)
function nx._install_view_keymaps(buf)
  nx.keymap.set(
    "n",
    "<CR>",
    nx.view.actions.confirm,
    { buffer = buf, default = true, desc = "Select entry" }
  )
end

local View = {}
View.__index = View

-- nx.view.create{ name?, filetype? } -> handle. Mints the backing read-only buffer
-- (off-screen until mounted) and returns the handle. `filetype` drives treesitter /
-- decoration on the view buffer.
function nx.view.create(opts)
  opts = opts or {}
  nx._view_next_id = nx._view_next_id + 1
  local id = nx._view_next_id
  local self = setmetatable({
    id = id,
    name = opts.name or "",
    filetype = opts.filetype or "",
    _userdata = {},
    _on_select = nil,
  }, View)
  nx._views[id] = self
  nx.view._create(id, self.name, self.filetype)
  return self
end

-- :set_lines(lines) — replace the view's content wholesale.
function View:set_lines(lines)
  nx.view._set_lines(self.id, lines or {})
  return self
end

-- :set_userdata(list) — opaque per-line data, parallel to the lines (1-based). The
-- entry for the selected line is handed to `on_select`. Pure Lua state.
function View:set_userdata(list)
  self._userdata = list or {}
  return self
end

-- :on_select(fn) — `fn(line, userdata)` fires on `<CR>` / confirm, with the 1-based
-- cursor line and that line's userdata entry. Pure Lua state.
function View:on_select(fn)
  self._on_select = fn
  return self
end

-- :bufnr() — the backing buffer number (from the mirror), or nil before the view's
-- buffer exists (i.e. before the create op has drained). The target for extmarks.
function View:bufnr()
  return nx._view_buf and nx._view_buf[self.id]
end

-- :set_decor(ns, marks) — replace namespace `ns`'s decoration on the view buffer
-- with `marks`. Each mark is `{ line, col, <extmark opts> }` (0-based `line`/`col`,
-- then any `nvim_buf_set_extmark` opt: `hl_group`, `end_col`, `virt_text`,
-- `sign_text`, `priority`, …). A no-op (warned) before the buffer exists.
function View:set_decor(ns, marks)
  local buf = self:bufnr()
  if not buf then
    return nx.notify("nx.view:set_decor: the view buffer does not exist yet", 3)
  end
  nx.buf.clear_namespace(buf, ns, 0, -1)
  for _, m in ipairs(marks or {}) do
    local o = {}
    for k, v in pairs(m) do
      if k ~= "line" and k ~= "col" then
        o[k] = v
      end
    end
    nx.buf.set_extmark(buf, ns, m.line, m.col, o)
  end
  return self
end

-- :mount(opts) — show the view. `opts.dock = "left"|"right"|"top"|"bottom"` mounts
-- it in that dock (`opts.size` columns/rows); `opts.split = "vsplit"|"split"` mounts
-- it in a split of the main editor area. Mounting focuses the view.
function View:mount(opts)
  opts = opts or {}
  if opts.dock then
    nx.view._mount_dock(self.id, opts.dock, opts.size)
  elseif opts.split then
    nx.view._mount_split(self.id, opts.split ~= "split")
  elseif opts.float then
    nx.notify("nx.view:mount{ float } is not implemented yet (use dock/split)", 4)
  else
    nx.notify("nx.view:mount: pass one of { dock = … } / { split = … }", 4)
  end
  return self
end

-- :unmount() — remove the view from view, keeping it (and its content) alive for a
-- later :mount.
function View:unmount()
  nx.view._unmount(self.id)
  return self
end

-- :focus() — move focus to the window showing the view.
function View:focus()
  nx.view._focus(self.id)
  return self
end

-- :line() — the view's 1-based cursor line (from the mirror), valid while the view
-- is focused. nil before the buffer exists.
function View:line()
  return nx._view_line and nx._view_line[self.id]
end

-- :cursor() — the view's cursor as `(line, col)`. `col` is always 0 in v1 (a view's
-- cursor rests at column 0); the line is `:line()`.
function View:cursor()
  return self:line(), 0
end

-- :set_cursor(line) — focus the view and move its cursor to 1-based `line` (clamped
-- to the content; column 0). The reveal / find-file primitive — the one sanctioned
-- cursor write; ordinary navigation is plain normal-mode motion.
function View:set_cursor(line)
  nx.view._set_cursor(self.id, line)
  return self
end

-- :redraw() — request a repaint. The editor already repaints at the end of every
-- input batch / drained chunk, so this is a no-op kept for API completeness (and so
-- a plugin can express intent at the call site).
function View:redraw()
  return self
end

-- :close() — unmount the view and drop its backing buffer and registry entry.
function View:close()
  nx.view._destroy(self.id)
  nx._views[self.id] = nil
end

-- nx._view_select(id, line) — dispatch a `<CR>`/confirm on view `id` to its handle's
-- `on_select(line, userdata[line])`. Called from the server after the core records
-- the select. A no-op when the view has no handler.
function nx._view_select(id, line)
  local v = nx._views[id]
  if not v or not v._on_select then
    return
  end
  local ud = v._userdata and v._userdata[line]
  v._on_select(line, ud)
end
