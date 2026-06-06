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
  ERROR = 1, WARN = 2, INFO = 3, HINT = 4,
  [1] = "ERROR", [2] = "WARN", [3] = "INFO", [4] = "HINT",
}

-- The mirror the server pushes into; keyed by bufnr → list of diagnostic tables.
vim._diagnostics = vim._diagnostics or {}
function vim._set_diagnostics(bufnr, list)
  vim._diagnostics[bufnr or 0] = list or {}
end

local function diag_current_bufnr()
  return vim._cur_buf and vim._cur_buf.bufnr or 0
end

-- vim.diagnostic.get([bufnr, [opts]]): diagnostics as plain tables. `nil` bufnr →
-- every buffer; `0` → the current one. `opts.severity` (a number) filters. The
-- entries are copied out (callers must not mutate the mirror), each tagged with
-- its `bufnr`, matching neovim's shape.
function vim.diagnostic.get(bufnr, opts)
  opts = opts or {}
  local out = {}
  local function collect(b)
    for _, d in ipairs(vim._diagnostics[b] or {}) do
      if opts.severity == nil or d.severity == opts.severity then
        local copy = { bufnr = b }
        for k, v in pairs(d) do copy[k] = v end
        out[#out + 1] = copy
      end
    end
  end
  if bufnr == nil then
    for b in pairs(vim._diagnostics) do collect(b) end
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

function vim.diagnostic.setloclist(_opts)
  vim._diagnostic_setloclist()
end

-- vim.diagnostic.config([opts]): merge `opts` into the stored config and return
-- the merged table when called bare. nxvim has one diagnostic surface — the
-- underline spans — so the `underline` key is honored (false hides the
-- squiggles); virt-text/signs and the rest are stored without behavior until a
-- surface exists.
-- INCOMPLETE: only `underline` drives anything. `virtual_text`, `signs`,
-- `virtual_lines`, `float`, `severity_sort`, … are recorded and echoed back but
-- have no rendering surface, so a config enabling inline virtual-text diagnostics
-- sees no change. `_namespace` is ignored (one global config). Faithful once
-- those diagnostic surfaces exist.
vim.diagnostic._config = { underline = true }
function vim.diagnostic.config(opts, _namespace)
  if opts == nil then return vim.diagnostic._config end
  for k, v in pairs(opts) do vim.diagnostic._config[k] = v end
  -- `underline` is true/false/table (a table is an enabled, filtered form);
  -- only an explicit `false` disables.
  vim._diagnostic_config(vim.diagnostic._config.underline ~= false)
end
