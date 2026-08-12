# Writing bemtvi plugins

A plugin is **pure Lua over the `btv.*` API** — no Rust, no Vimscript. The server
owns every UI surface (windows, floats, the completion menu, the statusline); a
plugin supplies *data and behavior* and reaches the screen through `btv.*`.
Our prime directive is every feature that can be a plugin *is* one, so we exercise
our own APIs.

If you've written a neovim plugin, the shape is familiar — a `lua/<name>/init.lua`
module exposing `setup(opts)` — but the API you call is `btv.*`, not `vim.*`. (A
closed whitelist of `vim.*` muscle-memory aliases exists for config ergonomics; new
plugin code should target `btv.*` directly.)

## Writing plugins with a coding agent

The `btv.*` model is not in any agent's training data — left to itself, an agent
writes neovim plugins that fail on bemtvi (`vim.uv`, blocking `vim.wait`, a
`nvim_buf_set_lines` that does not exist). **[bemtvi-plugin-skills](https://github.com/bemtvi/bemtvi-plugin-skills)**
is a set of agent skills that teach it the real model, the surfaces below, and the
traps; it also bundles a generated snapshot of the `btv.*` API reference and every
public `btv.*`/`vim.*` name in a live editor.

```sh
npx skills add https://github.com/bemtvi/bemtvi-plugin-skills   # any agent
```

Claude Code can install it as a plugin instead:

```
/plugin marketplace add bemtvi/bemtvi-plugin-skills
/plugin install bemtvi-plugin-skills
```

One skill per surface — scaffolding, async, keymaps/commands/events,
buffers/windows, UI and components, views and docks, decorations, picker,
completion, statusline, LSP and diagnostics, fs/process/net, testing, vimdoc, and
porting from neovim — with the `bemtvi` skill routing to the right one.

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
  btv.keymap.set("n", "<leader>x", function()
    btv.notify("hello from my-plugin", "info")
  end, { desc = "my-plugin: do the thing" })
end

return M
```

`setup` should be **idempotent** (a user may call it more than once) and side-effect
light at module load — do the wiring in `setup`, not at `require` time, so load order
and lazy-loading stay predictable.

## Installing & loading

Two paths, both runtimepath-based:

**The built-in manager (`btv.plugins`).** Declare plugins in `init.lua`; the manager
clones, runtimepaths, and loads them (with optional lazy triggers):

```lua
btv.plugins({
  -- eager:
  { "bemtvi/bemtvi-keys-helper",
    config = function() require("bemtvi-keys-helper").setup({}) end },

  -- lazy by key / command / event / filetype:
  { "owner/repo", keys = { "<leader>ff" }, cmd = "Find",
    config = function() require("repo").setup({}) end },

  -- pinned, or a local checkout for development:
  { "owner/repo", tag = "v1.0.0" },
  { name = "my-plugin", dir = "/path/to/my-plugin",
    config = function() require("my-plugin").setup({}) end },
})
```

Then `:PluginSync` (realize the declared + locked state), `:PluginInstall`,
`:PluginUpdate` (fast-forward, advancing past the lockfile), `:PluginRestore`
(check out the locked commits), `:PluginLock`, `:PluginClean`, `:PluginList`, or the
`:Plugins` dashboard. Cloned plugins live under `stdpath("data")/plugins/<name>`.

Each of those verbs takes an optional plugin list — `:PluginUpdate my-plugin`,
`<Tab>`-completed — and then touches only those plugins (and their dependencies),
leaving every other checkout and lockfile entry alone; in the dashboard the
lower-case keys (`i` `u` `s` `r` `x`) do the same for the row under the cursor. When
you are developing one plugin, that is the verb you want: re-cloning or updating the
whole set to test one change is how a working editor turns into a bisect.

Every install/update records each plugin's resolved commit in
`<config>/bemtvi-lock.json`, and installing reproduces those commits — so a config plus
its lockfile pins the exact plugin tree. `:PluginSync` therefore does *not* move a
plugin the lockfile pins; advancing past it is `:PluginUpdate`'s job, and
`:PluginRestore` goes back. See
[Configuration → the lockfile](../book/src/guide/configuration.md) for the full rules. A spec with `cmd`/`event`/`ft`/`keys` (or
`lazy = true`) loads on first use; `config` runs after the plugin is on the
runtimepath, `init` runs at startup regardless. Restoring a workspace session counts as
first use too: a persisted [`btv.view`](features/ui-primitives.md#persisting-a-view-across-sessions)
slot names its owner, so a lazy plugin whose sidebar was open when you quit is loaded by
the restore itself — no trigger to press, nothing extra to declare.

Git submodules are initialised by default — the manager clones with
`--recurse-submodules` and runs `git submodule update --init --recursive` on update,
so a plugin that vendors its dependencies as submodules lands complete. Set
`submodules = false` on a spec to opt a plugin out (skips the extra git work for one
you know has none).

**By hand.** Drop the plugin under `<config>/pack/*/start/*` and `require` it from
`init.lua` — the runtimepath picks it up with no manager involved.

## The `btv.*` surfaces you'll use

A plugin composes these (each has runnable examples under [`examples/`](../examples)
and a deeper treatment in the [API design](specs/2026-06-11-native-plugin-api.md)):

- **Keymaps** — `btv.keymap.set(mode, lhs, rhs, opts)` / `btv.keymap.del`; introspect
  with `btv.keymap.get`. Always pass a `desc` — it surfaces in completion and
  which-key.
- **User commands** — `btv.command(name, fn, { desc = …, usage = …, complete = … })`;
  `fn` receives `{ args, fargs, bang, … }`. `usage` is the argument signature in vim
  help notation (`usage = "[file]"`, `"{name}"`) — it heads the command's `:`-completion
  docs pane as `:Name <usage>`, exactly like a built-in. `complete` (`"file"` / `"dir"`
  / a `fn(args)`) drives `<Tab>` completion of the argument.
- **Autocmds / events** — `btv.on(event, { pattern = … }, fn)` for editor lifecycle
  events (`BufReadPost`, `FileType`, …), or `btv.on(event, fn)` with no options.
- **Options & vars** — read/write `btv.o` (global), `btv.bo` (buffer), `btv.wo`
  (window), and `btv.g` (globals). Edge docks have their own scope too:
  `btv.dock.opt(side)` (e.g. `btv.dock.opt("left").size = 32`), alongside
  `btv.bo`/`btv.wo` — per-dock `showtabline`, `laststatus`, `size`, `title`,
  `winhighlight`, and `autohide`.
- **Highlights** — `btv.hl.define(ns, name, spec)`, `btv.hl.get`, `btv.hl.exists`.
  Define your groups as fallbacks (`btv.hl.exists` guard) so a colorscheme that
  already styles them wins.
- **Messages** — `btv.notify(msg, level)`.
- **Async** — the editor is single-threaded and tick-based, so anything that waits
  is promise-based: `btv.async`/`btv.await`, `btv.promise`, `btv.run`/`btv.run_stream`
  (subprocesses), `btv.fs` (filesystem), `btv.timer`, `btv.utils.debounce`, and the
  scheduling primitives `btv.schedule` (end of the current tick) /
  `btv.on_next_tick` / `btv.wait_for(pred, opts)` (across ticks). Reach for
  `on_next_tick`/`wait_for` — never a `btv.schedule` re-arm — when waiting on state
  that only refreshes *between* ticks (e.g. a freshly-mounted window id). Full
  guide: **[Async & promises](async.md)**.
- **UI** — the floating-widget layer `btv.ui.input`/`select`/`confirm`/`float`
  (promise-based, never steals focus for `float`), and `btv.component` (reactive
  state + a pure render + lifecycle) for live popups. Bigger server-owned surfaces:
  `btv.picker` (fuzzy finder), `btv.complete` (completion sources), `btv.snippet`,
  `btv.statusline` (composable segments), `btv.decor` (viewport decorations /
  extmarks), `btv.dock` (edge docks), and `btv.view` (a read-only mountable content
  surface — what a plugin ui is built on).

When something genuinely useful is missing, the convention is to add it to `btv.*`
for everyone, so let us know if you find anything missing.

## A worked example

[`bemtvi-keys-helper`](https://github.com/bemtvi/bemtvi-keys-helper) — the
first-party which-key — is a compact, real-world plugin: it subscribes to the
pending-key oracle (`btv.on_key_pending`), debounces with `btv.utils.debounce`, and
renders the continuations on a non-focus `btv.component{ surface = "float" }`. It is
packaged exactly as above (`lua/bemtvi-keys-helper/init.lua` with `setup`/`add`) and
carries its own test suite.

The [`examples/`](../examples) directory has ~85 self-contained configs — one per
feature — that double as plugin-authoring references.

## Testing

Plugins are pure Lua, so their tests are too. bemtvi ships a native test framework
(`btv.test`) and a headless runner — write `test/*_spec.lua`, then:

```sh
bemtvi --test-plugin .
```

The suite drives a **real** editor (feed keys, assert on buffer / cursor / UI) and
exits `0`/`1` for CI. See the full guide: **[Testing plugins](plugin-testing.md)**.

## See also

- [bemtvi-plugin-skills](https://github.com/bemtvi/bemtvi-plugin-skills) —
  agent skills for authoring plugins, one per `btv.*` surface.
- [Native plugin API design](specs/2026-06-11-native-plugin-api.md) — the model and
  six worked API examples.
- [ADR 0002 — native plugin system](decisions/0002-native-plugin-system.md) — why
  `btv.*`, and the exact `vim.*` alias whitelist.
- [Testing bemtvi plugins](specs/2026-06-19-lua-plugin-testing.md) — `btv.test` +
  `bemtvi --test-plugin`.
- [Architecture](architecture.md) — the crate layout, tick model, and Lua bridge.
