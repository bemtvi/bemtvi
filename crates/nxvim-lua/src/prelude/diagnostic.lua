-- nxvim Lua prelude — nx.diagnostic [alias vim.diagnostic].
-- The Lua diagnostics surface (nx.diagnostic.get / goto / setloclist / config) over the mirror the server pushes.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `nx.*` layered on the Rust bridge.

local vim = vim

-- ----- nx.diagnostic: the Lua diagnostics surface ---------------------------
-- `get` reads the Rust→Lua mirror (`nx._diagnostics`, keyed by bufnr, refreshed
-- on every publishDiagnostics via `nx._set_diagnostics`); the actions
-- (`goto_next`/`goto_prev`/`setloclist`) and `config` enqueue an `LspOp` the
-- server applies, reusing the native cursor-move / panel / underline paths.
nx.diagnostic = nx.diagnostic or {}

-- Severity is numbered 1=ERROR…4=HINT (neovim), and the table reverse-maps the
-- number back to its name (`nx.diagnostic.severity[1] == "ERROR"`).
nx.diagnostic.severity = {
  ERROR = 1,
  WARN = 2,
  INFO = 3,
  HINT = 4,
  [1] = "ERROR",
  [2] = "WARN",
  [3] = "INFO",
  [4] = "HINT",
}

-- The mirror the server pushes into; keyed by bufnr → list of diagnostic tables.
nx._diagnostics = nx._diagnostics or {}
function nx._set_diagnostics(bufnr, list)
  nx._diagnostics[bufnr or 0] = list or {}
end

-- Client-set diagnostics (vim.diagnostic.set), keyed by bufnr → namespace → list.
-- Kept apart from the LSP-pushed mirror above so a plugin that manages its own
-- diagnostics (a plugin's view, in its own scratch buffer + namespace)
-- never collides with a file buffer's LSP diagnostics. `get` merges both.
-- These DO render: every `set`/`reset` flattens the buffer's namespaces and
-- pushes them across to the server's render store (`nx._set_client_diagnostics`),
-- so the underline / virtual-text / sign surfaces paint them next to the LSP set.
nx._diagnostics_ns = nx._diagnostics_ns or {}

-- Flatten `bufnr`'s client-set diagnostics across every namespace into one list
-- and push it to the server's render store (replace semantics — an empty list
-- clears the buffer). Each entry is normalized to the {lnum,col,end_lnum,end_col,
-- severity,message,source} shape the Rust bridge reads; `col`/`end_col` are native
-- byte columns. Called after any `set`/`reset` so the painted set tracks the table.
local function diag_sync_client(bufnr)
  local flat = {}
  local byns = nx._diagnostics_ns[bufnr]
  if byns then
    for _, list in pairs(byns) do
      for _, d in ipairs(list) do
        local lnum = d.lnum or 0
        local col = d.col or 0
        flat[#flat + 1] = {
          lnum = lnum,
          col = col,
          end_lnum = d.end_lnum or lnum,
          end_col = d.end_col or col,
          severity = d.severity or nx.diagnostic.severity.ERROR,
          message = d.message or "",
          source = d.source,
        }
      end
    end
  end
  nx._set_client_diagnostics(bufnr, flat)
end

local function diag_current_bufnr()
  return nx._cur_buf and nx._cur_buf.bufnr or 0
end

-- Does namespace `ns` satisfy a `get`/`reset` `namespace` filter? `want` is nil
-- (any), a single id, or a list of ids.
local function diag_ns_wanted(ns, want)
  if want == nil then
    return true
  end
  if type(want) == "table" then
    for _, w in ipairs(want) do
      if w == ns then
        return true
      end
    end
    return false
  end
  return ns == want
end

-- nx.diagnostic.get([bufnr, [opts]]): diagnostics as plain tables. `nil` bufnr →
-- every buffer; `0` → the current one. `opts.severity` (a number) filters. The
-- entries are copied out (callers must not mutate the mirror), each tagged with
-- its `bufnr`, matching neovim's shape.
function nx.diagnostic.get(bufnr, opts)
  opts = opts or {}
  local out = {}
  local function take(b, d)
    if opts.severity == nil or d.severity == opts.severity then
      local copy = { bufnr = b }
      for k, v in pairs(d) do
        copy[k] = v
      end
      out[#out + 1] = copy
    end
  end
  local function collect(b)
    -- `opts.namespace` (a single id or a list) restricts to client-set
    -- diagnostics in those namespaces; absent, every source is merged (the
    -- LSP-pushed mirror plus all client namespaces), matching neovim.
    if opts.namespace == nil then
      for _, d in ipairs(nx._diagnostics[b] or {}) do
        take(b, d)
      end
    end
    local byns = nx._diagnostics_ns[b]
    if byns then
      for ns, list in pairs(byns) do
        if diag_ns_wanted(ns, opts.namespace) then
          for _, d in ipairs(list) do
            take(b, d)
          end
        end
      end
    end
  end
  if bufnr == nil then
    local seen = {}
    for b in pairs(nx._diagnostics) do
      seen[b] = true
      collect(b)
    end
    for b in pairs(nx._diagnostics_ns) do
      if not seen[b] then
        collect(b)
      end
    end
  else
    if bufnr == 0 then
      bufnr = diag_current_bufnr()
    end
    collect(bufnr)
  end
  return out
end

function nx.diagnostic.goto_next(opts)
  opts = opts or {}
  nx._diagnostic_goto(true, opts.severity)
end

function nx.diagnostic.goto_prev(opts)
  opts = opts or {}
  nx._diagnostic_goto(false, opts.severity)
end

-- Severity (1=ERROR…4=HINT) → quickfix type char, matching neovim's
-- `vim.diagnostic.toqflist`.
local SEVERITY_TYPE = { "E", "W", "I", "N" }

-- nx.diagnostic.toqflist(diagnostics): convert a list of diagnostic tables (the
-- shape `nx.diagnostic.get` returns) into quickfix/location-list items. Mirrors
-- neovim's `vim.diagnostic.toqflist`: 0-based diagnostic positions become 1-based
-- list columns, the message becomes the entry text, and severity maps to the type
-- char. The buffer name is resolved into `filename` so the entry is jumpable.
function nx.diagnostic.toqflist(diagnostics)
  local items = {}
  for _, d in ipairs(diagnostics or {}) do
    local fname = nil
    if d.bufnr then
      local ok, name = pcall(vim.api.nvim_buf_get_name, d.bufnr)
      if ok and name ~= "" then
        fname = name
      end
    end
    items[#items + 1] = {
      bufnr = d.bufnr,
      filename = fname,
      lnum = (d.lnum or 0) + 1,
      -- An absent end defaults to the start (neovim's behavior); emitting a
      -- 1-based start with a 0 end would be an invalid backwards range.
      end_lnum = (d.end_lnum or d.lnum or 0) + 1,
      col = (d.col or 0) + 1,
      end_col = (d.end_col or d.col or 0) + 1,
      text = d.message or "",
      type = SEVERITY_TYPE[d.severity] or "E",
    }
  end
  return items
end

-- nx.diagnostic.setqflist([opts]): replace the quickfix list with every buffer's
-- diagnostics and (unless `opts.open == false`) open the quickfix window. `opts`:
-- `severity` filter, `title`, `open`.
function nx.diagnostic.setqflist(opts)
  opts = opts or {}
  local diags = nx.diagnostic.get(nil, { severity = opts.severity })
  local items = nx.diagnostic.toqflist(diags)
  nx.setqflist({}, " ", { title = opts.title or "Diagnostics", items = items })
  if opts.open ~= false then
    vim.cmd("copen")
  end
end

-- nx.diagnostic.setloclist([opts]): populate the current window's location list
-- with the current buffer's diagnostics (a real, navigable loclist now that one
-- exists) and open it. `opts`: `bufnr` (default current), `severity`, `title`,
-- `open`.
function nx.diagnostic.setloclist(opts)
  opts = opts or {}
  -- Capture the owner window explicitly so the list lands on this window even
  -- though the `:lopen` below moves focus into the (new) loclist window.
  local win = nx.win.current()
  local diags = nx.diagnostic.get(opts.bufnr or 0, { severity = opts.severity })
  local items = nx.diagnostic.toqflist(diags)
  nx.setloclist(win, {}, " ", { title = opts.title or "Diagnostics", items = items })
  if opts.open ~= false then
    vim.cmd("lopen")
  end
end

-- nx.diagnostic.open_float([opts]): open a float listing the cursor line's
-- diagnostics in full (the multi-line messages with source/code that the inline
-- virtual text truncates). The server reads the cursor at apply time and routes
-- through the float surface (the bottom panel hover uses); a clean line opens
-- nothing. `opts` (scope/severity filters, formatting) is not yet honored — the
-- default cursor-line scope is what nxvim shows.
function nx.diagnostic.open_float(_opts)
  nx._diagnostic_open_float()
end

-- The built-in gutter sign glyphs, indexed 1=ERROR…4=HINT (neovim's `E`/`W`/`I`/
-- `H` letters), overridden per-severity by the `signs.text` map.
local DEFAULT_SIGN_TEXT = { "E", "W", "I", "H" }

-- nx.diagnostic.config([opts]): merge `opts` into the stored config and return
-- the merged table when called bare. nxvim renders three surfaces — the underline
-- spans (`underline`), the inline end-of-line message (`virtual_text`), and the
-- gutter sign column (`signs`) — so those keys drive rendering; the rest are
-- stored without behavior until a surface exists.
-- INCOMPLETE: `virtual_lines`, `severity_sort`, … are recorded and echoed back
-- but have no rendering surface yet (`float` is honored by
-- `nx.diagnostic.open_float`, but the `config.float` defaults that pre-style it
-- are not). `_namespace` is ignored (one global config). Faithful once those
-- diagnostic surfaces exist.
nx.diagnostic._config = { underline = true, virtual_text = false, signs = true }
function nx.diagnostic.config(opts, _namespace)
  if opts == nil then
    return nx.diagnostic._config
  end
  for k, v in pairs(opts) do
    nx.diagnostic._config[k] = v
  end
  -- `underline`/`virtual_text`/`signs` are true/false/table (a table is an
  -- enabled, filtered form); only an explicit `false`/`nil` disables. The
  -- virt-text `prefix` rides the `virtual_text` table form (default `■ `); the
  -- per-severity sign glyphs ride the `signs.text` map (default `E`/`W`/`I`/`H`).
  local vt = nx.diagnostic._config.virtual_text
  local prefix = "■ "
  if type(vt) == "table" and vt.prefix ~= nil then
    prefix = tostring(vt.prefix)
  end
  local signs = nx.diagnostic._config.signs
  local sign_text =
    { DEFAULT_SIGN_TEXT[1], DEFAULT_SIGN_TEXT[2], DEFAULT_SIGN_TEXT[3], DEFAULT_SIGN_TEXT[4] }
  if type(signs) == "table" and type(signs.text) == "table" then
    for sev = 1, 4 do
      if signs.text[sev] ~= nil then
        sign_text[sev] = tostring(signs.text[sev])
      end
    end
  end
  nx._diagnostic_config(
    nx.diagnostic._config.underline ~= false,
    vt ~= false and vt ~= nil,
    prefix,
    signs ~= false and signs ~= nil,
    sign_text
  )
end

-- nx.diagnostic.set(namespace, bufnr, diagnostics[, opts]): replace the
-- diagnostics for (namespace, bufnr). `bufnr` 0 means the current buffer. The
-- entries are stored as given (each typically { lnum, col, message, severity });
-- `opts` (display overrides) is accepted but not yet honored — see the INCOMPLETE
-- note on nx._diagnostics_ns. A plugin drives its own
-- buffer's diagnostics through this.
function nx.diagnostic.set(namespace, bufnr, diagnostics, _opts)
  if bufnr == 0 then
    bufnr = diag_current_bufnr()
  end
  local byns = nx._diagnostics_ns[bufnr]
  if not byns then
    byns = {}
    nx._diagnostics_ns[bufnr] = byns
  end
  byns[namespace] = diagnostics or {}
  diag_sync_client(bufnr)
end

-- nx.diagnostic.reset([namespace, [bufnr]]): clear client-set diagnostics. With
-- no args, every namespace in every buffer; with a namespace, that namespace in
-- every buffer (or just `bufnr` when given). A plugin calls this when its
-- float closes — it used to crash because the function was missing.
function nx.diagnostic.reset(namespace, bufnr)
  if namespace == nil then
    -- Collect the affected buffers before wiping so each can be re-synced empty.
    local bufs = {}
    for b in pairs(nx._diagnostics_ns) do
      bufs[#bufs + 1] = b
    end
    nx._diagnostics_ns = {}
    for _, b in ipairs(bufs) do
      diag_sync_client(b)
    end
    return
  end
  if bufnr ~= nil then
    if bufnr == 0 then
      bufnr = diag_current_bufnr()
    end
    local byns = nx._diagnostics_ns[bufnr]
    if byns then
      byns[namespace] = nil
    end
    diag_sync_client(bufnr)
    return
  end
  for b, byns in pairs(nx._diagnostics_ns) do
    byns[namespace] = nil
    diag_sync_client(b)
  end
end

-- Muscle-memory alias: `vim.diagnostic` is the same table as the canonical
-- `nx.diagnostic` defined above.
vim.diagnostic = nx.diagnostic
