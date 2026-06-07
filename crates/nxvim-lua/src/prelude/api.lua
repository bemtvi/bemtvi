-- nxvim Lua prelude — Lua-side API registries.
-- User-command and autocmd registration kept purely in Lua (the vim._fire dispatcher the server reads back), plus the callable-and-indexable vim.cmd.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

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

-- vim._bufs / vim._cur_cursor / vim._cur_win: the Rust→Lua buffer mirror the
-- buffer-read API (Phase 6) resolves against. The server refreshes it via
-- vim._set_buf_mirror before running any Lua that can read buffer or cursor
-- state, so nvim_buf_get_lines / nvim_win_get_cursor / nvim_buf_is_loaded read
-- live data without reaching the Server. vim._bufs[bufnr] = { lines, name,
-- loaded }; nvim_buf_set_lines write-through mutates `lines` here directly so a
-- read-after-write within one chunk stays consistent (the real buffer catches up
-- when the server drains the queued BufOp).
vim._bufs = vim._bufs or {}
vim._cur_cursor = vim._cur_cursor or { row = 1, col = 0 }
vim._cur_win = vim._cur_win or 1000
-- Per-buffer option store backing vim.bo / nvim_set_option_value (Phase 6); the
-- table is created here so the earlier-defined setter can index it safely. This
-- holds *arbitrary* (Lua-only) buffer options plugins set; the wired indentation
-- options (tabstop/shiftwidth/expandtab) are read from `vim._bo_mirror` instead,
-- which the server refreshes from the core (see vim.bo in timer.lua).
vim._bo_store = vim._bo_store or {}
-- Rust→Lua mirror of the core's buffer-local option values, refreshed by the
-- server (vim._set_bo_mirror) before any Lua that can read options. Keyed by
-- bufnr → { tabstop, shiftwidth, expandtab }. Authoritative for the wired
-- options, so a read reflects the core default until set and a value set through
-- the `:set` ex path, not just one written from Lua.
vim._bo_mirror = vim._bo_mirror or {}

function vim._set_bo_mirror(entries)
  vim._bo_mirror = entries or {}
end

vim._wins = vim._wins or {}
vim._win_order = vim._win_order or { 1000 }
vim._next_win = vim._next_win or 1001
-- Tab mirror (Phase 3): `vim._tabs[id]` = per-tab record ({ id, windows,
-- current_window }), `vim._tab_order` the tabline order `nvim_list_tabpages`
-- returns, `vim._cur_tab` the active id. Seeded to the single startup tab so a
-- read before the server's first mirror push still answers.
vim._tabs = vim._tabs or { [1] = { id = 1, windows = { 1000 }, current_window = 1000 } }
vim._tab_order = vim._tab_order or { 1 }
vim._cur_tab = vim._cur_tab or 1
-- Arbitrary (Lua-only) window options plugins set via vim.wo; the wired gutter
-- options (number/relativenumber) live on the `vim._wins` mirror instead.
vim._wo_store = vim._wo_store or {}

function vim._set_buf_mirror(entries, row, col, win, wins, next_win)
  -- The server omits `lines` for a buffer whose changedtick is unchanged (the
  -- cheap cursor-moved-no-edit path); keep the prior `lines` in that case.
  for bufnr, entry in pairs(entries) do
    if entry.lines == nil then
      local prev = vim._bufs[bufnr]
      if prev then entry.lines = prev.lines end
    end
    entry.loaded = true
  end
  vim._bufs = entries
  vim._cur_cursor = { row = row or 1, col = col or 0 }
  vim._cur_win = win or 1000
  -- The window snapshot (Phase 5): `vim._wins[id]` = per-window record, and
  -- `vim._win_order` the layout order `nvim_list_wins` returns. `vim._next_win`
  -- is the id the next `nvim_open_win` will get, so it can return synchronously.
  local by_id, order = {}, {}
  for _, w in ipairs(wins or {}) do
    by_id[w.id] = w
    order[#order + 1] = w.id
  end
  vim._wins = by_id
  vim._win_order = order
  vim._next_win = next_win or vim._next_win
end

-- Receive the tab mirror (Phase 3): `tabs` is the tabline-ordered array the
-- server pushed, `cur` the active tab id. Keyed by id into `vim._tabs` with the
-- order kept in `vim._tab_order`, mirroring the window mirror's shape.
function vim._set_tab_mirror(tabs, cur)
  local by_id, order = {}, {}
  for _, t in ipairs(tabs or {}) do
    by_id[t.id] = t
    order[#order + 1] = t.id
  end
  vim._tabs = by_id
  vim._tab_order = order
  vim._cur_tab = cur or 1
end

-- Resolve a buffer handle to a concrete bufnr (0 / nil -> current buffer), the
-- one place the buffer-read API maps neovim's "0 means current" convention.
function vim._resolve_bufnr(bufnr)
  if bufnr == nil or bufnr == 0 then return (vim._cur_buf or {}).bufnr or 0 end
  return bufnr
end

-- Normalize a neovim line index against a buffer of `n` real lines, shared by
-- nvim_buf_get_lines and nvim_buf_set_lines (and mirrored on the Rust side so the
-- write-through and the real apply can't disagree): negatives count from the end
-- (`-1` == one past the last line), then clamp into [0, n]. `strict` raises on an
-- out-of-range index instead of clamping (neovim's strict_indexing).
function vim._norm_line_index(i, n, strict)
  local orig = i
  if i < 0 then i = n + i + 1 end
  if strict and (orig > n or i < 0) then
    error("Index out of bounds", 3)
  end
  if i < 0 then i = 0 elseif i > n then i = n end
  return i
end

function vim.api.nvim_create_user_command(name, command, _opts)
  vim._user_commands[name] = command
end

-- nvim_buf_create_user_command(buffer, name, command, opts): in neovim this
-- registers a *buffer-local* command; nxvim has no per-buffer command registry
-- yet, so it registers globally (the buffer scope is ignored). Enough for an
-- `on_attach` that defines a convenience command (e.g. rust_analyzer's
-- `:LspCargoReload`) to load without error.
-- INCOMPLETE: `buffer` is ignored — the command exists everywhere, not only in
-- its buffer. A per-buffer command registry (the analogue of the buffer-local
-- keymap scoping `vim._keymaps` already does) is the fix.
function vim.api.nvim_buf_create_user_command(_buffer, name, command, _opts)
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
-- case `file` falls back to `pattern` (the old behavior). `data` is the optional
-- `args.data` payload (LspAttach/LspDetach carry `{ client_id = … }`); nil otherwise.
function vim._fire(event, pattern, buf, file, data)
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
          cb({ id = au.id, event = event, match = pattern, buf = buf, file = file or pattern, data = data })
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

-- A few more vim.api the configs touch. `nvim_get_current_buf` resolves against
-- the single-buffer snapshot (faithful: it returns the real current buffer). The
-- window/cursor/line-access getters (Phase 6) read the `vim._bufs` / `vim._cur_*`
-- mirror the server refreshes before running Lua, so they return live state, and
-- `nvim_buf_set_lines` write-through updates the mirror then queues the real edit.
-- (`nvim_create_augroup`/`_autocmd`/`nvim_buf_get_name`/`nvim_echo` are the real,
-- behavior-carrying ones, defined elsewhere.)
function vim.api.nvim_get_current_buf() return (vim._cur_buf or {}).bufnr or 0 end

-- Window API (Phase 5). Reads resolve against the `vim._wins` mirror the server
-- refreshes before running Lua; mutations queue a WindowOp (the `vim._win_*` /
-- `vim._open_win` Rust bridges) the server drains into the live editor after the
-- chunk, the same "Lua queues, core mutates" flow as the buffer API. `0`/`nil`
-- means the current window throughout.
local function resolve_win(win)
  if win == nil or win == 0 then return vim._cur_win or 1000 end
  return win
end

function vim.api.nvim_get_current_win() return vim._cur_win or 1000 end

function vim.api.nvim_list_wins() return vim._win_order or { vim._cur_win or 1000 } end

function vim.api.nvim_set_current_win(win)
  win = resolve_win(win)
  vim._cur_win = win -- write-through so a read-after-set in this chunk is consistent
  vim._set_current_win(win)
end

function vim.api.nvim_win_get_buf(win)
  win = resolve_win(win)
  local w = (vim._wins or {})[win]
  return w and w.buffer or vim.api.nvim_get_current_buf()
end

function vim.api.nvim_win_set_buf(win, buf)
  win = resolve_win(win)
  vim._win_set_buf(win, buf or 0)
end

function vim.api.nvim_win_get_cursor(win)
  win = resolve_win(win)
  local w = (vim._wins or {})[win]
  if w then return { w.row, w.col } end
  local c = vim._cur_cursor or { row = 1, col = 0 }
  return { c.row, c.col }
end

function vim.api.nvim_win_set_cursor(win, pos)
  win = resolve_win(win)
  local row, col = pos[1], pos[2]
  vim._win_set_cursor(win, row - 1, col) -- queue (server takes a 0-based line)
  -- Write-through the mirror so a read-after-write within this chunk agrees.
  local w = (vim._wins or {})[win]
  if w then w.row, w.col = row, col end
  if win == (vim._cur_win or 1000) then vim._cur_cursor = { row = row, col = col } end
end

function vim.api.nvim_win_get_width(win)
  win = resolve_win(win)
  local w = (vim._wins or {})[win]
  return w and w.width or 0
end

function vim.api.nvim_win_get_height(win)
  win = resolve_win(win)
  local w = (vim._wins or {})[win]
  return w and w.height or 0
end

function vim.api.nvim_win_set_width(win, width) vim._win_set_width(resolve_win(win), width) end
function vim.api.nvim_win_set_height(win, height) vim._win_set_height(resolve_win(win), height) end

function vim.api.nvim_win_close(win, force)
  win = resolve_win(win)
  vim._win_close(win, force and true or false)
  -- Write-through: drop it from the mirror so a within-chunk read agrees.
  if (vim._wins or {})[win] then
    vim._wins[win] = nil
    local order = {}
    for _, id in ipairs(vim._win_order or {}) do
      if id ~= win then order[#order + 1] = id end
    end
    vim._win_order = order
  end
end

-- ----- tab pages (Phase 3) -------------------------------------------------
-- Reads resolve from the `vim._tabs` mirror the server pushes before each Lua
-- entry; `nvim_set_current_tabpage` is the lone mutation (queue + write-through),
-- the same "Lua queues, core mutates" flow as the window API. `0`/`nil` is the
-- current tab throughout.
local function resolve_tab(tab)
  if tab == nil or tab == 0 then return vim._cur_tab or 1 end
  return tab
end

function vim.api.nvim_get_current_tabpage() return vim._cur_tab or 1 end

function vim.api.nvim_list_tabpages() return vim._tab_order or { vim._cur_tab or 1 } end

function vim.api.nvim_tabpage_is_valid(tab)
  return (vim._tabs or {})[resolve_tab(tab)] ~= nil
end

function vim.api.nvim_tabpage_get_number(tab)
  tab = resolve_tab(tab)
  for i, id in ipairs(vim._tab_order or {}) do
    if id == tab then return i end
  end
  return 0
end

function vim.api.nvim_tabpage_list_wins(tab)
  local t = (vim._tabs or {})[resolve_tab(tab)]
  return t and t.windows or {}
end

function vim.api.nvim_tabpage_get_win(tab)
  local t = (vim._tabs or {})[resolve_tab(tab)]
  return t and t.current_window or (vim._cur_win or 1000)
end

function vim.api.nvim_set_current_tabpage(tab)
  tab = resolve_tab(tab)
  vim._cur_tab = tab -- write-through so a read-after-set in this chunk is consistent
  -- The active window/cursor follow the new tab; the server re-pushes the full
  -- mirror after the op drains, so a coarse write-through of the focus is enough.
  local t = (vim._tabs or {})[tab]
  if t and t.current_window then vim._cur_win = t.current_window end
  vim._set_current_tab(tab)
end

-- The float config values nxvim can position / draw. An unsupported one fails
-- loud (the no-silent-stub rule), mirroring the RPC `parse_float_config`, rather
-- than quietly falling back. Phase 2 supports these; the rest grow as needed.
local FLOAT_RELATIVE = { editor = true, cursor = true, win = true }
local FLOAT_ANCHOR = { NW = true, NE = true, SW = true, SE = true }
local FLOAT_BORDER = {
  none = true, single = true, rounded = true, double = true, solid = true,
}

-- Flatten neovim's `title` (a string, or a list of `{text, hl}` chunks) to the
-- plain string nxvim draws on the border; the per-chunk highlight is dropped.
local function float_title(title)
  if type(title) == "string" then return title end
  if type(title) == "table" then
    local parts = {}
    for _, chunk in ipairs(title) do
      parts[#parts + 1] = type(chunk) == "table" and chunk[1] or chunk
    end
    local joined = table.concat(parts)
    if joined ~= "" then return joined end
  end
  return nil
end

-- nvim_open_win(buffer, enter, config): both forms. A non-empty `config.relative`
-- opens a **float** positioned absolutely on top of the tiled layout; otherwise it
-- is the split form (`config.vertical` / `config.split == "left"/"right"` makes a
-- vsplit). Returns the new window's id (predicted from the mirror's `_next_win`);
-- the real window is created when the queued op drains.
function vim.api.nvim_open_win(buffer, enter, config)
  config = config or {}
  local id = vim._next_win or 1001
  vim._next_win = id + 1
  local enters = enter ~= false
  local buf = (buffer == nil or buffer == 0) and vim.api.nvim_get_current_buf() or buffer
  -- The float placement to seed into the mirror so a `nvim_win_get_config` later
  -- in this chunk sees it (set in the float branch below; nil for a split).
  local float_record = nil

  if type(config.relative) == "string" and config.relative ~= "" then
    -- Float form. Validate the enumerated fields loudly before queuing.
    if not FLOAT_RELATIVE[config.relative] then
      error("nvim_open_win: 'relative' value '" .. config.relative .. "' is not supported yet", 2)
    end
    local anchor = config.anchor or "NW"
    if not FLOAT_ANCHOR[anchor] then
      error("nvim_open_win: invalid 'anchor': '" .. tostring(anchor) .. "'", 2)
    end
    local border = config.border or "none"
    if type(border) ~= "string" or not FLOAT_BORDER[border] then
      error("nvim_open_win: 'border' style '" .. tostring(border) .. "' is not supported yet", 2)
    end
    if not config.width or not config.height
        or config.width <= 0 or config.height <= 0 then
      error("nvim_open_win: 'width' and 'height' must be positive", 2)
    end
    float_record = {
      relative = config.relative,
      win = (config.win and config.win ~= 0) and config.win or nil,
      anchor = anchor,
      row = math.floor(config.row or 0),
      col = math.floor(config.col or 0),
      width = math.floor(config.width),
      height = math.floor(config.height),
      zindex = math.floor(config.zindex or 50),
      focusable = config.focusable ~= false,
      border = border,
      title = float_title(config.title),
    }
    vim._open_float({
      buf = buffer or 0,
      enter = enters,
      relative = float_record.relative,
      win = config.win or 0,
      anchor = float_record.anchor,
      row = float_record.row,
      col = float_record.col,
      width = float_record.width,
      height = float_record.height,
      zindex = float_record.zindex,
      focusable = float_record.focusable,
      border = float_record.border,
      title = float_record.title,
    })
  else
    local vertical = config.vertical == true
      or config.split == "left" or config.split == "right"
    vim._open_win(buffer or 0, vertical, enters)
  end

  -- Write-through: reflect the new window in the mirror so reads later in this
  -- chunk (nvim_list_wins, nvim_win_get_buf) see it before the op drains. Real
  -- dimensions land on the next mirror refresh.
  vim._wins = vim._wins or {}
  vim._win_order = vim._win_order or {}
  -- A split inherits the source (current) window's gutter options, as the core
  -- does; fall back to the defaults when the mirror has no current window yet.
  local src = (vim._wins or {})[vim._cur_win or 1000] or {}
  local number = src.number
  if number == nil then number = true end
  local relativenumber = src.relativenumber
  if relativenumber == nil then relativenumber = true end
  vim._wins[id] = {
    id = id, buffer = buf, row = 1, col = 0, width = 0, height = 0,
    number = number, relativenumber = relativenumber, float = float_record,
  }
  vim._win_order[#vim._win_order + 1] = id
  if enters then vim._cur_win = id end
  return id
end

-- nvim_win_get_config(win): the float placement of `win` as neovim's config map,
-- or `{ relative = "" }` for a tiled window. Reads the `vim._wins` mirror (the
-- server pushes each float's config into `w.float`; `nvim_open_win` /
-- `nvim_win_set_config` write through it so a read within the same chunk agrees).
function vim.api.nvim_win_get_config(win)
  win = resolve_win(win)
  local f = ((vim._wins or {})[win] or {}).float
  if not f then return { relative = "" } end
  -- Return a fresh table so a caller mutating the result can't corrupt the mirror.
  local cfg = {
    relative = f.relative, anchor = f.anchor, row = f.row, col = f.col,
    width = f.width, height = f.height, zindex = f.zindex,
    focusable = f.focusable, border = f.border,
  }
  if f.win then cfg.win = f.win end
  if f.title then cfg.title = f.title end
  return cfg
end

-- nvim_win_set_config(win, config): move/resize/restyle a float, or convert a
-- window between floating and tiled. `config` is a *partial* — only the keys
-- given change (the core merges over the current placement); `relative = ""`
-- re-tiles a float. Validates the enumerated fields loudly (the no-silent-stub
-- rule), queues the op, and write-throughs the mirror so a `get_config` later in
-- this chunk sees the change before the op drains.
function vim.api.nvim_win_set_config(win, config)
  win = resolve_win(win)
  config = config or {}
  local relative = config.relative
  local make_tiled = relative == ""
  if type(relative) == "string" and relative ~= "" and not FLOAT_RELATIVE[relative] then
    error("nvim_win_set_config: 'relative' value '" .. relative .. "' is not supported yet", 2)
  end
  if config.anchor ~= nil and not FLOAT_ANCHOR[config.anchor] then
    error("nvim_win_set_config: invalid 'anchor': '" .. tostring(config.anchor) .. "'", 2)
  end
  if config.border ~= nil
      and (type(config.border) ~= "string" or not FLOAT_BORDER[config.border]) then
    error("nvim_win_set_config: 'border' style '" .. tostring(config.border) .. "' is not supported yet", 2)
  end
  local floor = function(v) return v and math.floor(v) or nil end
  vim._win_set_config(win, {
    relative = type(relative) == "string" and relative or nil,
    win = config.win,
    anchor = config.anchor,
    row = floor(config.row),
    col = floor(config.col),
    width = floor(config.width),
    height = floor(config.height),
    zindex = floor(config.zindex),
    focusable = config.focusable,
    border = config.border,
    title = float_title(config.title),
  })
  -- Write-through the mirror: drop the float on a re-tile, else merge the present
  -- fields over the window's current placement (creating it on a tiled → float).
  local w = (vim._wins or {})[win]
  if w then
    if make_tiled then
      w.float = nil
    else
      local f = w.float or { relative = "editor", anchor = "NW", row = 0, col = 0,
        width = 1, height = 1, zindex = 50, focusable = true, border = "none" }
      if type(relative) == "string" then f.relative = relative end
      if config.win and config.win ~= 0 then f.win = config.win end
      if config.anchor then f.anchor = config.anchor end
      if config.row then f.row = math.floor(config.row) end
      if config.col then f.col = math.floor(config.col) end
      if config.width then f.width = math.floor(config.width) end
      if config.height then f.height = math.floor(config.height) end
      if config.zindex then f.zindex = math.floor(config.zindex) end
      if config.focusable ~= nil then f.focusable = config.focusable end
      if config.border then f.border = config.border end
      local title = float_title(config.title)
      if title then f.title = title end
      w.float = f
    end
  end
end

-- vim.wo: window-local options, indexed by window id (`vim.wo[win].number`), the
-- window analogue of vim.bo. The number-gutter options nxvim's core honors —
-- number/relativenumber and their nu/rnu abbreviations — are *wired*: a write
-- reaches the live editor (it changes that window's gutter) and a read returns
-- the core's current value from the `vim._wins` mirror the server refreshes (the
-- default until set, or a value set via the `:set` ex path). Any other option
-- falls back to the plain `vim._wo_store` (observable, not yet honored). A bare
-- `vim.wo.<opt>` (no window id) targets the current window.
local WIN_OPT_CANON = {
  number = "number", nu = "number",
  relativenumber = "relativenumber", rnu = "relativenumber",
}
local WIN_OPT_DEFAULT = { number = true, relativenumber = true }

local function wo_get(win, opt)
  local canon = WIN_OPT_CANON[opt]
  if canon then
    local w = (vim._wins or {})[win]
    if w ~= nil and w[canon] ~= nil then return w[canon] end
    return WIN_OPT_DEFAULT[canon]
  end
  local store = vim._wo_store[win]
  if store ~= nil and store[opt] ~= nil then return store[opt] end
  return nil
end
local function wo_set(win, opt, value)
  local canon = WIN_OPT_CANON[opt]
  if canon then
    -- Queue the change for the core and update the mirror so a read-after-write
    -- within this chunk is consistent (the server overwrites it on the next push).
    vim._win_set_option(win, canon, value)
    local w = (vim._wins or {})[win]
    if w then w[canon] = value end
    return
  end
  vim._wo_store[win] = vim._wo_store[win] or {}
  vim._wo_store[win][opt] = value
end
local function wo_proxy(win)
  win = resolve_win(win)
  return setmetatable({}, {
    __index = function(_, opt) return wo_get(win, opt) end,
    __newindex = function(_, opt, value) wo_set(win, opt, value) end,
  })
end
vim.wo = setmetatable({}, {
  __index = function(_, k)
    -- numeric key -> per-window proxy; option name -> current-window value.
    if type(k) == "number" then return wo_proxy(k) end
    return wo_get(resolve_win(0), k)
  end,
  __newindex = function(_, k, value) wo_set(resolve_win(0), k, value) end,
})

function vim.api.nvim_win_get_option(win, name) return vim.wo[resolve_win(win)][name] end
function vim.api.nvim_win_set_option(win, name, value)
  vim.wo[resolve_win(win)][name] = value
end

function vim.api.nvim_buf_is_loaded(bufnr)
  return vim._bufs[vim._resolve_bufnr(bufnr)] ~= nil
end

-- nvim_buf_is_valid: whether the handle names a buffer nxvim knows about. With no
-- separate "valid but unloaded" notion in the snapshot mirror yet, this matches
-- is_loaded (every mirrored buffer is loaded).
function vim.api.nvim_buf_is_valid(bufnr)
  return vim._bufs[vim._resolve_bufnr(bufnr)] ~= nil
end

-- nvim_buf_line_count: number of lines in the buffer snapshot.
function vim.api.nvim_buf_line_count(bufnr)
  local buf = vim._bufs[vim._resolve_bufnr(bufnr)]
  return (buf and buf.lines) and #buf.lines or 0
end

-- nvim_buf_get_offset: byte offset of the start of (0-based) line `index`, i.e.
-- the sum of every preceding line's bytes plus its newline. `index == line_count`
-- yields the buffer's total byte length. Backs vim.treesitter._range.add_bytes
-- for buffer-sourced node ranges.
function vim.api.nvim_buf_get_offset(bufnr, index)
  local buf = vim._bufs[vim._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then return -1 end
  local lines = buf.lines
  local off = 0
  for i = 1, index do
    off = off + #(lines[i] or "") + 1
  end
  return off
end

-- nvim_buf_get_text: the text in the (0-based, end-exclusive) byte range
-- [start_row,start_col)..[end_row,end_col), returned as a list of lines (the span
-- split on newlines). Columns are byte indices into their line. vim.treesitter
-- uses this to extract node text from a buffer.
function vim.api.nvim_buf_get_text(bufnr, start_row, start_col, end_row, end_col, _opts)
  local buf = vim._bufs[vim._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then return {} end
  local lines = buf.lines
  if start_row == end_row then
    return { (lines[start_row + 1] or ""):sub(start_col + 1, end_col) }
  end
  local out = { (lines[start_row + 1] or ""):sub(start_col + 1) }
  for r = start_row + 1, end_row - 1 do
    out[#out + 1] = lines[r + 1] or ""
  end
  out[#out + 1] = (lines[end_row + 1] or ""):sub(1, end_col)
  return out
end

function vim.api.nvim_buf_get_lines(bufnr, start, end_, strict)
  local buf = vim._bufs[vim._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then
    if strict then error("Invalid buffer id", 2) end
    return {}
  end
  local lines = buf.lines
  local n = #lines
  local s = vim._norm_line_index(start, n, strict)
  local e = vim._norm_line_index(end_, n, strict)
  if e < s then e = s end
  local out = {}
  for i = s + 1, e do
    out[#out + 1] = lines[i]
  end
  return out
end

-- INCOMPLETE: can't produce a buffer without a final newline — `normalize()`
-- always re-adds the trailing phantom `\n` (no `nofixeol`). Each call is also its
-- own undo step (no `undojoin` coalescing), so a plugin issuing many small edits
-- leaves many undo entries. Faithful once the core models `nofixeol`/`undojoin`.
function vim.api.nvim_buf_set_lines(bufnr, start, end_, strict, repl)
  local id = vim._resolve_bufnr(bufnr)
  local buf = vim._bufs[id]
  if not buf or not buf.lines then
    if strict then error("Invalid buffer id", 2) end
    return
  end
  local lines = buf.lines
  local n = #lines
  local s = vim._norm_line_index(start, n, strict)
  local e = vim._norm_line_index(end_, n, strict)
  if e < s then e = s end
  -- Write-through: splice the mirror so a read-after-write within this chunk is
  -- consistent, then queue the real edit (the server re-derives the byte range).
  local updated = {}
  for i = 1, s do updated[#updated + 1] = lines[i] end
  for i = 1, #repl do updated[#updated + 1] = repl[i] end
  for i = e + 1, n do updated[#updated + 1] = lines[i] end
  buf.lines = updated
  vim._buf_set_lines(id, start, end_, repl)
end

-- nvim_set_option_value(name, value, opts): set an option in the scope its name
-- implies. A window-local option (number/relativenumber) — or any option with an
-- explicit `opts.win` — routes through vim.wo (the targeted window, else the
-- current one); otherwise it routes through vim.bo (`opts.buf`, else the current
-- buffer). The wired options reach the core; everything else lands in the
-- observable per-scope store.
function vim.api.nvim_set_option_value(name, value, opts)
  opts = opts or {}
  if opts.win or WIN_OPT_CANON[name] then
    vim.wo[opts.win and resolve_win(opts.win) or resolve_win(0)][name] = value
    return
  end
  local buf = opts.buf and vim._resolve_bufnr(opts.buf) or vim._resolve_bufnr(0)
  vim.bo[buf][name] = value
end

-- nvim_get_option_value(name, opts): read an option from the scope its name
-- implies (see nvim_set_option_value), so a wired option reflects the core's
-- current value (default until set).
function vim.api.nvim_get_option_value(name, opts)
  opts = opts or {}
  if opts.win or WIN_OPT_CANON[name] then
    return vim.wo[opts.win and resolve_win(opts.win) or resolve_win(0)][name]
  end
  local buf = opts.buf and vim._resolve_bufnr(opts.buf) or vim._resolve_bufnr(0)
  return vim.bo[buf][name]
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

