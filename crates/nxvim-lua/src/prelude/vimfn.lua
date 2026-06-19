-- nxvim Lua prelude — vim.fn editor-query builtins.
-- The Vimscript builtins that read live editor state through the state mirror:
-- nx.line / nx.col / nx.expand / nx.localtime / nx.undotree, and the
-- nx.pum / nx.pos / nx.match / nx.jumplist registries that plugins query. Each is
-- authored as an nx.* noun with its vim.fn.* alias.
local vim = vim
local fn = vim.fn
nx = nx or {}

-- nx.line(expr) [alias vim.fn.line]: a buffer line number. "." is the cursor line
-- (1-based), "$" the last line (the line count). The window-relative forms
-- ("w0"/"w$") need the scroll position, which the mirror doesn't carry yet, so they
-- error loud.
function nx.line(expr)
  if expr == "." then
    return (nx._cur_cursor or {}).row or 1
  elseif expr == "$" then
    local buf = nx._bufs[nx._resolve_bufnr(0)]
    return (buf and buf.lines) and #buf.lines or 1
  end
  error("line(): unsupported expression '" .. tostring(expr) .. "'", 2)
end
vim.fn.line = nx.line

-- nx.col(expr) [alias vim.fn.col]: a byte column (1-based). "." is the cursor
-- column, "$" one past the end of the cursor line (its byte length + 1), matching vim.
function nx.col(expr)
  if expr == "." then
    return ((nx._cur_cursor or {}).col or 0) + 1
  elseif expr == "$" then
    local buf = nx._bufs[nx._resolve_bufnr(0)]
    local row = (nx._cur_cursor or {}).row or 1
    local ln = (buf and buf.lines) and buf.lines[row] or ""
    return #ln + 1
  end
  error("col(): unsupported expression '" .. tostring(expr) .. "'", 2)
end
vim.fn.col = nx.col

-- nx.localtime() [alias vim.fn.localtime]: the current time in seconds. nxvim
-- sources this from a MONOTONIC clock (the server's `nx._mono_secs`, the same base
-- stamped onto undo nodes), not wall-clock unix epoch, so `localtime() - node.time`
-- elapsed math (e.g. the undotree visualizer's "N minutes ago") stays correct and
-- non-negative across NTP steps and manual clock changes. Only differences matter.
function nx.localtime()
  return nx._mono_secs or 0
end
vim.fn.localtime = nx.localtime

-- nx.undotree.get([bufnr]) [alias vim.fn.undotree]: the buffer's undo tree, in neovim's shape
-- ({ seq_last, seq_cur, save_last, save_cur, time_cur, synced, entries }, each
-- entry { seq, time, save?, alt? }). Reads the `nx._undotree` mirror the server
-- projects from the core's branching history before each Lua entry; `bufnr`
-- 0/nil is the current buffer. A buffer with no recorded history yet yields an
-- empty-`entries` tree rather than erroring.
nx.undotree = nx.undotree or {}
function nx.undotree.get(bufnr)
  bufnr = nx._resolve_bufnr(bufnr)
  local t = (nx._undotree or {})[bufnr]
  if t == nil then
    return {
      synced = 1,
      seq_last = 0,
      seq_cur = 0,
      save_last = 0,
      save_cur = 0,
      time_cur = 0,
      entries = {},
    }
  end
  return t
end
vim.fn.undotree = nx.undotree.get

-- (vim.fn.fnamemodify lives in prelude/fs.lua, alongside the other path vim.fn;
-- this chunk's expand routes through it at call time.)

-- nx.expand(expr) [alias vim.fn.expand]: the `%` (current file) forms autocmd
-- callbacks and statuslines use to resolve paths, backed by the current-buffer
-- snapshot. `%` is the stored name; `%:<mods>` routes through fnamemodify (so `%:t`,
-- `%:p`, `%:h`, `%:r`, `%:~:.`, … all work). A non-`%` expression errors loud.
-- (the override below extends this with cursor keywords / globs, re-binding nx.expand.)
function nx.expand(expr)
  local name = (nx._cur_buf or {}).name or ""
  if expr == "%" then
    return name
  end
  local mods = expr:match("^%%(:.*)$")
  if mods then
    return vim.fn.fnamemodify(name, mods)
  end
  error("expand(): unsupported expression '" .. tostring(expr) .. "'", 2)
end
vim.fn.expand = nx.expand

-- nx.pum.visible() [alias vim.fn.pumvisible]: whether the insert-mode completion
-- popup is showing. nxvim doesn't surface the popup-menu state to Lua, so this is
-- truthfully 0 in the contexts a plugin checks it (a prompt buffer has no ins-completion
-- menu) — an honest "not visible", not a faked value.
nx.pum = nx.pum or {}
function nx.pum.visible()
  return 0
end
vim.fn.pumvisible = nx.pum.visible

-- nx.jumplist.get([winnr [, tabnr]]) [alias vim.fn.getjumplist]: the window's jumplist as
-- `{ list, curidx }`. `list` is an array of `{ bufnr, lnum, col, coladd }` dicts
-- oldest-first (lnum 1-based, col 0-based byte); `curidx` is the navigation
-- pointer `<C-o>`/`<C-i>` walk — a 0-based index into `list`, equal to `#list`
-- when sitting at the present (not navigating). `winnr` is a window-ID or a
-- 1-based window number (default: the current window). `tabnr` is accepted but
-- only the current tab's windows are mirrored, so an off-tab window yields
-- `{ {}, 0 }`. Reads the window mirror the server pushes (`nx._wins`).
nx.jumplist = nx.jumplist or {}
function nx.jumplist.get(winnr, _tabnr)
  local id
  if winnr == nil or winnr == 0 then
    id = nx._cur_win or 1000
  elseif (nx._wins or {})[winnr] then
    id = winnr -- already a window-ID
  else
    id = (nx._win_order or {})[winnr] or 0
  end
  local w = (nx._wins or {})[id]
  if not w then
    return { {}, 0 }
  end
  local list = {}
  for _, e in ipairs(w.jumps or {}) do
    list[#list + 1] = { bufnr = e.bufnr, lnum = e.lnum, col = e.col, coladd = e.coladd or 0 }
  end
  return { list, w.jump_idx or #list }
end
fn.getjumplist = nx.jumplist.get

-- nx.pos.get(expr) [alias vim.fn.getpos]: a position as `{bufnr, lnum, col, off}`
-- (1-based lnum/col). "." is the cursor; "'<" / "'>" are the visual-selection
-- corners — nxvim doesn't mirror those marks to vim.fn yet, so they fall back to the
-- cursor (a grep-from-selection plugin then greps the cursor word, a graceful
-- degradation rather than an error). Backs a plugin's visual-selection range read.
nx.pos = nx.pos or {}
function nx.pos.get(expr)
  local c = nx._cur_cursor or { row = 1, col = 0 }
  if expr == "." or expr == "'<" or expr == "'>" or expr == "v" then
    return { 0, c.row, c.col + 1, 0 }
  end
  return { 0, 0, 0, 0 }
end
fn.getpos = nx.pos.get

-- nx.pos.set(expr, pos) [alias vim.fn.setpos]: move the cursor when `expr` is "."
-- (the only settable position nxvim models); `pos` is `{bufnr, lnum, col, off}`.
-- Other marks are accepted but not stored (no writable-mark mirror), returning 0.
function nx.pos.set(expr, pos)
  if expr == "." then
    -- The mutating `vim.api.nvim_win_set_cursor` is intentionally nil in Lua
    -- (ADR 0002); move the cursor through the supported `nx._win_set_cursor`
    -- bridge instead (0-based line, 0-based col).
    nx._win_set_cursor(0, math.max(0, (pos[2] or 1) - 1), math.max(0, (pos[3] or 1) - 1))
  end
  return 0
end
fn.setpos = nx.pos.set

-- ----- match highlighting (matchadd family) ----------------------------------
-- A per-window registry of match-highlight requests. INCOMPLETE: the registry is
-- faithful (ids are allocated, stored, and removable, and getmatches reflects it),
-- but nxvim does not yet RENDER these matches — there is no `:match`/`matchadd`
-- decoration path in the core. A plugin uses it to tint the searched term inside
-- a previewer; the preview content is correct, the term is just not yet tinted.
-- This is the documented-approximation pattern (observable state, rendering TBD),
-- chosen over a loud failure so the previewer runs rather than erroring.
nx._matches = nx._matches or {}
nx._match_seq = nx._match_seq or 0
local function match_store(win)
  win = (win == nil or win == 0) and (nx._cur_win or 1000) or win
  nx._matches[win] = nx._matches[win] or {}
  return nx._matches[win]
end
-- nx.match.* (aliases vim.fn.matchadd / matchaddpos / matchdelete / clearmatches /
-- getmatches): the per-window match-highlight registry.
nx.match = nx.match or {}
function nx.match.add(group, pattern, priority, id, opts)
  nx._match_seq = nx._match_seq + 1
  local mid = (id and id ~= -1) and id or nx._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pattern = pattern, priority = priority or 10, id = mid }
  return mid
end
function nx.match.addpos(group, pos, priority, id, opts)
  nx._match_seq = nx._match_seq + 1
  local mid = (id and id ~= -1) and id or nx._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pos = pos, priority = priority or 10, id = mid }
  return mid
end
function nx.match.delete(id, win)
  local store = match_store(win)
  local existed = store[id] ~= nil
  store[id] = nil
  return existed and 0 or -1
end
function nx.match.clear(win)
  nx._matches[(win == nil or win == 0) and (nx._cur_win or 1000) or win] = {}
  return 0
end
function nx.match.get(win)
  local out = {}
  for _, m in pairs(match_store(win)) do
    out[#out + 1] = m
  end
  table.sort(out, function(a, b)
    return a.id < b.id
  end)
  return out
end
fn.matchadd = nx.match.add
fn.matchaddpos = nx.match.addpos
fn.matchdelete = nx.match.delete
fn.clearmatches = nx.match.clear
fn.getmatches = nx.match.get

-- nx.expand(expr[, nosuf, list]) [alias vim.fn.expand]: superset of the
-- snapshot-backed `%` form (the base nx.expand above) that
-- plugins also drive with cursor keywords, `~`/`$ENV` paths, and
-- wildcards. Resolution order:
--   * `%`, `%:<mods>`         — the current file (delegated to the base impl)
--   * `<cword>` / `<cWORD>`   — the (WORD) under the cursor
--   * `<cfile>`               — the path-like token under the cursor
--   * a `:<mods>` suffix on any of the cursor keywords routes through fnamemodify
--   * leading `~` / `$VAR`    — home / environment expansion
--   * a wildcard (`*`/`?`)    — glob (returns a list when `list` is truthy)
--   * anything else           — the path with ~/$ expanded, returned verbatim
-- This re-binds nx.expand (the base loaded earlier), keeping its `%` behavior.
local expand_pct = nx.expand
local function cursor_word(big)
  local c = nx._cur_cursor or { row = 1, col = 0 }
  local buf = nx._bufs and nx._bufs[nx._resolve_bufnr(0)]
  local line = (buf and buf.lines and buf.lines[c.row]) or ""
  local col = (c.col or 0) + 1 -- 1-based byte index of the cursor
  -- `<cword>` is a run of keyword chars (word + underscore); `<cWORD>` is a run of
  -- non-blanks. Scan left and right from the cursor over the matching class.
  local class = big and "%S" or "[%w_]"
  if col > #line then
    col = #line
  end
  if col < 1 then
    return ""
  end
  if line:sub(col, col):match(class) == nil then
    -- Cursor not on the class: vim scans forward to the next match on the line.
    local s = line:find(class, col)
    if not s then
      return ""
    end
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
  if p:sub(1, 1) == "~" then
    p = (os.getenv("HOME") or "") .. p:sub(2)
  end
  p = p:gsub("%$([%w_]+)", function(v)
    return os.getenv(v) or ("$" .. v)
  end)
  return p
end
function nx.expand(expr, nosuf, list)
  expr = tostring(expr)
  -- `%`-family: keep the existing snapshot-backed behavior verbatim.
  if expr == "%" or expr:match("^%%:") then
    return expand_pct(expr, nosuf, list)
  end
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
    if mods ~= "" then
      word = fn.fnamemodify(word, mods)
    end
    return word
  end
  -- Home / env expansion, returned verbatim. Wildcard (`*`/`?`) globbing is NOT
  -- supported here: globbing touches the filesystem, and nxvim has no synchronous fs
  -- (all fs is async via `nx.fs`, ADR 0002 rule 3 — nothing blocks the editor tick).
  -- A pattern with wildcards therefore comes back unexpanded; use `nx.fs` (async) to
  -- walk a directory instead.
  return expand_path(expr)
end
vim.fn.expand = nx.expand

-- nx.fname.modify(fname, mods) [alias vim.fn.fnamemodify]: apply vim's filename
-- modifiers left to right. A pure path-string helper (no I/O beyond reading cwd),
-- so it lives with the vim.fn read builtins — `expand('%:t')` / `'%:h'` and a
-- `'statusline'` `%f` route through it. Supported: `:p` (absolute against cwd),
-- `:~` (relative to $HOME with `~`), `:.` (relative to cwd when under it), `:h`
-- (head/dir), `:t` (tail), `:r` (root, strip one extension — a leading dot isn't
-- one), `:e` (extension; consecutive `:e` widen it to the last k dot-components,
-- vim's quirk). An unsupported modifier errors loud rather than silently passing
-- the name through. Cases match real neovim's vim.fn.fnamemodify.
nx.fname = nx.fname or {}
function nx.fname.modify(fname, mods)
  fname = fname or ""
  mods = mods or ""
  local i, n = 1, #mods
  while i <= n do
    local m = mods:sub(i, i + 1)
    if m == ":p" then
      if fname == "" then
        fname = vim.fn.getcwd()
      elseif fname:sub(1, 1) ~= "/" then
        fname = vim.fn.getcwd() .. "/" .. fname
      end
      i = i + 2
    elseif m == ":~" then
      local home = os.getenv("HOME") or ""
      if home ~= "" and (fname == home or fname:sub(1, #home + 1) == home .. "/") then
        fname = "~" .. fname:sub(#home + 1)
      end
      i = i + 2
    elseif m == ":." then
      local cwd = vim.fn.getcwd()
      if cwd ~= "" and fname:sub(1, #cwd + 1) == cwd .. "/" then
        fname = fname:sub(#cwd + 2)
      end
      i = i + 2
    elseif m == ":h" then
      local head = fname:match("^(.*)/[^/]*$")
      if head == nil then
        fname = "."
      elseif head == "" then
        fname = "/"
      else
        fname = head
      end
      i = i + 2
    elseif m == ":t" then
      fname = fname:match("[^/]*$") or ""
      i = i + 2
    elseif m == ":r" then
      -- Strip the last extension of the tail component (a leading dot isn't one).
      local dir, tail = fname:match("^(.*/)([^/]*)$")
      if not tail then
        dir, tail = "", fname
      end
      for p = #tail, 2, -1 do
        if tail:sub(p, p) == "." then
          tail = tail:sub(1, p - 1)
          break
        end
      end
      fname = dir .. tail
      i = i + 2
    elseif m == ":e" then
      -- Count the run of consecutive `:e`; k of them widen the extension to its
      -- last k dot-separated components (capped at the count of extensions).
      local k = 0
      while mods:sub(i, i + 1) == ":e" do
        k = k + 1
        i = i + 2
      end
      local tail = fname:match("[^/]*$") or ""
      local dots = {}
      for p = 2, #tail do
        if tail:sub(p, p) == "." then
          dots[#dots + 1] = p
        end
      end
      if #dots == 0 then
        fname = ""
      else
        local idx = #dots - k + 1
        if idx < 1 then
          idx = 1
        end
        fname = tail:sub(dots[idx] + 1)
      end
    else
      error("fnamemodify(): unsupported modifier '" .. mods:sub(i) .. "'", 2)
    end
  end
  return fname
end
vim.fn.fnamemodify = nx.fname.modify

-- Read one list mirror (`nx._qflist` for the quickfix list, or
-- `nx._loclist[winid]` for a window's location list) into the dict/array shape
-- getqflist/getloclist return. `mirror` is the entry array; `title` its title.
local function nx_read_list(mirror, title, what)
  mirror = mirror or {}
  if type(what) == "table" then
    local r = {}
    if what.title ~= nil then
      r.title = title or ""
    end
    if what.size ~= nil then
      r.size = #mirror
    end
    if what.items ~= nil then
      local items = {}
      for i, e in ipairs(mirror) do
        items[i] = e
      end
      r.items = items
    end
    return r
  end
  local out = {}
  for i, e in ipairs(mirror) do
    out[i] = e
  end
  return out
end

-- nx.getqflist([what]) [alias vim.fn.getqflist]: the quickfix list. With no
-- argument (or a non-table), returns the array of entry dicts (a shallow copy of
-- the `nx._qflist` mirror the server pushes). With a `what` dict, returns a dict
-- carrying only the requested keys (`title` / `items` / `size`).
function nx.getqflist(what)
  return nx_read_list(nx._qflist, nx._qflist_title, what)
end
vim.fn.getqflist = nx.getqflist

-- Normalize the public `(list, action, what)` setqflist/setloclist tail into the
-- positional `(items, lines, efm, action, title)` nx._set_qflist expects.
local function nx_setlist_args(list, action, what)
  action = action or " "
  local title, efm, items, lines = nil, nil, nil, nil
  if type(what) == "table" then
    title = what.title
    efm = what.efm
    if type(what.lines) == "table" then
      lines = what.lines
    end
    if type(what.items) == "table" then
      items = what.items
    end
  end
  if lines == nil and items == nil and type(list) == "table" then
    items = list -- including the empty list, which clears
  end
  if title ~= nil and type(title) ~= "string" then
    title = tostring(title)
  end
  return items, lines, efm, action, title
end

-- nx.setqflist(list, action, what) [alias vim.fn.setqflist]: populate the
-- quickfix list. `list` is an array of entry dicts; `action` is " " (new / the
-- default), "a" (append), or "r" (replace current). `what` may instead carry
-- `lines` (raw output parsed against `efm`), `items`, `title`, or `efm`. The work
-- happens server-side (a queued op), so the parsed result is visible to
-- getqflist() only after the server drains the op — read it on a later tick.
function nx.setqflist(list, action, what)
  local items, lines, efm, act, title = nx_setlist_args(list, action, what)
  nx._set_qflist(items, lines, efm, act, title, nil)
  return 0
end
vim.fn.setqflist = nx.setqflist

-- nx.getloclist(winnr[, what]) [alias vim.fn.getloclist]: the location list of
-- window `winnr` (0 = current window; otherwise an nxvim window id, NOT vim's
-- 1-based window number). Same return shape as getqflist; an empty list when the
-- window has none.
function nx.getloclist(winnr, what)
  local win = winnr
  if win == nil or win == 0 then
    win = nx.win.current()
  end
  local entry = nx._loclist[win]
  if entry == nil then
    return nx_read_list({}, "", what)
  end
  return nx_read_list(entry.items, entry.title, what)
end
vim.fn.getloclist = nx.getloclist

-- nx.setloclist(winnr, list, action, what) [alias vim.fn.setloclist]: populate the
-- location list of window `winnr` (0 = current window; otherwise an nxvim window
-- id). Same `list`/`action`/`what` semantics as setqflist, only scoped to a
-- window. Queued server-side like setqflist.
function nx.setloclist(winnr, list, action, what)
  local items, lines, efm, act, title = nx_setlist_args(list, action, what)
  -- 0 / nil ride through as 0 ("current window at drain time"); the server resolves
  -- it. A non-zero winnr is taken as a window id.
  nx._set_qflist(items, lines, efm, act, title, winnr or 0)
  return 0
end
vim.fn.setloclist = nx.setloclist

-- nx.qf: the canonical quickfix / location-list surface (ADR 0002). The
-- `vim.fn.*` names above are muscle-memory aliases that delegate here. The window
-- and navigation commands are thin wrappers over the `:c*` / `:l*` ex-commands.
nx.qf = nx.qf or {}
nx.qf.setqflist = nx.setqflist
nx.qf.getqflist = nx.getqflist
nx.qf.setloclist = nx.setloclist
nx.qf.getloclist = nx.getloclist
function nx.qf.open(height)
  vim.cmd(height and ("copen " .. height) or "copen")
end
function nx.qf.close()
  vim.cmd("cclose")
end
function nx.qf.next()
  vim.cmd("cnext")
end
function nx.qf.prev()
  vim.cmd("cprev")
end
function nx.qf.older(count)
  vim.cmd(count and ("colder " .. count) or "colder")
end
function nx.qf.newer(count)
  vim.cmd(count and ("cnewer " .. count) or "cnewer")
end
-- Location-list counterparts to the window / navigation wrappers above: thin
-- wrappers over the `:l*` ex-commands, acting on the current window's list.
function nx.qf.lopen(height)
  vim.cmd(height and ("lopen " .. height) or "lopen")
end
function nx.qf.lclose()
  vim.cmd("lclose")
end
function nx.qf.lnext()
  vim.cmd("lnext")
end
function nx.qf.lprev()
  vim.cmd("lprev")
end
function nx.qf.lolder(count)
  vim.cmd(count and ("lolder " .. count) or "lolder")
end
function nx.qf.lnewer(count)
  vim.cmd(count and ("lnewer " .. count) or "lnewer")
end

-- nx._qf_make(cmd, efm, title, open, jump, loclist_win): the async :make / :grep
-- producer (dispatched from the server, which already expanded
-- 'makeprg'/'grepprg' and merged stderr into stdout via the shell). Spawn `cmd`
-- through the same job machinery as nx.run / vim.system; on exit, split its
-- combined output into lines and hand them to nx._qf_populate, which parses them
-- against `efm` into the quickfix list (or `loclist_win`'s location list — see
-- nx._set_qflist) and then opens the window / jumps to the first error per
-- `open`/`jump`. On a build with no local process spawn (the serverless web build)
-- the underlying spawn op fails loud, exactly like vim.system.
function nx._qf_make(cmd, efm, title, open, jump, loclist_win)
  local id = nx._next_cb_id()
  nx._cb_fns[id] = function(result)
    local out = (result.stdout or "") .. (result.stderr or "")
    local lines = vim.split(out, "\n", { plain = true })
    -- A trailing newline leaves one empty final segment; drop just that one so the
    -- parser doesn't see a phantom blank line (internal blanks are preserved).
    if lines[#lines] == "" then
      lines[#lines] = nil
    end
    nx._qf_populate(lines, efm, title, open, jump, loclist_win)
  end
  nx._system_async(id, { "sh", "-c", cmd }, nil, nil, nil)
end
