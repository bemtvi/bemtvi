-- bemtvi Lua prelude — `btv.diagnostic` [alias `vim.diagnostic`].
-- The Lua diagnostics surface (`btv.diagnostic.get` / goto / setloclist / config) over the mirror the server pushes.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `btv.*` layered on the Rust bridge.

local vim = vim

-- ----- btv.diagnostic: the Lua diagnostics surface ---------------------------
-- `get` reads the Rust→Lua mirror (`btv._diagnostics`, keyed by bufnr, refreshed
-- on every `publishDiagnostics` via `btv._set_diagnostics`); the actions
-- (`goto_next`/`goto_prev`/`setloclist`) and `config` enqueue an `LspOp` the
-- server applies, reusing the native cursor-move / panel / underline paths.
btv.diagnostic = btv.diagnostic or {}

-- Severity is numbered 1=ERROR…4=HINT (neovim), and the table reverse-maps the
-- number back to its name (`btv.diagnostic.severity[1] == "ERROR"`).
btv.diagnostic.severity = {
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
btv._diagnostics = btv._diagnostics or {}
function btv._set_diagnostics(bufnr, list)
  btv._diagnostics[bufnr or 0] = list or {}
end

-- Client-set diagnostics (`vim.diagnostic.set`), keyed by bufnr → namespace → list.
-- Kept apart from the LSP-pushed mirror above so a plugin that manages its own
-- diagnostics (a plugin's view, in its own scratch buffer + namespace)
-- never collides with a file buffer's LSP diagnostics. `get` merges both.
-- These DO render: every `set`/`reset` flattens the buffer's namespaces and
-- pushes them across to the server's render store (`btv._set_client_diagnostics`),
-- so the underline / virtual-text / sign surfaces paint them next to the LSP set.
btv._diagnostics_ns = btv._diagnostics_ns or {}

-- Flatten `bufnr`'s client-set diagnostics across every namespace into one list
-- and push it to the server's render store (replace semantics — an empty list
-- clears the buffer). Each entry is normalized to the {lnum,col,end_lnum,end_col,
-- severity,message,source} shape the Rust bridge reads; `col`/`end_col` are native
-- byte columns. Called after any `set`/`reset` so the painted set tracks the table.
local function diag_sync_client(bufnr)
  local flat = {}
  local byns = btv._diagnostics_ns[bufnr]
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
          severity = d.severity or btv.diagnostic.severity.ERROR,
          message = d.message or "",
          source = d.source,
        }
      end
    end
  end
  btv._set_client_diagnostics(bufnr, flat)
end

local function diag_current_bufnr()
  return btv._cur_buf and btv._cur_buf.bufnr or 0
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

-- `btv.diagnostic.get([bufnr, [opts]])`: diagnostics as plain tables. `nil` bufnr →
-- every buffer; `0` → the current one. `opts.severity` (a number) filters. The
-- entries are copied out (callers must not mutate the mirror), each tagged with
-- its `bufnr`, matching neovim's shape.
function btv.diagnostic.get(bufnr, opts)
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
      for _, d in ipairs(btv._diagnostics[b] or {}) do
        take(b, d)
      end
    end
    local byns = btv._diagnostics_ns[b]
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
    for b in pairs(btv._diagnostics) do
      seen[b] = true
      collect(b)
    end
    for b in pairs(btv._diagnostics_ns) do
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

function btv.diagnostic.goto_next(opts)
  opts = opts or {}
  btv._diagnostic_goto(true, opts.severity)
end

function btv.diagnostic.goto_prev(opts)
  opts = opts or {}
  btv._diagnostic_goto(false, opts.severity)
end

-- Severity (1=ERROR…4=HINT) → quickfix type char, matching neovim's
-- `vim.diagnostic.toqflist`.
local SEVERITY_TYPE = { "E", "W", "I", "N" }

-- `btv.diagnostic.toqflist(diagnostics)`: convert a list of diagnostic tables (the
-- shape `btv.diagnostic.get` returns) into quickfix/location-list items. Mirrors
-- neovim's `vim.diagnostic.toqflist`: 0-based diagnostic positions become 1-based
-- list columns, the message becomes the entry text, and severity maps to the type
-- char. The buffer name is resolved into `filename` so the entry is jumpable.
function btv.diagnostic.toqflist(diagnostics)
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

-- `btv.diagnostic.setqflist([opts])`: replace the quickfix list with every buffer's
-- diagnostics and (unless `opts.open == false`) open the quickfix window. `opts`:
-- `severity` filter, `title`, `open`.
function btv.diagnostic.setqflist(opts)
  opts = opts or {}
  local diags = btv.diagnostic.get(nil, { severity = opts.severity })
  local items = btv.diagnostic.toqflist(diags)
  btv.setqflist({}, " ", { title = opts.title or "Diagnostics", items = items })
  if opts.open ~= false then
    vim.cmd("copen")
  end
end

-- `btv.diagnostic.setloclist([opts])`: populate the current window's location list
-- with the current buffer's diagnostics (a real, navigable loclist now that one
-- exists) and open it. `opts`: `bufnr` (default current), `severity`, `title`,
-- `open`.
function btv.diagnostic.setloclist(opts)
  opts = opts or {}
  -- Capture the owner window explicitly so the list lands on this window even
  -- though the `:lopen` below moves focus into the (new) loclist window.
  local win = btv.win.current()
  local diags = btv.diagnostic.get(opts.bufnr or 0, { severity = opts.severity })
  local items = btv.diagnostic.toqflist(diags)
  btv.setloclist(win, {}, " ", { title = opts.title or "Diagnostics", items = items })
  if opts.open ~= false then
    vim.cmd("lopen")
  end
end

-- `btv.diagnostic.open_float([opts])`: open a float listing the cursor line's
-- diagnostics in full (the multi-line messages with source/code that the inline
-- virtual text truncates). The server reads the cursor at apply time and routes
-- through the float surface (the bottom panel hover uses); a clean line opens
-- nothing.
--
-- What bemtvi shows is exactly neovim's DEFAULT `scope = "line"`, so that value is
-- accepted. Everything else neovim models — `scope` of `"cursor"`/`"buffer"`, the
-- `severity` filter, `bufnr`/`pos`, and the presentation options (`border`,
-- `header`, `source`, `format`) — is **rejected loudly** rather than dropped: a
-- silently-ignored `scope = "buffer"` would show one line's diagnostics while the
-- caller believed it asked for the whole buffer's.
function btv.diagnostic.open_float(opts)
  if opts ~= nil then
    if type(opts) ~= "table" then
      error("btv.diagnostic.open_float: opts must be a table, got " .. type(opts), 2)
    end
    for k, v in pairs(opts) do
      if k ~= "scope" then
        error("btv.diagnostic.open_float: unsupported option '" .. tostring(k) .. "'", 2)
      end
      if v ~= "line" then
        error(
          "btv.diagnostic.open_float: unsupported scope '"
            .. tostring(v)
            .. "' (bemtvi shows the cursor line, neovim's default scope='line')",
          2
        )
      end
    end
  end
  btv._diagnostic_open_float()
end

-- ----- the built-in diagnostic-navigation keymaps (neovim's core defaults) ----
-- `]d`/`[d` jump to the next/previous diagnostic; `]e`/`[e` to the next/previous
-- *error* (severity ERROR only). `<C-w>d` (show the cursor's diagnostics in a
-- float) is the third upstream default, but it rides the native `<C-w>` window
-- grammar in core, not the keymap engine, so it is wired there — not here.
-- Registered with `default = true` so a user/plugin map on any of these wins, and
-- an empty-function map disables it; mirrors the cmdline defaults in keymap.lua.
--
-- NOTE: upstream neovim ships `]d`/`[d` and `<C-w>d` as core defaults but NOT
-- `]e`/`[e` — those are a common (telescope/trouble-era) convention for
-- error-only navigation, added here alongside the real defaults.
for _, m in ipairs({
  {
    "]d",
    function()
      btv.diagnostic.goto_next()
    end,
    "Next diagnostic",
  },
  {
    "[d",
    function()
      btv.diagnostic.goto_prev()
    end,
    "Previous diagnostic",
  },
  {
    "]e",
    function()
      btv.diagnostic.goto_next({ severity = btv.diagnostic.severity.ERROR })
    end,
    "Next error",
  },
  {
    "[e",
    function()
      btv.diagnostic.goto_prev({ severity = btv.diagnostic.severity.ERROR })
    end,
    "Previous error",
  },
}) do
  btv.keymap.set("n", m[1], m[2], { default = true, desc = m[3] })
end

-- The built-in gutter sign glyphs, indexed 1=ERROR…4=HINT (neovim's `E`/`W`/`I`/
-- `H` letters), overridden per-severity by the `signs.text` map.
local DEFAULT_SIGN_TEXT = { "E", "W", "I", "H" }

-- The default quiet gap, in ms, before a diagnostic update that landed while you
-- were typing is applied (`update_in_insert`'s number form). Mirrors
-- `DEFAULT_INSERT_DEBOUNCE_MS` server-side, which is the value that actually
-- applies until a config call overrides it.
local DEFAULT_INSERT_DEBOUNCE_MS = 3000

-- The stored (merged) config `btv.diagnostic.config` reads and writes. Above the
-- docstring, not between it and the function: the book generator takes the `--`
-- block *immediately* above a definition, so anything wedged in between silently
-- drops the whole doc from the rendered page.
btv.diagnostic._config = {
  underline = true,
  virtual_text = false,
  signs = true,
  update_in_insert = DEFAULT_INSERT_DEBOUNCE_MS,
}

-- `btv.diagnostic.config([opts])`: merge `opts` into the stored config and return
-- the merged table when called bare. bemtvi renders three surfaces — the underline
-- spans (`underline`), the inline end-of-line message (`virtual_text`), and the
-- gutter sign column (`signs`) — so those keys drive rendering; `update_in_insert`
-- gates *when* an update reaches all three. The rest are stored without behavior
-- until a surface exists.
--
-- `update_in_insert` decides what happens to a diagnostic update that arrives while
-- you are typing. A language server re-diagnoses after every `didChange` — i.e.
-- after every keystroke — so applied as they land, the squiggles, signs and inline
-- messages churn under the cursor over errors that exist only because the line
-- isn't finished. bemtvi takes a **number of milliseconds** here as well as neovim's
-- two booleans:
--
-- ```lua
-- -- the default: apply the newest update once typing has been quiet for 3s
-- btv.diagnostic.config({ update_in_insert = 3000 })
--
-- btv.diagnostic.config({ update_in_insert = true })   -- apply every update at once
-- btv.diagnostic.config({ update_in_insert = false })  -- hold everything until InsertLeave
-- ```
--
-- Nothing is ever dropped: while an update is held the *newest* one is kept, and
-- leaving insert mode applies it immediately whatever the interval is. Nor does
-- anything drift out of place while you wait — a displayed diagnostic is anchored to
-- the text it flags, so its squiggle, sign, inline message and `]d` target follow
-- that text as you edit around it.
--
-- INCOMPLETE: `virtual_lines`, `severity_sort`, … are recorded and echoed back
-- but have no rendering surface yet (`float` is honored by
-- `btv.diagnostic.open_float`, but the `config.float` defaults that pre-style it
-- are not). `_namespace` is ignored (one global config). Faithful once those
-- diagnostic surfaces exist.
function btv.diagnostic.config(opts, _namespace)
  if opts == nil then
    return btv.diagnostic._config
  end
  for k, v in pairs(opts) do
    btv.diagnostic._config[k] = v
  end
  -- `underline`/`virtual_text`/`signs` are true/false/table (a table is an
  -- enabled, filtered form); only an explicit `false`/`nil` disables. The
  -- virt-text `prefix` rides the `virtual_text` table form (default `■ `); the
  -- per-severity sign glyphs ride the `signs.text` map (default `E`/`W`/`I`/`H`).
  local vt = btv.diagnostic._config.virtual_text
  local prefix = "■ "
  if type(vt) == "table" and vt.prefix ~= nil then
    prefix = tostring(vt.prefix)
  end
  local signs = btv.diagnostic._config.signs
  local sign_text =
    { DEFAULT_SIGN_TEXT[1], DEFAULT_SIGN_TEXT[2], DEFAULT_SIGN_TEXT[3], DEFAULT_SIGN_TEXT[4] }
  if type(signs) == "table" and type(signs.text) == "table" then
    for sev = 1, 4 do
      if signs.text[sev] ~= nil then
        sign_text[sev] = tostring(signs.text[sev])
      end
    end
  end
  -- `update_in_insert` is false / true / a number of ms; flatten it into the pair
  -- the server takes — "may an update apply before `InsertLeave`?" and "after how
  -- long a quiet gap?". A number <= 0 is the same as `true` (no wait).
  local uii = btv.diagnostic._config.update_in_insert
  local timed = type(uii) == "number"
  btv._diagnostic_config(
    btv.diagnostic._config.underline ~= false,
    vt ~= false and vt ~= nil,
    prefix,
    signs ~= false and signs ~= nil,
    sign_text,
    timed or uii == true,
    timed and math.max(0, math.floor(uii)) or 0
  )
end

-- `btv.diagnostic.set(namespace, bufnr, diagnostics[, opts])`: replace the
-- diagnostics for (namespace, bufnr). `bufnr` 0 means the current buffer. The
-- entries are stored as given (each typically { lnum, col, message, severity });
-- `opts` (display overrides) is accepted but not yet honored — see the INCOMPLETE
-- note on `btv._diagnostics_ns`. A plugin drives its own
-- buffer's diagnostics through this.
function btv.diagnostic.set(namespace, bufnr, diagnostics, _opts)
  if bufnr == 0 then
    bufnr = diag_current_bufnr()
  end
  local byns = btv._diagnostics_ns[bufnr]
  if not byns then
    byns = {}
    btv._diagnostics_ns[bufnr] = byns
  end
  byns[namespace] = diagnostics or {}
  diag_sync_client(bufnr)
end

-- `btv.diagnostic.reset([namespace, [bufnr]])`: clear client-set diagnostics. With
-- no args, every namespace in every buffer; with a namespace, that namespace in
-- every buffer (or just `bufnr` when given). A plugin calls this when its
-- float closes — it used to crash because the function was missing.
function btv.diagnostic.reset(namespace, bufnr)
  if namespace == nil then
    -- Collect the affected buffers before wiping so each can be re-synced empty.
    local bufs = {}
    for b in pairs(btv._diagnostics_ns) do
      bufs[#bufs + 1] = b
    end
    btv._diagnostics_ns = {}
    for _, b in ipairs(bufs) do
      diag_sync_client(b)
    end
    return
  end
  if bufnr ~= nil then
    if bufnr == 0 then
      bufnr = diag_current_bufnr()
    end
    local byns = btv._diagnostics_ns[bufnr]
    if byns then
      byns[namespace] = nil
    end
    diag_sync_client(bufnr)
    return
  end
  for b, byns in pairs(btv._diagnostics_ns) do
    byns[namespace] = nil
    diag_sync_client(b)
  end
end

-- Muscle-memory alias: `vim.diagnostic` is the same table as the canonical
-- `btv.diagnostic` defined above.
vim.diagnostic = btv.diagnostic
