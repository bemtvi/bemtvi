-- bemtvi Lua prelude — vim.fn editor-query builtins.
-- The Vimscript builtins that read live editor state through the state mirror:
-- btv.line / btv.col / btv.expand / btv.localtime / btv.undotree, and the
-- btv.pum / btv.pos / btv.match / btv.jumplist registries that plugins query. Each is
-- authored as an btv.* noun with its vim.fn.* alias.
local vim = vim
local fn = vim.fn
btv = btv or {}

-- `btv.line(expr)` [alias `vim.fn.line`]: a buffer line number. `"."` is the cursor line
-- (1-based), `"$"` the last line (the line count). The window-relative forms
-- (`"w0"`/`"w$"`) need the scroll position, which the mirror doesn't carry yet, so they
-- error loud.
function btv.line(expr)
  if expr == "." then
    return (btv._cur_cursor or {}).row or 1
  elseif expr == "$" then
    local buf = btv._bufs[btv._resolve_bufnr(0)]
    return (buf and buf.lines) and #buf.lines or 1
  end
  error("line(): unsupported expression '" .. tostring(expr) .. "'", 2)
end
vim.fn.line = btv.line

-- `btv.col(expr)` [alias `vim.fn.col`]: a byte column (1-based). `"."` is the cursor
-- column, `"$"` one past the end of the cursor line (its byte length + 1), matching vim.
function btv.col(expr)
  if expr == "." then
    return ((btv._cur_cursor or {}).col or 0) + 1
  elseif expr == "$" then
    local buf = btv._bufs[btv._resolve_bufnr(0)]
    local row = (btv._cur_cursor or {}).row or 1
    local ln = (buf and buf.lines) and buf.lines[row] or ""
    return #ln + 1
  end
  error("col(): unsupported expression '" .. tostring(expr) .. "'", 2)
end
vim.fn.col = btv.col

-- `btv.localtime()` [alias `vim.fn.localtime`]: the current time in seconds. bemtvi
-- sources this from a MONOTONIC clock (the server's `btv._mono_secs`, the same base
-- stamped onto undo nodes), not wall-clock unix epoch, so `localtime() - node.time`
-- elapsed math (e.g. the undotree visualizer's `"N minutes ago"`) stays correct and
-- non-negative across NTP steps and manual clock changes. Only differences matter.
function btv.localtime()
  return btv._mono_secs or 0
end
vim.fn.localtime = btv.localtime

-- `btv.undotree.get([bufnr])` [alias `vim.fn.undotree`]: the buffer's undo tree, in neovim's shape
-- ({ seq_last, seq_cur, save_last, save_cur, time_cur, synced, entries }, each
-- entry { seq, time, save?, alt? }). Reads the `btv._undotree` mirror the server
-- projects from the core's branching history before each Lua entry; `bufnr`
-- 0/nil is the current buffer. A buffer with no recorded history yet yields an
-- empty-`entries` tree rather than erroring.
btv.undotree = btv.undotree or {}
function btv.undotree.get(bufnr)
  bufnr = btv._resolve_bufnr(bufnr)
  local t = (btv._undotree or {})[bufnr]
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
vim.fn.undotree = btv.undotree.get

-- (vim.fn.fnamemodify lives in prelude/fs.lua, alongside the other path vim.fn;
-- this chunk's expand routes through it at call time.)

-- `btv.expand(expr)` [alias `vim.fn.expand`]: the `%` (current file) and `#`
-- (alternate file) forms autocmd callbacks and statuslines use to resolve paths,
-- backed by the current-buffer snapshot and the `#` mirror. `%` / `#` are the stored
-- names; a `:<mods>` suffix routes through `fnamemodify` (so `%:t`, `%:p`, `#:h`,
-- `#:r`, `%:~:.`, … all work). Any other expression errors loud.
-- (the override below extends this with cursor keywords / globs, re-binding btv.expand.)
function btv.expand(expr)
  -- `#` is the alternate file name, which the core tracks as a *name*: it stays
  -- resolvable after the buffer it named was `:bdelete`d, matching vim.
  local name = (btv._cur_buf or {}).name or ""
  local pat = "^%%(:.*)$"
  if expr:sub(1, 1) == "#" then
    name, pat = btv._alt_file or "", "^#(:.*)$"
  end
  if expr == "%" or expr == "#" then
    return name
  end
  local mods = expr:match(pat)
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
vim.fn.expand = btv.expand

-- `btv.pum.visible()` [alias `vim.fn.pumvisible`]: whether the insert-mode completion
-- popup is showing. bemtvi doesn't surface the popup-menu state to Lua, so this is
-- truthfully 0 in the contexts a plugin checks it (a prompt buffer has no ins-completion
-- menu) — an honest "not visible", not a faked value.
btv.pum = btv.pum or {}
function btv.pum.visible()
  return 0
end
vim.fn.pumvisible = btv.pum.visible

-- `btv.jumplist.get([winnr [, tabnr]])` [alias `vim.fn.getjumplist`]: the window's jumplist as
-- `{ list, curidx }`. `list` is an array of `{ bufnr, lnum, col, coladd }` dicts
-- oldest-first (lnum 1-based, col 0-based byte); `curidx` is the navigation
-- pointer `<C-o>`/`<C-i>` walk — a 0-based index into `list`, equal to `#list`
-- when sitting at the present (not navigating). `winnr` is a window-ID or a
-- 1-based window number (default: the current window). `tabnr` is accepted but
-- only the current tab's windows are mirrored, so an off-tab window yields
-- `{ {}, 0 }`. Reads the window mirror the server pushes (`btv._wins`).
btv.jumplist = btv.jumplist or {}
function btv.jumplist.get(winnr, _tabnr)
  local id
  if winnr == nil or winnr == 0 then
    id = btv._cur_win or 1000
  elseif (btv._wins or {})[winnr] then
    id = winnr -- already a window-ID
  else
    id = (btv._win_order or {})[winnr] or 0
  end
  local w = (btv._wins or {})[id]
  if not w then
    return { {}, 0 }
  end
  local list = {}
  for _, e in ipairs(w.jumps or {}) do
    list[#list + 1] = { bufnr = e.bufnr, lnum = e.lnum, col = e.col, coladd = e.coladd or 0 }
  end
  return { list, w.jump_idx or #list }
end
fn.getjumplist = btv.jumplist.get

-- `btv.pos.get(expr)` [alias `vim.fn.getpos`]: a position as `{bufnr, lnum, col, off}`
-- (1-based lnum/col). `"."` is the cursor; `"'<"` / `"'>"` are the visual-selection
-- corners — bemtvi doesn't mirror those marks to `vim.fn` yet, so they fall back to the
-- cursor (a grep-from-selection plugin then greps the cursor word, a graceful
-- degradation rather than an error). Backs a plugin's visual-selection range read.
btv.pos = btv.pos or {}
function btv.pos.get(expr)
  local c = btv._cur_cursor or { row = 1, col = 0 }
  if expr == "." or expr == "'<" or expr == "'>" or expr == "v" then
    return { 0, c.row, c.col + 1, 0 }
  end
  return { 0, 0, 0, 0 }
end
fn.getpos = btv.pos.get

-- `btv.pos.set(expr, pos)` [alias `vim.fn.setpos`]: move the cursor when `expr` is `"."`
-- (the only settable position bemtvi models); `pos` is `{bufnr, lnum, col, off}`.
-- Other marks are accepted but not stored (no writable-mark mirror), returning 0.
function btv.pos.set(expr, pos)
  if expr == "." then
    -- The mutating `vim.api.nvim_win_set_cursor` is intentionally nil in Lua
    -- (ADR 0002); move the cursor through the supported `btv._win_set_cursor`
    -- bridge instead (0-based line, 0-based col).
    btv._win_set_cursor(0, math.max(0, (pos[2] or 1) - 1), math.max(0, (pos[3] or 1) - 1))
  end
  return 0
end
fn.setpos = btv.pos.set

-- `btv.getmousepos()` [alias `vim.fn.getmousepos`]: the most recent mouse event's
-- position as a dict — `screenrow`/`screencol` (1-based global screen cell),
-- `winid` (the window the cell is in, 0 if none), `winrow`/`wincol` (1-based,
-- window-relative, gutter included), `line`/`column` (1-based buffer line and byte
-- column, 0 off a window's text), and `coladd` (always 0 — bemtvi has no
-- `'virtualedit'`). Reads the `btv._mouse_pos` mirror the server pushes from the
-- editor's last mouse cell, so a mouse mapping (`<RightMouse>`, `<MiddleMouse>`, …)
-- can act on the *clicked* position rather than the cursor.
function btv.getmousepos()
  local m = btv._mouse_pos or {}
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
fn.getmousepos = btv.getmousepos

-- ----- match highlighting (matchadd family) ----------------------------------
-- A per-window registry of match-highlight requests. INCOMPLETE: the registry is
-- faithful (ids are allocated, stored, and removable, and getmatches reflects it),
-- but bemtvi does not yet RENDER these matches — there is no `:match`/`matchadd`
-- decoration path in the core. A plugin uses it to tint the searched term inside
-- a previewer; the preview content is correct, the term is just not yet tinted.
-- This is the documented-approximation pattern (observable state, rendering TBD),
-- chosen over a loud failure so the previewer runs rather than erroring.
btv._matches = btv._matches or {}
btv._match_seq = btv._match_seq or 0
local function match_store(win)
  win = (win == nil or win == 0) and (btv._cur_win or 1000) or win
  btv._matches[win] = btv._matches[win] or {}
  return btv._matches[win]
end
-- `btv.match.*` (aliases `vim.fn.matchadd` / `matchaddpos` / `matchdelete` / `clearmatches` /
-- `getmatches`): the per-window match-highlight registry.
btv.match = btv.match or {}
-- `btv.match.add(group, pattern[, priority[, id[, opts]]])` -> id [alias `vim.fn.matchadd`]:
-- register a request to highlight every match of the regex `pattern` with highlight
-- group `group` in a window. `priority` orders overlapping matches (default 10); `id`
-- requests a specific match id (nil / -1 auto-allocates a fresh one); `opts.window`
-- targets a window other than the current one. Returns the match id.
--
-- CAVEAT: the registry is faithful — ids are allocated and stored, and `btv.match.get`
-- reflects them — but bemtvi does NOT yet render these matches (there is no `:match` /
-- `matchadd` decoration path in the core). The highlight is recorded but not painted,
-- and the call succeeds rather than failing loud. (A previewer that uses it to tint a
-- search term shows correct content, just un-tinted for now.)
function btv.match.add(group, pattern, priority, id, opts)
  btv._match_seq = btv._match_seq + 1
  local mid = (id and id ~= -1) and id or btv._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pattern = pattern, priority = priority or 10, id = mid }
  return mid
end
-- `btv.match.addpos(group, pos[, priority[, id[, opts]]])` -> id [alias `vim.fn.matchaddpos`]:
-- like `btv.match.add`, but highlights explicit positions instead of a regex. `pos` is a
-- list whose items are a line number, `{lnum}`, or `{lnum, col, len}` (1-based). Same
-- id / priority / `opts.window` handling — and the same not-yet-rendered caveat as
-- `btv.match.add`.
function btv.match.addpos(group, pos, priority, id, opts)
  btv._match_seq = btv._match_seq + 1
  local mid = (id and id ~= -1) and id or btv._match_seq
  local store = match_store(opts and opts.window)
  store[mid] = { group = group, pos = pos, priority = priority or 10, id = mid }
  return mid
end
-- `btv.match.delete(id[, win])` -> 0 | -1 [alias `vim.fn.matchdelete`]: remove the match
-- with id `id` from window `win` (0/nil = current). Returns 0 if it existed, else -1.
function btv.match.delete(id, win)
  local store = match_store(win)
  local existed = store[id] ~= nil
  store[id] = nil
  return existed and 0 or -1
end
-- `btv.match.clear([win])` -> 0 [alias `vim.fn.clearmatches`]: remove every match from
-- window `win` (0/nil = current).
function btv.match.clear(win)
  btv._matches[(win == nil or win == 0) and (btv._cur_win or 1000) or win] = {}
  return 0
end
-- `btv.match.get([win])` -> list [alias `vim.fn.getmatches`]: the registered matches of
-- window `win` (0/nil = current), id-ascending. Each entry is
-- `{ group, id, priority, pattern? | pos? }` — the `pattern` form from `btv.match.add`,
-- the `pos` form from `btv.match.addpos`.
function btv.match.get(win)
  local out = {}
  for _, m in pairs(match_store(win)) do
    out[#out + 1] = m
  end
  table.sort(out, function(a, b)
    return a.id < b.id
  end)
  return out
end
fn.matchadd = btv.match.add
fn.matchaddpos = btv.match.addpos
fn.matchdelete = btv.match.delete
fn.clearmatches = btv.match.clear
fn.getmatches = btv.match.get

-- `btv.expand(expr[, nosuf, list])` [alias `vim.fn.expand`]: superset of the
-- snapshot-backed `%` form (the base btv.expand above) that
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
-- This re-binds `btv.expand` (the base loaded earlier), keeping its `%` behavior.
local expand_pct = btv.expand
local function cursor_word(big)
  local c = btv._cur_cursor or { row = 1, col = 0 }
  local buf = btv._bufs and btv._bufs[btv._resolve_bufnr(0)]
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
-- The path of the script currently being sourced — vim's `<sfile>`/`<script>`,
-- via the shared stack walker (`btv.utils.caller_source`). Returns "" when no
-- script is on the stack (a bare `:lua` / RPC / callback context), matching
-- neovim's empty `<sfile>` outside a sourced file.
local function sourced_file()
  return btv.utils.caller_source() or ""
end
local function expand_path(p)
  -- Leading `~` / `~/` → `$HOME` via the shared helper; a `~user` form stays
  -- literal (vim leaves an unknown user's `~user` unexpanded too).
  p = btv.utils.expanduser(p)
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
function btv.expand(expr, nosuf, list)
  expr = tostring(expr)
  -- `%` / `#` families: keep the existing snapshot-backed behavior verbatim.
  if expr == "%" or expr:match("^%%:") or expr == "#" or expr:match("^#:") then
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
  -- supported here: globbing touches the filesystem, and bemtvi has no synchronous fs
  -- (all fs is async via `btv.fs`, ADR 0002 rule 3 — nothing blocks the editor tick).
  -- A pattern with wildcards therefore comes back unexpanded; use `btv.fs` (async) to
  -- walk a directory instead.
  return expand_path(expr)
end
vim.fn.expand = btv.expand

-- `btv.cwd()` [alias `vim.fn.getcwd`]: the editor's effective working directory, as an
-- absolute path with no trailing separator.
--
-- This is editor state, not a filesystem read: it is answered from the mirror the
-- server republishes on every `:cd` / `:lcd`, so it costs nothing and is safe on a
-- hot path. Over a daemon it reports the *daemon's* cwd — the directory relative
-- paths actually resolve against — which is the whole reason a plugin should ask
-- here rather than at whatever process it happens to be running in.
--
-- The fallback root: a config that walks up looking for a project marker and finds
-- none often wants "wherever the user is working" rather than the file's own
-- directory.
--
-- ```lua
-- local root = btv.await(btv.lsp.find_root(bufnr, { ".git" })) or btv.cwd()
-- ```
function btv.cwd()
  return vim.fn.getcwd()
end

-- `btv.pid()` [alias `vim.fn.getpid`]: this editor process's id.
--
-- What a language server wants when it offers a `--hostPID`-style flag: it watches
-- that process and exits when the process does, so a crashed editor doesn't leave a
-- server holding a project's worth of memory. In a daemon session this is still the
-- process the server is a child of, which is the one it must watch.
function btv.pid()
  return vim.fn.getpid()
end

-- `btv.version()` [alias `vim.version`]: the editor's version, as `"bemtvi <x.y.z>"`.
--
-- For the protocols that carry a client identity — LSP `clientInfo`, a vendor's
-- "integration version" telemetry field — and for a plugin reporting what it is
-- running under. It is a display string, not a comparable version object: bemtvi's
-- surfaces are not version-gated, so there is nothing here to branch on.
function btv.version()
  return vim.version
end

-- `btv.stdpath(what)` [alias `vim.fn.stdpath`]: the XDG directory bemtvi keeps `what`
-- in, as an absolute path with no trailing separator. `"config"`, `"data"`,
-- `"cache"`, `"state"`, `"log"`, `"run"`.
--
-- Where a plugin puts the things that are neither the user's code nor the editor's:
-- a downloaded language server, a parser cache, a scratch workspace. Ask here rather
-- than composing `$HOME` by hand — the answer honors `$XDG_*` and `$BEMTVI_CONFIG`, so
-- a hand-built path silently diverges from where the editor itself looks.
--
-- ```lua
-- local workspace = btv.utils.joinpath(btv.stdpath("cache"), "jdtls", "workspace")
-- ```
--
-- The directory is *named*, not created: `btv.fs.mkdir(dir, { recursive = true })`
-- when you are about to write into it.
function btv.stdpath(what)
  return vim.fn.stdpath(what)
end

-- `btv.fname.modify(fname, mods)` [alias `vim.fn.fnamemodify`]: apply vim's filename
-- modifiers left to right. A pure path-string helper (no I/O beyond reading cwd),
-- so it lives with the `vim.fn` read builtins — `expand('%:t')` / `'%:h'` and a
-- `'statusline'` `%f` route through it. Supported: `:p` (absolute against cwd),
-- `:~` (relative to `$HOME` with `~`), `:.` (relative to cwd when under it), `:h`
-- (head/dir), `:t` (tail), `:r` (root, strip one extension — a leading dot isn't
-- one), `:e` (extension; consecutive `:e` widen it to the last k dot-components,
-- vim's quirk). An unsupported modifier errors loud rather than silently passing
-- the name through. Cases match real neovim's `vim.fn.fnamemodify`.
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

btv.fname = btv.fname or {}
function btv.fname.modify(fname, mods)
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
      if cwd ~= "" and fname == cwd then
        -- The cwd itself reduces to ".", as in vim; the prefix test below needs one
        -- more character than the path has, so it can't catch the exact match.
        fname = "."
      elseif cwd ~= "" and fname:sub(1, #cwd + 1) == cwd .. "/" then
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
vim.fn.fnamemodify = btv.fname.modify

-- `btv.fname.escape(fname)` [alias `vim.fn.fnameescape`]: escape a file name so it can
-- be fed literally as an argument on the `:` command line (e.g. to `:edit`). Each
-- character vim treats as magic on the cmdline gets a backslash prepended — space,
-- tab, newline, and `* ? [ { ` $ \ % # ' " | ! <` — then a leading `>` or `+`
-- (special at the start of `:edit` / `:write`) and a lone `-` are guarded too.
-- Matches real neovim's `vim.fn.fnameescape` on Unix.
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
function btv.fname.escape(fname)
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
vim.fn.fnameescape = btv.fname.escape

-- Read one list mirror (`btv._qflist` for the quickfix list, or
-- `btv._loclist[winid]` for a window's location list) into the dict/array shape
-- getqflist/getloclist return. `mirror` is the entry array; `title` its title.
local function btv_read_list(mirror, title, what)
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

-- `btv.qf`: the canonical quickfix / location-list surface (ADR 0002). The list
-- accessors are defined as `btv.qf.*` here; the bare `btv.*` and `vim.fn.*` spellings are
-- muscle-memory aliases onto them (the `vim.fn` ones set inline, the bare-btv ones in
-- one block below the definitions). The window / navigation commands further down
-- are thin wrappers over the `:c*` / `:l*` ex-commands.
btv.qf = btv.qf or {}

-- `btv.qf.getqflist([what])` -> list | dict [aliases `btv.getqflist` / `vim.fn.getqflist`]:
-- the quickfix list. With no argument (or a non-table), returns the array of entry
-- dicts (a shallow copy of the `btv._qflist` mirror the server pushes). With a `what`
-- dict, returns a dict carrying only the requested keys (`title` / `items` / `size`).
function btv.qf.getqflist(what)
  return btv_read_list(btv._qflist, btv._qflist_title, what)
end
vim.fn.getqflist = btv.qf.getqflist

-- Normalize the public `(list, action, what)` setqflist/setloclist tail into the
-- positional `(items, lines, efm, action, title)` btv._set_qflist expects.
local function btv_setlist_args(list, action, what)
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

-- `btv.qf.setqflist(list[, action[, what]])` -> 0 [aliases `btv.setqflist` /
-- `vim.fn.setqflist`]: populate the quickfix list. `list` is an array of entry dicts;
-- `action` is `" "` (new / the default), `"a"` (append), or `"r"` (replace current).
-- `what` may instead carry `lines` (raw output parsed against `efm`), `items`,
-- `title`, or `efm`. The work happens server-side (a queued op), so the parsed result
-- is visible to `btv.qf.getqflist()` only after the server drains the op — read it on a
-- later tick.
function btv.qf.setqflist(list, action, what)
  local items, lines, efm, act, title = btv_setlist_args(list, action, what)
  btv._set_qflist(items, lines, efm, act, title, nil)
  return 0
end
vim.fn.setqflist = btv.qf.setqflist

-- `btv.qf.getloclist(winnr[, what])` -> list | dict [aliases `btv.getloclist` /
-- `vim.fn.getloclist`]: the location list of window `winnr` (0 = current window;
-- otherwise an bemtvi window id, NOT vim's 1-based window number). Same return shape
-- as `btv.qf.getqflist`; an empty list when the window has none.
function btv.qf.getloclist(winnr, what)
  local win = winnr
  if win == nil or win == 0 then
    win = btv.win.current()
  end
  local entry = btv._loclist[win]
  if entry == nil then
    return btv_read_list({}, "", what)
  end
  return btv_read_list(entry.items, entry.title, what)
end
vim.fn.getloclist = btv.qf.getloclist

-- `btv.qf.setloclist(winnr, list[, action[, what]])` -> 0 [aliases `btv.setloclist` /
-- `vim.fn.setloclist`]: populate the location list of window `winnr` (0 = current
-- window; otherwise an bemtvi window id). Same `list`/`action`/`what` semantics as
-- `btv.qf.setqflist`, only scoped to a window. Queued server-side like `setqflist`.
function btv.qf.setloclist(winnr, list, action, what)
  local items, lines, efm, act, title = btv_setlist_args(list, action, what)
  -- 0 / nil ride through as 0 ("current window at drain time"); the server resolves
  -- it. A non-zero winnr is taken as a window id.
  btv._set_qflist(items, lines, efm, act, title, winnr or 0)
  return 0
end
vim.fn.setloclist = btv.qf.setloclist

-- The "send/add results to a list" family — the bemtvi port of telescope's
-- send/add-to-{loc,qf}list actions, and the picker's quickfix-style sinks. `list`
-- is an array of entry dicts (same shape as setloclist); `opts.title` labels the
-- list / dock tab. All honor 'qfdock': with it ON (default, the bemtvi way) the
-- results open in the bottom dock — a *location-list send* opens a NEW tab (several
-- searches sit side by side, entries jump into the main layer); an *add* appends to
-- the focused dock loclist tab; the quickfix list is one reused tab. With it OFF
-- (the vim/telescope way) they open the classic bottom split of the current window.
local function btv_list_send(list, opts, action, to_qf)
  opts = opts or {}
  local title = opts.title
  if title ~= nil and type(title) ~= "string" then
    title = tostring(title)
  end
  btv._list_send(list or {}, title, action, to_qf)
  return 0
end

-- `send_to_loclist`: results -> a (new) location list. `add_to_loclist`: append.
function btv.qf.send_to_loclist(list, opts)
  return btv_list_send(list, opts, " ", false)
end
function btv.qf.add_to_loclist(list, opts)
  return btv_list_send(list, opts, "a", false)
end
-- `send_to_qflist`: results -> the global quickfix list. `add_to_qflist`: append.
function btv.qf.send_to_qflist(list, opts)
  return btv_list_send(list, opts, " ", true)
end
function btv.qf.add_to_qflist(list, opts)
  return btv_list_send(list, opts, "a", true)
end
btv.send_to_loclist = btv.qf.send_to_loclist
btv.add_to_loclist = btv.qf.add_to_loclist
btv.send_to_qflist = btv.qf.send_to_qflist
btv.add_to_qflist = btv.qf.add_to_qflist

-- Bare-btv muscle-memory aliases onto the canonical btv.qf.* list accessors (the
-- vim.fn.* aliases were set inline above).
btv.getqflist = btv.qf.getqflist
btv.setqflist = btv.qf.setqflist
btv.getloclist = btv.qf.getloclist
btv.setloclist = btv.qf.setloclist

-- Named lists (window-independent, addressed by name) ----------------------
--
-- A *named list* is like the global quickfix list — structured entries, its own
-- bottom-dock tab, `<CR>` jumps into the main editing layer — but there can be many,
-- each addressed by a stable name, and storage lives on the editor (not a window),
-- so it survives closing any window and never collides with the single quickfix.
-- That makes it the fit for a persistent plugin panel (e.g. dap's "All Breakpoints"):
-- the plugin pushes items with `btv.qf.list(name, items)` whenever its data changes, and
-- `btv.qf.show(name)` opens/focuses the tab. Both are thin queues over the existing
-- quickfix rendering and navigation — no datasource/refresh indirection.

-- `btv.qf.list(name, items[, opts])`: create or replace the named list `name` from
-- `items` (an array of entry dicts, the same shape `setqflist` takes), repainting its
-- tab if open. Does NOT open or focus the tab — call `btv.qf.show(name)` for that.
--
--   * `opts.title` (string) — the list title shown in the dock tab (defaults to `name`).
--   * `opts.action` (string) — `"r"` (default, replace in place) / `" "` (push a new
--     list onto the stack) / `"a"` (append to the current list).
--
-- Returns the name.
function btv.qf.list(name, items, opts)
  if type(name) ~= "string" or name == "" then
    error("btv.qf.list: name must be a non-empty string", 2)
  end
  if type(items) ~= "table" then
    error("btv.qf.list: items must be an array of entry dicts", 2)
  end
  opts = opts or {}
  local action = opts.action or "r"
  btv._set_qflist(items, nil, nil, action, opts.title or name, nil, name)
  return name
end

-- `btv.qf.show(name)`: open or focus the named list `name`'s bottom-dock tab — the
-- clean, window-independent reopen (no `set_current` / `on_next_tick` dance; the open is
-- sequenced server-side after any `btv.qf.list` queued in the same tick). Showing a name
-- with no items yet opens an empty tab. Returns the name.
function btv.qf.show(name)
  if type(name) ~= "string" or name == "" then
    error("btv.qf.show: name must be a non-empty string", 2)
  end
  btv._named_list_show(name)
  return name
end

-- `btv.qf.drop(name)`: forget the named list `name` — close its dock tab if open and
-- remove its contents from the editor. A no-op for a name that was never used.
function btv.qf.drop(name)
  if type(name) ~= "string" or name == "" then
    error("btv.qf.drop: name must be a non-empty string", 2)
  end
  btv._named_list_drop(name)
  return name
end

-- `btv.qf.open([height])` [wraps `:copen`]: open the quickfix window, optionally
-- `height` rows tall.
function btv.qf.open(height)
  vim.cmd(height and ("copen " .. height) or "copen")
end
-- `btv.qf.close()` [wraps `:cclose`]: close the quickfix window.
function btv.qf.close()
  vim.cmd("cclose")
end
-- `btv.qf.next()` [wraps `:cnext`]: jump to the next entry in the quickfix list.
function btv.qf.next()
  vim.cmd("cnext")
end
-- `btv.qf.prev()` [wraps `:cprev`]: jump to the previous entry in the quickfix list.
function btv.qf.prev()
  vim.cmd("cprev")
end
-- `btv.qf.older([count])` [wraps `:colder`]: go to an older quickfix list in the stack
-- (`count` lists back, default 1).
function btv.qf.older(count)
  vim.cmd(count and ("colder " .. count) or "colder")
end
-- `btv.qf.newer([count])` [wraps `:cnewer`]: go to a newer quickfix list in the stack
-- (`count` lists forward, default 1).
function btv.qf.newer(count)
  vim.cmd(count and ("cnewer " .. count) or "cnewer")
end
-- The location-list counterparts of the quickfix window / navigation wrappers
-- above — thin wrappers over the `:l*` ex-commands, acting on the CURRENT window's
-- location list rather than the global quickfix list.
-- `btv.qf.lopen([height])` [wraps `:lopen`]: open the location-list window, optionally
-- `height` rows tall.
function btv.qf.lopen(height)
  vim.cmd(height and ("lopen " .. height) or "lopen")
end
-- `btv.qf.lclose()` [wraps `:lclose`]: close the location-list window.
function btv.qf.lclose()
  vim.cmd("lclose")
end
-- `btv.qf.lnext()` [wraps `:lnext`]: jump to the next entry in the location list.
function btv.qf.lnext()
  vim.cmd("lnext")
end
-- `btv.qf.lprev()` [wraps `:lprev`]: jump to the previous entry in the location list.
function btv.qf.lprev()
  vim.cmd("lprev")
end
-- `btv.qf.lolder([count])` [wraps `:lolder`]: go to an older location list in the
-- window's stack (`count` lists back, default 1).
function btv.qf.lolder(count)
  vim.cmd(count and ("lolder " .. count) or "lolder")
end
-- `btv.qf.lnewer([count])` [wraps `:lnewer`]: go to a newer location list in the
-- window's stack (`count` lists forward, default 1).
function btv.qf.lnewer(count)
  vim.cmd(count and ("lnewer " .. count) or "lnewer")
end

-- btv._qf_make(cmd, efm, title, open, jump, loclist_win): the async :make / :grep
-- producer (dispatched from the server, which already expanded
-- 'makeprg'/'grepprg' and merged stderr into stdout via the shell). Spawn `cmd`
-- through the same job machinery as btv.run / vim.system; on exit, split its
-- combined output into lines and hand them to btv._qf_populate, which parses them
-- against `efm` into the quickfix list (or `loclist_win`'s location list — see
-- btv._set_qflist) and then opens the window / jumps to the first error per
-- `open`/`jump`. On a build with no local process spawn (the serverless web build)
-- the underlying spawn op fails loud, exactly like vim.system.
function btv._qf_make(cmd, efm, title, open, jump, loclist_win)
  local id = btv._next_cb_id()
  btv._cb_fns[id] = function(result)
    local out = (result.stdout or "") .. (result.stderr or "")
    local lines = vim.split(out, "\n", { plain = true })
    -- A trailing newline leaves one empty final segment; drop just that one so the
    -- parser doesn't see a phantom blank line (internal blanks are preserved).
    if lines[#lines] == "" then
      lines[#lines] = nil
    end
    btv._qf_populate(lines, efm, title, open, jump, loclist_win)
  end
  btv._bridge(id, function()
    btv._system_async(id, { "sh", "-c", cmd }, nil, nil, nil)
  end)
end
