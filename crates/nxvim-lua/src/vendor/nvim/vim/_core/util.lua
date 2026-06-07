-- Minimal excerpt of neovim runtime/lua/vim/_core/util.lua (commit 70cfeab):
-- only `nvim_on`, the one symbol the vendored `vim.treesitter.query` requires (to
-- clear its query cache on a `runtimepath` `OptionSet`). The full upstream module
-- also defines editor helpers (space_above/below, edit_in, …) that reach for
-- `vim.cmd`, `nvim_win_call`, `vim.fn.resolve`, … which nxvim doesn't expose;
-- carrying them would be dead code that errors on use, so this is a faithful
-- subset rather than a verbatim copy.
-- Copyright Neovim contributors. Licensed under the Apache License, Version 2.0;
-- see crates/nxvim-lua/src/vendor/nvim/LICENSE.

local M = {}

--- Register an autocommand. A thin wrapper over |nvim_create_autocmd()| (verbatim
--- from upstream `_core/util.lua`):
---   nvim_on('BufWritePost', group, function(ev) print(ev.file) end)
---   nvim_on({ 'BufRead', 'BufNew' }, group, { pattern = '*.lua' }, function(ev) end)
function M.nvim_on(events, group, opts_or_fn, fn)
  vim.validate('opts_or_fn', opts_or_fn, { 'function', 'table' })
  local opts
  if type(opts_or_fn) == 'function' then
    fn, opts = opts_or_fn, {}
  else
    opts = opts_or_fn
  end
  opts.group = group
  opts.callback = fn
  return vim.api.nvim_create_autocmd(events, opts)
end

return M
