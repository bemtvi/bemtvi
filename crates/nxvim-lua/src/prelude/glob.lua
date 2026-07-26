-- nxvim:prelude/glob — the `nx.glob.*` surface: shell/gitignore-style path patterns,
-- compiled to a Rust regex and cached.
--
-- Lua has no glob support, and its patterns are not a substitute — `string.match`
-- has no alternation, no `**`, and a different escaping model, so every plugin that
-- wanted "does this path look like X" grew its own translator. This is the one
-- engine: `globset` (the crate behind ripgrep's `--glob`) parses the pattern, the
-- result is compiled as a regex, and the compiled regex is **cached** by pattern +
-- options. Matching the same glob in a loop costs one parse total, and the whole
-- match runs in Rust.
--
-- The canonical engine is `nxvim_core::glob`; these are thin wrappers over its
-- `nx._glob*` bridges. The syntax and the two deliberate defaults (`*` stops at `/`;
-- path style is an explicit option, never the host's) are documented on
-- `nx.glob.match` below, since that is the entry point most callers read first.

nx.glob = nx.glob or {}

-- `nx.glob.match(pattern, path, opts)` -> boolean: does `path` match the glob
-- `pattern`? The one-shot form — it goes through the same compiled-regex cache as
-- `nx.glob.compile`, so calling it in a loop over many paths does not recompile.
-- Raises on an invalid pattern.
--
-- The glob syntax, shared by every function in this namespace:
--
-- ```
-- *        any run of characters, NOT crossing a `/` (see `literal_separator`)
-- **       any run of characters, crossing `/`  ( `**/x`, `a/**`, `a/**/b` )
--          — and it spans ZERO components too, so `**/x` matches a bare `x`
-- ?        exactly one character — also `/`-stopped, per `literal_separator`
-- [abc]    one of the listed characters       [a-z] a range
-- [!abc]   none of them                       `[^abc]` means the same thing
-- {a,b}    either alternative                 `{a,b/**}` nests
-- \x       a literal `x` (unix style only — in windows style `\` separates)
-- ```
--
-- Two defaults worth knowing:
--
--   * **`*` stops at `/`.** `nx.glob.match("*.lua", "a/b/c.lua")` is `false`; say
--     `"**/*.lua"`, or pass `basename = true` to match the path's last component.
--     This is shell / gitignore / LSP semantics. Pass `literal_separator = false`
--     for vim's autocmd rule, where `*` does cross `/`.
--   * **Path style is explicit, not the host's.** A pattern and its candidates are
--     unix-style (`/` separates, `\` escapes) unless you pass `style = "windows"`,
--     which makes `\` a separator too. It is deliberately NOT derived from the
--     machine nxvim runs on: a daemon or remote session can hand a Unix-hosted
--     editor Windows paths, so the convention belongs to the *path*, not the build.
--
-- An invalid pattern raises. The one exception is an unclosed class (`foo[bar`),
-- which is taken as literal text — a filename may genuinely contain a bracket.
--
-- `path` is matched as **bytes**, not decoded text: a name that is not valid UTF-8 (a
-- latin-1 filename on disk, a name that arrived over the encoding seam) matches by its
-- real bytes rather than raising or being lossily rewritten. The `pattern` must be
-- valid UTF-8 — glob syntax is ASCII, so a pattern that isn't is a mistake worth
-- hearing about.
--
-- opts (all optional):
--
-- ```
-- style             = "unix" | "windows"  -- separator convention (default: the host's)
-- ignorecase        = false               -- ASCII-case-insensitive match
-- literal_separator = true                -- `*`/`?` stop at a separator
-- basename          = false               -- a separator-less pattern matches the
--                                            path's last component
-- empty_alternates  = false               -- allow the empty branch in `{a,}`
-- ```
--
-- ```lua
-- nx.glob.match("*.lua", "init.lua")            --> true
-- nx.glob.match("*.lua", "conf/init.lua")       --> false  ( `*` stops at `/` )
-- nx.glob.match("**/*.lua", "conf/init.lua")    --> true
-- nx.glob.match("*.lua", "conf/init.lua", { basename = true })  --> true
-- nx.glob.match("src/**/*.{rs,toml}", "src/a/b/Cargo.toml")     --> true
-- ```
function nx.glob.match(pattern, path, opts)
  return nx._glob_match(pattern, path, opts)
end

-- `nx.glob.compile(pattern, opts)` -> glob: compile `pattern` into a reusable glob
-- object. Equivalent to `nx.glob.match` for a single test, but worth holding onto
-- when a pattern is stored and matched later (a watcher, a filter kept across
-- events) — it makes the compile explicit and its cost obvious. `opts` is as
-- documented on `nx.glob.match`. Raises on an invalid pattern.
--
-- The returned object has:
--
-- ```
-- g:test(path)  -> boolean: does `path` match
-- g:pattern()   -> the glob as written
-- g:regex()     -> the regex source the glob translated to
-- ```
--
-- `g:regex()` is for debugging: a glob that mismatches is far easier to reason about
-- against the regex it actually became. It is the regex *this glob runs*, not a
-- standalone equivalent — under `basename = true` the path is reduced to its last
-- component before the regex sees it, and that reduction is not in the regex. (Which
-- is why `nx.glob.to_regex`, whose job is to hand the translation to another engine,
-- refuses a `basename` glob rather than answering with a regex that means something
-- else.)
--
-- ```lua
-- local ignore = nx.glob.compile("**/target/**")
-- if ignore:test(path) then return end
-- ```
function nx.glob.compile(pattern, opts)
  return nx._glob(pattern, opts)
end

-- `nx.glob.set(patterns, opts)` -> globset: compile a **list** of globs into one
-- matcher. All the patterns share a single regex set, so testing a path against 50
-- globs is one pass rather than 50 — the shape an ignore list or a set of file-watcher
-- patterns wants. `opts` is as documented on `nx.glob.match` and applies to every
-- pattern. An invalid pattern fails the whole set (naming itself), rather than being
-- quietly dropped.
--
-- The returned object has:
--
-- ```
-- s:test(path)     -> boolean: does ANY pattern match
-- s:matches(path)  -> list of the 1-based indices of the patterns that matched
-- s:patterns()     -> the pattern list as written, in set order
-- ```
--
-- ```lua
-- local ignore = nx.glob.set({ "**/.git/**", "**/target/**", "*.tmp" })
-- local kept = nx.tbl.filter(function(p) return not ignore:test(p) end, paths)
-- ```
function nx.glob.set(patterns, opts)
  return nx._glob_set(patterns, opts)
end

-- `nx.glob.any(patterns, path, opts)` -> boolean: does any glob in `patterns` match
-- `path`? The one-shot form of `nx.glob.set` — cached the same way, so a repeated
-- call with the same list reuses the compiled set. `patterns` may also be a single
-- pattern string, so a caller taking "a glob or a list of globs" from user config
-- need not branch.
--
-- ```lua
-- nx.glob.any({ "*.rs", "*.toml" }, "Cargo.toml")  --> true
-- ```
function nx.glob.any(patterns, path, opts)
  -- Checked here rather than left to the bridge: the two branches below would name
  -- two DIFFERENT functions (`nx.glob.match` / `globset:test`) for the same mistake,
  -- neither of them the one the caller actually wrote.
  if type(path) ~= "string" then
    error(("nx.glob.any: path must be a string, got %s"):format(type(path)), 2)
  end
  if type(patterns) == "string" then
    return nx._glob_match(patterns, path, opts)
  end
  return nx._glob_set(patterns, opts):test(path)
end

-- `nx.glob.filter(patterns, paths, opts)` -> list: the entries of `paths` matching
-- any glob in `patterns`, in their original order. `patterns` may be a single
-- pattern string or a list, as in `nx.glob.any`. One compiled set for the whole
-- sweep.
--
-- ```lua
-- nx.async(function()
--   local rs = nx.glob.filter({ "**/*.rs" }, nx.await(nx.fs.walk(".")))
-- end)
-- ```
function nx.glob.filter(patterns, paths, opts)
  local set = type(patterns) == "string" and nx._glob_set({ patterns }, opts)
    or nx._glob_set(patterns, opts)
  local out = {}
  for i, path in ipairs(paths) do
    -- Named and INDEXED here rather than left to the bridge, which can only say
    -- "globset:test: path must be a string" — no help at all for finding which entry
    -- of a long list is the bad one.
    if type(path) ~= "string" then
      error(("nx.glob.filter: paths[%d] must be a string, got %s"):format(i, type(path)), 2)
    end
    if set:test(path) then
      out[#out + 1] = path
    end
  end
  return out
end

-- `nx.glob.to_regex(pattern, opts)` -> string: the regex source `pattern` translates
-- to, without compiling or caching it. For introspection, and for handing the
-- translation to another engine (it is a byte-oriented Rust `regex` pattern, the
-- dialect `nx.regex` speaks). `opts` is as documented on `nx.glob.match`.
--
-- A `basename = true` glob **raises** here instead of answering. `basename` reduces
-- the candidate to its last component before the regex runs; that is a step on the
-- *path*, not something the pattern can express (with `literal_separator = false` a
-- `*` crosses `/`, so no `(?:.*/)?` prefix is faithful). Since this function exists to
-- hand the regex to an engine that will not perform the reduction, returning one
-- anyway would be silently wrong — reduce the path yourself, or drop the option.
--
-- ```lua
-- nx.glob.to_regex("*.lua")  --> "(?-u)^[^/]*\\.lua$"
-- ```
function nx.glob.to_regex(pattern, opts)
  return nx._glob_to_regex(pattern, opts)
end

-- `nx.glob.is_glob(s)` -> boolean: does `s` carry a glob metacharacter (`*` `?` `[`
-- `{`) — i.e. is it a pattern at all, rather than a plain path? The canonical
-- predicate for code that branches on "did the user type a glob here" (expand it) or
-- "a literal path" (use it as-is).
--
-- ```lua
-- nx.glob.is_glob("src/*.rs")   --> true
-- nx.glob.is_glob("src/lib.rs") --> false
-- ```
function nx.glob.is_glob(s)
  return nx._glob_is_glob(s)
end
