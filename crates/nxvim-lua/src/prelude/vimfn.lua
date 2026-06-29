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
    -- A buffer with no file expands to "" for EVERY modifier (`%:p`, `%:h`, `%:t`, …),
    -- matching neovim — there is no path to modify. Without this, `%:p` would run
    -- `fnamemodify("", ":p")`, which resolves the empty name against the cwd and so
    -- makes a nameless/scratch buffer look like a real file at `<cwd>`.
    if name == "" then
      return ""
    end
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

-- nx.getmousepos() [alias vim.fn.getmousepos]: the most recent mouse event's
-- position as a dict — `screenrow`/`screencol` (1-based global screen cell),
-- `winid` (the window the cell is in, 0 if none), `winrow`/`wincol` (1-based,
-- window-relative, gutter included), `line`/`column` (1-based buffer line and byte
-- column, 0 off a window's text), and `coladd` (always 0 — nxvim has no
-- 'virtualedit'). Reads the `nx._mouse_pos` mirror the server pushes from the
-- editor's last mouse cell, so a mouse mapping (`<RightMouse>`, `<MiddleMouse>`, …)
-- can act on the *clicked* position rather than the cursor.
function nx.getmousepos()
  local m = nx._mouse_pos or {}
  return {
    screenrow = m.screenrow or 0,
    screencol = m.screencol or 0,
    winid = m.winid or 0,
    winrow = m.winrow or 0,
    wincol = m.wincol or 0,
    line = m.line or 0,
    column = m.column or 0,
    coladd = 0,
  }
end
fn.getmousepos = nx.getmousepos

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
-- nx.match.add(group, pattern[, priority[, id[, opts]]]) -> id [alias vim.fn.matchadd]:
-- register a request to highlight every match of the regex `pattern` with highlight
-- group `group` in a window. `priority` orders overlapping matches (default 10); `id`
-- requests a specific match id (nil / -1 auto-allocates a fresh one); `opts.window`
-- targets a window other than the current one. Returns the match id.
--
-- CAVEAT: the registry is faithful — ids are allocated and stored, and nx.match.get
-- reflects them — but nxvim does NOT yet render these matches (there is no `:match` /
-- matchadd decoration path in the core). The highlight is recorded but not painted,
-- and the call succeeds rather than failing loud. (A previewer that uses it to tint a
-- search term shows correct content, just un-tinted for now.)
function nx.match.add(group, pattern, priority, id, opts)
  nx._match_seq = nx._match_seq + 1
  local mid = (id and id ~= -1) and id or nx._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pattern = pattern, priority = priority or 10, id = mid }
  return mid
end
-- nx.match.addpos(group, pos[, priority[, id[, opts]]]) -> id [alias vim.fn.matchaddpos]:
-- like nx.match.add, but highlights explicit positions instead of a regex. `pos` is a
-- list whose items are a line number, `{lnum}`, or `{lnum, col, len}` (1-based). Same
-- id / priority / opts.window handling — and the same not-yet-rendered caveat as
-- nx.match.add.
function nx.match.addpos(group, pos, priority, id, opts)
  nx._match_seq = nx._match_seq + 1
  local mid = (id and id ~= -1) and id or nx._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pos = pos, priority = priority or 10, id = mid }
  return mid
end
-- nx.match.delete(id[, win]) -> 0 | -1 [alias vim.fn.matchdelete]: remove the match
-- with id `id` from window `win` (0/nil = current). Returns 0 if it existed, else -1.
function nx.match.delete(id, win)
  local store = match_store(win)
  local existed = store[id] ~= nil
  store[id] = nil
  return existed and 0 or -1
end
-- nx.match.clear([win]) -> 0 [alias vim.fn.clearmatches]: remove every match from
-- window `win` (0/nil = current).
function nx.match.clear(win)
  nx._matches[(win == nil or win == 0) and (nx._cur_win or 1000) or win] = {}
  return 0
end
-- nx.match.get([win]) -> list [alias vim.fn.getmatches]: the registered matches of
-- window `win` (0/nil = current), id-ascending. Each entry is
-- `{ group, id, priority, pattern? | pos? }` — the `pattern` form from nx.match.add,
-- the `pos` form from nx.match.addpos.
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
--   * `<sfile>` / `<script>`  — the path of the script being sourced (so the
--                               `<sfile>:p:h:h` "find my own root" idiom works)
--   * a `:<mods>` suffix on any of those keywords routes through fnamemodify
--   * an unmodeled `<...>` keyword errors loud (no silent literal passthrough)
--   * leading `~` / `$VAR`    — home / environment expansion
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
-- The path of the script currently being sourced — vim's `<sfile>`/`<script>`.
-- Walk the Lua call stack to the nearest real-file chunk: nxvim sources every
-- config / plugin / `require`d file with a `@<path>` chunk name (lifecycle.rs /
-- Lua's own loadfile), while the embedded prelude chunks are named
-- `nxvim:prelude/*` (no `@`) and C frames carry `=[C]`. So the first `@`-prefixed
-- source above this function is the user script whose code is running. Returns ""
-- when no script is on the stack (a bare `:lua` / RPC / callback context),
-- matching neovim's empty `<sfile>` outside a sourced file.
local function sourced_file()
  for lvl = 2, 40 do
    local info = debug.getinfo(lvl, "S")
    if not info then
      break
    end
    if info.source:sub(1, 1) == "@" then
      return info.source:sub(2)
    end
  end
  return ""
end
local function expand_path(p)
  if p:sub(1, 1) == "~" then
    p = (os.getenv("HOME") or "") .. p:sub(2)
  end
  -- Environment variables, both the `${VAR}` and bare `$VAR` forms vim accepts.
  -- Braces first, so `${VAR}` isn't half-eaten by the bare pass; an unset var is
  -- left verbatim (matching vim, and what plugins probe for).
  p = p:gsub("%${([%w_]+)}", function(v)
    return os.getenv(v) or ("${" .. v .. "}")
  end)
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
  -- Special `<...>` keywords, with an optional `:mods` filename-modifier suffix.
  local kw, mods = expr:match("^(<%a+>)(.*)$")
  if kw then
    local word
    if kw == "<cword>" then
      word = cursor_word(false)
    elseif kw == "<cWORD>" or kw == "<cfile>" then
      word = cursor_word(true)
    elseif kw == "<sfile>" or kw == "<script>" then
      word = sourced_file()
    else
      -- An angle-bracket token we don't model (e.g. `<afile>`, `<abuf>`,
      -- `<amatch>`). Fail loud rather than passing the literal text through as a
      -- bogus "path" — a plugin computing `<afile>:p:h` off such a string would
      -- chase a file that never existed.
      error("expand(): unsupported keyword '" .. kw .. "'", 2)
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
-- Lexically simplify an absolute path the way vim's `:p` does (its `simplify_filename`
-- half): collapse `//`, drop `.` components, and resolve each `..` against the preceding
-- component (a `..` at the root is dropped — you can't ascend past `/`). Pure string math,
-- no symlink resolution — so `fnamemodify(".", ":p")` is the cwd (not `<cwd>/.`) and
-- `"a/./b"`/`"a/../b"` collapse, matching neovim and keeping the result a clean prefix for
-- the `:.` / `:~` relativisers (a stray `/.` would defeat their literal cwd-prefix match).
local function simplify_abs(path)
  local parts = {}
  for comp in path:gmatch("[^/]+") do
    if comp == ".." then
      if #parts > 0 then
        table.remove(parts)
      end
    elseif comp ~= "." then
      parts[#parts + 1] = comp
    end
  end
  return "/" .. table.concat(parts, "/")
end

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
      fname = simplify_abs(fname)
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

-- nx.fname.escape(fname) [alias vim.fn.fnameescape]: escape a file name so it can
-- be fed literally as an argument on the `:` command line (e.g. to `:edit`). Each
-- character vim treats as magic on the cmdline gets a backslash prepended — space,
-- tab, newline, and `* ? [ { ` $ \ % # ' " | ! <` — then a leading `>` or `+`
-- (special at the start of `:edit` / `:write`) and a lone `-` are guarded too.
-- Matches real neovim's vim.fn.fnameescape on Unix.
local FNAME_ESC = {
  [" "] = true,
  ["\t"] = true,
  ["\n"] = true,
  ["*"] = true,
  ["?"] = true,
  ["["] = true,
  ["{"] = true,
  ["`"] = true,
  ["$"] = true,
  ["\\"] = true,
  ["%"] = true,
  ["#"] = true,
  ["'"] = true,
  ['"'] = true,
  ["|"] = true,
  ["!"] = true,
  ["<"] = true,
}
function nx.fname.escape(fname)
  fname = fname or ""
  local out = {}
  for p = 1, #fname do
    local ch = fname:sub(p, p)
    if FNAME_ESC[ch] then
      out[#out + 1] = "\\"
    end
    out[#out + 1] = ch
  end
  local s = table.concat(out)
  local first = s:sub(1, 1)
  if first == ">" or first == "+" or s == "-" then
    s = "\\" .. s
  end
  return s
end
vim.fn.fnameescape = nx.fname.escape

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

-- nx.qf: the canonical quickfix / location-list surface (ADR 0002). The list
-- accessors are defined as nx.qf.* here; the bare nx.* and vim.fn.* spellings are
-- muscle-memory aliases onto them (the vim.fn ones set inline, the bare-nx ones in
-- one block below the definitions). The window / navigation commands further down
-- are thin wrappers over the `:c*` / `:l*` ex-commands.
nx.qf = nx.qf or {}

-- nx.qf.getqflist([what]) -> list | dict [aliases nx.getqflist / vim.fn.getqflist]:
-- the quickfix list. With no argument (or a non-table), returns the array of entry
-- dicts (a shallow copy of the `nx._qflist` mirror the server pushes). With a `what`
-- dict, returns a dict carrying only the requested keys (`title` / `items` / `size`).
function nx.qf.getqflist(what)
  return nx_read_list(nx._qflist, nx._qflist_title, what)
end
vim.fn.getqflist = nx.qf.getqflist

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

-- nx.qf.setqflist(list[, action[, what]]) -> 0 [aliases nx.setqflist /
-- vim.fn.setqflist]: populate the quickfix list. `list` is an array of entry dicts;
-- `action` is " " (new / the default), "a" (append), or "r" (replace current).
-- `what` may instead carry `lines` (raw output parsed against `efm`), `items`,
-- `title`, or `efm`. The work happens server-side (a queued op), so the parsed result
-- is visible to nx.qf.getqflist() only after the server drains the op — read it on a
-- later tick.
function nx.qf.setqflist(list, action, what)
  local items, lines, efm, act, title = nx_setlist_args(list, action, what)
  nx._set_qflist(items, lines, efm, act, title, nil)
  return 0
end
vim.fn.setqflist = nx.qf.setqflist

-- nx.qf.getloclist(winnr[, what]) -> list | dict [aliases nx.getloclist /
-- vim.fn.getloclist]: the location list of window `winnr` (0 = current window;
-- otherwise an nxvim window id, NOT vim's 1-based window number). Same return shape
-- as nx.qf.getqflist; an empty list when the window has none.
function nx.qf.getloclist(winnr, what)
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
vim.fn.getloclist = nx.qf.getloclist

-- nx.qf.setloclist(winnr, list[, action[, what]]) -> 0 [aliases nx.setloclist /
-- vim.fn.setloclist]: populate the location list of window `winnr` (0 = current
-- window; otherwise an nxvim window id). Same `list`/`action`/`what` semantics as
-- nx.qf.setqflist, only scoped to a window. Queued server-side like setqflist.
function nx.qf.setloclist(winnr, list, action, what)
  local items, lines, efm, act, title = nx_setlist_args(list, action, what)
  -- 0 / nil ride through as 0 ("current window at drain time"); the server resolves
  -- it. A non-zero winnr is taken as a window id.
  nx._set_qflist(items, lines, efm, act, title, winnr or 0)
  return 0
end
vim.fn.setloclist = nx.qf.setloclist

-- The "send/add results to a list" family — the nxvim port of telescope's
-- send/add-to-{loc,qf}list actions, and the picker's quickfix-style sinks. `list`
-- is an array of entry dicts (same shape as setloclist); `opts.title` labels the
-- list / dock tab. All honor 'qfdock': with it ON (default, the nxvim way) the
-- results open in the bottom dock — a *location-list send* opens a NEW tab (several
-- searches sit side by side, entries jump into the main layer); an *add* appends to
-- the focused dock loclist tab; the quickfix list is one reused tab. With it OFF
-- (the vim/telescope way) they open the classic bottom split of the current window.
local function nx_list_send(list, opts, action, to_qf)
  opts = opts or {}
  local title = opts.title
  if title ~= nil and type(title) ~= "string" then
    title = tostring(title)
  end
  nx._list_send(list or {}, title, action, to_qf)
  return 0
end

-- send_to_loclist: results -> a (new) location list. add_to_loclist: append.
function nx.qf.send_to_loclist(list, opts)
  return nx_list_send(list, opts, " ", false)
end
function nx.qf.add_to_loclist(list, opts)
  return nx_list_send(list, opts, "a", false)
end
-- send_to_qflist: results -> the global quickfix list. add_to_qflist: append.
function nx.qf.send_to_qflist(list, opts)
  return nx_list_send(list, opts, " ", true)
end
function nx.qf.add_to_qflist(list, opts)
  return nx_list_send(list, opts, "a", true)
end
nx.send_to_loclist = nx.qf.send_to_loclist
nx.add_to_loclist = nx.qf.add_to_loclist
nx.send_to_qflist = nx.qf.send_to_qflist
nx.add_to_qflist = nx.qf.add_to_qflist

-- Bare-nx muscle-memory aliases onto the canonical nx.qf.* list accessors (the
-- vim.fn.* aliases were set inline above).
nx.getqflist = nx.qf.getqflist
nx.setqflist = nx.qf.setqflist
nx.getloclist = nx.qf.getloclist
nx.setloclist = nx.qf.setloclist

-- Named lists (window-independent, addressed by name) ----------------------
--
-- A *named list* is like the global quickfix list — structured entries, its own
-- bottom-dock tab, `<CR>` jumps into the main editing layer — but there can be many,
-- each addressed by a stable name, and storage lives on the editor (not a window),
-- so it survives closing any window and never collides with the single quickfix.
-- That makes it the fit for a persistent plugin panel (e.g. dap's "All Breakpoints"):
-- the plugin pushes items with nx.qf.list(name, items) whenever its data changes, and
-- nx.qf.show(name) opens/focuses the tab. Both are thin queues over the existing
-- quickfix rendering and navigation — no datasource/refresh indirection.

-- nx.qf.list(name, items[, opts]): create or replace the named list `name` from
-- `items` (an array of entry dicts, the same shape setqflist takes), repainting its
-- tab if open. Does NOT open or focus the tab — call nx.qf.show(name) for that.
--   opts.title  (string) the list title shown in the dock tab (defaults to `name`).
--   opts.action (string) "r" (default, replace in place) / " " (push a new list onto
--               the stack) / "a" (append to the current list).
-- Returns the name.
function nx.qf.list(name, items, opts)
  if type(name) ~= "string" or name == "" then
    error("nx.qf.list: name must be a non-empty string", 2)
  end
  if type(items) ~= "table" then
    error("nx.qf.list: items must be an array of entry dicts", 2)
  end
  opts = opts or {}
  local action = opts.action or "r"
  nx._set_qflist(items, nil, nil, action, opts.title or name, nil, name)
  return name
end

-- nx.qf.show(name): open or focus the named list `name`'s bottom-dock tab — the
-- clean, window-independent reopen (no set_current / on_next_tick dance; the open is
-- sequenced server-side after any nx.qf.list queued in the same tick). Showing a name
-- with no items yet opens an empty tab. Returns the name.
function nx.qf.show(name)
  if type(name) ~= "string" or name == "" then
    error("nx.qf.show: name must be a non-empty string", 2)
  end
  nx._named_list_show(name)
  return name
end

-- nx.qf.drop(name): forget the named list `name` — close its dock tab if open and
-- remove its contents from the editor. A no-op for a name that was never used.
function nx.qf.drop(name)
  if type(name) ~= "string" or name == "" then
    error("nx.qf.drop: name must be a non-empty string", 2)
  end
  nx._named_list_drop(name)
  return name
end

-- nx.qf.open([height]) [wraps `:copen`]: open the quickfix window, optionally
-- `height` rows tall.
function nx.qf.open(height)
  vim.cmd(height and ("copen " .. height) or "copen")
end
-- nx.qf.close() [wraps `:cclose`]: close the quickfix window.
function nx.qf.close()
  vim.cmd("cclose")
end
-- nx.qf.next() [wraps `:cnext`]: jump to the next entry in the quickfix list.
function nx.qf.next()
  vim.cmd("cnext")
end
-- nx.qf.prev() [wraps `:cprev`]: jump to the previous entry in the quickfix list.
function nx.qf.prev()
  vim.cmd("cprev")
end
-- nx.qf.older([count]) [wraps `:colder`]: go to an older quickfix list in the stack
-- (`count` lists back, default 1).
function nx.qf.older(count)
  vim.cmd(count and ("colder " .. count) or "colder")
end
-- nx.qf.newer([count]) [wraps `:cnewer`]: go to a newer quickfix list in the stack
-- (`count` lists forward, default 1).
function nx.qf.newer(count)
  vim.cmd(count and ("cnewer " .. count) or "cnewer")
end
-- The location-list counterparts of the quickfix window / navigation wrappers
-- above — thin wrappers over the `:l*` ex-commands, acting on the CURRENT window's
-- location list rather than the global quickfix list.
-- nx.qf.lopen([height]) [wraps `:lopen`]: open the location-list window, optionally
-- `height` rows tall.
function nx.qf.lopen(height)
  vim.cmd(height and ("lopen " .. height) or "lopen")
end
-- nx.qf.lclose() [wraps `:lclose`]: close the location-list window.
function nx.qf.lclose()
  vim.cmd("lclose")
end
-- nx.qf.lnext() [wraps `:lnext`]: jump to the next entry in the location list.
function nx.qf.lnext()
  vim.cmd("lnext")
end
-- nx.qf.lprev() [wraps `:lprev`]: jump to the previous entry in the location list.
function nx.qf.lprev()
  vim.cmd("lprev")
end
-- nx.qf.lolder([count]) [wraps `:lolder`]: go to an older location list in the
-- window's stack (`count` lists back, default 1).
function nx.qf.lolder(count)
  vim.cmd(count and ("lolder " .. count) or "lolder")
end
-- nx.qf.lnewer([count]) [wraps `:lnewer`]: go to a newer location list in the
-- window's stack (`count` lists forward, default 1).
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
