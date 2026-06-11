-- nxvim Lua prelude — vim.diagnostic.
-- The Lua diagnostics surface (vim.diagnostic.get / goto / setloclist / config) over the mirror the server pushes.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- vim.diagnostic: the Lua diagnostics surface ---------------------------
-- `get` reads the Rust→Lua mirror (`vim._diagnostics`, keyed by bufnr, refreshed
-- on every publishDiagnostics via `vim._set_diagnostics`); the actions
-- (`goto_next`/`goto_prev`/`setloclist`) and `config` enqueue an `LspOp` the
-- server applies, reusing the native cursor-move / panel / underline paths.
vim.diagnostic = vim.diagnostic or {}

-- Severity is numbered 1=ERROR…4=HINT (neovim), and the table reverse-maps the
-- number back to its name (`vim.diagnostic.severity[1] == "ERROR"`).
vim.diagnostic.severity = {
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
vim._diagnostics = vim._diagnostics or {}
function vim._set_diagnostics(bufnr, list) vim._diagnostics[bufnr or 0] = list or {} end

-- Client-set diagnostics (vim.diagnostic.set), keyed by bufnr → namespace → list.
-- Kept apart from the LSP-pushed mirror above so a plugin that manages its own
-- diagnostics (lazy.nvim's update view, in its own scratch buffer + namespace)
-- never collides with a file buffer's LSP diagnostics. `get` merges both.
-- INCOMPLETE: these are stored and read back by `get`, but not yet rendered —
-- nxvim's underline / virtual-text / sign surfaces are driven from the server's
-- LSP store, which Lua-set diagnostics don't reach. set→get→reset round-trips.
vim._diagnostics_ns = vim._diagnostics_ns or {}

local function diag_current_bufnr() return vim._cur_buf and vim._cur_buf.bufnr or 0 end

-- Does namespace `ns` satisfy a `get`/`reset` `namespace` filter? `want` is nil
-- (any), a single id, or a list of ids.
local function diag_ns_wanted(ns, want)
  if want == nil then return true end
  if type(want) == "table" then
    for _, w in ipairs(want) do
      if w == ns then return true end
    end
    return false
  end
  return ns == want
end

-- vim.diagnostic.get([bufnr, [opts]]): diagnostics as plain tables. `nil` bufnr →
-- every buffer; `0` → the current one. `opts.severity` (a number) filters. The
-- entries are copied out (callers must not mutate the mirror), each tagged with
-- its `bufnr`, matching neovim's shape.
function vim.diagnostic.get(bufnr, opts)
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
      for _, d in ipairs(vim._diagnostics[b] or {}) do
        take(b, d)
      end
    end
    local byns = vim._diagnostics_ns[b]
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
    for b in pairs(vim._diagnostics) do
      seen[b] = true
      collect(b)
    end
    for b in pairs(vim._diagnostics_ns) do
      if not seen[b] then collect(b) end
    end
  else
    if bufnr == 0 then bufnr = diag_current_bufnr() end
    collect(bufnr)
  end
  return out
end

function vim.diagnostic.goto_next(opts)
  opts = opts or {}
  vim._diagnostic_goto(true, opts.severity)
end

function vim.diagnostic.goto_prev(opts)
  opts = opts or {}
  vim._diagnostic_goto(false, opts.severity)
end

function vim.diagnostic.setloclist(_opts) vim._diagnostic_setloclist() end

-- vim.diagnostic.open_float([opts]): open a float listing the cursor line's
-- diagnostics in full (the multi-line messages with source/code that the inline
-- virtual text truncates). The server reads the cursor at apply time and routes
-- through the float surface (the bottom panel hover uses); a clean line opens
-- nothing. `opts` (scope/severity filters, formatting) is not yet honored — the
-- default cursor-line scope is what nxvim shows.
function vim.diagnostic.open_float(_opts) vim._diagnostic_open_float() end

-- The built-in gutter sign glyphs, indexed 1=ERROR…4=HINT (neovim's `E`/`W`/`I`/
-- `H` letters), overridden per-severity by the `signs.text` map.
local DEFAULT_SIGN_TEXT = { "E", "W", "I", "H" }

-- vim.diagnostic.config([opts]): merge `opts` into the stored config and return
-- the merged table when called bare. nxvim renders three surfaces — the underline
-- spans (`underline`), the inline end-of-line message (`virtual_text`), and the
-- gutter sign column (`signs`) — so those keys drive rendering; the rest are
-- stored without behavior until a surface exists.
-- INCOMPLETE: `virtual_lines`, `severity_sort`, … are recorded and echoed back
-- but have no rendering surface yet (`float` is honored by
-- `vim.diagnostic.open_float`, but the `config.float` defaults that pre-style it
-- are not). `_namespace` is ignored (one global config). Faithful once those
-- diagnostic surfaces exist.
vim.diagnostic._config = { underline = true, virtual_text = false, signs = true }
function vim.diagnostic.config(opts, _namespace)
  if opts == nil then return vim.diagnostic._config end
  for k, v in pairs(opts) do
    vim.diagnostic._config[k] = v
  end
  -- `underline`/`virtual_text`/`signs` are true/false/table (a table is an
  -- enabled, filtered form); only an explicit `false`/`nil` disables. The
  -- virt-text `prefix` rides the `virtual_text` table form (default `■ `); the
  -- per-severity sign glyphs ride the `signs.text` map (default `E`/`W`/`I`/`H`).
  local vt = vim.diagnostic._config.virtual_text
  local prefix = "■ "
  if type(vt) == "table" and vt.prefix ~= nil then prefix = tostring(vt.prefix) end
  local signs = vim.diagnostic._config.signs
  local sign_text =
    { DEFAULT_SIGN_TEXT[1], DEFAULT_SIGN_TEXT[2], DEFAULT_SIGN_TEXT[3], DEFAULT_SIGN_TEXT[4] }
  if type(signs) == "table" and type(signs.text) == "table" then
    for sev = 1, 4 do
      if signs.text[sev] ~= nil then sign_text[sev] = tostring(signs.text[sev]) end
    end
  end
  vim._diagnostic_config(
    vim.diagnostic._config.underline ~= false,
    vt ~= false and vt ~= nil,
    prefix,
    signs ~= false and signs ~= nil,
    sign_text
  )
end

-- vim.diagnostic.set(namespace, bufnr, diagnostics[, opts]): replace the
-- diagnostics for (namespace, bufnr). `bufnr` 0 means the current buffer. The
-- entries are stored as given (each typically { lnum, col, message, severity });
-- `opts` (display overrides) is accepted but not yet honored — see the INCOMPLETE
-- note on vim._diagnostics_ns. A plugin (lazy.nvim's update view) drives its own
-- buffer's diagnostics through this.
function vim.diagnostic.set(namespace, bufnr, diagnostics, _opts)
  if bufnr == 0 then bufnr = diag_current_bufnr() end
  local byns = vim._diagnostics_ns[bufnr]
  if not byns then
    byns = {}
    vim._diagnostics_ns[bufnr] = byns
  end
  byns[namespace] = diagnostics or {}
end

-- vim.diagnostic.reset([namespace, [bufnr]]): clear client-set diagnostics. With
-- no args, every namespace in every buffer; with a namespace, that namespace in
-- every buffer (or just `bufnr` when given). lazy.nvim calls this when its update
-- float closes — it used to crash because the function was missing.
function vim.diagnostic.reset(namespace, bufnr)
  if namespace == nil then
    vim._diagnostics_ns = {}
    return
  end
  if bufnr ~= nil then
    if bufnr == 0 then bufnr = diag_current_bufnr() end
    local byns = vim._diagnostics_ns[bufnr]
    if byns then byns[namespace] = nil end
    return
  end
  for _, byns in pairs(vim._diagnostics_ns) do
    byns[namespace] = nil
  end
end
