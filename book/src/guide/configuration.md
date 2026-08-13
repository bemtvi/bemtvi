# Configuration

bemtvi reads a Lua config. On startup it resolves a **config directory** — the
first of `$BEMTVI_CONFIG`, `$XDG_CONFIG_HOME/bemtvi`, or `~/.config/bemtvi` — and
sources `<config>/init.lua` before the first frame. The **runtimepath** is that
dir plus every `pack/*/start/*` entry under it:

```
~/.config/bemtvi/
├── init.lua                      # sourced at startup
└── pack/
    └── plugins/
        └── start/
            └── myplugin/       # a plugin; its lua/ and colors/ are found here
```

```lua
-- ~/.config/bemtvi/init.lua
require("myplugin").setup()
```

## `btv.*` vs `vim.*`

The editor's own config API is the **`btv.*` namespace** — see the
[API reference](../api/index.md). The only `vim.*` is a closed whitelist of
**muscle-memory aliases** (`vim.g`, `vim.o`/`vim.opt`, `vim.cmd`,
`vim.keymap.set`, autocmds, `vim.notify`, and friends), each a 1:1 alias over its
`btv.*` equivalent, so config can be written in familiar spellings:

```lua
vim.g.mapleader = " "
vim.o.number = true
btv.keymap.set("n", "<leader>w", "<cmd>w<cr>", { desc = "Save" })
```

The full whitelist lives in
[ADR 0002](https://github.com/bemtvi/bemtvi/blob/main/docs/decisions/0002-native-plugin-system.md).
A neovim colorscheme reaches for a handful of those aliases (notably the
`nvim_set_hl` highlight helper) and nothing more.


## Option scopes — which buffers a setting reaches

A buffer-local option (`tabstop`, `expandtab`, `foldmethod`, …) has two values: the
**local** one on each buffer, and the **global** one every newly created buffer is
born from. Which you write is what decides whether a config line applies to the
files you open later — the same model as vim:

```lua
vim.opt.tabstop = 3        -- :set       — this buffer AND the global value
vim.opt_local.tabstop = 8  -- :setlocal  — this buffer only
vim.opt_global.tabstop = 2 -- :setglobal — the global value only
```

`vim.o` and `vim.opt` are the ones a config almost always wants: a `vim.opt.tabstop
= 3` in `init.lua` sets the buffer you happen to be in *and* the value every file
opened afterwards starts from. `vim.bo` / `vim.wo` (and `vim.opt_local`) are the
per-instance surfaces — what a filetype rule uses, so one buffer's indent does not
become everyone's default:

```lua
btv.on("FileType", { pattern = "go", callback = function()
  vim.opt_local.expandtab = false   -- Go files only
end })
```

Reads follow the same split: `vim.o.tabstop` / `vim.bo.tabstop` report the current
buffer's value, `vim.go.tabstop` the global one. The ex commands `:set` /
`:setlocal` / `:setglobal` are the exact equivalents, and `:setglobal x?` reads the
global value where `:set x?` reads the buffer's. The by-name API spells the same
three: `nvim_set_option_value(name, value, {})` is a `:set` (both tiers), and a
`scope` or a `buf` / `win` target narrows it to one of them.

```lua
vim.api.nvim_set_option_value("tabstop", 3, {})                    -- :set
vim.api.nvim_set_option_value("tabstop", 8, { scope = "local" })   -- :setlocal
vim.api.nvim_set_option_value("tabstop", 2, { scope = "global" })  -- :setglobal
```

Three buffer options — `commentstring`, `foldexpr` and `foldmarker` — resolve their
global value as a **fallback when the buffer has none of its own**, rather than
copying it at creation. So a `:setglobal commentstring=…` reaches buffers that are
already open, where a `:setglobal tabstop=3` only reaches ones created afterwards. A
buffer that sets its own with `:setlocal` still wins either way. (This is a
deliberate departure from vim, where every buffer always holds a local value.)

A few buffer options have **no** global value, because the read decides them —
`fileencoding`, `bomb`, `fileformat`, `endofline` — as does `modifiable`, a
per-buffer marker, and the two nouns derived per buffer, `filetype` and
`ts_highlight`. `:setglobal` on one of those tells you so instead of storing a value
nothing would read.

Window options (`number`, `scrolloff`, `signcolumn`, …) carry the same two tiers,
with one extra rule from vim: a **split copies the window it came from**, so your
`init.lua` settings follow you into new splits whichever tier you wrote. The global
value is what `:setglobal` / `vim.go` read, and what a window created with no source
window to copy — a dock, the quickfix tab — is born from.


## Plugins — the built-in `:Plugins` manager

Dropping a checkout under `pack/*/start/*` works, but the ergonomic path is the
**built-in package manager**: there is no third-party manager layer because the
manager ships with bemtvi. You *declare* a set of plugins in `init.lua` with
`btv.plugins{}`; it clones/updates them over the async runtime (via `btv.git_local`
— first-party `gix`, no `git` binary) and loads each one — adds its directory to the runtimepath so `require`
and its `colors/` / `queries/` / `lsp/` resolve without a restart, sources its
`plugin/` scripts, and runs its `config`. Nothing blocks: every step is a
promise, so the UI paints before plugins finish loading.

```lua
-- ~/.config/bemtvi/init.lua
btv.plugins({
  -- "owner/repo" shorthand expands to a GitHub clone.
  { "bemtvi/bemtvi-keys-helper",
    config = function() require("bemtvi-keys-helper").setup({}) end },

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
spinner while a clone/pull runs, ✓/✗ on finish) and verb keymaps in two scopes.
**Upper-case acts on everything**: `I` install · `U` update · `S` sync · `R`
restore · `X` clean (plus `<C-r>` refresh, `<CR>` details, `q` quit).
**Lower-case acts on the plugin under the cursor**: `i` install · `u` update ·
`s` sync · `r` restore · `x` remove (delete just that clone — `x` then `i` is the
"give me a fresh copy" move). The same operations are available as ex-commands:

| Command | Action |
| --- | --- |
| `:PluginSync` | Realize the declared + locked state: install missing, check out what the lockfile pins, fast-forward the rest |
| `:PluginInstall` | Clone any declared plugin not yet on disk |
| `:PluginUpdate` | Fast-forward every unpinned plugin, **advancing past** the lockfile |
| `:PluginClean` | Remove cloned dirs no spec declares |
| `:PluginLock` | Record every installed plugin's commit to the lockfile |
| `:PluginRestore` | Check out every plugin at the commit the lockfile records |
| `:PluginList` | Print a one-line status (installed / loaded / missing) per plugin |
| `:PluginsWelcome` | Reopen the first-run offer of recommended plugins |

Every verb command above (all but `:PluginList` / `:PluginsWelcome`) takes an
**optional plugin list** — `:PluginUpdate bemtvi-tree`, `:PluginSync bemtvi-dap
bemtvi-line` — and `<Tab>` completes the declared names. Scoped that way it acts on
those plugins (and their dependencies, which a plugin needs to load at all) and
leaves every other plugin's checkout *and its lockfile entry* exactly as they were:
you get the one fix you were waiting for, not eleven other people's changes. The
same scope is a `plugins` option on the Lua verbs —
`btv.plugins.update({ plugins = "bemtvi-tree" })`. `:PluginClean` is the one verb
whose scope does *not* pull in dependencies (deleting a checkout nobody named is
destruction by inference), and it refuses outright to delete a local `dir`
plugin's checkout — that is your own working tree, not a managed clone.

bemtvi ships minimal; on a fresh setup it offers a recommended first-party set as
one decision, with `c` opening a checklist to pick individually — see
[Recommended plugins](recommended-plugins.md) for what's in it. See
[Writing plugins](../plugins/authoring.md) for authoring your own.

### The lockfile

Every install / update / sync records the commit each managed plugin resolved to in
`<config>/bemtvi-lock.json`, and installing **reproduces** those commits. **Commit it**
alongside your `init.lua`: config + lockfile is the pair that pins the exact plugin tree,
so a second machine gets the same code instead of whatever each remote's `HEAD` happens to
be that day.

```json
{
  "catppuccin": { "branch": "main", "commit": "0b0a9a1…" },
  "bemtvi-line": { "commit": "ada94b5…", "tag": "v2.1.0" }
}
```

It is generated, so treat it as output: a flat map keyed by plugin name, encoded with
sorted keys and 2-space indentation so a diff is one line per plugin that moved. An entry
records the commit *and* the declaration it resolved — the `branch` the plugin tracks, the
`tag` it was pinned to — which is what lets a changed spec invalidate it (below). A dev
`dir` plugin never contributes a commit (a working checkout is not a reproducible
artifact), and neither does one that isn't installed yet — but neither *loses* an entry
another machine wrote for it, since the file you commit must not be strip-mined by whichever
machine happens to sync. Only a plugin your config no longer declares is dropped.
`btv.plugins.lock()` writes it on demand (`:PluginLock`) and `btv.plugins.locked()` returns
the current contents; a **malformed** lockfile — including an entry with no commit — is a
hard error naming the file rather than being treated as "nothing pinned". Relocate it with
`btv.plugins.setup_manager{ lockfile = … }`.

**Which revision wins.** Highest first:

1. `commit = "…"` in the spec — a hand-written pin is an instruction, and always beats
   the lockfile, which is only a record.
2. the lockfile entry — it outranks the floating refs below, because pinning a floating
   ref is exactly what it is for.
3. `tag` / `version`, then `branch`.
4. the remote's default branch.

…but only while the entry still records what your spec *asks for*. Bump `tag = "v1"` to
`"v2"`, or point `branch` somewhere else, and the entry describes a resolution of a
question you are no longer asking: it is discarded and the new ref re-resolved, the way
`Cargo.lock` gives way to a changed `Cargo.toml`. Otherwise a record would outrank the
config that produced it, and your edit would be silently ignored forever.

A plugin with no lock entry and no pin still gets a fast `depth = 1` clone; reaching a
locked commit is the only reason to pay for a full one. If the lockfile names a commit the
remote no longer has (force-pushed away), the install **fails loud** and names the file to
fix — quietly installing something other than what the lockfile says would defeat the point
of having one.

**Reproduce vs advance** — the two verbs mean different things, like `cargo build` and
`cargo update`:

- **`:PluginSync`** *reproduces* the lockfile. Missing plugins are installed at their
  locked commits, and a plugin whose checkout has drifted from what the file records is
  moved back onto it — so pulling a colleague's newer `bemtvi-lock.json` and syncing gets
  you the tree it names. Realizing your declared state never moves a plugin *past* the
  recorded revision. (`:PluginInstall` only clones what is missing; it never moves an
  existing checkout, and records only the clones it made.)
- **`:PluginUpdate`** *advances past* it: each unpinned plugin is fast-forwarded to its
  branch tip and the lockfile is re-recorded. A plugin sitting on a locked (detached)
  commit is re-attached to the branch it tracks first, so a lock is never a permanent
  freeze.

Neither verb moves a plugin past an explicit `commit` / `tag` pin — that is what pinning
means — but both *realize* one: change the pin and the next sync checks the plugin out at
it. And if a plugin is detached with no branch recorded anywhere (a hand-written lock entry
with only a `commit`), `:PluginUpdate` fails loud rather than guessing a branch: add
`branch = "…"` to its spec.

**Going back.** `:PluginRestore` (or `R` in the dashboard) checks every plugin out at the
commit the lockfile records — the "that update broke my editor" verb, and the reason
recording commits is worth anything. Check out an older `bemtvi-lock.json` from your config
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

The [`examples/`](https://github.com/bemtvi/bemtvi/tree/main/examples)
directory has ~85 self-contained, end-to-end-verified configs — one per feature
(treesitter, LSP, floats, registers, tabs, mouse, statusline, completion,
picker, snippets, decor, docks, quickfix, image previews, …). Each is a config
dir you point bemtvi at:

```sh
BEMTVI_CONFIG=examples/treesitter cargo run -p bemtvi -- examples/treesitter/sample.rs
```
