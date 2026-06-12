-- nxvim Lua prelude — the `nx.*` namespace, nxvim's own config/plugin API.
--
-- This chunk loads LAST (see PRELUDE_MODULES in runtime.rs), after every `vim.*`
-- surface it builds on is defined. Per ADR 0002 the break is: `nx.*` is the
-- canonical editor API, and the bounded `vim.*` whitelist is *aliases onto it* —
-- the same objects, the same semantics, two names. Because the machinery is
-- already implemented under `vim.*`, this module makes `nx.<x>` the canonical
-- handle for that implementation; `vim.<x>` and `nx.<x>` are the same object, so
-- a write through either is seen through both. As surfaces grow their own native
-- shape, the implementation moves here and `vim.*` keeps forwarding.
--
-- Scope of this slice: the config surface a typical `init.lua` targets —
-- variables, options, dispatch, keymaps, event/command registration, and the
-- callback-shaped async already designed that way. The UI-orchestration
-- registries (`nx.complete` / `picker` / `statusline` / …) are separate slices.
-- `nx.treesitter` and `nx.bo`'s treesitter-relevant wiring land in later phases.

nx = nx or {}

-- Variables — global / buffer / window scoped (`nx.g.mapleader = " "`).
nx.g = vim.g
nx.b = vim.b
nx.w = vim.w

-- Options. `nx.o` is the scalar get/set; `nx.opt` the rich wrapper (`:append`,
-- list/map options); `nx.go` the editor-global scope; `nx.bo` / `nx.wo` the
-- buffer- / window-local scopes. `opt_local` / `opt_global` mirror neovim.
nx.o = vim.o
nx.opt = vim.opt
nx.opt_local = vim.opt_local
nx.opt_global = vim.opt_global
nx.go = vim.go
nx.bo = vim.bo
nx.wo = vim.wo

-- Dispatch — queue an ex-command (`nx.cmd("colorscheme catppuccin")`, or the
-- subcommand form `nx.cmd.colorscheme("catppuccin")`).
nx.cmd = vim.cmd

-- Keymaps — `nx.keymap.set(mode, lhs, rhs, opts)` / `nx.keymap.del(...)`.
nx.keymap = vim.keymap

-- Events — structured autocmd subscriptions. `nx.on(event, opts, fn)`: the
-- canonical verb. `fn` (when given) is the handler; otherwise `opts.callback` /
-- `opts.command` apply, exactly as the underlying registry expects. Returns the
-- subscription id (droppable with `nx.off`).
function nx.on(event, opts, fn)
  opts = opts or {}
  if fn ~= nil then
    -- Don't mutate the caller's table; layer the handler on a shallow copy.
    local merged = {}
    for k, v in pairs(opts) do
      merged[k] = v
    end
    merged.callback = fn
    opts = merged
  end
  return vim.api.nvim_create_autocmd(event, opts)
end

-- Drop a subscription created by `nx.on`.
function nx.off(id) return vim.api.nvim_del_autocmd(id) end

-- User commands — `nx.command(name, fn, opts)` defines `:Name`; `fn` is a
-- function or an ex-command string.
function nx.command(name, fn, opts) return vim.api.nvim_create_user_command(name, fn, opts) end

-- Callback-shaped async already designed that way upstream.
nx.notify = vim.notify
nx.schedule = vim.schedule

-- Treesitter highlight control, as declarative buffer state (the two-noun model).
-- `start`/`stop` are verbs over the nouns: which language (`nx.bo.filetype`) and
-- whether treesitter paints (`nx.bo.ts_highlight`). `start(buf, lang)` forces a
-- language and enables; `start(buf)` (no lang) just enables, keeping the buffer's
-- filetype; `stop(buf)` disables highlighting without clearing the filetype, so
-- LSP/indent still see the language.
nx.treesitter = nx.treesitter or {}

function nx.treesitter.start(buf, lang)
  buf = buf or 0
  if lang ~= nil and lang ~= "" then nx.bo[buf].filetype = lang end
  nx.bo[buf].ts_highlight = true
end

function nx.treesitter.stop(buf) nx.bo[buf or 0].ts_highlight = false end

return nx
