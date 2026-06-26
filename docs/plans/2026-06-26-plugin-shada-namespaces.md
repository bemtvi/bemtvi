# Plugin shada namespaces

*2026-06-26*

## Goal

Let a plugin **opt in** to shada persistence and keep its own cross-session
key/value data. A plugin reads and writes only an **isolated namespace** inside
the *current* shada store — it can never touch the core editor state (registers,
marks, history, …) and is keyed apart from every other plugin's namespace.

This namespace is **not** the `--shada-namespace` workspace scope (which chooses
*which* store directory the whole session uses). It is a logical sub-namespace
*inside* whatever store is active — global, `--shada-namespace ns/<id>`, or
remote. A plugin's data therefore follows the same store the rest of shada
follows, with no extra wiring.

## Surface (`nx.shada.plugin`)

```lua
-- Opt in: an isolated, persistent key/value store for THIS plugin. No argument —
-- the namespace is ASSIGNED from where this code lives, not chosen.
local store = nx.shada.plugin()

store:set("recent", { "a.txt", "b.txt" }) -- value: any JSON-able Lua value
local recent = store:get("recent")        -- the table back, or nil
store:delete("recent")
local keys = store:keys()                  -- the stored keys (sorted)
store:clear()                              -- drop every key in this namespace
```

- Values are any JSON-encodable Lua value (table / string / number / bool /
  nil), serialized with the same `lua_to_json` rule `nx.json.encode` uses.
- **The namespace is assigned, not chosen.** It is derived from the calling
  chunk's source file → the runtimepath / plugin directory that contains it
  (`debug.getinfo` + `nx._runtime_paths()`; the dir's basename is the namespace,
  the user's config root maps to a reserved `user`). A plugin therefore can't name
  — and so can't read or clobber — another plugin's slice. Isolation from *core*
  shada is also structural (a separate redb table).
- A bare `:lua` / RPC `exec_lua` / test chunk has no source file to attribute (mlua
  names it after its Rust call site, a relative `@crates/…` we reject by requiring
  an absolute path). Those contexts pass an explicit `dev_namespace` as an escape
  hatch; a *sourced* file passing one is a loud error (the namespace is assigned).
- Persistence rides the existing shada cadence: the debounced live checkpoint and
  the clean-exit flush. With shada disabled (the test default, `--noshada`) the
  store still works **in memory** for the session; it just isn't written —
  exactly like registers/marks.

## Design

`PersistState` already carries non-core transport fields the edit-host
fills/consumes (`session`). Plugin data is the same: a pure pass-through field
that `nxvim-core` never reads or writes. The Lua runtime owns the live map; the
edit-host moves it in (load) and out (flush).

### Data model (`nxvim-core/persist.rs`)

```rust
pub plugin_data: Vec<PluginNamespace>,   // new PersistState field

pub struct PluginNamespace { pub namespace: String, pub entries: Vec<PluginEntry> }
pub struct PluginEntry      { pub key: String, pub value: String }   // value = JSON
```

`Editor::export_persist` sets it empty; `import_persist`/`apply_persist` ignore
it (not editor state). It is a carrier only.

### Store (`nxvim-server/shada.rs`)

A new `plugin` table keyed `(namespace, key)` → msgpack `StoredPlugin { value,
ts }`, mirroring `MARKS_FILE` exactly:
- recency merge per `(namespace, key)` (newest `ts` wins) in `collect_merge`;
- clear-then-rewrite this instance's rows in `write_state`, stamped `ts`;
- `build_state` groups the merged rows back into `Vec<PluginNamespace>`.

### Lua runtime (`nxvim-lua`)

`nxvim-lua` does **not** depend on `nxvim-core`, so the live map is a plain
`BTreeMap<String, BTreeMap<String, String>>` on `Shared`, and the accessors trade
plain tuples (`Vec<(String, Vec<(String, String)>)>`), which the edit-host
converts to/from the `PersistState` types.

- `nx._shada_plugin_set(ns, key, value)` — `lua_to_json` → store the string.
- `nx._shada_plugin_get(ns, key)` — stored string → `json_to_lua`, or nil.
- `nx._shada_plugin_delete(ns, key)`, `_keys(ns)`, `_clear(ns)`.
- `nx.shada.plugin(ns)` returns the method handle (built in `install.rs` beside
  the existing `nx.shada.namespace` / `save_layout`).

`LuaRuntime::plugin_shada_export()` / `plugin_shada_seed(data)` /
`plugin_shada_merge(data, replace)` bridge the map to the edit-host.

### Edit-host wiring (`nxvim-server/lib.rs`)

- `shada_load`: take `state.plugin_data` out (like `session`) before
  `import_persist`, then `self.lua.plugin_shada_seed(plugin_data)`.
- `shada_checkpoint` / `shada_flush_final` / `shada_write_now`: after
  `export_persist`, `snap.plugin_data = self.lua.plugin_shada_export()`.
- `shada_read_now` (`:rshada[!]`): after `apply_persist`,
  `self.lua.plugin_shada_merge(state.plugin_data, replace)`.

## Phases

**Phase 1 — working persistence (this commit).** Data model + store table + Lua
API (`set`/`get`) + host load/checkpoint/flush wiring. End-to-end test: a plugin
`set`s in session 1, the server restarts, the plugin `get`s it back in session 2.

**Phase 2 — full surface + polish (done).** `delete` / `keys` / `clear`,
`:rshada` merge, the namespace-isolation test, the location-based *assignment* of
namespaces (a refinement over a chosen string — see below), the
`examples/plugin-shada/` config (verified end-to-end), and doc updates
(`architecture.md`, the native-plugin-API spec).

## Assigned namespaces (refinement)

A self-chosen namespace string makes isolation only cooperative — any code could
claim any string. So `nx.shada.plugin()` takes **no argument** and the namespace is
*assigned* from where the calling code lives:

- `caller_source()` walks the Lua stack to the nearest `@<path>` chunk (every
  config / plugin / `require`d file is sourced with one; the prelude and C frames
  aren't).
- `assign_namespace(src)` attributes that path to the longest runtimepath entry
  (`nx._runtime_paths()`) that contains it, then resolves the namespace in order:
  the canonical **name the package manager registered** for that dir, when the plugin
  was loaded through `nx.plugins` (`nx.plugins._namespace_for(dir)` — tightest
  identity, since a `name = …` spec can differ from the dir basename); the reserved
  `user` for the config root (`stdpath("config")`); otherwise the dir's **basename**
  (a plugin loaded outside the manager, e.g. a `pack/*/start/*` dir).
- A context that attributes to **no** rtp entry — a bare `:lua`, an RPC `exec_lua`,
  a test (mlua names these after their relative Rust call site, e.g. `@crates/…`, so
  they match no rtp entry) — has no plugin identity, so it must pass an explicit
  `dev_namespace`. A *sourced* file passing one is a loud error.

Keying on *attribution success* (rather than "is there a source file" / "is the
path absolute") is what makes this robust to both the synthetic exec chunk names
and a relative `NXVIM_CONFIG`.
