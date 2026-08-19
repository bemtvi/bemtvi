-- bemtvi Lua prelude — core standard library (pure helpers).
-- LuaJIT-compatible bit ops and the btv.tbl / btv.list / btv.str / btv.iter helpers
-- (with their vim.* aliases). No editor state lives here — the variable/option/
-- register stores moved to prelude/state.lua. Loaded first of the prelude chunks
-- by `LuaRuntime::new` (see runtime.rs).
local vim = vim
btv = btv or {}

-- ----- bit: LuaJIT-compatible bit ops on PUC Lua 5.4 ------------------------

-- neovim runs LuaJIT, which ships a global `bit` library; bemtvi runs PUC Lua
-- 5.4, which has native bitwise *operators* but no `bit` *table* (nor 5.2's
-- `bit32`). Plugins reach for it as `bit or bit32` (catppuccin hashes its config
-- with djb2 + xor), so provide a faithful pure-Lua implementation with LuaJIT's
-- 32-bit two's-complement semantics: results are normalized to the signed
-- [-2^31, 2^31) range and shift counts are taken mod 32. Only installed when
-- absent (always, on PUC).
if not bit then
  local POW = {}
  for i = 0, 32 do
    POW[i] = 2 ^ i
  end
  local M32 = POW[32]

  -- Wrap to the unsigned 32-bit range [0, 2^32).
  local function u32(x)
    return x % M32
  end
  -- Wrap to LuaJIT's signed 32-bit result range.
  local function tobit(x)
    x = u32(x)
    if x >= POW[31] then
      x = x - M32
    end
    return x
  end

  -- Apply `f` (operating on single bits) across all 32 bit positions.
  local function bitwise(a, b, f)
    a, b = u32(a), u32(b)
    local r = 0
    for i = 0, 31 do
      local abit, bbit = a % 2, b % 2
      if f(abit, bbit) == 1 then
        r = r + POW[i]
      end
      a, b = (a - abit) / 2, (b - bbit) / 2
    end
    return tobit(r)
  end

  bit = {
    tobit = tobit,
    band = function(a, b)
      return bitwise(a, b, function(x, y)
        return x * y
      end)
    end,
    bor = function(a, b)
      return bitwise(a, b, function(x, y)
        return (x + y > 0) and 1 or 0
      end)
    end,
    bxor = function(a, b)
      return bitwise(a, b, function(x, y)
        return (x ~= y) and 1 or 0
      end)
    end,
    bnot = function(a)
      return tobit(-1 - u32(a))
    end,
    lshift = function(a, n)
      return tobit(u32(a) * POW[n % 32])
    end,
    rshift = function(a, n)
      return tobit(math.floor(u32(a) / POW[n % 32]))
    end,
    arshift = function(a, n)
      return tobit(math.floor(tobit(a) / POW[n % 32]))
    end,
  }
end

-- `btv.str.*` string helpers (aliases `vim.fn.trim` / `str2list` / `nr2char` / `strchars` /
-- `strdisplaywidth` / `strcharpart` / `strtrans`). `btv.str.trim(text[, mask[, dir]])`:
-- strip the characters in `mask` (default the whitespace set) from `text`. `dir` 0
-- trims both ends (default), 1 leading only, 2 trailing only. `mask` is a *set* of
-- characters, not a pattern. nvim-dap-python trims command output through this.
btv.str = btv.str or {}
function btv.str.trim(text, mask, dir)
  text = tostring(text or "")
  if mask == nil or mask == "" then
    mask = " \t\n\r\f\v"
  end
  dir = dir or 0
  local set = {}
  for i = 1, #mask do
    set[mask:sub(i, i)] = true
  end
  local from, to = 1, #text
  if dir == 0 or dir == 1 then
    while from <= to and set[text:sub(from, from)] do
      from = from + 1
    end
  end
  if dir == 0 or dir == 2 then
    while to >= from and set[text:sub(to, to)] do
      to = to - 1
    end
  end
  return text:sub(from, to)
end
vim.fn.trim = btv.str.trim

-- ----- JSON -----------------------------------------------------------------

-- `btv.json` is the public JSON codec, wrapping the native `btv._json_*` bridges
-- (the same array-vs-object rule the msgpack path uses). `vim.json` aliases it for
-- muscle memory + neovim-plugin compat. Exposed here so no plugin re-implements a
-- JSON encoder of its own.
btv.json = btv.json or {}

-- `btv.json.encode(value[, opts]) -> string`. `opts.pretty` (default false) emits a
-- 2-space-indented, multi-line document for human-readable / diff-friendly files;
-- omit it for the compact one-liner.
function btv.json.encode(value, opts)
  return btv._json_encode(value, opts)
end

-- `btv.json.decode(str) -> value`. Parses a JSON document (objects -> string-keyed
-- tables, arrays -> sequences, `null` -> nil); raises on malformed input.
--
-- An EMPTY object decodes to `btv.json.empty_object()` rather than a bare `{}`, so
-- decode -> edit -> encode (how a plugin rewrites a JSON file it owns) preserves what
-- the document said: without the mark `{"pylsp":{}}` came back out as `{"pylsp":[]}`,
-- since a bare empty table is equally an empty array. It reads like any other empty
-- table (`next(t) == nil`, fill it in and it is an object with those keys); the one
-- place the mark shows is `btv.tbl.deep_extend`, where — like every JSON sentinel — it
-- is a VALUE that replaces, so a merged-in `{}` means `{}`.
function btv.json.decode(str)
  return btv._json_decode(str)
end

-- Which JSON sentinel `v` is — `"null"`, `"object"`, or nil for any other value. The
-- mark lives in the metatable (the Rust codec reads the same `__bemtvi_json` field), so
-- it survives `btv.tbl.deepcopy`, which preserves metatables. Shared by `btv.json.is_null`
-- and by `deep_extend`'s merge rule further down this file, which must treat a marked
-- table as a LEAF rather than as the empty map it otherwise looks like.
local function json_mark(v)
  if type(v) ~= "table" then
    return nil
  end
  local mt = getmetatable(v)
  return mt and rawget(mt, "__bemtvi_json") or nil
end

-- `btv.json.null`: the value that encodes as JSON `null`.
--
-- Lua has no way to *say* null: storing `nil` under a key simply removes the key, so
-- `{ token = nil }` encodes as `{}`, not `{"token": null}`. To a protocol peer those
-- are different messages — "unset, use your default" versus "explicitly nothing" — and
-- LSP servers do read them differently (`snyk_ls` expects a null token when there is
-- none). Put this in the table instead:
--
-- ```lua
-- init_options = { token = btv.env.get("SNYK_TOKEN") or btv.json.null }
-- ```
--
-- Identify it on the way back out with `btv.json.is_null(value)`, **not** `value ==
-- btv.json.null`: every path that stores it copies it (`btv.tbl.deepcopy`, and so the
-- `btv.tbl.deep_extend` behind `btv.lsp.config`), and a copy carries the mark but is a
-- different table.
--
-- `btv.json.decode` still turns an incoming `null` into `nil` (there is nowhere else for
-- it to go in a Lua table), so this is a write-side value.
btv.json.null = setmetatable({}, {
  __bemtvi_json = "null",
  __tostring = function()
    return "btv.json.null"
  end,
})

-- `btv.json.empty_object()` -> a table that encodes as JSON `{}` rather than `[]`.
--
-- The other thing Lua can't say: an empty table is both an empty array and an empty
-- object, and bemtvi's codec — like neovim's — has to pick one, so it picks `[]`. A
-- server given `[]` where its schema says object either rejects the message or
-- silently ignores the field, and the resulting "the server started but does nothing"
-- is exactly the kind of quiet wrongness worth a sentinel.
--
-- ```lua
-- init_options = { memory = { file_store = btv.json.empty_object() } }
-- ```
--
-- A fresh table each call, and the mark answers "array or object?" rather than
-- standing in for the contents: fill it in afterwards and it is still an object, with
-- everything you put in it (integer keys become the string keys JSON objects take).
--
-- `btv.json.decode` hands back this same shape for an empty object it read, which is
-- what makes decode -> encode round-trip a `{}` in a file rather than flattening it
-- to `[]`.
--
-- Its sibling is the value `btv.json.null`, which encodes as JSON `null` — the other
-- thing a Lua table cannot carry, since a `nil` value simply removes the key.
function btv.json.empty_object()
  return setmetatable({}, { __bemtvi_json = "object" })
end

-- `btv.json.is_null(value)` -> is `value` the JSON-null sentinel?
--
-- The read side of `btv.json.null`. It answers on a *copy* of the sentinel as well as on
-- the sentinel itself, which is what makes it the right test: a config's value has
-- almost always been through `btv.tbl.deep_extend` by the time anyone reads it back, and
-- that copies. Everything else — including `btv.json.empty_object()`, a bare `{}`, and
-- `nil` — is false.
--
-- ```lua
-- if btv.json.is_null(cfg.init_options.token) then
--   -- the user said "explicitly no token", not "I forgot to set one"
-- end
-- ```
function btv.json.is_null(value)
  return json_mark(value) == "null"
end

vim.json = btv.json

-- ----- btv.shada.plugin: opt-in isolated plugin storage ----------------------

-- `btv.shada = btv.shada or {}` — the table is created natively (namespace /
-- save_layout), but guard so this module never depends on load order.
btv.shada = btv.shada or {}

-- The caller-attribution and basename helpers live in `btv.utils`
-- (`btv.utils.caller_source` / `btv.utils.basename` — prelude/utils.lua, loaded after
-- this module but before any user code, and only ever called from user-code paths).
-- Attribution note: a bare `:lua` / RPC `exec_lua` / test chunk can be `@`-named too
-- (mlua labels it after its Rust call site, e.g. `@crates/…`), but that path is
-- under no runtimepath entry, so `assign_namespace` returns nil for it and the
-- caller treats it as a no-identity context.

-- Assign a namespace to a caller `src` by attributing it to the runtimepath entry
-- (plugin root) that contains it — the longest matching prefix wins, so a plugin dir
-- nested under a broader rtp entry attributes to the plugin, not the parent. Then,
-- in order: a plugin LOADED BY THE MANAGER (`btv.plugins`) keys on the canonical name
-- the manager registered (which a `name = …` spec can set apart from the directory
-- basename); the user's own config root maps to the reserved `user`; otherwise the
-- namespace is the directory's basename (e.g. `nvim-tree`) — the fallback for a
-- plugin loaded outside the manager. `nil` when `src` is under no runtimepath entry.
local function assign_namespace(src)
  local best
  for _, raw in ipairs(btv._runtime_paths()) do
    -- Trim trailing separators before matching: a runtimepath entry carried in with a
    -- trailing slash (e.g. `BEMTVI_CONFIG=examples/foo/`) would otherwise compare against
    -- `dir .. "/"` → a double-slash that never prefixes the `@<dir>/<file>` source path, so
    -- the calling file would attribute to NO plugin and persistence calls would raise.
    local dir = (raw:gsub("[/\\]+$", ""))
    if src == dir or src:sub(1, #dir + 1) == dir .. "/" then
      if not best or #dir > #best then
        best = dir
      end
    end
  end
  if not best then
    return nil
  end
  -- Manager-registered name wins (tightest identity).
  local managed = btv.plugins and btv.plugins._namespace_for and btv.plugins._namespace_for(best)
  if managed then
    return managed
  end
  -- The user's config root is a runtimepath entry too; map it to the reserved `user`
  -- namespace rather than its (machine-specific) directory name. Compare with trailing
  -- separators trimmed so `…/bemtvi` and `…/bemtvi/` match.
  local trim = function(p)
    return (p:gsub("[/\\]+$", ""))
  end
  local config = vim.fn and vim.fn.stdpath and vim.fn.stdpath("config")
  if config and trim(best) == trim(config) then
    return "user"
  end
  return btv.utils.basename(best)
end

-- `btv._resolve_namespace(dev_namespace, what)` -> the persistence namespace for the
-- calling context, with the `dev_namespace` escape-hatch contract: it is *assigned* from
-- the caller's plugin location (its runtimepath entry). A context that attributes to
-- nothing (a bare `:lua` / RPC `exec_lua` / test / off-rtp helper / async callback) MUST
-- pass one. A context that DOES attribute may pass one only if it **equals** the assigned
-- namespace (a redundant but harmless self-statement — useful when a framework resolves
-- the namespace once at an attributing call site and threads it explicitly through later
-- deferred/async calls); naming a *different* namespace is an error (it would break
-- isolation — a plugin can never name another's). Shared by `btv.shada.plugin()` and
-- `btv.view.create{ persist=, namespace= }` so the two persistence surfaces obey one rule.
-- `what` names the calling API in the errors; `error(_, 3)` points the blame at the user's
-- call to that API (1 = here, 2 = the API, 3 = its caller).
function btv._resolve_namespace(dev_namespace, what)
  -- Attribute the caller's source to its runtimepath entry. `nil` for a context with
  -- no attributable script (REPL / exec / test, or code outside every rtp entry).
  local src = btv.utils.caller_source()
  local assigned = src and assign_namespace(src) or nil
  if assigned then
    if dev_namespace ~= nil and dev_namespace ~= assigned then
      error(
        what
          .. ": this caller's namespace is '"
          .. assigned
          .. "' (assigned from its location); it cannot claim '"
          .. tostring(dev_namespace)
          .. "'",
        3
      )
    end
    return assigned
  end
  if type(dev_namespace) ~= "string" or dev_namespace == "" then
    error(
      what
        .. ": this caller attributes to no plugin (a bare :lua / RPC / test); "
        .. "pass an explicit namespace, or call it from a plugin file",
      3
    )
  end
  return dev_namespace
end

-- `btv.shada.plugin()` -> an isolated, cross-session key/value store for the calling
-- plugin. The handle persists into the *current* shada store (global, workspace, or
-- remote — whichever this session uses), walled off from the core registers / marks
-- / history and keyed apart from every other plugin's namespace.
--
-- The namespace is **assigned, not chosen**: it is derived from where the calling
-- code lives (its runtimepath / plugin directory), so a plugin can persist its own
-- data and can never name — and so never read or clobber — another plugin's slice.
-- Calling it from anywhere in a plugin's files resolves to that one plugin's
-- namespace. Code in the user's config maps to the reserved `user` namespace.
--
-- It is the plugin's opt-in: only a plugin that calls this gets shada storage.
-- Values may be any JSON-able Lua value (table / string / number / boolean); `get`
-- returns a fresh copy. Persistence rides the ordinary shada cadence (the debounced
-- checkpoint + the clean-exit flush); with shada disabled the store still works in
-- memory for the session but isn't written, exactly like registers and marks.
--
-- A namespace is capped at **1 MiB** (of serialized key+value bytes) so one plugin
-- can't bloat the shared store; a `set` that would cross the cap raises (the prior
-- value is left intact). Keep it to small, structured state — settings, a recent
-- list, a cursor table — not bulk data.
--
-- ```lua
-- local store = btv.shada.plugin()       -- no argument: namespace is assigned
-- store:set("recent", { "a.txt", "b.txt" })
-- local recent = store:get("recent")    -- the table back, or nil
-- store:delete("recent")
-- for _, k in ipairs(store:keys()) do … end
-- store:clear()
-- ```
--
-- `dev_namespace` is an escape hatch for a context whose code attributes to no
-- runtimepath entry — a bare `:lua`, an RPC `exec_lua`, a test, or a deferred/async
-- callback whose stack no longer carries the plugin chunk — where the namespace can't be
-- derived, so an explicit one is required.
function btv.shada.plugin(dev_namespace)
  local namespace = btv._resolve_namespace(dev_namespace, "btv.shada.plugin")
  return btv._shada_store(namespace)
end

-- `btv._shada_store(namespace)` — the store itself, with no attribution check. The
-- PRELUDE's own persistence (the picker's filter-line history, say) belongs to
-- bemtvi rather than to whoever happened to call the public API that reaches it:
-- `btv.shada.plugin` attributes to the caller, so a user config calling
-- `btv.picker.forget_history()` was refused for "claiming" the picker's namespace.
-- Not for plugin use — a plugin goes through `btv.shada.plugin`, which is exactly
-- the check this skips.
function btv._shada_store(namespace)
  return {
    namespace = namespace,
    -- store:set(key, value) — persist `value` (any JSON-able Lua value) under `key`.
    set = function(_, key, value)
      btv._shada_plugin_set(namespace, tostring(key), value)
    end,
    -- store:get(key) -> the stored value, or nil.
    get = function(_, key)
      return btv._shada_plugin_get(namespace, tostring(key))
    end,
    -- store:delete(key) — drop one key.
    delete = function(_, key)
      btv._shada_plugin_delete(namespace, tostring(key))
    end,
    -- store:keys() -> a sorted list of the stored keys.
    keys = function(_)
      return btv._shada_plugin_keys(namespace)
    end,
    -- store:clear() — drop every key in this namespace.
    clear = function(_)
      btv._shada_plugin_clear(namespace)
    end,
  }
end

-- `btv.shada.namespaces()` -> a sorted list of every plugin namespace currently
-- stored (after a shada load that is *all* persisted namespaces, not just the ones a
-- plugin opened this session). The audit primitive: a user can see what plugins have
-- stowed away, and the package manager forgets a removed plugin's namespace on
-- `:PluginClean`.
function btv.shada.namespaces()
  return btv._shada_plugin_namespaces()
end

-- `btv.shada.forget(namespace)` -> drop a whole namespace's stored data (it stops
-- being written at the next shada flush). The cross-session counterpart of a handle's
-- `:clear()`, but addressed by name — for pruning an orphan (e.g. an uninstalled
-- plugin's leftovers). Fails loud on a non-string / empty name.
function btv.shada.forget(namespace)
  if type(namespace) ~= "string" or namespace == "" then
    error("btv.shada.forget: namespace must be a non-empty string", 2)
  end
  btv._shada_plugin_clear(namespace)
end

-- ----- table / list / string helpers ----------------------------------------

-- `btv.tbl.*` / `btv.list.*` are the canonical table/list helper namespaces; the
-- bare `vim.tbl_*` / `vim.list_*` names are thin aliases onto them.
btv.tbl = btv.tbl or {}
btv.list = btv.list or {}

-- `btv.tbl.is_empty(t)` [alias `vim.tbl_isempty`]: does `t` have no entries?
function btv.tbl.is_empty(t)
  return next(t) == nil
end
vim.tbl_isempty = btv.tbl.is_empty

-- `btv.tbl.contains(t, value)` [alias `vim.tbl_contains`]: is `value` one of `t`'s values?
function btv.tbl.contains(t, value)
  for _, v in pairs(t) do
    if v == value then
      return true
    end
  end
  return false
end
vim.tbl_contains = btv.tbl.contains

-- `btv.tbl.keys(t)` [alias `vim.tbl_keys`]: a list of `t`'s keys.
function btv.tbl.keys(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  return keys
end
vim.tbl_keys = btv.tbl.keys

-- `btv.tbl.values(t)` [alias `vim.tbl_values`]: a list of `t`'s values.
function btv.tbl.values(t)
  local values = {}
  for _, v in pairs(t) do
    values[#values + 1] = v
  end
  return values
end
vim.tbl_values = btv.tbl.values

-- `btv.tbl.count(t)` [alias `vim.tbl_count`]: number of entries in `t` (any keys, not just the sequence).
function btv.tbl.count(t)
  local n = 0
  for _ in pairs(t) do
    n = n + 1
  end
  return n
end
vim.tbl_count = btv.tbl.count

-- `btv.tbl.deep_equal(a, b)` [alias `vim.deep_equal`]: structural equality (recurses
-- into tables, comparing keys and values). A general config/plugin helper.
function btv.tbl.deep_equal(a, b)
  if a == b then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  for k, v in pairs(a) do
    if not btv.tbl.deep_equal(v, b[k]) then
      return false
    end
  end
  for k in pairs(b) do
    if a[k] == nil then
      return false
    end
  end
  return true
end
vim.deep_equal = btv.tbl.deep_equal

-- `btv.npcall(fn, ...)` [alias `vim.npcall`]: `pcall` that maps failure to nil — `select(2, pcall(...))`
-- on success, nil on error. A neovim helper kept for config/plugin convenience
-- (wrap a call that may raise and treat failure as `"no value"`).
function btv.npcall(fn, ...)
  local ok, rv = pcall(fn, ...)
  if ok then
    return rv
  end
end
vim.npcall = btv.npcall

-- `btv.nonnil(...)` [alias `vim.nonnil`]: the first non-nil argument, or nil (verbatim from neovim's
-- vim/_core/shared.lua; the replacement for the deprecated `vim.F.if_nil`). A general
-- helper for defaulting an optional value.
function btv.nonnil(...)
  local nargs = select("#", ...)
  for i = 1, nargs do
    local v = select(i, ...)
    if v ~= nil then
      return v
    end
  end
  return nil
end
vim.nonnil = btv.nonnil

-- `btv._tointeger` / `btv._assert_integer`: integer coercion (verbatim from neovim's
-- vim/_core/shared.lua). `vim.func._memoize` uses them to parse a `concat-N` hash
-- spec; `_assert_integer` raises on a non-integer, `_tointeger` returns nil.
function btv._tointeger(x, base)
  local n = tonumber(x, base)
  if n and n == math.floor(n) then
    return n
  end
end

function btv._assert_integer(x, base)
  return btv._tointeger(x, base) or error(("Cannot convert %s to integer"):format(x))
end

-- `btv.tbl.get(o, ...)` [alias `vim.tbl_get`]: follow the `...` keys into nested table `o`, returning the
-- value reached or nil if any step is missing (or hits a non-table before the
-- last key). The safe nested access `lsp/<server>.lua` configs use to read deep
-- settings (e.g. `rust_analyzer`'s `settings['rust-analyzer'].cargo.sysrootSrc`).
function btv.tbl.get(o, ...)
  local keys = { ... }
  if #keys == 0 then
    return nil
  end
  for _, k in ipairs(keys) do
    if type(o) ~= "table" then
      return nil
    end
    o = o[k]
    if o == nil then
      return nil
    end
  end
  return o
end
vim.tbl_get = btv.tbl.get

-- `btv.tbl.filter(f, t)` [alias `vim.tbl_filter`]: Iterates with `pairs` (not `ipairs`) to match neovim: callers filter
-- name-keyed maps too (a plugin manager filters its plugin set, keyed by plugin
-- name), not just arrays. The result is always a fresh array.
function btv.tbl.filter(f, t)
  local out = {}
  for _, v in pairs(t) do
    if f(v) then
      out[#out + 1] = v
    end
  end
  return out
end
vim.tbl_filter = btv.tbl.filter

-- `btv.tbl.map(f, t)` [alias `vim.tbl_map`]: apply `f` to each value, keeping keys.
function btv.tbl.map(f, t)
  local out = {}
  for k, v in pairs(t) do
    out[k] = f(v)
  end
  return out
end
vim.tbl_map = btv.tbl.map

-- `btv.tbl.flatten(t)` [alias `vim.tbl_flatten`]: a single list with every nested list flattened into it
-- (depth-first). Deprecated in neovim but still called by `lspconfig.util`.
function btv.tbl.flatten(t)
  local out = {}
  local function flatten(list)
    for _, v in ipairs(list) do
      if type(v) == "table" then
        flatten(v)
      else
        out[#out + 1] = v
      end
    end
  end
  flatten(t)
  return out
end
vim.tbl_flatten = btv.tbl.flatten

-- `btv.tbl.deepcopy(orig)` [alias `vim.deepcopy`]: a recursive copy of `orig` (metatables preserved).
function btv.tbl.deepcopy(orig)
  if type(orig) ~= "table" then
    return orig
  end
  local copy = {}
  for k, v in pairs(orig) do
    copy[btv.tbl.deepcopy(k)] = btv.tbl.deepcopy(v)
  end
  return setmetatable(copy, getmetatable(orig))
end
vim.deepcopy = btv.tbl.deepcopy

-- True iff `t` is a *list* — a table whose keys are exactly `1..#t`. The one
-- implementation behind both `deep_extend`'s merge rule (just below) and the public
-- `btv.list.is_list` (further down this file), so the two can never drift apart, and
-- so neither routes through the `vim.islist` global a config is free to overwrite.
local function is_list(t)
  if type(t) ~= "table" then
    return false
  end
  local n = 0
  for _ in pairs(t) do
    n = n + 1
  end
  return n == #t
end

-- Is `t` mergeable — a table that is a *map* rather than a list? An empty table
-- counts (nothing distinguishes `{}` the empty map from `{}` the empty list, and
-- merging into it is lossless).
--
-- A JSON sentinel (`btv.json.null` / `btv.json.empty_object()`) is the one exception: it
-- IS an empty table, so without this it merged like an empty map — contributing no keys
-- and silently leaving whatever it was meant to override in place. `btv.lsp.config`
-- merges a user's config over a preset's, so "explicitly null this out" landed as
-- "keep the preset", with a wrong configuration on the wire and no error anywhere. The
-- sentinel is a *value*, so it replaces rather than merges, in both directions.
local function mergeable(t)
  return type(t) == "table" and json_mark(t) == nil and (next(t) == nil or not is_list(t))
end

-- `btv.tbl.deep_extend(behavior, ...)` [alias `vim.tbl_deep_extend`]: Merge `...` maps into one. `behavior` is `"force"` | `"keep"` | `"error"`. Nested
-- maps merge recursively; **lists are replaced whole**; scalar conflicts resolve per
-- `behavior`.
--
-- The list rule is neovim's, and it is load-bearing rather than cosmetic: merging a
-- list index-by-index fuses two unrelated entries and leaves a stale tail, so a
-- re-registered tool list (`btv.lsp.config`'s `settings.languages.<ft>`) would keep
-- the dropped tool's keys on entry 1 and its old entry 2 — a config nobody wrote.
--
-- The JSON sentinels are values, not maps: `btv.json.null` and `btv.json.empty_object()`
-- REPLACE whatever they are merged over (and are replaced by a later real value), so
-- "explicitly nothing" written over a preset's table means what it says.
function btv.tbl.deep_extend(behavior, ...)
  local result = {}
  local function merge(dst, src)
    for k, v in pairs(src) do
      if mergeable(v) and mergeable(dst[k]) then
        merge(dst[k], v)
      elseif dst[k] == nil or behavior == "force" then
        dst[k] = btv.tbl.deepcopy(v)
      elseif behavior == "error" then
        error("key found in more than one map: " .. tostring(k))
      end -- "keep": leave dst[k] as-is
    end
  end
  for i = 1, select("#", ...) do
    merge(result, (select(i, ...)))
  end
  return result
end
vim.tbl_deep_extend = btv.tbl.deep_extend

-- `btv.tbl.extend(behavior, ...)` [alias `vim.tbl_extend`]: Shallow variant of `btv.tbl.deep_extend`.
function btv.tbl.extend(behavior, ...)
  local result = {}
  for i = 1, select("#", ...) do
    for k, v in pairs((select(i, ...))) do
      if result[k] == nil or behavior == "force" then
        result[k] = v
      elseif behavior == "error" then
        error("key found in more than one map: " .. tostring(k))
      end
    end
  end
  return result
end
vim.tbl_extend = btv.tbl.extend

-- `btv.list.extend(dst, src, start, finish)` [alias `vim.list_extend`]: append `src[start..finish]` onto `dst`.
function btv.list.extend(dst, src, start, finish)
  start = start or 1
  finish = finish or #src
  for i = start, finish do
    dst[#dst + 1] = src[i]
  end
  return dst
end
vim.list_extend = btv.list.extend

-- `btv.list.slice(list, start, finish)` [alias `vim.list_slice`]: a copy of `list[start..finish]` (1-based,
-- inclusive; negative indices count from the end, as neovim). A completion plugin
-- caps its menu with `vim.list_slice(entries, 1, max_view_entries)`.
function btv.list.slice(list, start, finish)
  local n = #list
  start = start or 1
  finish = finish or n
  if start < 0 then
    start = n + start + 1
  end
  if finish < 0 then
    finish = n + finish + 1
  end
  local out = {}
  for i = start, finish do
    out[#out + 1] = list[i]
  end
  return out
end
vim.list_slice = btv.list.slice

-- `btv.str.startswith(s, prefix)` [alias `vim.startswith`]: does `s` begin with `prefix`?
function btv.str.startswith(s, prefix)
  return s:sub(1, #prefix) == prefix
end
vim.startswith = btv.str.startswith
-- `btv.str.endswith(s, suffix)` [alias `vim.endswith`]: does `s` end with `suffix`?
function btv.str.endswith(s, suffix)
  return suffix == "" or s:sub(-#suffix) == suffix
end
vim.endswith = btv.str.endswith

-- `btv.str.split(s, sep, opts)` [alias `vim.split`]: split `s` on `sep`.
function btv.str.split(s, sep, opts)
  -- Legacy positional form `vim.split(s, sep, plain)`: neovim keeps this
  -- backward-compat (a boolean third arg is the `plain` flag), and nvim-treesitter
  -- still calls `vim.split(path, '.', true)`. Without this it indexed a boolean as
  -- `opts.plain` and errored, breaking `require('nvim-treesitter').setup`.
  if type(opts) == "boolean" then
    opts = { plain = opts }
  end
  opts = opts or {}
  -- Empty separator: split into individual characters, matching neovim
  -- (`vim.split("nxso", "") == { "n", "x", "s", "o" }`, `vim.split("", "") == {}`)
  -- with no leading/trailing empty segment. `string.find(s, "", pos)` returns a
  -- zero-width match at `pos` (`from == pos`, `to == pos - 1`), so the generic
  -- loop below would leave `pos` unmoved and spin forever — a plugin hits this
  -- via `vim.split(modes, "")` (e.g. `"nxso"`). Handled up front; `trimempty` is a
  -- no-op here since single characters are never empty.
  if sep == "" then
    local parts = {}
    for i = 1, #s do
      parts[i] = string.sub(s, i, i)
    end
    return parts
  end
  local parts, pos = {}, 1
  while true do
    local from, to = string.find(s, sep, pos, opts.plain)
    if not from then
      parts[#parts + 1] = string.sub(s, pos)
      break
    end
    -- A zero-width separator match that doesn't advance the scan (`to < pos`,
    -- e.g. the pattern "x*" matching empty) would spin this loop forever; fail
    -- loud instead, exactly like neovim's gsplit ("Infinite loop detected").
    if to < pos then
      error("vim.split: separator pattern matched an empty string (would loop forever)", 2)
    end
    parts[#parts + 1] = string.sub(s, pos, from - 1)
    pos = to + 1
  end
  if opts.trimempty then
    while #parts > 0 and parts[#parts] == "" do
      parts[#parts] = nil
    end
    -- Drop leading empties with one shift (table.remove(parts, 1) per empty
    -- re-shifts the whole array each time — O(n²) on many leading separators).
    local first = 1
    while parts[first] == "" do
      first = first + 1
    end
    if first > 1 then
      local n = #parts
      table.move(parts, first, n, 1)
      for i = n - first + 2, n do
        parts[i] = nil
      end
    end
  end
  return parts
end
vim.split = btv.str.split

-- ----- vim.fn string-width / character builtins ------------------------------
-- The display/character helpers a popup plugin calls to lay out its
-- grid over UTF-8 text. These decode UTF-8 by hand over Lua's byte strings (5.4
-- ships a `utf8` library, but these predate the bump and could later use it).
-- (vim.fn already exists — the Rust bridge created it before the prelude loads —
-- so these extend it.)

-- Decode the codepoint starting at byte index `i` (1-based) of `s`, returning
-- (codepoint, byte_length), or (nil, 0) past the end. A malformed / truncated
-- sequence is treated as a single 1-byte char so iteration always advances.
local function utf8_decode(s, i)
  local b = s:byte(i)
  if not b then
    return nil, 0
  end
  if b < 0x80 then
    return b, 1
  end
  if b >= 0xF0 then
    local b2, b3, b4 = s:byte(i + 1), s:byte(i + 2), s:byte(i + 3)
    if b2 and b3 and b4 then
      return (b % 0x08) * 0x40000 + (b2 % 0x40) * 0x1000 + (b3 % 0x40) * 0x40 + (b4 % 0x40), 4
    end
  elseif b >= 0xE0 then
    local b2, b3 = s:byte(i + 1), s:byte(i + 2)
    if b2 and b3 then
      return (b % 0x10) * 0x1000 + (b2 % 0x40) * 0x40 + (b3 % 0x40), 3
    end
  elseif b >= 0xC0 then
    local b2 = s:byte(i + 1)
    if b2 then
      return (b % 0x20) * 0x40 + (b2 % 0x40), 2
    end
  end
  return b, 1 -- ASCII control, stray continuation, or truncated lead byte
end

-- Display cells one codepoint occupies: 2 for the common East-Asian-wide and
-- emoji ranges, else 1. INCOMPLETE: a pragmatic range check, not the full
-- Unicode east-asian-width / emoji tables, and combining marks (which should be
-- width 0) count as 1 — close enough for popup grid layout, wrong for dense CJK
-- with combining marks. A real impl would consult a generated width table.
local function char_width(cp)
  if
    cp >= 0x1100
    and (
      cp <= 0x115F -- Hangul Jamo
      or (cp >= 0x2E80 and cp <= 0xA4CF and cp ~= 0x303F) -- CJK … Yi
      or (cp >= 0xAC00 and cp <= 0xD7A3) -- Hangul Syllables
      or (cp >= 0xF900 and cp <= 0xFAFF) -- CJK Compat Ideographs
      or (cp >= 0xFE30 and cp <= 0xFE4F) -- CJK Compat Forms
      or (cp >= 0xFF00 and cp <= 0xFF60) -- Fullwidth Forms
      or (cp >= 0xFFE0 and cp <= 0xFFE6) -- Fullwidth signs
      or (cp >= 0x1F300 and cp <= 0x1FAFF) -- emoji & pictographs
      or (cp >= 0x20000 and cp <= 0x3FFFD) -- CJK Ext B+
    )
  then
    return 2
  end
  return 1
end

-- Encode codepoint `cp` to its UTF-8 byte string. The inverse of `utf8_decode`,
-- backing `vim.fn.nr2char`. An out-of-range / negative value is clamped to U+FFFD
-- (the replacement char) so it always yields a valid string.
local function utf8_encode(cp)
  cp = math.floor(tonumber(cp) or 0)
  if cp < 0 or cp > 0x10FFFF then
    cp = 0xFFFD
  end
  if cp < 0x80 then
    return string.char(cp)
  elseif cp < 0x800 then
    return string.char(0xC0 + math.floor(cp / 0x40), 0x80 + cp % 0x40)
  elseif cp < 0x10000 then
    return string.char(
      0xE0 + math.floor(cp / 0x1000),
      0x80 + math.floor(cp / 0x40) % 0x40,
      0x80 + cp % 0x40
    )
  end
  return string.char(
    0xF0 + math.floor(cp / 0x40000),
    0x80 + math.floor(cp / 0x1000) % 0x40,
    0x80 + math.floor(cp / 0x40) % 0x40,
    0x80 + cp % 0x40
  )
end

-- `btv.str.to_list(s[, utf8])` [alias `vim.fn.str2list`]: the codepoint of each character
-- in `s`, as a list of numbers (`str2list("AB") == { 65, 66 }`). bemtvi is always
-- UTF-8, so the `utf8` flag is accepted and ignored (the result is the same either
-- way). A plugin's key parser round-trips a keymap's lhs through this and `nr2char`.
function btv.str.to_list(s, _utf8)
  s = tostring(s or "")
  local out, i = {}, 1
  while i <= #s do
    local cp, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    out[#out + 1] = cp
    i = i + len
  end
  return out
end
vim.fn.str2list = btv.str.to_list

-- `btv.str.from_char(nr[, utf8])` [alias `vim.fn.nr2char`]: the string for codepoint `nr`
-- (`nr2char(65) == "A"`). The inverse of one `btv.str.to_list` element; bemtvi is always
-- UTF-8 so `utf8` is accepted and ignored.
function btv.str.from_char(nr, _utf8)
  return utf8_encode(nr)
end
vim.fn.nr2char = btv.str.from_char

-- `btv.str.chars(s[, skipcc])` [alias `vim.fn.strchars`]: number of characters
-- (codepoints) in `s`. INCOMPLETE: `skipcc` (skip composing characters) is ignored —
-- every codepoint counts, since bemtvi doesn't classify combining marks.
function btv.str.chars(s, _skipcc)
  s = tostring(s or "")
  local i, n = 1, 0
  while i <= #s do
    local _, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    i, n = i + len, n + 1
  end
  return n
end
vim.fn.strchars = btv.str.chars

-- `btv.str.displaywidth(s[, col])` [alias `vim.fn.strdisplaywidth`]: the display cells `s`
-- occupies, expanding tabs to the next tabstop boundary and counting wide chars as
-- two. `col` is the starting screen column used for tab-stop math (default 0); the
-- return value is the width of `s` itself (cells consumed beyond `col`). INCOMPLETE:
-- tabs expand on a fixed tabstop of 8, not the current buffer's `'tabstop'`.
function btv.str.displaywidth(s, col)
  s = tostring(s or "")
  local ts, base = 8, col or 0
  local w, i = base, 1
  while i <= #s do
    local cp, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    if cp == 9 then
      w = w + (ts - (w % ts)) -- tab advances to the next tabstop
    else
      w = w + char_width(cp)
    end
    i = i + len
  end
  return w - base
end
vim.fn.strdisplaywidth = btv.str.displaywidth

-- `btv.str.utfindex(s, [encoding,] index)` [alias `vim.str_utfindex`]: convert a *byte* offset into `s` to a
-- UTF code-unit count, supporting both neovim signatures (a completion plugin probes
-- the version and uses whichever the running editor offers):
--   * pre-0.11  `vim.str_utfindex(s [, byteidx])`        -> utf32, utf16  (two values)
--   * 0.11+     `vim.str_utfindex(s, encoding, byteidx)` -> single index for encoding
-- `byteidx` defaults to #s (end of string) and is clamped into range. The count is
-- whole codepoints whose start byte falls at or before `byteidx`; a codepoint
-- outside the BMP (4-byte UTF-8) is one utf-32 unit but two utf-16 units.
local function utf_unit_counts(s, byteidx)
  byteidx = byteidx or #s
  if byteidx < 0 then
    byteidx = 0
  elseif byteidx > #s then
    byteidx = #s
  end
  local u32, u16, i = 0, 0, 1
  while i <= byteidx do
    local _, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    u32 = u32 + 1
    u16 = u16 + (len == 4 and 2 or 1)
    i = i + len
  end
  return u32, u16
end

function btv.str.utfindex(s, a, b)
  s = tostring(s or "")
  if type(a) == "string" then
    -- 0.11+ form: (s, encoding, index). utf-8 reports the codepoint count.
    local u32, u16 = utf_unit_counts(s, b)
    if a == "utf-16" then
      return u16
    end
    return u32
  end
  -- legacy form: (s [, index]) -> utf32, utf16.
  return utf_unit_counts(s, a)
end
vim.str_utfindex = btv.str.utfindex

-- `btv.str.byteindex(s, [encoding,] index)` [alias `vim.str_byteindex`]: the inverse — the byte offset of the
-- `index`-th UTF code unit. Mirrors `str_utfindex`'s dual signature; the legacy form
-- counts utf-32 units (a 4-byte codepoint is one unit), the 0.11+ form honors the
-- requested encoding (utf-16 lets `index` land mid-astral, snapping to the
-- codepoint start). Clamps past-the-end indices to `#s`.
local function byteindex_for(s, index, utf16)
  if index == nil or index <= 0 then
    return 0
  end
  local i, units = 1, 0
  while i <= #s do
    local _, len = utf8_decode(s, i)
    if len == 0 then
      break
    end
    local step = (utf16 and len == 4) and 2 or 1
    if units + step > index then
      return i - 1
    end
    units = units + step
    i = i + len
    if units >= index then
      return i - 1
    end
  end
  return #s
end

function btv.str.byteindex(s, a, b)
  s = tostring(s or "")
  if type(a) == "string" then
    return byteindex_for(s, b, a == "utf-16")
  end
  return byteindex_for(s, a, false)
end
vim.str_byteindex = btv.str.byteindex

-- `btv.str.charpart(s, start[, len])` [alias `vim.fn.strcharpart`]: the substring of `s`
-- starting at character index `start` (0-based), spanning `len` characters (default:
-- to the end). A negative `start` drops that many leading characters off the count
-- (vim's behavior) and clamps the start to 0.
function btv.str.charpart(s, start, len)
  s = tostring(s or "")
  start = start or 0
  if start < 0 then
    if len ~= nil then
      len = len + start
    end
    start = 0
  end
  if len ~= nil and len <= 0 then
    return ""
  end
  local out, idx, i = {}, 0, 1
  while i <= #s do
    local _, blen = utf8_decode(s, i)
    if blen == 0 then
      break
    end
    if idx >= start and (len == nil or idx < start + len) then
      out[#out + 1] = s:sub(i, i + blen - 1)
    end
    idx = idx + 1
    i = i + blen
  end
  return table.concat(out)
end
vim.fn.strcharpart = btv.str.charpart

-- `btv.str.trans(s)` [alias `vim.fn.strtrans`]: `s` with unprintable characters shown as
-- printable text — control chars `0x00`–`0x1F` as `^@`…`^_`, `0x7F` as `^?` — matching vim, so a
-- key label built from raw bytes displays readably. Multibyte UTF-8 is left intact.
function btv.str.trans(s)
  s = tostring(s or "")
  return (
    s:gsub("[%z\1-\31\127]", function(c)
      local b = c:byte()
      if b == 127 then
        return "^?"
      end
      return "^" .. string.char(b + 64)
    end)
  )
end
vim.fn.strtrans = btv.str.trans

-- `btv.keytrans(s)` [alias `vim.fn.keytrans`]: translate the internal terminal-byte
-- form of a key sequence into readable key notation (`\15` → `<C-o>`, `\r` → `<CR>`,
-- `<Space>`, …) — the exact inverse of `nvim_replace_termcodes`, via the native
-- `btv._keytrans` (`parse_keys` + `key_to_notation`). A string already in notation
-- round-trips through unchanged.
function btv.keytrans(s)
  return btv._keytrans(tostring(s or ""))
end
vim.fn.keytrans = btv.keytrans

-- `btv.str.width(s)` [alias `btv.strwidth` / `vim.api.nvim_strwidth`]: the display width
-- of `s` in terminal cells, computed natively by the `btv._strwidth` Rust helper
-- (the same `unicode-width` table the renderer measures with — so this and the
-- drawn frame always agree). Wide (CJK / emoji) graphemes count as two, combining
-- marks as zero; tabs are NOT expanded (reach for `btv.str.displaywidth` when you
-- need tab expansion). This is the measure the `btv.align.*` helpers below size lines
-- against. The `btv.strwidth` / `nvim_strwidth` names are kept as neovim-compat
-- aliases onto the same native helper (they previously ran a coarser pure-Lua
-- heuristic that mis-sized combining marks as one cell).
assert(btv._strwidth, "btv.str.width: native btv._strwidth helper is missing")
btv.str.width = btv._strwidth
btv.strwidth = btv._strwidth
vim.api.nvim_strwidth = btv._strwidth

-- `btv.align.{left,center,right}(line, width)`: pad a single line with spaces so it
-- spans `width` display cells. `left` keeps the text at the start (pad on the
-- right), `right` pushes it to the end (pad on the left), and `center` splits the
-- padding, sending any odd leftover cell to the right. A line already at or wider
-- than `width` is returned unchanged — these only add spaces, never truncate.
-- Width is measured with `btv.str.width`, so wide glyphs pad correctly.
btv.align = btv.align or {}

-- `btv.align.left(line, width)`: pad `line` on the RIGHT with spaces so it spans
-- `width` display cells, keeping the text flush left. A line already at or wider
-- than `width` is returned unchanged — it only adds spaces, never truncates.
-- Width is measured with `btv.str.width`, so wide (CJK / emoji) glyphs pad correctly.
function btv.align.left(line, width)
  line = tostring(line or "")
  local pad = (width or 0) - btv.str.width(line)
  return pad > 0 and line .. string.rep(" ", pad) or line
end

-- `btv.align.right(line, width)`: like `btv.align.left`, but pads on the LEFT so the
-- text sits flush right within `width` display cells. At or over `width` → returned
-- unchanged; cell-width aware (`btv.str.width`).
function btv.align.right(line, width)
  line = tostring(line or "")
  local pad = (width or 0) - btv.str.width(line)
  return pad > 0 and string.rep(" ", pad) .. line or line
end

-- `btv.align.center(line, width)`: like `btv.align.left`, but splits the padding so the
-- text is centred within `width` display cells; an odd leftover cell goes to the
-- right. At or over `width` → returned unchanged; cell-width aware (`btv.str.width`).
function btv.align.center(line, width)
  line = tostring(line or "")
  local pad = (width or 0) - btv.str.width(line)
  if pad <= 0 then
    return line
  end
  local left = math.floor(pad / 2)
  return string.rep(" ", left) .. line .. string.rep(" ", pad - left)
end

-- `btv.tbl.spairs(t)` [alias `vim.spairs`]: `pairs()` in sorted-key order. Neovim's stable-iteration helper —
-- a custom `'tabline'`/`str_join` uses it so output order is deterministic.
function btv.tbl.spairs(t)
  local keys = {}
  for k in pairs(t) do
    keys[#keys + 1] = k
  end
  table.sort(keys)
  local i = 0
  return function()
    i = i + 1
    local k = keys[i]
    if k ~= nil then
      return k, t[k]
    end
  end
end
vim.spairs = btv.tbl.spairs

-- `btv.print(...)` [alias `vim.print`]: pretty-print each argument (via `btv.inspect`) on the message
-- line and return them unchanged, so it can wrap a value inline. Strings print
-- verbatim; tables are inspected.
function btv.print(...)
  local n = select("#", ...)
  local parts = {}
  for i = 1, n do
    local v = select(i, ...)
    parts[i] = type(v) == "string" and v or btv.inspect(v)
  end
  print(table.concat(parts, "\n"))
  return ...
end
vim.print = btv.print

-- ----- minimal vim.iter ------------------------------------------------------
-- A small chainable iterator over list-like tables: map / filter / each / fold
-- / totable, enough for what the colorscheme load path reaches for.
local Iter = {}
Iter.__index = Iter

-- `btv.iter(src[, state, ctrl])` [alias `vim.iter`]: wrap a list-like table OR a Lua iterator triple
-- in a chainable iterator. The triple form is what `vim.iter(vim.fs.parents(p))`
-- passes — `vim.fs.parents` returns `(fn, state, start)`, which Lua spreads as
-- three args here — so the ancestors are drained eagerly into the item list.
function btv.iter(src, state, ctrl)
  local items = {}
  if type(src) == "function" then
    local var = ctrl
    while true do
      local v = src(state, var)
      if v == nil then
        break
      end
      var = v
      items[#items + 1] = v
    end
  elseif type(src) == "table" then
    for _, v in ipairs(src) do
      items[#items + 1] = v
    end
  end
  return setmetatable({ _items = items }, Iter)
end
vim.iter = btv.iter

-- Iter:find(pred): the first item for which `pred(item)` is truthy (or, when
-- `pred` is a plain value, the first item equal to it), else nil.
function Iter:find(pred)
  for _, v in ipairs(self._items) do
    if type(pred) == "function" then
      if pred(v) then
        return v
      end
    elseif v == pred then
      return v
    end
  end
  return nil
end

-- Iter:any(pred): true iff `pred(item)` is truthy for some item.
function Iter:any(pred)
  for _, v in ipairs(self._items) do
    if pred(v) then
      return true
    end
  end
  return false
end

-- Iter:flatten(): flatten one level of list-valued items into the stream.
function Iter:flatten()
  local out = {}
  for _, v in ipairs(self._items) do
    if type(v) == "table" then
      for _, inner in ipairs(v) do
        out[#out + 1] = inner
      end
    else
      out[#out + 1] = v
    end
  end
  self._items = out
  return self
end

function Iter:map(f)
  local out = {}
  for _, v in ipairs(self._items) do
    local r = f(v)
    if r ~= nil then
      out[#out + 1] = r
    end
  end
  self._items = out
  return self
end

function Iter:filter(f)
  local out = {}
  for _, v in ipairs(self._items) do
    if f(v) then
      out[#out + 1] = v
    end
  end
  self._items = out
  return self
end

function Iter:each(f)
  for _, v in ipairs(self._items) do
    f(v)
  end
end

function Iter:fold(acc, f)
  for _, v in ipairs(self._items) do
    acc = f(acc, v)
  end
  return acc
end

function Iter:totable()
  return self._items
end

-- `btv.str.substitute(str, pat, sub, flags)` [alias `vim.fn.substitute`]: a real vim-regex
-- substitution, backed by the Rust engine (`btv._substitute`) so plugins that rely on
-- vim's magic dialect + replacement syntax (`\(\)`, `\{-}`, `&`, `\1`, `\U…\E`, …) get
-- the same result neovim gives. This is a DIFFERENT dialect from bemtvi's `/` search
-- (canonical regex); the divergence is intentional and lives in the compat layer. An
-- invalid / unsupported pattern raises (fail loud).
function btv.str.substitute(str, pat, sub, flags)
  return btv._substitute(tostring(str), tostring(pat), tostring(sub or ""), tostring(flags or ""))
end
vim.fn.substitute = btv.str.substitute

-- `vim.trim(s)`: aliases the canonical `btv.str.trim` (defined in stdlib.lua, a
-- superset accepting an optional mask/dir).
vim.trim = btv.str.trim

-- `btv.list.is_list(t)` [alias `vim.islist`]: true iff `t` is a list (a table whose
-- keys are exactly `1..#t`).
btv.list = btv.list or {}
function btv.list.is_list(t)
  return is_list(t)
end
vim.islist = btv.list.is_list
vim.tbl_islist = btv.list.is_list -- the pre-0.10 name
