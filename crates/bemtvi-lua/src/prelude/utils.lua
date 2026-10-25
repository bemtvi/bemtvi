-- bemtvi Lua prelude — btv.utils, the general-purpose helper namespace.
--
-- The home for broadly-useful utilities that aren't data helpers (those are
-- btv.tbl / btv.list / btv.str / btv.iter in prelude/stdlib.lua) and aren't a feature
-- API — control-flow / timing glue plugin authors reach for. bemtvi-native (no
-- vim.* twin). Loaded after prelude/runtime.lua (btv.timer / btv.schedule) and
-- prelude/promise.lua, so a util may build on timers AND the promise/async surface.
local vim = vim
btv = btv or {}
btv.utils = btv.utils or {}

-- ----- path helpers ----------------------------------------------------------
-- Pure string math over paths — nothing here touches the filesystem (all fs is async
-- `btv.fs`). The one copy of the little path idioms the prelude and plugins otherwise
-- re-derive: strip-the-last-component, last-component, `~`, the walk-up-the-tree loop,
-- and — the part that differs by platform — what a separator is and where a path's
-- root ends. Every platform-dependent answer below goes through `btv.utils.path_style`.

-- `btv.utils.path_style()`: the path convention the editor's paths follow — `"windows"`
-- or `"unix"` (the same spellings `btv.glob`'s `style` option takes).
--
-- Under `"windows"`, `/` and `\` both separate when a path is *read*, a path can be
-- rooted at a drive (`C:\`, `C:/`) or a UNC share (`\\server\share\`), and paths the
-- editor *produces* use `\`. Under `"unix"`, `/` is the only separator and `\` is an
-- ordinary character.
--
-- This is the single place the path helpers (`btv.utils.path_sep`, `path_root`,
-- `is_absolute`, `dirname`, and `btv.fname.modify` built on them) decide which
-- platform's rules apply. It follows the **session's** path syntax, which the server
-- publishes as `btv._path_style` (`"posix"` or `"windows"`) alongside `btv._cwd`: the
-- daemon's in a daemon session — a Windows client editing a unix host still sees unix
-- paths — and the host's otherwise. Before the first publish it answers for the host
-- (`btv.utils.is_windows`).
function btv.utils.path_style()
  local session = rawget(btv, "_path_style")
  if session == "windows" then
    return "windows"
  elseif session == "posix" then
    return "unix"
  end
  return btv.utils.is_windows() and "windows" or "unix"
end

local function windows_paths()
  return btv.utils.path_style() == "windows"
end

-- `btv.utils.path_sep()`: the separator the editor writes paths with — `"\"` under the
-- `"windows"` path style, `"/"` otherwise. Use it to *build* a path that will be shown
-- or compared against one the editor reports; when *reading* a path, accept either
-- separator on Windows (`btv.utils.path_root`, `dirname` and `basename` already do).
function btv.utils.path_sep()
  return windows_paths() and "\\" or "/"
end

-- The separator characters, as the body of a Lua pattern character class
-- (`"[" .. cls .. "]"` matches one separator, `"[^" .. cls .. "]"` one non-separator).
-- Private: the path helpers' own building block.
function btv.utils._sep_class()
  return windows_paths() and "/\\" or "/"
end

-- `path` with every separator spelled `/`, for COMPARING two paths that may spell
-- their separators differently (a runtimepath entry typed with `/` against a chunk
-- source Lua reports with `\`). Identity under the unix style, where `\` is not a
-- separator. Private: never hand the folded form back as a path.
function btv.utils._fold_seps(path)
  if windows_paths() then
    return (path:gsub("\\", "/"))
  end
  return path
end

-- Join `...` into one path by the Windows rules (`windows` true: `/` and `\` both
-- separate, `\` is written) or the unix ones (`/` only). The shared body of
-- `path_join` / `local_path_join`, which differ only in whose rules apply.
local function join_paths(windows, ...)
  local n = select("#", ...)
  local parts = { ... }
  local sep = windows and "\\" or "/"
  local cls = windows and "/\\" or "/"
  local out
  for i = 1, n do
    local seg = parts[i]
    if seg ~= nil and seg ~= "" then
      seg = tostring(seg)
      if windows then
        seg = (seg:gsub("/", "\\"))
      end
      if out == nil then
        out = seg
      else
        seg = (seg:gsub("^[" .. cls .. "]+", ""))
        if seg ~= "" then
          if out:find("[" .. cls .. "]$") then
            out = out .. seg
          else
            out = out .. sep .. seg
          end
        end
      end
    end
  end
  return out or ""
end

-- `btv.utils.path_join(...)`: join path segments into ONE filesystem path spelled with
-- the session's separator (`btv.utils.path_sep`), so a path built from a directory the
-- editor reported (`C:\Users\me\data`) never comes out half `\`, half `/`. Under the
-- `"windows"` style a `/` inside a segment is respelled `\` too, so
-- `path_join(cfg, "lua/plugins.lua")` is one native path; under `"unix"` a `\` is an
-- ordinary character and left alone. Separators already at a seam are not doubled,
-- and `nil` / `""` segments are skipped.
--
-- ```lua
-- btv.utils.path_join("/home/me", "plugins", "foo")   -- "/home/me/plugins/foo"
-- btv.utils.path_join([[C:\data]], "lua/x.lua")        -- [[C:\data\lua\x.lua]]
-- ```
--
-- It follows the *session's* path style — in a daemon session, the daemon's — so it is
-- for paths on the session filesystem (`btv.fs`, buffer names, `getcwd()`). A path on
-- THIS machine (`btv.fs_local`, `stdpath`, `package.path`) is joined with
-- `btv.utils.local_path_join`. This is for paths handed to the filesystem or shown as
-- files: `btv.utils.joinpath` stays neovim's `vim.fs.joinpath` (always `/`), and a glob
-- keeps its own `/`.
function btv.utils.path_join(...)
  return join_paths(windows_paths(), ...)
end

-- `btv.utils.local_path_join(...)`: `btv.utils.path_join`, but by THIS machine's rules
-- (`btv.utils.is_windows`) whatever the session's — the twin for paths on the local
-- filesystem (`btv.fs_local`, `stdpath`, the plugin manager's directories, `package.path`),
-- as `btv.fs_local` is the twin of `btv.fs`. Identical to `path_join` outside a daemon
-- session.
function btv.utils.local_path_join(...)
  return join_paths(btv.utils.is_windows(), ...)
end

-- `btv.utils.path_root(path)`: the root prefix of `path`, exactly as spelled in it,
-- or `""` for a relative path. `path:sub(#root + 1)` is then the part below the root.
--
-- ```
-- unix:     "/a/b"                -> "/"
-- windows:  "C:\a" / "C:/a"       -> "C:\" / "C:/"
--           "\\srv\share\a"       -> "\\srv\share\"
--           "\a"                  -> "\"   (rooted on the current drive)
-- both:     "a/b", ""             -> ""
-- ```
--
-- A drive-relative `C:a` has no root (it is relative to that drive's own cwd).
function btv.utils.path_root(path)
  if not windows_paths() then
    return path:sub(1, 1) == "/" and "/" or ""
  end
  return path:match("^[/\\][/\\][^/\\]+[/\\][^/\\]+[/\\]?")
    or path:match("^%a:[/\\]")
    or path:match("^[/\\]")
    or ""
end

-- `btv.utils.is_absolute(path)`: is `path` rooted — `/…` on Unix; a drive (`C:\…`),
-- UNC share (`\\server\share\…`) or bare separator (`\…`) on Windows? A relative path
-- is the one a cwd has to be prepended to (see `btv.fname.modify(path, ":p")`).
function btv.utils.is_absolute(path)
  return btv.utils.path_root(path) ~= ""
end

-- `btv.utils.dirname(path)`: the directory part of `path` — everything before the
-- last separator (`"/a/b/c.txt"` → `"/a/b"`). `""` for an entry directly under the
-- root (`"/a"` → `""`, and on Windows `"C:\a"` → `""`); a path with no separator comes
-- back unchanged (there is nothing to strip), which callers walking upward detect as
-- "no parent". On Windows both `/` and `\` separate.
function btv.utils.dirname(path)
  if not windows_paths() then
    return (path:gsub("/[^/]*$", ""))
  end
  local root = btv.utils.path_root(path)
  local head = path:sub(#root + 1):match("^(.*)[/\\][^/\\]*$")
  if head then
    return root .. head
  end
  return root ~= "" and "" or path
end

-- `btv.utils.basename(path)`: the last `/`- or `\`-separated component of `path`,
-- ignoring trailing separators (`"/a/b/"` → `"b"`). `nil` when nothing remains
-- (the root `"/"`, an empty string).
function btv.utils.basename(path)
  return (path:gsub("[/\\]+$", ""):match("[^/\\]+$"))
end

-- `btv.utils.expanduser(path)`: expand a leading `~` / `~/` to `$HOME`, so a config
-- value can point at a home-relative path (`"~/work/foo"`). Only the leading tilde
-- is touched — a mid-path `~` is a literal path component, and the `~user` form is
-- not resolved (returned unchanged, like vim with an unknown user). With no `$HOME`
-- in the environment the path is returned unchanged rather than mangled.
function btv.utils.expanduser(path)
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

-- `btv.utils.ancestors(path)`: iterate the ancestor directories of `path`, nearest
-- first — `dirname(path)`, then its parent, and so on. Stops before the empty
-- string, so the filesystem root itself is never produced. A **relative** `path` is
-- first resolved against the editor's working directory (`btv.fname.modify(path, ":p")`,
-- which also collapses `.` / `..`) — the ancestry of a relative path is only
-- meaningful against the cwd, and the paths callers walk are buffer names
-- (`btv.buf.name`, an autocmd's `ev.file`), which are whatever the user typed and so
-- routinely relative (`bemtvi src/main.rs`). Without that, the walk would stop at the
-- first typed component and never reach the project root. The upward-walk loop behind
-- `.editorconfig` discovery and LSP root-marker search:
--
-- ```lua
-- for dir in btv.utils.ancestors("/a/b/c.txt") do
--   -- "/a/b", then "/a"
-- end
-- ```
function btv.utils.ancestors(path)
  local dir = btv.utils.dirname(btv.fname.modify(path, ":p"))
  return function()
    if not dir or dir == "" then
      return nil
    end
    local cur = dir
    local parent = btv.utils.dirname(cur)
    dir = parent ~= cur and parent or nil
    return cur
  end
end

-- `btv.utils.joinpath(...)`: join path components with a single `/`, collapsing the
-- separators at each seam so `joinpath("/a/", "/b/", "c")` is `"/a/b/c"`. An empty
-- or `nil` component is skipped rather than producing a doubled slash, so a
-- conditionally-absent middle segment doesn't corrupt the result. A leading `/` on
-- the FIRST component is preserved (the only place an absolute path is decided);
-- everything after it is treated as relative, so a later `"/b"` appends rather than
-- restarting at the root. Pure string math — nothing is resolved against the
-- filesystem or the cwd.
--
-- ```lua
-- btv.utils.joinpath(root, "node_modules/.bin", cmd)
-- ```
function btv.utils.joinpath(...)
  local parts = { ... }
  local n = select("#", ...)
  local out = nil
  for i = 1, n do
    local p = parts[i]
    if p ~= nil and p ~= "" then
      if type(p) ~= "string" then
        error("btv.utils.joinpath: component " .. i .. " must be a string, got " .. type(p), 2)
      end
      if out == nil then
        -- The first component alone decides absolute-vs-relative; only its TRAILING
        -- separators are trimmed.
        out = p:gsub("/+$", "")
        -- A first component that is exactly the root collapses to "" above; keep it
        -- as "/" so the join below doesn't emit a relative path.
        if out == "" then
          out = "/"
        end
      else
        local seg = p:gsub("^/+", ""):gsub("/+$", "")
        if seg ~= "" then
          out = (out == "/" and "/" or out .. "/") .. seg
        end
      end
    end
  end
  return out or ""
end

-- `btv.utils.normalize(path)`: canonicalize a path as pure string math — expand a
-- leading `~`, fold `\` to `/`, collapse repeated separators, drop `.` components,
-- and resolve `..` against the component before it. Trailing separators are stripped
-- (except on the root itself). Nothing touches the filesystem, so this does NOT
-- resolve symlinks — a `..` after a symlinked directory resolves lexically, which is
-- what a config comparing two configured paths wants (for the real thing, `btv.fs.realpath`
-- is the async op).
--
-- A `..` that would escape a relative path is KEPT (`"a/../../b"` → `"../b"`), since
-- there is no cwd here to resolve it against; on an absolute path it is dropped at
-- the root (`"/../a"` → `"/a"`), matching every OS.
function btv.utils.normalize(path)
  if type(path) ~= "string" then
    error("btv.utils.normalize: path must be a string, got " .. type(path), 2)
  end
  if path == "" then
    return ""
  end
  path = btv.utils.expanduser(path:gsub("\\", "/"))
  local absolute = path:sub(1, 1) == "/"
  local out = {}
  for seg in path:gmatch("[^/]+") do
    if seg == ".." then
      local last = out[#out]
      if last ~= nil and last ~= ".." then
        out[#out] = nil -- cancel against the component before it
      elseif not absolute then
        out[#out + 1] = ".." -- nothing to cancel, and no root to clamp at
      end
      -- On an absolute path with nothing to cancel, `..` at the root is dropped.
    elseif seg ~= "." then
      out[#out + 1] = seg -- `.` is a no-op component, so only anything else lands
    end
  end
  local joined = table.concat(out, "/")
  if absolute then
    return "/" .. joined
  end
  return joined == "" and "." or joined
end

-- `btv.utils.relpath(base, target)`: `target` expressed relative to `base`, or `nil`
-- when `target` is not inside `base`. Both are normalized first, and the comparison
-- is on whole path COMPONENTS — so `relpath("/a/b", "/a/bc")` is nil, not `"c"`,
-- which a plain prefix-match would get wrong. `target == base` yields `"."`.
--
-- The "is this file under that directory?" test, which is how a config decides a
-- buffer belongs to a dependency tree (a cargo registry checkout, a Go module
-- cache) rather than the project:
--
-- ```lua
-- if btv.utils.relpath(cargo_registry, file) then --[[ it's a library file ]] end
-- ```
function btv.utils.relpath(base, target)
  base = btv.utils.normalize(base)
  target = btv.utils.normalize(target)
  if base == target then
    return "."
  end
  -- Root is the one base whose string form doesn't take a trailing separator.
  local prefix = base == "/" and "/" or base .. "/"
  if target:sub(1, #prefix) ~= prefix then
    return nil
  end
  return target:sub(#prefix + 1)
end

-- `btv.utils.is_windows()`: is this a Windows host? Answered from `package.config`'s
-- directory separator, which Lua fills in at build time — so it is correct in every
-- build (native, daemon-side, wasm) without a host round-trip or an env-var guess.
--
-- The check behind "which name does this program have here?" — a tool installed as
-- `foo` on Unix is `foo.exe` or `foo.bat` on Windows, and a plugin resolving an
-- executable has to know which to look for. Prefer `btv.fs.which`, which searches for
-- you; reach for this when the *name itself* differs.
function btv.utils.is_windows()
  -- `string.sub(package.config, …)` rather than the method form: selene's standard
  -- library model types `package.config` as a plain string without the string
  -- metatable, so `package.config:sub(…)` trips its stdlib check.
  return string.sub(package.config, 1, 1) == "\\"
end

-- ----- file:// URIs ----------------------------------------------------------
-- The one copy of the path <-> `file://` conversion. A language server addresses
-- every document by URI while bemtvi addresses it by path, so anything that hands a
-- document to a server (`btv.lsp.position_params`, a command's `arguments`) or reads
-- one back out of a reply crosses this seam. Pure string math — nothing is resolved
-- against the filesystem.

-- Percent-encode a `/`-separated URI path: everything outside the unreserved set.
local function encode_uri_path(p)
  return (
    p:gsub("[^%w%-%.%_%~/]", function(c)
      return string.format("%%%02X", string.byte(c))
    end)
  )
end

-- `btv.utils.uri_from_path(path)`: the `file://` URI naming `path`.
--
-- Percent-encodes everything outside the URI unreserved set, `/` excepted — a path
-- holding a space, a `#` or a non-ASCII character is otherwise a malformed URI the
-- server silently misreads (it truncates at the `#`, or fails to match the document
-- it already has open under the encoded spelling). The path is normalized first, so
-- the same file always produces the same URI and a server's document map hits, and a
-- RELATIVE path (a buffer name as typed) is resolved against the editor's cwd, since a
-- `file://` URI can only name an absolute path.
--
-- A Windows path takes the shape every LSP server (and the `url` crate's
-- `Url::from_file_path`) expects: `C:\Users\x\a.py` → `file:///C:/Users/x/a.py`, the
-- drive's `:` left unescaped; a UNC path names its server as the URI's host
-- (`\\srv\share\a` → `file://srv/share/a`).
function btv.utils.uri_from_path(path)
  if type(path) ~= "string" then
    error("btv.utils.uri_from_path: path must be a string, got " .. type(path), 2)
  end
  if not btv.utils.is_absolute(path) then
    path = btv.fname.modify(path, ":p")
  end
  local root = btv.utils.path_root(path)
  local drive = root:match("^(%a:)[/\\]$")
  if drive then
    return "file:///" .. drive .. encode_uri_path(btv.utils.normalize(path:sub(#drive + 1)))
  end
  local server, share = root:match("^[/\\][/\\]([^/\\]+)[/\\]([^/\\]+)")
  if server then
    local below = btv.utils.normalize("/" .. share .. "/" .. path:sub(#root + 1))
    return "file://" .. encode_uri_path(server) .. encode_uri_path(below)
  end
  return "file://" .. encode_uri_path(btv.utils.normalize(path))
end

-- `btv.utils.uri_from_buf(bufnr)`: buffer `bufnr`'s `file://` URI (`0`/nil = the
-- current buffer), or `""` for a buffer with no file — which is what a server should
-- be told, rather than a `file://` naming nothing.
function btv.utils.uri_from_buf(bufnr)
  local name = btv.buf.name(bufnr or 0)
  return (name == nil or name == "") and "" or btv.utils.uri_from_path(name)
end

-- `btv.utils.uri_to_path(uri)`: the filesystem path a `file://` URI names, with its
-- percent-escapes decoded, or `nil` for any other scheme.
--
-- `nil` rather than the raw string: a `deno:` / `jdt:` / `untitled:` URI names a
-- document that has no path at all, and treating one as a path creates a buffer for a
-- file that will never exist. Callers branch on the nil.
--
-- Under the Windows path style (`btv.utils.path_style`) the result is a native path:
-- `file:///C:/x/a.py` → `C:\x\a.py`, and a URI with a host is a UNC path
-- (`file://srv/share/a` → `\\srv\share\a`).
function btv.utils.uri_to_path(uri)
  if type(uri) ~= "string" then
    return nil
  end
  local path = uri:match("^file://(.*)$")
  if not path then
    return nil
  end
  path = path:gsub("%%(%x%x)", function(hex)
    return string.char(tonumber(hex, 16))
  end)
  if not windows_paths() then
    return path
  end
  local host, rest = path:match("^([^/]*)(.*)$")
  if host ~= "" and host ~= "localhost" then
    return "\\\\" .. host .. (rest:gsub("/", "\\"))
  end
  -- `/C:/x` → `C:/x`: the URI path's leading `/` sits before the drive.
  rest = rest:gsub("^/(%a:)", "%1")
  return (rest:gsub("/", "\\"))
end

-- ----- btv.utils.argv ---------------------------------------------------------
-- `btv.utils.argv(spec)`: build a flat argv list from a run-family spec — `spec.cmd`
-- is the program (a string, or an argv list whose first element is the program),
-- `spec.args` an optional list appended after it, so `{ cmd = "git", args = { "log" } }`
-- and `{ cmd = { "git", "log" } }` produce the same argv. The normalizer behind
-- `btv.run` / `btv.run_stream` / `btv.run_local`, public so a plugin composing specs
-- for those APIs (a task runner, a DAP adapter) shares the exact same shape.
function btv.utils.argv(spec)
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

-- ----- btv.utils.str_list -----------------------------------------------------
-- `btv.utils.str_list(spec, what)`: normalize a config value that may be one string
-- or a list of strings — a bare string becomes a one-element list, a list of
-- strings passes through, `nil` becomes `{}` (the caller's default applies).
-- Anything else raises, with `what` naming the option in the message (e.g.
-- `"btv.snippet.setup: jump_next"`). The shared normalizer behind key-spec options
-- (`btv.complete.setup` / `btv.snippet.setup`), public for any config surface that
-- accepts `string | string[]`.
function btv.utils.str_list(spec, what)
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

-- ----- btv.utils.caller_source ------------------------------------------------
-- `btv.utils.caller_source()`: the source path of the file whose code called into
-- the current function — walks up the Lua stack to the nearest real-file frame.
-- bemtvi sources every config / plugin / `require`d file with an `@<path>` chunk
-- name, while the embedded prelude chunks are named `bemtvi:prelude/*` (no `@`) and
-- C frames carry `=[C]` — so the first `@`-prefixed source above the caller is the
-- file whose code is running. `nil` when nothing on the stack is `@`-named.
-- Behind vim's `<sfile>` / `<script>` (`btv.expand`) and the plugin-persistence
-- namespace attribution (`btv._resolve_namespace`).
function btv.utils.caller_source()
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

-- ----- btv.utils.debounce -----------------------------------------------------
-- `btv.utils.debounce(fn, ms)`: wrap `fn` into a trailing-edge debounce over
-- `btv.timer` — the returned value runs `fn` once, `ms` after the LAST call, so a
-- burst of rapid calls collapses to a single invocation with the most recent
-- arguments. A timing/control-flow helper (which-key's show-delay, on-change
-- handlers, resize / scroll reactions); it runs nothing on the input path.
--
-- It is callback-shaped, NOT promise-shaped: debounce coalesces a stream of many
-- calls, whereas a promise models one eventual value — different jobs. They
-- compose, though: pass an `btv.async` function as `fn` to kick awaitable work after
-- the quiet period, and reach for `btv.promise.delay` when you want an *await-able*
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
function btv.utils.debounce(fn, ms)
  if type(fn) ~= "function" then
    error("btv.utils.debounce: fn must be a function", 2)
  end
  ms = ms or 0
  local timer -- the armed btv.timer handle while a call is pending, else nil
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
      timer = btv.timer(fire, ms)
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
