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

-- vim.treesitter.highlighter: nxvim does NOT implement decoration-provider
-- highlighting (vim.treesitter.start / the highlighter) — that's an explicit
-- non-goal of the platform; the Rust engine owns redraw highlighting. But the
-- vendored highlighter.lua can't even load here (it registers a decoration
-- provider at module scope, an API nxvim lacks), and plugins such as catppuccin
-- *probe* `vim.treesitter.highlighter.hl_map` (a field neovim removed) to detect
-- the pre-0.8 API. So replace the lazy require with a small honest table: probed
-- fields (hl_map) read nil so the legacy path is skipped and the plugin's
-- `@`-capture highlight groups still load; `active` is an empty map so
-- stop()/get_captures_at_pos resolve to "no active highlighter"; and the real
-- entry point (new) fails loud rather than faking that highlighting works.
vim.treesitter.highlighter = {
  active = {},
  new = function()
    vim._notimpl('vim.treesitter.highlighter.new (decoration-provider highlighting)')
  end,
}

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
