-- nxvim Lua prelude — process / JSON / version.
-- The vim.system object wrapper and vim.json over the Rust primitives, the vim.version surface, and the misc vim.* configs reach for.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- vim.system / vim.json -------------------------------------------------

-- vim.system(cmd, opts, on_exit): run `cmd` (an argv list) and return a handle.
-- `opts` may carry `cwd`, `env` (a {VAR=value} dict layered on the inherited
-- environment), and `text` (accepted; output is always returned as a string).
--
-- Two modes, split on whether an `on_exit` is given (the pragmatic
-- approximation of neovim's loop-pumping `:wait()`, which a single thread can't
-- replicate; see docs/plans/2026-06-06-async-lua-runtime.md):
--   * `on_exit` given  → ASYNC. The child runs in the event-loop actor (off the
--     server thread); `on_exit` fires on a later tick with { code, stdout, stderr }.
--     The handle exposes a real `pid` (filled once the spawn lands) and a working
--     `kill`. `:wait()` is unavailable on this handle (it would need to pump the
--     loop) and raises, pointing the caller at the synchronous form.
--   * no `on_exit`     → SYNCHRONOUS. The child runs to completion inline and
--     `:wait()` returns the already-complete result. This is what an
--     `lsp/<server>.lua` `root_dir` that shells out (rust_analyzer's `cargo
--     metadata` / `rustc --print sysroot`) needs — short, blocking, resolved
--     during `vim.lsp.enable`.
function vim.system(cmd, opts, on_exit)
  if type(opts) == "function" then
    on_exit, opts = opts, nil
  end
  opts = opts or {}
  if on_exit then
    local id = vim._next_cb_id()
    vim._cb_fns[id] = on_exit
    vim._system_async(id, cmd, opts.cwd, opts.env)
    return setmetatable({}, {
      __index = function(_, key)
        if key == "pid" then
          return vim._proc_pids[id]
        elseif key == "kill" then
          return function(_, signal) vim._system_kill(id, signal) end
        elseif key == "wait" then
          return function()
            error(
              "nxvim: vim.system():wait() is unavailable on a handle spawned "
                .. "with on_exit; call vim.system without on_exit for a synchronous result",
              2
            )
          end
        end
        return nil
      end,
    })
  end
  local result = vim._system(cmd, opts.cwd, opts.env, opts.text ~= false)
  return setmetatable({ pid = result.pid }, {
    __index = {
      wait = function() return result end,
      kill = function() end, -- already exited; nothing to signal
    },
  })
end

-- vim.json.encode/decode: JSON (de)serialization, backed by the Rust serde_json
-- bridge. `decode` maps objects to string-keyed tables, arrays to sequences, and
-- `null` to nil; `encode` treats a `1..n` table as an array and any other as an
-- object. `decode` raises on malformed input (neovim parity).
vim.json = vim.json or {}
function vim.json.encode(value) return vim._json_encode(value) end
function vim.json.decode(str, _opts) return vim._json_decode(str) end

-- ----- misc vim.* the configs reach for --------------------------------------

-- vim.NIL: the sentinel for JSON null (a value that survives table storage where
-- a literal nil would simply drop the key). Configs store it in init_options /
-- capabilities; nxvim doesn't yet forward those to a server, so it only needs to
-- be a distinct, stringifiable value. `vim.json.encode` maps it to JSON null.
vim.NIL = setmetatable({}, { __tostring = function() return "vim.NIL" end })

-- vim.empty_dict(): a fresh table that JSON-encodes as `{}` (an object), never
-- `[]`. nxvim's encoder already emits `{}` for an empty table, so a plain table
-- suffices.
function vim.empty_dict() return {} end

-- vim.trim(s): `s` with leading/trailing whitespace removed.
function vim.trim(s) return (tostring(s):gsub("^%s+", ""):gsub("%s+$", "")) end

-- vim.islist(t): true iff `t` is a list (a table whose keys are exactly 1..#t).
function vim.islist(t)
  if type(t) ~= "table" then return false end
  local n = 0
  for _ in pairs(t) do
    n = n + 1
  end
  return n == #t
end
vim.tbl_islist = vim.islist -- the pre-0.10 name

-- vim.version: callable (returns nxvim's emulated neovim version, stringifiable
-- as "0.11.0" — configs report it to the server) and a table of semver helpers.
-- nxvim targets the neovim 0.11 Lua surface, so that is what it reports.
local NVIM_VERSION = { major = 0, minor = 11, patch = 0 }
local function version_tbl(t)
  return setmetatable(t, {
    __tostring = function(v) return v.major .. "." .. v.minor .. "." .. v.patch end,
  })
end
vim.version = setmetatable({
  -- vim.version.parse("1.2.3"): a {major,minor,patch} table, or nil.
  parse = function(s)
    local a, b, c = tostring(s):match("v?(%d+)%.(%d+)%.?(%d*)")
    if not a then return nil end
    return version_tbl({ major = tonumber(a), minor = tonumber(b), patch = tonumber(c) or 0 })
  end,
  -- vim.version.cmp(a,b): -1 / 0 / 1. Accepts version tables or "x.y.z" strings.
  cmp = function(a, b)
    if type(a) == "string" then a = vim.version.parse(a) end
    if type(b) == "string" then b = vim.version.parse(b) end
    for _, k in ipairs({ "major", "minor", "patch" }) do
      if (a[k] or 0) ~= (b[k] or 0) then return (a[k] or 0) < (b[k] or 0) and -1 or 1 end
    end
    return 0
  end,
}, {
  __call = function()
    return version_tbl({
      major = NVIM_VERSION.major,
      minor = NVIM_VERSION.minor,
      patch = NVIM_VERSION.patch,
    })
  end,
})
vim.version.lt = function(a, b) return vim.version.cmp(a, b) < 0 end
vim.version.gt = function(a, b) return vim.version.cmp(a, b) > 0 end
vim.version.eq = function(a, b) return vim.version.cmp(a, b) == 0 end
vim.version.ge = function(a, b) return vim.version.cmp(a, b) >= 0 end
vim.version.le = function(a, b) return vim.version.cmp(a, b) <= 0 end
