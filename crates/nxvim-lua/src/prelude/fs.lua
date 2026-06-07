-- nxvim Lua prelude — filesystem surface.
-- vim.fs path helpers, vim.uri encoders, and the filesystem-facing vim.fn.* the load path calls.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- vim.fs: path helpers --------------------------------------------------
-- The subset of neovim's `vim.fs` the real `lsp/<server>.lua` config files reach
-- for to resolve a workspace root. Pure string/path math layered over the
-- Rust-backed `vim._readdir` / `vim.fn.getftime` / `vim.fn.getcwd` primitives.

vim.fs = vim.fs or {}

-- Join path segments with `/`, collapsing duplicate separators.
function vim.fs.joinpath(...)
  return (table.concat({ ... }, "/"):gsub("//+", "/"))
end

-- Expand a leading `~` and collapse duplicate / trailing slashes. (Minimal: no
-- `..` resolution — the config files don't need it.)
function vim.fs.normalize(path, _opts)
  if type(path) ~= "string" then return path end
  if path == "~" or vim.startswith(path, "~/") then
    path = (os.getenv("HOME") or "") .. path:sub(2)
  end
  path = path:gsub("//+", "/")
  if #path > 1 then path = (path:gsub("/$", "")) end
  return path
end

-- The directory part of `path` ("." when there is none, "/" at the root).
function vim.fs.dirname(path)
  if not path or path == "" then return "." end
  path = path:gsub("/+$", "")
  -- All slashes (the root "/") stripped to "": root's parent is itself, so the
  -- upward walks in vim.fs.root / vim.fs.parents terminate at "/" instead of
  -- escaping to "." (the cwd).
  if path == "" then return "/" end
  local dir = path:match("^(.*)/[^/]*$")
  if dir == nil then return "." end
  if dir == "" then return "/" end
  return dir
end

-- The final component of `path`.
function vim.fs.basename(path)
  if not path then return nil end
  return (path:gsub("/+$", ""):match("[^/]*$"))
end

-- Iterate the ancestors of `start` (each parent in turn, excluding `start`),
-- usable as `for dir in vim.fs.parents(path) do … end`.
function vim.fs.parents(start)
  return function(_, dir)
    local parent = vim.fs.dirname(dir)
    if parent == dir then return nil end
    return parent
  end, nil, start
end

-- Does `path` exist on disk (file or directory)? `getftime` stats both and
-- returns -1 only when the path can't be stat'd.
local function fs_exists(path)
  return vim.fn.getftime(path) ~= -1
end

-- vim.fs.find(names, opts): find paths matching `names` (a name, list of names,
-- or `function(name, path)` predicate). `opts.upward` walks ancestors of
-- `opts.path` (default cwd); otherwise it descends breadth-first. `opts.limit`
-- caps results (default 1). Enough for the root_dir helpers configs use.
function vim.fs.find(names, opts)
  opts = opts or {}
  local matches
  if type(names) == "function" then
    matches = names
  else
    local list = type(names) == "table" and names or { names }
    matches = function(n) return vim.tbl_contains(list, n) end
  end
  local path = opts.path or vim.fn.getcwd()
  local limit = opts.limit or 1
  local results = {}
  local function consider(dir, entry)
    if matches(entry, dir) then
      results[#results + 1] = vim.fs.joinpath(dir, entry)
    end
  end
  if opts.upward then
    local dir = path
    while dir do
      for _, entry in ipairs(vim._readdir(dir)) do
        consider(dir, entry)
        if #results >= limit then return results end
      end
      local parent = vim.fs.dirname(dir)
      if parent == dir then break end
      dir = parent
    end
  else
    local queue, scanned = { path }, 0
    while #queue > 0 and scanned < 4096 do
      local dir = table.remove(queue, 1)
      scanned = scanned + 1
      for _, entry in ipairs(vim._readdir(dir)) do
        local full = vim.fs.joinpath(dir, entry)
        consider(dir, entry)
        if #results >= limit then return results end
        if vim.fn.isdirectory(full) == 1 then queue[#queue + 1] = full end
      end
    end
  end
  return results
end

-- vim.fs.root(source, marker): the nearest ancestor of `source` (a path, or a
-- bufnr — 0/snapshot resolves to the current buffer's name, else cwd) that holds
-- `marker`. `marker` is a filename, a `function(name, path)` predicate, or a
-- list. A LIST is an ordered priority chain (neovim 0.11): each element is a
-- *tier* tried in turn — the highest-priority tier with a match anywhere up the
-- tree wins, regardless of depth. A tier that is itself a list groups names of
-- EQUAL priority (closest ancestor with any of them wins). So
-- `{ 'a', { 'b', 'c' }, 'd' }` means: prefer 'a'; else 'b'-or-'c'; else 'd'.
-- Returns nil if none match. This is what the vendored `lsp/<server>.lua` files
-- call to compute their `root_dir`.
function vim.fs.root(source, marker)
  local path
  if type(source) == "number" then
    path = vim.api.nvim_buf_get_name(source)
    if path == nil or path == "" then path = vim.fn.getcwd() end
  else
    path = source
  end
  path = vim.fs.normalize(path)
  -- Start at the path's directory when it is a file.
  local start = path
  if vim.fn.isdirectory(path) == 0 then start = vim.fs.dirname(path) end
  -- Normalize `marker` into the ordered list of tiers; each tier is a list of
  -- equal-priority names (or predicates). A bare string/function is one tier; a
  -- list marker is one tier per element (an element that is itself a list is a
  -- single equal-priority tier).
  local tiers
  if type(marker) == "table" then
    tiers = {}
    for _, m in ipairs(marker) do
      tiers[#tiers + 1] = type(m) == "table" and m or { m }
    end
  else
    tiers = { { marker } }
  end
  for _, names in ipairs(tiers) do
    local dir = start
    while dir do
      for _, m in ipairs(names) do
        if type(m) == "function" then
          for _, entry in ipairs(vim._readdir(dir)) do
            if m(entry, dir) then return dir end
          end
        elseif fs_exists(vim.fs.joinpath(dir, m)) then
          return dir
        end
      end
      local parent = vim.fs.dirname(dir)
      if parent == dir then break end
      dir = parent
    end
  end
  return nil
end

-- vim.fs.relpath(base, target): `target` expressed relative to `base`, or nil
-- when `base` is not an ancestor of `target` (the two are compared on a path
-- *segment* boundary, so "/a/b" is not an ancestor of "/a/bc"). Equal paths give
-- ".". Both are normalized first. rust_analyzer's `root_dir` uses it to decide
-- whether a file lives under a toolchain/registry/sysroot directory.
function vim.fs.relpath(base, target, _opts)
  base = vim.fs.normalize(base)
  target = vim.fs.normalize(target)
  if base == target then return "." end
  -- A trailing "/" makes the comparison segment-aligned; normalize strips it
  -- from "/a/b" (len > 1) but leaves the root "/" as-is, which already ends in /.
  local prefix = base
  if prefix:sub(-1) ~= "/" then prefix = prefix .. "/" end
  if target:sub(1, #prefix) == prefix then return target:sub(#prefix + 1) end
  return nil
end

-- ----- vim.uri ---------------------------------------------------------------
-- Minimal `file://` URI conversion. (The server does its own, encoding-aware URI
-- handling for actual LSP traffic; these back config-file path computations.)

function vim.uri_from_fname(path)
  path = vim.fs.normalize(path)
  if path:sub(1, 1) ~= "/" then path = "/" .. path end
  return "file://" .. path
end

function vim.uri_to_fname(uri)
  local path = (uri:gsub("^file://", ""))
  return (path:gsub("%%(%x%x)", function(h) return string.char(tonumber(h, 16)) end))
end

function vim.uri_from_bufnr(bufnr)
  return vim.uri_from_fname(vim.api.nvim_buf_get_name(bufnr))
end

-- ----- additional vim.fn -----------------------------------------------------

-- vim.fn.bufname(bufnr): the buffer's name, snapshot-backed via nvim_buf_get_name.
function vim.fn.bufname(bufnr) return vim.api.nvim_buf_get_name(bufnr or 0) end

-- vim.fn.fnamemodify(fname, mods): apply the `:p`/`:h`/`:t`/`:r`/`:e` filename
-- modifiers (left to right) configs use. `:p` absolutizes against cwd.
function vim.fn.fnamemodify(fname, mods)
  local result = fname or ""
  local i = 1
  while i <= #(mods or "") do
    if mods:sub(i, i) == ":" then
      local m = mods:sub(i + 1, i + 1)
      if m == "p" then
        if result:sub(1, 1) ~= "/" then result = vim.fs.joinpath(vim.fn.getcwd(), result) end
      elseif m == "h" then
        result = vim.fs.dirname(result)
      elseif m == "t" then
        result = vim.fs.basename(result)
      elseif m == "r" then
        result = (result:gsub("%.[^./]*$", ""))
      elseif m == "e" then
        result = result:match("%.([^./]*)$") or ""
      end
      i = i + 2
    else
      i = i + 1
    end
  end
  return result
end

-- A few more vim.fn used only inside deferred callbacks (handlers / user
-- commands) nxvim doesn't drive yet. `finddir` faithfully reuses the Rust-backed
-- directory search via vim.fs, so it stays. `bufnr` resolves against the Phase-6
-- buffer mirror; the register/quickfix/prompt ones can't be honored without those
-- UIs, so they raise via vim._notimpl rather than silently dropping the write.
function vim.fn.finddir(name, path)
  local hit = vim.fs.find(name, { path = path or vim.fn.getcwd(), upward = true, type = "directory" })[1]
  return hit or ""
end

-- vim.fn.bufnr(expr): the buffer number for `expr`. "" / "%" / nil / 0 -> current
-- buffer; a string -> the loaded buffer whose name matches (exact, else suffix),
-- -1 when none. Backed by the Phase-6 `vim._bufs` mirror.
function vim.fn.bufnr(expr)
  if expr == nil or expr == 0 or expr == "" or expr == "%" then
    return (vim._cur_buf or {}).bufnr or 0
  end
  if type(expr) == "number" then
    return vim._bufs[expr] and expr or -1
  end
  for bufnr, buf in pairs(vim._bufs) do
    local name = buf.name or ""
    if name == expr or name:sub(-#expr) == expr then return bufnr end
  end
  return -1
end

-- vim.fn.substitute(str, pat, sub, flags): a real vim-regex substitution, backed
-- by the Rust engine (`vim._substitute`) so plugins that rely on vim's magic
-- dialect + replacement syntax (`\(\)`, `\{-}`, `&`, `\1`, `\U…\E`, …) get the
-- same result neovim gives. This is a DIFFERENT dialect from nxvim's `/` search
-- (canonical regex); the divergence is intentional and lives in the `vim.fn.*`
-- compatibility layer. An invalid / unsupported pattern raises (fail loud).
function vim.fn.substitute(str, pat, sub, flags)
  return vim._substitute(tostring(str), tostring(pat), tostring(sub or ""), tostring(flags or ""))
end
function vim.fn.setreg(_name, _value, _opts) vim._notimpl("vim.fn.setreg") end
function vim.fn.setqflist(_list, _action, _what) vim._notimpl("vim.fn.setqflist") end

-- vim.fn.input(opts[, default, completion]) / vim.fn.confirm(msg, choices, …):
-- SYNCHRONOUS prompts — they block the calling chunk and RETURN the user's answer
-- inline (input → the typed string, "" on cancel; confirm → a 1-based button
-- index, 0 on cancel). They drive nxvim's command line (a CmdlineKind::Prompt /
-- ::Confirm) and suspend the running coroutine until the user answers, so they
-- work only inside a coroutine-PUMPED entry — a :lua chunk, a keymap RHS, or a
-- user command, all of which the server runs through vim._pump. Called from a
-- bare callback (a timer / vim.schedule / autocmd), there is no coroutine to
-- suspend, so they fail loud instead of hanging or faking a value.

-- Register a one-shot callback that resumes THIS coroutine with the prompt's
-- result, ask the server to open the prompt (via `open`, which queues the request
-- carrying the callback id), then yield until the result resumes us. Re-raises a
-- throwing continuation so the server surfaces it (E5108).
local function await_prompt(open)
  local co = coroutine.running()
  if not co then
    error(
      "vim.fn.input/confirm requires a synchronous pumped context "
        .. "(a :lua chunk, keymap, or command); it cannot block in a callback",
      0
    )
  end
  local cb = vim._next_cb_id()
  vim._cb_fns[cb] = function(value)
    local ok, err = coroutine.resume(co, value)
    if not ok then error(err, 0) end
  end
  open(cb)
  return coroutine.yield()
end

function vim.fn.input(opts, default, _completion)
  local prompt
  if type(opts) == "table" then
    prompt, default = opts.prompt, opts.default
  else
    prompt = opts
  end
  prompt = tostring(prompt or "")
  default = tostring(default or "")
  local text = await_prompt(function(cb) vim._ui_input(prompt, default, cb) end)
  return text or "" -- a cancelled input is "" (not nil); the contract vs vim.ui.input
end

function vim.fn.confirm(msg, choices, default, _type)
  -- Parse the `\n`-separated choice list. Each choice marks its accelerator with
  -- `&` (e.g. "&Yes"); absent, the first char is the accelerator. Render it
  -- bracketed for display ("&Yes" → "[Y]es") and lowercase it for key matching.
  local accels, labels = {}, {}
  for choice in tostring(choices or "&Ok"):gmatch("[^\n]+") do
    local amp = choice:find("&", 1, true)
    local acc, label
    if amp then
      acc = choice:sub(amp + 1, amp + 1)
      label = choice:sub(1, amp - 1) .. "[" .. acc .. "]" .. choice:sub(amp + 2)
    else
      acc = choice:sub(1, 1)
      label = "[" .. acc .. "]" .. choice:sub(2)
    end
    accels[#accels + 1] = acc:lower()
    labels[#labels + 1] = label
  end
  local label = tostring(msg or "") .. " " .. table.concat(labels, ", ") .. ": "
  default = tonumber(default) or 0
  local idx = await_prompt(function(cb) vim._confirm(label, accels, default, cb) end)
  return tonumber(idx) or 0
end

