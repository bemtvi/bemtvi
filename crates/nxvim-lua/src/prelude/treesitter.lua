-- nxvim Lua prelude — wires the vendored vim.treesitter onto nxvim's primitives.
--
-- The high-level `vim.treesitter.*` API is neovim's own Lua, vendored under
-- `src/vendor/nvim/` and registered in `package.preload` by runtime.rs. This
-- module supplies the few globals that API expects but nxvim's prelude doesn't
-- already define, then `require`s it and adapts the two seams where nxvim's
-- snapshot bridge diverges from neovim's live buffer access. The bespoke
-- low-level primitives it stands on (`vim._create_ts_parser`,
-- `vim._create_ts_querycursor`, …) are installed in Rust by nxvim-ts.

-- vim._defer_require(root, mod): a module table whose submodules are required
-- lazily on first access. Verbatim from neovim's vim/_core/shared.lua;
-- vim/treesitter.lua builds its module table with it.
function vim._defer_require(root, mod)
  return setmetatable({ _submodules = mod }, {
    __index = function(t, k)
      if not mod[k] then
        return
      end
      local name = string.format('%s.%s', root, k)
      t[k] = require(name)
      return t[k]
    end,
  })
end

-- vim.F: upstream helpers (pack_len/unpack_len) the memoizer uses to cache
-- multi-return values.
vim.F = require('vim.F')

-- vim.func: upstream module providing vim.func._memoize, through which
-- query.lua memoizes query.get / query.parse. Required (not assigned a stub) so
-- the real memoizer — with its :clear() cache control — is used.
vim.func = require('vim.func')

-- The high-level API itself, now that the globals it reads at load time exist.
vim.treesitter = require('vim.treesitter')

-- vim.treesitter.highlighter: nxvim does NOT run neovim's decoration-provider
-- highlighter on the redraw hot path — the Rust engine owns redraw highlighting.
-- The vendored highlighter.lua can't even load here (it registers a decoration
-- provider at module scope, an API nxvim lacks), and plugins such as catppuccin
-- *probe* `vim.treesitter.highlighter.hl_map` (a field neovim removed) to detect
-- the pre-0.8 API. So replace the lazy require with a small honest table: probed
-- fields (hl_map) read nil so the legacy path is skipped and the plugin's
-- `@`-capture highlight groups still load; `active[buf]` is populated by the
-- start/stop bridge below (so code that checks "is TS highlighting on for this
-- buffer?" sees the truth); and the real decoration-provider entry point (new)
-- fails loud rather than faking the upstream highlighter object.
vim.treesitter.highlighter = {
  active = {},
  new = function()
    vim._notimpl('vim.treesitter.highlighter.new (decoration-provider highlighting)')
  end,
}

-- vim.treesitter.start / stop — the bridge to nxvim's native engine (ADR 0001,
-- #1). Rather than running neovim's Lua highlighter (a decoration provider on the
-- redraw hot path nxvim lacks), `start` enables the in-core Rust engine for the
-- buffer at the resolved language, and `stop` disables it — via the `_ts_start` /
-- `_ts_stop` effects the server forwards to `Editor::ts_start` / `ts_stop`. This
-- subsumes the common case the extension table misses: a buffer with no known
-- extension (or a forced lang) gets highlighting once a config/plugin calls
-- `start`. Resolution stays faithful (neovim's filetype→lang mapping); only
-- *execution* moves to the native engine. Unlike upstream, this does NOT create a
-- Lua-side LanguageTree — a highlight-only buffer parses once (in the engine);
-- the double parse begins only if a plugin separately calls `get_parser`.
function vim.treesitter.start(buf, lang)
  buf = vim._resolve_bufnr(buf)
  lang = lang or vim.treesitter.language.get_lang(vim.bo[buf].filetype) or vim.bo[buf].filetype
  if not lang or lang == '' then
    error(
      ('vim.treesitter.start: could not determine language for buffer %d '
      .. '(set filetype or pass an explicit lang)'):format(buf)
    )
  end
  vim.treesitter.highlighter.active[buf] = { bufnr = buf, lang = lang }
  vim._ts_start(buf, lang)
end

function vim.treesitter.stop(buf)
  buf = vim._resolve_bufnr(buf)
  vim.treesitter.highlighter.active[buf] = nil
  vim._ts_stop(buf)
end

do
  local LanguageTree = require('vim.treesitter.languagetree')

  -- Snapshot seam #1 — re-read the buffer on every parse.
  --
  -- neovim attaches to the buffer (nvim_buf_attach) so byte edits invalidate the
  -- tree incrementally. nxvim's Lua bridge is a snapshot + effect queue, not a
  -- live handle: there is nothing to attach to. Instead, a buffer-sourced parser
  -- invalidates itself before each :parse(), so it re-parses the *current*
  -- snapshot (vim._bufs[bufnr], refreshed by the server before any Lua runs).
  -- This is a full reparse per parse — the "two parsers" cost the design accepts
  -- for v1; incremental reuse from buffer deltas is a later optimization. String
  -- parsers are immutable, so they keep their cached / incremental trees.
  local orig_parse = LanguageTree.parse
  function LanguageTree:parse(range, on_parse)
    if type(self._source) == 'number' then
      self:invalidate(true)
    end
    return orig_parse(self, range, on_parse)
  end

  -- Snapshot seam #2 — create a parser without nvim_buf_attach.
  --
  -- Upstream's _create_parser registers on_bytes/on_detach/on_reload via
  -- nvim_buf_attach. nxvim drives invalidation through the snapshot re-read above
  -- instead, so there is no live buffer to attach to. Overriding this one
  -- function is the honest seam: nxvim does not ship a no-op nvim_buf_attach that
  -- registers callbacks which would never fire.
  function vim.treesitter._create_parser(buf, lang, opts)
    return LanguageTree.new(vim._resolve_bufnr(buf), lang, opts)
  end
end

-- ~~~ Query-resolution bridge (ADR 0001, #4): Lua resolves, the engine executes.
--
-- The native engine compiles ONE `highlights.scm` / `indents.scm` per grammar; it
-- doesn't run neovim's query-merge logic, so a customized query (in-memory
-- `query.set`, an `after/queries` overlay, a `;extends` base merge) is otherwise
-- inert against the paint. The fix keeps resolution in the faithful vendored Lua
-- and only moves *execution* to the engine: when a paint-driving query changes,
-- the server pulls the merged string from here and pushes it to the engine.
do
  local tsquery = require('vim.treesitter.query')

  -- Track which (lang, name) currently have a non-empty explicit `query.set`, so
  -- the resolve seam below can fail loud if it ever stops capturing (e.g. a
  -- re-vendor changes how `get` reaches `parse`) rather than silently dropping the
  -- override.
  local was_set = {}
  local function key(lang, name)
    return lang .. '\0' .. name
  end

  -- Resolve the *merged* query string the engine should compile — exactly what
  -- `vim.treesitter.query.get(lang, name)` builds right before it parses. `get`
  -- returns a compiled Query (it keeps no source text) and is memoized, so we
  -- transiently wrap `tsquery.parse` (which `get` calls by table lookup) to
  -- capture the string it is handed, force a fresh resolve, then restore. This
  -- reuses ALL of upstream's merge logic (explicit + `;extends` + `after`) with no
  -- query-merge code duplicated in nxvim and no edit to the vendored file — the
  -- same "wrap from the prelude" discipline as the snapshot seams above.
  function vim.treesitter._resolved_query_string(lang, name)
    local captured = nil
    local orig_parse = tsquery.parse
    tsquery.parse = function(_, s)
      captured = s
      return true
    end
    local ok, err = pcall(function()
      tsquery.get:clear(lang, name) -- bypass the memo: force a real resolve
      tsquery.get(lang, name)
    end)
    tsquery.parse = orig_parse
    tsquery.get:clear(lang, name) -- drop our throwaway result from the memo
    if not ok then
      error(('treesitter: resolving query %s/%s failed: %s'):format(lang, name, err))
    end
    if captured == nil and was_set[key(lang, name)] then
      error(
        ('treesitter query bridge: the resolve seam captured nothing for the '
          .. 'set query %s/%s — the vendored query.get/parse contract changed'):format(lang, name)
      )
    end
    return captured
  end

  -- Wrap `query.set` to (1) store the override the same as upstream and (2) signal
  -- the server which query changed. Only the paint-relevant names drive the engine:
  -- `highlights` / `indents` paint directly, `injections` feeds the sub-language
  -- layers built on the root tree. Other names (folds, textobjects, …) update only
  -- the Lua side, exactly as before.
  local orig_set = tsquery.set
  function tsquery.set(lang, name, text)
    orig_set(lang, name, text)
    if text ~= nil and text ~= '' then
      was_set[key(lang, name)] = true
    else
      was_set[key(lang, name)] = nil
    end
    if name == 'highlights' or name == 'indents' or name == 'injections' then
      vim._ts_set_query(lang, name)
    end
  end
end
