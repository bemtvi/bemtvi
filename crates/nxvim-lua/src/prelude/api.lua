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
-- The editor's current mode() short code ("n"/"i"/"v"/…), refreshed alongside
-- the buffer mirror so vim.fn.mode() (and a %{} statusline expression calling it)
-- reflects the live mode.
vim._cur_mode = vim._cur_mode or "n"
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

function vim._set_bo_mirror(entries) vim._bo_mirror = entries or {} end

vim._wins = vim._wins or {}
vim._win_order = vim._win_order or { 1000 }
vim._next_win = vim._next_win or 1001
-- The id the next nvim_create_buf will mint, refreshed by the server (set_next_buf)
-- before each Lua entry. Seeded to 2 (the startup buffer is 1) so a create before
-- the first mirror push still predicts a fresh id.
vim._next_buf = vim._next_buf or 2
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

-- Extmark / namespace state (the decoration layer). `vim._namespaces` maps a
-- namespace name to its id and `vim._namespace_next` is the next id to mint, both
-- allocated entirely Lua-side (the sole allocator) so `nvim_create_namespace`
-- returns synchronously. `vim._extmarks[bufnr][ns][id]` mirrors each mark's
-- position/attrs for `nvim_buf_get_extmarks`; the server rebuilds it from the
-- authoritative core store before every chunk (so positions reflect edits), and
-- the set/del/clear wrappers write through it for read-after-write within a
-- chunk. `vim._extmark_next[bufnr][ns]` is the per-(buffer, namespace) id
-- allocator — persistent (never reset by the mirror refresh), so ids are never
-- reused, matching neovim.
vim._namespaces = vim._namespaces or {}
vim._namespace_next = vim._namespace_next or 1
vim._extmarks = vim._extmarks or {}
vim._extmark_next = vim._extmark_next or {}

-- Registered decoration providers, keyed by namespace id:
-- `{ on_start, on_buf, on_win, on_line, on_end }`. Populated by
-- `nvim_set_decoration_provider`; the server drives the entries each redraw (see
-- the `vim._decor_*` drivers below).
vim._decoration_providers = vim._decoration_providers or {}

-- Receive the extmark mirror: `entries[bufnr]` is the array of that buffer's
-- marks the server pushed from core (positions already shifted for any edits).
-- Rebuilds `vim._extmarks` from the authoritative state; the persistent
-- allocator (`vim._extmark_next`) is deliberately untouched.
function vim._set_extmark_mirror(entries)
  local marks = {}
  for bufnr, list in pairs(entries or {}) do
    local by_ns = {}
    for _, m in ipairs(list) do
      by_ns[m.ns] = by_ns[m.ns] or {}
      by_ns[m.ns][m.id] = {
        row = m.row,
        col = m.col,
        end_row = m.end_row,
        end_col = m.end_col,
        hl_group = m.hl_group,
        priority = m.priority,
      }
    end
    marks[bufnr] = by_ns
  end
  vim._extmarks = marks
end

-- vim._call_ctx_lock: set while inside an nvim_buf_call / nvim_win_call whose
-- target differs from the real current buffer/window (see those functions). nxvim
-- runs the callback in-VM with the "current" mirror swapped, so READS resolve to
-- the target and explicit-handle WRITES queue the right handle — but a mutation
-- that binds to "current" only at DRAIN time (an ex-command, feedkeys, an LSP buf
-- request) would run against the REAL current, which the call never switched.
-- Those funnels call vim._assert_call_ctx to fail loud rather than silently
-- mutate the wrong context (the no-silent-stub rule applied to a known gap).
vim._call_ctx_lock = false

function vim._assert_call_ctx(what)
  if vim._call_ctx_lock then
    error(
      "nvim_buf_call/nvim_win_call: "
        .. what
        .. " inside the callback would run "
        .. "against the real current buffer/window, not the one passed to the "
        .. "call — nxvim cannot retarget a queued mutation. Run it outside the "
        .. "call, or use an explicit-handle API.",
      0
    )
  end
end

-- Wrap the context-binding LSP / diagnostic bridges (Rust funnels that drain
-- against the current buffer/window) so they honor the call-context lock. Done
-- here, before lsp.lua defines the `vim.lsp.buf.*` wrappers that route through
-- them, so every entry is covered at the single chokepoint. (These bridges were
-- installed from Rust before the prelude loads, so they exist to be wrapped.)
do
  local guards = {
    _lsp_buf = "an LSP buf request",
    _lsp_buf_format = "vim.lsp.buf.format",
    _lsp_buf_code_action = "vim.lsp.buf.code_action",
    _lsp_buf_rename = "vim.lsp.buf.rename",
    _diagnostic_goto = "a diagnostic jump",
    _diagnostic_setloclist = "vim.diagnostic.setloclist",
  }
  for name, what in pairs(guards) do
    local raw = vim[name]
    if raw then
      vim[name] = function(...)
        vim._assert_call_ctx(what)
        return raw(...)
      end
    end
  end
  -- nvim_command is the Rust-installed ex-command funnel (vim.cmd's sibling); it
  -- runs against the real current buffer/window, so guard it the same way.
  local raw_command = vim.api.nvim_command
  if raw_command then
    vim.api.nvim_command = function(cmd)
      vim._assert_call_ctx("an ex-command (nvim_command)")
      return raw_command(cmd)
    end
  end
end

-- Rust→Lua mirror of the core highlight registry, refreshed by the server
-- (vim._set_hl_mirror) when the registry changes. Keyed by group name ->
-- { fg, bg, sp (0xRRGGBB ints), bold/italic/… (true when set), link (string) }.
-- Backs vim.api.nvim_get_hl; a link group carries only `link` (its own attrs are
-- ignored, matching neovim), and nvim_get_hl follows the chain for the resolved
-- form. Seeded empty so a read before the first push answers `{}` (no theme yet).
vim._hl_defs = vim._hl_defs or {}
function vim._set_hl_mirror(entries) vim._hl_defs = entries or {} end

function vim._set_buf_mirror(entries, row, col, win, wins, next_win, mode)
  vim._cur_mode = mode or "n"
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
  if strict and (orig > n or i < 0) then error("Index out of bounds", 3) end
  if i < 0 then
    i = 0
  elseif i > n then
    i = n
  end
  return i
end

function vim.api.nvim_create_user_command(name, command, _opts) vim._user_commands[name] = command end

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
-- An autocmd registered with `opts.once` (`:autocmd … ++once`) fires once and is
-- then dropped — collected during the pass and removed after it, so the live
-- iteration isn't mutated underneath `ipairs`.
function vim._fire(event, pattern, buf, file, data)
  local fired -- ids of `++once` autocmds to drop after this pass (nil = none)
  for _, au in ipairs(vim._autocmds) do
    local ev = au.event
    local ev_ok = ev == event or (type(ev) == "table" and vim.tbl_contains(ev, event))
    if ev_ok then
      local pat = au.opts.pattern
      local pat_ok = pat == nil
        or pat == "*"
        or pat == pattern
        or (type(pat) == "table" and vim.tbl_contains(pat, pattern))
      local buf_ok = au.buffer == nil or au.buffer == buf
      if pat_ok and buf_ok then
        local cb = au.opts.callback
        if type(cb) == "function" then
          cb({
            id = au.id,
            event = event,
            match = pattern,
            buf = buf,
            file = file or pattern,
            data = data,
          })
        elseif type(au.opts.command) == "string" then
          vim.cmd(au.opts.command)
        end
        if au.opts.once then
          fired = fired or {}
          fired[au.id] = true
        end
      end
    end
  end
  if fired then
    vim._autocmds = vim.tbl_filter(function(au) return not fired[au.id] end, vim._autocmds)
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
      for _, p in ipairs(patterns) do
        vim._fire(ev, p, buf, file)
      end
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
  for nm, id in pairs(vim._augroups) do
    group_name[id] = nm
  end
  local out = {}
  for _, au in ipairs(vim._autocmds) do
    -- match if any requested event is among the autocmd's events
    local ev_ok = true
    if want_events then
      ev_ok = false
      local evs = type(au.event) == "table" and au.event or { au.event }
      for _, w in ipairs(want_events) do
        if vim.tbl_contains(evs, w) then
          ev_ok = true
          break
        end
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

-- ----- :autocmd / :augroup / :doautocmd ex-commands --------------------------
-- The Vimscript front-end onto the autocmd registry above. The core ex-command
-- dispatch doesn't recognize these, so it defers them to the server, which parses
-- the argument line here and drives the same vim._autocmds / vim._augroups store
-- the nvim_* API uses — one store, two front-ends. Each `vim._ex_*` returns the
-- text the server surfaces: "" (nothing), a one-line message/error (echoed), or a
-- multi-line listing (shown in a panel).

-- The "current augroup" set by `:augroup {name}` and cleared by `:augroup END`.
-- It persists across command invocations, exactly like Vim's parser state, so a
-- block of `:autocmd`s between the two lands in that group.
vim._cur_augroup = nil

-- Does `au` match the group / event-list / pattern-list filter? A nil filter
-- field means "any" (so a bare `:autocmd!` clears everything in scope). Events
-- and patterns are lists; a "*" event matches any event. A pattern-less autocmd
-- is treated as "*" for matching, mirroring vim._fire's pattern rule.
local function au_matches(au, group, events, patterns)
  if group ~= nil and au.group ~= group then return false end
  if events ~= nil and not vim.tbl_contains(events, "*") then
    local evs = type(au.event) == "table" and au.event or { au.event }
    local hit = false
    for _, w in ipairs(events) do
      if vim.tbl_contains(evs, w) then
        hit = true
        break
      end
    end
    if not hit then return false end
  end
  if patterns ~= nil then
    local pat = au.opts.pattern
    if pat == nil then pat = "*" end
    local pats = type(pat) == "table" and pat or { pat }
    local hit = false
    for _, w in ipairs(patterns) do
      if vim.tbl_contains(pats, w) then
        hit = true
        break
      end
    end
    if not hit then return false end
  end
  return true
end

-- Render the autocmds matching the filter as a `:autocmd`-style listing.
local function au_list(group, events, patterns)
  local gname = {}
  for nm, id in pairs(vim._augroups) do
    gname[id] = nm
  end
  local lines = { "--- Autocommands ---" }
  for _, au in ipairs(vim._autocmds) do
    if au_matches(au, group, events, patterns) then
      local evs = type(au.event) == "table" and table.concat(au.event, ",") or tostring(au.event)
      local pat = au.opts.pattern
      pat = type(pat) == "table" and table.concat(pat, ",") or (pat or "*")
      local g = au.group and (gname[au.group] or ("group#" .. au.group)) or ""
      local body = au.opts.command or "<callback>"
      lines[#lines + 1] = string.format("%-10s %-12s %-16s %s", g, evs, pat, body)
    end
  end
  return table.concat(lines, "\n")
end

-- Pull the first whitespace-delimited word off `s`, returning the word and the
-- trimmed remainder (both "" when `s` is empty).
local function take_word(s)
  local w = s:match("^(%S+)")
  if not w then return "", "" end
  return w, vim.trim(s:sub(#w + 1))
end

-- :aug[roup][!] {name} | END. Without a bang: `END`/`end` leaves the current
-- group, an empty arg reports it, and any other name enters that group (creating
-- it without clearing — `:augroup` is not destructive). With a bang,
-- `:augroup! {name}` deletes the group and every autocmd in it.
function vim._ex_augroup(bang, args)
  args = vim.trim(args)
  if bang then
    if args == "" then return "E471: Argument required" end
    local id = vim._augroups[args]
    if id then
      vim._autocmds = vim.tbl_filter(function(au) return au.group ~= id end, vim._autocmds)
      vim._augroups[args] = nil
      if vim._cur_augroup == args then vim._cur_augroup = nil end
    end
    return ""
  end
  if args == "" then return vim._cur_augroup and ("augroup " .. vim._cur_augroup) or "" end
  if args == "END" or args == "end" then
    vim._cur_augroup = nil
    return ""
  end
  vim.api.nvim_create_augroup(args, { clear = false })
  vim._cur_augroup = args
  return ""
end

-- :au[tocmd][!] [group] [event[,event…]] [pat[,pat…]] [++once] [++nested] [cmd]
-- A leading word that names an existing augroup is the group; otherwise the
-- current `:augroup` (if any) applies. With a bang, the autocmds matching the
-- group/event/pattern filter are removed first; with a trailing command, a new
-- autocmd is then registered. With no command and no bang it lists the matching
-- autocmds. `<buffer>` as the pattern registers a buffer-local autocmd for the
-- current buffer. `++once` fires once then self-removes (honored by vim._fire);
-- `++nested` is accepted (nxvim already lets events nest).
function vim._ex_autocmd(bang, args)
  local rest = vim.trim(args)

  -- Optional leading group: only when the first word names an existing augroup.
  local group = vim._cur_augroup and vim._augroups[vim._cur_augroup] or nil
  local first = rest:match("^(%S+)")
  if first and vim._augroups[first] then
    group = vim._augroups[first]
    rest = vim.trim(rest:sub(#first + 1))
  end

  -- Event list (comma-separated). Absent only on a bare `:au` / `:au!`.
  local ev_word
  ev_word, rest = take_word(rest)
  local events = ev_word ~= "" and vim.split(ev_word, ",", { plain = true, trimempty = true })
    or nil

  -- Pattern list (comma-separated), or `<buffer>` for a buffer-local autocmd.
  local pat_word
  pat_word, rest = take_word(rest)
  local patterns, buffer
  if pat_word == "<buffer>" then
    buffer = 0 -- nvim_create_autocmd resolves 0 → current buffer
  elseif pat_word ~= "" then
    patterns = vim.split(pat_word, ",", { plain = true, trimempty = true })
  end

  -- ++once / ++nested flags precede the command body.
  local once, nested = false, false
  while true do
    local flag = rest:match("^(%+%+%S+)")
    if flag == "++once" then
      once = true
    elseif flag == "++nested" then
      nested = true
    else
      break
    end
    rest = vim.trim(rest:sub(#flag + 1))
  end

  local cmd = rest -- the remainder is the ex-command body (may contain spaces)

  if bang then
    -- A bang clears matching autocmds before any (re)definition. The scope is
    -- the resolved group (the current/explicit augroup, or any when none), the
    -- event list (nil/"*" = any), and the pattern list (nil = any).
    vim._autocmds = vim.tbl_filter(
      function(au) return not au_matches(au, group, events, patterns) end,
      vim._autocmds
    )
  end

  if cmd ~= "" then
    if not events then return "E216: No such event: a {event} is required to define an autocmd" end
    vim.api.nvim_create_autocmd(#events == 1 and events[1] or events, {
      group = group,
      pattern = patterns and (#patterns == 1 and patterns[1] or patterns) or nil,
      buffer = buffer,
      command = cmd,
      once = once,
      nested = nested,
    })
    return ""
  end

  -- No command: a bang was a pure clear (nothing to show); otherwise list.
  if bang then return "" end
  return au_list(group, events, patterns)
end

-- :doau[tocmd] {event} [pattern]: fire `event` now (optionally for a pattern),
-- the manual analogue of nvim_exec_autocmds. The optional [group] argument vim
-- accepts is not supported — vim._fire has no group filter — so the first word
-- is always the event; pass the event directly.
function vim._ex_doautocmd(args)
  args = vim.trim(args):gsub("^<nomodeline>%s*", "")
  local event, rest = take_word(args)
  if event == "" then return "E217: Can't execute autocommands for ALL events" end
  local pattern = rest ~= "" and rest or nil
  vim.api.nvim_exec_autocmds(event, { pattern = pattern })
  return ""
end

-- The three command families whose report/listing text is produced synchronously
-- in *this* Lua layer (the vim._ex_* drivers above), keyed by every abbreviation
-- the core ex-dispatch accepts (excmd.rs is_autocmd / is_augroup / is_doautocmd —
-- kept in lock-step). These are the only commands nvim_exec can faithfully capture
-- output from; everything else runs through the queued vim.cmd path, whose output
-- (if any) is asynchronous and not readable back here.
local AUTOCMD_HEADS = {}
for _, w in ipairs({ "au", "aut", "auto", "autoc", "autocm", "autocmd" }) do
  AUTOCMD_HEADS[w] = "au"
end
for _, w in ipairs({ "aug", "augr", "augro", "augrou", "augroup" }) do
  AUTOCMD_HEADS[w] = "aug"
end
for _, w in ipairs({ "doau", "doaut", "doauto", "doautoc", "doautocm", "doautocmd" }) do
  AUTOCMD_HEADS[w] = "doau"
end

-- nvim_exec(src, output): run the ex-command(s) in `src` (one or more newline-
-- separated lines) and, when `output` is truthy, return the text they produced as
-- a single string; otherwise return "". This is the legacy (pre-0.9) form lualine
-- calls — `nvim_exec('au lualine <event> <pat>', true):find(cmd)` — to read the
-- `:au` listing back and dedupe its autocmds.
--
-- nxvim can only *capture* output from the command families whose listing/report
-- text is generated synchronously in Lua (the autocmd group). Any other command is
-- still run, via the normal queued `vim.cmd` path, but its message-line output is
-- asynchronous and cannot be read back here. So requesting `output` capture of a
-- non-capturable command FAILS LOUD rather than returning a misleading "" — a stub
-- that faked an empty capture would make a caller's `:find` on the result silently
-- wrong, exactly the "quietly succeeds" failure nxvim forbids.
local function exec_capture(src, output)
  local captured = {}
  for line in (tostring(src) .. "\n"):gmatch("([^\n]*)\n") do
    local cmd = vim.trim(line):gsub("^:+%s*", "") -- tolerate a leading ':'
    if cmd ~= "" and cmd:sub(1, 1) ~= '"' then -- skip blanks and " comment lines
      local head, rest = cmd:match("^(%S+)%s*(.*)$")
      local bang = head:sub(-1) == "!"
      if bang then head = head:sub(1, -2) end
      local kind = AUTOCMD_HEADS[head]
      local text
      if kind == "au" then
        text = vim._ex_autocmd(bang, rest)
      elseif kind == "aug" then
        text = vim._ex_augroup(bang, rest)
      elseif kind == "doau" then
        text = vim._ex_doautocmd(rest)
      elseif output then
        error("nvim_exec: output capture is unsupported for ':" .. head .. "'", 0)
      else
        vim.cmd(cmd) -- run it the normal (queued) way; nothing to capture
      end
      if text and text ~= "" then captured[#captured + 1] = text end
    end
  end
  return output and table.concat(captured, "\n") or ""
end

function vim.api.nvim_exec(src, output) return exec_capture(src, output) end

-- nvim_exec2(src, opts): the 0.9+ replacement for nvim_exec — same execution, but
-- the captured text is returned under `.output` (only when `opts.output` is set).
function vim.api.nvim_exec2(src, opts)
  opts = opts or {}
  local out = exec_capture(src, opts.output)
  return opts.output and { output = out } or {}
end

-- nvim_buf_get_name(bufnr): the snapshot buffer's name when `bufnr` is 0/nil or
-- matches the snapshot, else "". Snapshot-backed (vim._cur_buf) as an interim
-- until a real per-bufnr registry exists. (A separate, core-backed
-- nvim_buf_get_name *RPC* method serves remote clients; this is the in-VM Lua
-- binding autocmd callbacks reach for.)
function vim.api.nvim_buf_get_name(bufnr)
  local cur = vim._cur_buf or { bufnr = 0, name = "" }
  if bufnr == nil or bufnr == 0 or bufnr == cur.bufnr then return cur.name end
  -- A non-current buffer: resolve its name from the full buffer mirror (which
  -- carries every open buffer), so e.g. a custom 'tabline' can name a buffer
  -- shown in another tab. Empty for an unknown handle.
  local b = (vim._bufs or {})[bufnr]
  return (b and b.name) or ""
end

-- A few more vim.api the configs touch. `nvim_get_current_buf` resolves against
-- the single-buffer snapshot (faithful: it returns the real current buffer). The
-- window/cursor/line-access getters (Phase 6) read the `vim._bufs` / `vim._cur_*`
-- mirror the server refreshes before running Lua, so they return live state, and
-- `nvim_buf_set_lines` write-through updates the mirror then queues the real edit.
-- (`nvim_create_augroup`/`_autocmd`/`nvim_buf_get_name`/`nvim_echo` are the real,
-- behavior-carrying ones, defined elsewhere.)
function vim.api.nvim_get_current_buf() return (vim._cur_buf or {}).bufnr or 0 end

-- nvim_get_mode(): the editor's current mode, read from the `vim._cur_mode`
-- snapshot the server refreshes before each Lua entry. `blocking` is always
-- false — the in-VM Lua bindings only run when the server is between keys, so it
-- is never blocked on input here. (The dedicated RPC method serves remote clients.)
function vim.api.nvim_get_mode() return { mode = vim._cur_mode or "n", blocking = false } end

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
  if w then
    w.row, w.col = row, col
  end
  if win == (vim._cur_win or 1000) then vim._cur_cursor = { row = row, col = col } end
end

-- nvim_get_current_line(): the text of the line the cursor is on in the current
-- window/buffer (no trailing newline). Composed from the cursor row and the
-- buffer's lines — nvim-cmp reads this when it builds a completion `context`
-- (cmp.utils.api.get_current_line), which runs as soon as `cmp.setup` spins up
-- its core, so a missing builtin broke cmp (and every cmp source) at load.
function vim.api.nvim_get_current_line()
  local row = vim.api.nvim_win_get_cursor(0)[1] -- 1-based
  local lines = vim.api.nvim_buf_get_lines(0, row - 1, row, false)
  return lines[1] or ""
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

-- ----- window view / screen position (the vim.fn.* which-key reads) ----------
-- These resolve the *current* window — which `nvim_win_call(win, fn)` swaps to its
-- target for the duration of the call (vim._cur_win / vim._cur_cursor), so which-
-- key's `nvim_win_call(popup, vim.fn.winsaveview)` reads the popup's view.

-- vim.fn.winsaveview(): the current window's view — cursor position, scroll
-- (`topline`/`leftcol`), and the cursor-restore fields neovim returns. nxvim has
-- no separate `curswant`/`coladd`/`skipcol` state, so those mirror `col` / are 0.
function vim.fn.winsaveview()
  local win = vim._cur_win or 1000
  local w = (vim._wins or {})[win] or {}
  local c = vim._cur_cursor or {}
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

-- vim.fn.winrestview(view): restore the current window's view from a (partial)
-- dict — the inverse of winsaveview. `lnum`/`col` move the cursor; `topline`
-- scrolls (1-based -> the server's 0-based top); `leftcol` is mirrored. Both
-- mutations queue against the concrete current-window handle, so they are honored
-- even inside the `nvim_win_call` context lock (an explicit-handle write, not a
-- "current"-bound one). which-key uses it to scroll its popup.
function vim.fn.winrestview(view)
  view = view or {}
  local win = vim._cur_win or 1000
  if view.lnum then vim.api.nvim_win_set_cursor(win, { view.lnum, view.col or 0 }) end
  local w = (vim._wins or {})[win]
  if view.topline then
    vim._win_set_topline(win, math.max(0, view.topline - 1))
    if w then w.topline = view.topline end -- write-through for read-after-set
  end
  if view.leftcol and w then w.leftcol = view.leftcol end
end

-- vim.fn.screenrow() / screencol(): the cursor's 1-based position on the whole
-- screen, mirrored by the server (vim._cur_screenrow / _cur_screencol) for the
-- focused window. which-key reads them to avoid drawing its popup over the cursor.
function vim.fn.screenrow() return vim._cur_screenrow or 0 end
function vim.fn.screencol() return vim._cur_screencol or 0 end

-- nvim_win_call(win, fn) / nvim_buf_call(buf, fn): run `fn` as if `win`/`buf`
-- were current, returning fn's result. In neovim these temporarily switch the
-- editor's current window/buffer for the duration of the callback; in nxvim the
-- callback runs synchronously in-VM, where "current" is the mirror the server
-- pushed (vim._cur_win / vim._cur_buf / vim._cur_cursor). So these swap that
-- mirror context for the call, run `fn`, and restore it — which makes every
-- *read* inside the callback (nvim_win_get_cursor, nvim_get_current_buf,
-- vim.fn.line/col/winnr, …) resolve against the requested window/buffer, and
-- every explicit-handle write (nvim_buf_set_lines(buf, …), nvim_win_set_cursor(
-- win, …)) resolve the swapped mirror at call time and queue that concrete
-- handle — so it, too, targets the right place.
--
-- What nxvim CAN'T do is retarget a mutation that binds to "current" only at
-- DRAIN time — an ex-command (vim.cmd), feedkeys, or an LSP buf request — since
-- the queued-ops model applies those against the editor's real current
-- buffer/window after the chunk, which this call never actually switched. Rather
-- than silently mutate the wrong context, `vim._call_ctx_lock` is set for the
-- duration of a call whose target differs from the real current, and those
-- funnels raise while it is set (see vim._assert_call_ctx). which-key uses these
-- calls to read a window's view/dimensions, which is fully faithful.
function vim.api.nvim_win_call(win, fn)
  win = resolve_win(win)
  local saved_win, saved_cursor, saved_buf = vim._cur_win, vim._cur_cursor, vim._cur_buf
  local saved_lock = vim._call_ctx_lock
  local w = (vim._wins or {})[win]
  vim._cur_win = win
  if w then
    vim._cur_cursor = { row = w.row or 1, col = w.col or 0 }
    local b = (vim._bufs or {})[w.buffer]
    vim._cur_buf = { bufnr = w.buffer, name = (b and b.name) or "", filetype = "" }
  end
  -- Lock context-dependent mutations when this actually switches windows (stay
  -- locked if an enclosing call already did).
  vim._call_ctx_lock = saved_lock or (win ~= saved_win)
  local ok, ret = pcall(fn)
  vim._cur_win, vim._cur_cursor, vim._cur_buf = saved_win, saved_cursor, saved_buf
  vim._call_ctx_lock = saved_lock
  if not ok then error(ret, 0) end
  return ret
end

function vim.api.nvim_buf_call(buf, fn)
  buf = vim._resolve_bufnr(buf)
  local saved_buf = vim._cur_buf
  local saved_lock = vim._call_ctx_lock
  local b = (vim._bufs or {})[buf]
  vim._cur_buf = {
    bufnr = buf,
    name = (b and b.name) or "",
    filetype = (saved_buf and buf == saved_buf.bufnr) and saved_buf.filetype or "",
  }
  vim._call_ctx_lock = saved_lock or (buf ~= (saved_buf and saved_buf.bufnr))
  local ok, ret = pcall(fn)
  vim._cur_buf = saved_buf
  vim._call_ctx_lock = saved_lock
  if not ok then error(ret, 0) end
  return ret
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

function vim.api.nvim_tabpage_is_valid(tab) return (vim._tabs or {})[resolve_tab(tab)] ~= nil end

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
  none = true,
  single = true,
  rounded = true,
  double = true,
  solid = true,
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
    if not config.width or not config.height or config.width <= 0 or config.height <= 0 then
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
    local vertical = config.vertical == true or config.split == "left" or config.split == "right"
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
    id = id,
    buffer = buf,
    row = 1,
    col = 0,
    width = 0,
    height = 0,
    number = number,
    relativenumber = relativenumber,
    float = float_record,
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
  if
    config.border ~= nil
    and (type(config.border) ~= "string" or not FLOAT_BORDER[config.border])
  then
    error(
      "nvim_win_set_config: 'border' style '" .. tostring(config.border) .. "' is not supported yet",
      2
    )
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
      local f = w.float
        or {
          relative = "editor",
          anchor = "NW",
          row = 0,
          col = 0,
          width = 1,
          height = 1,
          zindex = 50,
          focusable = true,
          border = "none",
        }
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
  number = "number",
  nu = "number",
  relativenumber = "relativenumber",
  rnu = "relativenumber",
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
function vim.api.nvim_win_set_option(win, name, value) vim.wo[resolve_win(win)][name] = value end

function vim.api.nvim_buf_is_loaded(bufnr) return vim._bufs[vim._resolve_bufnr(bufnr)] ~= nil end

-- nvim_buf_is_valid: whether the handle names a buffer nxvim knows about. With no
-- separate "valid but unloaded" notion in the snapshot mirror yet, this matches
-- is_loaded (every mirrored buffer is loaded).
function vim.api.nvim_buf_is_valid(bufnr) return vim._bufs[vim._resolve_bufnr(bufnr)] ~= nil end

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
  for i = 1, s do
    updated[#updated + 1] = lines[i]
  end
  for i = 1, #repl do
    updated[#updated + 1] = repl[i]
  end
  for i = e + 1, n do
    updated[#updated + 1] = lines[i]
  end
  buf.lines = updated
  vim._buf_set_lines(id, start, end_, repl)
  -- Fire on_bytes synchronously for any attached parser, matching neovim: an
  -- edit-then-:parse() within one entry must see the edit (the vim.treesitter
  -- LanguageTree's on_bytes edits its trees). The server suppresses its own
  -- on_bytes for this same edit (apply_buf_op truncates the buffer's treesitter
  -- journal after applying), so the tree is edited exactly once. Byte offsets come
  -- from the *old* line bytes (`lines`, captured before the splice); each editable
  -- line carries its trailing newline, so the trailing-`\n` line model and the
  -- server's byte math agree. The on_bytes row/col fields are relative deltas
  -- (start at the replaced range's first line, column 0 — set_lines is linewise).
  if vim._buf_attached[id] then
    local start_byte = 0
    for i = 1, s do
      start_byte = start_byte + #lines[i] + 1
    end
    local old_byte = 0
    for i = s + 1, e do
      old_byte = old_byte + #lines[i] + 1
    end
    local new_byte = 0
    for i = 1, #repl do
      new_byte = new_byte + #repl[i] + 1
    end
    vim._buf_bytes_changed(id, 0, s, 0, start_byte, e - s, 0, old_byte, #repl, 0, new_byte)
  end
end

-- ===== Extmarks / decoration layer =====================================
-- See docs/specs/2026-06-07-extmark-decoration-layer-design.md. v1 carries the
-- highlight-relevant attrs only; virtual text / signs / conceal / ephemeral are
-- not modelled yet and are rejected loudly rather than silently ignored.

-- nvim_create_namespace(name): create-or-get a namespace id by name (an empty /
-- nil name mints a fresh anonymous one each call). Ids are allocated Lua-side, so
-- the call returns synchronously; the server only ever sees the id on a mark.
function vim.api.nvim_create_namespace(name)
  name = name or ""
  if name ~= "" and vim._namespaces[name] then return vim._namespaces[name] end
  local id = vim._namespace_next
  vim._namespace_next = id + 1
  if name ~= "" then vim._namespaces[name] = id end
  return id
end

-- The extmark options v1 RENDERS: position, span, highlight, priority. `end_line`
-- is neovim's deprecated alias for `end_row` (cmp's decoration provider uses it);
-- `ephemeral` marks a single-frame decoration a provider places during redraw.
local EXTMARK_OPT_OK = {
  id = true,
  end_row = true,
  end_line = true,
  end_col = true,
  hl_group = true,
  priority = true,
  ephemeral = true,
}
-- Decoration options nxvim ACCEPTS and STORES (so nvim_buf_get_extmarks(…,
-- {details=true}) returns them) but does NOT yet render — virtual text, virtual
-- lines, signs, conceal, and the line/gravity flags. A documented approximation
-- (the matchadd / winblend pattern): a plugin that decorates with virtual text
-- (telescope's right-aligned result counter, a preview overlay, gitsigns) loads
-- and runs; the supplementary glyphs just aren't painted yet. Rejecting them loud
-- (the v1 behavior) would instead break the plugin's render path. The core
-- extmark store still tracks the mark's POSITION (for get_extmarks), only the
-- decoration payload is unrendered.
local EXTMARK_OPT_DECORATION = {
  virt_text = true,
  virt_text_pos = true,
  virt_text_win_col = true,
  virt_text_hide = true,
  virt_text_repeat_linebreak = true,
  hl_mode = true,
  hl_eol = true,
  virt_lines = true,
  virt_lines_above = true,
  virt_lines_leftcol = true,
  sign_text = true,
  sign_hl_group = true,
  number_hl_group = true,
  line_hl_group = true,
  cursorline_hl_group = true,
  conceal = true,
  spell = true,
  ui_watched = true,
  url = true,
  right_gravity = true,
  end_right_gravity = true,
  strict = true,
  undo_restore = true,
  invalidate = true,
}

-- nvim_buf_set_extmark(buffer, ns, line, col, opts) -> id. `line`/`col` are
-- 0-based (col a byte offset). Returns the (allocated or given) mark id. Queues
-- the real mutation for the server, which converts positions to byte offsets.
function vim.api.nvim_buf_set_extmark(buffer, ns, line, col, opts)
  local b = vim._resolve_bufnr(buffer)
  opts = opts or {}
  -- Collect any accepted-but-unrendered decoration payload so a details read can
  -- return it; reject only a key from neither set (a true unknown).
  local decoration = nil
  for k in pairs(opts) do
    if not EXTMARK_OPT_OK[k] then
      if EXTMARK_OPT_DECORATION[k] then
        decoration = decoration or {}
        decoration[k] = opts[k]
      else
        error("nvim_buf_set_extmark: option '" .. tostring(k) .. "' is not supported yet", 2)
      end
    end
  end
  local hl_group = opts.hl_group
  if hl_group ~= nil and type(hl_group) ~= "string" then
    error("nvim_buf_set_extmark: hl_group must be a string (group lists not supported yet)", 2)
  end
  -- `end_line` is the deprecated alias for `end_row`; honor either.
  local end_row = opts.end_row
  if end_row == nil then end_row = opts.end_line end
  if (end_row == nil) ~= (opts.end_col == nil) then
    error("nvim_buf_set_extmark: end_row/end_line and end_col must be given together", 2)
  end
  local priority = opts.priority or 4096

  -- An ephemeral mark is a single-frame decoration: it is only valid while the
  -- server is driving a decoration provider (neovim errors otherwise), carries no
  -- id, and bypasses the persistent store/mirror entirely — the server folds it
  -- into the per-frame ephemeral store it clears each redraw.
  if opts.ephemeral then
    if not vim._in_decoration then
      error(
        "nvim_buf_set_extmark: ephemeral marks are only valid inside a decoration provider callback",
        2
      )
    end
    vim._extmark_set_ephemeral(b, ns, line, col, end_row, opts.end_col, hl_group, priority)
    return -1
  end

  vim._extmark_next[b] = vim._extmark_next[b] or {}
  local mark_id = opts.id or vim._extmark_next[b][ns] or 1
  -- Advance the allocator past this id so a later auto-id can't collide.
  vim._extmark_next[b][ns] = math.max(vim._extmark_next[b][ns] or 1, mark_id + 1)

  -- Write-through the mirror (read-after-write within this chunk).
  vim._extmarks[b] = vim._extmarks[b] or {}
  vim._extmarks[b][ns] = vim._extmarks[b][ns] or {}
  vim._extmarks[b][ns][mark_id] = {
    row = line,
    col = col,
    end_row = end_row,
    end_col = opts.end_col,
    hl_group = hl_group,
    priority = priority,
    decoration = decoration,
  }
  vim._extmark_set(b, ns, mark_id, line, col, end_row, opts.end_col, hl_group, priority)
  return mark_id
end

-- nvim_buf_del_extmark(buffer, ns, id) -> bool (whether it existed).
function vim.api.nvim_buf_del_extmark(buffer, ns, id)
  local b = vim._resolve_bufnr(buffer)
  local marks = vim._extmarks[b] and vim._extmarks[b][ns]
  local existed = marks ~= nil and marks[id] ~= nil
  if existed then marks[id] = nil end
  vim._extmark_del(b, ns, id)
  return existed
end

-- nvim_buf_clear_namespace(buffer, ns, line_start, line_end): drop ns's marks in
-- the line range (`line_end == -1` ⇒ to end of buffer). `ns == -1` clears every
-- namespace, matching neovim.
function vim.api.nvim_buf_clear_namespace(buffer, ns, line_start, line_end)
  local b = vim._resolve_bufnr(buffer)
  if ns == -1 then
    for nsid in pairs(vim._extmarks[b] or {}) do
      vim.api.nvim_buf_clear_namespace(b, nsid, line_start, line_end)
    end
    return
  end
  local marks = vim._extmarks[b] and vim._extmarks[b][ns]
  if marks then
    for id, m in pairs(marks) do
      if line_end == -1 or (m.row >= line_start and m.row < line_end) then marks[id] = nil end
    end
  end
  vim._extmark_clear(b, ns, line_start, line_end)
end

-- ----- Decoration providers --------------------------------------------------
-- A decoration provider is a per-redraw callback set. The server drives it each
-- frame: on_start(tick) once, then for every window on_win and (unless on_win
-- returned false) on_line per visible row, then on_end(tick). Inside on_win /
-- on_line the provider places EPHEMERAL extmarks — single-frame highlights that
-- the server folds into a store it clears before the next redraw. nvim-cmp uses
-- this to highlight the matched characters of each completion entry.

-- nvim_set_decoration_provider(ns, opts): register a provider for namespace `ns`
-- ({ on_start, on_buf, on_win, on_line, on_end }, each a function). An empty
-- `opts` deregisters it (neovim's clear form). Each callback must be a function;
-- an unknown key is rejected loud rather than silently dropped.
function vim.api.nvim_set_decoration_provider(ns, opts)
  opts = opts or {}
  if next(opts) == nil then
    vim._decoration_providers[ns] = nil
    return
  end
  local KNOWN = { on_start = true, on_buf = true, on_win = true, on_line = true, on_end = true }
  local prov = {}
  for k, v in pairs(opts) do
    if not KNOWN[k] then
      error("nvim_set_decoration_provider: unknown option '" .. tostring(k) .. "'", 2)
    end
    if type(v) ~= "function" then
      error("nvim_set_decoration_provider: '" .. k .. "' must be a function", 2)
    end
    prov[k] = v
  end
  vim._decoration_providers[ns] = prov
end

-- Whether any provider is registered — the server's per-frame fast-path gate, so
-- a redraw with no provider skips the whole drive (and its buffer-mirror push).
function vim._has_decoration_providers() return next(vim._decoration_providers) ~= nil end

-- Begin a decoration frame: each provider's on_start(tick).
function vim._decor_frame_start(tick)
  for _, p in pairs(vim._decoration_providers) do
    if p.on_start then p.on_start("start", tick) end
  end
end

-- Drive every provider for one window: on_win("win", ns, win, buf, top, bot),
-- then — unless on_win returned false — on_line("line", ns, win, buf, row) for
-- each 0-based buffer row in [top, bot]. Ephemeral extmarks the callbacks place
-- are only accepted while `vim._in_decoration` is set. A provider that errors is
-- surfaced (its message returned for the server to echo) and dropped, matching
-- neovim — so a broken provider fails loud once instead of every frame. Returns
-- "" when all ran clean.
function vim._decor_on_win(win, buf, top, bot)
  vim._in_decoration = true
  local err, dead
  for ns, p in pairs(vim._decoration_providers) do
    local disabled = false
    if p.on_win then
      local ok, ret = pcall(p.on_win, "win", ns, win, buf, top, bot)
      if not ok then
        err = err or tostring(ret)
        dead = dead or {}
        dead[ns] = true
        disabled = true
      elseif ret == false then
        disabled = true -- provider opted this window out of per-line callbacks
      end
    end
    if p.on_line and not disabled then
      for row = top, bot do
        local ok, e = pcall(p.on_line, "line", ns, win, buf, row)
        if not ok then
          err = err or tostring(e)
          dead = dead or {}
          dead[ns] = true
          break
        end
      end
    end
  end
  vim._in_decoration = false
  if dead then
    for ns in pairs(dead) do
      vim._decoration_providers[ns] = nil
    end
  end
  return err or ""
end

-- End a decoration frame: each provider's on_end(tick).
function vim._decor_frame_end(tick)
  for _, p in pairs(vim._decoration_providers) do
    if p.on_end then p.on_end("end", tick) end
  end
end

-- Normalize a `get_extmarks` position argument to an inclusive (row, col) bound.
-- v1 supports the common `0` (buffer start), `-1` (buffer end), and `{row, col}`
-- forms; a bare mark-id position is rejected rather than silently mishandled.
local function extmark_pos_bound(p)
  if p == 0 then return 0, 0 end
  if p == -1 then return math.huge, math.huge end
  if type(p) == "table" then return p[1] or 0, p[2] or 0 end
  error("nvim_buf_get_extmarks: only 0, -1, and {row, col} positions are supported", 2)
end

-- nvim_buf_get_extmarks(buffer, ns, start, end_, opts) -> list of {id, row, col}
-- (or {id, row, col, details} with opts.details), in (row, col, id) order. `ns ==
-- -1` returns marks from every namespace. Reads the mirror, so it reflects marks
-- set earlier in this chunk and positions current as of chunk start.
function vim.api.nvim_buf_get_extmarks(buffer, ns, start, end_, opts)
  local b = vim._resolve_bufnr(buffer)
  opts = opts or {}
  local sr, sc = extmark_pos_bound(start)
  local er, ec = extmark_pos_bound(end_)
  local out = {}
  local function in_range(row, col)
    if row < sr or (row == sr and col < sc) then return false end
    if row > er or (row == er and col > ec) then return false end
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
  local bufmarks = vim._extmarks[b] or {}
  if ns == -1 then
    for nsid, marks in pairs(bufmarks) do
      collect(nsid, marks)
    end
  else
    collect(ns, bufmarks[ns] or {})
  end
  table.sort(out, function(x, y)
    if x[2] ~= y[2] then return x[2] < y[2] end
    if x[3] ~= y[3] then return x[3] < y[3] end
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

-- ----- vim.fn editor-state builtins (statusline / lualine, Phase 5) ----------
-- The Vimscript builtins a real `'statusline'` (and lualine) call from inside a
-- `%{}`/`%!` expression. Each reads the Rust→Lua mirror the server refreshes
-- before evaluating the statusline (vim._cur_mode / vim._cur_cursor / vim._bufs /
-- vim._cur_buf / vim._wins), so a live redraw reflects the current frame. An
-- unsupported argument fails loud (the no-silent-stub rule) rather than guessing.

-- vim.fn.mode([expanded]): the single-letter mode code ("n"/"i"/"v"/"V"/"R"/"c").
-- INCOMPLETE: `expanded` is ignored — the core has a flat Mode (no operator-pending
-- / sub-state), so mode(1)'s multi-char forms ("no", "niI", …) don't exist here;
-- the short code is returned for both. Faithful for the modes nxvim has.
function vim.fn.mode(_expanded) return vim._cur_mode or "n" end

-- vim.fn.line(expr): a buffer line number. "." is the cursor line (1-based), "$"
-- the last line (the line count). The window-relative forms ("w0"/"w$") need the
-- scroll position, which the mirror doesn't carry yet, so they error loud.
function vim.fn.line(expr)
  if expr == "." then
    return (vim._cur_cursor or {}).row or 1
  elseif expr == "$" then
    local buf = vim._bufs[vim._resolve_bufnr(0)]
    return (buf and buf.lines) and #buf.lines or 1
  end
  error("line(): unsupported expression '" .. tostring(expr) .. "'", 2)
end

-- vim.fn.col(expr): a byte column (1-based). "." is the cursor column, "$" one
-- past the end of the cursor line (its byte length + 1), matching vim.
function vim.fn.col(expr)
  if expr == "." then
    return ((vim._cur_cursor or {}).col or 0) + 1
  elseif expr == "$" then
    local buf = vim._bufs[vim._resolve_bufnr(0)]
    local row = (vim._cur_cursor or {}).row or 1
    local ln = (buf and buf.lines) and buf.lines[row] or ""
    return #ln + 1
  end
  error("col(): unsupported expression '" .. tostring(expr) .. "'", 2)
end

-- vim.fn.winnr([arg]): the current window's 1-based number (its index in the
-- layout order), or with "$" the number of windows. (vim's "#" previous-window
-- form needs window history the mirror doesn't keep, so it errors loud.)
function vim.fn.winnr(arg)
  if arg == nil or arg == "." then
    local cur = vim._cur_win or 1000
    for i, id in ipairs(vim._win_order or {}) do
      if id == cur then return i end
    end
    return 0
  elseif arg == "$" then
    return #(vim._win_order or {})
  end
  error("winnr(): unsupported argument '" .. tostring(arg) .. "'", 2)
end

-- vim.fn.tabpagenr([arg]): the current tab page's 1-based number, or with "$" the
-- number of tab pages — the tab analogue of winnr(). Backs the loop in a custom
-- `'tabline'` (`for i = 1, tabpagenr('$')`). Resolves from the `vim._tabs` /
-- vim._tab_order mirror the server pushes before evaluating the tabline.
function vim.fn.tabpagenr(arg)
  if arg == nil or arg == "." then
    return vim.api.nvim_tabpage_get_number(0)
  elseif arg == "$" then
    return #(vim._tab_order or { vim._cur_tab or 1 })
  end
  error("tabpagenr(): unsupported argument '" .. tostring(arg) .. "'", 2)
end

-- vim.fn.localtime(): the current time in seconds. nxvim sources this from a
-- MONOTONIC clock (the server's `vim._mono_secs`, the same base stamped onto undo
-- nodes), not wall-clock unix epoch, so `localtime() - node.time` elapsed math
-- (e.g. the undotree visualizer's "N minutes ago") stays correct and non-negative
-- across NTP steps and manual clock changes. Only differences are meaningful.
function vim.fn.localtime() return vim._mono_secs or 0 end

-- vim.fn.undotree([bufnr]): the buffer's undo tree, in neovim's shape
-- ({ seq_last, seq_cur, save_last, save_cur, time_cur, synced, entries }, each
-- entry { seq, time, save?, alt? }). Reads the `vim._undotree` mirror the server
-- projects from the core's branching history before each Lua entry; `bufnr`
-- 0/nil is the current buffer. A buffer with no recorded history yet yields an
-- empty-`entries` tree rather than erroring.
function vim.fn.undotree(bufnr)
  bufnr = vim._resolve_bufnr(bufnr)
  local t = (vim._undotree or {})[bufnr]
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

-- vim.fn.tabpagebuflist(nr): the list of buffer numbers shown in tab page `nr`
-- (1-based; nil/0 is the current tab), one per window in that tab — what a custom
-- `'tabline'` label reads to find the tab's active file. Reads the tab mirror's
-- per-window `buffers` (parallel to `windows`), which the server fills for EVERY
-- tab — unlike the global window mirror, which only carries the current tab, so
-- `nvim_win_get_buf` would resolve an inactive tab's window to the current buffer.
function vim.fn.tabpagebuflist(nr)
  local tab_id
  if nr == nil or nr == 0 then
    tab_id = vim._cur_tab or 1
  else
    tab_id = (vim._tab_order or {})[nr]
  end
  local t = (vim._tabs or {})[tab_id]
  local bufs = {}
  for _, buf in ipairs(t and t.buffers or {}) do
    bufs[#bufs + 1] = buf
  end
  return bufs
end

-- (vim.fn.bufnr / bufname live in prelude/fs.lua, which loads after this chunk —
-- the canonical "additional vim.fn" home — so they aren't (re)defined here.)

-- vim.fn.winwidth(nr) / winheight(nr): a window's text dimensions. 0 is the
-- current window; a positive `nr` is a window *number* (1-based layout index),
-- resolved through the layout order to the mirror entry.
local function win_by_number(nr)
  if nr == nil or nr == 0 then return vim._cur_win or 1000 end
  return (vim._win_order or {})[nr]
end
function vim.fn.winwidth(nr)
  local w = (vim._wins or {})[win_by_number(nr)]
  return w and w.width or 0
end
function vim.fn.winheight(nr)
  local w = (vim._wins or {})[win_by_number(nr)]
  return w and w.height or 0
end

-- (vim.fn.fnamemodify lives in prelude/fs.lua, alongside the other path vim.fn;
-- this chunk's expand routes through it at call time.)

-- vim.fn.expand: the `%` (current file) forms autocmd callbacks and statuslines
-- use to resolve paths, backed by the current-buffer snapshot. `%` is the stored
-- name; `%:<mods>` routes through fnamemodify (so `%:t`, `%:p`, `%:h`, `%:r`,
-- `%:~:.`, … all work). A non-`%` expression errors loud.
function vim.fn.expand(expr)
  local name = (vim._cur_buf or {}).name or ""
  if expr == "%" then return name end
  local mods = expr:match("^%%(:.*)$")
  if mods then return vim.fn.fnamemodify(name, mods) end
  error("expand(): unsupported expression '" .. tostring(expr) .. "'", 2)
end

-- nvim_replace_termcodes(str, from_part, do_lt, special): in neovim, translate
-- key notation (`<CR>`, `<C-w>`, `<lt>`, …) into the internal terminal-byte
-- encoding. nxvim represents keys as that *notation* throughout — parse_keys and
-- nvim_feedkeys consume notation directly — so the canonical internal form of a
-- key string already IS the notation, and this returns `str` unchanged. The
-- result round-trips exactly through nvim_feedkeys (which re-parses the notation),
-- which is the contract callers rely on (build a "feed string", later feed it).
-- The flags (from_part / do_lt / special) only shape neovim's byte output and are
-- accepted for call-compatibility; `<lt>` and the special names are handled by
-- parse_keys at feed time, so no pre-translation is needed here.
function vim.api.nvim_replace_termcodes(str, _from_part, _do_lt, _special)
  return tostring(str or "")
end

-- nvim_feedkeys(keys, mode, escape_ks): enqueue `keys` (vim notation) into the
-- editor's typeahead, to run at the end of the current input batch / off-tick
-- settle. `mode` flags: 'n' = noremap (feed straight, the fed keys are not
-- themselves remapped); 'm' (or the empty/default mode) = remap (fed keys are run
-- through the mapping engine, so they can trigger mappings); 'i' = insert at the
-- FRONT of the typeahead (ahead of keys already queued). The 't' (as-if-typed)
-- and 'x' (execute now) flags are accepted: nxvim always processes the typeahead
-- within the same turn, so 'x' needs no special handling. `escape_ks` is accepted
-- and ignored (nxvim notation carries no K_SPECIAL byte escaping).
function vim.api.nvim_feedkeys(keys, mode, _escape_ks)
  -- Fed keys run against the real current window at drain time, so they can't be
  -- retargeted inside a context-swapped nvim_win_call/nvim_buf_call — fail loud.
  vim._assert_call_ctx("nvim_feedkeys")
  mode = tostring(mode or "")
  local remap = mode:find("n", 1, true) == nil -- noremap only with an explicit 'n'
  local insert = mode:find("i", 1, true) ~= nil
  vim._feedkeys(tostring(keys or ""), remap, insert)
end

-- nvim_create_buf(listed, scratch) -> bufnr: create a new, empty buffer without a
-- window and return its handle. `listed`/`scratch` are accepted (a scratch buffer
-- is unlisted with no file) but nxvim's core models neither buflisted nor buftype
-- yet, so they don't change behavior — the buffer is a plain empty buffer either
-- way. The id is predicted from `vim._next_buf` (the server's next buffer id,
-- refreshed in the mirror) so it returns synchronously, exactly as nvim_open_win
-- predicts a window id; the real buffer is created when the queued op drains. The
-- new buffer is mirrored into `vim._bufs` immediately so nvim_buf_set_lines and
-- the other buffer-read API work on it within this same chunk.
-- INCOMPLETE: `listed`/`scratch` are ignored (no buflisted/buftype in the core),
-- so a scratch buffer is still listed by `:ls`. Faithful once the core models them.
function vim.api.nvim_create_buf(_listed, _scratch)
  local id = vim._next_buf or 2
  vim._next_buf = id + 1
  vim._bufs = vim._bufs or {}
  vim._bufs[id] = { lines = { "" }, name = "", loaded = true }
  vim._create_buf()
  return id
end

-- nvim_buf_delete(buffer, opts): remove `buffer` from the editor (the popup
-- teardown which-key runs when it closes its scratch buffer). `opts.force` drops
-- a modified buffer without the E89 guard; `opts.unload` is accepted but maps to
-- the same removal (nxvim's core has no "unloaded but listed" buffer state yet).
-- Drops the buffer from the `vim._bufs` mirror (write-through) so a read later in
-- this chunk agrees, then queues the real removal.
-- INCOMPLETE: `opts.unload` can't keep the buffer listed-but-unloaded (no such
-- core state), so it behaves like a full delete.
function vim.api.nvim_buf_delete(buffer, opts)
  local id = vim._resolve_bufnr(buffer)
  opts = opts or {}
  if vim._bufs then vim._bufs[id] = nil end
  vim._buf_delete(id, opts.force and true or false)
end

-- vim.api.nvim_set_hl is installed from Rust (it captures the group definition
-- for the server to fold into the core highlight registry), so it is not
-- (re)defined here — doing so would shadow the Rust-backed version.

-- nvim_get_hl(ns, opts): read highlight group definitions from the `vim._hl_defs`
-- mirror the server refreshes when the registry changes. `ns` is accepted but
-- ignored (namespace 0 only, as nvim_set_hl). Forms:
--   * opts.name given          -> that group's definition. A link group returns
--                                 `{ link = "Target" }`; a concrete group returns
--                                 its colors (fg/bg/sp as 0xRRGGBB ints) and the
--                                 set boolean attrs. Unknown group -> `{}`.
--   * opts.name + link = false -> follow the link chain and return the resolved
--                                 concrete definition (what which-key reads to
--                                 blend popup colors).
--   * no name                  -> every group keyed by name.
-- A fresh table is returned each call so a caller mutating it can't corrupt the
-- mirror. INCOMPLETE: only namespace 0 is modelled (per-namespace highlights via
-- nvim_set_hl(ns, …) fold into the global table), and the extra metadata neovim
-- attaches (`default`, `cterm*`) is absent — nxvim's registry is truecolor-only.
local function copy_hl_def(d)
  local out = {}
  for k, v in pairs(d) do
    out[k] = v
  end
  return out
end

function vim.api.nvim_get_hl(_ns, opts)
  opts = opts or {}
  local defs = vim._hl_defs or {}
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

-- nvim_get_hl_by_name(name, rgb): the pre-0.9 highlight reader (lualine and other
-- older plugins still call it). Returns the *resolved* group (link chain followed)
-- in the legacy shape — `foreground`/`background`/`special` truecolor ints plus
-- the set boolean attrs — rather than nvim_get_hl's `fg`/`bg`/`sp`. nxvim's
-- registry is truecolor-only, so only `rgb == true` (RGB output) can be honored; a
-- cterm read (`rgb` false/nil) has no backing model and fails loud rather than
-- returning RGB ints mislabeled as cterm indices. An unknown group returns `{}`.
function vim.api.nvim_get_hl_by_name(name, rgb)
  if rgb ~= true then
    error(
      "nvim_get_hl_by_name: nxvim is truecolor-only; cterm output (rgb=false) is not modelled",
      0
    )
  end
  local d = vim.api.nvim_get_hl(0, { name = name, link = false })
  local out = {}
  if d.fg ~= nil then out.foreground = d.fg end
  if d.bg ~= nil then out.background = d.bg end
  if d.sp ~= nil then out.special = d.sp end
  for _, attr in ipairs({ "bold", "italic", "underline", "undercurl", "strikethrough", "reverse" }) do
    if d[attr] then out[attr] = true end
  end
  return out
end

-- nvim__redraw(opts): in neovim, force a UI repaint mid-execution (flushing the
-- screen before the current chunk returns). nxvim's server repaints at the end
-- of every input / RPC / event turn the Lua ran under (see `Server::handle` /
-- `settle_events`), so the popup a chunk just built (its float window + buffer
-- lines + extmarks, all queued and drained right after the chunk) paints on that
-- same turn without an explicit flush — this is correct by construction, not a
-- silent stub. The Lua VM runs inside the server's single thread, so it cannot
-- itself drive a synchronous mid-chunk repaint; `opts` (valid/flush/cursor/…) is
-- accepted for call-compatibility and needs no action here.
function vim.api.nvim__redraw(_opts) end

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
    vim._assert_call_ctx("an ex-command (vim.cmd)")
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
    for i = 1, select("#", ...) do
      parts[i] = tostring((select(i, ...)))
    end
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
