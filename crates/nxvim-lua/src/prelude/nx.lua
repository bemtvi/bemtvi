-- nxvim Lua prelude — the `nx.*` namespace, nxvim's own config/plugin API.
--
-- This chunk loads LAST (see PRELUDE_MODULES in runtime.rs). Per ADR 0002 the
-- break is: `nx.*` is the canonical editor API, and the bounded `vim.*` whitelist
-- is *aliases onto it* — the same objects, the same semantics, two names. The
-- variable / option / dispatch / keymap surfaces are now *authored as `nx.*`* in
-- their home prelude chunks (stdlib / timer / nvim_api / keymap, plus `nx.cmd`
-- seeded by the Rust bridge), each setting the matching `vim.*` name to the same
-- object right after. So those nouns are already on `nx` by the time this chunk
-- runs — it does not re-bind them. What lives here is the rest of the config
-- surface a typical `init.lua` targets that has no `vim.*` twin or needs an
-- nxvim-native shape: event/command registration, the callback-shaped async, and
-- treesitter control.
--
-- The UI-orchestration registries (`nx.complete` / `picker` / `statusline` / …)
-- are separate slices.

nx = nx or {}

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

-- (`nx.notify` / `nx.schedule` — the callback-shaped async — are authored as
-- `nx.*` in prelude/runtime.lua, with `vim.*` aliased onto them there.)

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

-- Install (or, with `text = nil`, drop) a treesitter query override for
-- `(lang, name)` — e.g. a custom `highlights` or `injections` query. Replaces the
-- engine's on-disk query directly; there is no `;extends`/runtimepath merge (that
-- neovim-compat resolution does not exist in nxvim).
function nx.treesitter.set_query(lang, name, text) vim._nx_set_ts_query(lang, name, text) end

-- The bounded `vim.treesitter` alias (the muscle-memory whitelist, ADR 0002):
-- only the `start`/`stop` toggle, mapped 1:1 onto the `nx` verbs. Every other
-- `vim.treesitter.*` field is absent and fails loud on access (no parser API, no
-- `query.*` — those are deliberately not part of nxvim's surface).
vim.treesitter = { start = nx.treesitter.start, stop = nx.treesitter.stop }

return nx
