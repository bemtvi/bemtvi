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
function vim.fs.joinpath(...) return (table.concat({ ... }, "/"):gsub("//+", "/")) end

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
  end,
    nil,
    start
end

-- Does `path` exist on disk (file or directory)? `getftime` stats both and
-- returns -1 only when the path can't be stat'd.
local function fs_exists(path) return vim.fn.getftime(path) ~= -1 end

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
    if matches(entry, dir) then results[#results + 1] = vim.fs.joinpath(dir, entry) end
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

function vim.uri_from_bufnr(bufnr) return vim.uri_from_fname(vim.api.nvim_buf_get_name(bufnr)) end

-- ----- additional vim.fn -----------------------------------------------------

-- vim.fn.bufname(bufnr): the buffer's name, snapshot-backed via nvim_buf_get_name.
function vim.fn.bufname(bufnr) return vim.api.nvim_buf_get_name(bufnr or 0) end

-- vim.fn.fnamemodify(fname, mods): apply vim's filename modifiers left to right.
-- Supported: `:p` (make absolute against cwd), `:~` (relative to $HOME with `~`),
-- `:.` (relative to cwd when under it), `:h` (head/dir), `:t` (tail), `:r` (root,
-- strip one extension — a leading dot isn't one), `:e` (extension; consecutive
-- `:e` widen it to the last k dot-components, vim's quirk). An unsupported
-- modifier (`:s///`, `:gs`, `:8`, …) errors loud rather than silently passing
-- the name through. Cases match real neovim's vim.fn.fnamemodify.
function vim.fn.fnamemodify(fname, mods)
  fname = fname or ""
  mods = mods or ""
  local i, n = 1, #mods
  while i <= n do
    local m = mods:sub(i, i + 1)
    if m == ":p" then
      if fname == "" then
        fname = vim.fn.getcwd()
      elseif fname:sub(1, 1) ~= "/" then
        fname = vim.fn.getcwd() .. "/" .. fname
      end
      i = i + 2
    elseif m == ":~" then
      local home = os.getenv("HOME") or ""
      if home ~= "" and (fname == home or fname:sub(1, #home + 1) == home .. "/") then
        fname = "~" .. fname:sub(#home + 1)
      end
      i = i + 2
    elseif m == ":." then
      local cwd = vim.fn.getcwd()
      if cwd ~= "" and fname:sub(1, #cwd + 1) == cwd .. "/" then fname = fname:sub(#cwd + 2) end
      i = i + 2
    elseif m == ":h" then
      local head = fname:match("^(.*)/[^/]*$")
      if head == nil then
        fname = "."
      elseif head == "" then
        fname = "/"
      else
        fname = head
      end
      i = i + 2
    elseif m == ":t" then
      fname = fname:match("[^/]*$") or ""
      i = i + 2
    elseif m == ":r" then
      -- Strip the last extension of the tail component (a leading dot isn't one).
      local dir, tail = fname:match("^(.*/)([^/]*)$")
      if not tail then
        dir, tail = "", fname
      end
      for p = #tail, 2, -1 do
        if tail:sub(p, p) == "." then
          tail = tail:sub(1, p - 1)
          break
        end
      end
      fname = dir .. tail
      i = i + 2
    elseif m == ":e" then
      -- Count the run of consecutive `:e`; k of them widen the extension to its
      -- last k dot-separated components (capped at the count of extensions).
      local k = 0
      while mods:sub(i, i + 1) == ":e" do
        k = k + 1
        i = i + 2
      end
      local tail = fname:match("[^/]*$") or ""
      local dots = {}
      for p = 2, #tail do
        if tail:sub(p, p) == "." then dots[#dots + 1] = p end
      end
      if #dots == 0 then
        fname = ""
      else
        local idx = #dots - k + 1
        if idx < 1 then idx = 1 end
        fname = tail:sub(dots[idx] + 1)
      end
    else
      error("fnamemodify(): unsupported modifier '" .. mods:sub(i) .. "'", 2)
    end
  end
  return fname
end

-- A few more vim.fn used only inside deferred callbacks (handlers / user
-- commands) nxvim doesn't drive yet. `finddir` faithfully reuses the Rust-backed
-- directory search via vim.fs, so it stays. `bufnr` resolves against the Phase-6
-- buffer mirror; the register/quickfix/prompt ones can't be honored without those
-- UIs, so they raise via vim._notimpl rather than silently dropping the write.
function vim.fn.finddir(name, path)
  local hit =
    vim.fs.find(name, { path = path or vim.fn.getcwd(), upward = true, type = "directory" })[1]
  return hit or ""
end

-- vim.fn.bufnr(expr): the buffer number for `expr`. "" / "%" / nil / 0 -> current
-- buffer; "$" -> the last (largest) buffer number; a string -> the loaded buffer
-- whose name matches (exact, else suffix), -1 when none. Backed by the Phase-6
-- `vim._bufs` mirror.
function vim.fn.bufnr(expr)
  if expr == nil or expr == 0 or expr == "" or expr == "%" then
    return (vim._cur_buf or {}).bufnr or 0
  end
  if expr == "$" then
    local max = 0
    for id in pairs(vim._bufs or {}) do
      if id > max then max = id end
    end
    return max
  end
  if type(expr) == "number" then return vim._bufs[expr] and expr or -1 end
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
-- The read-only special registers `vim.fn.setreg` refuses to write: search `/`,
-- last-insert `.`, filename `%`, last-command `:`, expression `=`, alternate `#`.
-- nxvim can't honor a write to these (their value projects from live editor
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

-- vim.fn.setreg(name, value [, options]): write a register. `name` "" / "@" means
-- the unnamed register `"`. `value` is a string (charwise) or a list of strings
-- (one per line, linewise). `options` is a string of flags: c/v charwise, l/V
-- linewise, a/A append; b / <C-v> (blockwise) is rejected (no visual-block mode
-- yet). An uppercase register name also appends. A string ending in a newline is
-- linewise when no type flag forces otherwise. Returns 0 on success (1 is vim's
-- failure code, but the failure cases here raise instead). The write is queued
-- for the server (`vim._set_reg`) and write-through the mirror so a getreg later
-- in the same chunk is consistent.
function vim.fn.setreg(name, value, options)
  name = tostring(name)
  if name == "" or name == "@" then name = '"' end
  local reg = name:sub(1, 1)
  if SETREG_READONLY[reg] then error("E354: Invalid register name: '" .. reg .. "'") end

  local linewise, append = false, false
  local text
  if type(value) == "table" then
    -- A list is linewise: each item is a line, with a trailing newline so the
    -- last item is a whole line too.
    text = table.concat(value, "\n")
    if #value > 0 then text = text .. "\n" end
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
  if reg:match("%u") then append = true end
  -- A trailing newline on a plain string makes it linewise (vim), unless a flag
  -- already decided the type.
  if type(value) ~= "table" and not type_given and text:sub(-1) == "\n" then linewise = true end

  local lower = reg:lower()
  vim._registers = vim._registers or {}
  local t = linewise and "V" or "v"
  if append and vim._registers[lower] then
    local prev = vim._registers[lower]
    vim._registers[lower] = {
      text = prev.text .. text,
      type = (prev.type == "V" or linewise) and "V" or "v",
    }
  else
    vim._registers[lower] = { text = text, type = t }
  end
  vim._set_reg(lower, text, linewise, append)
  return 0
end

-- vim.fn.getreg(name [, ...]): the text stored in register `name` ("" / "@" /
-- nil = the unnamed register), or "" when the register is empty / unset. Reads
-- the `vim._registers` mirror the server refreshes before this chunk; an
-- uppercase name reads its lowercase store, matching vim.
function vim.fn.getreg(name)
  name = tostring(name or '"')
  if name == "" or name == "@" then name = '"' end
  local reg = name:sub(1, 1):lower()
  local entry = (vim._registers or {})[reg]
  return entry and entry.text or ""
end

-- vim.fn.getregtype(name): "v" (charwise), "V" (linewise), or "" for an unknown
-- register. An empty / unset (but valid) register is charwise -> "v", matching
-- vim. Blockwise ("<C-v>{width}") waits on visual-block mode.
function vim.fn.getregtype(name)
  name = tostring(name or '"')
  if name == "" or name == "@" then name = '"' end
  local reg = name:sub(1, 1):lower()
  local entry = (vim._registers or {})[reg]
  return entry and entry.type or "v"
end
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
  -- Resume the OUTERMOST driver, not `co` directly: under a `vim.co_pcall` the
  -- running coroutine is the inner protected one, and the relay chain (which
  -- holds the protected call's continuation) drives it from the root down.
  -- Resuming the root lets that chain forward the value back to `co`. Without any
  -- co_pcall on the stack the map is empty and the root IS `co` — unchanged.
  local root, drivers = co, vim._co_driver
  while drivers and drivers[root] do
    root = drivers[root]
  end
  local cb = vim._next_cb_id()
  vim._cb_fns[cb] = function(value)
    local ok, err = coroutine.resume(root, value)
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

-- vim.fn.getcharstr([expr]): read one key as a string.
-- * no arg / expr == 0 — BLOCK until a key is typed and return its vim notation
--   (e.g. "f", "<Esc>", "<C-w>"). Like vim.fn.input/confirm it suspends the
--   running coroutine, so it only works inside a coroutine-PUMPED entry (a keymap
--   RHS, :lua chunk, or user command). The next key the server receives resumes
--   it — that key is consumed here rather than routed to the editor, exactly as
--   vim's getchar() pulls from the typeahead.
-- * expr == 1 — PEEK: return the pending typeahead char without waiting, "" when
--   none. nxvim exposes no typeahead between input batches, so this is always ""
--   (an honest "nothing is pending", not a faked value), and it never blocks.
function vim.fn.getcharstr(expr)
  if expr == 1 then return "" end
  return await_prompt(function(cb) vim._getchar(cb) end) or ""
end

-- vim.wait(time, callback, interval, fast_only): block the calling chunk for up
-- to `time` ms, polling `callback` every `interval` ms (default 200) and returning
-- as soon as it is truthy. This is a REAL pump, not a sleep that ignores the
-- condition: while parked, the server keeps draining its event loop — timers fire,
-- scheduled work runs — so a condition driven by other async work (e.g. nvim-cmp's
-- throttle timer flipping its `running` flag) is actually observed. We implement it
-- on the same park/resume spine as the prompts above: a repeating poll timer ticks
-- on the loop while the coroutine is suspended, and the first tick that satisfies
-- the condition (or reaches the deadline) resumes us with the verdict.
--
-- Like input/getchar it suspends the running coroutine, so it only works inside a
-- coroutine-pumped entry (a :lua chunk, keymap, or user command); a bare callback
-- (timer/schedule/autocmd) has nothing to suspend and fails loud rather than
-- busy-spinning the single server thread into a deadlock (the timers that would
-- satisfy the condition can't fire while a callback monopolizes the thread).
--
-- Returns (true) when `callback` returned truthy within `time`; (false, -1) on
-- timeout. With no `callback` it waits the full `time` then returns (false, -1) — a
-- plain timed sleep. (`fast_only` is accepted and ignored: nxvim has no fast-event
-- context to restrict to, so every wait already permits all work.)
function vim.wait(time, callback, interval, _fast_only)
  time = tonumber(time) or 0
  interval = tonumber(interval) or 200
  if interval < 1 then interval = 1 end

  -- An already-satisfied condition never parks (matches neovim, and keeps a
  -- `vim.wait(0, cond)` poll cheap). A non-positive timeout that isn't already
  -- satisfied times out at once.
  if callback and callback() then return true end
  if time <= 0 then return false, -1 end

  local co = coroutine.running()
  if not co then
    error(
      "vim.wait requires a synchronous pumped context "
        .. "(a :lua chunk, keymap, or command); it cannot block in a callback",
      0
    )
  end
  -- Resume the OUTERMOST driver (the await_prompt rationale): under a vim.co_pcall
  -- the running coroutine is the inner protected one; the relay chain holds the
  -- continuation and drives it from the root down.
  local root, drivers = co, vim._co_driver
  while drivers and drivers[root] do
    root = drivers[root]
  end

  local deadline = vim.uv.now() + time
  local id = vim._next_cb_id()
  local result
  local function finish(value)
    if result ~= nil then return end -- guard against a double resume
    result = value
    vim._timer_active[id] = nil
    vim._timer_stop(id)
    vim._cb_fns[id] = nil
    local ok, err = coroutine.resume(root, value)
    if not ok then error(err, 0) end
  end
  vim._cb_fns[id] = function()
    if callback and callback() then return finish(true) end
    if vim.uv.now() >= deadline then return finish(false) end
    -- otherwise leave the repeating poll timer armed for the next tick
  end
  vim._timer_active[id] = true
  -- First tick no later than the deadline; thereafter every `interval` ms.
  vim._timer_start(id, math.min(interval, time), interval)
  if coroutine.yield() then
    return true
  else
    return false, -1
  end
end
