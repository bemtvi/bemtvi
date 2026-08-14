-- bemtvi Lua prelude — editor state.
-- The Rust↔Lua mirror state the server refreshes each chunk (buffers / windows /
-- tabs / extmarks / highlights / registers) and their setters; the shared
-- resolvers (`btv._resolve_bufnr` / `btv._resolve_win` / `btv._norm_line_index`) and the
-- `nvim_buf_call/win_call` context lock; and the scalar surfaces plugins read and
-- write: variables (`btv.g/b/w/v` + `vim.env`), options (`btv.o/bo/wo/go/opt` + the
-- `btv.option` by-name funnel), and registers (`btv.reg`). Loaded right after runtime,
-- before the entity API that reads this state.
local vim = vim
local api = vim.api
btv = btv or {}
btv.option = btv.option or {}

-- ----- option / variable stores ---------------------------------------------

-- `btv.g`: global variables. Plain storage; reading an unset key yields nil.
btv.g = btv.g or {}
vim.g = btv.g

-- `vim.w` / `vim.b`: window- and buffer-scoped variables. In neovim each is indexed
-- first by a window/buffer handle (`vim.w[win].name`) and bare access targets the
-- *current* window/buffer (`vim.w.name`). bemtvi backs them with a per-handle Lua
-- store rather than a core var dict — enough for plugins that stash a marker on a
-- window/buffer and read it back (trouble.nvim tags its own windows with
-- `vim.w[win].trouble` and skips them when picking a target window; a missing
-- `vim.w` made that an index-of-nil at setup). `vim.w[0]` / `vim.b[0]` resolve to
-- the current handle, like the rest of the API.
local function scoped_vars(store, current)
  return setmetatable({}, {
    __index = function(_, k)
      if type(k) == "number" then
        local h = (k == 0) and current() or k
        local t = store[h]
        if not t then
          t = {}
          store[h] = t
        end
        return t
      end
      -- bare `vim.w.name`: the current handle's var.
      local t = store[current()]
      return t and t[k]
    end,
    __newindex = function(_, k, v)
      if type(k) == "number" then
        error("vim.w/vim.b: assign fields on vim.w[handle], not the handle itself", 2)
      end
      local h = current()
      local t = store[h]
      if not t then
        t = {}
        store[h] = t
      end
      t[k] = v
    end,
  })
end
btv._w_vars = btv._w_vars or {}
btv._b_vars = btv._b_vars or {}
btv.w = scoped_vars(btv._w_vars, function()
  return vim.api.nvim_get_current_win()
end)
btv.b = scoped_vars(btv._b_vars, function()
  return vim.api.nvim_get_current_buf()
end)
vim.w = btv.w
vim.b = btv.b

-- `btv.o`: editor options with neovim's set-semantics — a write reaches the
-- option's real home and a read returns the core's current value (the default
-- until set, and a value set through the `:set` ex path, not just one written
-- from Lua). The wired options route to the scope their name implies:
--   * number / relativenumber       -> window-local (delegated to `btv.wo`)
--   * tabstop / shiftwidth /
--     softtabstop / expandtab       -> buffer-local (delegated to `btv.bo`)
--   * ignorecase / smartcase /
--     wrapscan / hlsearch /
--     incsearch / showtabline       -> global (`btv._go_mirror` + the
--                                      `btv._set_global_option` Rust bridge)
-- Any other option (termguicolors, background, winblend, pumblend, …) lands in
-- the plain Lua store `btv._o_store`: observable read/write, not yet honored.
--
-- `btv.wo` / `btv.bo` are defined later in this chunk; `btv.o` only touches them from
-- inside its metamethods, which run at config time once every chunk has loaded,
-- so the forward reference is fine.

-- Window- and buffer-local options `vim.o` forwards to `vim.wo` / `vim.bo`, keyed by
-- both the full name and its abbreviation (the delegate canonicalizes again).
--
-- DERIVED, not hand-kept: the server injects core's option catalog
-- (`btv._options_catalog`, built from `bemtvi_core::options::options_catalog()` — the same
-- list `:set` resolves against) before any config runs, and `btv._set_options_catalog`
-- below fills these from its `scope` column. They used to be hand-written name lists and
-- drifted: `vim.opt.foldmethod = "marker"` (and nine more) fell into the unmodeled
-- `btv._o_store` and silently did nothing, while `:set foldmethod=marker` worked.
local O_WIN = {}
local O_BUF = {}

-- Global (editor-wide) options: canonical name keyed by name and abbreviation.
local O_GLOBAL = {
  ignorecase = "ignorecase",
  ic = "ignorecase",
  smartcase = "smartcase",
  scs = "smartcase",
  wrapscan = "wrapscan",
  ws = "wrapscan",
  hlsearch = "hlsearch",
  hls = "hlsearch",
  incsearch = "incsearch",
  is = "incsearch",
  autoread = "autoread",
  ar = "autoread",
  imagepreview = "imagepreview",
  imgp = "imagepreview",
  httphost = "httphost",
  httpport = "httpport",
  showtabline = "showtabline",
  stal = "showtabline",
  laststatus = "laststatus",
  ls = "laststatus",
  pummaxwidth = "pummaxwidth",
  pmw = "pummaxwidth",
  statusline = "statusline",
  stl = "statusline",
  tabline = "tabline",
  tal = "tabline",
  guifont = "guifont",
  gfn = "guifont",
  regexsyntax = "regexsyntax",
  rxs = "regexsyntax",
  fileencodings = "fileencodings",
  fencs = "fileencodings",
  errorformat = "errorformat",
  efm = "errorformat",
  switchbuf = "switchbuf",
  swb = "switchbuf",
  makeprg = "makeprg",
  mp = "makeprg",
  grepprg = "grepprg",
  gp = "grepprg",
  grepformat = "grepformat",
  gfm = "grepformat",
  qfdock = "qfdock",
  qfd = "qfdock",
  bdclosetab = "bdclosetab",
  bdct = "bdclosetab",
  relativesplits = "relativesplits",
  relativedocks = "relativedocks",
  equalalways = "equalalways",
  ea = "equalalways",
  workspacepersistunnamed = "workspacepersistunnamed",
  scrollanim = "scrollanim",
  sca = "scrollanim",
  scrollanimduration = "scrollanimduration",
  scad = "scrollanimduration",
  scrollback = "scrollback",
  scbk = "scrollback",
  history = "history",
  hi = "history",
  persisthistory = "persisthistory",
  phisto = "persisthistory",
  timeout = "timeout",
  to = "timeout",
  timeoutlen = "timeoutlen",
  tm = "timeoutlen",
  -- The editor screen extent (the server pushes the live size into the mirror);
  -- read-mostly here — a float-positioning plugin reads them to
  -- center its windows, and `:set columns=` is not honored (the client owns the
  -- terminal size), but a write still lands in the mirror so a read-back agrees.
  columns = "columns",
  co = "columns",
  lines = "lines",
}
-- Core defaults, the safety net before the server has pushed the mirror.
local O_GLOBAL_DEFAULT = {
  ignorecase = false,
  smartcase = false,
  wrapscan = true,
  hlsearch = true,
  incsearch = true,
  autoread = true,
  imagepreview = false,
  httphost = "127.0.0.1",
  httpport = 0,
  showtabline = 1,
  laststatus = 2,
  pummaxwidth = 50,
  statusline = "",
  tabline = "",
  guifont = "",
  regexsyntax = "pcre",
  fileencodings = "ucs-bom,utf-8,latin1",
  errorformat = "",
  switchbuf = "",
  makeprg = "make",
  grepprg = "grep -n $* /dev/null",
  grepformat = "%f:%l:%c:%m,%f:%l:%m,%f:%l%m,%f %l%m",
  qfdock = true,
  bdclosetab = true,
  relativesplits = true,
  relativedocks = false,
  equalalways = true,
  workspacepersistunnamed = true,
  scrollanim = true,
  scrollanimduration = 160,
  scrollback = 10000,
  history = 10000,
  persisthistory = "workspace,global",
  timeout = true,
  timeoutlen = 1000,
  columns = 80,
  lines = 24,
}

-- Rust→Lua mirror of the core's global option values, refreshed by the server
-- (`btv._set_go_mirror`) before any Lua that can read options. Authoritative for
-- the wired global options, so a read reflects the core default until set and a
-- value set through the `:set` ex path, not just one written from Lua.
btv._go_mirror = btv._go_mirror or {}
function btv._set_go_mirror(t)
  btv._go_mirror = t or {}
end

-- Rust→Lua mirror of the keyboard-macro state, refreshed by the server each tick:
-- `recording` is the register `<F2>{reg}` is recording into, `executing` the one a
-- `<F3>{reg}` playback is running, each `nil` when idle. Backs `btv.macro.recording`
-- / `btv.macro.executing` (and their `vim.fn.reg_recording` / `reg_executing`
-- aliases), which is how a statusline or a plugin asks "is a macro in flight".
btv._macro_state = btv._macro_state or {}
function btv._set_macro_state(t)
  btv._macro_state = t or {}
end

-- Rust→Lua mirror of the core register file, refreshed by the server
-- (`btv._set_reg_mirror`) before any Lua that can read registers. Keyed by the
-- single-char register name -> { text, type } where type is `"v"` (charwise) or
-- `"V"` (linewise). Backs `vim.fn.getreg` / getregtype; `vim.fn.setreg` write-through
-- mutates it directly so a read-after-write within one chunk stays consistent
-- (core catches up when the server drains the queued RegisterSetOp).
btv._registers = btv._registers or {}
function btv._set_reg_mirror(t)
  btv._registers = t or {}
end

-- The read-only special registers (`%` `/` `:` `.`), refreshed **in place** on the
-- table `btv._set_reg_mirror` installed. They are resolved from live editor state
-- (buffer name, last search, last command line, the insert in progress) rather than
-- from the stored register file, so they move on ticks where no register was
-- written -- `.` on literally every keystroke of an insert. Re-pushing the whole
-- mirror for them would re-copy every stored register's text along with them, which
-- is exactly the O(stored bytes) per keystroke this split exists to avoid. A special
-- that no longer resolves is cleared (nil), not left stale.
local REG_SPECIALS = { "%", "/", ":", "." }
function btv._set_reg_specials(t)
  t = t or {}
  for _, name in ipairs(REG_SPECIALS) do
    btv._registers[name] = t[name]
  end
end

-- Rust→Lua mirror of the set marks (the current buffer's locals, the globals, and
-- the numbered marks), refreshed by the server (`btv._set_marks_mirror`) before any
-- Lua that can read it — so `btv.mark.list` sees positions that shift with edits and
-- restore on undo. Each row is `{ name, bufnr, line, col, path, text }` with 0-based
-- `line`/`col`. Reads only (marks are set with `m{x}` / the shada), like `btv._bufs`.
btv._marks = btv._marks or {}
function btv._set_marks_mirror(list)
  btv._marks = list or {}
end

-- Rust→Lua mirror of the core quickfix list, refreshed by the server
-- (`btv._set_qflist_mirror`) before any Lua that can read it. `btv._qflist` is the
-- array of entry dicts in list order; `btv._qflist_title` the list title. Backs
-- `vim.fn.getqflist`; `vim.fn.setqflist` queues a server-side op (no write-through —
-- the parsed result only exists once the server drains the QfSetOp).
btv._qflist = btv._qflist or {}
btv._qflist_title = btv._qflist_title or ""
function btv._set_qflist_mirror(items, title)
  btv._qflist = items or {}
  btv._qflist_title = title or ""
end

-- Per-window location-list mirror, the location-list twin of `btv._qflist`:
-- `btv._loclist[winid] = { items = {…}, title = "…" }` for every window that has a
-- location list. The server replaces the whole table each tick (so a window that
-- lost its loclist drops out). Backs `vim.fn.getloclist`.
btv._loclist = btv._loclist or {}
function btv._set_loclist_mirror(win, items, title)
  btv._loclist[win] = { items = items or {}, title = title or "" }
end
function btv._clear_loclist_mirror()
  btv._loclist = {}
end

-- Arbitrary (Lua-only) global options plugins set via `vim.o`; the wired options
-- live in their scope (`vim.wo` / `vim.bo` / `btv._go_mirror`) instead. Seeded with
-- the few defaults colorschemes read (termguicolors / background / *blend).
btv._o_store = btv._o_store
  or {
    background = "dark",
    termguicolors = false,
    winblend = 0,
    pumblend = 0,
    -- Read-mostly editor options plugins read to lay out
    -- floats and gate behavior. Observable defaults matching neovim's; not yet
    -- honored by the core (the client owns the cmdline / message regions), but a
    -- read returns a sane value instead of nil (which a `- cmdheight` arithmetic or
    -- a `.. report` concat would choke on).
    cmdheight = 1,
    report = 2,
    eventignore = "",
    ambiwidth = "single",
    helplang = "en",
    mouse = "",
    guicursor = "",
    shell = os.getenv("SHELL") or "/bin/sh",
    -- On by default in vim/neovim. Plugin managers gate their own startup on it
    -- (some bail out of setup() entirely when `not vim.go.loadplugins`), so a
    -- nil default would silently abort them before they ever run.
    loadplugins = true,
  }

-- The seeded store keys are options bemtvi *does* model (read-mostly: it keeps a
-- value but the core doesn't act on it). Writing one is expected — colorschemes
-- set `background` / `termguicolors`, plugins set `winblend` — so it must store
-- silently, unlike a genuinely-unknown name (a typo) which warns once. Snapshot
-- the seed keys now, before any catch-all write grows the store.
btv._o_known = btv._o_known
  or (function()
    local known = {}
    for k in pairs(btv._o_store) do
      known[k] = true
    end
    -- The runtimepath family is modeled by the `btv.opt` machinery (`OPT_RTP`): a
    -- `vim.opt.rtp:append(...)` write is expected and seeds package.path, so it must
    -- store silently like the seeds — the "unknown option 'rtp'" warning would tell a
    -- user the exactly-documented pattern is a typo.
    for _, k in ipairs({ "runtimepath", "rtp", "packpath", "pp" }) do
      known[k] = true
    end
    return known
  end)()

-- A write of an option bemtvi doesn't model (any scope) lands in the read/write
-- catch-all `btv._o_store` — kept rather than rejected, so a neovim config or
-- colorscheme that sets an option bemtvi hasn't implemented (termguicolors,
-- background, signcolumn, …) still loads instead of aborting at that line. But
-- warn (once per name) so a genuine typo — `vim.o.numbr = true` — is *visible*
-- rather than silently swallowed (and silently read back). `:set` rejects the same
-- name outright with E518; `btv.o`/`vim.o`/`btv.opt`/`btv.go` are the lenient compat
-- surface, so they warn-and-store instead of failing.
local warned_unknown_opt = {}
local function warn_unknown_opt(name)
  if warned_unknown_opt[name] then
    return
  end
  warned_unknown_opt[name] = true
  btv.notify(
    "bemtvi: unknown option '"
      .. tostring(name)
      .. "' — stored but not applied (a typo, or an option bemtvi doesn't model)",
    btv.log.levels.WARN
  )
end

-- A `vim.go` / `vim.opt_global` write of an option bemtvi models perfectly well but which
-- has NO global value — the four buffer slots the read decides (`fileencoding`, `bomb`,
-- `fileformat`, `endofline`), the per-buffer marker `modifiable`, and the two nouns
-- derived per buffer (`filetype`, `ts_highlight`). The ex twin answers `E5100: {opt} has
-- no global value`; this surface stays lenient (a warning, not an error, so a config
-- carries on) but must name the SAME reason. Falling into `warn_unknown_opt` said "a
-- typo, or an option bemtvi doesn't model" — false on both counts, and it sent the reader
-- hunting for a misspelling. The write is rejected rather than stored: `btv._o_store` is
-- read back, and a value nothing honors reading back as if it took is the silent-stub
-- failure this codebase forbids.
local warned_no_tier = {}
local function warn_no_global_value(name)
  if warned_no_tier[name] then
    return
  end
  warned_no_tier[name] = true
  btv.notify(
    "bemtvi: '"
      .. tostring(name)
      .. "' has no global value — the vim.go/vim.opt_global write was ignored; it is "
      .. "decided per buffer (use vim.bo / vim.opt_local). `:setglobal` answers E5100.",
    btv.log.levels.WARN
  )
end

local function o_get(k)
  if O_WIN[k] then
    return vim.wo[k]
  end
  if O_BUF[k] then
    return vim.bo[k]
  end
  local canon = O_GLOBAL[k]
  if canon then
    local v = btv._go_mirror[canon]
    if v ~= nil then
      return v
    end
    return O_GLOBAL_DEFAULT[canon]
  end
  return btv._o_store[k]
end
--- Which scope `btv.o` / `btv.opt` routes option `k` to: `"window"`, `"buffer"`,
--- `"global"`, or `nil` for a name that falls into the unmodeled `btv._o_store`
--- catch-all. Introspection only — the guard test that walks the option catalog
--- asserts every buffer/window option is routed rather than silently stored.
function btv._o_route(k)
  if O_GLOBAL[k] then
    return "global"
  end
  if O_WIN[k] then
    return "window"
  end
  if O_BUF[k] then
    return "buffer"
  end
  return nil
end

local function o_set(k, v)
  if O_WIN[k] then
    vim.wo[k] = v
    -- …and the option's GLOBAL value, as `:set` does: the tier `:setglobal` reads and
    -- the one a window minted with no source window to copy (a dock, the quickfix tab)
    -- is born from. An ordinary split still copies the window it came from, so the
    -- config carries into new splits either way.
    btv._wo_global_set(k, v)
    return
  end
  if O_BUF[k] then
    vim.bo[k] = v
    -- …and the option's GLOBAL value, the tier a newly created buffer is born from —
    -- what `:set` does in vim, and what makes `vim.opt.tabstop = 3` in an `init.lua`
    -- reach files opened later instead of only the buffer that was current while the
    -- config ran. `vim.bo` / `vim.opt_local` are the local-only surfaces. A name whose
    -- value the read decides (`fileencoding`, `bomb`, `fileformat`, `endofline`,
    -- `modifiable`) has no tier and the call is a no-op there.
    btv._bo_global_set(k, v)
    return
  end
  local canon = O_GLOBAL[k]
  if canon then
    -- Queue the change for the core and write through the mirror so a
    -- read-after-write within this chunk is consistent (the server overwrites it
    -- on the next push).
    btv._set_global_option(canon, v)
    btv._go_mirror[canon] = v
    return
  end
  -- A seeded read-mostly option (background, termguicolors, winblend, …) is
  -- modeled — store it silently. Warn only for a name bemtvi has never seen.
  if not btv._o_known[k] then
    warn_unknown_opt(k)
  end
  btv._o_store[k] = v
end

btv.o = setmetatable({}, {
  __index = function(_, k)
    return o_get(k)
  end,
  __newindex = function(_, k, v)
    o_set(k, v)
  end,
})
vim.o = btv.o

-- Rust→Lua mirror of the core's per-workspace option OVERRIDES (`btv.wso`), refreshed by
-- the server (`btv._set_wso_mirror`) each frame: canonical global-option name -> the
-- workspace's overriding value currently in effect (including overrides restored from
-- the workspace shada). Empty outside a --workspace session / before any override.
btv._wso_mirror = btv._wso_mirror or {}
function btv._set_wso_mirror(t)
  btv._wso_mirror = t or {}
end

-- `btv.wso` — per-workspace option overrides that take PRECEDENCE over the global value
-- (the value `btv.o` reads). Only GLOBAL options can be overridden — window/buffer
-- options are per-instance, with no global tier to sit above. `btv.wso.foo = v` sets the
-- override (so `btv.o.foo` then reads `v` while the workspace is open); `btv.wso.foo = nil`
-- clears it (back to the global value); `btv.wso.foo` reads the override, or nil when none.
-- Overrides persist in the workspace shada and are re-applied at the next launch.
local function wso_canon(k)
  local canon = O_GLOBAL[k]
  if not canon then
    error(
      "btv.wso: '"
        .. tostring(k)
        .. "' is not a global option — only global options take a workspace override "
        .. "(window/buffer options are per-instance)",
      2
    )
  end
  return canon
end
btv.wso = setmetatable({}, {
  __index = function(_, k)
    return btv._wso_mirror[wso_canon(k)]
  end,
  __newindex = function(_, k, v)
    local canon = wso_canon(k)
    -- Queue the override (v == nil clears it) and write through the mirror so a
    -- read-after-write within this chunk is consistent (the server overwrites it on the
    -- next push, reflecting the core's validated/merged overlay).
    btv._set_workspace_option(canon, v)
    btv._wso_mirror[canon] = v
  end,
})

-- An option name bemtvi actually models (any scope): the routed window/buffer/
-- global options plus the read-mostly catch-all store. Used by `vim.fn.exists` to
-- answer the `&opt` / `+opt` probe honestly — 1 only for options we really have.
local function option_known(name)
  return O_WIN[name]
    or O_BUF[name]
    or O_GLOBAL[name] ~= nil
    or O_GLOBAL_DEFAULT[name] ~= nil
    or btv._o_store[name] ~= nil
end

-- `btv.exists`(expr) [alias `vim.fn.exists`]: does the vim entity named by `expr` exist? (1 / 0). bemtvi
-- answers the forms it can verify and reports 0 for the rest (rather than a fake
-- 1) so feature-probing stays honest:
--   * `'&opt'` / `'&l:opt'` / `'&g:opt'` / `'+opt'`  -> an option bemtvi models. A completion
--     plugin gates every window-option write on `exists('+'..key)`, so an unknown
--     option is skipped instead of erroring the float setup.
--   * `'g:'`/`'b:'`/`'w:'`/`'t:'`/`'v:'` prefixed name -> that scoped variable is set.
--   * `':Cmd'` -> a user command bemtvi can confirm (2, neovim's exact-match value);
--     a buffer-local command for the current buffer counts, like at dispatch.
--   * everything else (`'*func'`, built-in `':write'`, bare names) -> 0 (can't confirm).
function btv.exists(expr)
  expr = tostring(expr or "")
  local lead = expr:sub(1, 1)
  if lead == "&" or lead == "+" then
    local name = expr:sub(2):gsub("^[gl]:", "")
    return option_known(name) and 1 or 0
  end
  local scope, name = expr:match("^([gbwtv]):(.+)$")
  if scope then
    local tbl = ({ g = vim.g, b = vim.b, w = vim.w, t = vim.t, v = vim.v })[scope]
    if tbl == nil then
      return 0
    end
    local ok, val = pcall(function()
      return tbl[name]
    end)
    return (ok and val ~= nil) and 1 or 0
  end
  -- `':Cmd'` — a user command (global or buffer-local for the current buffer). neovim
  -- answers 2 for an exact match, so a `exists(':Foo') == 2` probe works. Built-in
  -- ex-commands aren't introspectable here, so they stay 0 (honest probing).
  local cmd = expr:match("^:(%a[%w_]*)")
  if cmd and btv._resolve_user_command then
    return btv._resolve_user_command(cmd, 0) ~= nil and 2 or 0
  end
  return 0
end
vim.fn.exists = btv.exists

-- `btv.opt`: neovim's rich Option object. Reading a field yields an Option wrapping
-- the option's current value; the methods (:get / :append / :prepend / :remove)
-- and the +/-/^ operators mutate list / char-flag / key:val-map options the way
-- plugin configs (and plugin managers) expect, and a table assignment
-- (`btv.opt.rtp = { ... }`) encodes back to the option's comma string. Scope
-- routing is inherited from `btv.o`. For the runtimepath family a mutation also
-- feeds Lua's package.path, so a freshly-added plugin dir becomes require-able —
-- matching neovim, where runtimepath drives module search. (The earlier thin
-- scalar proxy sufficed for colorscheme get/set but broke `vim.opt.rtp:append`.)

-- Option "kinds": list (comma-separated <-> Lua array), map (comma-separated
-- key:val <-> Lua table), flag (concatenated single chars <-> char set). Keyed by
-- full name and abbreviation; everything else is a plain scalar.
local OPT_LIST = {
  runtimepath = true,
  rtp = true,
  packpath = true,
  pp = true,
  path = true,
  pa = true,
  tags = true,
  tag = true,
  wildignore = true,
  wig = true,
  backupdir = true,
  bdir = true,
  directory = true,
  dir = true,
  undodir = true,
  udir = true,
  diffopt = true,
  dip = true,
  completeopt = true,
  cot = true,
  sessionoptions = true,
  ssop = true,
  viewoptions = true,
  vop = true,
  switchbuf = true,
  swb = true,
  clipboard = true,
  cb = true,
  spelllang = true,
  spl = true,
  errorformat = true,
  efm = true,
  grepformat = true,
  gfm = true,
  comments = true,
  com = true,
  whichwrap = true,
  ww = true,
  virtualedit = true,
  ve = true,
  complete = true,
  cpt = true,
  wildmode = true,
  wim = true,
  colorcolumn = true,
  cc = true,
}
local OPT_MAP = {
  listchars = true,
  lcs = true,
  fillchars = true,
  fcs = true,
}
local OPT_FLAG = {
  shortmess = true,
  shm = true,
  formatoptions = true,
  fo = true,
  cpoptions = true,
  cpo = true,
  guioptions = true,
  go = true,
  mouse = true,
  concealcursor = true,
  cocu = true,
}

-- The kind of `name`. `assigning_table` biases an unknown option toward `"list"`
-- (a plugin passing a table almost always means a comma list); otherwise unknown
-- options are scalars.
local function opt_kind(name, assigning_table)
  if OPT_LIST[name] then
    return "list"
  end
  if OPT_MAP[name] then
    return "map"
  end
  if OPT_FLAG[name] then
    return "flag"
  end
  return assigning_table and "list" or "scalar"
end

local function opt_split_comma(raw)
  local out = {}
  for piece in tostring(raw or ""):gmatch("[^,]+") do
    out[#out + 1] = piece
  end
  return out
end

-- Decode the option's stored string form into its kind's Lua value.
local function opt_decode(kind, raw)
  if kind == "list" then
    return opt_split_comma(raw)
  elseif kind == "map" then
    local m = {}
    for _, piece in ipairs(opt_split_comma(raw)) do
      local key, val = piece:match("^(.-):(.*)$")
      if key then
        m[key] = val
      else
        m[piece] = true
      end
    end
    return m
  elseif kind == "flag" then
    local m, s = {}, tostring(raw or "")
    for i = 1, #s do
      m[s:sub(i, i)] = true
    end
    return m
  end
  return raw
end

-- Encode a kind's Lua value back to the option's string form.
local function opt_encode(kind, val)
  if kind == "list" then
    local parts = {}
    for _, v in ipairs(val) do
      parts[#parts + 1] = tostring(v)
    end
    return table.concat(parts, ",")
  elseif kind == "map" then
    local parts = {}
    for k, v in pairs(val) do
      if v == true then
        parts[#parts + 1] = k
      elseif v then
        parts[#parts + 1] = k .. ":" .. tostring(v)
      end
    end
    return table.concat(parts, ",")
  elseif kind == "flag" then
    local parts = {}
    if vim.islist(val) then
      for _, c in ipairs(val) do
        parts[#parts + 1] = c
      end
    else
      for k, v in pairs(val) do
        if v then
          parts[#parts + 1] = k
        end
      end
    end
    return table.concat(parts)
  end
  return val
end

-- Appending to the runtimepath family must make the new dir's lua/ require-able,
-- the way neovim drives module search off runtimepath. Mirror the pattern
-- seed_package_path uses on the host side.
local OPT_RTP = { runtimepath = true, rtp = true, packpath = true, pp = true }
local function opt_seed_require(name, entries)
  if not OPT_RTP[name] then
    return
  end
  for _, e in ipairs(entries) do
    e = tostring(e)
    package.path = package.path .. ";" .. e .. "/lua/?.lua;" .. e .. "/lua/?/init.lua"
  end
end

-- Read/write one option in a named scope, the dispatcher every `vim.opt*` table and
-- every `Option` method flushes through:
--   * "o"      — `vim.opt` / `vim.o`: vim's `:set`. A buffer-local option moves BOTH its
--                global value and the current buffer's; a window option, the current
--                window's.
--   * "local"  — `vim.opt_local`: `:setlocal`, this buffer/window only.
--   * "global" — `vim.opt_global`: `:setglobal`, the value a new buffer is born from.
-- A global-scope option has one value, so every scope reaches the same place for it.
-- `btv.go` / `vim.bo` / `vim.wo` are indexed at call time (they are defined further down
-- this chunk), which is fine — these run at config time, once the whole chunk has loaded.
local function scope_get(scope, k)
  if scope == "global" then
    return btv.go[k]
  end
  if scope == "local" then
    if O_WIN[k] then
      return vim.wo[k]
    end
    if O_BUF[k] then
      return vim.bo[k]
    end
  end
  return o_get(k)
end

local function scope_set(scope, k, v)
  if scope == "global" then
    btv.go[k] = v
    return
  end
  if scope == "local" then
    if O_WIN[k] then
      vim.wo[k] = v
      return
    end
    if O_BUF[k] then
      vim.bo[k] = v
      return
    end
  end
  o_set(k, v)
end

local Option = {}
Option.__index = Option

-- An `Option` handle remembers which scope it came from, so `:append`/`:remove` and a
-- later assignment flush to the same tier the read used ("o" = vim's `:set`, "local" =
-- `:setlocal`, "global" = `:setglobal`). Absent ⇒ "o", the plain `vim.opt`.
local function opt_new(name, kind, value, scope)
  return setmetatable({ _name = name, _kind = kind, _value = value, _scope = scope }, Option)
end

-- A scalar option being list-mutated (an unknown comma option) promotes to a list.
local function opt_promote(self)
  if self._kind == "scalar" then
    self._kind = "list"
    self._value = opt_split_comma(self._value)
  end
end

-- Apply op ∈ {append, prepend, remove} to `self._value`, writing through unless
-- `noflush` (the +/-/^ operators build a value that the assignment flushes).
local function opt_mutate(self, op, v, noflush)
  opt_promote(self)
  local kind = self._kind
  if kind == "flag" then
    local s = tostring(v)
    for i = 1, #s do
      self._value[s:sub(i, i)] = (op ~= "remove") or nil
    end
  elseif kind == "map" then
    if op == "remove" then
      local keys = type(v) == "table" and (vim.islist(v) and v or vim.tbl_keys(v)) or { v }
      for _, k in ipairs(keys) do
        self._value[k] = nil
      end
    else
      for k, val in pairs(v) do
        if op == "append" or self._value[k] == nil then
          self._value[k] = val
        end
      end
    end
  else -- list
    local items = {}
    if type(v) == "table" then
      for _, x in ipairs(v) do
        items[#items + 1] = x
      end
    else
      items[1] = v
    end
    if op == "remove" then
      local drop = {}
      for _, x in ipairs(items) do
        drop[x] = true
      end
      local keep = {}
      for _, x in ipairs(self._value) do
        if not drop[x] then
          keep[#keep + 1] = x
        end
      end
      self._value = keep
    elseif op == "prepend" then
      for i = #items, 1, -1 do
        table.insert(self._value, 1, items[i])
      end
      opt_seed_require(self._name, items)
    else -- append
      for _, x in ipairs(items) do
        self._value[#self._value + 1] = x
      end
      opt_seed_require(self._name, items)
    end
  end
  if not noflush then
    scope_set(self._scope, self._name, opt_encode(self._kind, self._value))
  end
  return self
end

function Option:append(v)
  return opt_mutate(self, "append", v, false)
end
function Option:prepend(v)
  return opt_mutate(self, "prepend", v, false)
end
function Option:remove(v)
  return opt_mutate(self, "remove", v, false)
end
function Option:get()
  if self._kind == "scalar" then
    return self._value
  end
  return vim.deepcopy(self._value)
end

local function opt_clone(self)
  return opt_new(self._name, self._kind, vim.deepcopy(self._value), self._scope)
end
Option.__add = function(self, v)
  return opt_mutate(opt_clone(self), "append", v, true)
end
Option.__pow = function(self, v)
  return opt_mutate(opt_clone(self), "prepend", v, true)
end
Option.__sub = function(self, v)
  return opt_mutate(opt_clone(self), "remove", v, true)
end
Option.__tostring = function(self)
  return tostring(opt_encode(self._kind, self._value))
end

local function opt_assign(name, v, scope)
  if getmetatable(v) == Option then
    scope_set(scope, name, opt_encode(v._kind, v._value))
    if v._kind == "list" then
      opt_seed_require(name, v._value)
    end
  elseif type(v) == "table" then
    local kind = opt_kind(name, true)
    scope_set(scope, name, opt_encode(kind, v))
    if kind == "list" then
      opt_seed_require(name, v)
    end
  else
    scope_set(scope, name, v)
  end
end

-- One `vim.opt`-shaped table per scope: same Option machinery, different tier.
local function opt_table(scope)
  return setmetatable({}, {
    __index = function(_, k)
      local kind = opt_kind(k, false)
      return opt_new(k, kind, opt_decode(kind, scope_get(scope, k)), scope)
    end,
    __newindex = function(_, k, v)
      opt_assign(k, v, scope)
    end,
  })
end

btv.opt = opt_table("o")
vim.opt = btv.opt
-- The scoped twins, as in neovim: `vim.opt_local` writes only the current
-- buffer/window, `vim.opt_global` only the global value a new buffer is born from
-- (`:setlocal` / `:setglobal`). A global-scope option has a single value, so all three
-- reach the same place for it.
btv.opt_local = opt_table("local")
btv.opt_global = opt_table("global")
vim.opt_local = btv.opt_local
vim.opt_global = btv.opt_global

-- `btv.go`: the *global* value of options (neovim's editor-wide scope). Unlike
-- `btv.o` it never delegates to the window/buffer scope — reading a window/buffer
-- option through `btv.go` yields its global default, matching neovim's "go is the
-- global option store" semantics. The wired global options reflect the core
-- (`btv._go_mirror`, the same home `vim.o`'s global branch uses); any other option
-- lands in the plain `btv._o_store` (observable read/write, not yet honored).
local function go_get(k)
  -- A buffer-local option read through `vim.go` means its *global value* — the tier a
  -- new buffer is born from — not the current buffer's (that is `vim.bo` / `vim.o`).
  local tiered = btv._bo_global_get(k)
  if tiered ~= nil then
    return tiered
  end
  tiered = btv._wo_global_get(k)
  if tiered ~= nil then
    return tiered
  end
  local canon = O_GLOBAL[k]
  if canon then
    local v = btv._go_mirror[canon]
    if v ~= nil then
      return v
    end
    return O_GLOBAL_DEFAULT[canon]
  end
  return btv._o_store[k]
end
local function go_set(k, v)
  -- `vim.go.tabstop = 3` writes the buffer-local option's global value only, leaving
  -- the current buffer alone — vim's `:setglobal`.
  if btv._bo_global_set(k, v) or btv._wo_global_set(k, v) then
    return
  end
  local canon = O_GLOBAL[k]
  if canon then
    btv._set_global_option(canon, v)
    btv._go_mirror[canon] = v
    return
  end
  -- A name bemtvi DOES model, in a scope with no global tier to write (checked after
  -- `O_GLOBAL`, so `'regexsyntax'` — global-local, whose tier is the editor-wide option —
  -- is caught above rather than here). Say what is actually wrong instead of the
  -- typo warning below, and reject the write.
  if O_BUF[k] or O_WIN[k] then
    warn_no_global_value(k)
    return
  end
  -- A seeded read-mostly option is modeled — store silently; warn only on a name
  -- bemtvi has never seen (a typo). Mirrors `o_set`.
  if not btv._o_known[k] then
    warn_unknown_opt(k)
  end
  btv._o_store[k] = v
end
btv.go = setmetatable({}, {
  __index = function(_, k)
    return go_get(k)
  end,
  __newindex = function(_, k, v)
    go_set(k, v)
  end,
})
vim.go = btv.go

-- `btv.bo`: buffer-local options, indexed by bufnr (`btv.bo[buf].filetype`).
--
-- The indentation options bemtvi's core honors — tabstop/shiftwidth/expandtab and
-- their `ts`/`sw`/`et` abbreviations — are *wired*: a write reaches the live
-- editor (it changes how the buffer renders tabs and indents on <Tab>), and a
-- read returns the core's current value (`btv._bo_mirror`, refreshed by the
-- server) — the option default until set, and a value set through the `:set`
-- ex-command path, not just one written from Lua.
--
-- `filetype`/`ft` stays authoritative from the current-buffer snapshot (it backs
-- the `root_dir` filetype checks configs do at load) unless a write overrode it.
-- Any other option falls back to the plain Lua store `btv._bo_store` (observable
-- read/write, but not yet driving editor behavior). A bare `btv.bo.<opt>` (no
-- bufnr) targets the current buffer. The `btv._bo_mirror` / `btv._bo_store`
-- mirrors and the `btv._resolve_bufnr` / `btv._buf_set_option` bridges this reads
-- are defined below (this file + the Rust bridge), but only touched
-- from inside the metamethods at config time, so the forward reference is fine.

-- Canonical name of a *wired* (core-honored) buffer option, or nil for the rest.
local BUF_OPT_CANON = {
  tabstop = "tabstop",
  ts = "tabstop",
  shiftwidth = "shiftwidth",
  sw = "shiftwidth",
  softtabstop = "softtabstop",
  sts = "softtabstop",
  expandtab = "expandtab",
  et = "expandtab",
  -- The grammar-free indent fallbacks (`autoindent`/`smartindent`) and the
  -- bracket/quote auto-pairing toggle (`autopairs`); writes reach the live
  -- editor, reads return the core's value.
  autoindent = "autoindent",
  ai = "autoindent",
  smartindent = "smartindent",
  si = "smartindent",
  autopairs = "autopairs",
  -- Whether `=` reindents blank lines (default off ⇒ blank lines snap to
  -- column 0); a write reaches the live editor, a read returns the core value.
  indentemptylines = "indentemptylines",
  iel = "indentemptylines",
  -- The buffer-local override of the global `regexsyntax` dialect for `/` and
  -- `:s`. `btv.bo.regexsyntax = "vim"` pins this buffer; reads return the
  -- *effective* dialect (the override resolved against the global).
  regexsyntax = "regexsyntax",
  rxs = "regexsyntax",
  -- The on-disk charset (`btv.bo.fileencoding = "latin1"`) and whether a BOM is
  -- written (`btv.bo.bomb`). Reads return the core's per-buffer value (set via
  -- `:set fenc`, read-detection, or here).
  fileencoding = "fileencoding",
  fenc = "fileencoding",
  bomb = "bomb",
  -- The line-ending style (`btv.bo.fileformat` → `"unix"`/`"dos"`/`"mac"`). Reads return
  -- the core's per-buffer value (set from the bytes on read, or via `:set ff=`).
  fileformat = "fileformat",
  ff = "fileformat",
  -- Whether the document ends with a line break (`btv.bo.endofline`, set from the
  -- bytes on read) and whether a write supplies a missing one
  -- (`btv.bo.fixendofline = false` to round-trip a no-newline file byte for byte).
  endofline = "endofline",
  eol = "endofline",
  fixendofline = "fixendofline",
  fixeol = "fixendofline",
  -- The comment template `gc`/`gcc` wrap lines with. Reads return the *effective*
  -- value (the buffer override, else the filetype default); a write sets the
  -- per-buffer override (empty falls back to the filetype default).
  commentstring = "commentstring",
  cms = "commentstring",
  -- Whether the buffer's text may be changed (`vim.bo.modifiable`). The server
  -- mirrors it, and `btv.buf.set_lines` reads it to fail loud before queueing an edit
  -- a `nomodifiable` buffer would refuse.
  modifiable = "modifiable",
  ma = "modifiable",
  -- The fold buffer-options: the method (`btv.bo.foldmethod = "expr"`), the
  -- `foldmethod=expr` expression (`btv.bo.foldexpr = "v:lua.vim.treesitter.foldexpr()"`),
  -- the `foldmethod=marker` delimiter pair (`btv.bo.foldmarker = "{{{,}}}"`), and the
  -- nesting / minimum-span caps. The `vim.bo` companions to the `:set foldmethod=…` /
  -- `:set foldexpr=…` / `:set foldmarker=…` paths; writes reach the live fold engine.
  foldmethod = "foldmethod",
  fdm = "foldmethod",
  foldexpr = "foldexpr",
  fde = "foldexpr",
  foldmarker = "foldmarker",
  fmr = "foldmarker",
  foldnestmax = "foldnestmax",
  fdn = "foldnestmax",
  foldminlines = "foldminlines",
  fml = "foldminlines",
  -- How many undo states the buffer's history keeps (`:set undolevels=…`).
  undolevels = "undolevels",
  ul = "undolevels",
  -- The change flag (`vim.bo.modified` / `:set [no]modified`). Reads return the
  -- server-mirrored buffer state; a write reaches the core to set/clear it — clearing
  -- is how a plugin that fills a buffer as a *read* (a `BufReadCmd` directory listing)
  -- marks it not-an-unsaved-edit (no `[+]`, no E37 on `:q`).
  modified = "modified",
  mod = "modified",
}
-- Core defaults, the safety net when the mirror hasn't been pushed for a buffer.
-- Match bemtvi's core: tabstop 4, with shiftwidth/softtabstop following it via
-- their sentinels (0 = follow tabstop, -1 = follow shiftwidth); regexsyntax
-- `"pcre"` (the buffer follows the global, whose default is pcre); fileencoding
-- `"utf-8"` with no BOM.
local BUF_OPT_DEFAULT = {
  tabstop = 4,
  shiftwidth = 0,
  softtabstop = -1,
  expandtab = false,
  autoindent = false,
  smartindent = false,
  autopairs = false,
  regexsyntax = "pcre",
  fileencoding = "utf-8",
  bomb = false,
  fileformat = "unix",
  endofline = false,
  fixendofline = true,
  commentstring = "",
  modifiable = true,
  foldmethod = "manual",
  foldexpr = "",
  foldmarker = "{{{,}}}",
  foldnestmax = 20,
  foldminlines = 1,
  undolevels = 1000,
  modified = false,
}

-- The buffer-local options that HAVE a global value — the tier a newly created buffer is
-- born from (`Editor::buf_opts_global`). Canonical name -> true, DERIVED from core's
-- catalog (`btv._set_options_catalog`), whose `global_tier` column comes straight from
-- `bemtvi_core::options::has_global_tier` — the one place the question is answered, shared
-- with the ex `:setglobal` path that rejects a tier-less option with `E5100`.
--
-- Left out, and why: the four slots the *read* decides (`fileencoding` / `bomb` /
-- `fileformat` / `endofline`), the per-buffer marker `modifiable`, and the two nouns
-- derived per buffer (`filetype`, `ts_highlight`). Writing one through `vim.go` would be a
-- value nothing reads, so the core rejects it loudly and this table keeps the Lua side
-- from ever asking.
local BO_GLOBAL_TIER = {}

--- Read the **global value** of buffer-local option `opt` — what a newly created buffer
--- is born with. `nil` for a name with no global value (see `BO_GLOBAL_TIER`), which is
--- how `vim.go` / `vim.o` tell "this has a tier" from "this does not".
function btv._bo_global_get(opt)
  local canon = BUF_OPT_CANON[opt]
  if canon == nil or not BO_GLOBAL_TIER[canon] then
    return nil
  end
  local v = btv._bo_global[canon]
  if v ~= nil then
    return v
  end
  return BUF_OPT_DEFAULT[canon]
end

--- Write the **global value** of buffer-local option `opt`, leaving every open buffer
--- alone (vim's `:setglobal`). Returns whether `opt` has a tier at all, so a caller can
--- fall through to another scope when it does not.
function btv._bo_global_set(opt, value)
  local canon = BUF_OPT_CANON[opt]
  if canon == nil or not BO_GLOBAL_TIER[canon] then
    return false
  end
  -- Queue the change for the core and echo it into the mirror, so a read-after-write
  -- within this chunk is consistent (the server overwrites it on the next push).
  btv._buf_set_option_global(canon, value)
  btv._bo_global[canon] = value
  return true
end

local function bo_get(bufnr, opt)
  local canon = BUF_OPT_CANON[opt]
  if canon then
    local mirror = btv._bo_mirror[bufnr]
    if mirror ~= nil and mirror[canon] ~= nil then
      return mirror[canon]
    end
    return BUF_OPT_DEFAULT[canon]
  end
  -- `modified` is read-only buffer *state* (not a settable option), mirrored by
  -- the server so a `'tabline'`/statusline label can read `btv.bo[n].modified`.
  if opt == "modified" or opt == "mod" then
    local mirror = btv._bo_mirror[bufnr]
    return (mirror ~= nil and mirror.modified) or false
  end
  -- `filetype` (the treesitter *language* noun) and `ts_highlight` (the *whether*
  -- noun) are wired to the core; reads come from the bo mirror the server pushes
  -- (so `:set ft`, `:setf`, and `btv.bo.filetype` all agree), with the current-
  -- buffer snapshot / the default as the pre-first-push fallback.
  if opt == "filetype" or opt == "ft" then
    local mirror = btv._bo_mirror[bufnr]
    if mirror ~= nil and mirror.filetype ~= nil then
      return mirror.filetype
    end
    return (btv._cur_buf or {}).filetype
  end
  if opt == "ts_highlight" then
    local mirror = btv._bo_mirror[bufnr]
    if mirror ~= nil and mirror.ts_highlight ~= nil then
      return mirror.ts_highlight
    end
    return true
  end
  -- `buftype` is read-only buffer *kind* state ("" ordinary, "nofile" scratch
  -- surface, "quickfix", "terminal"), derived by the core and mirrored by the
  -- server so a statusline / plugin can gate on it the neovim way (`buftype ~= ""`
  -- means "not a real file buffer"). Empty string until the first push.
  if opt == "buftype" or opt == "bt" then
    local mirror = btv._bo_mirror[bufnr]
    return (mirror ~= nil and mirror.buftype) or ""
  end
  local store = btv._bo_store[bufnr]
  if store ~= nil and store[opt] ~= nil then
    return store[opt]
  end
  return nil
end
local function bo_set(bufnr, opt, value)
  local canon = BUF_OPT_CANON[opt]
  if canon then
    -- Queue the change for the core and update the mirror so a read-after-write
    -- within this chunk is consistent (the server overwrites it on the next push).
    btv._buf_set_option(bufnr, canon, value)
    btv._bo_mirror[bufnr] = btv._bo_mirror[bufnr] or {}
    btv._bo_mirror[bufnr][canon] = value
    return
  end
  -- `filetype` / `ts_highlight` are the wired treesitter nouns: write through to
  -- the core and echo into the mirror for read-after-write within this chunk.
  if opt == "filetype" or opt == "ft" then
    btv._buf_set_option(bufnr, "filetype", value)
    btv._bo_mirror[bufnr] = btv._bo_mirror[bufnr] or {}
    btv._bo_mirror[bufnr].filetype = value
    return
  end
  if opt == "ts_highlight" then
    btv._buf_set_option(bufnr, "ts_highlight", value)
    btv._bo_mirror[bufnr] = btv._bo_mirror[bufnr] or {}
    btv._bo_mirror[bufnr].ts_highlight = value
    return
  end
  btv._bo_store[bufnr] = btv._bo_store[bufnr] or {}
  btv._bo_store[bufnr][opt] = value
end
local function bo_proxy(bufnr)
  bufnr = btv._resolve_bufnr(bufnr)
  return setmetatable({}, {
    __index = function(_, opt)
      return bo_get(bufnr, opt)
    end,
    __newindex = function(_, opt, value)
      bo_set(bufnr, opt, value)
    end,
  })
end
btv.bo = setmetatable({}, {
  __index = function(_, k)
    -- numeric key -> per-buffer proxy; option name -> current-buffer value.
    if type(k) == "number" then
      return bo_proxy(k)
    end
    return bo_get(btv._resolve_bufnr(0), k)
  end,
  __newindex = function(_, k, value)
    bo_set(btv._resolve_bufnr(0), k, value)
  end,
})
vim.bo = btv.bo

-- `btv.wo`: window-local options, indexed by window id (`btv.wo[win].number`), the
-- window analogue of `btv.bo`. The number-gutter options bemtvi's core honors —
-- number/relativenumber and their nu/rnu abbreviations — are *wired*: a write
-- reaches the live editor (it changes that window's gutter) and a read returns
-- the core's current value from the `btv._wins` mirror the server refreshes (the
-- default until set, or a value set via the `:set` ex path). Any other option
-- falls back to the plain `btv._wo_store` (observable, not yet honored). A bare
-- `btv.wo.<opt>` (no window id) targets the current window. As with `btv.bo` the
-- `btv._wins` / `btv._wo_store` mirrors and the `btv._resolve_win` /
-- `btv._win_set_option` bridges are defined below (this file), reached
-- only from the metamethods at config time.
local WIN_OPT_CANON = {
  number = "number",
  nu = "number",
  relativenumber = "relativenumber",
  rnu = "relativenumber",
  cursorline = "cursorline",
  cul = "cursorline",
  wrap = "wrap",
  scrolloff = "scrolloff",
  so = "scrolloff",
  colorcolumn = "colorcolumn",
  cc = "colorcolumn",
  -- A per-window override of the global `'scrollanim'`: `vim.wo[win].scrollanim = false`
  -- makes that window's scrolls snap (the side-by-side diff opts its panes out
  -- so a synced scroll doesn't desync). `vim.o.scrollanim` stays the global default.
  scrollanim = "scrollanim",
  numberwidth = "numberwidth",
  nuw = "numberwidth",
  signcolumn = "signcolumn",
  scl = "signcolumn",
  fillchars = "fillchars",
  fcs = "fillchars",
  breakindent = "breakindent",
  bri = "breakindent",
  showbreak = "showbreak",
  sbr = "showbreak",
  breakindentopt = "breakindentopt",
  briopt = "breakindentopt",
  sidescroll = "sidescroll",
  ss = "sidescroll",
  sidescrolloff = "sidescrolloff",
  siso = "sidescrolloff",
  padding = "padding",
  pad = "padding",
  winhighlight = "winhighlight",
  winhl = "winhighlight",
  -- The per-window fold options: the fold-column gutter width (`vim.wo.foldcolumn`),
  -- whether closed folds collapse on screen (`vim.wo.foldenable`), and the
  -- open-depth threshold (`vim.wo.foldlevel` — folds deeper than this display
  -- closed). The `vim.wo`/`vim.o` companions to `:set foldcolumn=`/`foldlevel=`.
  foldcolumn = "foldcolumn",
  fdc = "foldcolumn",
  foldenable = "foldenable",
  fen = "foldenable",
  foldlevel = "foldlevel",
  fdl = "foldlevel",
}
local WIN_OPT_DEFAULT = {
  number = true,
  relativenumber = true,
  cursorline = false,
  wrap = false,
  scrolloff = 0,
  colorcolumn = "",
  scrollanim = true, -- resolved default before the mirror lands (global default is on)
  numberwidth = 4,
  signcolumn = "auto",
  fillchars = "",
  breakindent = false,
  showbreak = "",
  breakindentopt = "",
  sidescroll = 1,
  sidescrolloff = 0,
  padding = "",
  winhighlight = "",
  foldcolumn = 0,
  foldenable = true,
  foldlevel = 0,
}
-- Exposed for this file's nvim_{get,set}_option_value, which classify a name
-- as window-scoped before routing it through `btv.wo`.
btv._win_opt_canon = WIN_OPT_CANON

-- The window-local options that have a GLOBAL value — vim's `:setglobal` tier, which
-- `vim.go` / `vim.opt_global` read and write. Name/abbrev -> canonical name, DERIVED from
-- core's catalog (`btv._set_options_catalog`) rather than aliased to `WIN_OPT_CANON`: a
-- *split* still copies the window it came from, so the tier is what seeds a window minted
-- with no source (a dock, the quickfix tab).
--
-- The distinction the alias got wrong is `'scrollanim'`, which is a **global** option with
-- a per-window override — its global value is the editor-wide one `vim.o` reads, not a
-- window tier — so `vim.go.scrollanim` answered a tier nothing populates and always said
-- `true`. Deriving from the catalog's own `scope` column keeps that straight.
local WO_GLOBAL_TIER = {}

--- Read the **global value** of window-local option `opt`. `nil` when `opt` is not a
--- window option at all, so `vim.go` can fall through to the other scopes.
function btv._wo_global_get(opt)
  local canon = WO_GLOBAL_TIER[opt]
  if canon == nil then
    return nil
  end
  local v = btv._wo_global[canon]
  if v ~= nil then
    return v
  end
  return WIN_OPT_DEFAULT[canon]
end

--- Write the **global value** of window-local option `opt`, leaving every open window
--- alone (vim's `:setglobal`). Returns whether `opt` is a window option at all.
function btv._wo_global_set(opt, value)
  local canon = WO_GLOBAL_TIER[opt]
  if canon == nil then
    return false
  end
  btv._win_set_option_global(canon, value)
  btv._wo_global[canon] = value
  return true
end

local function wo_get(win, opt)
  local canon = WIN_OPT_CANON[opt]
  if canon then
    local w = (btv._wins or {})[win]
    if w ~= nil and w[canon] ~= nil then
      return w[canon]
    end
    return WIN_OPT_DEFAULT[canon]
  end
  local store = btv._wo_store[win]
  if store ~= nil and store[opt] ~= nil then
    return store[opt]
  end
  return nil
end
local function wo_set(win, opt, value)
  local canon = WIN_OPT_CANON[opt]
  if canon then
    -- Queue the change for the core and update the mirror so a read-after-write
    -- within this chunk is consistent (the server overwrites it on the next push).
    btv._win_set_option(win, canon, value)
    local w = (btv._wins or {})[win]
    if w then
      w[canon] = value
    end
    return
  end
  btv._wo_store[win] = btv._wo_store[win] or {}
  btv._wo_store[win][opt] = value
end
local function wo_proxy(win)
  win = btv._resolve_win(win)
  return setmetatable({}, {
    __index = function(_, opt)
      return wo_get(win, opt)
    end,
    __newindex = function(_, opt, value)
      wo_set(win, opt, value)
    end,
  })
end
btv.wo = setmetatable({}, {
  __index = function(_, k)
    -- numeric key -> per-window proxy; option name -> current-window value.
    if type(k) == "number" then
      return wo_proxy(k)
    end
    return wo_get(btv._resolve_win(0), k)
  end,
  __newindex = function(_, k, value)
    wo_set(btv._resolve_win(0), k, value)
  end,
})
vim.wo = btv.wo

-- `btv.v` [alias `vim.v`]: neovim's predefined `v:` variables. bemtvi backs the few with a real
-- editor source from a Rust→Lua mirror (`btv._v_mirror`) the server refreshes
-- before any Lua that can read them:
--   * count    — the count accumulated for the pending command (0 when none)
--   * count1   — count, but at least 1 (v:count1)
--   * register — the register named by a leading `"x`, else `"` (the unnamed)
--   * operator — the pending operator char (`d`/`c`/`y`/…), `""` when none
-- `vim_did_enter` is set to 1 once the startup VimEnter point passes (it is NOT
-- overwritten by the per-tick mirror refresh, so it stays sticky). `v:true` /
-- `v:false` are the boolean constants plugins compare against (reached via
-- `vim.v["true"]` since `true` is a Lua keyword). An unknown `v:` name reads
-- whatever was stored (nil if never set) rather than failing — `v:` is a
-- variable table, and many of neovim's predefined vars are legitimately empty.
btv._v_mirror = btv._v_mirror or { vim_did_enter = 0 }
-- Refresh the editor-sourced fields (count/register/operator); vim_did_enter and
-- any plugin-set var are preserved (the server pushes this every tick).
function btv._set_v_mirror(count, count1, register, operator)
  local m = btv._v_mirror
  m.count, m.count1, m.register, m.operator = count, count1, register, operator
end
function btv._set_vim_did_enter(v)
  btv._v_mirror.vim_did_enter = v and 1 or 0
end
-- The last mouse event's position, pushed every tick by the server (backs
-- `vim.fn.getmousepos` — see vimfn.lua). 1-based; winid/line/column are 0 off a
-- window's text.
btv._mouse_pos = btv._mouse_pos or {}
function btv._set_mouse_pos(screenrow, screencol, winid, winrow, wincol, line, column)
  local m = btv._mouse_pos
  m.screenrow, m.screencol, m.winid = screenrow, screencol, winid
  m.winrow, m.wincol, m.line, m.column = winrow, wincol, line, column
end
btv.v = setmetatable({}, {
  __index = function(_, k)
    if k == "true" then
      return true
    end
    if k == "false" then
      return false
    end
    local m = btv._v_mirror
    if k == "count" then
      return m.count or 0
    end
    if k == "count1" then
      return m.count1 or 1
    end
    if k == "register" then
      return m.register or '"'
    end
    if k == "operator" then
      return m.operator or ""
    end
    if k == "vim_did_enter" then
      return m.vim_did_enter or 0
    end
    -- `v:shell_error` is the exit status of the last `:!`/`system()` shell-out,
    -- 0 before any has run. `vim.fn.system`/`systemlist` write it; a plugin
    -- bootstrap branches on it (`if vim.v.shell_error ~= 0 then …`), so a `nil`
    -- default would read as "the clone failed" the very first time.
    if k == "shell_error" then
      return m.shell_error or 0
    end
    -- `v:exiting` is `v:null` (→ `vim.NIL` in Lua) until the editor is actually
    -- exiting, when it becomes the exit code. Plugins gate async work on it —
    -- a typical `exiting()` check is literally `vim.v.exiting ~= vim.NIL`, so a
    -- plain `nil` here reads as "already exiting" and the whole async runner
    -- (its git clone/install) silently refuses to start. Default to `vim.NIL`.
    if k == "exiting" then
      if m.exiting == nil then
        return vim.NIL
      end
      return m.exiting
    end
    return m[k]
  end,
  __newindex = function(_, k, v)
    btv._v_mirror[k] = v
  end,
})
vim.v = btv.v

-- `vim.env`: process environment, read through to the host; writes shadow locally
-- (a Lua-only override that wins over the host on the next read). bemtvi ships its
-- runtime embedded in the binary rather than as an on-disk $VIMRUNTIME tree, but
-- plugins concatenate `vim.env.VIMRUNTIME .. "/..."` unconditionally (some
-- source `$VIMRUNTIME/filetype.lua` at startup), so a nil there is a load-time
-- crash. Fall back to the data-dir runtime path: it need not be populated (bemtvi
-- does its own filetype detection), and a `:source` of a missing file under it
-- fails soft.
btv._env_shadow = btv._env_shadow or {}
vim.env = setmetatable({}, {
  __index = function(_, k)
    if btv._env_shadow[k] ~= nil then
      return btv._env_shadow[k]
    end
    local v = os.getenv(k)
    if v ~= nil then
      return v
    end
    if k == "VIMRUNTIME" then
      return vim.fn.stdpath("data") .. "/runtime"
    end
    return nil
  end,
  __newindex = function(_, k, v)
    btv._env_shadow[k] = v
  end,
})

-- `btv.log` [alias `vim.log`]: the log-level constants plugins compare against.
btv.log = { levels = { TRACE = 0, DEBUG = 1, INFO = 2, WARN = 3, ERROR = 4, OFF = 5 } }
vim.log = btv.log

-- `btv.reg.recording`() / `btv.reg.executing`() [aliases `vim.fn.reg_recording` /
-- reg_executing]: the register name of an in-progress macro recording / replay, or
-- `""` when none. bemtvi's core has no `q`-macro recording yet, so both are always `""` —
-- an honest "nothing in progress" (the value vim returns the vast majority of the
-- time), not a faked recording state. A statusline `%{reg_recording()}` stays blank.
btv.reg = btv.reg or {}
function btv.reg.recording()
  return ""
end
function btv.reg.executing()
  return ""
end
vim.fn.reg_recording = btv.reg.recording
vim.fn.reg_executing = btv.reg.executing

-- `btv.mark.list`([opts]) -> the set marks, current-buffer-relative like `:marks`:
-- the current buffer's automatic specials (`"`, `` ` ``, `.`, `^`, `[`, `]`, `<`,
-- `>`) and lowercase `a`–`z`, then the global `A`–`Z`, then the numbered `0`–`9`.
-- Reads the server-pushed `btv._marks` mirror (never live state). Each entry is:
--
-- ```lua
-- { name = "a",           -- the mark's one-character name
--   bufnr = 3,            -- the buffer it points into (0 for a pending mark)
--   line = 11,            -- 0-based line
--   col = 4,              -- 0-based byte column
--   path = "src/x.rs",    -- file to open on a jump ("" for an unnamed buffer)
--   text = "let x = 1" }  -- the line's text, or the file for an out-of-buffer mark
-- ```
--
-- `opts.names` (a string) filters to just those mark names, like `:marks aB`.
btv.mark = btv.mark or {}
function btv.mark.list(opts)
  opts = opts or {}
  local names = opts.names
  local out = {}
  for _, m in ipairs(btv._marks or {}) do
    if names == nil or names:find(m.name, 1, true) then
      out[#out + 1] = {
        name = m.name,
        bufnr = m.bufnr,
        line = m.line,
        col = m.col,
        path = m.path,
        text = m.text,
      }
    end
  end
  return out
end

-- `btv._cur_buf`: the current-buffer snapshot the server refreshes (via
-- `btv._set_cur_buf`) immediately before firing a buffer/mode autocmd, so a
-- callback can resolve "the buffer that fired" — `nvim_buf_get_name`(0) and
-- `expand('%')` read it. An interim until a real per-bufnr registry exists; with
-- the core single-message-at-a-time it can't go stale mid-dispatch.
btv._cur_buf = btv._cur_buf or { bufnr = 0, name = "", filetype = "" }

-- `btv._alt_file`: the alternate file name (vim's `#`), refreshed by the server
-- alongside the buffer mirror and read by `expand("#")`. A *name*, not a handle —
-- it outlives a `:bdelete` of the buffer it named, exactly as vim's `#` does.
btv._alt_file = btv._alt_file or ""

-- `btv._alt_buf`: the alternate buffer's *handle* (vim's `#` as a bufnr, `0` when there
-- is none), refreshed beside `btv._alt_file`. The handle is the fact a buffer *list*
-- needs — which row to flag `#` (`:ls`, the `buffers` picker) — while the name above is
-- what `:e #` reopens; they part ways the moment the alternate's buffer is deleted.
-- Read through `btv.buf.alternate()` / `vim.fn.bufnr("#")`.
btv._alt_buf = btv._alt_buf or 0

function btv._set_cur_buf(bufnr, name, filetype)
  btv._cur_buf = { bufnr = bufnr or 0, name = name or "", filetype = filetype or "" }
end

-- `btv._bufs` / `btv._cur_cursor` / `btv._cur_win`: the Rust→Lua buffer mirror the
-- buffer-read API (Phase 6) resolves against. The server refreshes it via
-- `btv._set_buf_mirror` before running any Lua that can read buffer or cursor
-- state, so `nvim_buf_get_lines` / `nvim_win_get_cursor` / `nvim_buf_is_loaded` read
-- live data without reaching the Server. `btv._bufs`[bufnr] = { lines, name,
-- loaded }. (Read-only: the buffer-text mutation surface is intentionally absent
-- from bemtvi's Lua API — see prelude/api.lua's header — so nothing writes `lines`
-- back through here; mutation is via ex-commands / keystrokes.)
btv._bufs = btv._bufs or {}
btv._cur_cursor = btv._cur_cursor or { row = 1, col = 0 }
btv._cur_win = btv._cur_win or 1000
-- The editor's current mode() short code (`"n"`/`"i"`/`"v"`/…), refreshed alongside
-- the buffer mirror so `vim.fn.mode`() (and a %{} statusline expression calling it)
-- reflects the live mode.
btv._cur_mode = btv._cur_mode or "n"
-- The open command line's type char (`":"`/`"/"`/`"?"`/`"@"`, or `""` when none is open),
-- refreshed alongside the buffer mirror so `vim.fn.getcmdtype`() reflects this frame.
btv._cur_cmdtype = btv._cur_cmdtype or ""
-- Per-buffer option store backing `vim.bo` / `nvim_set_option_value` (Phase 6); the
-- table is created here so the earlier-defined setter can index it safely. This
-- holds *arbitrary* (Lua-only) buffer options plugins set; the wired indentation
-- options (tabstop/shiftwidth/expandtab) are read from `btv._bo_mirror` instead,
-- which the server refreshes from the core (see `btv.bo` / `vim.bo` in this file).
btv._bo_store = btv._bo_store or {}
-- Rust→Lua mirror of the core's buffer-local option values, refreshed by the
-- server (`btv._set_bo_mirror`) before any Lua that can read options. Keyed by
-- bufnr → row. Authoritative for the wired options, so a read reflects the core
-- default until set and a value set through the `:set` ex path, not just one
-- written from Lua. The server pushes only the rows that moved since the last
-- refresh (an option change or a buffer edit) plus the bufnrs to drop (deleted
-- buffers), so an untouched push costs nothing.
btv._bo_mirror = btv._bo_mirror or {}

function btv._set_bo_mirror(entries, removed)
  for bufnr, row in pairs(entries or {}) do
    btv._bo_mirror[bufnr] = row
  end
  for _, bufnr in ipairs(removed or {}) do
    btv._bo_mirror[bufnr] = nil
  end
end

-- Rust→Lua mirror of the GLOBAL values of the buffer-local options (the tier a newly
-- created buffer is born from), refreshed by the server beside `btv._bo_mirror`. Read by
-- `vim.go` / `vim.opt_global`; only the options that have a tier appear (see
-- `BO_GLOBAL_TIER`).
btv._bo_global = btv._bo_global or {}

function btv._set_bo_global(entry)
  btv._bo_global = entry or {}
end

-- The window twin of `btv._bo_global`: the GLOBAL values of the window-local options,
-- refreshed by the server and read by `vim.go` / `vim.opt_global`.
btv._wo_global = btv._wo_global or {}

function btv._set_wo_global(entry)
  btv._wo_global = entry or {}
end

btv._wins = btv._wins or {}
btv._win_all = btv._win_all or { 1000 }
btv._win_order = btv._win_order or { 1000 }
btv._next_win = btv._next_win or 1001
-- Tab mirror (Phase 3): `btv._tabs[id]` = per-tab record ({ id, windows,
-- current_window }), `btv._tab_order` the tabline order `nvim_list_tabpages`
-- returns, `btv._cur_tab` the active id. Seeded to the single startup tab so a
-- read before the server's first mirror push still answers.
btv._tabs = btv._tabs or { [1] = { id = 1, windows = { 1000 }, current_window = 1000 } }
btv._tab_order = btv._tab_order or { 1 }
btv._cur_tab = btv._cur_tab or 1
-- Arbitrary (Lua-only) window options plugins set via `vim.wo`; the wired gutter
-- options (number/relativenumber) live on the `btv._wins` mirror instead.
btv._wo_store = btv._wo_store or {}

-- Extmark / namespace state (the decoration layer). `btv._namespaces` maps a
-- namespace name to its id and `btv._namespace_next` is the next id to mint, both
-- allocated entirely Lua-side (the sole allocator) so `nvim_create_namespace`
-- returns synchronously. `btv._extmarks[bufnr][ns][id]` mirrors each mark's
-- position/attrs for `nvim_buf_get_extmarks`; the server rebuilds it from the
-- authoritative core store before every chunk (so positions reflect edits), and
-- the set/del/clear wrappers write through it for read-after-write within a
-- chunk. `btv._extmark_next[bufnr][ns]` is the per-(buffer, namespace) id
-- allocator — persistent (never reset by the mirror refresh), so ids are never
-- reused, matching neovim.
btv._namespaces = btv._namespaces or {}
btv._namespace_next = btv._namespace_next or 1
btv._extmarks = btv._extmarks or {}
btv._extmark_next = btv._extmark_next or {}

-- Receive the extmark mirror: `entries[bufnr]` is the array of that buffer's
-- marks the server pushed from core (positions already shifted for any edits).
-- Rebuilds `btv._extmarks` from the authoritative state; the persistent
-- allocator (`btv._extmark_next`) is deliberately untouched.
function btv._set_extmark_mirror(entries, positions)
  local marks = {}
  for bufnr, list in pairs(entries or {}) do
    -- `true` ⇒ the buffer's mark SET did not change, only (possibly) where the marks
    -- sit. Keep the decorations we already hold and let the position pass below move
    -- them; rebuilding them would re-allocate every hl_group / sign / gravity field
    -- on every keystroke, which is exactly what this avoids.
    if list == true then
      marks[bufnr] = btv._extmarks[bufnr] or {}
    else
      local by_ns = {}
      for _, m in ipairs(list) do
        by_ns[m.ns] = by_ns[m.ns] or {}
        -- Reconstruct the `decoration` sub-table from the round-tripped sign fields so
        -- a get_extmarks(details=true) AFTER this server refresh still returns the
        -- gutter sign (the same shape the same-chunk write-through stored).
        local decoration
        if m.sign_text ~= nil or m.sign_hl_group ~= nil then
          decoration = { sign_text = m.sign_text, sign_hl_group = m.sign_hl_group }
        end
        if m.line_fill_text ~= nil then
          decoration = decoration or {}
          decoration.line_fill = { text = m.line_fill_text, hl_group = m.line_fill_hl }
        end
        if m.line_hl_group ~= nil then
          decoration = decoration or {}
          decoration.line_hl_group = m.line_hl_group
        end
        by_ns[m.ns][m.id] = {
          row = m.row,
          col = m.col,
          end_row = m.end_row,
          end_col = m.end_col,
          hl_group = m.hl_group,
          priority = m.priority,
          decoration = decoration,
          -- Only the non-default gravity rides the wire; fill the default when absent
          -- (start defaults right-gravity `true`, end defaults left-gravity → `false`).
          right_gravity = m.right_gravity ~= false,
          end_right_gravity = m.end_right_gravity == true,
        }
      end
      marks[bufnr] = by_ns
    end
  end
  btv._extmarks = marks
  -- Positions only: `[ns, id, row, col, end_row, end_col]` per mark, flat, with
  -- `-1` for a mark with no end. One table per buffer, no per-mark allocation.
  for bufnr, flat in pairs(positions or {}) do
    local by_ns = marks[bufnr]
    if by_ns then
      for i = 1, #flat, 6 do
        local ns_marks = by_ns[flat[i]]
        local m = ns_marks and ns_marks[flat[i + 1]]
        if m then
          m.row, m.col = flat[i + 2], flat[i + 3]
          if flat[i + 4] >= 0 then
            m.end_row, m.end_col = flat[i + 4], flat[i + 5]
          end
        end
      end
    end
  end
end

-- `btv._call_ctx_lock`: set while inside an `nvim_buf_call` / `nvim_win_call` whose
-- target differs from the real current buffer/window (see those functions). bemtvi
-- runs the callback in-VM with the "current" mirror swapped, so READS resolve to
-- the target and explicit-handle WRITES queue the right handle — but a mutation
-- that binds to "current" only at DRAIN time (an ex-command, feedkeys, an LSP buf
-- request) would run against the REAL current, which the call never switched.
-- Those funnels call `btv._assert_call_ctx` to fail loud rather than silently
-- mutate the wrong context (the no-silent-stub rule applied to a known gap).
btv._call_ctx_lock = false

-- The lock may be `true` (an `btv.win.call` / `btv.buf.call` the author wrote) or a string
-- naming the scope that installed it — a per-window event fire, say, which the handler's
-- author never asked for and would not recognise as a "call".
function btv._assert_call_ctx(what)
  local lock = btv._call_ctx_lock
  if lock then
    error(
      (type(lock) == "string" and lock or "nvim_buf_call/nvim_win_call")
        .. ": "
        .. what
        .. " here would run against the real current buffer/window, not the one this "
        .. "callback is scoped to — bemtvi cannot retarget a queued mutation. Run it "
        .. "outside, or use an explicit-handle API (btv.wo[win], btv.bo[buf], "
        .. "btv.win.set_cursor, …).",
      0
    )
  end
end

-- Wrap the context-binding LSP / diagnostic bridges (Rust funnels that drain
-- against the current buffer/window) so they honor the call-context lock. Done
-- here, before lsp.lua defines the `btv.lsp.*` verb wrappers that route through
-- them, so every entry is covered at the single chokepoint. (These native
-- bridges are `btv._*` — installed from Rust before the prelude loads — so they
-- exist on `btv` to be wrapped.)
do
  local guards = {
    -- The `btv.*` funnels themselves, not just their `vim.*` aliases: `btv.cmd` is the
    -- ex-command entry the prime-directive API points at, and `btv._feedkeys` queues keys
    -- the editor replays against whatever is current when it drains. Guarding only the
    -- aliases had it backwards — `vim.cmd("…")` raised inside a call while the `btv.cmd`
    -- it wraps went through and mutated the focused window. It matters beyond an explicit
    -- `btv.win.call`: a `BufWinEnter` handler runs in the context of the window that
    -- *displayed*, which for a session restore filling background windows is not the
    -- focused one, and its author never wrote a `call` at all.
    cmd = "an ex-command (btv.cmd)",
    _feedkeys = "feedkeys",
    _lsp_buf = "an LSP buf request",
    _lsp_buf_format = "btv.lsp.format",
    _lsp_buf_code_action = "btv.lsp.code_action",
    _lsp_buf_rename = "btv.lsp.rename",
    _diagnostic_goto = "a diagnostic jump",
    _diagnostic_setloclist = "btv.diagnostic.setloclist",
  }
  -- The `<expr>` textlock rides the same chokepoint, for the same reason the call-context
  -- lock does: an `<expr>` mapping RHS must *compute* keys, not change the editor, and
  -- `vim.cmd` has raised `E5555` on that since the sandbox landed while the `btv.cmd` it
  -- wraps queued a command the server then silently discarded — so the canonical spelling
  -- was the one that failed quietly, and the mapping went on to feed its keys as if
  -- nothing had happened. Only the ex funnel is listed: feedkeys deliberately relies on
  -- the server's post-fire discard instead (see `expr_map_discards_queued_effects`).
  local expr_blocked = { cmd = "btv.cmd" }
  for name, what in pairs(guards) do
    local raw = btv[name]
    if raw then
      local blocked = expr_blocked[name]
      btv[name] = function(...)
        if blocked and btv._expr_lock then
          error(
            "E5555: <expr> mapping must not change the editor (" .. blocked .. " is blocked)",
            0
          )
        end
        btv._assert_call_ctx(what)
        return raw(...)
      end
    end
  end
  -- `nvim_command` is the `vim.api` alias of `btv.cmd` (the Rust-installed ex-command
  -- funnel, `vim.cmd`'s sibling), installed here rather than aliased bare in the block
  -- at the end of the file so it picks up the guarded `btv.cmd` above — one assertion,
  -- from the funnel, however it was reached.
  vim.api.nvim_command = btv.cmd
end

-- Rust→Lua mirror of the core highlight registry, refreshed by the server
-- (`btv._set_hl_mirror`) when the registry changes. Keyed by group name ->
-- { fg, bg, sp (0xRRGGBB ints), bold/italic/… (true when set), link (string) }.
-- Backs `vim.api.nvim_get_hl`; a link group carries only `link` (its own attrs are
-- ignored, matching neovim), and `nvim_get_hl` follows the chain for the resolved
-- form. Seeded empty so a read before the first push answers `{}` (no theme yet).
btv._hl_defs = btv._hl_defs or {}
function btv._set_hl_mirror(entries)
  btv._hl_defs = entries or {}
end

-- Per-namespace mirror for non-zero namespaces: `btv._hl_defs_ns`[ns][name] =
-- def. Kept separate from the global `btv._hl_defs` so `nvim_set_hl`(ns, …) never
-- clobbers a colorscheme's global group; `nvim_get_hl`(ns, …) reads it. Refreshed
-- by the server (`btv._set_hl_mirror_ns`) under the same generation gate as the
-- global push.
btv._hl_defs_ns = btv._hl_defs_ns or {}
function btv._set_hl_mirror_ns(by_ns)
  btv._hl_defs_ns = by_ns or {}
end

function btv._set_buf_mirror(entries, row, col, win, wins, cur_wins, next_win, mode, cmdtype)
  btv._cur_mode = mode or "n"
  btv._cur_cmdtype = cmdtype or ""
  -- A buffer arrives in one of three shapes. `lines` present is a full push (a
  -- buffer Lua has not seen, a whole-rope replacement, an unfoldable edit batch).
  -- `delta` present is the incremental push: splice its rows into the array we
  -- already hold, so an edit costs the rows that changed rather than the whole
  -- buffer. Neither present means the text did not change (the cheap
  -- cursor-moved-no-edit path) and the prior array carries over untouched.
  for bufnr, entry in pairs(entries) do
    if entry.lines == nil then
      local prev = btv._bufs[bufnr]
      local lines = prev and prev.lines
      local delta = entry.delta
      if delta then
        if not lines then
          error(
            ("btv._set_buf_mirror: buffer %s got a line delta with no mirror to splice it into"):format(
              tostring(bufnr)
            )
          )
        end
        -- `start`/`old_end` are 0-based and end-exclusive; the array is 1-based.
        local n = #lines
        local start, old_end = delta.start, delta.old_end
        local added, removed = #delta.lines, old_end - start
        if added ~= removed then
          -- Shift the untouched tail into place first (`table.move` handles the
          -- overlap in both directions), then clear what a shrink left stranded
          -- past the new end.
          table.move(lines, old_end + 1, n, start + added + 1)
          for i = n + added - removed + 1, n do
            lines[i] = nil
          end
        end
        for i = 1, added do
          lines[start + i] = delta.lines[i]
        end
        entry.delta = nil
      end
      entry.lines = lines
    end
    entry.loaded = true
  end
  btv._bufs = entries
  btv._cur_cursor = { row = row or 1, col = col or 0 }
  btv._cur_win = win or 1000
  -- The window snapshot (Phase 5): `btv._wins[id]` = per-window record for every
  -- window in *every* tab, and `btv._win_all` that whole set in the order
  -- `nvim_list_wins` returns. `btv._win_order` is the narrower current-tab layout
  -- order the window-*number* surface (`winnr()` / `win_getid()`) counts, since a
  -- window number is per-tab. `btv._next_win` is the id the next `nvim_open_win`
  -- will get, so it can return synchronously.
  local by_id, order = {}, {}
  for _, w in ipairs(wins or {}) do
    local prev = btv._wins[w.id]
    -- The jumplist is gated on its generation: the server omits `jumps` unless
    -- the list moved since the last push, and an unchanged row carries the old
    -- list over — a repaint never re-serializes a whole jumplist (`jump_idx` is
    -- always fresh).
    if prev and w.jump_gen == prev.jump_gen then
      w.jumps = prev.jumps
    end
    by_id[w.id] = w
    order[#order + 1] = w.id
  end
  btv._wins = by_id
  btv._win_all = order
  btv._win_order = cur_wins or order
  btv._next_win = next_win or btv._next_win
end

-- Receive the tab mirror (Phase 3): `tabs` is the tabline-ordered array the
-- server pushed, `cur` the active tab id. Keyed by id into `btv._tabs` with the
-- order kept in `btv._tab_order`, mirroring the window mirror's shape.
function btv._set_tab_mirror(tabs, cur)
  local by_id, order = {}, {}
  for _, t in ipairs(tabs or {}) do
    by_id[t.id] = t
    order[#order + 1] = t.id
  end
  btv._tabs = by_id
  btv._tab_order = order
  btv._cur_tab = cur or 1
end

-- Resolve a buffer handle to a concrete bufnr (0 / nil -> current buffer), the
-- one place the buffer-read API maps neovim's "0 means current" convention.
function btv._resolve_bufnr(bufnr)
  if bufnr == nil or bufnr == 0 then
    return (btv._cur_buf or {}).bufnr or 0
  end
  return bufnr
end

-- Normalize a neovim line index against a buffer of `n` real lines, used by the
-- buffer read range API (`nvim_buf_get_lines` / `nvim_buf_get_text`): negatives count
-- from the end (`-1` == one past the last line), then clamp into [0, n]. `strict`
-- raises on an out-of-range index instead of clamping (neovim's strict_indexing).
function btv._norm_line_index(i, n, strict)
  local orig = i
  if i < 0 then
    i = n + i + 1
  end
  if strict and (orig > n or i < 0) then
    error("Index out of bounds", 3)
  end
  if i < 0 then
    i = 0
  elseif i > n then
    i = n
  end
  return i
end

-- Current-window resolution (`0`/`nil` -> the current window). Exposed on the
-- global `vim` table — mirroring `btv._resolve_bufnr` — so the `btv.wo` machinery,
-- authored earlier in this file, can share it; the local
-- alias keeps this file's many call sites terse.
function btv._resolve_win(win)
  if win == nil or win == 0 then
    return btv._cur_win or 1000
  end
  return win
end
local resolve_win = btv._resolve_win

-- Which tier an `nvim_{set,get}_option_value` call targets, from its `opts`. neovim's
-- `scope` is `"local"` (the window/buffer value) or `"global"` (the `:setglobal` tier);
-- an unrecognized value fails loud instead of being dropped, since silently treating
-- `scope = "gloabl"` as local reads the wrong number and looks like it worked.
--
-- With no `scope`, the two verbs differ, as in neovim: a *set* matches `:set` — "for
-- global-local options, both the global and local value are set" — while a *get* reads
-- the local value. A `buf` / `win` target narrows a set to that instance (neovim: `buf`
-- implies `scope` is local), so only the untargeted, unscoped set writes both tiers.
-- Returns `"global"`, `"local"`, or `"both"`.
local function opt_scope_of(opts, where, setting)
  local scope = opts.scope
  if scope == "global" then
    return "global"
  end
  if scope == "local" then
    return "local"
  end
  if scope ~= nil then
    error(where .. ": invalid scope '" .. tostring(scope) .. "' (expected 'local' or 'global')", 2)
  end
  if setting and not opts.buf and not opts.win then
    return "both"
  end
  return "local"
end

-- `btv.option.set`(name, value, opts) [alias `nvim_set_option_value`]: set an option
-- in the scope its name implies. A window-local option (number/relativenumber) —
-- or any option with an explicit `opts.win` — routes through `btv.wo` (the targeted
-- window, else the current one); otherwise it routes through `btv.bo` (`opts.buf`,
-- else the current buffer). The wired options reach the core; everything else
-- lands in the observable per-scope store. (The scoped tables `btv.o` / `btv.bo` / `btv.wo`
-- are the primary option API; this is the by-name funnel plugins reach for.)
--
-- `opts.scope` is neovim's local/global selector over the two tiers a buffer- or
-- window-local option carries: `"local"` writes only the targeted instance
-- (`:setlocal`), `"global"` only the value a new buffer is born from (`:setglobal`).
-- Any other value fails loud. With **no** `opts.scope` and no `opts.buf` / `opts.win`,
-- this is a plain `:set` and writes **both** — matching neovim, and the reason
-- `nvim_set_option_value("tabstop", 3, {})` in a config reaches the files you open
-- afterwards rather than only the buffer that was current while it ran. Naming a
-- `buf` / `win` narrows it back to that instance.
function btv.option.set(name, value, opts)
  opts = opts or {}
  local scope = opt_scope_of(opts, "nvim_set_option_value", true)
  if scope == "global" then
    btv.go[name] = value
    return
  end
  if scope == "both" then
    -- `btv.o` is the both-tiers funnel: it forwards to the window/buffer scope the
    -- name implies AND moves that option's global value, exactly as `:set` does.
    btv.o[name] = value
    return
  end
  if opts.win or btv._win_opt_canon[name] then
    vim.wo[opts.win and resolve_win(opts.win) or resolve_win(0)][name] = value
    return
  end
  local buf = opts.buf and btv._resolve_bufnr(opts.buf) or btv._resolve_bufnr(0)
  vim.bo[buf][name] = value
end

-- `btv.option.get`(name, opts) [alias `nvim_get_option_value`]: read an option from
-- the scope its name implies (see `btv.option.set`), so a wired option reflects the
-- core's current value (default until set). `opts.scope = "global"` reads the
-- `:setglobal` tier instead; a *read* with no scope is the local value (neovim's
-- default for the getter, where the setter's is `:set`).
function btv.option.get(name, opts)
  opts = opts or {}
  if opt_scope_of(opts, "nvim_get_option_value", false) == "global" then
    return btv.go[name]
  end
  if opts.win or btv._win_opt_canon[name] then
    return vim.wo[opts.win and resolve_win(opts.win) or resolve_win(0)][name]
  end
  local buf = opts.buf and btv._resolve_bufnr(opts.buf) or btv._resolve_bufnr(0)
  return vim.bo[buf][name]
end

vim.api.nvim_set_option_value = btv.option.set
vim.api.nvim_get_option_value = btv.option.get

-- `btv.set_option` / `btv.get_option`(name[, value]) [aliases `nvim_set_option` /
-- `nvim_get_option`]: the global-scope (deprecated) accessors. Route through `btv.o`,
-- the canonical global-option table that canonicalizes the scope the name implies.
function btv.set_option(name, value)
  btv.o[name] = value
end
function btv.get_option(name)
  return btv.o[name]
end
api.nvim_set_option = btv.set_option
api.nvim_get_option = btv.get_option

-- `btv.env.get`(name) [alias `vim.fn.getenv`]: an environment variable's value, or
-- v:null (`vim.NIL`) when unset — matching neovim, which returns v:null rather than an
-- empty string so a caller can distinguish `""` from absent. A name set via `btv.env.set`
-- this session is read back from the shadow store first.
btv._env_shadow = btv._env_shadow or {}
btv.env = btv.env or {}
function btv.env.get(name)
  local v = btv._env_shadow[name]
  if v ~= nil then
    return v
  end
  v = os.getenv(name)
  if v == nil then
    return vim.NIL
  end
  return v
end
vim.fn.getenv = btv.env.get

-- `btv.env.set`(name, value) [alias `vim.fn.setenv`]: set an environment variable for
-- this session. bemtvi can't mutate the real process environment from Lua, so the
-- value lands in a shadow store getenv/`vim.env` read back — observable within the
-- editor, which is what a plugin setting e.g. $GIT_DIR before spawning an in-process
-- child expects. A nil/v:null value unsets it.
function btv.env.set(name, value)
  if value == nil or value == vim.NIL then
    btv._env_shadow[name] = nil
  else
    btv._env_shadow[name] = tostring(value)
  end
end
vim.fn.setenv = btv.env.set

-- The read-only special registers `vim.fn.setreg` refuses to write: search `/`,
-- last-insert `.`, filename `%`, last-command `:`, expression `=`, alternate `#`.
-- bemtvi can't honor a write to these (their value projects from live editor
-- state), so it errors loud rather than storing a cell that the read path would
-- silently shadow.
local SETREG_READONLY = {
  ["/"] = true,
  ["."] = true,
  ["%"] = true,
  [":"] = true,
  ["="] = true,
  ["#"] = true,
}

-- `btv.reg.set`(name, value [, options]) [alias `vim.fn.setreg`]: write a register.
-- `name` `""` / `"@"` means the unnamed register `"`. `value` is a string (charwise) or
-- a list of strings (one per line, linewise). `options` is a string of flags: c/v
-- charwise, l/V linewise, a/A append; b / <C-v> (blockwise) is rejected (no
-- visual-block mode yet). An uppercase register name also appends. A string ending
-- in a newline is linewise when no type flag forces otherwise. Returns 0 on success
-- (1 is vim's failure code, but the failure cases here raise instead). The write is
-- queued for the server (`btv._set_reg`) and write-through the mirror so a getreg
-- later in the same chunk is consistent.
function btv.reg.set(name, value, options)
  name = tostring(name)
  if name == "" or name == "@" then
    name = '"'
  end
  local reg = name:sub(1, 1)
  if SETREG_READONLY[reg] then
    error("E354: Invalid register name: '" .. reg .. "'")
  end

  local linewise, append = false, false
  local text
  if type(value) == "table" then
    -- A list is linewise: each item is a line, with a trailing newline so the
    -- last item is a whole line too.
    text = table.concat(value, "\n")
    if #value > 0 then
      text = text .. "\n"
    end
    linewise = true
  else
    text = tostring(value)
  end

  local opts = options and tostring(options) or ""
  local type_given = false
  for i = 1, #opts do
    local ch = opts:sub(i, i)
    if ch == "a" or ch == "A" then
      append = true
    elseif ch == "l" or ch == "V" then
      linewise, type_given = true, true
    elseif ch == "c" or ch == "v" then
      linewise, type_given = false, true
    elseif ch == "b" or ch == "\22" then
      error("vim.fn.setreg: blockwise registers are not supported yet")
    end
  end
  -- An uppercase register name appends to its lowercase store.
  if reg:match("%u") then
    append = true
  end
  -- A trailing newline on a plain string makes it linewise (vim), unless a flag
  -- already decided the type.
  if type(value) ~= "table" and not type_given and text:sub(-1) == "\n" then
    linewise = true
  end

  local lower = reg:lower()
  btv._registers = btv._registers or {}
  local t = linewise and "V" or "v"
  if append and btv._registers[lower] then
    local prev = btv._registers[lower]
    btv._registers[lower] = {
      text = prev.text .. text,
      type = (prev.type == "V" or linewise) and "V" or "v",
    }
  else
    btv._registers[lower] = { text = text, type = t }
  end
  btv._set_reg(lower, text, linewise, append)
  return 0
end
vim.fn.setreg = btv.reg.set

-- `btv.reg.get`(name [, ...]) [alias `vim.fn.getreg`]: the text stored in register `name`
-- (`""` / `"@"` / nil = the unnamed register), or `""` when the register is empty / unset.
-- Reads the `btv._registers` mirror the server refreshes before this chunk; an
-- uppercase name reads its lowercase store, matching vim.
function btv.reg.get(name)
  name = tostring(name or '"')
  if name == "" or name == "@" then
    name = '"'
  end
  local reg = name:sub(1, 1):lower()
  local entry = (btv._registers or {})[reg]
  return entry and entry.text or ""
end
vim.fn.getreg = btv.reg.get

-- `btv.reg.gettype`(name) [alias `vim.fn.getregtype`]: `"v"` (charwise), `"V"` (linewise), or
-- `""` for an unknown register. An empty / unset (but valid) register is charwise ->
-- `"v"`, matching vim. Blockwise (`"<C-v>{width}"`) waits on visual-block mode.
function btv.reg.gettype(name)
  name = tostring(name or '"')
  if name == "" or name == "@" then
    name = '"'
  end
  local reg = name:sub(1, 1):lower()
  local entry = (btv._registers or {})[reg]
  return entry and entry.type or "v"
end
vim.fn.getregtype = btv.reg.gettype

-- `btv.macro`: the keyboard macros `<F2>{reg}` records and `<F3>{reg}` plays back.
-- A macro is an ordinary register holding bemtvi key notation (`ciwfoo<Esc>`), so
-- `btv.reg.get`/`btv.reg.set` read and write one like any other register — these
-- verbs are for the state the register cannot carry.
btv.macro = btv.macro or {}

-- `btv.macro.recording`() [alias `vim.fn.reg_recording`]: the name of the register a
-- macro is being recorded into right now, or `nil` when nothing is recording. The
-- `vim.fn` alias returns `""` instead of `nil`, matching vim.
function btv.macro.recording()
  return btv._macro_state.recording
end

-- `btv.macro.executing`() [alias `vim.fn.reg_executing`]: the name of the register
-- whose macro is playing back right now, or `nil` when none is. Useful to skip work
-- that only matters to a human watching — a plugin can cheaply no-op while a long
-- `100<F3>a` runs. Nested playback reports the INNERMOST register.
function btv.macro.executing()
  return btv._macro_state.executing
end

-- `btv.macro.play`(name [, count]): play register `name` back as keys, `count` times
-- (default 1) — the `<F3>{name}` command from Lua. The keys are queued and run after
-- the current chunk settles, like any other key feed; the call returns immediately.
function btv.macro.play(name, count)
  name = tostring(name or "")
  if #name ~= 1 then
    error("btv.macro.play: expected a single-character register name, got " .. vim.inspect(name))
  end
  local keys = btv.reg.get(name)
  if keys == "" then
    return
  end
  for _ = 1, math.max(1, tonumber(count) or 1) do
    btv._feedkeys(keys, true, false)
  end
end

vim.fn.reg_recording = function()
  return btv.macro.recording() or ""
end
vim.fn.reg_executing = function()
  return btv.macro.executing() or ""
end

-- ---------------------------------------------------------------------------
-- The option catalog, and the routing tables derived from it.
-- ---------------------------------------------------------------------------

--- Receive core's option catalog and rebuild every table that routes an option name to
--- its scope. Called once by the server (`LuaRuntime::set_options_catalog`) before any
--- config runs, from `bemtvi_core::options::options_catalog()` — the same list `:set`
--- resolves against, so the Lua surfaces can no longer disagree with the ex ones about
--- where an option lives or whether it has a global value.
---
--- Each row is `{ name, abbrev, kind, scope, global_tier, doc }`. Four tables come out:
---
--- ```
--- O_WIN / O_BUF         -- which scope `vim.o` / `vim.opt` forwards a write to
--- WO_GLOBAL_TIER        -- the window options `vim.go` / `vim.opt_global` reach
--- BO_GLOBAL_TIER        -- the buffer options `vim.go` / `vim.opt_global` reach
--- ```
---
--- `WIN_OPT_CANON` / `BUF_OPT_CANON` (the `vim.wo` / `vim.bo` name maps) are extended
--- with any catalog name they lack, for the same reason. They keep their own entries the
--- catalog has no row for — the read-only buffer *state* nouns (`modified`, `buftype`)
--- and `'winhighlight'`.
---
--- `O_GLOBAL` wins the overlap: `'regexsyntax'` is global-local (a buffer may pin a
--- dialect, or follow the editor-wide one), and `vim.o.regexsyntax` has always meant the
--- editor-wide value — the only way to move it. `vim.bo.regexsyntax` is the per-buffer
--- surface.
function btv._set_options_catalog(rows)
  btv._options_catalog = rows or {}
  for _, r in ipairs(btv._options_catalog) do
    local spellings = { r.name }
    if r.abbrev then
      spellings[#spellings + 1] = r.abbrev
    end
    for _, spelling in ipairs(spellings) do
      if not O_GLOBAL[spelling] then
        if r.scope == "window" then
          O_WIN[spelling] = true
          if WIN_OPT_CANON[spelling] == nil then
            WIN_OPT_CANON[spelling] = r.name
          end
          if r.global_tier then
            WO_GLOBAL_TIER[spelling] = r.name
          end
        elseif r.scope == "buffer" then
          O_BUF[spelling] = true
          if BUF_OPT_CANON[spelling] == nil then
            BUF_OPT_CANON[spelling] = r.name
          end
        end
      end
    end
    if r.scope == "buffer" and r.global_tier and not O_GLOBAL[r.name] then
      BO_GLOBAL_TIER[r.name] = true
    end
  end
end
