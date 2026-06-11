-- nxvim Lua prelude — plugin-compat surface.
-- The deprecated-but-still-called `nvim_*` aliases, the `vim.fn.*` window/match/
-- completion builtins, and the `vim.uv` extras a full plugin stack (telescope.nvim
-- + plenary.nvim) reaches for that the focused earlier chunks don't cover. Layered
-- over the real primitives those chunks installed (nvim_set_option_value,
-- nvim_buf_set_extmark, the buffer/window mirror), so each is a faithful adapter,
-- not a hollow stub. Loaded last (after api/fs/uv), so every surface it builds on
-- already exists. (See docs/architecture.md → Lua bridge.)

local vim = vim
local api = vim.api

-- ===== nvim_* deprecated aliases & small gaps ================================

-- nvim_buf_set_option / nvim_buf_get_option(buf, name[, value]): the pre-0.10
-- option accessors, deprecated in favor of nvim_set_option_value but still called
-- pervasively (telescope sets bufhidden/modifiable/filetype/buftype on every
-- scratch buffer). Route to the scoped accessor with the buffer fixed.
function api.nvim_buf_set_option(buf, name, value)
  api.nvim_set_option_value(name, value, { buf = buf })
end
function api.nvim_buf_get_option(buf, name) return api.nvim_get_option_value(name, { buf = buf }) end

-- nvim_set_option / nvim_get_option(name[, value]): the global-scope deprecated
-- forms. Route through vim.o, which canonicalizes the scope the name implies.
function api.nvim_set_option(name, value) vim.o[name] = value end
function api.nvim_get_option(name) return vim.o[name] end

-- nvim_win_is_valid(win): whether `win` names a window the mirror knows about
-- (0/nil is the current window, always valid while one exists). The window
-- analogue of nvim_buf_is_valid — picker teardown/resize guards call it constantly.
function api.nvim_win_is_valid(win)
  if win == nil or win == 0 then return (vim._cur_win or nil) ~= nil end
  return (vim._wins or {})[win] ~= nil
end

-- nvim_buf_set_text(buf, sr, sc, er, ec, repl): replace the (0-based, end-
-- exclusive) byte range with `repl` (a list of lines that may merge/split lines),
-- the character-precise sibling of nvim_buf_set_lines. Splices the affected lines
-- in Lua then routes the result through nvim_buf_set_lines, so the write-through
-- mirror + queued core edit stay consistent.
function api.nvim_buf_set_text(buf, sr, sc, er, ec, repl)
  local id = vim._resolve_bufnr(buf)
  local first = api.nvim_buf_get_lines(id, sr, sr + 1, false)[1] or ""
  local last = (er == sr) and first or (api.nvim_buf_get_lines(id, er, er + 1, false)[1] or "")
  local prefix, suffix = first:sub(1, sc), last:sub(ec + 1)
  local newlines = {}
  for i = 1, #repl do
    newlines[i] = repl[i]
  end
  if #newlines == 0 then newlines = { "" } end
  newlines[1] = prefix .. newlines[1]
  newlines[#newlines] = newlines[#newlines] .. suffix
  api.nvim_buf_set_lines(id, sr, er + 1, false, newlines)
end

-- nvim_buf_set_name(buf, name): name a buffer. INCOMPLETE: nxvim has no core
-- buffer-rename bridge, so this updates the snapshot mirror only (so a read-back
-- and a name-keyed nvim_buf_get_name agree) without re-associating the core
-- buffer's file. Enough for telescope giving its prompt/results scratch buffers a
-- display name; a real rename awaits a core BufOp.
function api.nvim_buf_set_name(buf, name)
  local id = vim._resolve_bufnr(buf)
  if (vim._bufs or {})[id] then vim._bufs[id].name = name end
  if vim._cur_buf and vim._cur_buf.bufnr == id then vim._cur_buf.name = name end
end

-- nvim_buf_add_highlight(buf, ns, hl_group, line, col_start, col_end): the legacy
-- single-line highlight call, implemented (as in neovim) over an extmark. `col_end
-- == -1` highlights to end of line. Returns the namespace used. This is telescope's
-- highest-frequency decoration call (match positions + the results list).
function api.nvim_buf_add_highlight(buf, ns, hl_group, line, col_start, col_end)
  local id = vim._resolve_bufnr(buf)
  local ec = col_end
  if ec == nil or ec < 0 then
    local ln = api.nvim_buf_get_lines(id, line, line + 1, false)[1] or ""
    ec = #ln
  end
  -- An ungrouped highlight (ns -1/0) still needs a real namespace to live in.
  local nsid = (ns == nil or ns == -1 or ns == 0) and api.nvim_create_namespace("") or ns
  api.nvim_buf_set_extmark(id, nsid, line, col_start or 0, {
    end_row = line,
    end_col = ec,
    hl_group = hl_group,
  })
  return nsid
end

-- Message writers. nxvim funnels editor messages through `print` (the message
-- line); error vs out is not visually distinguished yet, but the text is shown
-- rather than swallowed. nvim_err_writeln/out_write append a newline; the *_write
-- forms don't (the message line is line-oriented, so both just print).
function api.nvim_err_writeln(msg) print(tostring(msg or "")) end
function api.nvim_err_write(msg) print(tostring(msg or "")) end
function api.nvim_out_write(msg) print(tostring(msg or "")) end

-- nvim_call_function(name, args): invoke `vim.fn[name]` with the arg list — the
-- API-namespace bridge to Vimscript builtins. A function nxvim doesn't provide
-- fails loud (the no-silent-stub rule) naming itself.
function api.nvim_call_function(name, args)
  local f = vim.fn[name]
  if type(f) ~= "function" then vim._notimpl("vim.fn." .. tostring(name)) end
  local unpack = table.unpack or unpack
  return f(unpack(args or {}))
end

-- nvim_win_get_position(win): the window's top-left as 0-based {row, col} screen
-- coordinates. Exact for a float (its placement); a tiled window's screen origin
-- isn't carried in the mirror, so it reports {0, 0} — a documented approximation
-- (telescope positions its own floats and reads their config directly, so the
-- value it cares about is exact). 0/nil is the current window.
function api.nvim_win_get_position(win)
  win = (win == nil or win == 0) and (vim._cur_win or 1000) or win
  local f = ((vim._wins or {})[win] or {}).float
  if f then return { f.row or 0, f.col or 0 } end
  return { 0, 0 }
end

-- nvim_list_bufs(): every buffer handle the snapshot mirror knows, ascending.
function api.nvim_list_bufs()
  local ids = {}
  for id in pairs(vim._bufs or {}) do
    ids[#ids + 1] = id
  end
  table.sort(ids)
  return ids
end

-- nvim_list_uis(): the attached UIs. nxvim drives one client at a time, so this
-- reports a single UI sized to the editor screen (vim.o.columns/lines), with the
-- fields a layout calculation reads. The ext_* feature flags are all false
-- (nxvim's redraw protocol carries no external-UI widgets).
function api.nvim_list_uis()
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

-- nvim_cmd(cmd, opts): the structured ex-command form. nxvim's command engine
-- consumes a string, so flatten {cmd, args, bang} into one and route through
-- nvim_command. `opts.output` capture isn't modelled (returns ""); the common
-- callers (telescope `nvim_cmd{cmd='normal', args={...}, bang=true}`) only need
-- the side effect.
function api.nvim_cmd(cmd, opts)
  local s = cmd.cmd
  if cmd.bang then s = s .. "!" end
  if cmd.args and #cmd.args > 0 then s = s .. " " .. table.concat(cmd.args, " ") end
  api.nvim_command(s)
  if opts and opts.output then return "" end
end

-- nvim_clear_autocmds(opts): remove every autocmd matching the filter — the bulk
-- analogue of nvim_del_autocmd. `opts.event` (string/list), `opts.group` (id or
-- name), `opts.buffer`, and `opts.pattern` (string/list) all narrow the set; an
-- empty opts clears everything. Mirrors nvim_get_autocmds' matching.
function api.nvim_clear_autocmds(opts)
  opts = opts or {}
  local want_events = opts.event and (type(opts.event) == "table" and opts.event or { opts.event })
  local want_group = opts.group
  if type(want_group) == "string" then want_group = vim._augroups[want_group] end
  local want_pats = opts.pattern
    and (type(opts.pattern) == "table" and opts.pattern or { opts.pattern })
  vim._autocmds = vim.tbl_filter(function(au)
    if want_events then
      local evs = type(au.event) == "table" and au.event or { au.event }
      local hit = false
      for _, w in ipairs(want_events) do
        if vim.tbl_contains(evs, w) then
          hit = true
          break
        end
      end
      if not hit then return true end -- keep: event doesn't match the filter
    end
    if want_group ~= nil and au.group ~= want_group then return true end
    if opts.buffer ~= nil and au.buffer ~= opts.buffer then return true end
    if want_pats then
      local pat = au.opts.pattern
      if not vim.tbl_contains(want_pats, pat) then return true end
    end
    return false -- drop: every given filter matched
  end, vim._autocmds)
end

-- nvim_get_commands(opts) / nvim_buf_get_commands(buf, opts): the user-command
-- registry as neovim's introspection map (name -> definition record). nxvim's
-- registry stores only the command body, so the record carries `name`/`definition`
-- with permissive defaults for the rest — enough for telescope's `:commands`
-- picker to list and run them. `nvim_get_commands` returns the globals;
-- `nvim_buf_get_commands(buf)` returns the buffer-local commands for `buf`
-- (0 = current), matching neovim's split.
local function command_record(name, def)
  return {
    name = name,
    definition = type(def) == "string" and def or "",
    nargs = "*",
    bang = false,
    bar = false,
    register = false,
    complete = nil,
    range = nil,
  }
end
local function commands_map(registry)
  local out = {}
  for name, def in pairs(registry or {}) do
    out[name] = command_record(name, def)
  end
  return out
end
function api.nvim_get_commands(_opts) return commands_map(vim._user_commands) end
function api.nvim_buf_get_commands(buf, _opts)
  if buf == nil or buf == 0 then buf = vim._cur_buf and vim._cur_buf.bufnr or 0 end
  return commands_map((vim._buf_user_commands or {})[buf])
end

-- ----- nvim_buf_attach: the buffer-change callback channel --------------------
-- telescope drives its prompt filtering off `on_lines`: it attaches to the prompt
-- buffer, and every time the typed query changes the callback re-runs the finder.
-- The registry lives here keyed by bufnr; the server calls `vim._buf_changed`
-- (from push_buf_mirror) when an attached buffer's changedtick bumps, and
-- `nvim_buf_delete` fires `on_detach`.
vim._buf_attached = vim._buf_attached or {}

-- nvim_buf_attach(buf, send_buffer, opts): register `opts`' callbacks (on_lines /
-- on_bytes / on_detach / on_reload) for `buf`. `send_buffer` is accepted and
-- ignored (a callback reads content via nvim_buf_get_lines, so the initial
-- buffer-text payload neovim sends with send_buffer=true isn't needed). Returns
-- true. Multiple attaches stack, matching neovim.
function api.nvim_buf_attach(buf, _send_buffer, opts)
  local id = vim._resolve_bufnr(buf)
  vim._buf_attached[id] = vim._buf_attached[id] or {}
  table.insert(vim._buf_attached[id], opts or {})
  return true
end

-- vim._buf_changed(buf, tick, first, last, new_last): invoke each attached
-- on_lines callback with neovim's argument tuple ("lines", buf, changedtick,
-- firstline, lastline, new_lastline, byte_count). A callback that returns true (or
-- errors) is detached, its on_detach run — matching neovim's contract. Called by
-- the server when the buffer's text changed; `first`/`last`/`new_last` are coarse
-- (whole-buffer) since the change range isn't diffed, which the dominant consumers
-- (telescope's prompt on_lines, which re-reads the buffer) don't depend on.
function vim._buf_changed(buf, tick, first, last, new_last)
  local list = vim._buf_attached[buf]
  if not list then return end
  local survivors = {}
  for _, cbs in ipairs(list) do
    local detach = false
    if cbs.on_lines then
      local ok, ret = pcall(cbs.on_lines, "lines", buf, tick, first, last, new_last, 0)
      if not ok then
        vim.notify("nxvim: nvim_buf_attach on_lines errored and was detached: " .. tostring(ret))
        detach = true
      elseif ret == true then
        detach = true
      end
    end
    if detach then
      if cbs.on_detach then pcall(cbs.on_detach, "detach", buf) end
    else
      survivors[#survivors + 1] = cbs
    end
  end
  vim._buf_attached[buf] = (#survivors > 0) and survivors or nil
end

-- vim._buf_bytes_changed(buf, tick, start_row, start_col, start_byte, old_row,
-- old_col, old_byte, new_row, new_col, new_byte): invoke each attached on_bytes
-- callback with neovim's on_bytes argument tuple (the "bytes" event name, the
-- bufnr/changedtick, then the relative row/col/byte deltas). The dominant consumer
-- is the vendored vim.treesitter LanguageTree, whose on_bytes edits its trees so
-- the next :parse() reparses incrementally. Detach-on-error / detach-on-true match
-- neovim, exactly as vim._buf_changed does for on_lines. The server fires this (for
-- every edit since the last frame, in order) before any plugin Lua runs.
function vim._buf_bytes_changed(
  buf,
  tick,
  start_row,
  start_col,
  start_byte,
  old_row,
  old_col,
  old_byte,
  new_row,
  new_col,
  new_byte
)
  local list = vim._buf_attached[buf]
  if not list then return end
  local survivors = {}
  for _, cbs in ipairs(list) do
    local detach = false
    if cbs.on_bytes then
      local ok, ret = pcall(
        cbs.on_bytes,
        "bytes",
        buf,
        tick,
        start_row,
        start_col,
        start_byte,
        old_row,
        old_col,
        old_byte,
        new_row,
        new_col,
        new_byte
      )
      if not ok then
        vim.notify("nxvim: nvim_buf_attach on_bytes errored and was detached: " .. tostring(ret))
        detach = true
      elseif ret == true then
        detach = true
      end
    end
    if detach then
      if cbs.on_detach then pcall(cbs.on_detach, "detach", buf) end
    else
      survivors[#survivors + 1] = cbs
    end
  end
  vim._buf_attached[buf] = (#survivors > 0) and survivors or nil
end

-- vim._buf_reloaded(buf): invoke each attached on_reload callback ("reload", buf) —
-- fired when the whole rope was replaced (undo/redo, :e), where byte deltas are
-- meaningless. The treesitter LanguageTree's on_reload invalidates its tree so the
-- next :parse() is a full reparse of the current snapshot.
function vim._buf_reloaded(buf)
  local list = vim._buf_attached[buf]
  if not list then return end
  local survivors = {}
  for _, cbs in ipairs(list) do
    local detach = false
    if cbs.on_reload then
      local ok, ret = pcall(cbs.on_reload, "reload", buf)
      if not ok then
        vim.notify("nxvim: nvim_buf_attach on_reload errored and was detached: " .. tostring(ret))
        detach = true
      elseif ret == true then
        detach = true
      end
    end
    if detach then
      if cbs.on_detach then pcall(cbs.on_detach, "detach", buf) end
    else
      survivors[#survivors + 1] = cbs
    end
  end
  vim._buf_attached[buf] = (#survivors > 0) and survivors or nil
end

-- Fire on_detach for every callback attached to `buf` and clear them — run when
-- the buffer is deleted (below).
function vim._fire_on_detach(buf)
  local list = vim._buf_attached[buf]
  if not list then return end
  for _, cbs in ipairs(list) do
    if cbs.on_detach then pcall(cbs.on_detach, "detach", buf) end
  end
  vim._buf_attached[buf] = nil
end

-- Wrap nvim_buf_delete so a deleted buffer's on_detach callbacks fire (telescope's
-- picker teardown relies on the prompt buffer's on_detach to clean up state).
local raw_buf_delete = api.nvim_buf_delete
function api.nvim_buf_delete(buffer, opts)
  vim._fire_on_detach(vim._resolve_bufnr(buffer))
  return raw_buf_delete(buffer, opts)
end

-- ===== vim.fn — window / position / match / completion builtins =============

local fn = vim.fn

-- vim.fn.getenv(name): an environment variable's value, or v:null (vim.NIL) when
-- unset — matching neovim, which returns v:null rather than an empty string so a
-- caller can distinguish "" from absent. A name set via vim.fn.setenv this session
-- is read back from the shadow store first.
vim._env_shadow = vim._env_shadow or {}
function fn.getenv(name)
  local v = vim._env_shadow[name]
  if v ~= nil then return v end
  v = os.getenv(name)
  if v == nil then return vim.NIL end
  return v
end

-- vim.fn.setenv(name, value): set an environment variable for this session.
-- nxvim can't mutate the real process environment from Lua, so the value lands in
-- a shadow store getenv/vim.env read back — observable within the editor, which is
-- what a plugin setting e.g. $GIT_DIR before spawning a child expects when the
-- spawn also runs in-process. A nil/v:null value unsets it.
function fn.setenv(name, value)
  if value == nil or value == vim.NIL then
    vim._env_shadow[name] = nil
  else
    vim._env_shadow[name] = tostring(value)
  end
end

-- vim.fn.fnameescape(name): escape a filename for use in an ex command, backslash-
-- escaping the characters vim treats specially there. Used before `:edit <file>`.
function fn.fnameescape(name)
  return (tostring(name):gsub("[ \t\n*?%[%]{}`$\\%%#'\"|!<>();&]", "\\%0"))
end

-- vim.fn.shellescape(str[, special]): quote `str` for the shell. Single-quote
-- wrapping with embedded quotes escaped; `special` additionally backslash-escapes
-- `!` and `%`/`#` (vim's cmdline specials), which telescope passes when building a
-- grep command.
function fn.shellescape(str, special)
  str = tostring(str)
  local escaped = "'" .. str:gsub("'", "'\\''") .. "'"
  if special then escaped = escaped:gsub("[!%%#]", "\\%0") end
  return escaped
end

-- vim.fn.prompt_setprompt(buf, text) / prompt_getprompt(buf): the prefix of a
-- |prompt-buffer|. nxvim has no `buftype=prompt` in the core, so this EMULATES the
-- prompt-buffer prefix by writing `text` as the start of line 0 — which is what a
-- real prompt buffer's first line contains, and what telescope reads back (its
-- `_get_prompt` strips `#prompt_prefix` bytes, and its prefix highlight paints
-- columns [0, #prefix)). The previously-set prefix is stripped first so repeated
-- calls don't stack. INCOMPLETE vs a true prompt buffer: the prefix is ordinary
-- (editable) text rather than a protected region — telescope keeps the cursor past
-- it, so typing/backspace behave correctly in practice.
vim._prompt_prefix = vim._prompt_prefix or {}
function fn.prompt_setprompt(buf, text)
  local id = vim._resolve_bufnr(buf)
  local old = vim._prompt_prefix[id] or ""
  local new = tostring(text or "")
  vim._prompt_prefix[id] = new
  local line0 = api.nvim_buf_get_lines(id, 0, 1, false)[1] or ""
  if old ~= "" and line0:sub(1, #old) == old then line0 = line0:sub(#old + 1) end
  api.nvim_buf_set_lines(id, 0, 1, false, { new .. line0 })
end
function fn.prompt_getprompt(buf) return vim._prompt_prefix[vim._resolve_bufnr(buf)] or "" end

-- vim.fn.pumvisible(): whether the insert-mode completion popup is showing.
-- nxvim doesn't surface the popup-menu state to Lua, so this is truthfully 0 in
-- the contexts telescope checks it (its prompt has no ins-completion menu) — an
-- honest "not visible", not a faked value.
function fn.pumvisible() return 0 end

-- vim.fn.getbufline(buf, lnum[, end]): lines `lnum..end` (1-based inclusive) of a
-- buffer, or just `lnum` when `end` is omitted. Wraps nvim_buf_get_lines (0-based,
-- end-exclusive). An out-of-range request yields {} (vim), not an error.
function fn.getbufline(buf, lnum, lend)
  lend = lend or lnum
  return api.nvim_buf_get_lines(vim._resolve_bufnr(buf), lnum - 1, lend, false)
end

-- vim.fn.win_getid([winnr[, tabnr]]): the window id for a 1-based window number
-- (default: the current window). `tabnr` is accepted but only the current tab's
-- layout order is consulted (the global window mirror carries it).
function fn.win_getid(winnr, _tabnr)
  if winnr == nil or winnr == 0 then return vim._cur_win or 1000 end
  return (vim._win_order or {})[winnr] or 0
end

-- vim.fn.getjumplist([winnr [, tabnr]]): the window's jumplist as
-- `{ list, curidx }`. `list` is an array of `{ bufnr, lnum, col, coladd }` dicts
-- oldest-first (lnum 1-based, col 0-based byte); `curidx` is the navigation
-- pointer `<C-o>`/`<C-i>` walk — a 0-based index into `list`, equal to `#list`
-- when sitting at the present (not navigating). `winnr` is a window-ID or a
-- 1-based window number (default: the current window). `tabnr` is accepted but
-- only the current tab's windows are mirrored, so an off-tab window yields
-- `{ {}, 0 }`. Reads the window mirror the server pushes (`vim._wins`).
function fn.getjumplist(winnr, _tabnr)
  local id
  if winnr == nil or winnr == 0 then
    id = vim._cur_win or 1000
  elseif (vim._wins or {})[winnr] then
    id = winnr -- already a window-ID
  else
    id = (vim._win_order or {})[winnr] or 0
  end
  local w = (vim._wins or {})[id]
  if not w then return { {}, 0 } end
  local list = {}
  for _, e in ipairs(w.jumps or {}) do
    list[#list + 1] = { bufnr = e.bufnr, lnum = e.lnum, col = e.col, coladd = e.coladd or 0 }
  end
  return { list, w.jump_idx or #list }
end

-- vim.fn.win_gotoid(id): focus window `id`; returns 1 on success, 0 if unknown.
function fn.win_gotoid(id)
  if not (vim._wins or {})[id] then return 0 end
  api.nvim_set_current_win(id)
  return 1
end

-- vim.fn.win_findbuf(bufnr): the ids of every window currently displaying `bufnr`.
function fn.win_findbuf(bufnr)
  local out = {}
  for _, id in ipairs(vim._win_order or {}) do
    local w = vim._wins[id]
    if w and w.buffer == bufnr then out[#out + 1] = id end
  end
  return out
end

-- vim.fn.win_gettype([winid]): "popup" for a float, "" for a normal window — the
-- distinction telescope draws to know whether a window is one of its own floats.
function fn.win_gettype(winid)
  winid = (winid == nil or winid == 0) and (vim._cur_win or 1000) or winid
  local w = (vim._wins or {})[winid]
  return (w and w.float) and "popup" or ""
end

-- vim.fn.win_screenpos(winnr): the 1-based (row, col) screen position of a
-- window's top-left text cell. Known exactly for a float (its placement); a tiled
-- window's screen origin isn't carried in the mirror, so it reports {1, 1}
-- (top-left) — a documented approximation telescope tolerates (it positions its
-- own floats and reads their config directly).
function fn.win_screenpos(winnr)
  local id = fn.win_getid(winnr)
  local w = (vim._wins or {})[id]
  if w and w.float then return { (w.float.row or 0) + 1, (w.float.col or 0) + 1 } end
  return { 1, 1 }
end

-- vim.fn.getwininfo([winid]): per-window info dicts (all windows when winid is
-- omitted). Carries the fields a layout reads — winid/winnr/bufnr/width/height/
-- tabnr — from the window mirror. INCOMPLETE: topline/botline are coarse (the
-- mirror has no per-window scroll), and winrow/wincol use the float placement when
-- present, else 1 (tiled origins aren't mirrored).
function fn.getwininfo(winid)
  local function info(id, winnr)
    local w = (vim._wins or {})[id] or {}
    local pos = fn.win_screenpos(winnr)
    return {
      winid = id,
      winnr = winnr,
      bufnr = w.buffer or 0,
      width = w.width or 0,
      height = w.height or 0,
      tabnr = vim._cur_tab or 1,
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
    for i, id in ipairs(vim._win_order or {}) do
      if id == winid then
        idx = i
        break
      end
    end
    return idx > 0 and { info(winid, idx) } or {}
  end
  local out = {}
  for i, id in ipairs(vim._win_order or {}) do
    out[#out + 1] = info(id, i)
  end
  return out
end

-- vim.fn.screenpos(win, lnum, col): the 1-based screen cell {row, col, curscol,
-- endcol} of buffer position [lnum, col] in window `win` (0/current). nvim-cmp
-- reads it to anchor its completion menu at the cursor. Computed from the window
-- mirror's origin + scroll: row counts down from the top text line; col is the
-- display width of the line up to `col`, shifted by the horizontal scroll.
-- INCOMPLETE: inherits win_screenpos's tiled-origin approximation ({1,1} when the
-- mirror has no real screen origin) and does not add a number/sign textoff (the
-- gutter width is client-side, not in the server's text geometry — see the
-- diagnostics sign-column note in known-approximations). curscol/endcol collapse
-- onto col (no multicell-char straddle modelled). Faithful for the common
-- single-window, gutterless, unscrolled case cmp positions against.
function fn.screenpos(win, lnum, col)
  local id = (win == nil or win == 0) and (vim._cur_win or 1000) or win
  local winnr = 1
  for i, wid in ipairs(vim._win_order or {}) do
    if wid == id then
      winnr = i
      break
    end
  end
  local origin = fn.win_screenpos(winnr) -- {row, col}, 1-based
  local w = (vim._wins or {})[id] or {}
  local topline = w.topline or 1
  local leftcol = w.leftcol or 0
  local buf = w.buffer or vim._cur_buf or 0
  local line = (vim.api.nvim_buf_get_lines(buf, lnum - 1, lnum, false))[1] or ""
  local dcol = vim.fn.strdisplaywidth(string.sub(line, 1, math.max(0, col - 1)))
  local scol = origin[2] + dcol - leftcol
  return { row = origin[1] + (lnum - topline), col = scol, curscol = scol, endcol = scol }
end

-- vim.fn.getbufinfo([arg]): per-buffer info dicts. `arg` is a bufnr (one buffer),
-- an opts table ({buflisted=1, bufloaded=1, …} — filters), or absent (all
-- buffers). nxvim's core models neither buflisted nor a changed flag yet, so every
-- buffer reports listed/loaded and unchanged; the filters narrow accordingly.
function fn.getbufinfo(arg)
  local function info(id, buf)
    local windows = fn.win_findbuf(id)
    return {
      bufnr = id,
      name = buf.name or "",
      changed = 0,
      changedtick = 0,
      hidden = #windows == 0 and 1 or 0,
      listed = 1,
      loaded = 1,
      lnum = 1,
      linecount = (buf.lines and #buf.lines) or 0,
      variables = {},
      windows = windows,
    }
  end
  if type(arg) == "number" then
    local buf = (vim._bufs or {})[arg]
    return buf and { info(arg, buf) } or {}
  end
  local opts = type(arg) == "table" and arg or {}
  local out = {}
  for id, buf in pairs(vim._bufs or {}) do
    local keep = true
    if opts.buflisted == 1 then keep = true end -- every buffer is listed (no buftype model)
    if opts.bufloaded == 1 and not buf.loaded then keep = false end
    if keep then out[#out + 1] = info(id, buf) end
  end
  table.sort(out, function(a, b) return a.bufnr < b.bufnr end)
  return out
end

-- vim.fn.getpos(expr): a position as `{bufnr, lnum, col, off}` (1-based lnum/col).
-- "." is the cursor; "'<" / "'>" are the visual-selection corners — nxvim doesn't
-- mirror those marks to vim.fn yet, so they fall back to the cursor (telescope's
-- grep-from-selection then greps the cursor word, a graceful degradation rather
-- than an error). Backs telescope's visual-selection range read.
function fn.getpos(expr)
  local c = vim._cur_cursor or { row = 1, col = 0 }
  if expr == "." or expr == "'<" or expr == "'>" or expr == "v" then
    return { 0, c.row, c.col + 1, 0 }
  end
  return { 0, 0, 0, 0 }
end

-- vim.fn.setpos(expr, pos): move the cursor when `expr` is "." (the only settable
-- position nxvim models); `pos` is `{bufnr, lnum, col, off}`. Other marks are
-- accepted but not stored (no writable-mark mirror), returning 0 either way.
function fn.setpos(expr, pos)
  if expr == "." then api.nvim_win_set_cursor(0, { pos[2], math.max(0, (pos[3] or 1) - 1) }) end
  return 0
end

-- (vim.fn.winsaveview / winrestview are provided by an earlier prelude chunk,
-- which restores the scroll offsets from the window mirror's topline/leftcol — a
-- faithful round-trip — so they are deliberately NOT redefined here.)

-- ----- match highlighting (matchadd family) ----------------------------------
-- A per-window registry of match-highlight requests. INCOMPLETE: the registry is
-- faithful (ids are allocated, stored, and removable, and getmatches reflects it),
-- but nxvim does not yet RENDER these matches — there is no `:match`/`matchadd`
-- decoration path in the core. telescope uses it to tint the searched term inside
-- a previewer; the preview content is correct, the term is just not yet tinted.
-- This is the documented-approximation pattern (observable state, rendering TBD),
-- chosen over a loud failure so the previewer runs rather than erroring.
vim._matches = vim._matches or {}
vim._match_seq = vim._match_seq or 0
local function match_store(win)
  win = (win == nil or win == 0) and (vim._cur_win or 1000) or win
  vim._matches[win] = vim._matches[win] or {}
  return vim._matches[win]
end
function fn.matchadd(group, pattern, priority, id, opts)
  vim._match_seq = vim._match_seq + 1
  local mid = (id and id ~= -1) and id or vim._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pattern = pattern, priority = priority or 10, id = mid }
  return mid
end
function fn.matchaddpos(group, pos, priority, id, opts)
  vim._match_seq = vim._match_seq + 1
  local mid = (id and id ~= -1) and id or vim._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pos = pos, priority = priority or 10, id = mid }
  return mid
end
function fn.matchdelete(id, win)
  local store = match_store(win)
  local existed = store[id] ~= nil
  store[id] = nil
  return existed and 0 or -1
end
function fn.clearmatches(win)
  vim._matches[(win == nil or win == 0) and (vim._cur_win or 1000) or win] = {}
  return 0
end
function fn.getmatches(win)
  local out = {}
  for _, m in pairs(match_store(win)) do
    out[#out + 1] = m
  end
  table.sort(out, function(a, b) return a.id < b.id end)
  return out
end

-- vim.fn.getcompletion(pat, type[, filtered]): completion candidates of a kind.
-- Implemented for the kinds with a real source in nxvim: file/dir (glob), command
-- (the user-command registry), buffer (open buffer names), filetype (a fixed set
-- isn't tracked, so it returns matches among open buffers' filetypes). An
-- unsupported `type` fails loud rather than returning a misleading empty list.
function fn.getcompletion(pat, ctype, _filtered)
  pat = tostring(pat or "")
  if ctype == "file" or ctype == "file_in_path" or ctype == "dir" then
    local hits = fn.glob(pat .. "*", false, true)
    if type(hits) == "string" then hits = vim.split(hits, "\n", { trimempty = true }) end
    if ctype == "dir" then
      hits = vim.tbl_filter(function(p) return fn.isdirectory(p) == 1 end, hits)
    end
    return hits
  elseif ctype == "command" then
    -- Both the globals and the current buffer's local commands are reachable on
    -- the command line, so completion offers both (a name in both registries is
    -- listed once).
    local seen, out = {}, {}
    local function offer(name)
      if not seen[name] and name:sub(1, #pat) == pat then
        seen[name] = true
        out[#out + 1] = name
      end
    end
    local cur = vim._cur_buf and vim._cur_buf.bufnr or 0
    for name in pairs((vim._buf_user_commands or {})[cur] or {}) do
      offer(name)
    end
    for name in pairs(vim._user_commands or {}) do
      offer(name)
    end
    table.sort(out)
    return out
  elseif ctype == "buffer" then
    local out = {}
    for _, buf in pairs(vim._bufs or {}) do
      local name = buf.name or ""
      if name ~= "" and name:find(pat, 1, true) then out[#out + 1] = name end
    end
    table.sort(out)
    return out
  elseif ctype == "color" then
    -- Colorscheme names: the basenames of `colors/*.{lua,vim}` across the
    -- runtimepath, prefix-filtered, deduped (the same name in two rtp dirs lists
    -- once). lazy.nvim's loader probes this to skip re-loading an already-available
    -- colorscheme before searching its managed plugins for the file.
    local seen, out = {}, {}
    for _, ext in ipairs({ "lua", "vim" }) do
      for _, path in ipairs(vim.api.nvim_get_runtime_file("colors/*." .. ext, true)) do
        local name = path:match("([^/]+)%." .. ext .. "$")
        if name and not seen[name] and name:sub(1, #pat) == pat then
          seen[name] = true
          out[#out + 1] = name
        end
      end
    end
    table.sort(out)
    return out
  end
  vim._notimpl("vim.fn.getcompletion(type='" .. tostring(ctype) .. "')")
end

-- vim.fn.getqflist(what) / getloclist(win, what): the quickfix / location lists.
-- nxvim has no quickfix stack yet, so these report an empty list — an honest "no
-- entries", which is the correct answer whenever nothing has populated them (and
-- vim.fn.setqflist raises rather than pretending to fill one). With a `what`
-- request they return the dict shape neovim does, all fields empty/zero.
local function empty_list_query(what)
  if type(what) == "table" then
    local out = {}
    if what.items ~= nil then out.items = {} end
    if what.title ~= nil then out.title = "" end
    if what.nr ~= nil then out.nr = 0 end
    if what.size ~= nil then out.size = 0 end
    if what.winid ~= nil then out.winid = 0 end
    if what.context ~= nil then out.context = vim.NIL end
    if what.id ~= nil then out.id = 0 end
    return out
  end
  return {}
end
function fn.getqflist(what) return empty_list_query(what) end
function fn.getloclist(_win, what) return empty_list_query(what) end

-- vim.fn.expand(expr[, nosuf, list]): superset of the snapshot-backed `%` form
-- (defined in prelude/fs.lua) that telescope/plenary also drive with cursor
-- keywords, `~`/`$ENV` paths, and wildcards. Resolution order:
--   * `%`, `%:<mods>`         — the current file (delegated to the fs.lua impl)
--   * `<cword>` / `<cWORD>`   — the (WORD) under the cursor
--   * `<cfile>`               — the path-like token under the cursor
--   * a `:<mods>` suffix on any of the cursor keywords routes through fnamemodify
--   * leading `~` / `$VAR`    — home / environment expansion
--   * a wildcard (`*`/`?`)    — glob (returns a list when `list` is truthy)
--   * anything else           — the path with ~/$ expanded, returned verbatim
-- This replaces the fs.lua definition (loaded earlier), keeping its `%` behavior.
local expand_pct = fn.expand
local function cursor_word(big)
  local c = vim._cur_cursor or { row = 1, col = 0 }
  local buf = vim._bufs and vim._bufs[vim._resolve_bufnr(0)]
  local line = (buf and buf.lines and buf.lines[c.row]) or ""
  local col = (c.col or 0) + 1 -- 1-based byte index of the cursor
  -- `<cword>` is a run of keyword chars (word + underscore); `<cWORD>` is a run of
  -- non-blanks. Scan left and right from the cursor over the matching class.
  local class = big and "%S" or "[%w_]"
  if col > #line then col = #line end
  if col < 1 then return "" end
  if line:sub(col, col):match(class) == nil then
    -- Cursor not on the class: vim scans forward to the next match on the line.
    local s = line:find(class, col)
    if not s then return "" end
    col = s
  end
  local b = col
  while b > 1 and line:sub(b - 1, b - 1):match(class) do
    b = b - 1
  end
  local e = col
  while e < #line and line:sub(e + 1, e + 1):match(class) do
    e = e + 1
  end
  return line:sub(b, e)
end
local function expand_path(p)
  if p:sub(1, 1) == "~" then p = (os.getenv("HOME") or "") .. p:sub(2) end
  p = p:gsub("%$([%w_]+)", function(v) return os.getenv(v) or ("$" .. v) end)
  return p
end
function fn.expand(expr, nosuf, list)
  expr = tostring(expr)
  -- `%`-family: keep the existing snapshot-backed behavior verbatim.
  if expr == "%" or expr:match("^%%:") then return expand_pct(expr, nosuf, list) end
  -- Cursor keywords, with an optional `:mods` filename-modifier suffix.
  local kw, mods = expr:match("^(<c%a+>)(.*)$")
  if kw then
    local word
    if kw == "<cword>" then
      word = cursor_word(false)
    elseif kw == "<cWORD>" or kw == "<cfile>" then
      word = cursor_word(true)
    else
      word = ""
    end
    if mods ~= "" then word = fn.fnamemodify(word, mods) end
    return word
  end
  -- Wildcard expansion → glob.
  if expr:find("[*?]") then return fn.glob(expand_path(expr), nosuf, list) end
  -- A plain string: home / env expansion, returned verbatim (vim's behavior for a
  -- path with no wildcards).
  return expand_path(expr)
end

-- ===== vim.uv extras =========================================================

local uv = vim.uv -- == vim.loop

-- uv.os_getenv(name): an environment variable's value, or nil. The luv accessor
-- plenary uses where vim.fn.getenv isn't reached.
if uv.os_getenv == nil then
  function uv.os_getenv(name) return os.getenv(name) end
end

-- uv.os_environ(): the whole environment as a { NAME = value } map. luv exposes it;
-- nxvim has no enumerate-env host primitive, so it returns the shadow overlay only
-- (entries set via vim.fn.setenv this session). INCOMPLETE: pre-existing process
-- env vars aren't enumerated — read a specific one via uv.os_getenv instead.
if uv.os_environ == nil then
  function uv.os_environ()
    local out = {}
    for k, v in pairs(vim._env_shadow or {}) do
      out[k] = v
    end
    return out
  end
end
