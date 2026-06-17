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
-- nx._buf_user_commands[bufnr][name] = command: the buffer-local command
-- registry (the analogue of the buffer-scoped `nx._keymaps` entries). A
-- buffer-local command shadows a global one of the same name and is invisible
-- from any other buffer — see nx._resolve_user_command.
nx._buf_user_commands = nx._buf_user_commands or {}
-- nx._buf_user_command_desc[bufnr][name] = desc: the buffer-local twin of
-- nx._user_command_desc.
nx._buf_user_command_desc = nx._buf_user_command_desc or {}
nx._autocmds = nx._autocmds or {}
nx._augroups = nx._augroups or {}
local augroup_seq, autocmd_seq = 0, 0

-- nx.user_command.create(name, command, opts) [alias nvim_create_user_command]:
-- register a global `:Name`. `command` is a function or an ex-command string.
-- `opts.desc` (a one-line summary) is stored alongside the body — get() surfaces it
-- and the command-line completion catalog shows it as the command's docs.
function nx.user_command.create(name, command, opts)
  nx._user_commands[name] = command
  nx._user_command_desc[name] = type(opts) == "table" and opts.desc or nil
end

-- nx.user_command.buf_create(buffer, name, command, opts) [alias
-- nvim_buf_create_user_command]: register a *buffer-local* command (`buffer` 0 =
-- current). It dispatches only while that buffer is current and shadows a global
-- command of the same name there — everywhere else it's unknown. Lives in its own
-- per-bufnr table so the global registry stays clean; nx._resolve_user_command
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
end

-- Resolve a typed `:Name` to its command definition for buffer `bufnr` (0 =
-- current): a buffer-local command for that buffer wins over a global of the
-- same name (matching neovim), and a buffer-local command is invisible from any
-- other buffer. The server passes the editor's authoritative current bufnr, so
-- this never relies on a possibly-stale nx._cur_buf. Returns the function /
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
  nx._purge_buf_keymaps(bufnr)
end

-- nx.augroup.create(name[, {clear=…}]) [alias nvim_create_augroup]: define (or
-- look up) an augroup. When the group already exists and `clear` is set (the
-- default), its autocmds are removed first — so re-sourcing a config that
-- recreates its groups doesn't double-register. The group id is stable across
-- recreation (callers store it and pass it as `opts.group` to nx.autocmd.create).
function nx.augroup.create(name, opts)
  opts = opts or {}
  local clear = opts.clear ~= false -- absent → clear, matching neovim's default
  local id = nx._augroups[name]
  if id and clear then
    nx._autocmds = vim.tbl_filter(function(au)
      return au.group ~= id
    end, nx._autocmds)
  end
  if not id then
    augroup_seq = augroup_seq + 1
    id = augroup_seq
    nx._augroups[name] = id
  end
  return id
end

-- nx.autocmd.create(event, opts) [alias nvim_create_autocmd]: register a
-- callback/command for `event`. `opts.group` (numeric id or augroup name) ties it
-- to a group so a later `clear` can drop it; `opts.buffer` makes it buffer-local
-- (only fires for that buffer; 0 resolves to the current snapshot buffer at
-- registration time).
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
  return autocmd_seq
end

-- nx.autocmd.del(id) [alias nvim_del_autocmd]: remove the autocmd with this id,
-- so it stops firing.
function nx.autocmd.del(id)
  nx._autocmds = vim.tbl_filter(function(au)
    return au.id ~= id
  end, nx._autocmds)
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
-- Returns whether any autocmd actually ran — the `apply_autocmds()` boolean
-- neovim's `buf_check_timestamp` branches on (an autocmd ran → honor v:fcs_choice;
-- none → default warning). Callers that ignore the return value are unaffected.
function nx._fire(event, pattern, buf, file, data)
  local any = false
  local fired -- ids of `++once` autocmds to drop after this pass (nil = none)
  for _, au in ipairs(nx._autocmds) do
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

-- The `FileChangedShell` round-trip the server's file-change reconcile drives
-- (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → the watch leg). Set
-- `v:fcs_reason` to `reason` and reset `v:fcs_choice` to "" (neovim's defaults
-- before the autocmd), fire `FileChangedShell` for `buf`/`file`, and return whether
-- any handler ran. A handler reads `vim.v.fcs_reason` and may set `vim.v.fcs_choice`
-- to "reload"/"edit"/"ask" to redirect the reconcile; the server reads it back via
-- `nx._fcs_choice`.
function nx._fire_file_changed(reason, buf, file)
  nx._v_mirror.fcs_reason = reason
  nx._v_mirror.fcs_choice = ""
  return nx._fire("FileChangedShell", file, buf, file)
end

-- Read the `v:fcs_choice` a `FileChangedShell` handler set (or "" if none did) —
-- the second half of the round-trip above.
function nx._fcs_choice()
  return nx._v_mirror.fcs_choice or ""
end

-- Fire `DirChanged` after a `:cd` / `:chdir` changed the working directory (the
-- server calls this through `LuaRuntime::fire_dir_changed`). Set `v:event` to
-- neovim's `{ cwd, scope, changed_window }` payload before firing — a handler
-- reading `vim.v.event.cwd` (project / session plugins) sees it — and pass the
-- same table as `args.data`. The autocmd pattern matches `scope` ("global" for
-- `:cd`); `<afile>` (`args.file`) is the new directory.
function nx._fire_dir_changed(scope, cwd)
  local event = { cwd = cwd, scope = scope, changed_window = false }
  nx._v_mirror.event = event
  nx._fire("DirChanged", scope, nil, cwd, event)
end

-- nx.autocmd.exec(event, opts) [alias nvim_exec_autocmds]: fire `event` (or a
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

-- nx.autocmd.get(opts) [alias nvim_get_autocmds]: introspect the registered
-- autocmds — a debugging affordance for confirming what `clear`/`del` left
-- behind. Returns a list of `{id, event, group, group_name, pattern, buffer,
-- command}` entries, optionally filtered by `opts.event` (string or list) and
-- `opts.group` (id or name). Run it interactively as
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
-- and patterns are lists; a "*" event matches any event. A pattern-less autocmd
-- is treated as "*" for matching, mirroring nx._fire's pattern rule.
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
-- trimmed remainder (both "" when `s` is empty).
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
-- current buffer. `++once` fires once then self-removes (honored by nx._fire);
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
-- the manual analogue of nvim_exec_autocmds. The optional [group] argument vim
-- accepts is not supported — nx._fire has no group filter — so the first word
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
-- same nx._user_commands (or, with -buffer, the current buffer's local) store
-- the nvim_create_user_command API uses, so a `:command`-defined command and an
-- API-defined one dispatch identically — which is how most vimscript plugins
-- define their commands. Returns "" on success, an `E…` error, or a newline-
-- joined listing for a bare `:command`. `bang` is the replace-existing `!`.
--
-- INCOMPLETE vs neovim: attributes other than -buffer are parsed-and-ignored
-- (the command still registers and runs, just without arg-count / completion
-- enforcement); the range/count escapes (<line1>/<line2>/<count>) and an
-- invocation-time <bang> aren't plumbed through user-command dispatch yet, so
-- they expand to "".
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

-- nx.exec(src, output) [alias nvim_exec]: run the ex-command(s) in `src` and,
-- when `output` is truthy, return the text they produced (see exec_capture).
function nx.exec(src, output)
  return exec_capture(src, output)
end

-- nvim_exec2(src, opts): the 0.9+ neovim-shaped wrapper around nvim_exec — same
-- execution, but the captured text is returned under `.output` (only when
-- `opts.output` is set). A `vim.api`-only compat shim with no distinct nx twin
-- (the canonical nxvim form is nx.exec); its body only wraps the sibling nvim_
-- function, so it carries no implementation of its own.
function vim.api.nvim_exec2(src, opts)
  opts = opts or {}
  local out = vim.api.nvim_exec(src, opts.output)
  return opts.output and { output = out } or {}
end

-- ----- vim.cmd: callable AND indexable ---------------------------------------
-- vim.cmd("…") queues a raw ex-command (the Rust function installed earlier);
-- vim.cmd.colorscheme("x") / vim.cmd.set("number") build "<name> <args…>".
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

-- nx.autocmd.clear(opts) [alias nvim_clear_autocmds]: remove every autocmd
-- matching the filter — the bulk analogue of nx.autocmd.del. `opts.event`
-- (string/list), `opts.group` (id or name), `opts.buffer`, and `opts.pattern`
-- (string/list) all narrow the set; an empty opts clears everything. Mirrors
-- nx.autocmd.get's matching.
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
      local pat = au.opts.pattern
      if not vim.tbl_contains(want_pats, pat) then
        return true
      end
    end
    return false -- drop: every given filter matched
  end, nx._autocmds)
end
api.nvim_clear_autocmds = nx.autocmd.clear

-- nx.user_command.get(opts) / nx.user_command.buf_get(buf, opts) [aliases
-- nvim_get_commands / nvim_buf_get_commands]: the user-command registry as
-- neovim's introspection map (name -> definition record). nxvim's registry stores
-- only the command body, so the record carries `name`/`definition` with permissive
-- defaults for the rest — enough for a command picker to list and run
-- them. `nx.user_command.get` returns the globals; `nx.user_command.buf_get(buf)`
-- returns the buffer-local commands for `buf` (0 = current), matching neovim's split.
local function command_record(name, def, desc)
  return {
    name = name,
    definition = type(def) == "string" and def or "",
    -- The one-line summary passed to create() (`""` when none) — neovim omits this
    -- from nvim_get_commands, but the command-line completion catalog wants it.
    desc = desc or "",
    nargs = "*",
    bang = false,
    bar = false,
    register = false,
    complete = nil,
    range = nil,
  }
end
local function commands_map(registry, descs)
  local out = {}
  descs = descs or {}
  for name, def in pairs(registry or {}) do
    out[name] = command_record(name, def, descs[name])
  end
  return out
end
function nx.user_command.get(_opts)
  return commands_map(nx._user_commands, nx._user_command_desc)
end
function nx.user_command.buf_get(buf, _opts)
  if buf == nil or buf == 0 then
    buf = nx._cur_buf and nx._cur_buf.bufnr or 0
  end
  return commands_map((nx._buf_user_commands or {})[buf], (nx._buf_user_command_desc or {})[buf])
end
api.nvim_get_commands = nx.user_command.get
api.nvim_buf_get_commands = nx.user_command.buf_get
