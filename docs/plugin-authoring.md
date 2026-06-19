# Writing nxvim plugins

A plugin is **pure Lua over the `nx.*` API** — no Rust, no Vimscript. The server
owns every UI surface (windows, floats, the completion menu, the statusline); a
plugin supplies *data and behavior* and reaches the screen through `nx.*`.
Our prime directive is every feature that can be a plugin *is* one, so we excersise
our own APIs.

If you've written a neovim plugin, the shape is familiar — a `lua/<name>/init.lua`
module exposing `setup(opts)` — but the API you call is `nx.*`, not `vim.*`. (A
closed whitelist of `vim.*` muscle-memory aliases exists for config ergonomics; new
plugin code should target `nx.*` directly.)

## Anatomy of a plugin

A plugin is a directory laid out the way neovim plugins are, so it resolves on the
runtimepath:

```
my-plugin/
├── lua/
│   └── my-plugin/
│       └── init.lua        # the module: returns { setup = … }
├── plugin/                 # optional: *.lua here is auto-sourced at load
│   └── my-plugin.lua
├── after/plugin/           # optional: sourced after plugin/
├── colors/                 # optional: colors/<name>.lua for a colorscheme
└── test/                   # optional: *_spec.lua (see Testing)
    └── my-plugin_spec.lua
```

The conventional entry point is a module that exposes `setup`:

```lua
-- lua/my-plugin/init.lua
local M = {}

function M.setup(opts)
  opts = opts or {}
  nx.keymap.set("n", "<leader>x", function()
    nx.notify("hello from my-plugin", "info")
  end, { desc = "my-plugin: do the thing" })
end

return M
```

`setup` should be **idempotent** (a user may call it more than once) and side-effect
light at module load — do the wiring in `setup`, not at `require` time, so load order
and lazy-loading stay predictable.

## Installing & loading

Two paths, both runtimepath-based:

**The built-in manager (`nx.plugins`).** Declare plugins in `init.lua`; the manager
clones, runtimepaths, and loads them (with optional lazy triggers):

```lua
nx.plugins({
  -- eager:
  { "davidrios/nxvim-keys-helper",
    config = function() require("nxvim-keys-helper").setup({}) end },

  -- lazy by key / command / event / filetype:
  { "owner/repo", keys = { "<leader>ff" }, cmd = "Find",
    config = function() require("repo").setup({}) end },

  -- pinned, or a local checkout for development:
  { "owner/repo", tag = "v1.0.0" },
  { name = "my-plugin", dir = "/path/to/my-plugin",
    config = function() require("my-plugin").setup({}) end },
})
```

Then `:PluginSync` (clone missing + update), `:PluginInstall`, `:PluginUpdate`,
`:PluginClean`, `:PluginList`, or the `:Plugins` dashboard. Cloned plugins live under
`stdpath("data")/plugins/<name>`. A spec with `cmd`/`event`/`ft`/`keys` (or
`lazy = true`) loads on first use; `config` runs after the plugin is on the
runtimepath, `init` runs at startup regardless.

**By hand.** Drop the plugin under `<config>/pack/*/start/*` and `require` it from
`init.lua` — the runtimepath picks it up with no manager involved.

## The `nx.*` surfaces you'll use

A plugin composes these (each has runnable examples under [`examples/`](../examples)
and a deeper treatment in the [API design](specs/2026-06-11-native-plugin-api.md)):

- **Keymaps** — `nx.keymap.set(mode, lhs, rhs, opts)` / `nx.keymap.del`; introspect
  with `nx.keymap.get`. Always pass a `desc` — it surfaces in completion and
  which-key.
- **User commands** — `nx.command(name, fn, { desc = … })`; `fn` receives
  `{ args, fargs, bang, … }`.
- **Autocmds / events** — `nx.on(event, { pattern = … }, fn)` for editor lifecycle
  events (`BufReadPost`, `FileType`, …).
- **Options & vars** — read/write `nx.o` (global), `nx.bo` (buffer), `nx.wo`
  (window), and `nx.g` (globals). Edge docks have their own scope too:
  `nx.dock.opt(side)` (e.g. `nx.dock.opt("left").size = 32`), alongside
  `nx.bo`/`nx.wo` — per-dock `showtabline`, `laststatus`, `size`, `title`,
  `winhighlight`, and `autohide`.
- **Highlights** — `nx.hl.define(ns, name, spec)`, `nx.hl.get`, `nx.hl.exists`.
  Define your groups as fallbacks (`nx.hl.exists` guard) so a colorscheme that
  already styles them wins.
- **Messages** — `nx.notify(msg, level)`.
- **Async** — the editor is single-threaded and tick-based, so anything that waits
  is promise-based: `nx.async`/`nx.await`, `nx.promise`, `nx.run`/`nx.run_stream`
  (subprocesses), `nx.fs` (filesystem), `nx.timer`, `nx.utils.debounce`, and the
  scheduling primitives `nx.schedule` (end of the current tick) /
  `nx.on_next_tick` / `nx.wait_for(pred, opts)` (across ticks). Reach for
  `on_next_tick`/`wait_for` — never a `nx.schedule` re-arm — when waiting on state
  that only refreshes *between* ticks (e.g. a freshly-mounted window id).
- **UI** — the floating-widget layer `nx.ui.input`/`select`/`confirm`/`float`
  (promise-based, never steals focus for `float`), and `nx.component` (reactive
  state + a pure render + lifecycle) for live popups. Bigger server-owned surfaces:
  `nx.picker` (fuzzy finder), `nx.complete` (completion sources), `nx.snippet`,
  `nx.statusline` (composable segments), `nx.decor` (viewport decorations /
  extmarks), `nx.dock` (edge docks), and `nx.view` (a read-only mountable content
  surface — what a file-tree explorer is built on).

When something genuinely useful is missing, the convention is to add it to `nx.*`
for everyone, not to inline it as a plugin private.

## A worked example

[`nxvim-keys-helper`](https://github.com/davidrios/nxvim-keys-helper) — the
first-party which-key — is a compact, real-world plugin: it subscribes to the
pending-key oracle (`nx.on_key_pending`), debounces with `nx.utils.debounce`, and
renders the continuations on a non-focus `nx.component{ surface = "float" }`. It is
packaged exactly as above (`lua/nxvim-keys-helper/init.lua` with `setup`/`add`) and
carries its own test suite.

The [`examples/`](../examples) directory has ~40 self-contained configs — one per
feature — that double as plugin-authoring references.

## Testing

Plugins are pure Lua, so their tests are too. nxvim ships a native test framework
(`nx.test`) and a headless runner — write `test/*_spec.lua`, then:

```sh
nxvim --test-plugin .
```

The suite drives a **real** editor (feed keys, assert on buffer / cursor / UI) and
exits `0`/`1` for CI. See the full guide:
**[Testing nxvim plugins](specs/2026-06-19-lua-plugin-testing.md)**.

## See also

- [Native plugin API design](specs/2026-06-11-native-plugin-api.md) — the model and
  six worked API examples.
- [ADR 0002 — native plugin system](decisions/0002-native-plugin-system.md) — why
  `nx.*`, and the exact `vim.*` alias whitelist.
- [Testing nxvim plugins](specs/2026-06-19-lua-plugin-testing.md) — `nx.test` +
  `nxvim --test-plugin`.
- [Architecture](architecture.md) — the crate layout, tick model, and Lua bridge.
