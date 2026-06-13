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
-- nxvim-native shape: event/command registration and the callback-shaped async.

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
  return nx.autocmd.create(event, opts)
end

-- Drop a subscription created by `nx.on`.
function nx.off(id)
  return nx.autocmd.del(id)
end

-- User commands — `nx.command(name, fn, opts)` defines `:Name`; `fn` is a
-- function or an ex-command string.
function nx.command(name, fn, opts)
  return nx.user_command.create(name, fn, opts)
end

-- (`nx.notify` / `nx.schedule` — the callback-shaped async — are authored as
-- `nx.*` in prelude/runtime.lua, with `vim.*` aliased onto them there.)
--
-- Treesitter highlighting is controlled declaratively through buffer options
-- (nx.bo.filetype + nx.bo.ts_highlight), part of the options surface in
-- prelude/state.lua — there is no separate nx.treesitter verb API.

return nx
