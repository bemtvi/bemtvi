-- nxvim Lua prelude — nx.utils, the general-purpose helper namespace.
--
-- The home for broadly-useful utilities that aren't data helpers (those are
-- nx.tbl / nx.list / nx.str / nx.iter in prelude/stdlib.lua) and aren't a feature
-- API — control-flow / timing glue plugin authors reach for. nxvim-native (no
-- vim.* twin). Loaded after prelude/runtime.lua (nx.timer / nx.schedule) and
-- prelude/promise.lua, so a util may build on timers AND the promise/async surface.
local vim = vim
nx = nx or {}
nx.utils = nx.utils or {}

-- ----- path helpers ----------------------------------------------------------
-- Pure string math over `/`-separated paths — nothing here touches the filesystem
-- (all fs is async `nx.fs`). The one copy of the little path idioms the prelude and
-- plugins otherwise re-derive: strip-the-last-component, last-component, `~`, and
-- the walk-up-the-tree loop.

-- `nx.utils.dirname(path)`: the directory part of `path` — everything before the
-- last `/` (`"/a/b/c.txt"` → `"/a/b"`). `""` for an entry directly under the root
-- (`"/a"` → `""`); a path with no `/` comes back unchanged (there is nothing to
-- strip), which callers walking upward detect as "no parent".
function nx.utils.dirname(path)
  return (path:gsub("/[^/]*$", ""))
end

-- `nx.utils.basename(path)`: the last `/`- or `\`-separated component of `path`,
-- ignoring trailing separators (`"/a/b/"` → `"b"`). `nil` when nothing remains
-- (the root `"/"`, an empty string).
function nx.utils.basename(path)
  return (path:gsub("[/\\]+$", ""):match("[^/\\]+$"))
end

-- `nx.utils.expanduser(path)`: expand a leading `~` / `~/` to `$HOME`, so a config
-- value can point at a home-relative path (`"~/work/foo"`). Only the leading tilde
-- is touched — a mid-path `~` is a literal path component, and the `~user` form is
-- not resolved (returned unchanged, like vim with an unknown user). With no `$HOME`
-- in the environment the path is returned unchanged rather than mangled.
function nx.utils.expanduser(path)
  local home = os.getenv("HOME")
  if not home or home == "" then
    return path
  elseif path == "~" then
    return home
  elseif path:sub(1, 2) == "~/" then
    return home .. path:sub(2)
  end
  return path
end

-- `nx.utils.ancestors(path)`: iterate the ancestor directories of `path`, nearest
-- first — `dirname(path)`, then its parent, and so on. Stops before the empty
-- string, so the filesystem root itself is never produced; for a relative path the
-- walk ends at its first component. The upward-walk loop behind `.editorconfig`
-- discovery and LSP root-marker search:
--
-- ```lua
-- for dir in nx.utils.ancestors("/a/b/c.txt") do
--   -- "/a/b", then "/a"
-- end
-- ```
function nx.utils.ancestors(path)
  local dir = nx.utils.dirname(path)
  return function()
    if not dir or dir == "" then
      return nil
    end
    local cur = dir
    local parent = nx.utils.dirname(cur)
    dir = parent ~= cur and parent or nil
    return cur
  end
end

-- ----- nx.utils.argv ---------------------------------------------------------
-- `nx.utils.argv(spec)`: build a flat argv list from a run-family spec — `spec.cmd`
-- is the program (a string, or an argv list whose first element is the program),
-- `spec.args` an optional list appended after it, so `{ cmd = "git", args = { "log" } }`
-- and `{ cmd = { "git", "log" } }` produce the same argv. The normalizer behind
-- `nx.run` / `nx.run_stream` / `nx.run_local`, public so a plugin composing specs
-- for those APIs (a task runner, a DAP adapter) shares the exact same shape.
function nx.utils.argv(spec)
  local cmd = spec.cmd
  if type(cmd) == "string" then
    cmd = { cmd }
  end
  local argv = {}
  for _, c in ipairs(cmd) do
    argv[#argv + 1] = c
  end
  for _, a in ipairs(spec.args or {}) do
    argv[#argv + 1] = a
  end
  return argv
end

-- ----- nx.utils.str_list -----------------------------------------------------
-- `nx.utils.str_list(spec, what)`: normalize a config value that may be one string
-- or a list of strings — a bare string becomes a one-element list, a list of
-- strings passes through, `nil` becomes `{}` (the caller's default applies).
-- Anything else raises, with `what` naming the option in the message (e.g.
-- `"nx.snippet.setup: jump_next"`). The shared normalizer behind key-spec options
-- (`nx.complete.setup` / `nx.snippet.setup`), public for any config surface that
-- accepts `string | string[]`.
function nx.utils.str_list(spec, what)
  if spec == nil then
    return {}
  elseif type(spec) == "string" then
    return { spec }
  elseif type(spec) == "table" then
    for _, k in ipairs(spec) do
      if type(k) ~= "string" then
        error(what .. " must be string(s), got " .. type(k))
      end
    end
    return spec
  end
  error(what .. " must be a string or list of strings")
end

-- ----- nx.utils.caller_source ------------------------------------------------
-- `nx.utils.caller_source()`: the source path of the file whose code called into
-- the current function — walks up the Lua stack to the nearest real-file frame.
-- nxvim sources every config / plugin / `require`d file with an `@<path>` chunk
-- name, while the embedded prelude chunks are named `nxvim:prelude/*` (no `@`) and
-- C frames carry `=[C]` — so the first `@`-prefixed source above the caller is the
-- file whose code is running. `nil` when nothing on the stack is `@`-named.
-- Behind vim's `<sfile>` / `<script>` (`nx.expand`) and the plugin-persistence
-- namespace attribution (`nx._resolve_namespace`).
function nx.utils.caller_source()
  for lvl = 2, 40 do
    local info = debug.getinfo(lvl, "S")
    if not info then
      break
    end
    if info.source:sub(1, 1) == "@" then
      return info.source:sub(2)
    end
  end
  return nil
end

-- ----- nx.utils.debounce -----------------------------------------------------
-- `nx.utils.debounce(fn, ms)`: wrap `fn` into a trailing-edge debounce over
-- `nx.timer` — the returned value runs `fn` once, `ms` after the LAST call, so a
-- burst of rapid calls collapses to a single invocation with the most recent
-- arguments. A timing/control-flow helper (which-key's show-delay, on-change
-- handlers, resize / scroll reactions); it runs nothing on the input path.
--
-- It is callback-shaped, NOT promise-shaped: debounce coalesces a stream of many
-- calls, whereas a promise models one eventual value — different jobs. They
-- compose, though: pass an `nx.async` function as `fn` to kick awaitable work after
-- the quiet period, and reach for `nx.promise.delay` when you want an *await-able*
-- one-shot sleep instead.
--
-- The result is callable AND carries:
--
-- ```
-- :cancel()  drop a pending invocation (the next call re-arms)
-- :flush()   run a pending invocation now (no-op when idle)
-- ```
--
-- Each call (re)arms the timer; nothing fires until the calls stop for `ms`.
function nx.utils.debounce(fn, ms)
  if type(fn) ~= "function" then
    error("nx.utils.debounce: fn must be a function", 2)
  end
  ms = ms or 0
  local timer -- the armed nx.timer handle while a call is pending, else nil
  -- The most recent call's arguments, captured `{ ... }` + count (the prelude's
  -- vararg idiom, e.g. schedule_wrap — PUC has no whitelisted table.pack); nil
  -- when idle.
  local args, argc
  local function fire()
    timer = nil
    local a, n = args, argc
    args, argc = nil, nil
    fn(table.unpack(a, 1, n))
  end
  local debounced = setmetatable({}, {
    __call = function(_, ...)
      args, argc = { ... }, select("#", ...)
      if timer then
        timer:stop()
      end
      timer = nx.timer(fire, ms)
    end,
  })
  function debounced:cancel()
    if timer then
      timer:stop()
      timer = nil
    end
    args, argc = nil, nil
  end
  function debounced:flush()
    if timer then
      timer:stop()
      fire()
    end
  end
  return debounced
end
