-- bemtvi Lua prelude — the READ-ONLY btv.* editor-entity API + highlights/namespaces.
-- The context getters an autocmd / keymap callback reads to know what fired:
-- btv.buf.* (name / current / lines / text / line_count / is_loaded / list / …),
-- btv.win.* (current / buf / list / width / height / config / nr / …), btv.cursor.get /
-- btv.cursor.set (the cursor read + the reveal/jump write),
-- btv.tabpage.* reads, btv.screen.*, btv.mode / btv.current_line, plus the highlight
-- read/define (btv.hl), namespaces (btv.ns), and the extmark decoration layer. The
-- matching vim.api.nvim_* / vim.fn.* names are aliased onto each native.
--
-- The buffer-TEXT mutations are btv.buf.set_lines (alias nvim_buf_set_lines) — the
-- whole-line write — and btv.buf.set_text (alias nvim_buf_set_text) — the precise
-- sub-line range write; both async promises queued through the btv._buf_set_lines /
-- btv._buf_set_text bridges (see below). The buffer CHANGE stream is btv.buf.attach —
-- the server's on_bytes byte-delta channel, wired for on_bytes / on_reload (the
-- nvim_buf_attach on_lines / on_detach callbacks and a vim.api.nvim_buf_attach alias
-- stay deferred). The rest of the lifecycle surface — set_name, nvim_create_buf /
-- nvim_buf_delete — stays absent until a real need lands. (The window / tab / float create-and-modify API — nvim_open_win,
-- nvim_win_set_*, nvim_set_current_* — is queued through btv._open_win / btv._win_*.)
--
-- Reads the mirror state / resolvers from prelude/state.lua (loaded just before
-- this chunk).
local vim = vim
local api = vim.api
btv = btv or {}

-- The `btv.*` entity tables this chunk reads through (ADR 0002). `btv.hl` may
-- already exist (the Rust bridge seeds `btv.hl.define`); the rest are fresh here.
btv.buf = btv.buf or {}
btv.win = btv.win or {}
btv.cursor = btv.cursor or {}
btv.tabpage = btv.tabpage or {}
btv.hl = btv.hl or {}
btv.ns = btv.ns or {}

-- Current-window resolver (`0`/`nil` → the current window), defined on `vim` in
-- prelude/state.lua (which loads first) so the `btv.wo` machinery can share it.
-- The local alias keeps this file's many window call sites terse.
local resolve_win = btv._resolve_win

-- `btv.buf.name(bufnr)` -> string [alias `nvim_buf_get_name` / `vim.fn.bufname`]: the full
-- name (path) of buffer `bufnr`; `0` / nil means the current buffer. Returns `""` for
-- an unnamed or unknown buffer. Read from the buffer mirror, so it can name any open
-- buffer — e.g. a custom 'tabline' labelling a buffer shown in another tab.
function btv.buf.name(bufnr)
  local cur = btv._cur_buf or { bufnr = 0, name = "" }
  if bufnr == nil or bufnr == 0 or bufnr == cur.bufnr then
    return cur.name
  end
  -- A non-current buffer: resolve its name from the full buffer mirror (which
  -- carries every open buffer), so e.g. a custom 'tabline' can name a buffer
  -- shown in another tab. Empty for an unknown handle.
  local b = (btv._bufs or {})[bufnr]
  return (b and b.name) or ""
end

-- The buffer / window / cursor getters read the `btv._bufs` / `btv._cur_*` mirror the
-- server refreshes before each Lua entry, so they return live state as of the start
-- of this chunk. There is deliberately no buffer *mutation* surface here (no
-- `btv.buf.set_lines`): plugins supply data through the higher-level surfaces, per
-- this file's header. Throughout, a `bufnr` of `0` / nil means the current buffer.
--
-- `btv.buf.current()` -> bufnr [alias `nvim_get_current_buf`]: the current buffer's
-- number (`0` if there is none).
function btv.buf.current()
  return (btv._cur_buf or {}).bufnr or 0
end

-- `nvim_get_mode()`: the editor's current mode, read from the `btv._cur_mode`
-- snapshot the server refreshes before each Lua entry. `blocking` is always
-- false — the in-VM Lua bindings only run when the server is between keys, so it
-- is never blocked on input here. (The dedicated RPC method serves remote clients.)
function btv.mode()
  return { mode = btv._cur_mode or "n", blocking = false }
end

-- Window API (Phase 5). Reads resolve against the `btv._wins` mirror the server
-- refreshes before running Lua; mutations queue a `WindowOp` (the `btv._win_*` /
-- `btv._open_win` Rust bridges) the server drains into the live editor after the
-- chunk, the same "Lua queues, core mutates" flow as the buffer API. `0`/`nil`
-- means the current window throughout.

function btv.win.current()
  return btv._cur_win or 1000
end

function btv.win.list()
  -- Spans every tab, matching `nvim_list_wins` (and its RPC twin); the per-tab
  -- window *number* order is `btv._win_order`.
  return btv._win_all or { btv._cur_win or 1000 }
end

-- `btv.win.set_current(win)`: focus `win` (make it the current window) [alias
-- `nvim_set_current_win`]. `win` is a window id (0 / nil = the current window, a no-op).
-- The switch is queued and applied after the Lua chunk like the other window ops; the
-- mirror is updated write-through so an immediate `btv.win.current()` / current-buffer read
-- in the same chunk reflects the new focus.
function btv.win.set_current(win)
  win = resolve_win(win)
  btv._set_current_win(win)
  btv._cur_win = win
  local w = (btv._wins or {})[win]
  if w then
    btv._cur_cursor = { row = w.row or 1, col = w.col or 0 }
    local b = (btv._bufs or {})[w.buffer]
    btv._cur_buf =
      { bufnr = w.buffer, name = (b and b.name) or "", filetype = (b and b.filetype) or "" }
  end
end
vim.api.nvim_set_current_win = btv.win.set_current

function btv.win.buf(win)
  win = resolve_win(win)
  local w = (btv._wins or {})[win]
  return w and w.buffer or vim.api.nvim_get_current_buf()
end

function btv.cursor.get(win)
  win = resolve_win(win)
  local w = (btv._wins or {})[win]
  if w then
    return { w.row, w.col }
  end
  local c = btv._cur_cursor or { row = 1, col = 0 }
  return { c.row, c.col }
end

-- `btv.cursor.set(pos[, win])`: move window `win`'s cursor (`0`/nil = the current
-- window) to `pos`, a `{ row, col }` pair in the SAME convention `btv.cursor.get`
-- returns — a 1-based `row` and a 0-based byte `col`. The
-- target is clamped into the buffer. This is the setter half of the cursor surface:
-- the reveal / jump-to primitive a picker or a "go to definition"-style plugin uses;
-- ordinary navigation stays plain normal-mode motion. Like the rest of the window
-- mutation API it queues a window op the server applies after this chunk (the same
-- "Lua queues, core mutates" flow), via the `btv._win_set_cursor` bridge.
function btv.cursor.set(pos, win)
  if type(pos) ~= "table" or type(pos[1]) ~= "number" then
    error("btv.cursor.set: pos must be a { row, col } table (1-based row, 0-based col)", 2)
  end
  btv._win_set_cursor(win or 0, pos[1] - 1, pos[2] or 0)
end

-- `nvim_get_current_line()`: the text of the line the cursor is on in the current
-- window/buffer (no trailing newline). Composed from the cursor row and the
-- buffer's lines — a completion plugin reads this when it builds a completion
-- `context`, which runs as soon as its core spins up, so a missing builtin would
-- break completion (and every completion source) at load.
function btv.current_line()
  local row = vim.api.nvim_win_get_cursor(0)[1] -- 1-based
  local lines = vim.api.nvim_buf_get_lines(0, row - 1, row, false)
  return lines[1] or ""
end

function btv.win.width(win)
  win = resolve_win(win)
  local w = (btv._wins or {})[win]
  return w and w.width or 0
end

function btv.win.height(win)
  win = resolve_win(win)
  local w = (btv._wins or {})[win]
  return w and w.height or 0
end

-- `nvim_win_call(win, fn)` / `nvim_buf_call(buf, fn)`: run `fn` as if `win`/`buf`
-- were current, returning fn's result. In neovim these temporarily switch the
-- editor's current window/buffer for the duration of the callback; in bemtvi the
-- callback runs synchronously in-VM, where "current" is the mirror the server
-- pushed (`btv._cur_win` / `btv._cur_buf` / `btv._cur_cursor`). So these swap that
-- mirror context for the call, run `fn`, and restore it — which makes every
-- *read* inside the callback (`nvim_win_get_cursor`, `nvim_get_current_buf`,
-- `vim.fn.line`/`col`/`winnr`, …) resolve against the requested window/buffer, and
-- every explicit-handle write that bemtvi *does* expose (`vim.bo[buf]` option sets,
-- `nvim_buf_set_extmark(buf, …)`) resolves the swapped mirror at call time and queues
-- that concrete handle — so it, too, targets the right place.
--
-- What bemtvi CAN'T do is retarget a mutation that binds to "current" only at
-- DRAIN time — an ex-command (`vim.cmd`), feedkeys, or an LSP buf request — since
-- the queued-ops model applies those against the editor's real current
-- buffer/window after the chunk, which this call never actually switched. Rather
-- than silently mutate the wrong context, `btv._call_ctx_lock` is set for the
-- duration of a call whose target differs from the real current, and those
-- funnels raise while it is set (see `btv._assert_call_ctx`). Plugins use these
-- calls to read a window's view/dimensions, which is fully faithful.
function btv.win.call(win, fn)
  win = resolve_win(win)
  local saved_win, saved_cursor, saved_buf = btv._cur_win, btv._cur_cursor, btv._cur_buf
  local saved_lock = btv._call_ctx_lock
  local w = (btv._wins or {})[win]
  btv._cur_win = win
  if w then
    btv._cur_cursor = { row = w.row or 1, col = w.col or 0 }
    local b = (btv._bufs or {})[w.buffer]
    btv._cur_buf = { bufnr = w.buffer, name = (b and b.name) or "", filetype = "" }
  end
  -- Lock context-dependent mutations when this actually switches windows (stay
  -- locked if an enclosing call already did).
  btv._call_ctx_lock = saved_lock or (win ~= saved_win)
  local ok, ret = pcall(fn)
  btv._cur_win, btv._cur_cursor, btv._cur_buf = saved_win, saved_cursor, saved_buf
  btv._call_ctx_lock = saved_lock
  if not ok then
    error(ret, 0)
  end
  return ret
end

-- `btv.buf.call(buf, fn)` -> any [alias `nvim_buf_call`]: run `fn` with `buf` (0/nil =
-- current) installed as the current-buffer context, then restore the previous
-- context and return whatever `fn` returned. Use it so buffer-relative lookups
-- inside `fn` (name, options) resolve against `buf`. An error in `fn` propagates.
function btv.buf.call(buf, fn)
  buf = btv._resolve_bufnr(buf)
  local saved_buf = btv._cur_buf
  local saved_lock = btv._call_ctx_lock
  local b = (btv._bufs or {})[buf]
  btv._cur_buf = {
    bufnr = buf,
    name = (b and b.name) or "",
    filetype = (saved_buf and buf == saved_buf.bufnr) and saved_buf.filetype or "",
  }
  btv._call_ctx_lock = saved_lock or (buf ~= (saved_buf and saved_buf.bufnr))
  local ok, ret = pcall(fn)
  btv._cur_buf = saved_buf
  btv._call_ctx_lock = saved_lock
  if not ok then
    error(ret, 0)
  end
  return ret
end

-- ----- tab pages (Phase 3) -------------------------------------------------
-- Reads resolve from the `btv._tabs` mirror the server pushes before each Lua
-- entry; `nvim_set_current_tabpage` is the lone mutation (queue + write-through),
-- the same "Lua queues, core mutates" flow as the window API. `0`/`nil` is the
-- current tab throughout.
local function resolve_tab(tab)
  if tab == nil or tab == 0 then
    return btv._cur_tab or 1
  end
  return tab
end

function btv.tabpage.current()
  return btv._cur_tab or 1
end

function btv.tabpage.list()
  return btv._tab_order or { btv._cur_tab or 1 }
end

function btv.tabpage.is_valid(tab)
  return (btv._tabs or {})[resolve_tab(tab)] ~= nil
end

function btv.tabpage.number(tab)
  tab = resolve_tab(tab)
  for i, id in ipairs(btv._tab_order or {}) do
    if id == tab then
      return i
    end
  end
  return 0
end

function btv.tabpage.wins(tab)
  local t = (btv._tabs or {})[resolve_tab(tab)]
  return t and t.windows or {}
end

function btv.tabpage.win(tab)
  local t = (btv._tabs or {})[resolve_tab(tab)]
  return t and t.current_window or (btv._cur_win or 1000)
end

-- `btv.tabpage.set_current`(tab) [alias `nvim_set_current_tabpage`]: make `tab` the
-- active tab page (`0`/`nil` = the current one, which is a no-op). The lone tab
-- MUTATION in the API — every other `nvim_tabpage_*` call is a read off the
-- `btv._tabs` mirror. The switch is queued for the server (`btv._set_current_tab`)
-- and written through the mirror, so a read later in the same chunk already agrees.
-- An unknown tab id fails loud rather than silently doing nothing.
function btv.tabpage.set_current(tab)
  tab = resolve_tab(tab)
  if not btv.tabpage.is_valid(tab) then
    error("nvim_set_current_tabpage: invalid tabpage id " .. tostring(tab), 2)
  end
  btv._set_current_tab(tab)
  -- Write-through: the current tab, and the window focus that comes with it.
  btv._cur_tab = tab
  local t = (btv._tabs or {})[tab]
  if t and t.current_window then
    btv._cur_win = t.current_window
  end
end

-- `nvim_win_get_config(win)`: the float placement of `win` as neovim's config map,
-- or `{ relative = "" }` for a tiled window. Reads the `btv._wins` mirror (the
-- server pushes each float's config into `w.float`; `nvim_open_win` /
-- `nvim_win_set_config` write through it so a read within the same chunk agrees).
function btv.win.config(win)
  win = resolve_win(win)
  local f = ((btv._wins or {})[win] or {}).float
  if not f then
    return { relative = "" }
  end
  -- Return a fresh table so a caller mutating the result can't corrupt the mirror.
  local cfg = {
    relative = f.relative,
    anchor = f.anchor,
    row = f.row,
    col = f.col,
    width = f.width,
    height = f.height,
    zindex = f.zindex,
    focusable = f.focusable,
    border = f.border,
  }
  if f.align then
    cfg.align = f.align
  end
  if f.win then
    cfg.win = f.win
  end
  if f.title then
    cfg.title = f.title
  end
  return cfg
end

-- `btv.buf.is_loaded(bufnr)` -> bool [alias `nvim_buf_is_loaded`]: whether `bufnr` (0/nil
-- = current) names a buffer that is loaded into memory. Backed by the buffer mirror,
-- which carries every loaded buffer.
function btv.buf.is_loaded(bufnr)
  return btv._bufs[btv._resolve_bufnr(bufnr)] ~= nil
end

-- `btv.buf.is_valid(bufnr)` -> bool [alias `nvim_buf_is_valid`]: whether `bufnr` (0/nil =
-- current) names a buffer bemtvi knows about. There is no separate "valid but
-- unloaded" state in the mirror yet, so this currently matches `btv.buf.is_loaded`.
function btv.buf.is_valid(bufnr)
  return btv._bufs[btv._resolve_bufnr(bufnr)] ~= nil
end

-- `btv.buf.changedtick(bufnr)` -> integer [alias `nvim_buf_get_changedtick`]: the
-- buffer's change counter (0/nil = current buffer), bumped by the core on every text
-- change and never otherwise. `0` for an unknown buffer.
--
-- This is the canonical "did this buffer's text change" signal: cache derived state
-- against it and recompute only when it moves, instead of redoing the work on every
-- event. A statusline component that enumerates matches, a plugin that parses the
-- buffer, any per-buffer memo — key it on `(bufnr, changedtick)`:
--
-- ```lua
-- local memo = {}
-- local function matches(buf)
--   local tick = btv.buf.changedtick(buf)
--   local m = memo[buf]
--   if m and m.tick == tick then
--     return m.value
--   end
--   local value = expensive_scan(buf)
--   memo[buf] = { tick = tick, value = value }
--   return value
-- end
-- ```
function btv.buf.changedtick(bufnr)
  local buf = btv._bufs[btv._resolve_bufnr(bufnr)]
  return (buf and buf.changedtick) or 0
end

-- `btv.buf.line_count(bufnr)` -> integer [alias `nvim_buf_line_count`]: the number of
-- lines in `bufnr` (0/nil = current); `0` for an unknown buffer.
function btv.buf.line_count(bufnr)
  local buf = btv._bufs[btv._resolve_bufnr(bufnr)]
  return (buf and buf.lines) and #buf.lines or 0
end

-- `btv.buf.offset(bufnr, index)` -> integer [alias `nvim_buf_get_offset`]: the byte
-- offset at which 0-based line `index` starts — the sum of every preceding line's
-- bytes plus its newline. `index == line_count` gives the buffer's total byte
-- length. Returns `-1` for an unknown buffer.
function btv.buf.offset(bufnr, index)
  local buf = btv._bufs[btv._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then
    return -1
  end
  local lines = buf.lines
  local off = 0
  for i = 1, index do
    off = off + #(lines[i] or "") + 1
  end
  return off
end

-- `btv.buf.text(bufnr, start_row, start_col, end_row, end_col[, opts])` -> lines [alias
-- `nvim_buf_get_text`]: the text of `bufnr` spanning (start_row, start_col) up to
-- (end_row, end_col), returned as a list of lines (the span split on newlines). Rows
-- are 0-based; columns are 0-based byte indices into their line; the end position is
-- exclusive. Use this for a sub-line span — use `btv.buf.lines` for whole lines.
function btv.buf.text(bufnr, start_row, start_col, end_row, end_col, _opts)
  local buf = btv._bufs[btv._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then
    return {}
  end
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

-- `btv.buf.lines(bufnr, start, end_[, strict])` -> lines [alias `nvim_buf_get_lines`]:
-- the lines of `bufnr` (0/nil = current) in the 0-based, end-EXCLUSIVE range
-- [start, end_). Negative indices count back from the end (`-1` is one past the last
-- line), so `(0, -1)` is the whole buffer. With `strict` true an out-of-range index
-- errors; otherwise indices clamp into range. Returns a list of strings, each
-- without its trailing newline.
function btv.buf.lines(bufnr, start, end_, strict)
  local buf = btv._bufs[btv._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then
    if strict then
      error("Invalid buffer id", 2)
    end
    return {}
  end
  local lines = buf.lines
  local n = #lines
  local s = btv._norm_line_index(start, n, strict)
  local e = btv._norm_line_index(end_, n, strict)
  if e < s then
    e = s
  end
  local out = {}
  for i = s + 1, e do
    out[#out + 1] = lines[i]
  end
  return out
end

-- `btv.buf.set_lines(bufnr, start, end_, strict, replacement)` -> promise [alias
-- `nvim_buf_set_lines`]: replace lines [start, end_) of `bufnr` (0/nil = current) with
-- `replacement` (a list of whole-line strings), 0-based and end-EXCLUSIVE. Negative
-- indices count back from the end (`-1` is one past the last line), so `(0, -1, …)`
-- replaces the WHOLE buffer and `(n, n, …, { "x" })` appends. With `strict` true an
-- out-of-range index errors; otherwise indices clamp.
--
-- This is the editor's ONE buffer-text mutation, and it is ASYNCHRONOUS: the Lua VM
-- cannot touch the live buffer mid-chunk, so the edit is QUEUED and applied right after
-- this chunk. The returned promise fulfils (with nil) on the next tick — once the edit
-- has landed and the buffer mirror reflects it — so `btv.await(btv.buf.set_lines(…))` is
-- the point at which a following `btv.buf.lines` read sees the new content. The shape and
-- the buffer's modifiability are validated SYNCHRONOUSLY and raise (fail loud) before
-- anything is queued: a non-table replacement, a non-string / newline-bearing line, an
-- unknown buffer, a `nomodifiable` buffer, or (under `strict`) an out-of-range index. A
-- read-only buffer KIND (terminal / `btv.view` / quickfix) is refused loudly server-side.
function btv.buf.set_lines(bufnr, start, end_, strict, replacement)
  local buf = btv._resolve_bufnr(bufnr)
  local mirror = btv._bufs[buf]
  if not mirror or not mirror.lines then
    error("nvim_buf_set_lines: invalid buffer id " .. tostring(buf), 2)
  end
  local bo = btv._bo_mirror[buf]
  if bo and bo.modifiable == false then
    error("nvim_buf_set_lines: buffer " .. tostring(buf) .. " is not 'modifiable'", 2)
  end
  if type(replacement) ~= "table" then
    error("nvim_buf_set_lines: replacement must be a list of strings", 2)
  end
  for i = 1, #replacement do
    local line = replacement[i]
    if type(line) ~= "string" then
      error(("nvim_buf_set_lines: replacement[%d] is not a string"):format(i), 2)
    end
    if line:find("\n", 1, true) then
      error(("nvim_buf_set_lines: replacement[%d] contains a newline"):format(i), 2)
    end
  end
  local n = #mirror.lines
  local s = btv._norm_line_index(start, n, strict)
  local e = btv._norm_line_index(end_, n, strict)
  if e < s then
    e = s
  end
  btv._buf_set_lines(buf, s, e, replacement)
  return btv.promise.new(function(resolve)
    btv.on_next_tick(resolve)
  end)
end

-- `btv.buf.set_text(bufnr, start_row, start_col, end_row, end_col, replacement)` -> promise
-- [alias `nvim_buf_set_text`]: replace the CHARACTER range
-- `(start_row, start_col)`..`(end_row, end_col)` of `bufnr` (0/nil = current) with
-- `replacement` (a list of lines). 0-based; rows are line indices, columns are byte
-- offsets within the line; the range is end-EXCLUSIVE. Unlike `set_lines` this is a
-- precise SUB-LINE edit: `set_text(0, 4, 0, 7, { "xy" })` replaces bytes 4..7 of line 0
-- in place, and `replacement` is spliced verbatim (its lines joined by `\n`, with NO
-- trailing newline added). The precise edit a snippet / structural-edit plugin uses to
-- swap a trigger word for an expanded body, or to update a mirror inline.
--
-- Like `set_lines` this is the editor's ONE buffer-text mutation and is ASYNCHRONOUS:
-- the edit is QUEUED and applied right after this chunk, and the returned promise fulfils
-- (with nil) on the next tick — once the mirror reflects it — so
-- `btv.await(btv.buf.set_text(…))` is the point a following read sees the new content. The
-- shape and modifiability are validated SYNCHRONOUSLY and raise before anything is queued
-- (a non-table replacement, a non-string / newline-bearing line, an unknown or
-- `nomodifiable` buffer); the server refuses a read-only KIND (terminal / `btv.view` /
-- quickfix) and an inverted span loudly. Out-of-range coordinates clamp into the buffer.
function btv.buf.set_text(bufnr, start_row, start_col, end_row, end_col, replacement)
  local buf = btv._resolve_bufnr(bufnr)
  local mirror = btv._bufs[buf]
  if not mirror or not mirror.lines then
    error("nvim_buf_set_text: invalid buffer id " .. tostring(buf), 2)
  end
  local bo = btv._bo_mirror[buf]
  if bo and bo.modifiable == false then
    error("nvim_buf_set_text: buffer " .. tostring(buf) .. " is not 'modifiable'", 2)
  end
  if type(replacement) ~= "table" then
    error("nvim_buf_set_text: replacement must be a list of strings", 2)
  end
  for i = 1, #replacement do
    local line = replacement[i]
    if type(line) ~= "string" then
      error(("nvim_buf_set_text: replacement[%d] is not a string"):format(i), 2)
    end
    if line:find("\n", 1, true) then
      error(("nvim_buf_set_text: replacement[%d] contains a newline"):format(i), 2)
    end
  end
  btv._buf_set_text(buf, start_row, start_col, end_row, end_col, replacement)
  return btv.promise.new(function(resolve)
    btv.on_next_tick(resolve)
  end)
end

-- ===== Buffer change notifications (nvim_buf_attach `on_bytes`) =============
-- The server projects every applied edit into neovim's `on_bytes` byte-delta tuple
-- and fires it here (a resync — undo/redo/`:e`, where deltas are meaningless — fires
-- `on_reload` instead). `btv.buf.attach` is the public subscriber: a plugin that keeps
-- derived state in sync as the user types (a snippet engine mirroring/transforming a
-- tabstop, a live structural editor) reacts to the precise delta rather than
-- re-scanning the whole buffer each keystroke.
--
-- `btv.buf._attached[bufnr] = { [handle] = { on_bytes = fn, on_reload = fn } }`.
btv.buf._attached = btv.buf._attached or {}
btv.buf._attach_next = btv.buf._attach_next or 0

-- `btv.buf.attach(buffer, { on_bytes = fn, on_reload = fn })` -> detach()
-- [partial alias `nvim_buf_attach`]: subscribe to `buffer`'s (0/nil = current) change
-- stream. At least one callback is required; an unsupported key fails loud (only
-- `on_bytes` / `on_reload` are wired — `on_lines` / `on_detach` / `on_changedtick`
-- stay deferred). Returns a `detach()` closure that removes this subscription; a
-- callback may also return `true` to detach itself (neovim's convention). The
-- callbacks fire on the editor thread AFTER every mirror is consistent, so one may
-- read `btv.buf.lines` / `btv.buf.extmarks` and see the post-edit state.
--
-- `on_bytes` is called
-- `on_bytes("bytes", bufnr, changedtick, start_row, start_col, start_byte,
--            old_end_row, old_end_col, old_end_byte,
--            new_end_row, new_end_col, new_end_byte)` — the exact neovim tuple (rows
-- 0-based, `*_byte` a total buffer byte offset, the `old_*` / `new_*` the removed /
-- inserted extent). `on_reload` is called `on_reload("reload", bufnr)` when the whole
-- rope was replaced (the delta stream is invalid — re-read the buffer whole).
function btv.buf.attach(buffer, opts)
  if type(opts) ~= "table" then
    error("btv.buf.attach: opts must be a table of callbacks", 2)
  end
  local allowed = { on_bytes = true, on_reload = true }
  for k, v in pairs(opts) do
    if not allowed[k] then
      error(
        "btv.buf.attach: unsupported callback '"
          .. tostring(k)
          .. "' (only on_bytes / on_reload are wired)",
        2
      )
    end
    if type(v) ~= "function" then
      error("btv.buf.attach: " .. k .. " must be a function, got " .. type(v), 2)
    end
  end
  if not opts.on_bytes and not opts.on_reload then
    error("btv.buf.attach: at least one of on_bytes / on_reload is required", 2)
  end
  local b = btv._resolve_bufnr(buffer)
  btv.buf._attached[b] = btv.buf._attached[b] or {}
  btv.buf._attach_next = btv.buf._attach_next + 1
  local handle = btv.buf._attach_next
  btv.buf._attached[b][handle] = { on_bytes = opts.on_bytes, on_reload = opts.on_reload }
  return function()
    local subs = btv.buf._attached[b]
    if subs then
      subs[handle] = nil
      if next(subs) == nil then
        btv.buf._attached[b] = nil
      end
    end
  end
end

-- Run one attached callback, surfacing an error like the other dispatchers (a bad
-- callback must not kill the others or the edit loop). A callback returning `true`
-- detaches itself; returns whether to drop the subscription.
local function fire_attach_cb(fn, kind, ...)
  local ok, res = pcall(fn, ...)
  if not ok then
    btv.notify("E5108: Error in btv.buf.attach " .. kind .. " callback: " .. tostring(res), "error")
    return false
  end
  return res == true
end

-- Dispatcher for the server's `on_bytes` firing — the byte-delta tuple, per attached
-- subscriber. Kept an early return when the buffer has no subscribers, so the common
-- (unattached) edit pays nothing beyond the table lookup.
function btv._buf_bytes_changed(buf, tick, sr, sc, sb, oer, oec, oeb, ner, nec, neb)
  local subs = btv.buf._attached[buf]
  if not subs then
    return
  end
  for handle, cb in pairs(subs) do
    if cb.on_bytes then
      local drop = fire_attach_cb(
        cb.on_bytes,
        "on_bytes",
        "bytes",
        buf,
        tick,
        sr,
        sc,
        sb,
        oer,
        oec,
        oeb,
        ner,
        nec,
        neb
      )
      if drop then
        subs[handle] = nil
      end
    end
  end
  if next(subs) == nil then
    btv.buf._attached[buf] = nil
  end
end

-- Dispatcher for the server's `on_reload` firing (a wholesale rope replace).
function btv._buf_reloaded(buf)
  local subs = btv.buf._attached[buf]
  if not subs then
    return
  end
  for handle, cb in pairs(subs) do
    if cb.on_reload then
      if fire_attach_cb(cb.on_reload, "on_reload", "reload", buf) then
        subs[handle] = nil
      end
    end
  end
  if next(subs) == nil then
    btv.buf._attached[buf] = nil
  end
end

-- `btv.buf.search(bufnr, pattern, opts)` -> match | nil: find `pattern` in `bufnr`
-- (0/nil = current) over the buffer mirror, line by line. The native counterpart to
-- scanning lines in Lua — it runs the match in Rust (the `regex` crate or the vim
-- engine) so a plugin can jump straight to a section (a conflict marker, a heading).
--
-- opts (all optional):
--
-- ```
-- plain      = false             -- literal substring (ignores `engine`)
-- engine     = "pcre" | "vim"    -- regex dialect (default "pcre")
-- from       = { line=1, col=0 } -- start position: 1-based line, 0-based byte col
-- backward   = false             -- search upward from `from` instead of down
-- ignorecase = false             -- case-insensitive match
-- ```
--
-- Returns nil when there is no match, else:
--
-- ```
-- { line, col, end_line, end_col, text, captures }
-- ```
--
-- with `line`/`end_line` 1-based, `col`/`end_col` 0-based byte offsets (end
-- exclusive), `text` the matched substring, and `captures` the submatch strings
-- (`\1`.., `""` for a group that didn't participate). Matching is line-by-line, so a
-- multi-line (`\n`-spanning) pattern is not supported.
function btv.buf.search(bufnr, pattern, opts)
  local buf = btv._bufs[btv._resolve_bufnr(bufnr)]
  if not buf or not buf.lines then
    return nil
  end
  return btv._buf_search(buf.lines, pattern, opts or {})
end

-- `btv.regex(pattern, opts)` -> regex: compile `pattern` into a reusable regex object
-- for matching Lua **strings** — a more capable `string.find`/`match`/`gmatch`/`gsub`
-- with a real regex dialect (named groups, alternation, lazy quantifiers, …). The
-- match runs in Rust, so a string you already hold in Lua is matched in place (no
-- copy). For searching *buffer* text instead, use `btv.buf.search`. Raises on an
-- invalid pattern.
--
-- opts (all optional):
--
-- ```
-- engine     = "pcre" | "vim"  -- regex dialect (default "pcre", the `regex` crate)
-- plain      = false           -- match the pattern literally (ignores `engine`)
-- ignorecase = false           -- case-insensitive match
-- ```
--
-- Offsets follow the `string` library: 1-based and byte-based, with `:find`'s `end`
-- inclusive, so `s:sub(re:find(s))` is the matched text. The returned object has:
--
-- ```
-- re:find(s, init?)    -> start, end, cap1, … | nil   (like string.find)
-- re:match(s, init?)   -> the capture(s), or the whole match if the pattern
--                         has none, or nil            (like string.match)
-- re:gmatch(s)         -> iterator over each match's captures or whole match
--                                                     (like string.gmatch)
-- re:gsub(s, repl, n?) -> newstring, count            (like string.gsub)
--     repl is a string (`%0` whole match, `%1`-`%9` captures, `%%` literal `%`),
--     a function called with the captures (return nil/false to keep the match),
--     or a table keyed by the first capture.
-- re:test(s)           -> boolean: does the pattern match anywhere
-- ```
--
-- `init` is 1-based and may be negative to count from the end, as in `string.find`.
--
-- ```lua
-- local re = btv.regex([[(\w+)@(\w+)]])
-- local _, _, user, host = re:find("to jo@acme now")  -- "jo", "acme"
-- for word in btv.regex([[\w+]]):gmatch("one two") do ... end
-- local masked = btv.regex([[\d]]):gsub("id 42", "*")  -- "id **"
-- ```
function btv.regex(pattern, opts)
  return btv._regex(pattern, opts)
end

-- ===== Extmarks layer ==================================================
-- See docs/specs/2026-06-07-extmark-decoration-layer-design.md. v1 carries the
-- highlight-relevant attrs only; virtual text / signs / conceal are not modelled
-- yet and are rejected loudly rather than silently ignored.

-- `nvim_create_namespace(name)`: create-or-get a namespace id by name (an empty /
-- nil name mints a fresh anonymous one each call). Ids are allocated Lua-side, so
-- the call returns synchronously; the server only ever sees the id on a mark.
function btv.ns.create(name)
  name = name or ""
  if name ~= "" and btv._namespaces[name] then
    return btv._namespaces[name]
  end
  local id = btv._namespace_next
  btv._namespace_next = id + 1
  if name ~= "" then
    btv._namespaces[name] = id
  end
  return id
end

-- The extmark options v1 RENDERS: position, span, highlight, priority. `end_line`
-- is neovim's deprecated alias for `end_row`.
local EXTMARK_OPT_OK = {
  id = true,
  end_row = true,
  end_line = true,
  end_col = true,
  hl_group = true,
  priority = true,
  -- Anchor gravity (neovim's `right_gravity` / `end_right_gravity`) — HONORED end
  -- to end: threaded to the core `ExtmarkStore` so the mark tracks edits per its
  -- gravity, letting a plugin place a *growing* range (a live snippet tabstop) that
  -- swallows text typed at its edges. Defaults `true` / `false`.
  right_gravity = true,
  end_right_gravity = true,
}
-- Decoration options bemtvi ACCEPTS and STORES (so nvim_buf_get_extmarks(…,
-- {details=true}) returns them), forwarded to the server as the `decoration` payload.
-- `virt_text` / `virt_lines` (with their `virt_text_pos` / `hl_mode` / `win_col` /
-- `virt_lines_above` / `virt_text_hide` modifiers) RENDER end to end across the TUI,
-- GUI, and web clients; the rest are accepted and stored but NOT yet painted. Two of
-- those are deliberate, not just unfinished:
--   * `virt_text_repeat_linebreak` is a no-op BY DESIGN — it only repeats the virt
--     text at a soft-wrap boundary, and bemtvi has no soft-wrap ('wrap' isn't a
--     modelled option), so there is no wrap point to repeat at.
--   * `virt_lines_leftcol` (start a virtual line over the gutter rather than the text
--     body) is a pending refinement: it needs a per-virtual-row flag threaded through
--     the core row layout and the wire; until then virtual lines start at the text body.
-- `line_hl_group` (a full-width line background, neovim's `line_hl_group`) also
-- RENDERS — projected as the per-window `line_bg` layer painted under the text, the
-- `'cursorline'` model — so a plugin can back a whole line (e.g. a markdown code block
-- with `@markup.raw.block`) end to end. So does the gutter sign pair `sign_text` +
-- `sign_hl_group`: the sign is merged with the LSP diagnostic signs into the row's single
-- sign cell (`extmarks.rs::merged_sign_cells`, highest `priority` wins) and its
-- highlight is resolved through the window's `winhighlight` into the cell's style, so a
-- plugin's signs paint AND take their colour. The remainder — conceal and the other
-- line-highlight groups — are accepted and stored but unpainted. (The gravity flags
-- `right_gravity` / `end_right_gravity` are NOT in this bucket — they are honored end to
-- end; see `EXTMARK_OPT_OK` and `btv.buf.set_extmark`.) For the unpainted rest, a
-- documented approximation (the matchadd / winblend pattern): a plugin that decorates
-- with them loads and runs, those supplementary bits just aren't painted yet. Rejecting
-- them loud would instead break the plugin's render path. The core extmark store still
-- tracks the mark's POSITION (for get_extmarks) regardless.
local EXTMARK_OPT_DECORATION = {
  virt_text = true,
  virt_text_pos = true,
  virt_text_win_col = true,
  virt_text_hide = true,
  -- `virt_text_fg_only` — btv-native: paint the virtual text in its highlight group's
  -- FOREGROUND only, so the surface underneath keeps its own background. For an
  -- overlay glyph that is chrome rather than a highlight of text (the signature
  -- float's active-parameter caret); RENDERS end to end.
  virt_text_fg_only = true,
  virt_text_repeat_linebreak = true,
  hl_mode = true,
  hl_eol = true,
  virt_lines = true,
  virt_lines_above = true,
  virt_lines_leftcol = true,
  sign_text = true,
  sign_hl_group = true,
  -- `line_fill = { text, hl_group }` — an btv-native whole-line fill (the text
  -- repeated across the line's width), e.g. a rule on a blank alignment row. Both
  -- `sign_text` and `line_fill` RENDER (the rest below are stored-but-unpainted).
  line_fill = true,
  number_hl_group = true,
  line_hl_group = true,
  cursorline_hl_group = true,
  conceal = true,
  spell = true,
  ui_watched = true,
  url = true,
  strict = true,
  undo_restore = true,
  invalidate = true,
}

-- Split a `set_extmark`-shaped option table into the positional payload the
-- `btv._extmark_set` bridge takes, validating as it goes: an unknown key fails loud,
-- an accepted-but-not-position key is collected into the `decoration` payload, and the
-- `end_line` alias / gravity defaults are resolved. `prefix` names the calling surface
-- in an error and `level` is the `error()` level to blame (`0` where there is no useful
-- source position, as inside a decor provider).
--
-- Shared by `btv.buf.set_extmark` and `btv.decor`'s `publish` so the two carry ONE
-- decoration vocabulary that cannot drift — a key either surface accepts, the other
-- accepts too, and a decoration added to the extmark layer reaches a viewport provider
-- the same day.
--
-- Returns `hl_group, end_row, end_col, priority, decoration, right_gravity,
-- end_right_gravity`.
function btv._extmark_split_opts(opts, prefix, level)
  -- Collect any decoration payload so a details read can return it; reject only a key
  -- from neither set (a true unknown).
  local decoration = nil
  for k in pairs(opts) do
    if not EXTMARK_OPT_OK[k] then
      if EXTMARK_OPT_DECORATION[k] then
        decoration = decoration or {}
        decoration[k] = opts[k]
      else
        error(prefix .. ": option '" .. tostring(k) .. "' is not supported yet", level)
      end
    end
  end
  local hl_group = opts.hl_group
  if hl_group ~= nil and type(hl_group) ~= "string" then
    error(prefix .. ": hl_group must be a string (group lists not supported yet)", level)
  end
  -- `end_line` is the deprecated alias for `end_row`; honor either.
  local end_row = opts.end_row
  if end_row == nil then
    end_row = opts.end_line
  end
  if (end_row == nil) ~= (opts.end_col == nil) then
    error(prefix .. ": end_row/end_line and end_col must be given together", level)
  end
  local priority = opts.priority or 4096
  -- Anchor gravity — neovim's defaults (start right-gravity, end left-gravity) unless
  -- the caller opts into a growing range.
  local right_gravity = opts.right_gravity
  if right_gravity == nil then
    right_gravity = true
  elseif type(right_gravity) ~= "boolean" then
    error(prefix .. ": right_gravity must be a boolean", level)
  end
  local end_right_gravity = opts.end_right_gravity
  if end_right_gravity == nil then
    end_right_gravity = false
  elseif type(end_right_gravity) ~= "boolean" then
    error(prefix .. ": end_right_gravity must be a boolean", level)
  end
  return hl_group, end_row, opts.end_col, priority, decoration, right_gravity, end_right_gravity
end

-- `btv.buf.set_extmark(buffer, ns, line, col[, opts])` -> id [alias
-- `nvim_buf_set_extmark`]: place (or update, via `opts.id`) an extmark in `buffer`
-- under namespace `ns` (see `btv.ns.create`) at 0-based `line` / `col` (col a byte
-- offset). `opts` carries the highlight-relevant attrs — `end_row` / `end_col` for a
-- ranged mark, `hl_group`, `priority`, … — and an unsupported decoration key fails
-- loud rather than being ignored. Returns the mark id. The mutation is queued for
-- the server, but the mirror is written through, so a read later in this chunk sees it.
--
-- `right_gravity` (default `true`) and `end_right_gravity` (default `false`) set the
-- anchor gravity — the direction each edge is dragged when text is inserted *at* it —
-- and are HONORED against the live rope, not just stored. The default is a range that
-- does NOT grow when you type at its edges (a highlight span); `right_gravity = false`
-- with `end_right_gravity = true` makes an (even empty) range GROW to swallow text
-- typed at either edge — the anchor shape a live snippet tabstop needs.
function btv.buf.set_extmark(buffer, ns, line, col, opts)
  local b = btv._resolve_bufnr(buffer)
  opts = opts or {}
  -- Level 3 blames THIS function's caller: 1 is the splitter, 2 is here.
  local hl_group, end_row, end_col, priority, decoration, right_gravity, end_right_gravity =
    btv._extmark_split_opts(opts, "nvim_buf_set_extmark", 3)

  btv._extmark_next[b] = btv._extmark_next[b] or {}
  local mark_id = opts.id or btv._extmark_next[b][ns] or 1
  -- Advance the allocator past this id so a later auto-id can't collide.
  btv._extmark_next[b][ns] = math.max(btv._extmark_next[b][ns] or 1, mark_id + 1)

  -- Write-through the mirror (read-after-write within this chunk).
  btv._extmarks[b] = btv._extmarks[b] or {}
  btv._extmarks[b][ns] = btv._extmarks[b][ns] or {}
  btv._extmarks[b][ns][mark_id] = {
    row = line,
    col = col,
    end_row = end_row,
    end_col = end_col,
    hl_group = hl_group,
    priority = priority,
    decoration = decoration,
    right_gravity = right_gravity,
    end_right_gravity = end_right_gravity,
  }
  btv._extmark_set(
    b,
    ns,
    mark_id,
    line,
    col,
    end_row,
    end_col,
    hl_group,
    priority,
    decoration,
    right_gravity,
    end_right_gravity
  )
  return mark_id
end

-- `btv.buf.del_extmark(buffer, ns, id)` -> bool [alias `nvim_buf_del_extmark`]: remove
-- mark `id` of namespace `ns` from `buffer`. Returns whether the mark existed.
function btv.buf.del_extmark(buffer, ns, id)
  local b = btv._resolve_bufnr(buffer)
  local marks = btv._extmarks[b] and btv._extmarks[b][ns]
  local existed = marks ~= nil and marks[id] ~= nil
  if existed then
    marks[id] = nil
  end
  btv._extmark_del(b, ns, id)
  return existed
end

-- `btv.buf.clear_namespace(buffer, ns, line_start, line_end)` [alias
-- `nvim_buf_clear_namespace`]: drop namespace `ns`'s extmarks in `buffer` whose line
-- is in the 0-based range [line_start, line_end) — `line_end == -1` means to the end
-- of the buffer. `ns == -1` clears every namespace.
function btv.buf.clear_namespace(buffer, ns, line_start, line_end)
  local b = btv._resolve_bufnr(buffer)
  if ns == -1 then
    for nsid in pairs(btv._extmarks[b] or {}) do
      vim.api.nvim_buf_clear_namespace(b, nsid, line_start, line_end)
    end
    return
  end
  local marks = btv._extmarks[b] and btv._extmarks[b][ns]
  if marks then
    for id, m in pairs(marks) do
      if line_end == -1 or (m.row >= line_start and m.row < line_end) then
        marks[id] = nil
      end
    end
  end
  btv._extmark_clear(b, ns, line_start, line_end)
end

-- Normalize a `get_extmarks` position argument to an inclusive (row, col) bound.
-- v1 supports the common `0` (buffer start), `-1` (buffer end), and `{row, col}`
-- forms; a bare mark-id position is rejected rather than silently mishandled.
local function extmark_pos_bound(p)
  if p == 0 then
    return 0, 0
  end
  if p == -1 then
    return math.huge, math.huge
  end
  if type(p) == "table" then
    return p[1] or 0, p[2] or 0
  end
  error("nvim_buf_get_extmarks: only 0, -1, and {row, col} positions are supported", 2)
end

-- `btv.buf.extmarks(buffer, ns, start, end_[, opts])` -> list [alias
-- `nvim_buf_get_extmarks`]: the extmarks of namespace `ns` in `buffer` within the
-- position range `start`..`end_` — each bound is `0` (buffer start), `-1` (buffer
-- end), or a `{row, col}` pair. Entries come in (row, col, id) order, each
-- `{id, row, col}` (or `{id, row, col, details}` with `opts.details`). `ns == -1` returns
-- marks from every namespace. Reads the mirror, so it reflects marks set earlier in
-- this chunk; positions are current as of chunk start.
function btv.buf.extmarks(buffer, ns, start, end_, opts)
  local b = btv._resolve_bufnr(buffer)
  opts = opts or {}
  local sr, sc = extmark_pos_bound(start)
  local er, ec = extmark_pos_bound(end_)
  local out = {}
  local function in_range(row, col)
    if row < sr or (row == sr and col < sc) then
      return false
    end
    if row > er or (row == er and col > ec) then
      return false
    end
    return true
  end
  local function collect(nsid, marks)
    for id, m in pairs(marks) do
      if in_range(m.row, m.col) then
        local e = { id, m.row, m.col }
        if opts.details then
          local d = {
            ns_id = nsid,
            end_row = m.end_row,
            end_col = m.end_col,
            hl_group = m.hl_group,
            priority = m.priority,
            right_gravity = m.right_gravity,
            end_right_gravity = m.end_right_gravity,
          }
          -- Spread any accepted-but-unrendered decoration payload (virt_text, …)
          -- into the details dict at top level, matching neovim's shape.
          if m.decoration then
            for k, v in pairs(m.decoration) do
              d[k] = v
            end
          end
          e[4] = d
        end
        out[#out + 1] = e
      end
    end
  end
  local bufmarks = btv._extmarks[b] or {}
  if ns == -1 then
    for nsid, marks in pairs(bufmarks) do
      collect(nsid, marks)
    end
  else
    collect(ns, bufmarks[ns] or {})
  end
  table.sort(out, function(x, y)
    if x[2] ~= y[2] then
      return x[2] < y[2]
    end
    if x[3] ~= y[3] then
      return x[3] < y[3]
    end
    return x[1] < y[1]
  end)
  if opts.limit and #out > opts.limit then
    local trimmed = {}
    for i = 1, opts.limit do
      trimmed[i] = out[i]
    end
    out = trimmed
  end
  return out
end

-- `nvim_replace_termcodes(str, from_part, do_lt, special)`: translate key notation
-- (`<CR>`, `<C-w>`, `<lt>`, …) into neovim's terminal-byte encoding — `<C-o>` →
-- `\15`, `<CR>` → `\r`, `<Esc>` → `\27`, `<lt>` → `<` — via the native
-- `btv._replace_termcodes`. Keys with no single ASCII byte (arrows, function keys)
-- have no such encoding bemtvi can emit (neovim uses K_SPECIAL sequences bemtvi
-- doesn't model), so they stay as `<...>` notation, which `parse_keys` still
-- consumes — so the result round-trips exactly through `nvim_feedkeys` (build a
-- "feed string", later feed it). The flags (`from_part` / `do_lt` / `special`) only
-- shape neovim's byte output and are accepted for call-compatibility; bemtvi always
-- translates the special names and `<lt>`.
function btv.replace_termcodes(str, _from_part, _do_lt, _special)
  return btv._replace_termcodes(tostring(str or ""))
end

-- `btv.hl.define` (the canonical highlight setter) is installed from Rust — it
-- captures the group definition for the server to fold into the core highlight
-- registry — so it is not (re)defined here; the `vim.api.nvim_set_hl` alias is
-- added in the block at the end of the file.

-- `nvim_get_hl(ns, opts)`: read highlight group definitions from the mirror the
-- server refreshes when the registry changes. `ns == 0` reads the global table
-- (`btv._hl_defs`); a non-zero `ns` reads that namespace's own table
-- (`btv._hl_defs_ns[ns]`), with no fallback to the global table — matching
-- neovim, where a group not defined in the namespace reads `{}` and render-time
-- fallback is a separate mechanism. Forms:
--   * opts.name given          -> that group's definition. A link group returns
--                                 `{ link = "Target" }`; a concrete group returns
--                                 its colors (fg/bg/sp as 0xRRGGBB ints) and the
--                                 set boolean attrs. Unknown group -> `{}`.
--   * opts.name + link = false -> follow the link chain and return the resolved
--                                 concrete definition (what a popup plugin reads
--                                 to blend popup colors).
--   * no name                  -> every group keyed by name.
-- A fresh table is returned each call so a caller mutating it can't corrupt the
-- mirror. INCOMPLETE: `nvim_win_set_hl_ns` (render-time namespace selection) is
-- not modelled, and the extra metadata neovim attaches (`default`, `cterm*`) is
-- absent — bemtvi's registry is truecolor-only.
local function copy_hl_def(d)
  local out = {}
  for k, v in pairs(d) do
    out[k] = v
  end
  return out
end

function btv.hl.get(ns, opts)
  opts = opts or {}
  local defs = ((ns == nil or ns == 0) and btv._hl_defs or (btv._hl_defs_ns or {})[ns]) or {}
  if opts.name ~= nil then
    local name = opts.name
    if opts.link == false then
      -- Follow the link chain to the concrete definition (cycle-guarded).
      local seen = 0
      while defs[name] and defs[name].link ~= nil and seen < 32 do
        name = defs[name].link
        seen = seen + 1
      end
    end
    local d = defs[name]
    return d and copy_hl_def(d) or {}
  end
  local out = {}
  for name, d in pairs(defs) do
    out[name] = copy_hl_def(d)
  end
  return out
end

-- ----- vim.api.nvim_* compatibility aliases --------------------------------
-- The muscle-memory `vim.api.nvim_*` names, each forwarding to the canonical
-- `btv.*` native defined above (same function object, same signature).
vim.api.nvim_buf_get_lines = btv.buf.lines
vim.api.nvim_buf_set_lines = btv.buf.set_lines
vim.api.nvim_buf_set_text = btv.buf.set_text
vim.api.nvim_buf_get_text = btv.buf.text
vim.api.nvim_buf_get_name = btv.buf.name
vim.api.nvim_buf_get_offset = btv.buf.offset
vim.api.nvim_buf_line_count = btv.buf.line_count
vim.api.nvim_buf_is_loaded = btv.buf.is_loaded
vim.api.nvim_buf_is_valid = btv.buf.is_valid
vim.api.nvim_buf_call = btv.buf.call
vim.api.nvim_get_current_buf = btv.buf.current
vim.api.nvim_buf_set_extmark = btv.buf.set_extmark
vim.api.nvim_buf_del_extmark = btv.buf.del_extmark
vim.api.nvim_buf_get_extmarks = btv.buf.extmarks
vim.api.nvim_buf_clear_namespace = btv.buf.clear_namespace
vim.api.nvim_list_wins = btv.win.list
vim.api.nvim_get_current_win = btv.win.current
vim.api.nvim_win_get_buf = btv.win.buf
vim.api.nvim_win_get_config = btv.win.config
vim.api.nvim_win_get_height = btv.win.height
vim.api.nvim_win_get_width = btv.win.width
vim.api.nvim_win_call = btv.win.call
vim.api.nvim_win_get_cursor = btv.cursor.get
vim.api.nvim_list_tabpages = btv.tabpage.list
vim.api.nvim_get_current_tabpage = btv.tabpage.current
vim.api.nvim_tabpage_get_number = btv.tabpage.number
vim.api.nvim_tabpage_get_win = btv.tabpage.win
vim.api.nvim_tabpage_list_wins = btv.tabpage.wins
vim.api.nvim_tabpage_is_valid = btv.tabpage.is_valid
vim.api.nvim_set_current_tabpage = btv.tabpage.set_current
vim.api.nvim_set_hl = btv.hl.define
vim.api.nvim_get_hl = btv.hl.get
vim.api.nvim_create_namespace = btv.ns.create
vim.api.nvim_get_mode = btv.mode
vim.api.nvim_get_current_line = btv.current_line
vim.api.nvim_replace_termcodes = btv.replace_termcodes

-- ----- window view / screen position (the vim.fn.* popup plugins read) -------
-- These resolve the *current* window — which `nvim_win_call(win, fn)` swaps to its
-- target for the duration of the call (`btv._cur_win` / `btv._cur_cursor`), so a
-- `nvim_win_call(popup, vim.fn.winsaveview)` reads the popup's view.

-- `btv.win.saveview()` [alias `vim.fn.winsaveview`]: the current window's view — cursor
-- position, scroll (`topline`/`leftcol`), and the cursor-restore fields neovim
-- returns. bemtvi has no separate `curswant`/`coladd`/`skipcol` state, so those
-- mirror `col` / are 0.
function btv.win.saveview()
  local win = btv._cur_win or 1000
  local w = (btv._wins or {})[win] or {}
  local c = btv._cur_cursor or {}
  local lnum = c.row or w.row or 1
  local col = c.col or w.col or 0
  return {
    lnum = lnum,
    col = col,
    coladd = 0,
    curswant = col,
    topline = w.topline or 1,
    leftcol = w.leftcol or 0,
    skipcol = 0,
  }
end
vim.fn.winsaveview = btv.win.saveview

-- Window-scroll / cursor SETTERS. These take an *explicit* window id (0 = current)
-- and queue a concrete-handle op, so — unlike a `winrestview()` that binds to
-- "current" — they work from inside `btv.win.call(other, …)` without tripping the
-- call-context lock. Built for a side-by-side diff / scrollbind plugin that mirrors
-- one window's view onto another. A `WinScrolled` autocmd fires for the moved window.

-- `btv.win.set_topline(win, topline)`: scroll `win` so its first visible buffer line is
-- `topline` (1-based, neovim/`winsaveview` convention; clamped to the last line).
function btv.win.set_topline(win, topline)
  btv._win_set_topline(win or 0, math.max(0, (topline or 1) - 1))
end

-- `btv.win.set_leftcol(win, leftcol)`: horizontally scroll `win` so its first visible
-- screen column is `leftcol` (0-based). Only meaningful under `'nowrap'`.
function btv.win.set_leftcol(win, leftcol)
  btv._win_set_leftcol(win or 0, math.max(0, leftcol or 0))
end

-- `btv.win.set_cursor(win, line[, col])`: move `win`'s cursor to `line` (1-based) /
-- `col` (0-based byte column, default 0). The explicit-win counterpart of the
-- (intentionally-absent) `nvim_win_set_cursor`, and the sanctioned caret primitive a
-- pure-Lua editor feature drives with. It works mid-**insert** (a `col` one past the
-- last char is legal there), so a snippet engine owning its own tabstop session jumps
-- between tabstops by calling this from its insert-mode jump key — the caret moves and
-- further typing lands at the new spot.
function btv.win.set_cursor(win, line, col)
  btv._win_set_cursor(win or 0, math.max(0, (line or 1) - 1), math.max(0, col or 0))
end

-- `btv.win.select_range(win, s_row, s_col, e_row, e_col[, opts])`: enter **Select mode**
-- over the 0-based, end-exclusive byte range `(s_row, s_col)` .. `(e_row, e_col)` in `win`.
-- The range is highlighted like a charwise Visual selection, but the next printable key /
-- `<CR>` / `<BS>` **replaces** it (deletes the range and enters Insert with that input) —
-- vim's `v_CTRL-G` behavior. This is the Select-mode sibling of `btv.win.set_cursor` and the
-- P6 snippet-engine primitive: a consumer selecting a **non-empty** placeholder `${1:default}`
-- calls this (so typing replaces the default); an empty range degrades to caret-plus-Insert
-- at the start (an empty tabstop uses `btv.win.set_cursor` instead).
--
-- `opts.on_escape` controls where `<Esc>` (with nothing typed) leaves the kept selection:
--
-- ```
-- "normal"   -- (default) keep the text, drop to Normal on the selection — vim's v_CTRL-G
-- "insert"   -- keep the text, park the caret in Insert past it (a snippet engine wants this,
--            --   so it can keep editing the placeholder)
-- ```
function btv.win.select_range(win, s_row, s_col, e_row, e_col, opts)
  local escape_insert = (opts and opts.on_escape) == "insert"
  btv._win_select_range(
    win or 0,
    math.max(0, s_row or 0),
    math.max(0, s_col or 0),
    math.max(0, e_row or 0),
    math.max(0, e_col or 0),
    escape_insert
  )
end

-- `btv.win.restview(win, view)`: restore `win`'s view from a `winsaveview`-shaped table
-- (`topline` 1-based, `leftcol` 0-based, optional `lnum`/`col` cursor) — the
-- explicit-win `winrestview` analogue. Only the present fields are applied.
function btv.win.restview(win, view)
  win = win or 0
  view = view or {}
  if view.topline then
    btv.win.set_topline(win, view.topline)
  end
  if view.leftcol then
    btv.win.set_leftcol(win, view.leftcol)
  end
  if view.lnum then
    btv.win.set_cursor(win, view.lnum, view.col or 0)
  end
end

-- `btv.screen.row()` / `btv.screen.col()` [aliases `vim.fn.screenrow` / `screencol`]: the
-- cursor's 1-based position on the whole screen, mirrored by the server
-- (`btv._cur_screenrow` / `_cur_screencol`) for the focused window. Popup plugins read
-- them to avoid drawing a popup over the cursor.
btv.screen = btv.screen or {}
function btv.screen.row()
  return btv._cur_screenrow or 0
end
function btv.screen.col()
  return btv._cur_screencol or 0
end
vim.fn.screenrow = btv.screen.row
vim.fn.screencol = btv.screen.col

-- `btv.wo` / `vim.wo` (window-local options) live with the other option scopes in
-- prelude/state.lua; the gutter mirror (`btv._wins`) and `btv._resolve_win` it
-- reads are defined here. The deprecated window-scoped getters/setters carry no
-- implementation of their own — they wrap the sibling `nvim_*_option_value` funnels
-- (`btv.option.get` / `btv.option.set`) with the scope pinned to a window.
function vim.api.nvim_win_get_option(win, name)
  return vim.api.nvim_get_option_value(name, { win = win or 0 })
end
function vim.api.nvim_win_set_option(win, name, value)
  vim.api.nvim_set_option_value(name, value, { win = win or 0 })
end

-- ----- vim.fn editor-state builtins (statusline / lualine, Phase 5) ----------
-- The Vimscript builtins a real `'statusline'` (and lualine) call from inside a
-- `%{}`/`%!` expression. Each reads the Rust→Lua mirror the server refreshes
-- before evaluating the statusline (`btv._cur_mode` / `btv._cur_cursor` / `btv._bufs` /
-- `btv._cur_buf` / `btv._wins`), so a live redraw reflects the current frame. An
-- unsupported argument fails loud (the no-silent-stub rule) rather than guessing.

-- `btv.mode_str([expanded])` [alias `vim.fn.mode`]: the single-letter mode code
-- (`"n"`/`"i"`/`"v"`/`"V"`/`"R"`/`"c"`). (Distinct from `btv.mode()`, which returns the
-- `nvim_get_mode` `{mode, blocking}` table.) INCOMPLETE: `expanded` is ignored — the
-- core has a flat `Mode` (no operator-pending / sub-state), so `mode(1)`'s multi-char
-- forms (`"no"`, `"niI"`, …) don't exist here; the short code is returned for both.
function btv.mode_str(_expanded)
  return btv._cur_mode or "n"
end
vim.fn.mode = btv.mode_str

-- `btv.cmdtype.get()` [alias `vim.fn.getcmdtype`]: the type char of the open command
-- line — `":"` (ex), `"/"` or `"?"` (search), `"@"` (a scripted input/confirm prompt) — or
-- `""` when none is open. Read from the `btv._cur_cmdtype` mirror the server refreshes.
btv.cmdtype = btv.cmdtype or {}
function btv.cmdtype.get()
  return btv._cur_cmdtype or ""
end
vim.fn.getcmdtype = btv.cmdtype.get

-- `btv.win.nr([arg])` [alias `vim.fn.winnr`]: the current window's 1-based number (its
-- index in the layout order), or with `"$"` the number of windows. (vim's `"#"`
-- previous-window form needs window history the mirror doesn't keep, so it errors.)
function btv.win.nr(arg)
  if arg == nil or arg == "." then
    local cur = btv._cur_win or 1000
    for i, id in ipairs(btv._win_order or {}) do
      if id == cur then
        return i
      end
    end
    return 0
  elseif arg == "$" then
    return #(btv._win_order or {})
  end
  error("winnr(): unsupported argument '" .. tostring(arg) .. "'", 2)
end
vim.fn.winnr = btv.win.nr

-- `btv.tabpage.nr([arg])` [alias `vim.fn.tabpagenr`]: the current tab page's 1-based
-- number, or with `"$"` the number of tab pages — the tab analogue of `winnr()`. Backs
-- the loop in a custom `'tabline'` (`for i = 1, tabpagenr('$')`). Resolves from the
-- `btv._tabs` / `btv._tab_order` mirror the server pushes before evaluating the tabline.
function btv.tabpage.nr(arg)
  if arg == nil or arg == "." then
    return vim.api.nvim_tabpage_get_number(0)
  elseif arg == "$" then
    return #(btv._tab_order or { btv._cur_tab or 1 })
  end
  error("tabpagenr(): unsupported argument '" .. tostring(arg) .. "'", 2)
end
vim.fn.tabpagenr = btv.tabpage.nr

-- `btv.tabpage.buflist(nr)` [alias `vim.fn.tabpagebuflist`]: the list of buffer numbers
-- shown in tab page `nr` (1-based; nil/0 is the current tab), one per window in that
-- tab — what a custom `'tabline'` label reads to find the tab's active file. Reads
-- the tab mirror's per-window `buffers` (parallel to `windows`), which the server
-- fills for EVERY tab — unlike the global window mirror, which only carries the
-- current tab, so `nvim_win_get_buf` would resolve an inactive tab's window to the
-- current buffer.
function btv.tabpage.buflist(nr)
  local tab_id
  if nr == nil or nr == 0 then
    tab_id = btv._cur_tab or 1
  else
    tab_id = (btv._tab_order or {})[nr]
  end
  local t = (btv._tabs or {})[tab_id]
  local bufs = {}
  for _, buf in ipairs(t and t.buffers or {}) do
    bufs[#bufs + 1] = buf
  end
  return bufs
end
vim.fn.tabpagebuflist = btv.tabpage.buflist

-- (`vim.fn.bufnr` / `bufname` live in prelude/fs.lua, which loads after this chunk —
-- the canonical "additional vim.fn" home — so they aren't (re)defined here.)

-- `btv.win.width_nr(nr)` / `btv.win.height_nr(nr)` [aliases `vim.fn.winwidth` / `winheight`]:
-- a window's text dimensions, addressed by window *number* (1-based layout index;
-- 0 = current). The `_nr` suffix distinguishes them from `btv.win.width` / `btv.win.height`
-- (the `nvim_win_get_*` form), which take a window *handle*.
local function win_by_number(nr)
  if nr == nil or nr == 0 then
    return btv._cur_win or 1000
  end
  return (btv._win_order or {})[nr]
end
function btv.win.width_nr(nr)
  local w = (btv._wins or {})[win_by_number(nr)]
  return w and w.width or 0
end
function btv.win.height_nr(nr)
  local w = (btv._wins or {})[win_by_number(nr)]
  return w and w.height or 0
end
vim.fn.winwidth = btv.win.width_nr
vim.fn.winheight = btv.win.height_nr

-- `nvim_get_hl_by_name(name, rgb)`: the pre-0.9 highlight reader (lualine and other
-- older plugins still call it). Returns the *resolved* group (link chain followed)
-- in the legacy shape — `foreground`/`background`/`special` truecolor ints plus
-- the set boolean attrs — rather than `nvim_get_hl`'s `fg`/`bg`/`sp`. bemtvi's
-- registry is truecolor-only, so only `rgb == true` (RGB output) can be honored; a
-- cterm read (`rgb` false/nil) has no backing model and fails loud rather than
-- returning RGB ints mislabeled as cterm indices. An unknown group returns `{}`.
function vim.api.nvim_get_hl_by_name(name, rgb)
  if rgb ~= true then
    error(
      "nvim_get_hl_by_name: bemtvi is truecolor-only; cterm output (rgb=false) is not modelled",
      0
    )
  end
  local d = vim.api.nvim_get_hl(0, { name = name, link = false })
  local out = {}
  if d.fg ~= nil then
    out.foreground = d.fg
  end
  if d.bg ~= nil then
    out.background = d.bg
  end
  if d.sp ~= nil then
    out.special = d.sp
  end
  for _, attr in ipairs({ "bold", "italic", "underline", "undercurl", "strikethrough", "reverse" }) do
    if d[attr] then
      out[attr] = true
    end
  end
  return out
end

-- `nvim_echo` aliases `btv.echo`. Bind the private `btv._echo` bridge directly: this chunk
-- loads before btv.lua (where the documented `btv.echo` wrapper is defined), and both name
-- the same native, so the alias is identical either way.
vim.api.nvim_echo = btv._echo

-- `btv.hl.exists(name)`: is the highlight group `name` defined? Returns a native
-- boolean (the rest of `btv.*` is boolean, not vim's 1/0). Backed by the same
-- `btv._hl_defs` registry `nvim_get_hl` reads (concrete groups and links both count).
function btv.hl.exists(name)
  return (btv._hl_defs or {})[name] ~= nil
end
-- `vim.fn.hlexists` keeps the vimscript 1/0 contract: LuaSnip probes it as
-- `vim.fn.hlexists(group) == 1 and group or nil`, which a boolean would break.
function vim.fn.hlexists(name)
  return btv.hl.exists(name) and 1 or 0
end

-- The slot -> source-group chain `btv.hl.palette` resolves, and the One Dark literal
-- each chain ends in. The literals ARE the built-in `bemtvi` colorscheme's palette, so
-- a session with no colorscheme loaded still reads as the editor's own theme rather
-- than as some foreign plugin default. Kept above the docstring so the generated book
-- page stays attached to the function (the generator takes the comment block
-- immediately above a definition).
--
-- A chain lists the canonical groups that carry that hue in vim's own group
-- vocabulary, most specific first. `Error` is deliberately absent from `red`: vim's
-- `Error` is white-ON-red, so its `fg` is a *background* colour and would invert any
-- accent derived from it.
local PALETTE_SLOTS = {
  -- surfaces
  { "bg", { { "Normal", "bg" } }, "#282c34" },
  { "bg_alt", { { "NormalFloat", "bg" }, { "StatusLine", "bg" } }, "#21252b" },
  { "bg_cursorline", { { "CursorLine", "bg" } }, "#2c313a" },
  { "bg_sel", { { "Visual", "bg" } }, "#3e4451" },
  -- text
  { "fg", { { "Normal", "fg" } }, "#abb2bf" },
  { "muted", { { "Comment", "fg" }, { "LineNr", "fg" }, { "NonText", "fg" } }, "#5c6370" },
  -- accents
  {
    "red",
    { { "DiagnosticError", "fg" }, { "ErrorMsg", "fg" }, { "Exception", "fg" } },
    "#e06c75",
  },
  { "green", { { "String", "fg" }, { "DiagnosticOk", "fg" } }, "#98c379" },
  { "yellow", { { "Type", "fg" }, { "DiagnosticWarn", "fg" }, { "WarningMsg", "fg" } }, "#e5c07b" },
  { "blue", { { "Function", "fg" }, { "Directory", "fg" }, { "Title", "fg" } }, "#61afef" },
  { "purple", { { "Keyword", "fg" }, { "Statement", "fg" }, { "PreProc", "fg" } }, "#c678dd" },
  { "cyan", { { "Operator", "fg" }, { "Special", "fg" }, { "DiagnosticInfo", "fg" } }, "#56b6c2" },
  { "orange", { { "Constant", "fg" }, { "Number", "fg" }, { "Boolean", "fg" } }, "#d19a66" },
}

-- `btv.hl.palette()` -> the ACTIVE colorscheme's semantic palette, as a table of
-- `"#rrggbb"` strings. This is the canonical way a plugin picks default colors: read
-- the hues the running theme actually uses instead of hardcoding one theme's hex
-- values, which go wrong the moment any other colorscheme (or a light flavour of the
-- same one) is loaded.
--
-- Each slot resolves through a chain of the canonical vim/treesitter groups that carry
-- that hue, most specific first, falling back to the built-in `bemtvi` (One Dark)
-- value when the active theme defines none of them — so a bare session with no
-- colorscheme still lands on the editor's own colors:
--
-- ```
-- SURFACES
--   bg             Normal.bg
--   bg_alt         NormalFloat.bg -> StatusLine.bg    (a float / sidebar / status strip)
--   bg_cursorline  CursorLine.bg
--   bg_sel         Visual.bg
-- TEXT
--   fg             Normal.fg
--   muted          Comment.fg -> LineNr.fg -> NonText.fg   (guides, dimmed entries)
-- ACCENTS
--   red            DiagnosticError.fg -> ErrorMsg.fg -> Exception.fg
--   green          String.fg -> DiagnosticOk.fg
--   yellow         Type.fg -> DiagnosticWarn.fg -> WarningMsg.fg
--   blue           Function.fg -> Directory.fg -> Title.fg
--   purple         Keyword.fg -> Statement.fg -> PreProc.fg
--   cyan           Operator.fg -> Special.fg -> DiagnosticInfo.fg
--   orange         Constant.fg -> Number.fg -> Boolean.fg
-- ```
--
-- Links are followed to the concrete definition, so a theme that writes
-- `Function = { link = "Blue" }` still resolves. A fresh table is returned each call
-- and nothing is cached: re-call it from a `ColorScheme` handler to restyle live.
--
-- ```lua
-- local function paint()
--   local p = btv.hl.palette()
--   btv.hl.define(0, "MyPluginKey", { fg = p.cyan })
--   btv.hl.define(0, "MyPluginDim", { fg = p.muted, italic = true })
-- end
-- paint()
-- btv.on("ColorScheme", {}, paint)
-- ```
function btv.hl.palette()
  local out = {}
  for _, slot in ipairs(PALETTE_SLOTS) do
    local name, chain, fallback = slot[1], slot[2], slot[3]
    local value = nil
    for _, source in ipairs(chain) do
      local def = btv.hl.get(0, { name = source[1], link = false })
      local v = def and def[source[2]]
      if type(v) == "number" then
        value = string.format("#%06x", v)
        break
      end
    end
    out[name] = value or fallback
  end
  return out
end

-- The groups `btv.hl.fallback` has installed, `name -> the spec it wrote`. This is the
-- ownership ledger that lets a re-apply tell "still holding OUR default" apart from
-- "a theme or the user has since claimed it". Kept above the docstring so the
-- generated book page stays attached to the function.
local hl_fallback_owned = {}

-- A `"#rrggbb"` string or a 0xRRGGBB int -> the int, for comparing a spec (which
-- writes strings) against a live definition (which reads ints).
local function hl_color_num(v)
  if type(v) == "string" then
    return tonumber((v:gsub("^#", "")), 16)
  end
  return v
end

-- Is the live definition `live` exactly the spec `spec` (over the union of their
-- keys, colours compared numerically)? `link` and the boolean attrs compare directly.
local function hl_def_equal(live, spec)
  local keys = {}
  for k in pairs(live) do
    keys[k] = true
  end
  for k in pairs(spec) do
    keys[k] = true
  end
  for k in pairs(keys) do
    local a, b = live[k], spec[k]
    if k == "fg" or k == "bg" or k == "sp" then
      a, b = hl_color_num(a), hl_color_num(b)
    end
    if a ~= b then
      return false
    end
  end
  return true
end

-- `btv.hl.fallback(name, spec)` -> `true` when it installed: define highlight group
-- `name` as a DEFAULT that yields to the active colorscheme and to the user. It writes
-- `spec` only when the group is undefined, or when the group still holds exactly what
-- this API last wrote for it — so a theme (or an explicit `btv.hl.define`) that claims
-- the group is never clobbered, and a stale default from a *previous* theme is.
--
-- That second half is what a plain `if not btv.hl.exists(name)` guard gets wrong.
-- `:colorscheme` drops only the outgoing scheme's OWN groups; a plugin's stay. So a
-- group the new theme doesn't model still holds the default derived from the old one,
-- `exists` reports true, and the guard skips the re-apply — leaving, say, a
-- dark-flavour hex on a light theme. Pair this with `btv.hl.palette` and a
-- `ColorScheme` handler and the defaults track whatever is loaded:
--
-- ```lua
-- local function paint()
--   local p = btv.hl.palette()
--   btv.hl.fallback("MyPluginKey", { fg = p.cyan })
--   btv.hl.fallback("MyPluginDim", { fg = p.muted, italic = true })
-- end
-- paint()
-- btv.on("ColorScheme", {}, paint)
-- ```
function btv.hl.fallback(name, spec)
  if type(name) ~= "string" or name == "" then
    error("btv.hl.fallback: `name` must be a non-empty highlight group name", 2)
  end
  if type(spec) ~= "table" then
    error("btv.hl.fallback: `spec` must be a highlight definition table", 2)
  end
  local live = btv.hl.get(0, { name = name })
  local claimed = next(live) ~= nil
  local owned = hl_fallback_owned[name]
  if claimed and not (owned and hl_def_equal(live, owned)) then
    -- Somebody else owns this group: the colorscheme styled it, or the user did.
    hl_fallback_owned[name] = nil
    return false
  end
  btv.hl.define(0, name, spec)
  local copy = {}
  for k, v in pairs(spec) do
    copy[k] = v
  end
  hl_fallback_owned[name] = copy
  return true
end

-- ===== nvim_* deprecated aliases & small gaps ================================

-- `btv.buf.set_option(buf, name, value)` [alias `nvim_buf_set_option`]: set buffer-local
-- option `name` to `value` on `buf` (0/nil = current). A pre-0.10 accessor kept
-- because plugins call it pervasively (bufhidden / modifiable / filetype / buftype
-- on scratch buffers); in new code prefer `btv.option.set(name, value, { buf = buf })`,
-- which this wraps.
function btv.buf.set_option(buf, name, value)
  btv.option.set(name, value, { buf = buf })
end
-- `btv.buf.get_option(buf, name)` -> value [alias `nvim_buf_get_option`]: read buffer-
-- local option `name` from `buf` (0/nil = current) — the read counterpart of
-- `btv.buf.set_option`. In new code prefer `btv.option.get(name, { buf = buf })`.
function btv.buf.get_option(buf, name)
  return btv.option.get(name, { buf = buf })
end
api.nvim_buf_set_option = btv.buf.set_option
api.nvim_buf_get_option = btv.buf.get_option

-- `nvim_win_is_valid(win)`: whether `win` names a window the mirror knows about
-- (0/nil is the current window, always valid while one exists). The window
-- analogue of `nvim_buf_is_valid` — picker teardown/resize guards call it constantly.
function btv.win.is_valid(win)
  if win == nil or win == 0 then
    return (btv._cur_win or nil) ~= nil
  end
  return (btv._wins or {})[win] ~= nil
end
api.nvim_win_is_valid = btv.win.is_valid

-- Message writers (aliases `nvim_err_writeln` / `nvim_err_write` / `nvim_out_write`).
-- Error writers route through `btv._echo_err`, which lands on the message line and
-- in `:messages` painted red (the core `echo_err` path); `out_write` funnels
-- through `print` like a plain message. `nvim_err_writeln`/`out_write` append a
-- newline; the `*_write` forms don't (the message line is line-oriented, so both
-- just emit the text).
function btv.err_writeln(msg)
  btv._echo_err(tostring(msg or ""))
end
function btv.err_write(msg)
  btv._echo_err(tostring(msg or ""))
end
function btv.out_write(msg)
  print(tostring(msg or ""))
end
api.nvim_err_writeln = btv.err_writeln
api.nvim_err_write = btv.err_write
api.nvim_out_write = btv.out_write

-- `nvim_win_get_position(win)`: the window's top-left as 0-based {row, col} screen
-- coordinates. Exact for a float (its placement); a tiled window's screen origin
-- isn't carried in the mirror, so it reports {0, 0} — a documented approximation
-- (a float-positioning plugin positions its own floats and reads their config
-- directly, so the value it cares about is exact). 0/nil is the current window.
function btv.win.position(win)
  win = (win == nil or win == 0) and (btv._cur_win or 1000) or win
  local f = ((btv._wins or {})[win] or {}).float
  if f then
    return { f.row or 0, f.col or 0 }
  end
  return { 0, 0 }
end
api.nvim_win_get_position = btv.win.position

-- `btv.buf.list([opts])` -> list of bufnr [alias `nvim_list_bufs`, which always lists
-- all]: the buffer handles the mirror knows, ascending. By default every buffer
-- across every layer (main area + all docks). Pass `{ focused = true }` to list only
-- the **focused** layer's buffers — the per-region list (`:ls` is scoped the same
-- way), so a dock reports just its own buffers and the main area just its own.
function btv.buf.list(opts)
  local focused_only = type(opts) == "table" and opts.focused == true
  local ids = {}
  for id, b in pairs(btv._bufs or {}) do
    if not focused_only or b.focused then
      ids[#ids + 1] = id
    end
  end
  table.sort(ids)
  return ids
end
-- `nvim_list_bufs` takes no arguments and lists *all* buffers; bind the default
-- (all-layers) behavior so a stray caller can never accidentally scope it.
function api.nvim_list_bufs()
  return btv.buf.list()
end

-- `btv.buf.alternate()` -> bufnr | nil: the **alternate buffer** (vim's `#`, the
-- `<C-^>` target), or nil when there is none. This is the handle, not the name:
-- `vim.fn.expand("#")` answers what `:e #` would reopen — a name that outlives a
-- `:bdelete` of the buffer it came from — while this answers which *open* buffer a
-- list flags `#` (what `:ls` and the `buffers` picker mark). A buffer that has been
-- closed is nobody's alternate here, so the two disagree exactly when they should.
function btv.buf.alternate()
  local b = btv._alt_buf or 0
  return b ~= 0 and btv._bufs[b] and b or nil
end

-- `btv.list_uis()` [alias `nvim_list_uis`]: the attached UIs. bemtvi drives one client
-- at a time, so this reports a single UI sized to the editor screen
-- (`vim.o.columns`/`lines`), with the fields a layout calculation reads. The `ext_*`
-- feature flags are all false (bemtvi's redraw protocol carries no external-UI
-- widgets).
function btv.list_uis()
  return {
    {
      width = vim.o.columns,
      height = vim.o.lines,
      rgb = true,
      ext_cmdline = false,
      ext_popupmenu = false,
      ext_tabline = false,
      ext_wildmenu = false,
      ext_messages = false,
      ext_linegrid = true,
      ext_multigrid = false,
      ext_hlstate = false,
      ext_termcolors = false,
      chan = 1,
    },
  }
end
api.nvim_list_uis = btv.list_uis

-- `nvim_cmd(cmd, opts)`: the structured ex-command form. bemtvi's command engine
-- consumes a string, so flatten {cmd, args, bang} into one and route through
-- `btv.cmd` — the body only adapts the table shape onto that funnel, so it carries
-- no implementation of its own (there is no structured btv twin; the canonical
-- bemtvi form is the string-taking `btv.cmd`). `cmd.mods` rides along as `btv.cmd`'s
-- modifier table, so `mods = { silent = true }` runs the command under `:silent`
-- (and `mods.emsg_silent` under `:silent!`); a modifier bemtvi doesn't dispatch
-- raises there rather than being silently dropped. `opts.output` capture isn't
-- modelled (returns `""`); the common callers
-- (`nvim_cmd{cmd='normal', args={...}, bang=true}`) only need the side effect.
function api.nvim_cmd(cmd, opts)
  local s = cmd.cmd
  if cmd.bang then
    s = s .. "!"
  end
  if cmd.args and #cmd.args > 0 then
    s = s .. " " .. table.concat(cmd.args, " ")
  end
  btv._assert_call_ctx("an ex-command (nvim_cmd)")
  btv.cmd(s, cmd.mods)
  if opts and opts.output then
    return ""
  end
end

-- `btv.buf.getline(buf, lnum[, end])` [alias `vim.fn.getbufline`]: lines `lnum..end`
-- (1-based inclusive) of a buffer, or just `lnum` when `end` is omitted. Wraps
-- `btv.buf.lines` (0-based, end-exclusive). An out-of-range request yields {} (vim).
function btv.buf.getline(buf, lnum, lend)
  lend = lend or lnum
  return api.nvim_buf_get_lines(btv._resolve_bufnr(buf), lnum - 1, lend, false)
end
vim.fn.getbufline = btv.buf.getline

-- `btv.win.getid([winnr[, tabnr]])` [alias `vim.fn.win_getid`]: the window id for a
-- 1-based window number (default: the current window). `tabnr` is accepted but only
-- the current tab's layout order is consulted (the global window mirror carries it).
function btv.win.getid(winnr, _tabnr)
  if winnr == nil or winnr == 0 then
    return btv._cur_win or 1000
  end
  return (btv._win_order or {})[winnr] or 0
end
vim.fn.win_getid = btv.win.getid

-- `btv.win.findbuf(bufnr)` [alias `vim.fn.win_findbuf`]: the ids of every window currently
-- displaying `bufnr`, in *any* tab (vim spans tabpages here).
function btv.win.findbuf(bufnr)
  local out = {}
  for _, id in ipairs(btv._win_all or {}) do
    local w = btv._wins[id]
    if w and w.buffer == bufnr then
      out[#out + 1] = id
    end
  end
  return out
end
vim.fn.win_findbuf = btv.win.findbuf

-- `btv.win.gettype([winid])` [alias `vim.fn.win_gettype`]: `"popup"` for a float, `""` for a
-- normal window — the distinction a plugin draws to know whether a window is one of
-- its own floats.
function btv.win.gettype(winid)
  winid = (winid == nil or winid == 0) and (btv._cur_win or 1000) or winid
  local w = (btv._wins or {})[winid]
  return (w and w.float) and "popup" or ""
end
vim.fn.win_gettype = btv.win.gettype

-- `btv.win.screenpos(winnr)` [alias `vim.fn.win_screenpos`]: the 1-based (row, col) screen
-- position of a window's top-left text cell. Known exactly for a float (its
-- placement); a tiled window's screen origin isn't carried in the mirror, so it
-- reports {1, 1} (top-left) — a documented approximation float-positioning plugins
-- tolerate (they position their own floats and read their config directly).
function btv.win.screenpos(winnr)
  local id = btv.win.getid(winnr)
  local w = (btv._wins or {})[id]
  if w and w.float then
    return { (w.float.row or 0) + 1, (w.float.col or 0) + 1 }
  end
  return { 1, 1 }
end
vim.fn.win_screenpos = btv.win.screenpos

-- `btv.wininfo.get([winid])` [alias `vim.fn.getwininfo`]: per-window info dicts (all
-- windows when winid is omitted). Carries the fields a layout reads —
-- winid/winnr/bufnr/width/height/tabnr — from the window mirror. INCOMPLETE:
-- topline/botline are coarse (the mirror has no per-window scroll), and winrow/wincol
-- use the float placement when present, else 1 (tiled origins aren't mirrored).
btv.wininfo = btv.wininfo or {}
function btv.wininfo.get(winid)
  local function info(id, winnr)
    local w = (btv._wins or {})[id] or {}
    local pos = btv.win.screenpos(winnr)
    return {
      winid = id,
      winnr = winnr,
      bufnr = w.buffer or 0,
      width = w.width or 0,
      height = w.height or 0,
      tabnr = btv._cur_tab or 1,
      winrow = pos[1],
      wincol = pos[2],
      topline = 1,
      botline = (w.height or 0),
      terminal = 0,
      quickfix = 0,
      loclist = 0,
      variables = {},
    }
  end
  if winid and winid ~= 0 then
    local idx = 0
    for i, id in ipairs(btv._win_order or {}) do
      if id == winid then
        idx = i
        break
      end
    end
    return idx > 0 and { info(winid, idx) } or {}
  end
  local out = {}
  for i, id in ipairs(btv._win_order or {}) do
    out[#out + 1] = info(id, i)
  end
  return out
end
vim.fn.getwininfo = btv.wininfo.get

-- `btv.screen.pos(win, lnum, col)` [alias `vim.fn.screenpos`]: the 1-based screen cell
-- {row, col, curscol, endcol} of buffer position [lnum, col] in window `win`
-- (0/current). A completion plugin reads it to anchor its completion menu at the cursor.
-- Computed from the window mirror's origin + scroll: row counts down from the top
-- text line; col is the display width of the line up to `col`, shifted by the
-- horizontal scroll. INCOMPLETE: inherits `btv.win.screenpos`'s tiled-origin
-- approximation ({1,1}) and does not add a number/sign textoff; curscol/endcol
-- collapse onto col. Faithful for the common single-window, gutterless case.
function btv.screen.pos(win, lnum, col)
  local id = (win == nil or win == 0) and (btv._cur_win or 1000) or win
  local winnr = 1
  for i, wid in ipairs(btv._win_order or {}) do
    if wid == id then
      winnr = i
      break
    end
  end
  local origin = btv.win.screenpos(winnr) -- {row, col}, 1-based
  local w = (btv._wins or {})[id] or {}
  local topline = w.topline or 1
  local leftcol = w.leftcol or 0
  local buf = w.buffer or btv._cur_buf or 0
  local line = (vim.api.nvim_buf_get_lines(buf, lnum - 1, lnum, false))[1] or ""
  local dcol = vim.fn.strdisplaywidth(string.sub(line, 1, math.max(0, col - 1)))
  local scol = origin[2] + dcol - leftcol
  return { row = origin[1] + (lnum - topline), col = scol, curscol = scol, endcol = scol }
end
vim.fn.screenpos = btv.screen.pos

btv.bufinfo = btv.bufinfo or {}
-- `btv.bufinfo.get([arg])` [alias `vim.fn.getbufinfo`]: per-buffer info dicts. `arg` is a
-- bufnr (one buffer), an opts table ({buflisted=1, bufloaded=1, …} — filters), or
-- absent (all buffers). bemtvi's core doesn't model `buflisted`, so every buffer
-- reports listed/loaded and the filters only narrow; `changed` / `changedtick` /
-- `lnum` are real (the buffer's modified flag, its change counter, and the cursor
-- line `:ls` reports for it).
function btv.bufinfo.get(arg)
  local function info(id, buf)
    local windows = btv.win.findbuf(id)
    return {
      bufnr = id,
      name = buf.name or "",
      changed = btv.bo[id].modified and 1 or 0,
      changedtick = buf.changedtick or 0,
      hidden = #windows == 0 and 1 or 0,
      listed = 1,
      loaded = 1,
      -- The buffer's last-known cursor line, from the mirror the core fills with the
      -- same value `:ls` prints as `line N` (live cursor for the current buffer, the
      -- position stashed on switch for any other) — not the placeholder `1` this used
      -- to report for every buffer.
      lnum = buf.lnum or 1,
      linecount = (buf.lines and #buf.lines) or 0,
      variables = {},
      windows = windows,
    }
  end
  if type(arg) == "number" then
    local buf = (btv._bufs or {})[arg]
    return buf and { info(arg, buf) } or {}
  end
  local opts = type(arg) == "table" and arg or {}
  local out = {}
  for id, buf in pairs(btv._bufs or {}) do
    -- `buflisted=1` filters nothing: every buffer is listed (no buftype model).
    local keep = true
    if opts.bufloaded == 1 and not buf.loaded then
      keep = false
    end
    if keep then
      out[#out + 1] = info(id, buf)
    end
  end
  table.sort(out, function(a, b)
    return a.bufnr < b.bufnr
  end)
  return out
end
vim.fn.getbufinfo = btv.bufinfo.get

-- `vim.fn.bufname(bufnr)`: the buffer's name — `btv.buf.name` already resolves 0/nil to
-- the current buffer, so this is a direct alias onto it.
vim.fn.bufname = btv.buf.name

-- `btv.buf.nr(expr)` [alias `vim.fn.bufnr`]: the buffer number for `expr`. `""` / `"%"` / nil
-- / 0 -> current buffer; `"#"` -> the alternate buffer (`-1` when there is none);
-- `"$"` -> the last (largest) buffer number; a string -> the
-- loaded buffer whose name matches (exact, else suffix), -1 when none. Backed by
-- the Phase-6 `btv._bufs` mirror.
function btv.buf.nr(expr)
  if expr == nil or expr == 0 or expr == "" or expr == "%" then
    return (btv._cur_buf or {}).bufnr or 0
  end
  if expr == "#" then
    return btv.buf.alternate() or -1
  end
  if expr == "$" then
    local max = 0
    for id in pairs(btv._bufs or {}) do
      if id > max then
        max = id
      end
    end
    return max
  end
  if type(expr) == "number" then
    return btv._bufs[expr] and expr or -1
  end
  -- An exactly-named buffer always wins; only when none exists does a suffix
  -- match apply (taking the lowest bufnr so ties resolve deterministically —
  -- `pairs` order must not decide which of two suffix matches is returned).
  local suffix
  for bufnr, buf in pairs(btv._bufs) do
    local name = buf.name or ""
    if name == expr then
      return bufnr
    end
    if name:sub(-#expr) == expr and (suffix == nil or bufnr < suffix) then
      suffix = bufnr
    end
  end
  return suffix or -1
end
vim.fn.bufnr = btv.buf.nr
