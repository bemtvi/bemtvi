-- bemtvi Lua prelude — autocmds, augroups, user commands, ex-command drivers.
-- The autocmd / augroup / user-command registries kept purely in Lua (authored as
-- btv.autocmd.* / btv.augroup.* / btv.user_command.*), the btv._fire dispatcher the
-- server reads back, the :autocmd / :augroup / :doautocmd / :command ex front-ends,
-- btv.exec, and the callable-and-indexable vim.cmd. The matching vim.api.nvim_*
-- names are aliased onto each native.
local vim = vim
local api = vim.api
btv = btv or {}
btv.autocmd = btv.autocmd or {}
btv.augroup = btv.augroup or {}
btv.user_command = btv.user_command or {}

-- ----- API surface stored purely in Lua --------------------------------------
-- Registration that needn't touch the editor lives in Lua tables; the server
-- reads them when it must (e.g. dispatching a user command typed as `:Foo`).

btv._user_commands = btv._user_commands or {}
-- btv._user_command_desc[name] = desc: the optional one-line `desc` passed to
-- create(), kept parallel to the body registry (so the dispatch path stays a plain
-- name -> body lookup). Surfaced by get() and the command-line completion catalog.
btv._user_command_desc = btv._user_command_desc or {}
-- btv._user_command_complete[name] = spec: the optional `complete` passed to
-- create() — `"dir"` / `"file"` (the only argument completers wired so far; an
-- unknown spec is stored but ignored). Kept parallel to the body registry like the
-- desc table; the command-line completer reads it to offer path completion for a
-- user command's argument (e.g. the GUI's `:workspace <dir>`).
btv._user_command_complete = btv._user_command_complete or {}
-- btv._user_command_usage[name] = usage: the optional `usage` passed to create() — the
-- argument signature shown after the command name in the command-line completion docs
-- pane (e.g. `[config]`, `{file}`), exactly as a built-in command's synopsis carries it.
-- Kept parallel to the body registry like the desc / complete tables.
btv._user_command_usage = btv._user_command_usage or {}
-- btv._buf_user_commands[bufnr][name] = command: the buffer-local command
-- registry (the analogue of the buffer-scoped `btv._keymaps` entries). A
-- buffer-local command shadows a global one of the same name and is invisible
-- from any other buffer — see btv._resolve_user_command.
btv._buf_user_commands = btv._buf_user_commands or {}
-- btv._buf_user_command_desc[bufnr][name] = desc: the buffer-local twin of
-- btv._user_command_desc.
btv._buf_user_command_desc = btv._buf_user_command_desc or {}
-- btv._buf_user_command_complete[bufnr][name] = spec: the buffer-local twin of
-- btv._user_command_complete.
btv._buf_user_command_complete = btv._buf_user_command_complete or {}
-- btv._buf_user_command_usage[bufnr][name] = usage: the buffer-local twin of
-- btv._user_command_usage.
btv._buf_user_command_usage = btv._buf_user_command_usage or {}
btv._autocmds = btv._autocmds or {}
btv._augroups = btv._augroups or {}
local augroup_seq, autocmd_seq = 0, 0

-- A monotonic version bumped on every change to btv._autocmds (register / delete /
-- clear). The server reads it once per input batch (LuaRuntime::autocmd_version)
-- and, only when it advanced, refreshes its cached set of registered event names —
-- the gate that lets high-frequency events (CursorMoved / TextChanged) cost nothing
-- when no handler wants them (mirroring btv._keymaps_version). Bumped through
-- btv._au_touch() at every mutation site below.
btv._au_version = btv._au_version or 0
function btv._au_touch()
  btv._au_version = btv._au_version + 1
end

-- neovim autocmd-event aliases → the canonical event bemtvi actually fires
-- (neovim's `src/nvim/auevents.lua` alias table). Registering, firing, querying,
-- or clearing by an alias behaves exactly as if the canonical name were used, so a
-- config that does `autocmd BufRead` (muscle memory for `BufReadPost`) still fires.
-- Limited to aliases whose target bemtvi emits — an alias pointing at an unemitted
-- event would just be a silent no-op, so we don't pretend to support it.
local EVENT_ALIASES = {
  BufRead = "BufReadPost",
  BufWrite = "BufWritePre",
  BufCreate = "BufAdd",
  FileEncoding = "EncodingChanged",
}

-- Canonicalize an event name (string) or list of names: each alias maps to its
-- real event, everything else passes through unchanged. Preserves the shape of the
-- input (string in → string out, list in → list out) so callers stay simple.
---@overload fun(ev: string): string
---@overload fun(ev: string[]): string[]
local function au_canon_event(ev)
  if type(ev) == "table" then
    local out = {}
    for i, e in ipairs(ev) do
      out[i] = EVENT_ALIASES[e] or e
    end
    return out
  end
  return EVENT_ALIASES[ev] or ev
end
btv._canon_event = au_canon_event

-- The distinct event names any registered autocmd listens for (an autocmd may name
-- a single event or a list). The server caches this — refreshed only when
-- btv._au_version advances — so its per-key lifecycle diff can skip computing /
-- firing an event nothing is registered for.
function btv._au_event_set()
  local seen = {}
  local out = {}
  for _, au in ipairs(btv._autocmds) do
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

-- Reject a user-command name the ex-command dispatcher can never reach. `:Name`
-- resolves by an exact match on the command word, so a name carrying whitespace or
-- punctuation is registered-but-undispatchable: `:Name` reports `E492` for a command
-- sitting right there in the registry, and every diagnostic shows it as present. A
-- trailing space in a config is invisible on the page, so this has to fail at
-- REGISTRATION — the call site that would notice never runs. Lowercase names stay
-- legal: bemtvi dispatches plugin-provided `:help` / `:h`, so this checks the
-- characters, not vim's uppercase-initial convention.
local function check_command_name(name)
  if type(name) ~= "string" or name == "" or name:find("[^%w_]") then
    error("E182: Invalid command name: " .. tostring(name), 0)
  end
end

-- `btv.user_command.create(name, command, opts)` [alias `nvim_create_user_command`]:
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
--     `btv.async` function) lists in the picker. A throw / rejection yields no candidates.
function btv.user_command.create(name, command, opts)
  check_command_name(name)
  btv._user_commands[name] = command
  btv._user_command_desc[name] = type(opts) == "table" and opts.desc or nil
  btv._user_command_complete[name] = type(opts) == "table" and opts.complete or nil
  btv._user_command_usage[name] = type(opts) == "table" and opts.usage or nil
end

-- `btv.user_command.buf_create(buffer, name, command, opts)` [alias
-- `nvim_buf_create_user_command`]: register a *buffer-local* command (`buffer` 0 =
-- current). It dispatches only while that buffer is current and shadows a global
-- command of the same name there — everywhere else it's unknown. Lives in its own
-- per-bufnr table so the global registry stays clean; `btv._resolve_user_command`
-- consults both at dispatch.
function btv.user_command.buf_create(buffer, name, command, opts)
  check_command_name(name)
  if buffer == nil or buffer == 0 then
    buffer = btv._cur_buf and btv._cur_buf.bufnr or 0
  end
  -- Lazily materialize the buffer's slot in each per-bufnr registry.
  local function slot(registry)
    local t = registry[buffer]
    if not t then
      t = {}
      registry[buffer] = t
    end
    return t
  end
  slot(btv._buf_user_commands)[name] = command
  slot(btv._buf_user_command_desc)[name] = type(opts) == "table" and opts.desc or nil
  slot(btv._buf_user_command_complete)[name] = type(opts) == "table" and opts.complete or nil
  slot(btv._buf_user_command_usage)[name] = type(opts) == "table" and opts.usage or nil
end

-- Resolve a typed `:Name` to its command definition for buffer `bufnr` (0 =
-- current): a buffer-local command for that buffer wins over a global of the
-- same name (matching neovim), and a buffer-local command is invisible from any
-- other buffer. The server passes the editor's authoritative current bufnr, so
-- this never relies on a possibly-stale `btv._cur_buf`. Returns the function /
-- string body, or nil when no command matches.
function btv._resolve_user_command(name, bufnr)
  if bufnr == nil or bufnr == 0 then
    bufnr = btv._cur_buf and btv._cur_buf.bufnr or 0
  end
  local locals = btv._buf_user_commands[bufnr]
  if locals and locals[name] ~= nil then
    return locals[name]
  end
  return btv._user_commands[name]
end

-- Drop everything scoped to buffer `bufnr` when the server reports it deleted, so
-- a later buffer reusing the bufnr can't inherit a stale buffer-local command or
-- mapping (matching neovim's bufwipe cleanup). The keymap purge lives in
-- keymap.lua, where the trie source / fn table are owned.
function btv._cleanup_buffer(bufnr)
  btv._buf_user_commands[bufnr] = nil
  btv._buf_user_command_desc[bufnr] = nil
  btv._buf_user_command_complete[bufnr] = nil
  btv._buf_user_command_usage[bufnr] = nil
  btv._purge_buf_keymaps(bufnr)
end

-- `btv.augroup.create(name, opts)` -> id [alias `nvim_create_augroup`]: define (or look
-- up) an autocommand group and return its numeric id. An augroup is just a named
-- bucket for autocmds: pass the returned id as `opts.group` to `btv.autocmd.create` so
-- the whole set can be cleared and re-registered as a unit.
--
-- Arguments:
--   * `name` — the group name (string). Calling create again with the same name
--     returns the SAME id; the id is stable across recreation, so it's safe to store.
--   * `opts.clear` — when the group already exists, whether to remove its existing
--     autocmds first. Defaults to TRUE (matching neovim). This is what makes
--     re-sourcing your config idempotent: a config that does
--     `btv.augroup.create("MyGroup")` on every load clears the previous run's autocmds
--     instead of double-registering them. Pass `{ clear = false }` to keep them
--     (the augroup-block / `:augroup` ex-command path uses this to append).
--
-- The idiomatic pattern — own a group, then hang autocmds off it:
--
-- ```lua
-- local grp = btv.augroup.create("MyConfig")                 -- clears on re-source
-- btv.autocmd.create("BufEnter", {
--   group = grp,                                            -- numeric id, or "MyConfig"
--   callback = function(ev) btv.notify("entered " .. (ev.file or "[No Name]")) end,
-- })
-- ```
-- Resolve an `opts.group` (an augroup id, or its name) to an id. `nil` means "every
-- group" — no filter on the query APIs, ungrouped on `create`. An unknown NAME fails
-- loud rather than falling back to nil, because every consumer degrades badly on nil
-- and does so SILENTLY: a typo'd `nvim_exec_autocmds{group=…}` would broadcast to
-- every subscriber, a typo'd `nvim_clear_autocmds{group=…}` would delete every
-- autocmd, and a typo'd `nvim_create_autocmd{group=…}` would register the autocmd
-- ungrouped — where no later `augroup(…, {clear=true})` can reach it, so handlers
-- stack on every config reload (the exact failure that idiom exists to prevent).
-- `where` names the calling API in the error. Matches neovim, which raises on an
-- invalid augroup.
local function au_resolve_group(spec, where)
  if spec == nil or type(spec) == "number" then
    return spec
  end
  if type(spec) ~= "string" then
    error(where .. ": `group` must be an augroup id or name, got " .. type(spec), 2)
  end
  local id = btv._augroups[spec]
  if id == nil then
    error(where .. ": invalid augroup '" .. spec .. "'", 2)
  end
  return id
end
btv._resolve_augroup = au_resolve_group

function btv.augroup.create(name, opts)
  opts = opts or {}
  local clear = opts.clear ~= false -- absent → clear, matching neovim's default
  local id = btv._augroups[name]
  if id and clear then
    btv._autocmds = vim.tbl_filter(function(au)
      return au.group ~= id
    end, btv._autocmds)
    btv._au_touch()
  end
  if not id then
    augroup_seq = augroup_seq + 1
    id = augroup_seq
    btv._augroups[name] = id
  end
  return id
end

-- vim's file-pattern rules, spelled as `btv.glob` options. They differ from the
-- `btv.glob` defaults on exactly two counts, both deliberate:
--   * `literal_separator = false` — a bare `*` CROSSES `/` in vim (`/etc/*` matches
--     `/etc/nginx/nginx.conf`), where every other glob dialect stops at it.
--   * `basename = true` — a separator-less pattern matches the path TAIL, so
--     `*.lua` fires for `/a/b/c/init.lua`.
local AU_GLOB = { literal_separator = false, basename = true }

-- Compile every glob in `pat` (a string, a list, or nil) at REGISTRATION time so an
-- invalid one raises where the caller wrote it, naming the pattern and the reason.
--
-- Without this the failure is invisible: matching happens inside a `pcall` per event
-- fire (it must — an autocmd cannot be allowed to raise out of every subsequent event),
-- so a pattern that cannot compile just never matches, forever, with no diagnostic
-- anywhere. Compiling here is also free at fire time: it warms the very cache the
-- matcher reads.
--
-- A metacharacter-free pattern is skipped — it is only ever an exact compare, so it
-- never reaches the glob engine (and a `[No Name]`-style name must stay usable).
local function au_check_patterns(event, pat)
  if pat == nil then
    return
  end
  local list = type(pat) == "table" and pat or { pat }
  for _, p in ipairs(list) do
    if type(p) == "string" and btv.glob.is_glob(p) then
      local ok, err = pcall(btv.glob.compile, p, AU_GLOB)
      if not ok then
        error(
          ("btv.autocmd.create: %s autocmd has an invalid pattern %q: %s"):format(
            type(event) == "table" and table.concat(event, "/") or tostring(event),
            p,
            -- Parenthesized: `gsub` returns (string, count), and the second value
            -- would ride into `format` as a stray argument.
            (tostring(err):gsub("^.*btv%.glob: ", ""))
          ),
          -- Level 3, not 2: 1 is this function, 2 is `btv.autocmd.create` (which is
          -- prelude source the caller never wrote), 3 is the config line that called
          -- it. Blaming `bemtvi:prelude/autocmd` for a typo in the user's pattern is
          -- the opposite of raising "where the caller wrote it".
          3
        )
      end
    end
  end
end

-- Modules that merely *forward* a registration — `btv.on` hands straight to
-- `btv.autocmd.create`, and `create` is where the capture happens — so their frames are
-- never the interesting answer and the walk skips past them. Every other frame is
-- reported as-is: a user's `init.lua`, a plugin, or another prelude module registering
-- a handler of its own (`bemtvi:prelude/statusline:186` is a useful answer).
local SITE_SKIP = {
  ["bemtvi:prelude/autocmd"] = true,
  ["bemtvi:prelude/btv"] = true,
}

-- `debug.getinfo().short_src` renders a *named string chunk* — which is how every
-- prelude module is loaded (`bemtvi:prelude/autocmd`, see `runtime.rs`) — in the
-- bracketed form `[string "bemtvi:prelude/autocmd"]`. Unwrap it to the bare chunk name
-- so `SITE_SKIP` can compare exact module names, and so a site that legitimately lands
-- in the prelude reads `bemtvi:prelude/statusline:186` rather than the noisy form. A
-- file-backed chunk (a user's `init.lua`, a plugin) has no wrapper and passes through.
local function unwrap_chunk_name(src)
  return src:match('^%[string "(.*)"%]$') or src
end

-- Where the calling code registered this autocmd, as `"src:line"`. Captured ONCE per
-- registration — never per fire — so the cost is paid at config time and a
-- slow, hung, or contract-violating handler can be traced back to the line that
-- installed it. Without this, "a FileType handler exceeded its budget" is unactionable
-- with N plugins loaded. Returns nil rather than guessing when the stack is all C
-- frames (a registration driven from Rust, e.g. the `:autocmd` ex-command).
local function capture_site()
  for lvl = 2, 12 do
    local info = debug.getinfo(lvl, "Sl")
    if not info then
      return nil
    end
    local src = info.short_src and unwrap_chunk_name(info.short_src)
    if src and src ~= "[C]" and not SITE_SKIP[src] then
      return src .. ":" .. tostring(info.currentline)
    end
  end
  return nil
end

-- `btv.autocmd.create(event, opts)` -> id [alias `nvim_create_autocmd`]: run something
-- whenever `event` fires. Returns the autocmd's numeric id (pass it to
-- `btv.autocmd.del` to remove it). `event` is an event name (`"FileType"`,
-- `"BufEnter"`, …) or a list of names to share one handler — see the
-- [autocommand events](../plugins/autocmd-events.md) reference for the events
-- bemtvi emits and what each carries. neovim aliases (`"BufRead"` -> `BufReadPost`,
-- `"BufWrite"` -> `BufWritePre`, `"BufCreate"` -> `BufAdd`, `"FileEncoding"` ->
-- `EncodingChanged`) are accepted and canonicalized to the real event.
--
-- `opts` fields:
--   * `callback` — a function run when the event fires; OR `command` — an
--     ex-command string queued instead. Provide one of the two.
--   * `pattern` — a glob (or list of globs) the event's match string is tested
--     against (e.g. `"*.lua"`, `{ "*.c", "*.h" }`). Omitted / `"*"` matches all.
--   * `group` — an augroup, by numeric id or by name (see `btv.augroup.create`). Ties
--     this autocmd to the group so a later `clear` of that group drops it.
--   * `buffer` — make it buffer-local: it then fires only for that buffer (and
--     `pattern` is ignored). `0` resolves to the current buffer at registration time.
--   * `once` — fire once, then auto-remove. `desc` — a human description.
--   * `timeout` — how many milliseconds the editor waits for *this* handler's returned
--     promise before warning and moving on (default 500). It bounds the **wait**, never
--     the delivery: the work keeps running and late subscribers are still served. Raise
--     it for a handler you know is slow (a first LSP spawn) so it does not warn on every
--     open. Ignored by the hot-path events, whose handlers must be synchronous.
--
-- The `callback` receives one table describing the event:
--   `{ id, event, match, buf, file, data }` — `id` this autocmd's id, `event` the
--   event name, `match` the matched pattern string, `buf` the buffer number, `file`
--   its name, and `data` an event-specific payload (e.g. `LspAttach` carries
--   `{ client_id = … }`), nil for most events.
--
-- An invalid glob in `pattern` raises **here**, at registration, naming the pattern and
-- the reason — an autocmd that could never match is a typo, and the alternative is one
-- that silently never fires for the rest of the session.
--
-- ```lua
-- btv.autocmd.create("FileType", {
--   pattern = "markdown",
--   callback = function(ev)
--     btv.bo[ev.buf].textwidth = 80
--   end,
-- })
-- ```
function btv.autocmd.create(event, opts)
  -- Named here, because the alternative is the worst error in the API: a non-table
  -- `opts` (nearly always the handler, passed as if this were `btv.on`) reaches
  -- `opts.pattern` below and raises `attempt to index a function value` from inside
  -- the prelude — no mention of the caller, and since a config is one chunk, every
  -- line after the registration silently never runs. `btv.on` accepts the bare-handler
  -- form outright; this signature is neovim's, where `opts` is always a table, so it
  -- says so and points at the spelling that does take one.
  if opts ~= nil and type(opts) ~= "table" then
    error(
      ("btv.autocmd.create: opts must be a table, got %s%s"):format(
        type(opts),
        type(opts) == "function" and " (for a bare handler use btv.on(event, fn))" or ""
      ),
      2
    )
  end
  opts = opts or {}
  event = au_canon_event(event)
  au_check_patterns(event, opts.pattern)
  autocmd_seq = autocmd_seq + 1
  local group = au_resolve_group(opts.group, "nvim_create_autocmd")
  local buffer = opts.buffer
  if buffer == 0 then
    buffer = btv._cur_buf and btv._cur_buf.bufnr or 0
  end
  btv._autocmds[#btv._autocmds + 1] = {
    id = autocmd_seq,
    event = event,
    opts = opts,
    group = group,
    buffer = buffer,
    site = capture_site(),
  }
  btv._au_touch()
  return autocmd_seq
end

-- `btv.autocmd.del(id)` [alias `nvim_del_autocmd`]: remove the autocmd with this id,
-- so it stops firing.
function btv.autocmd.del(id)
  btv._autocmds = vim.tbl_filter(function(au)
    return au.id ~= id
  end, btv._autocmds)
  btv._au_touch()
end

-- Does a single autocmd pattern `pat` match the event's `pattern` (the file path
-- for file events, a filetype / id / mode-code for others)? Beyond an exact match
-- and `*`, a `pat` holding a glob metacharacter is matched as vim's file-pattern
-- through `btv.glob` (the canonical engine: `bemtvi_core::glob`, which compiles the
-- glob to a cached regex). A metacharacter-free `pat` is only ever an exact compare,
-- so a `FileType` `rust` autocmd can't glob-match a path whose tail is `rust`.
local function au_one_pattern_matches(pat, pattern)
  if pat == "*" or pat == pattern then
    return true
  end
  if pattern == nil or type(pat) ~= "string" then
    return false
  end
  if not btv.glob.is_glob(pat) then
    return false -- no glob: exact compare above is the only match
  end
  -- A backstop, not the diagnostic: `au_check_patterns` already rejected an
  -- uncompilable pattern at registration, so reaching the failure arm here means the
  -- pattern arrived some other way. It must not raise out of every subsequent event
  -- fire, so it matches nothing — the literal spelling was already caught by the exact
  -- compare above. (An unclosed class like `foo[bar` is not invalid: the engine takes
  -- it as literal text.)
  local ok, matched = pcall(btv.glob.match, pat, pattern, AU_GLOB)
  return ok and matched
end

-- Whether the autocmd's `pat` (a string, a list, or nil = match-all) matches the
-- fired `pattern`. Used by `btv._fire` below.
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
-- Shared matcher for the event fire paths: call `run(au)` for each registered autocmd
-- matching (`event`, `pattern`, `buf`) in registration order, then drop any `++once`
-- ones that fired (collected during the pass and removed after it, so the live `ipairs`
-- isn't mutated underneath). `btv._fire` / `btv._fire_gated` each supply their own
-- per-handler `run` (they differ only in what they do with the callback's return value).
-- `min_id` (optional) restricts the pass to autocmds registered *after* that id — the
-- replay filter: it re-delivers an event to handlers that appeared while the first
-- dispatch's async handlers were still running, without re-running the ones that
-- already saw it. Ids are monotonic (`autocmd_seq`), so the comparison is exact and no
-- handler can ever observe the same fire twice.
local function au_dispatch(event, pattern, buf, run, group, min_id)
  local fired -- ids of `++once` autocmds to drop after this pass (nil = none)
  for _, au in ipairs(btv._autocmds) do
    local ev = au.event
    local ev_ok = ev == event or (type(ev) == "table" and vim.tbl_contains(ev, event))
    if
      ev_ok
      and (min_id == nil or au.id > min_id)
      and au_pattern_matches(au.opts.pattern, pattern)
      and (au.buffer == nil or au.buffer == buf)
      -- `group` (an augroup id, nil = every group) narrows a MANUAL fire to one
      -- group's handlers. Editor-triggered events pass nil: a real event belongs to
      -- every subscriber. See `btv.autocmd.exec`.
      and (group == nil or au.group == group)
    then
      run(au)
      if au.opts.once then
        fired = fired or {}
        fired[au.id] = true
      end
    end
  end
  if fired then
    btv._autocmds = vim.tbl_filter(function(au)
      return not fired[au.id]
    end, btv._autocmds)
  end
end

-- ----- hot-path events --------------------------------------------------------

-- The **hot-path** events: the ones the editor fires while converging a single input
-- tick — i.e. on (nearly) every keypress. Their handlers must be **synchronous**: a
-- hot-path handler that returns a promise raises, because the editor will not wait
-- for it and an author who returns one is expecting sequencing that cannot happen.
-- Async work is still allowed from a hot-path handler; it just has to be *started
-- and not returned* (`btv.schedule` / `btv.on_next_tick` are the escape hatches).
--
-- The split is what keeps the settle protocol off the input tick entirely: only a
-- *non*-hot event can park the gated read chain or arm a replay pass, and those fire
-- roughly once per buffer rather than once per key. Every other event is non-hot and
-- async-capable. See `docs/plans/2026-07-26-async-event-model.md`.
local HOT_EVENTS = {
  BufEnter = true,
  BufLeave = true,
  CursorMoved = true,
  CursorMovedI = true,
  InsertEnter = true,
  InsertLeave = true,
  ModeChanged = true,
  TextChanged = true,
  TextChangedI = true,
  WinEnter = true,
  WinLeave = true,
  WinResized = true,
  WinScrolled = true,
}
btv._hot_events = HOT_EVENTS

-- The error raised when a hot-path handler returns a promise. It names the event and
-- where the handler was registered (`au.site`, captured at registration; until that
-- lands the autocmd id stands in), and — the part that makes it actionable rather
-- than merely loud — spells out the escape hatch, so the fix is obvious from the
-- message alone without reading any docs.
local function hot_async_error(au, event)
  local where = au.site and ("registered at " .. au.site) or ("autocmd id " .. tostring(au.id))
  return "bemtvi: "
    .. event
    .. " handlers must be synchronous ("
    .. where
    .. "): it fires on every keypress, so the editor cannot wait for a promise. "
    .. "Start the async work with btv.schedule / btv.on_next_tick and return nothing."
end

-- Track the promise a non-hot-path handler returned. The editor does **not** block on
-- it — only `BufWritePre` awaits (`btv._fire_gated`); every other non-hot event is
-- *async-tolerant*: a handler may kick off async work (an LSP request from
-- `FileType`, a `btv.fs` write from `BufWritePost`) and the fire returns without
-- waiting. But an async-tolerant return must not be *silently dropped* — a handler
-- whose promise rejects (a failed request, a throw in a `:next`) has to surface, not
-- vanish. Attaching this `:catch` marks the promise handled, so the generic
-- unhandled-rejection reporter (`promise.lua`) steps aside and the error lands on the
-- message line **named for the event that raised it** — which handler failed is the
-- whole point of surfacing it. A non-promise return is ignored (the common case).
local function track_au_promise(ret, event)
  if btv._is_promise(ret) then
    ret:catch(function(err)
      btv.notify(
        "bemtvi: autocmd " .. tostring(event) .. " handler rejected: " .. tostring(err),
        "error"
      )
    end)
  end
end

-- ----- settle protocol: replay to late subscribers ----------------------------
--
-- Neovim's event guarantee is trivially "when the fire returns, everything it
-- triggered has finished" — it is synchronous. Ours cannot be, so the async analogue
-- has two halves: events are ordered against each other (the gated read chain), and
-- **anyone who shows up during the settle window still gets the event**. This is that
-- second half, and it is what makes an `ft`-lazy plugin with an *async* `config` work:
-- the trigger loads the plugin, the plugin registers its own `FileType` handler a tick
-- or more later, and that handler still runs for the buffer that woke it.
--
-- See `docs/plans/2026-07-26-async-event-model.md`.

-- How long a fire waits for its async handlers before replaying anyway. This is NOT
-- merely diagnostic: once the read chain gates on these settles, a handler that never
-- resolves would leave a buffer permanently half-initialized, so expiry has to advance
-- the world regardless. Overridable per-autocmd via `opts.timeout` for a legitimately
-- slow one-off (a first LSP spawn) that should not warn on every open.
local DEFAULT_SETTLE_BUDGET_MS = 500
-- A replayed handler may itself load something that registers more handlers, so the
-- pass runs to a fixpoint. An unbounded registration loop must fail loud rather than
-- spin, so the rounds are capped.
local REPLAY_MAX_ROUNDS = 8
-- Sentinel distinguishing "the budget elapsed" from "the handlers settled" as the
-- winner of the race below (a handler could otherwise fulfil with anything).
local SETTLE_TIMEOUT = "\0btv_settle_timeout"

-- Handler promises still in flight, keyed by a monotonic token: `btv.autocmd.pending()`
-- reads it. Entries are cleared when their promise settles, so what remains past its
-- budget is exactly the set of slow — or permanently hung — handlers. A hung handler
-- never settles and so never warns on completion; this table is how it stays visible.
btv._au_pending = btv._au_pending or {}
local au_pending_seq = 0

-- The budget for one fire: the most generous request among the handlers we are waiting
-- on, so a handler that asked for more time gets it rather than being cut short by a
-- stricter sibling on the same event. The default is what a handler that expressed no
-- preference asks for — NOT a floor, so a lone `timeout = 20` handler really does get
-- 20ms rather than being silently raised to the default.
local function settle_budget(waits)
  local budget
  for _, w in ipairs(waits) do
    local t = w.au.opts.timeout
    if type(t) ~= "number" then
      t = DEFAULT_SETTLE_BUDGET_MS
    end
    if budget == nil or t > budget then
      budget = t
    end
  end
  return budget or DEFAULT_SETTLE_BUDGET_MS
end

local function site_of(w)
  return w.au.site or ("autocmd id " .. tostring(w.au.id))
end

-- Where the still-unsettled handlers of this fire were registered, for the warnings.
-- Naming the site is the whole point: "a FileType handler was slow" is unactionable
-- with N plugins loaded.
--
-- Falls back to naming EVERY handler this fire waited on when none looks unsettled.
-- That is not hypothetical: the budget timer and the handlers can become ready in the
-- same loop turn, and `race` may still pick the timeout even though `finally` has
-- already marked them done — leaving the precise filter with nothing to report. Naming
-- all of them is less pointed but always true; naming none is useless.
local function unsettled_sites(waits)
  local out = {}
  for _, w in ipairs(waits) do
    if not w.done then
      out[#out + 1] = site_of(w)
    end
  end
  if #out == 0 then
    for _, w in ipairs(waits) do
      out[#out + 1] = site_of(w)
    end
  end
  return table.concat(out, ", ")
end

local arm_settle -- forward declaration: each replay round can arm the next

-- Wait for one fire's async handlers, then hand the event to whatever registered while
-- they were running. `cursor` is `{ hw = <highest autocmd id already delivered to> }`,
-- so a replay reaches exactly the handlers that missed the event.
--
-- The cursor is a shared, mutable box rather than a per-round number **on purpose**: one
-- fire can have two live replay paths at once. A fire whose budget expired declares
-- convergence and replays, and its late subscribers may go async and arm a further
-- round — while the handler that blew the budget is still running and will replay again
-- when it finally settles. Given a number, each path advances its own copy and both
-- dispatch the same id range, so a handler registered in between receives the event
-- TWICE — the exact thing the watermark exists to prevent. Sharing the box makes the
-- delivered-up-to point global to the fire, so whichever path reaches a handler first is
-- the only one that does.
--
-- `on_done` (optional) is called EXACTLY ONCE when this fire has fully converged —
-- handlers settled and every replay round done. It is how a *gated* caller learns the
-- event is finished, so the read chain can advance to its next stage knowing the
-- previous one saw a settled world. A timeout counts as converged: the chain must
-- advance even on a handler that never resolves, or the buffer stays half-initialized.
arm_settle = function(ctx, cursor, waits, round, on_done)
  local finished = false
  local function finish()
    if finished then
      return
    end
    finished = true
    if on_done then
      on_done()
    end
  end

  if round > REPLAY_MAX_ROUNDS then
    btv.notify(
      "bemtvi: "
        .. ctx.event
        .. " autocmd replay did not converge after "
        .. REPLAY_MAX_ROUNDS
        .. " rounds (handlers keep registering handlers); giving up",
      "warn"
    )
    finish()
    return
  end

  local promises = {}
  for i, w in ipairs(waits) do
    promises[i] = w.promise
    -- Per-handler completion, so a timeout can name *which* handler is slow rather
    -- than the whole batch.
    w.promise:finally(function()
      w.done = true
    end)
  end

  local started = btv.now_ms()
  local budget = settle_budget(waits)
  au_pending_seq = au_pending_seq + 1
  local token = au_pending_seq
  btv._au_pending[token] = {
    event = ctx.event,
    buf = ctx.buf,
    budget = budget,
    started = started,
    waits = waits,
  }

  -- Deliver the event to handlers registered since `cursor.hw`, and if any of *those* go
  -- async, arm the next round for them. Reads `autocmd_seq` before dispatching, since
  -- dispatching can register more. Returns whether a nested round was armed and given
  -- ownership of `finish` — when it wasn't, this fire has converged here.
  -- `chain` false means nobody is waiting on convergence any more (we already advanced
  -- past a timeout), so a nested round runs purely for the late subscribers' benefit.
  local function replay(chain)
    local next_hw = autocmd_seq
    local late
    au_dispatch(ctx.event, ctx.pattern, ctx.buf, function(au)
      local cb = au.opts.callback
      if type(cb) == "function" then
        local ret = cb({
          id = au.id,
          event = ctx.event,
          match = ctx.pattern,
          buf = ctx.buf,
          file = ctx.file or ctx.pattern,
          data = ctx.data,
        })
        if btv._is_promise(ret) then
          late = late or {}
          late[#late + 1] = { promise = ret, au = au }
          track_au_promise(ret, ctx.event)
        end
      elseif type(au.opts.command) == "string" then
        vim.cmd(au.opts.command)
      end
    end, ctx.group, cursor.hw)
    -- `next_hw` was read before the dispatch, and `autocmd_seq` only grows, so this is
    -- monotonic no matter which of the fire's live paths gets here first.
    cursor.hw = next_hw
    if late then
      arm_settle(ctx, cursor, late, round + 1, chain and finish or nil)
      return chain
    end
    return false
  end

  local settled = btv.promise.all_settled(promises)
  local timed_out = false
  -- The sites that were still unsettled when the budget blew. Captured THERE and
  -- reused by the completion warning below, because by the time a handler finally
  -- settles it is `done` and `unsettled_sites` would report nothing — losing exactly
  -- the file:line that makes the warning actionable.
  local overdue_sites = ""

  btv.promise.race({ settled, btv.promise.delay(budget, SETTLE_TIMEOUT) }):next(function(res)
    if res == SETTLE_TIMEOUT then
      timed_out = true
      overdue_sites = unsettled_sites(waits)
      btv.notify(
        "bemtvi: "
          .. ctx.event
          .. " handler exceeded its "
          .. budget
          .. "ms budget ("
          .. overdue_sites
          .. "); continuing without it",
        "warn"
      )
      -- Replay for the late subscribers, then declare convergence regardless: the
      -- budget bounds how long we WAIT, never whether subscribers get the event, and a
      -- gated caller must advance rather than hang on a handler that may never resolve.
      replay(false)
      finish()
    elseif not replay(true) then
      -- Converged here: no nested round took ownership of `finish`.
      finish()
    end
  end)

  -- Independently of the race: when the handlers do eventually settle, drop them from
  -- the pending table, and — if we had already given up on them — say so with the real
  -- elapsed time and replay once more for anything that registered in the meantime.
  settled:next(function()
    btv._au_pending[token] = nil
    if timed_out then
      btv.notify(
        "bemtvi: "
          .. ctx.event
          .. " handler ("
          .. overdue_sites
          .. ") settled "
          .. math.floor(btv.now_ms() - started)
          .. "ms after starting, past its "
          .. budget
          .. "ms budget",
        "warn"
      )
      -- Late subscribers still get the event; convergence was already declared at the
      -- timeout, so nothing is waiting on this round.
      replay(false)
    end
  end)
end

-- ----- the startup announce window --------------------------------------------

-- Plugins load **asynchronously**. `btv.plugins` awaits a spec's directory before
-- sourcing it, and a spec's `config` may `btv.await` on its own — so a plugin's `config`,
-- and every autocmd that config registers, lands several ticks into startup, *after* the
-- file named on the command line has been read. Painting before the plugins are up is
-- deliberate (it is what makes `bemtvi file.txt` open instantly), so the read genuinely
-- happens first — and a plugin whose behavior hangs off `BufReadPost` therefore does
-- nothing at all for that file, while `:e` on the same file later in the same session
-- works. Restored session windows have the same gap for the same reason.
--
-- The fix is the guarantee the settle protocol already gives *within* one fire — "late
-- subscribers still get the event" — widened to the plugin-load boundary. Every
-- first-announce event fired before the plugins are ready is RECORDED here; when
-- `PluginsLoaded` closes the window, each record is re-dispatched to the handlers that
-- registered inside it. Nothing is delayed (the built-in `FileType` consumers —
-- treesitter, LSP attach — still fire on the read, so the file colours immediately) and
-- nothing fires twice: delivery is filtered by the same registration watermark that
-- guards the async replay, and the two share one cursor per fire, so a handler either
-- path has already served is never re-run by the other.
--
-- Only the **first-announce** events are replayable. They fire once per read and carry
-- no pairing semantics, so re-delivering one is unambiguous. `BufEnter` / `BufWinEnter`
-- are deliberately NOT here: they mean "became current" / "became displayed", which may
-- no longer be true when the window closes, and a `BufEnter` replayed without its
-- `BufLeave` twin would be a lie about editor state.
local REPLAYABLE_TO_PLUGINS = {
  BufReadPost = true,
  BufNewFile = true,
  FileType = true,
}

-- Open until `PluginsLoaded` (`btv._replay_startup_announces`), which happens exactly
-- once per session. While open, every replayable fire is recorded below; once closed,
-- recording stops for good and a read announces to its handlers and no one else.
local startup_window_open = true
-- The recorded announces in fire order, plus a `(event, buffer)` -> position index that
-- keeps the list deduped: a buffer re-read (`:e!`) or a filetype changed during startup
-- replays only its LATEST state, in the position its first announce took — so a record
-- pair stays in `BufReadPost` → `FileType` order.
local startup_announces = {}
local startup_index = {}

-- Record one replayable fire. `cursor` is the fire's shared delivered-up-to box (see
-- `arm_settle`), held by reference rather than copied: if the fire is still settling
-- when the window closes, both replay paths read and advance the same watermark.
local function record_startup_announce(ctx, cursor)
  local key = ctx.event .. "\0" .. tostring(ctx.buf)
  local rec = { ctx = ctx, cursor = cursor }
  local at = startup_index[key]
  if at then
    startup_announces[at] = rec
  else
    startup_announces[#startup_announces + 1] = rec
    startup_index[key] = #startup_announces
  end
end

-- Close the startup announce window and hand every recorded announce to the handlers
-- that registered while it was open — the plugins. Called once, by the plugin manager,
-- as `PluginsLoaded` fires (`prelude/plugins.lua`).
--
-- Each record is dispatched with its own watermark as `min_id`, so a handler that
-- already received the event (your `init.lua`'s, which was registered before the read)
-- is skipped and a plugin's is served. A replayed handler may itself register more
-- handlers, so the pass repeats while the registry keeps growing, bounded by
-- `REPLAY_MAX_ROUNDS` like the async replay. A buffer closed since its announce is
-- dropped rather than announced under a dead id — read from the live `btv._bufs` mirror
-- each round, since a replayed handler can itself delete a buffer. Returns how many
-- handler invocations it made, for the tests.
function btv._replay_startup_announces()
  if not startup_window_open then
    return 0
  end
  startup_window_open = false
  local records = startup_announces
  startup_announces, startup_index = {}, {}
  local delivered = 0
  for _ = 1, REPLAY_MAX_ROUNDS do
    local grew = false
    for _, rec in ipairs(records) do
      local ctx, cursor = rec.ctx, rec.cursor
      local next_hw = autocmd_seq
      -- A nil mirror means "not published yet", not "no buffers": never let a missing
      -- mirror silently swallow every replay.
      local bufs = btv._bufs
      if next_hw > cursor.hw and (ctx.buf == nil or bufs == nil or bufs[ctx.buf] ~= nil) then
        au_dispatch(ctx.event, ctx.pattern, ctx.buf, function(au)
          local cb = au.opts.callback
          if type(cb) == "function" then
            -- Nothing is sequenced behind a replay, so a returned promise is tracked
            -- (its rejection still surfaces, named for the event) but not awaited.
            track_au_promise(
              cb({
                id = au.id,
                event = ctx.event,
                match = ctx.pattern,
                buf = ctx.buf,
                file = ctx.file or ctx.pattern,
                data = ctx.data,
              }),
              ctx.event
            )
            delivered = delivered + 1
          elseif type(au.opts.command) == "string" then
            vim.cmd(au.opts.command)
            delivered = delivered + 1
          end
        end, nil, cursor.hw)
        cursor.hw = next_hw
        grew = true
      end
    end
    if not grew then
      break
    end
  end
  return delivered
end

-- `btv.autocmd.pending()` -> the autocmd handler promises still in flight past their
-- settle budget, as `{ event, buf, site, elapsed_ms, budget }` entries. A handler that
-- hangs forever never settles and so never warns on completion — this is where it stays
-- visible. Inspect with `:lua print(vim.inspect(btv.autocmd.pending()))`.
function btv.autocmd.pending()
  local now = btv.now_ms()
  local out = {}
  for _, e in pairs(btv._au_pending) do
    local elapsed = now - e.started
    if elapsed > e.budget then
      out[#out + 1] = {
        event = e.event,
        buf = e.buf,
        site = unsettled_sites(e.waits),
        elapsed_ms = math.floor(elapsed),
        budget = e.budget,
      }
    end
  end
  return out
end

-- `group` is the optional trailing augroup-id filter (see `au_dispatch`); the Rust
-- fire paths pass fewer arguments, so an editor-triggered event leaves it nil and
-- reaches every subscriber.
function btv._fire(event, pattern, buf, file, data, group)
  local any = false
  local hot = HOT_EVENTS[event]
  -- Ids at or below this existed when we dispatched; anything above registered DURING
  -- this fire and never saw it. Captured before the pass, used by the replay.
  local watermark = autocmd_seq
  local waits -- nil unless a non-hot handler returned a pending promise
  au_dispatch(event, pattern, buf, function(au)
    local cb = au.opts.callback
    if type(cb) == "function" then
      local ret = cb({
        id = au.id,
        event = event,
        match = pattern,
        buf = buf,
        file = file or pattern,
        data = data,
      })
      -- A hot-path handler must be synchronous; a promise here is a contract
      -- violation, so it raises like any other handler error (aborting the rest of
      -- this fire and surfacing through `report_autocmd_err`) rather than being
      -- quietly tracked and never awaited.
      if hot then
        if btv._is_promise(ret) then
          error(hot_async_error(au, event), 0)
        end
      else
        if btv._is_promise(ret) then
          waits = waits or {}
          waits[#waits + 1] = { promise = ret, au = au }
        end
        track_au_promise(ret, event)
      end
      any = true
    elseif type(au.opts.command) == "string" then
      vim.cmd(au.opts.command)
      any = true
    end
  end, group)
  -- A first-announce event fired before the plugins are up is recorded for replay when
  -- they land (see `record_startup_announce`). Only editor-triggered fires: a manual
  -- `btv.autocmd.exec(..., { group = … })` is scoped to one group by intent, and
  -- re-delivering it to every subscriber later would break that scoping.
  local replayable = startup_window_open and group == nil and REPLAYABLE_TO_PLUGINS[event]
  -- Only a fire that actually went async pays for the settle protocol; with no pending
  -- promise this returns exactly as it always did — same tick, no timer, no bookkeeping
  -- — which is the overwhelmingly common case and every case on the hot path.
  if waits or replayable then
    local ctx =
      { event = event, pattern = pattern, buf = buf, file = file, data = data, group = group }
    -- The delivered-up-to watermark, shared by the async settle's replay rounds and the
    -- startup-window replay so neither re-runs a handler the other already served. A
    -- fire that stayed synchronous has reached every handler registered so far — even
    -- one a handler added mid-pass — so its cursor starts at the current id instead.
    local cursor = { hw = waits and watermark or autocmd_seq }
    if waits then
      arm_settle(ctx, cursor, waits, 1)
    end
    if replayable then
      record_startup_announce(ctx, cursor)
    end
  end
  return any
end

-- Fire an **awaited** event: run every matching handler like `btv._fire`, but a handler
-- may return a promise the caller must wait on before proceeding. Drives `BufWritePre`
-- (the write waits for format/trim-on-save — including *async* handlers, e.g. an LSP
-- format that resolves a tick later — to settle before the bytes serialize), each stage
-- of the gated read chain (`BufReadPost`/`BufNewFile` → `FileType`), and the `*Pre`
-- stages of the exit sequence.
-- Collects each handler's returned promise; if none are pending, returns `true` so the
-- caller commits synchronously (identical timing to the plain `btv._fire` path). Otherwise
-- waits for all of them via `btv.promise.all_settled` and, once settled, signals the
-- server through `btv._au_gate_done(gate_id)` (the parked follow-up's key), returning
-- `false` so the caller defers until that signal. `all_settled` never rejects, so a
-- handler whose promise *rejects* still lets the gated action proceed — a failing
-- formatter must not silently block saving, and a failing read handler must not leave a
-- buffer half-announced. That is a LIVENESS decision, not permission to hide the error:
-- `track_au_promise` still reports the rejection named for the event, exactly as on the
-- ungated path. (It has to be attached here explicitly — `all_settled` subscribes with a
-- rejection handler of its own, which marks the promise handled and would otherwise leave
-- even the generic unhandled-rejection reporter silent.)
-- It rides the same settle protocol as `btv._fire`, so a gated event ALSO replays to
-- handlers that registered during its async tail — and the gate is signalled only once
-- that whole fixpoint has converged. That is what lets the read chain advance knowing
-- the previous stage saw a fully settled world: when `FileType` fires, an async
-- `BufReadPost` handler has finished *and* anything it registered has run.
function btv._fire_gated(event, pattern, buf, file, gate_id, data)
  local hot = HOT_EVENTS[event]
  local watermark = autocmd_seq
  local waits -- nil until a handler returns a pending promise
  au_dispatch(event, pattern, buf, function(au)
    local cb = au.opts.callback
    local ret
    if type(cb) == "function" then
      ret = cb({
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
    if btv._is_promise(ret) then
      -- No gated event is hot today, but the guard is the contract rather than a
      -- reaction to a caller: being sequenced behind a gate must never be the loophole
      -- that lets a per-keypress event go async.
      if hot then
        error(hot_async_error(au, event), 0)
      end
      waits = waits or {}
      waits[#waits + 1] = { promise = ret, au = au }
      track_au_promise(ret, event)
    end
  end)
  -- The read chain's stages come through here, so this is where the startup file's
  -- `BufReadPost` / `FileType` get recorded for the plugins that were not loaded yet.
  local replayable = startup_window_open and REPLAYABLE_TO_PLUGINS[event]
  if not waits and not replayable then
    return true
  end
  local ctx = { event = event, pattern = pattern, buf = buf, file = file, data = data }
  -- One shared watermark per fire — see the twin in `btv._fire`.
  local cursor = { hw = waits and watermark or autocmd_seq }
  if replayable then
    record_startup_announce(ctx, cursor)
  end
  if not waits then
    return true
  end
  arm_settle(ctx, cursor, waits, 1, function()
    btv._au_gate_done(gate_id)
  end)
  return false
end

-- Fire a `*Cmd` autocmd (currently `BufReadCmd`) and return whether a handler
-- **claimed** the action. The server uses `BufReadCmd` to let a plugin own a buffer's
-- read (vim's "replace the read" hook — the file-explorer-as-plugin rides it): a
-- claimed read skips the server's default load. Unlike `btv._fire` (which reports
-- merely whether a handler *ran*), a `*Cmd` handler claims by **returning a truthy
-- value** — so a `pattern = "*"` handler can decide per path (claim a directory,
-- return nil for a regular file so the default read proceeds). `path` is the match /
-- `<afile>`; `buf` is the (empty) buffer the handler fills; `isdir` is whether `path`
-- is a directory (surfaced as `args.isdir`), the fs fact a `*Cmd` handler branches on
-- without an async re-stat — the file explorer claims directories, declines files.
function btv._fire_read_cmd(event, path, buf, isdir)
  local claimed = false
  local fired -- ids of `++once` autocmds to drop after this pass (nil = none)
  for _, au in ipairs(btv._autocmds) do
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
    btv._autocmds = vim.tbl_filter(function(au)
      return not fired[au.id]
    end, btv._autocmds)
  end
  return claimed
end

-- Fire an event **in a window's context** — the window the event is *about* is the
-- current one for the duration, which is how neovim fires the per-window events: it has
-- genuinely entered the window by the time the handler runs, so `btv.wo`, `btv.win.current()`
-- and the cursor reads all address it. `BufWinEnter` is the case that needs it: a session
-- restore fills *background* windows, and a handler placing per-window state there would
-- otherwise land it in whatever window happened to be focused.
--
-- The context is the mirror one (`btv.win.call`), not a real focus change: the editor does
-- not move the user's cursor to run a handler. That makes every *read* resolve against
-- `win`, and every explicit-handle write (`btv.wo[win]`, an extmark on a buffer) target the
-- right place. A mutation that binds to "current" only at drain time — `btv.cmd`, feedkeys —
-- raises through `btv._call_ctx_lock` while the two differ, rather than silently landing in
-- the focused window; when they're the same (everything the user types), nothing is locked
-- and this is exactly the plain fire.
--
-- The context covers the handler's SYNCHRONOUS run — it is a mirror swap around one call,
-- and the mirror is rebuilt by the server every tick. A handler that returns a promise
-- resumes, past the await, in the ordinary context: `btv.win.current()` is the focused
-- window again there, so an async tail must capture the window *before* its first await
-- and write through the explicit handle (`btv.wo[win]`). Documented in
-- `docs/autocmd-events.md` under `BufWinEnter`; nothing can retarget a continuation that
-- runs after the fire has returned.
function btv._fire_in_win(win, event, pattern, buf, file)
  return btv.win.call(win, function()
    -- Name the scope in the lock, so a handler that trips it is told about the *event*
    -- it is in rather than about an `btv.win.call` its author never wrote.
    if btv._call_ctx_lock == true then
      btv._call_ctx_lock = ("the %s fire for window %d"):format(event, win)
    end
    return btv._fire(event, pattern, buf, file)
  end)
end

-- The `FileChangedShell` round-trip the server's file-change reconcile drives
-- (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → the watch leg). Set
-- `v:fcs_reason` to `reason` and reset `v:fcs_choice` to `""` (neovim's defaults
-- before the autocmd), fire `FileChangedShell` for `buf`/`file`, and return whether
-- any handler ran. A handler reads `vim.v.fcs_reason` and may set `vim.v.fcs_choice`
-- to `"reload"`/`"edit"`/`"ask"` to redirect the reconcile; the server reads it back via
-- `btv._fcs_choice`.
function btv._fire_file_changed(reason, buf, file)
  btv._v_mirror.fcs_reason = reason
  btv._v_mirror.fcs_choice = ""
  return btv._fire("FileChangedShell", file, buf, file)
end

-- Read the `v:fcs_choice` a `FileChangedShell` handler set (or `""` if none did) —
-- the second half of the round-trip above.
function btv._fcs_choice()
  return btv._v_mirror.fcs_choice or ""
end

-- Fire `DirChanged` after a `:cd` / `:chdir` changed the working directory (the
-- server calls this through `LuaRuntime::fire_dir_changed`). Set `v:event` to
-- neovim's `{ cwd, scope, changed_window }` payload before firing — a handler
-- reading `vim.v.event.cwd` (project / session plugins) sees it — and pass the
-- same table as `args.data`. The autocmd pattern matches `scope` (`"global"` for
-- `:cd`); `<afile>` (`args.file`) is the new directory.
function btv._fire_dir_changed(scope, cwd)
  local event = { cwd = cwd, scope = scope, changed_window = false }
  btv._v_mirror.event = event
  btv._fire("DirChanged", scope, nil, cwd, event)
end

-- `btv.autocmd.exec(event, opts)` [alias `nvim_exec_autocmds`]: fire `event` (or a
-- list of events) manually. `opts.pattern` (string or list) is matched as in
-- registration; `opts.buffer` supplies the buffer context (defaulting to the
-- current snapshot buffer), and the callback's `args.file` is the snapshot name
-- when firing for it. `opts.data` is an arbitrary payload delivered to each handler
-- as `args.data` (e.g. `btv.autocmd.exec("User", { pattern = "MyEvent", data = … })`).
--
-- `opts.group` (an augroup id or name) narrows the fire to THAT group's handlers.
-- Reach for it when a plugin needs to re-run its own handlers for a buffer — a
-- late-loading plugin catching up on a `FileType` that already fired, say — without
-- re-broadcasting the event to every other subscriber (the LSP dispatcher,
-- treesitter, the user's own autocmds), which would make them redo work they have
-- already correctly done. An unknown group name fails loud.
function btv.autocmd.exec(event, opts)
  opts = opts or {}
  local events = au_canon_event(type(event) == "table" and event or { event })
  local buf = opts.buffer
  if buf == nil then
    buf = btv._cur_buf and btv._cur_buf.bufnr or nil
  end
  local file
  if btv._cur_buf and buf == btv._cur_buf.bufnr then
    file = btv._cur_buf.name
  end
  local group = au_resolve_group(opts.group, "nvim_exec_autocmds")
  local patterns = opts.pattern
  local data = opts.data
  for _, ev in ipairs(events) do
    if type(patterns) == "table" then
      for _, p in ipairs(patterns) do
        btv._fire(ev, p, buf, file, data, group)
      end
    else
      btv._fire(ev, patterns, buf, file, data, group)
    end
  end
end

-- `btv.autocmd.get(opts)` [alias `nvim_get_autocmds`]: introspect the registered
-- autocmds — a debugging affordance for confirming what `clear`/`del` left
-- behind. Returns a list of
-- `{id, event, group, group_name, pattern, buffer, command, site}` entries — `site`
-- being the `"src:line"` the autocmd was registered at — optionally
-- filtered by `opts.event` (string or list) and `opts.group` (id or name). Run it
-- interactively as
-- `:lua print(vim.inspect(btv.autocmd.get({})))`.
function btv.autocmd.get(opts)
  opts = opts or {}
  local want_events = opts.event
    and au_canon_event(type(opts.event) == "table" and opts.event or { opts.event })
  local want_group = au_resolve_group(opts.group, "nvim_get_autocmds")
  -- reverse map: group id → its registered name, for human-readable output
  local group_name = {}
  for nm, id in pairs(btv._augroups) do
    group_name[id] = nm
  end
  local out = {}
  for _, au in ipairs(btv._autocmds) do
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
        -- `"src:line"` of the registration (nil when it came from a C frame). The
        -- handle for "which of my handlers is the slow one" — see `capture_site`.
        site = au.site,
      }
    end
  end
  return out
end

-- ----- :autocmd / :augroup / :doautocmd ex-commands --------------------------
-- The Vimscript front-end onto the autocmd registry above. The core ex-command
-- dispatch doesn't recognize these, so it defers them to the server, which parses
-- the argument line here and drives the same btv._autocmds / btv._augroups store
-- the nvim_* API uses — one store, two front-ends. Each `btv._ex_*` returns the
-- text the server surfaces: "" (nothing), a one-line message/error (echoed), or a
-- multi-line listing (shown in a panel).

-- The "current augroup" set by `:augroup {name}` and cleared by `:augroup END`.
-- It persists across command invocations, exactly like Vim's parser state, so a
-- block of `:autocmd`s between the two lands in that group.
btv._cur_augroup = nil

-- Does `au` match the group / event-list / pattern-list filter? A nil filter
-- field means "any" (so a bare `:autocmd!` clears everything in scope). Events
-- and patterns are lists; a `"*"` event matches any event. A pattern-less autocmd
-- is treated as `"*"` for matching, mirroring `btv._fire`'s pattern rule.
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
  for nm, id in pairs(btv._augroups) do
    gname[id] = nm
  end
  local lines = { "--- Autocommands ---" }
  for _, au in ipairs(btv._autocmds) do
    if au_matches(au, group, events, patterns) then
      local evs = type(au.event) == "table" and table.concat(au.event, ",") or tostring(au.event)
      local pat = au.opts.pattern
      pat = type(pat) == "table" and table.concat(pat, ",") or (pat or "*")
      local g = au.group and (gname[au.group] or ("group#" .. au.group)) or ""
      -- A callback has no source text to show, so name where it was registered
      -- instead — the listing's whole job is telling you *which* handler is which,
      -- and `<callback>` repeated N times does not. (vim's answer to the same
      -- problem is `:verbose autocmd`'s "Last set from …".)
      local body = au.opts.command or (au.site and ("<callback> " .. au.site) or "<callback>")
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
function btv._ex_augroup(bang, args)
  args = vim.trim(args)
  if bang then
    if args == "" then
      return "E471: Argument required"
    end
    local id = btv._augroups[args]
    if id then
      btv._autocmds = vim.tbl_filter(function(au)
        return au.group ~= id
      end, btv._autocmds)
      btv._au_touch()
      btv._augroups[args] = nil
      if btv._cur_augroup == args then
        btv._cur_augroup = nil
      end
    end
    return ""
  end
  if args == "" then
    return btv._cur_augroup and ("augroup " .. btv._cur_augroup) or ""
  end
  if args == "END" or args == "end" then
    btv._cur_augroup = nil
    return ""
  end
  btv.augroup.create(args, { clear = false })
  btv._cur_augroup = args
  return ""
end

-- :au[tocmd][!] [group] [event[,event…]] [pat[,pat…]] [++once] [++nested] [cmd]
-- A leading word that names an existing augroup is the group; otherwise the
-- current `:augroup` (if any) applies. With a bang, the autocmds matching the
-- group/event/pattern filter are removed first; with a trailing command, a new
-- autocmd is then registered. With no command and no bang it lists the matching
-- autocmds. `<buffer>` as the pattern registers a buffer-local autocmd for the
-- current buffer. `++once` fires once then self-removes (honored by `btv._fire`);
-- `++nested` is accepted (bemtvi already lets events nest).
function btv._ex_autocmd(bang, args)
  local rest = vim.trim(args)

  -- Optional leading group: only when the first word names an existing augroup.
  local group = btv._cur_augroup and btv._augroups[btv._cur_augroup] or nil
  local first = rest:match("^(%S+)")
  if first and btv._augroups[first] then
    group = btv._augroups[first]
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
    btv._autocmds = vim.tbl_filter(function(au)
      return not au_matches(au, group, events, patterns)
    end, btv._autocmds)
    btv._au_touch()
  end

  if cmd ~= "" then
    if not events then
      return "E216: No such event: a {event} is required to define an autocmd"
    end
    btv.autocmd.create(#events == 1 and events[1] or events, {
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

-- :doau[tocmd] [group] {event} [pattern]: fire `event` now (optionally for a
-- pattern), the manual analogue of `nvim_exec_autocmds`. The optional leading
-- [group] narrows the fire to one augroup's autocmds, exactly as `opts.group` does
-- on the API. Disambiguated the way vim does it: the first word is the group only
-- when it NAMES a live augroup *and* another word follows it — so `:doautocmd User
-- Marker` still reads `User` as the event, and only a real group name shifts it.
function btv._ex_doautocmd(args)
  args = vim.trim(args):gsub("^<nomodeline>%s*", "")
  local event, rest = take_word(args)
  if event ~= "" and rest ~= "" and btv._augroups[event] ~= nil then
    local group = event
    event, rest = take_word(rest)
    if event == "" then
      return "E217: Can't execute autocommands for ALL events"
    end
    btv.autocmd.exec(event, { pattern = rest ~= "" and rest or nil, group = group })
    return ""
  end
  if event == "" then
    return "E217: Can't execute autocommands for ALL events"
  end
  local pattern = rest ~= "" and rest or nil
  btv.autocmd.exec(event, { pattern = pattern })
  return ""
end

-- :com[mand][!] [attrs] {Name} {replacement} — define a user command. The
-- replacement is a verbatim ex-command template, run on invocation with the
-- common `<…>` escapes expanded against that call's args. It registers into the
-- same `btv._user_commands` (or, with `-buffer`, the current buffer's local) store
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
function btv._ex_command(bang, args, bufnr)
  local s = vim.trim(args or "")
  if s == "" then
    -- Bare `:command`: list the defined command names (global + this buffer's
    -- locals), one per line. Minimal but real — not a silent no-op.
    local names = {}
    for name in pairs(btv._user_commands) do
      names[#names + 1] = name
    end
    local locals = (btv._buf_user_commands or {})[bufnr or 0]
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
  local store = btv._user_commands
  if buffer_local then
    if bufnr == nil or bufnr == 0 then
      bufnr = btv._cur_buf and btv._cur_buf.bufnr or 0
    end
    btv._buf_user_commands[bufnr] = btv._buf_user_commands[bufnr] or {}
    store = btv._buf_user_commands[bufnr]
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
-- in *this* Lua layer (the btv._ex_* drivers above), keyed by every abbreviation
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
-- bemtvi can only *capture* output from the command families whose listing/report
-- text is generated synchronously in Lua (the autocmd group). Any other command is
-- still run, via the normal queued `vim.cmd` path, but its message-line output is
-- asynchronous and cannot be read back here. So requesting `output` capture of a
-- non-capturable command FAILS LOUD rather than returning a misleading `""` — a stub
-- that faked an empty capture would make a caller's `:find` on the result silently
-- wrong, exactly the "quietly succeeds" failure bemtvi forbids.
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
        text = btv._ex_autocmd(bang, rest)
      elseif kind == "aug" then
        text = btv._ex_augroup(bang, rest)
      elseif kind == "doau" then
        text = btv._ex_doautocmd(rest)
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

-- `btv.exec(src, output)` [alias `nvim_exec`]: run the ex-command(s) in `src` and,
-- when `output` is truthy, return the text they produced (see `exec_capture`).
function btv.exec(src, output)
  return exec_capture(src, output)
end

-- `nvim_exec2(src, opts)`: the 0.9+ neovim-shaped wrapper around `nvim_exec` — same
-- execution, but the captured text is returned under `.output` (only when
-- `opts.output` is set). A `vim.api`-only compat shim with no distinct btv twin
-- (the canonical bemtvi form is `btv.exec`); its body only wraps the sibling nvim_
-- function, so it carries no implementation of its own.
function vim.api.nvim_exec2(src, opts)
  opts = opts or {}
  local out = vim.api.nvim_exec(src, opts.output)
  return opts.output and { output = out } or {}
end

-- ----- vim.cmd: callable AND indexable ---------------------------------------
-- `vim.cmd("…")` queues a raw ex-command (the Rust function installed earlier);
-- `vim.cmd.colorscheme("x")` / `vim.cmd.set("number")` build `"<name> <args…>"`.
-- Every form also takes the modifier table `btv.cmd` accepts —
-- `vim.cmd("write", { silent = true })`, `vim.cmd.write({ mods = { silent = true } })`
-- — and the neovim structured form `vim.cmd{ cmd = …, args = …, mods = … }` routes
-- through `nvim_cmd`, so `mods.silent` reaches the same `:silent` modifier instead
-- of being dropped.
do
  local raw_cmd = vim.cmd
  -- An <expr> mapping RHS must not change editor state (textlock): while
  -- btv._expr_lock is set, running an ex-command raises instead of mutating.
  local function raw(c, opts)
    if btv._expr_lock then
      error("E5555: <expr> mapping must not change the editor (vim.cmd is blocked)", 0)
    end
    btv._assert_call_ctx("an ex-command (vim.cmd)")
    return raw_cmd(c, opts)
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
      return raw(s, first.mods)
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
    __call = function(_, c, opts)
      -- The structured neovim form (`{ cmd = "echo", args = {…}, mods = {…} }`)
      -- flattens through the shared `nvim_cmd` adapter rather than a second copy
      -- of that shape here.
      if type(c) == "table" then
        return vim.api.nvim_cmd(c, opts or {})
      end
      return raw(c, opts)
    end,
    __index = function(_, name)
      return function(...)
        return build(name, ...)
      end
    end,
  })
end

vim.api.nvim_create_user_command = btv.user_command.create
vim.api.nvim_buf_create_user_command = btv.user_command.buf_create
vim.api.nvim_create_augroup = btv.augroup.create
vim.api.nvim_create_autocmd = btv.autocmd.create
vim.api.nvim_del_autocmd = btv.autocmd.del
vim.api.nvim_exec_autocmds = btv.autocmd.exec
vim.api.nvim_get_autocmds = btv.autocmd.get
vim.api.nvim_exec = btv.exec

-- `btv.autocmd.clear(opts)` [alias `nvim_clear_autocmds`]: remove every autocmd
-- matching the filter — the bulk analogue of `btv.autocmd.del`. `opts.event`
-- (string/list), `opts.group` (id or name), `opts.buffer`, and `opts.pattern`
-- (string/list) all narrow the set; an empty opts clears everything. Mirrors
-- `btv.autocmd.get`'s matching.
function btv.autocmd.clear(opts)
  opts = opts or {}
  local want_events = opts.event
    and au_canon_event(type(opts.event) == "table" and opts.event or { opts.event })
  local want_group = au_resolve_group(opts.group, "nvim_clear_autocmds")
  local want_pats = opts.pattern
    and (type(opts.pattern) == "table" and opts.pattern or { opts.pattern })
  btv._autocmds = vim.tbl_filter(function(au)
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
  end, btv._autocmds)
  btv._au_touch()
end
api.nvim_clear_autocmds = btv.autocmd.clear

-- `btv.user_command.get(opts)` / `btv.user_command.buf_get(buf, opts)` [aliases
-- `nvim_get_commands` / `nvim_buf_get_commands`]: the user-command registry as
-- neovim's introspection map (name -> definition record). bemtvi's registry stores
-- only the command body, so the record carries `name`/`definition` with permissive
-- defaults for the rest — enough for a command picker to list and run
-- them. `btv.user_command.get` returns the globals; `btv.user_command.buf_get(buf)`
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
function btv.user_command.get(_opts)
  return commands_map(
    btv._user_commands,
    btv._user_command_desc,
    btv._user_command_complete,
    btv._user_command_usage
  )
end
function btv.user_command.buf_get(buf, _opts)
  if buf == nil or buf == 0 then
    buf = btv._cur_buf and btv._cur_buf.bufnr or 0
  end
  return commands_map(
    (btv._buf_user_commands or {})[buf],
    (btv._buf_user_command_desc or {})[buf],
    (btv._buf_user_command_complete or {})[buf],
    (btv._buf_user_command_usage or {})[buf]
  )
end
api.nvim_get_commands = btv.user_command.get
api.nvim_buf_get_commands = btv.user_command.buf_get

-- `btv._remote_ts_autoinstall(langs)`: in an edit-host (daemon) session, lazily install the
-- tree-sitter parsers the remote daemon had — the first time a buffer of one of those
-- filetypes opens. `langs` is the list the server hands over (already filtered to parsers
-- NOT installed on this client). It registers a `FileType` autocmd that `:TSInstall`s the
-- buffer's filetype on first sight (deduped per session). Parsers are native + compiled
-- locally, so this mirrors the remote's language set without fetching its wrong-arch
-- binaries. Dogfoods the public `FileType` + `:TSInstall` surface — the server only supplies
-- the language list.
function btv._remote_ts_autoinstall(langs)
  local want = {}
  for _, lang in ipairs(langs or {}) do
    want[lang] = true
  end
  if next(want) == nil then
    return
  end
  local requested = {}
  btv.autocmd.create("FileType", {
    callback = function(ev)
      local ft = ev.match
      if ft and ft ~= "" and want[ft] and not requested[ft] then
        requested[ft] = true
        vim.cmd("TSInstall " .. ft)
      end
    end,
  })
end
