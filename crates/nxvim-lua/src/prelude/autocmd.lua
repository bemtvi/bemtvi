-- nxvim Lua prelude — autocmds, augroups, user commands, ex-command drivers.
-- The autocmd / augroup / user-command registries kept purely in Lua (authored as
-- nx.autocmd.* / nx.augroup.* / nx.user_command.*), the nx._fire dispatcher the
-- server reads back, the :autocmd / :augroup / :doautocmd / :command ex front-ends,
-- nx.exec, and the callable-and-indexable vim.cmd. The matching vim.api.nvim_*
-- names are aliased onto each native.
local vim = vim
local api = vim.api
nx = nx or {}
nx.autocmd = nx.autocmd or {}
nx.augroup = nx.augroup or {}
nx.user_command = nx.user_command or {}

-- ----- API surface stored purely in Lua --------------------------------------
-- Registration that needn't touch the editor lives in Lua tables; the server
-- reads them when it must (e.g. dispatching a user command typed as `:Foo`).

nx._user_commands = nx._user_commands or {}
-- nx._user_command_desc[name] = desc: the optional one-line `desc` passed to
-- create(), kept parallel to the body registry (so the dispatch path stays a plain
-- name -> body lookup). Surfaced by get() and the command-line completion catalog.
nx._user_command_desc = nx._user_command_desc or {}
-- nx._user_command_complete[name] = spec: the optional `complete` passed to
-- create() — `"dir"` / `"file"` (the only argument completers wired so far; an
-- unknown spec is stored but ignored). Kept parallel to the body registry like the
-- desc table; the command-line completer reads it to offer path completion for a
-- user command's argument (e.g. the GUI's `:workspace <dir>`).
nx._user_command_complete = nx._user_command_complete or {}
-- nx._user_command_usage[name] = usage: the optional `usage` passed to create() — the
-- argument signature shown after the command name in the command-line completion docs
-- pane (e.g. `[config]`, `{file}`), exactly as a built-in command's synopsis carries it.
-- Kept parallel to the body registry like the desc / complete tables.
nx._user_command_usage = nx._user_command_usage or {}
-- nx._buf_user_commands[bufnr][name] = command: the buffer-local command
-- registry (the analogue of the buffer-scoped `nx._keymaps` entries). A
-- buffer-local command shadows a global one of the same name and is invisible
-- from any other buffer — see nx._resolve_user_command.
nx._buf_user_commands = nx._buf_user_commands or {}
-- nx._buf_user_command_desc[bufnr][name] = desc: the buffer-local twin of
-- nx._user_command_desc.
nx._buf_user_command_desc = nx._buf_user_command_desc or {}
-- nx._buf_user_command_complete[bufnr][name] = spec: the buffer-local twin of
-- nx._user_command_complete.
nx._buf_user_command_complete = nx._buf_user_command_complete or {}
-- nx._buf_user_command_usage[bufnr][name] = usage: the buffer-local twin of
-- nx._user_command_usage.
nx._buf_user_command_usage = nx._buf_user_command_usage or {}
nx._autocmds = nx._autocmds or {}
nx._augroups = nx._augroups or {}
local augroup_seq, autocmd_seq = 0, 0

-- A monotonic version bumped on every change to nx._autocmds (register / delete /
-- clear). The server reads it once per input batch (LuaRuntime::autocmd_version)
-- and, only when it advanced, refreshes its cached set of registered event names —
-- the gate that lets high-frequency events (CursorMoved / TextChanged) cost nothing
-- when no handler wants them (mirroring nx._keymaps_version). Bumped through
-- nx._au_touch() at every mutation site below.
nx._au_version = nx._au_version or 0
function nx._au_touch()
  nx._au_version = nx._au_version + 1
end

-- The distinct event names any registered autocmd listens for (an autocmd may name
-- a single event or a list). The server caches this — refreshed only when
-- nx._au_version advances — so its per-key lifecycle diff can skip computing /
-- firing an event nothing is registered for.
function nx._au_event_set()
  local seen = {}
  local out = {}
  for _, au in ipairs(nx._autocmds) do
    local evs = type(au.event) == "table" and au.event or { au.event }
    for _, ev in ipairs(evs) do
      if not seen[ev] then
        seen[ev] = true
        out[#out + 1] = ev
      end
    end
  end
  return out
end

-- `nx.user_command.create(name, command, opts)` [alias `nvim_create_user_command`]:
-- register a global `:Name`. `command` is a function or an ex-command string.
-- `opts.desc` (a one-line summary) is stored alongside the body — `get()` surfaces it
-- and the command-line completion catalog shows it as the command's docs.
-- `opts.usage` (a string) is the command's ARGUMENT signature — the part after the
-- name, in vim help notation (`{arg}` required, `[arg]` optional), e.g.
-- `usage = "[config]"`. The completion docs pane heads with `:Name <usage>` exactly as
-- a built-in's synopsis does, so a plugin command's parameters are discoverable in the
-- same place. Omit it for a command that takes no arguments.
-- `opts.complete` makes `<Tab>` in the command's argument offer completion:
--   * `"dir"` / `"file"` — path completion via the picker the built-in `:cd`/`:edit` use;
--   * a function `fn(args)` — generate candidates dynamically. `args` is the list of
--     whitespace-separated argument words typed so far, the last being the partial word
--     under the cursor (`:Cmd <Tab>` → `{}`, `:Cmd a<Tab>` → `{"a"}`, `:Cmd a b<Tab>` →
--     `{"a","b"}`). It returns a list of candidates — each a string, or a table
--     `{ label =, insert =, doc = }`. A SYNC function (returns a list) shows inline in
--     the wildmenu and is re-run as you type; an ASYNC one (returns a promise, e.g. an
--     `nx.async` function) lists in the picker. A throw / rejection yields no candidates.
function nx.user_command.create(name, command, opts)
  nx._user_commands[name] = command
  nx._user_command_desc[name] = type(opts) == "table" and opts.desc or nil
  nx._user_command_complete[name] = type(opts) == "table" and opts.complete or nil
  nx._user_command_usage[name] = type(opts) == "table" and opts.usage or nil
end

-- `nx.user_command.buf_create(buffer, name, command, opts)` [alias
-- `nvim_buf_create_user_command`]: register a *buffer-local* command (`buffer` 0 =
-- current). It dispatches only while that buffer is current and shadows a global
-- command of the same name there — everywhere else it's unknown. Lives in its own
-- per-bufnr table so the global registry stays clean; `nx._resolve_user_command`
-- consults both at dispatch.
function nx.user_command.buf_create(buffer, name, command, opts)
  if buffer == nil or buffer == 0 then
    buffer = nx._cur_buf and nx._cur_buf.bufnr or 0
  end
  local cmds = nx._buf_user_commands[buffer]
  if not cmds then
    cmds = {}
    nx._buf_user_commands[buffer] = cmds
  end
  cmds[name] = command
  local descs = nx._buf_user_command_desc[buffer]
  if not descs then
    descs = {}
    nx._buf_user_command_desc[buffer] = descs
  end
  descs[name] = type(opts) == "table" and opts.desc or nil
  local completes = nx._buf_user_command_complete[buffer]
  if not completes then
    completes = {}
    nx._buf_user_command_complete[buffer] = completes
  end
  completes[name] = type(opts) == "table" and opts.complete or nil
  local usages = nx._buf_user_command_usage[buffer]
  if not usages then
    usages = {}
    nx._buf_user_command_usage[buffer] = usages
  end
  usages[name] = type(opts) == "table" and opts.usage or nil
end

-- Resolve a typed `:Name` to its command definition for buffer `bufnr` (0 =
-- current): a buffer-local command for that buffer wins over a global of the
-- same name (matching neovim), and a buffer-local command is invisible from any
-- other buffer. The server passes the editor's authoritative current bufnr, so
-- this never relies on a possibly-stale `nx._cur_buf`. Returns the function /
-- string body, or nil when no command matches.
function nx._resolve_user_command(name, bufnr)
  if bufnr == nil or bufnr == 0 then
    bufnr = nx._cur_buf and nx._cur_buf.bufnr or 0
  end
  local locals = nx._buf_user_commands[bufnr]
  if locals and locals[name] ~= nil then
    return locals[name]
  end
  return nx._user_commands[name]
end

-- Drop everything scoped to buffer `bufnr` when the server reports it deleted, so
-- a later buffer reusing the bufnr can't inherit a stale buffer-local command or
-- mapping (matching neovim's bufwipe cleanup). The keymap purge lives in
-- keymap.lua, where the trie source / fn table are owned.
function nx._cleanup_buffer(bufnr)
  nx._buf_user_commands[bufnr] = nil
  nx._buf_user_command_desc[bufnr] = nil
  nx._buf_user_command_complete[bufnr] = nil
  nx._buf_user_command_usage[bufnr] = nil
  nx._purge_buf_keymaps(bufnr)
end

-- `nx.augroup.create(name, opts)` -> id [alias `nvim_create_augroup`]: define (or look
-- up) an autocommand group and return its numeric id. An augroup is just a named
-- bucket for autocmds: pass the returned id as `opts.group` to `nx.autocmd.create` so
-- the whole set can be cleared and re-registered as a unit.
--
-- Arguments:
--   * `name` — the group name (string). Calling create again with the same name
--     returns the SAME id; the id is stable across recreation, so it's safe to store.
--   * `opts.clear` — when the group already exists, whether to remove its existing
--     autocmds first. Defaults to TRUE (matching neovim). This is what makes
--     re-sourcing your config idempotent: a config that does
--     `nx.augroup.create("MyGroup")` on every load clears the previous run's autocmds
--     instead of double-registering them. Pass `{ clear = false }` to keep them
--     (the augroup-block / `:augroup` ex-command path uses this to append).
--
-- The idiomatic pattern — own a group, then hang autocmds off it:
--
-- ```lua
-- local grp = nx.augroup.create("MyConfig")                 -- clears on re-source
-- nx.autocmd.create("BufEnter", {
--   group = grp,                                            -- numeric id, or "MyConfig"
--   callback = function(ev) nx.notify("entered " .. (ev.file or "[No Name]")) end,
-- })
-- ```
function nx.augroup.create(name, opts)
  opts = opts or {}
  local clear = opts.clear ~= false -- absent → clear, matching neovim's default
  local id = nx._augroups[name]
  if id and clear then
    nx._autocmds = vim.tbl_filter(function(au)
      return au.group ~= id
    end, nx._autocmds)
    nx._au_touch()
  end
  if not id then
    augroup_seq = augroup_seq + 1
    id = augroup_seq
    nx._augroups[name] = id
  end
  return id
end

-- `nx.autocmd.create(event, opts)` -> id [alias `nvim_create_autocmd`]: run something
-- whenever `event` fires. Returns the autocmd's numeric id (pass it to
-- `nx.autocmd.del` to remove it). `event` is an event name (`"FileType"`,
-- `"BufEnter"`, …) or a list of names to share one handler — see the
-- [autocommand events](../plugins/autocmd-events.md) reference for the events
-- nxvim emits and what each carries.
--
-- `opts` fields:
--   * `callback` — a function run when the event fires; OR `command` — an
--     ex-command string queued instead. Provide one of the two.
--   * `pattern` — a glob (or list of globs) the event's match string is tested
--     against (e.g. `"*.lua"`, `{ "*.c", "*.h" }`). Omitted / `"*"` matches all.
--   * `group` — an augroup, by numeric id or by name (see `nx.augroup.create`). Ties
--     this autocmd to the group so a later `clear` of that group drops it.
--   * `buffer` — make it buffer-local: it then fires only for that buffer (and
--     `pattern` is ignored). `0` resolves to the current buffer at registration time.
--   * `once` — fire once, then auto-remove. `desc` — a human description.
--
-- The `callback` receives one table describing the event:
--   `{ id, event, match, buf, file, data }` — `id` this autocmd's id, `event` the
--   event name, `match` the matched pattern string, `buf` the buffer number, `file`
--   its name, and `data` an event-specific payload (e.g. `LspAttach` carries
--   `{ client_id = … }`), nil for most events.
--
-- ```lua
-- nx.autocmd.create("FileType", {
--   pattern = "markdown",
--   callback = function(ev)
--     nx.bo[ev.buf].textwidth = 80
--   end,
-- })
-- ```
function nx.autocmd.create(event, opts)
  opts = opts or {}
  autocmd_seq = autocmd_seq + 1
  local group = opts.group
  if type(group) == "string" then
    group = nx._augroups[group]
  end
  local buffer = opts.buffer
  if buffer == 0 then
    buffer = nx._cur_buf and nx._cur_buf.bufnr or 0
  end
  nx._autocmds[#nx._autocmds + 1] =
    { id = autocmd_seq, event = event, opts = opts, group = group, buffer = buffer }
  nx._au_touch()
  return autocmd_seq
end

-- `nx.autocmd.del(id)` [alias `nvim_del_autocmd`]: remove the autocmd with this id,
-- so it stops firing.
function nx.autocmd.del(id)
  nx._autocmds = vim.tbl_filter(function(au)
    return au.id ~= id
  end, nx._autocmds)
  nx._au_touch()
end

-- Fire the registered autocmds for `event` whose pattern matches `pattern`,
-- with optional buffer context. Called from Rust (`LuaRuntime::fire_autocmd*`)
-- when the editor triggers an event, and from `nvim_exec_autocmds`. A function
-- handler runs with the callback args table `{id, event, match, buf, file}`; a
-- string `command` is queued as an ex-command. Match rules: event equals (or is
-- in) the registered event; pattern is nil/"*", equals `pattern`, or is in the
-- registered pattern list; a buffer-local autocmd only fires for its `buffer`.
-- `buf`/`file` are nil for back-compat callers (e.g. ColorScheme), in which
-- case `file` falls back to `pattern` (the old behavior). `data` is the optional
-- `args.data` payload (`LspAttach`/`LspDetach` carry `{ client_id = … }`); nil otherwise.
-- An autocmd registered with `opts.once` (`:autocmd … ++once`) fires once and is
-- then dropped — collected during the pass and removed after it, so the live
-- iteration isn't mutated underneath `ipairs`.
-- Returns whether any autocmd actually ran — the `apply_autocmds()` boolean
-- neovim's `buf_check_timestamp` branches on (an autocmd ran → honor v:fcs_choice;
-- none → default warning). Callers that ignore the return value are unaffected.
-- Does a single autocmd pattern `pat` match the event's `pattern` (the file path
-- for file events, a filetype / id / mode-code for others)? Beyond an exact match
-- and `*`, a `pat` holding a shell glob metacharacter (`*` `?` `[`) is matched as
-- vim's file-pattern: a glob with no `/` matches the path *tail* (`*.lua` matches
-- any `.lua` file), one with a `/` the whole path. A metacharacter-free `pat` is
-- only ever an exact compare (so a `FileType` `rust` autocmd can't glob-match a path).
local function au_one_pattern_matches(pat, pattern)
  if pat == "*" or pat == pattern then
    return true
  end
  if pattern == nil or type(pat) ~= "string" then
    return false
  end
  if not pat:find("[%*%?%[]") then
    return false -- no glob: exact compare above is the only match
  end
  -- A separator-less glob matches the path tail (basename), like vim.
  local target = pattern
  if not pat:find("/", 1, true) then
    target = pattern:match("[^/]*$") or pattern
  end
  -- Build an anchored Lua pattern: escape Lua magic (but not the glob `* ? [`),
  -- then turn the shell wildcards into their Lua-pattern equivalents. A bracket
  -- class rides through as-is, but its negation spellings need repair: shell-style
  -- `[!abc]` becomes Lua's `[^abc]`, and vim-style `[^abc]` gets its `^` un-escaped
  -- (the blanket escape above can't tell it opened a class).
  local lp = pat:gsub("[%(%)%.%%%+%-%^%$]", "%%%1"):gsub("%*", ".*"):gsub("%?", ".")
  lp = lp:gsub("%[!", "[^"):gsub("%[%%%^", "[^")
  -- A malformed class (`foo[bar`) is not a valid Lua pattern; treat it as matching
  -- nothing rather than raising out of every subsequent event fire (a buffer
  -- literally named `foo[bar` was already caught by the exact compare above).
  local ok, matched = pcall(string.match, target, "^" .. lp .. "$")
  return ok and matched ~= nil
end

-- Whether the autocmd's `pat` (a string, a list, or nil = match-all) matches the
-- fired `pattern`. Used by `nx._fire` below.
local function au_pattern_matches(pat, pattern)
  if pat == nil then
    return true
  end
  if type(pat) == "table" then
    for _, p in ipairs(pat) do
      if au_one_pattern_matches(p, pattern) then
        return true
      end
    end
    return false
  end
  return au_one_pattern_matches(pat, pattern)
end

function nx._fire(event, pattern, buf, file, data)
  local any = false
  local fired -- ids of `++once` autocmds to drop after this pass (nil = none)
  for _, au in ipairs(nx._autocmds) do
    local ev = au.event
    local ev_ok = ev == event or (type(ev) == "table" and vim.tbl_contains(ev, event))
    if ev_ok then
      local pat = au.opts.pattern
      local pat_ok = au_pattern_matches(pat, pattern)
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
          any = true
        elseif type(au.opts.command) == "string" then
          vim.cmd(au.opts.command)
          any = true
        end
        if au.opts.once then
          fired = fired or {}
          fired[au.id] = true
        end
      end
    end
  end
  if fired then
    nx._autocmds = vim.tbl_filter(function(au)
      return not fired[au.id]
    end, nx._autocmds)
  end
  return any
end

-- Fire a `*Cmd` autocmd (currently `BufReadCmd`) and return whether a handler
-- **claimed** the action. The server uses `BufReadCmd` to let a plugin own a buffer's
-- read (vim's "replace the read" hook — the file-explorer-as-plugin rides it): a
-- claimed read skips the server's default load. Unlike `nx._fire` (which reports
-- merely whether a handler *ran*), a `*Cmd` handler claims by **returning a truthy
-- value** — so a `pattern = "*"` handler can decide per path (claim a directory,
-- return nil for a regular file so the default read proceeds). `path` is the match /
-- `<afile>`; `buf` is the (empty) buffer the handler fills; `isdir` is whether `path`
-- is a directory (surfaced as `args.isdir`), the fs fact a `*Cmd` handler branches on
-- without an async re-stat — the file explorer claims directories, declines files.
function nx._fire_read_cmd(event, path, buf, isdir)
  local claimed = false
  local fired -- ids of `++once` autocmds to drop after this pass (nil = none)
  for _, au in ipairs(nx._autocmds) do
    local ev = au.event
    local ev_ok = ev == event or (type(ev) == "table" and vim.tbl_contains(ev, event))
    if
      ev_ok
      and au_pattern_matches(au.opts.pattern, path)
      and (au.buffer == nil or au.buffer == buf)
    then
      local cb = au.opts.callback
      local ret
      if type(cb) == "function" then
        ret = cb({ id = au.id, event = event, match = path, buf = buf, file = path, isdir = isdir })
      elseif type(au.opts.command) == "string" then
        -- A command-form `*Cmd` handler can't return a value; running it is the claim.
        vim.cmd(au.opts.command)
        ret = true
      end
      if ret then
        claimed = true
      end
      if au.opts.once then
        fired = fired or {}
        fired[au.id] = true
      end
    end
  end
  if fired then
    nx._autocmds = vim.tbl_filter(function(au)
      return not fired[au.id]
    end, nx._autocmds)
  end
  return claimed
end

-- The `FileChangedShell` round-trip the server's file-change reconcile drives
-- (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → the watch leg). Set
-- `v:fcs_reason` to `reason` and reset `v:fcs_choice` to `""` (neovim's defaults
-- before the autocmd), fire `FileChangedShell` for `buf`/`file`, and return whether
-- any handler ran. A handler reads `vim.v.fcs_reason` and may set `vim.v.fcs_choice`
-- to `"reload"`/`"edit"`/`"ask"` to redirect the reconcile; the server reads it back via
-- `nx._fcs_choice`.
function nx._fire_file_changed(reason, buf, file)
  nx._v_mirror.fcs_reason = reason
  nx._v_mirror.fcs_choice = ""
  return nx._fire("FileChangedShell", file, buf, file)
end

-- Read the `v:fcs_choice` a `FileChangedShell` handler set (or `""` if none did) —
-- the second half of the round-trip above.
function nx._fcs_choice()
  return nx._v_mirror.fcs_choice or ""
end

-- Fire `DirChanged` after a `:cd` / `:chdir` changed the working directory (the
-- server calls this through `LuaRuntime::fire_dir_changed`). Set `v:event` to
-- neovim's `{ cwd, scope, changed_window }` payload before firing — a handler
-- reading `vim.v.event.cwd` (project / session plugins) sees it — and pass the
-- same table as `args.data`. The autocmd pattern matches `scope` (`"global"` for
-- `:cd`); `<afile>` (`args.file`) is the new directory.
function nx._fire_dir_changed(scope, cwd)
  local event = { cwd = cwd, scope = scope, changed_window = false }
  nx._v_mirror.event = event
  nx._fire("DirChanged", scope, nil, cwd, event)
end

-- `nx.autocmd.exec(event, opts)` [alias `nvim_exec_autocmds`]: fire `event` (or a
-- list of events) manually. `opts.pattern` (string or list) is matched as in
-- registration; `opts.buffer` supplies the buffer context (defaulting to the
-- current snapshot buffer), and the callback's `args.file` is the snapshot name
-- when firing for it.
function nx.autocmd.exec(event, opts)
  opts = opts or {}
  local events = type(event) == "table" and event or { event }
  local buf = opts.buffer
  if buf == nil then
    buf = nx._cur_buf and nx._cur_buf.bufnr or nil
  end
  local file
  if nx._cur_buf and buf == nx._cur_buf.bufnr then
    file = nx._cur_buf.name
  end
  local patterns = opts.pattern
  for _, ev in ipairs(events) do
    if type(patterns) == "table" then
      for _, p in ipairs(patterns) do
        nx._fire(ev, p, buf, file)
      end
    else
      nx._fire(ev, patterns, buf, file)
    end
  end
end

-- `nx.autocmd.get(opts)` [alias `nvim_get_autocmds`]: introspect the registered
-- autocmds — a debugging affordance for confirming what `clear`/`del` left
-- behind. Returns a list of
-- `{id, event, group, group_name, pattern, buffer, command}` entries, optionally
-- filtered by `opts.event` (string or list) and `opts.group` (id or name). Run it
-- interactively as
-- `:lua print(vim.inspect(nx.autocmd.get({})))`.
function nx.autocmd.get(opts)
  opts = opts or {}
  local want_events = opts.event and (type(opts.event) == "table" and opts.event or { opts.event })
  local want_group = opts.group
  if type(want_group) == "string" then
    want_group = nx._augroups[want_group]
  end
  -- reverse map: group id → its registered name, for human-readable output
  local group_name = {}
  for nm, id in pairs(nx._augroups) do
    group_name[id] = nm
  end
  local out = {}
  for _, au in ipairs(nx._autocmds) do
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
-- the argument line here and drives the same nx._autocmds / nx._augroups store
-- the nvim_* API uses — one store, two front-ends. Each `nx._ex_*` returns the
-- text the server surfaces: "" (nothing), a one-line message/error (echoed), or a
-- multi-line listing (shown in a panel).

-- The "current augroup" set by `:augroup {name}` and cleared by `:augroup END`.
-- It persists across command invocations, exactly like Vim's parser state, so a
-- block of `:autocmd`s between the two lands in that group.
nx._cur_augroup = nil

-- Does `au` match the group / event-list / pattern-list filter? A nil filter
-- field means "any" (so a bare `:autocmd!` clears everything in scope). Events
-- and patterns are lists; a `"*"` event matches any event. A pattern-less autocmd
-- is treated as `"*"` for matching, mirroring `nx._fire`'s pattern rule.
local function au_matches(au, group, events, patterns)
  if group ~= nil and au.group ~= group then
    return false
  end
  if events ~= nil and not vim.tbl_contains(events, "*") then
    local evs = type(au.event) == "table" and au.event or { au.event }
    local hit = false
    for _, w in ipairs(events) do
      if vim.tbl_contains(evs, w) then
        hit = true
        break
      end
    end
    if not hit then
      return false
    end
  end
  if patterns ~= nil then
    local pat = au.opts.pattern
    if pat == nil then
      pat = "*"
    end
    local pats = type(pat) == "table" and pat or { pat }
    local hit = false
    for _, w in ipairs(patterns) do
      if vim.tbl_contains(pats, w) then
        hit = true
        break
      end
    end
    if not hit then
      return false
    end
  end
  return true
end

-- Render the autocmds matching the filter as a `:autocmd`-style listing.
local function au_list(group, events, patterns)
  local gname = {}
  for nm, id in pairs(nx._augroups) do
    gname[id] = nm
  end
  local lines = { "--- Autocommands ---" }
  for _, au in ipairs(nx._autocmds) do
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
-- trimmed remainder (both `""` when `s` is empty).
local function take_word(s)
  local w = s:match("^(%S+)")
  if not w then
    return "", ""
  end
  return w, vim.trim(s:sub(#w + 1))
end

-- :aug[roup][!] {name} | END. Without a bang: `END`/`end` leaves the current
-- group, an empty arg reports it, and any other name enters that group (creating
-- it without clearing — `:augroup` is not destructive). With a bang,
-- `:augroup! {name}` deletes the group and every autocmd in it.
function nx._ex_augroup(bang, args)
  args = vim.trim(args)
  if bang then
    if args == "" then
      return "E471: Argument required"
    end
    local id = nx._augroups[args]
    if id then
      nx._autocmds = vim.tbl_filter(function(au)
        return au.group ~= id
      end, nx._autocmds)
      nx._au_touch()
      nx._augroups[args] = nil
      if nx._cur_augroup == args then
        nx._cur_augroup = nil
      end
    end
    return ""
  end
  if args == "" then
    return nx._cur_augroup and ("augroup " .. nx._cur_augroup) or ""
  end
  if args == "END" or args == "end" then
    nx._cur_augroup = nil
    return ""
  end
  nx.augroup.create(args, { clear = false })
  nx._cur_augroup = args
  return ""
end

-- :au[tocmd][!] [group] [event[,event…]] [pat[,pat…]] [++once] [++nested] [cmd]
-- A leading word that names an existing augroup is the group; otherwise the
-- current `:augroup` (if any) applies. With a bang, the autocmds matching the
-- group/event/pattern filter are removed first; with a trailing command, a new
-- autocmd is then registered. With no command and no bang it lists the matching
-- autocmds. `<buffer>` as the pattern registers a buffer-local autocmd for the
-- current buffer. `++once` fires once then self-removes (honored by `nx._fire`);
-- `++nested` is accepted (nxvim already lets events nest).
function nx._ex_autocmd(bang, args)
  local rest = vim.trim(args)

  -- Optional leading group: only when the first word names an existing augroup.
  local group = nx._cur_augroup and nx._augroups[nx._cur_augroup] or nil
  local first = rest:match("^(%S+)")
  if first and nx._augroups[first] then
    group = nx._augroups[first]
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
    nx._autocmds = vim.tbl_filter(function(au)
      return not au_matches(au, group, events, patterns)
    end, nx._autocmds)
    nx._au_touch()
  end

  if cmd ~= "" then
    if not events then
      return "E216: No such event: a {event} is required to define an autocmd"
    end
    nx.autocmd.create(#events == 1 and events[1] or events, {
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
  if bang then
    return ""
  end
  return au_list(group, events, patterns)
end

-- :doau[tocmd] {event} [pattern]: fire `event` now (optionally for a pattern),
-- the manual analogue of `nvim_exec_autocmds`. The optional [group] argument vim
-- accepts is not supported — `nx._fire` has no group filter — so the first word
-- is always the event; pass the event directly.
function nx._ex_doautocmd(args)
  args = vim.trim(args):gsub("^<nomodeline>%s*", "")
  local event, rest = take_word(args)
  if event == "" then
    return "E217: Can't execute autocommands for ALL events"
  end
  local pattern = rest ~= "" and rest or nil
  nx.autocmd.exec(event, { pattern = pattern })
  return ""
end

-- :com[mand][!] [attrs] {Name} {replacement} — define a user command. The
-- replacement is a verbatim ex-command template, run on invocation with the
-- common `<…>` escapes expanded against that call's args. It registers into the
-- same `nx._user_commands` (or, with `-buffer`, the current buffer's local) store
-- the `nvim_create_user_command` API uses, so a `:command`-defined command and an
-- API-defined one dispatch identically — which is how most vimscript plugins
-- define their commands. Returns `""` on success, an `E…` error, or a newline-
-- joined listing for a bare `:command`. `bang` is the replace-existing `!`.
--
-- INCOMPLETE vs neovim: attributes other than `-buffer` are parsed-and-ignored
-- (the command still registers and runs, just without arg-count / completion
-- enforcement); the range/count escapes (`<line1>`/`<line2>`/`<count>`) and an
-- invocation-time `<bang>` aren't plumbed through user-command dispatch yet, so
-- they expand to `""`.
function nx._ex_command(bang, args, bufnr)
  local s = vim.trim(args or "")
  if s == "" then
    -- Bare `:command`: list the defined command names (global + this buffer's
    -- locals), one per line. Minimal but real — not a silent no-op.
    local names = {}
    for name in pairs(nx._user_commands) do
      names[#names + 1] = name
    end
    local locals = (nx._buf_user_commands or {})[bufnr or 0]
    if locals then
      for name in pairs(locals) do
        names[#names + 1] = name
      end
    end
    if #names == 0 then
      return "No user commands are defined"
    end
    table.sort(names)
    return "Name\n" .. table.concat(names, "\n")
  end

  -- Consume leading `-attr[=val]` tokens; only -buffer changes behavior here.
  local buffer_local = false
  while true do
    local attr, rest = s:match("^(%-%S+)%s+(.*)$")
    if not attr then
      attr = s:match("^(%-%S+)%s*$")
      rest = ""
    end
    if not attr then
      break
    end
    if attr == "-buffer" then
      buffer_local = true
    end
    s = rest
  end

  -- The command name (vim requires it to start with an uppercase letter), then
  -- the verbatim replacement (everything past the first run of whitespace).
  local name, repl = s:match("^(%S+)%s+(.*)$")
  if not name then
    name = s:match("^(%S+)$")
    repl = ""
  end
  if not name or not name:match("^%u") then
    return "E182: Invalid command name"
  end

  -- Resolve the target store (global, or this buffer's local table for -buffer),
  -- then refuse to clobber an existing command unless `!` was given (E174).
  local store = nx._user_commands
  if buffer_local then
    if bufnr == nil or bufnr == 0 then
      bufnr = nx._cur_buf and nx._cur_buf.bufnr or 0
    end
    nx._buf_user_commands[bufnr] = nx._buf_user_commands[bufnr] or {}
    store = nx._buf_user_commands[bufnr]
  end
  if not bang and store[name] ~= nil then
    return "E174: Command already exists: add ! to replace it"
  end

  -- A function body (not a raw string) so the `<…>` escapes are expanded per
  -- invocation before the resulting ex-command is queued via vim.cmd.
  store[name] = function(o)
    local a = o.args or ""
    local function q(v)
      return "'" .. tostring(v):gsub("'", "''") .. "'"
    end
    local fargs = {}
    for _, w in ipairs(o.fargs or {}) do
      fargs[#fargs + 1] = q(w)
    end
    local out = repl
      :gsub("<q%-args>", function()
        return q(a)
      end)
      :gsub("<f%-args>", function()
        return table.concat(fargs, ", ")
      end)
      :gsub("<args>", function()
        return a
      end)
      :gsub("<bang>", function()
        return o.bang and "!" or ""
      end)
      :gsub("<line1>", function()
        return ""
      end)
      :gsub("<line2>", function()
        return ""
      end)
      :gsub("<count>", function()
        return ""
      end)
    vim.cmd(out)
  end
  return ""
end

-- The three command families whose report/listing text is produced synchronously
-- in *this* Lua layer (the nx._ex_* drivers above), keyed by every abbreviation
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

-- `nvim_exec(src, output)`: run the ex-command(s) in `src` (one or more newline-
-- separated lines) and, when `output` is truthy, return the text they produced as
-- a single string; otherwise return `""`. This is the legacy (pre-0.9) form lualine
-- calls — `nvim_exec('au lualine <event> <pat>', true):find(cmd)` — to read the
-- `:au` listing back and dedupe its autocmds.
--
-- nxvim can only *capture* output from the command families whose listing/report
-- text is generated synchronously in Lua (the autocmd group). Any other command is
-- still run, via the normal queued `vim.cmd` path, but its message-line output is
-- asynchronous and cannot be read back here. So requesting `output` capture of a
-- non-capturable command FAILS LOUD rather than returning a misleading `""` — a stub
-- that faked an empty capture would make a caller's `:find` on the result silently
-- wrong, exactly the "quietly succeeds" failure nxvim forbids.
local function exec_capture(src, output)
  local captured = {}
  for line in (tostring(src) .. "\n"):gmatch("([^\n]*)\n") do
    local cmd = vim.trim(line):gsub("^:+%s*", "") -- tolerate a leading ':'
    if cmd ~= "" and cmd:sub(1, 1) ~= '"' then -- skip blanks and " comment lines
      local head, rest = cmd:match("^(%S+)%s*(.*)$")
      local bang = head:sub(-1) == "!"
      if bang then
        head = head:sub(1, -2)
      end
      local kind = AUTOCMD_HEADS[head]
      local text
      if kind == "au" then
        text = nx._ex_autocmd(bang, rest)
      elseif kind == "aug" then
        text = nx._ex_augroup(bang, rest)
      elseif kind == "doau" then
        text = nx._ex_doautocmd(rest)
      elseif output then
        error("nvim_exec: output capture is unsupported for ':" .. head .. "'", 0)
      else
        vim.cmd(cmd) -- run it the normal (queued) way; nothing to capture
      end
      if text and text ~= "" then
        captured[#captured + 1] = text
      end
    end
  end
  return output and table.concat(captured, "\n") or ""
end

-- `nx.exec(src, output)` [alias `nvim_exec`]: run the ex-command(s) in `src` and,
-- when `output` is truthy, return the text they produced (see `exec_capture`).
function nx.exec(src, output)
  return exec_capture(src, output)
end

-- `nvim_exec2(src, opts)`: the 0.9+ neovim-shaped wrapper around `nvim_exec` — same
-- execution, but the captured text is returned under `.output` (only when
-- `opts.output` is set). A `vim.api`-only compat shim with no distinct nx twin
-- (the canonical nxvim form is `nx.exec`); its body only wraps the sibling nvim_
-- function, so it carries no implementation of its own.
function vim.api.nvim_exec2(src, opts)
  opts = opts or {}
  local out = vim.api.nvim_exec(src, opts.output)
  return opts.output and { output = out } or {}
end

-- ----- vim.cmd: callable AND indexable ---------------------------------------
-- `vim.cmd("…")` queues a raw ex-command (the Rust function installed earlier);
-- `vim.cmd.colorscheme("x")` / `vim.cmd.set("number")` build `"<name> <args…>"`.
do
  local raw_cmd = vim.cmd
  -- An <expr> mapping RHS must not change editor state (textlock): while
  -- nx._expr_lock is set, running an ex-command raises instead of mutating.
  local function raw(c)
    if nx._expr_lock then
      error("E5555: <expr> mapping must not change the editor (vim.cmd is blocked)", 0)
    end
    nx._assert_call_ctx("an ex-command (vim.cmd)")
    return raw_cmd(c)
  end
  local function build(name, ...)
    local first = ...
    if type(first) == "table" then
      local s = name
      if first.bang then
        s = s .. "!"
      end
      if first.args then
        s = s .. " " .. table.concat(first.args, " ")
      end
      return raw(s)
    end
    local parts = {}
    for i = 1, select("#", ...) do
      parts[i] = tostring((select(i, ...)))
    end
    local s = name
    if #parts > 0 then
      s = s .. " " .. table.concat(parts, " ")
    end
    return raw(s)
  end
  vim.cmd = setmetatable({}, {
    __call = function(_, c)
      return raw(c)
    end,
    __index = function(_, name)
      return function(...)
        return build(name, ...)
      end
    end,
  })
end

vim.api.nvim_create_user_command = nx.user_command.create
vim.api.nvim_buf_create_user_command = nx.user_command.buf_create
vim.api.nvim_create_augroup = nx.augroup.create
vim.api.nvim_create_autocmd = nx.autocmd.create
vim.api.nvim_del_autocmd = nx.autocmd.del
vim.api.nvim_exec_autocmds = nx.autocmd.exec
vim.api.nvim_get_autocmds = nx.autocmd.get
vim.api.nvim_exec = nx.exec

-- `nx.autocmd.clear(opts)` [alias `nvim_clear_autocmds`]: remove every autocmd
-- matching the filter — the bulk analogue of `nx.autocmd.del`. `opts.event`
-- (string/list), `opts.group` (id or name), `opts.buffer`, and `opts.pattern`
-- (string/list) all narrow the set; an empty opts clears everything. Mirrors
-- `nx.autocmd.get`'s matching.
function nx.autocmd.clear(opts)
  opts = opts or {}
  local want_events = opts.event and (type(opts.event) == "table" and opts.event or { opts.event })
  local want_group = opts.group
  if type(want_group) == "string" then
    want_group = nx._augroups[want_group]
  end
  local want_pats = opts.pattern
    and (type(opts.pattern) == "table" and opts.pattern or { opts.pattern })
  nx._autocmds = vim.tbl_filter(function(au)
    if want_events then
      local evs = type(au.event) == "table" and au.event or { au.event }
      local hit = false
      for _, w in ipairs(want_events) do
        if vim.tbl_contains(evs, w) then
          hit = true
          break
        end
      end
      if not hit then
        return true
      end -- keep: event doesn't match the filter
    end
    if want_group ~= nil and au.group ~= want_group then
      return true
    end
    if opts.buffer ~= nil and au.buffer ~= opts.buffer then
      return true
    end
    if want_pats then
      -- au.opts.pattern may itself be a list (a multi-pattern autocmd); match if
      -- any stored pattern overlaps any requested one, like au_matches.
      local pat = au.opts.pattern
      local pats = type(pat) == "table" and pat or { pat }
      local hit = false
      for _, w in ipairs(want_pats) do
        if vim.tbl_contains(pats, w) then
          hit = true
          break
        end
      end
      if not hit then
        return true
      end
    end
    return false -- drop: every given filter matched
  end, nx._autocmds)
  nx._au_touch()
end
api.nvim_clear_autocmds = nx.autocmd.clear

-- `nx.user_command.get(opts)` / `nx.user_command.buf_get(buf, opts)` [aliases
-- `nvim_get_commands` / `nvim_buf_get_commands`]: the user-command registry as
-- neovim's introspection map (name -> definition record). nxvim's registry stores
-- only the command body, so the record carries `name`/`definition` with permissive
-- defaults for the rest — enough for a command picker to list and run
-- them. `nx.user_command.get` returns the globals; `nx.user_command.buf_get(buf)`
-- returns the buffer-local commands for `buf` (0 = current), matching neovim's split.
local function command_record(name, def, desc, complete, usage)
  return {
    name = name,
    definition = type(def) == "string" and def or "",
    -- The one-line summary passed to create() (`""` when none) — neovim omits this
    -- from nvim_get_commands, but the command-line completion catalog wants it.
    desc = desc or "",
    -- The argument signature passed to create() (`""` when none) — the synopsis the
    -- completion docs pane shows after the name, like a built-in's.
    usage = usage or "",
    nargs = "*",
    bang = false,
    bar = false,
    register = false,
    -- The argument completer (`"dir"` / `"file"`) passed to create(), or nil — read
    -- by the command-line completer to offer path completion for the argument.
    complete = complete,
    range = nil,
  }
end
local function commands_map(registry, descs, completes, usages)
  local out = {}
  descs = descs or {}
  completes = completes or {}
  usages = usages or {}
  for name, def in pairs(registry or {}) do
    out[name] = command_record(name, def, descs[name], completes[name], usages[name])
  end
  return out
end
function nx.user_command.get(_opts)
  return commands_map(
    nx._user_commands,
    nx._user_command_desc,
    nx._user_command_complete,
    nx._user_command_usage
  )
end
function nx.user_command.buf_get(buf, _opts)
  if buf == nil or buf == 0 then
    buf = nx._cur_buf and nx._cur_buf.bufnr or 0
  end
  return commands_map(
    (nx._buf_user_commands or {})[buf],
    (nx._buf_user_command_desc or {})[buf],
    (nx._buf_user_command_complete or {})[buf],
    (nx._buf_user_command_usage or {})[buf]
  )
end
api.nvim_get_commands = nx.user_command.get
api.nvim_buf_get_commands = nx.user_command.buf_get

-- `nx._remote_ts_autoinstall(langs)`: in an edit-host (daemon) session, lazily install the
-- tree-sitter parsers the remote daemon had — the first time a buffer of one of those
-- filetypes opens. `langs` is the list the server hands over (already filtered to parsers
-- NOT installed on this client). It registers a `FileType` autocmd that `:TSInstall`s the
-- buffer's filetype on first sight (deduped per session). Parsers are native + compiled
-- locally, so this mirrors the remote's language set without fetching its wrong-arch
-- binaries. Dogfoods the public `FileType` + `:TSInstall` surface — the server only supplies
-- the language list.
function nx._remote_ts_autoinstall(langs)
  local want = {}
  for _, lang in ipairs(langs or {}) do
    want[lang] = true
  end
  if next(want) == nil then
    return
  end
  local requested = {}
  nx.autocmd.create("FileType", {
    callback = function(ev)
      local ft = ev.match
      if ft and ft ~= "" and want[ft] and not requested[ft] then
        requested[ft] = true
        vim.cmd("TSInstall " .. ft)
      end
    end,
  })
end
