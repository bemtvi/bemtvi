-- nxvim Lua prelude — built-in `.editorconfig` support.
--
-- On every file-backed buffer read (BufReadPost / BufNewFile) we walk the
-- directory tree upward from the file, collect every `.editorconfig` along the
-- way (stopping at one that declares `root = true`), match the file path against
-- each `[glob]` section, and apply the merged properties to the buffer's options.
-- All filesystem access goes through the async `nx.fs` seam (`nx.async` +
-- `nx.await`) so it never blocks the editor tick — local on a bare native build,
-- the daemon's `luafs_op` over the wire otherwise, so this works on every front
-- end including the browser edit-host.
--
-- Toggle, mirroring neovim's editorconfig surface:
--   * `vim.g.editorconfig = false` disables it globally (default: on).
--   * `vim.b[bufnr].editorconfig = false` disables it for one buffer; a buffer's
--     explicit value (true/false) overrides the global one.
--
-- Properties honored (the ones that map to a real nxvim option — per the
-- EditorConfig spec, unrecognized/unsupported properties are simply ignored):
--   indent_style  -> 'expandtab'           (tab => off, space => on)
--   indent_size   -> 'shiftwidth'/'softtabstop' (and 'tabstop' when tab_width unset)
--   tab_width     -> 'tabstop'
--   end_of_line   -> 'fileformat'          (lf=>unix, crlf=>dos, cr=>mac)
--   charset       -> 'fileencoding' (+ 'bomb' for utf-8-bom)
-- The write-time transforms (`trim_trailing_whitespace`, `insert_final_newline`)
-- and `max_line_length` are NOT applied: nxvim fires `BufWritePre` *after* the
-- buffer is serialized to disk (it's a notification, not an interception point),
-- so there is no honest hook to rewrite the buffer before a save, and no backing
-- option for the line-length limit. The full resolved property set is still
-- exposed via `nx.editorconfig.properties(bufnr)` for plugins that want them.

nx = nx or {}
nx.editorconfig = nx.editorconfig or {}
local M = nx.editorconfig

-- Resolved property table per bufnr, exposed via `M.properties` (and the source
-- for any plugin acting on properties with no nxvim option).
M._resolved = M._resolved or {}

-- ----- glob matching ---------------------------------------------------------

-- Expand EditorConfig brace groups — `{a,b,c}` alternation and `{m..n}` numeric
-- ranges — into a flat list of plain globs (no braces). Nested groups are handled
-- by re-expanding the substituted result. A single-element group with no comma or
-- range (e.g. `{x}`) is treated as a literal and left in place.
local function expand_braces(g)
  local i = 1
  while i <= #g do
    local c = g:sub(i, i)
    if c == "\\" then
      i = i + 2
    elseif c == "{" then
      -- Scan to the matching `}`, recording top-level comma split points.
      local depth, j = 1, i + 1
      local parts, start = {}, i + 1
      while j <= #g and depth > 0 do
        local cj = g:sub(j, j)
        if cj == "\\" then
          j = j + 1
        elseif cj == "{" then
          depth = depth + 1
        elseif cj == "}" then
          depth = depth - 1
          if depth == 0 then
            break
          end
        elseif cj == "," and depth == 1 then
          parts[#parts + 1] = g:sub(start, j - 1)
          start = j + 1
        end
        j = j + 1
      end
      if depth == 0 then
        local prefix, suffix = g:sub(1, i - 1), g:sub(j + 1)
        if #parts > 0 then
          parts[#parts + 1] = g:sub(start, j - 1)
          local out = {}
          for _, p in ipairs(parts) do
            for _, sub in ipairs(expand_braces(prefix .. p .. suffix)) do
              out[#out + 1] = sub
            end
          end
          return out
        end
        local lo, hi = g:sub(i + 1, j - 1):match("^(-?%d+)%.%.(-?%d+)$")
        if lo then
          lo, hi = tonumber(lo), tonumber(hi)
          local step = (lo <= hi) and 1 or -1
          local out = {}
          for n = lo, hi, step do
            for _, sub in ipairs(expand_braces(prefix .. tostring(n) .. suffix)) do
              out[#out + 1] = sub
            end
          end
          return out
        end
        -- Literal single-element brace: skip past it and keep scanning.
        i = j + 1
      else
        i = i + 1
      end
    else
      i = i + 1
    end
  end
  return { g }
end

-- Parse a `[...]` character class starting at index `gi` (the `[`). Returns a
-- predicate over a single char and the index just past the closing `]`, or nil if
-- there is no closing `]` (so the caller treats `[` as a literal).
local function parse_class(g, gi)
  local j = gi + 1
  local neg = false
  if g:sub(j, j) == "!" then
    neg = true
    j = j + 1
  end
  local ranges = {}
  local first = true
  while j <= #g do
    local cj = g:sub(j, j)
    if cj == "]" and not first then
      break
    end
    first = false
    if cj == "\\" then
      cj = g:sub(j + 1, j + 1)
      ranges[#ranges + 1] = { cj, cj }
      j = j + 2
    elseif
      g:sub(j + 1, j + 1) == "-"
      and g:sub(j + 2, j + 2) ~= "]"
      and g:sub(j + 2, j + 2) ~= ""
    then
      ranges[#ranges + 1] = { cj, g:sub(j + 2, j + 2) }
      j = j + 3
    else
      ranges[#ranges + 1] = { cj, cj }
      j = j + 1
    end
  end
  if j > #g then
    return nil
  end
  return function(ch)
    if ch == "" then
      return false
    end
    local hit = false
    for _, r in ipairs(ranges) do
      if ch >= r[1] and ch <= r[2] then
        hit = true
        break
      end
    end
    if neg then
      return not hit
    end
    return hit
  end,
    j + 1
end

-- Match a brace-free EditorConfig glob `g` against path `p` (both `/`-separated),
-- by recursive backtracking. `*` matches any run without `/`; `**` any run
-- including `/`; `**/` additionally matches zero path segments; `?` one non-`/`
-- char; `[...]` a class. Backslash escapes the next char.
local function match_glob(g, p)
  local function m(gi, pi)
    while gi <= #g do
      local c = g:sub(gi, gi)
      if c == "*" then
        if g:sub(gi + 1, gi + 1) == "*" then
          gi = gi + 2
          if g:sub(gi, gi) == "/" then
            -- `**/`: zero or more whole directory segments.
            local rest = gi + 1
            if m(rest, pi) then
              return true
            end
            for k = pi, #p do
              if p:sub(k, k) == "/" and m(rest, k + 1) then
                return true
              end
            end
            return false
          end
          -- bare `**`: any run including `/`.
          for k = pi, #p + 1 do
            if m(gi, k) then
              return true
            end
          end
          return false
        end
        -- `*`: any run not crossing `/`.
        gi = gi + 1
        for k = pi, #p + 1 do
          if k > pi and p:sub(k - 1, k - 1) == "/" then
            break
          end
          if m(gi, k) then
            return true
          end
        end
        return false
      elseif c == "?" then
        local pc = p:sub(pi, pi)
        if pc == "" or pc == "/" then
          return false
        end
        gi, pi = gi + 1, pi + 1
      elseif c == "[" then
        local pred, nexti = parse_class(g, gi)
        if not pred then
          if p:sub(pi, pi) ~= "[" then
            return false
          end
          gi, pi = gi + 1, pi + 1
        else
          local pc = p:sub(pi, pi)
          if pc == "/" or not pred(pc) then
            return false
          end
          gi, pi = nexti, pi + 1
        end
      elseif c == "\\" then
        if p:sub(pi, pi) ~= g:sub(gi + 1, gi + 1) then
          return false
        end
        gi, pi = gi + 2, pi + 1
      else
        if p:sub(pi, pi) ~= c then
          return false
        end
        gi, pi = gi + 1, pi + 1
      end
    end
    return pi > #p
  end
  return m(1, 1)
end

-- Does section header `glob` (from a `.editorconfig` in some directory) match the
-- file path `rel` (relative to that directory, `/`-separated)? A glob with no `/`
-- matches the basename at any depth (`**/` is implied); a leading `/` anchors to
-- the config directory; any other `/` makes it relative to the config directory.
local function section_matches(glob, rel)
  local g = glob
  if g:sub(1, 1) == "/" then
    g = g:sub(2)
  elseif not g:find("/", 1, true) then
    g = "**/" .. g
  end
  for _, e in ipairs(expand_braces(g)) do
    if match_glob(e, rel) then
      return true
    end
  end
  return false
end

-- ----- parsing ---------------------------------------------------------------

-- Keys whose value is a free-form string and must NOT be lowercased.
-- Parse `.editorconfig` text into `{ root = bool, sections = { {glob=, props=} } }`.
-- Keys and values are lowercased (EditorConfig property names and the values we act
-- on are all case-insensitive); `=` is the sole property delimiter.
local function parse(text)
  local cfg = { root = false, sections = {} }
  local section -- props table of the current `[glob]` (nil = preamble)
  for raw in (text .. "\n"):gmatch("(.-)\r?\n") do
    local line = raw:gsub("^%s+", ""):gsub("%s+$", "")
    local first = line:sub(1, 1)
    if first == "[" and line:sub(-1) == "]" then
      section = {}
      cfg.sections[#cfg.sections + 1] = { glob = line:sub(2, -2), props = section }
    elseif line ~= "" and first ~= "#" and first ~= ";" then
      -- a property line (blanks and `#`/`;` comments fall through, ignored)
      local k, v = line:match("^([^=]+)=(.*)$")
      if k then
        k = k:gsub("%s+$", ""):lower()
        v = v:gsub("^%s+", ""):gsub("%s+$", ""):lower()
        if section then
          section[k] = v
        elseif k == "root" then
          cfg.root = (v == "true")
        end
      end
    end
  end
  return cfg
end

-- ----- application -----------------------------------------------------------

local function dirname(path)
  return (path:gsub("/[^/]*$", ""))
end

-- Apply merged EditorConfig properties to buffer `bufnr`'s options.
local function apply(bufnr, props)
  if props.indent_style == "tab" then
    nx.bo[bufnr].expandtab = false
  elseif props.indent_style == "space" then
    nx.bo[bufnr].expandtab = true
  end

  -- `indent_size = tab` means "follow tab_width"; otherwise it is numeric. When
  -- tab_width is unset it defaults to indent_size (EditorConfig spec).
  local indent_size = props.indent_size
  if indent_size == "tab" then
    indent_size = nil
  else
    indent_size = tonumber(indent_size)
  end
  local tab_width = tonumber(props.tab_width) or indent_size
  if tab_width and tab_width > 0 then
    nx.bo[bufnr].tabstop = tab_width
  end
  if indent_size and indent_size > 0 then
    nx.bo[bufnr].shiftwidth = indent_size
    nx.bo[bufnr].softtabstop = indent_size
  end

  local eol = props.end_of_line
  if eol == "lf" then
    nx.bo[bufnr].fileformat = "unix"
  elseif eol == "crlf" then
    nx.bo[bufnr].fileformat = "dos"
  elseif eol == "cr" then
    nx.bo[bufnr].fileformat = "mac"
  end

  local cs = props.charset
  if cs == "utf-8" or cs == "latin1" or cs == "utf-16le" or cs == "utf-16be" then
    nx.bo[bufnr].fileencoding = cs
  elseif cs == "utf-8-bom" then
    nx.bo[bufnr].fileencoding = "utf-8"
    nx.bo[bufnr].bomb = true
  end
end

-- Resolve the effective properties for `file` and apply them to `bufnr`. Walks
-- the tree upward collecting `.editorconfig` files (nearest first), stops at a
-- `root = true` one, then applies farthest-first so the nearest file wins; within
-- a file, later matching sections override earlier ones.
M._run = nx.async(function(bufnr, file)
  -- (dir, cfg) pairs, nearest directory first.
  local chain = {}
  local dir = dirname(file)
  while dir and dir ~= "" do
    local ok, text = pcall(nx.await, nx.fs.read_text(dir .. "/.editorconfig"))
    if ok and type(text) == "string" then
      local cfg = parse(text)
      chain[#chain + 1] = { dir = dir, cfg = cfg }
      if cfg.root then
        break
      end
    end
    local parent = dirname(dir)
    if parent == dir then
      break
    end
    dir = parent
  end

  -- Merge farthest-first (so nearer overrides), section order preserved.
  local props = {}
  for idx = #chain, 1, -1 do
    local entry = chain[idx]
    local rel = file:sub(#entry.dir + 2) -- strip "<dir>/"
    for _, sec in ipairs(entry.cfg.sections) do
      if section_matches(sec.glob, rel) then
        for k, v in pairs(sec.props) do
          -- `unset` reverts a property to "unspecified".
          props[k] = (v ~= "unset") and v or nil
        end
      end
    end
  end

  M._resolved[bufnr] = props
  if next(props) ~= nil then
    apply(bufnr, props)
  end
end)

-- Is EditorConfig active for this buffer? A buffer-local value wins over the
-- global one; both default to enabled.
local function enabled(bufnr)
  local b = vim.b[bufnr].editorconfig
  if b ~= nil then
    return b ~= false
  end
  if vim.g.editorconfig ~= nil then
    return vim.g.editorconfig ~= false
  end
  return true
end

-- ----- wiring ----------------------------------------------------------------

if vim.g.editorconfig == nil then
  vim.g.editorconfig = true
end

local grp = nx.augroup.create("nxEditorConfig")

local function on_open(ev)
  local bufnr = ev.buf
  local file = ev.file
  if type(file) ~= "string" or file == "" then
    return
  end
  if not enabled(bufnr) then
    return
  end
  -- Fire-and-forget: the async chain settles over the next few ticks.
  M._run(bufnr, file):catch(function(err)
    nx.notify("nx.editorconfig: " .. tostring(err), vim.log.levels.WARN)
  end)
end

nx.on("BufReadPost", { group = grp }, on_open)
nx.on("BufNewFile", { group = grp }, on_open)

-- Drop a deleted buffer's resolved entry (a reused bufnr should start clean).
nx.on("BufDelete", { group = grp }, function(ev)
  M._resolved[ev.buf] = nil
end)

-- The EditorConfig properties resolved for `bufnr` (defaults to the current
-- buffer), as a raw `{ key = value }` table — including ones with no nxvim option
-- (e.g. `trim_trailing_whitespace`, `max_line_length`) so a plugin can act on
-- them. `nil` until the async resolution for that buffer has settled.
function M.properties(bufnr)
  if bufnr == nil or bufnr == 0 then
    bufnr = vim.api.nvim_get_current_buf()
  end
  return M._resolved[bufnr]
end

return M
