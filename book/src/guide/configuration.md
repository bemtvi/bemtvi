# Configuration

nxvim reads a Lua config. On startup it resolves a **config directory** — the
first of `$NXVIM_CONFIG`, `$XDG_CONFIG_HOME/nxvim`, or `~/.config/nxvim` — and
sources `<config>/init.lua` before the first frame. The **runtimepath** is that
dir plus every `pack/*/start/*` entry under it:

```
~/.config/nxvim/
├── init.lua                      # sourced at startup
└── pack/
    └── plugins/
        └── start/
            └── myplugin/       # a plugin; its lua/ and colors/ are found here
```

```lua
-- ~/.config/nxvim/init.lua
require("myplugin").setup()
```

## `nx.*` vs `vim.*`

The editor's own config API is the **`nx.*` namespace** — see the
[API reference](../api/index.md). The only `vim.*` is a closed whitelist of
**muscle-memory aliases** (`vim.g`, `vim.o`/`vim.opt`, `vim.cmd`,
`vim.keymap.set`, autocmds, `vim.notify`, and friends), each a 1:1 alias over its
`nx.*` equivalent, so config can be written in familiar spellings:

```lua
vim.g.mapleader = " "
vim.o.number = true
nx.keymap.set("n", "<leader>w", "<cmd>w<cr>", { desc = "Save" })
```

The full whitelist lives in
[ADR 0002](https://github.com/davidrios/nxvim/blob/main/docs/decisions/0002-native-plugin-system.md).
A neovim colorscheme reaches for a handful of those aliases (notably the
`nvim_set_hl` highlight helper) and nothing more.


## Plugins — the built-in `:Plugins` manager

Dropping a checkout under `pack/*/start/*` works, but the ergonomic path is the
**built-in package manager**: there is no third-party manager layer because the
manager ships with nxvim. You *declare* a set of plugins in `init.lua` with
`nx.plugins{}`; it clones/updates them over the async runtime (via `nx.git_local`
— first-party `gix`, no `git` binary) and loads each one — adds its directory to the runtimepath so `require`
and its `colors/` / `queries/` / `lsp/` resolve without a restart, sources its
`plugin/` scripts, and runs its `config`. Nothing blocks: every step is a
promise, so the UI paints before plugins finish loading.

```lua
-- ~/.config/nxvim/init.lua
nx.plugins({
  -- "owner/repo" shorthand expands to a GitHub clone.
  { "nxvim/nxvim-keys-helper",
    config = function() require("nxvim-keys-helper").setup({}) end },

  -- Lazy-load on a trigger: any of cmd / event / ft / keys makes it lazy.
  { "someone/markdown-tools", ft = "markdown" },

  -- Pin a ref, rename, add dependencies.
  { "owner/repo", tag = "v1.2.0", name = "repo",
    dependencies = { "owner/dep" } },
})
```

Each spec is the repo (`"owner/repo"` shorthand, or `src` / `url`, or a local
`dir`) plus optional fields: `name`, `branch`, `tag` (alias `version`),
`commit`, `dependencies` (alias `deps`), `enabled`, `init` (run before load) and
`config` (run after). Lazy triggers — `cmd`, `event`, `ft`, `keys` — defer
loading until first use; set `lazy = false` to force eager load even with a
trigger. Clones land under the data dir (not your config repo), which the
manager owns.

Run `:Plugins` to open the **dashboard** — a lazy.nvim-style floating UI listing
every declared plugin grouped by load state, with live per-plugin progress (a
spinner while a clone/pull runs, ✓/✗ on finish) and verb keymaps: `I` install ·
`U` update · `S` sync · `R` restore · `X` clean. The same operations are available as
ex-commands:

| Command | Action |
| --- | --- |
| `:PluginSync` | Realize the declared + locked state: install missing, fast-forward what the lockfile doesn't pin |
| `:PluginInstall` | Clone any declared plugin not yet on disk |
| `:PluginUpdate` | Fast-forward every unpinned plugin, **advancing past** the lockfile |
| `:PluginClean` | Remove cloned dirs no spec declares |
| `:PluginLock` | Record every installed plugin's commit to the lockfile |
| `:PluginRestore` | Check out every plugin at the commit the lockfile records |
| `:PluginList` | Print a one-line status (installed / loaded / missing) per plugin |
| `:PluginsWelcome` | Reopen the first-run welcome checklist of recommended plugins |

nxvim ships minimal; on a fresh setup the welcome checklist offers a recommended
first-party set pre-ticked. See [Writing plugins](../plugins/authoring.md) for
authoring your own.

### The lockfile

Every install / update / sync records the commit each managed plugin resolved to in
`<config>/nxvim-lock.json`, and installing **reproduces** those commits. **Commit it**
alongside your `init.lua`: config + lockfile is the pair that pins the exact plugin tree,
so a second machine gets the same code instead of whatever each remote's `HEAD` happens to
be that day.

```json
{
  "catppuccin": { "branch": "main", "commit": "0b0a9a1…" },
  "nxvim-line": { "commit": "ada94b5…" }
}
```

It is generated, so treat it as output: a flat map keyed by plugin name, encoded with
sorted keys and 2-space indentation so a diff is one line per plugin that moved. A dev
`dir` plugin is never recorded (a working checkout is not a reproducible artifact), and
neither is a plugin that isn't installed yet. `nx.plugins.lock()` writes it on demand
(`:PluginLock`) and `nx.plugins.locked()` returns the current contents; a **malformed**
lockfile is a hard error naming the file rather than being treated as "nothing pinned".
Relocate it with `nx.plugins.setup_manager{ lockfile = … }`.

**Which revision wins.** Highest first:

1. `commit = "…"` in the spec — a hand-written pin is an instruction, and always beats
   the lockfile, which is only a record.
2. the lockfile entry — it outranks the floating refs below, because pinning a floating
   ref is exactly what it is for.
3. `tag` / `version`, then `branch`.
4. the remote's default branch.

A plugin with no lock entry and no pin still gets a fast `depth = 1` clone; reaching a
locked commit is the only reason to pay for a full one. If the lockfile names a commit the
remote no longer has (force-pushed away), the install **fails loud** and names the file to
fix — quietly installing something other than what the lockfile says would defeat the point
of having one.

**Reproduce vs advance** — the two verbs mean different things, like `cargo build` and
`cargo update`:

- **`:PluginSync`** (and `:PluginInstall`) *reproduce* the lockfile. Missing plugins are
  installed at their locked commits; a plugin the lockfile pins is left exactly where it
  is. Realizing your declared state never silently moves a plugin past the recorded
  revision.
- **`:PluginUpdate`** *advances past* it: each unpinned plugin is fast-forwarded to its
  branch tip and the lockfile is re-recorded. A plugin sitting on a locked (detached)
  commit is re-attached to the branch it tracks first, so a lock is never a permanent
  freeze.

A spec `commit` / `tag` pin is still never moved by either verb — that is what an explicit
pin means. And if a plugin is detached with no branch recorded anywhere (a hand-written
lock entry with only a `commit`), `:PluginUpdate` fails loud rather than guessing a branch:
add `branch = "…"` to its spec.

**Going back.** `:PluginRestore` (or `R` in the dashboard) checks every plugin out at the
commit the lockfile records — the "that update broke my editor" verb, and the reason
recording commits is worth anything. Check out an older `nxvim-lock.json` from your config
repo and restore to get exactly that plugin tree back.

Restore reaches commits a shallow clone does not contain: it deepens the clone
(`fetch --unshallow`) and retries, so an unpinned `depth = 1` install is no obstacle. A
plugin already at its locked commit is left alone rather than re-checked-out. A commit that
is genuinely gone from the remote is reported **loud and by name**, and that plugin's
checkout is left untouched — a rollback that silently skipped a plugin while reporting
success is the one failure you must not miss.

`:PluginList` and the dashboard mark a plugin **drifted** when its checkout is at a
different commit than the lockfile records — the signal that the tree no longer matches what
your config promises.

## Runnable examples

The [`examples/`](https://github.com/davidrios/nxvim/tree/main/examples)
directory has ~85 self-contained, end-to-end-verified configs — one per feature
(treesitter, LSP, floats, registers, tabs, mouse, statusline, completion,
picker, snippets, decor, docks, quickfix, image previews, …). Each is a config
dir you point nxvim at:

```sh
NXVIM_CONFIG=examples/treesitter cargo run -p nxvim -- examples/treesitter/sample.rs
```
