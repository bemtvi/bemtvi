# The native plugin API (`nx.*`) — design sketch

> **Status: PROPOSAL (2026-06-11).** Supersedes the plugin half of
> [ADR 0001](../decisions/0001-native-engines-vendored-lua-apis.md): the
> "vendored neovim Lua APIs on top" strategy is **abandoned** for everything
> except trivial data-shaped surfaces (colorschemes via `nvim_set_hl`). This
> document sketches the replacement: nxvim's own plugin system, designed *for*
> the snapshot + effect-queue + client-server architecture instead of hiding it.

## Why the emulation failed

The compat effort kept succeeding at the test level ("this driven path passes")
and failing at the user level ("the plugin is a daily tool"). The root cause is
structural, not effort: neovim plugins are **imperative programs written against
neovim's runtime model** — synchronous re-entrant editor access, blocking reads
(`getcharstr`, `vim.wait`), libuv as a public API (`vim.uv` timers / check
handles / processes), frame-time rendering hooks (decoration providers), and the
unbounded `vim.fn` inventory. nxvim's model is snapshot reads + queued effects on
a pure synchronous core. Emulating the former on the latter means reimplementing
neovim's event loop and renderer contract shim by shim — the one thing this
architecture exists to refuse. The plugins that fought hardest (cmp, telescope,
which-key, lualine) are all **UI-orchestration programs**: they want to own frame
time and input loops.

That diagnosis dictates the design.

## The model in one sentence

**The server owns every UI surface and the frame; plugins are async, declarative
*providers* of data and behavior.** Where neovim says "here are buffer
primitives and hooks, draw your own completion menu", nxvim says "here is a
completion engine; give it items."

Five rules, all of which the architecture already enforces internally — the API
just makes them the documented contract instead of a hidden shim:

1. **Reads are snapshots.** `nx.buf.lines(b)` etc. read the state pushed at Lua
   entry. Documented, not disguised.
2. **Writes are queued effects.** Applied at the settle point
   (`apply_lua_effects → run_pending → redraw`), same as today. Async writers
   guard with a changedtick: `nx.buf.edit{tick = t, ...}` fails loud if stale.
3. **Nothing blocks, ever.** No `vim.wait`, no blocking `getcharstr`, no uv
   handles. Anything that waits takes a callback (`nx.ui.input`, `nx.spawn`,
   `nx.fs.*`, `nx.timer`). This deletes the pcall-yield problem class on PUC Lua
   by construction.
4. **No frame-time Lua.** Plugins publish decorations / segments / items
   whenever they like; the server folds them into the next frame. A plugin can
   never make redraw slow. (ADR 0001's bridge pattern, promoted to *the* API.)
5. **Registrations are data.** Providers register with a name + schema and get
   called with a context + a `respond` continuation carrying a generation token;
   stale responses are dropped by the engine.

Because Lua already influences the editor only through the same queues RPC
clients use, every `nx.*` registration gets an RPC twin for free
(`nx_complete_register`, …) — out-of-process plugins in any language are the
same surface, later. The in-process Lua host is v1.

## The surface

| Namespace | What it is | Backed by (exists today) |
| --- | --- | --- |
| `nx.buf` / `nx.win` / `nx.cursor` | snapshot reads, queued edits, `on_change` byte-delta subscription | mirrors + effect queues + the edit journal |
| `nx.on(event, opts, fn)` | structured event subscriptions | the lifecycle/autocmd diff |
| `nx.spawn` / `nx.timer` / `nx.fs.*` | async process / timer / fs, callback-based | evloop actor + HostFs seams |
| `nx.hl.set(ns, buf, marks)` | batch-published decorations | the extmark layer + priorities |
| `nx.keymap` / `nx.command` / `nx.cmd` | maps, user commands, ex dispatch | existing |
| `nx.ui.input` / `select` / `confirm` / `float` | small async UI primitives | cmdline + floats + pmenu |
| `nx.complete` | **native completion engine**; plugins = sources | pmenu + docs float, native LSP, evloop debounce; Rust fuzzy matcher (new) |
| `nx.statusline` | segment registry + layout; event-keyed invalidation | server-side statusline render |
| `nx.picker` | **native fuzzy picker** (prompt + list + preview); plugins = sources | floats + the panel's input-grab pattern; matcher shared with completion |
| `nx.snippet` | **native snippet engine** (LSP grammar, tabstop mode, choices) | the existing LSP-snippet parse; tabstop session modeled like multi-cursor placement mode |
| `nx.tree` | generic dock/tree views (file explorer, symbols, git) | the panel generalized to a persistent vertical dock |

### Plugins, manifests, and lazy-loading (the lazy.nvim answer)

A plugin is a directory with a cheap data-only manifest; code loads on first
contribution hit (VS Code-style activation, which lazy.nvim approximates from
the outside because neovim plugins can't declare contributions):

```lua
-- ~/.config/nxvim/plugins/nx-files/plugin.lua  (data only; no requires)
return {
  name = "nx-files",
  contributes = {
    picker  = { "files", "live_grep" },
    tree    = { "files" },
    command = { "FilesToggle" },
  },
}
```

`init.lua` declares the set; a built-in manager syncs it over the async runtime
(real `git clone` via `nx.spawn` — the machinery the lazy compat work already
proved out):

```lua
nx.plugins {
  { "davidrios/nx-files" },
  { "davidrios/nx-emoji" },
}
-- :PluginSync clones/updates; :PluginList shows state
```

There is no lazy.nvim-shaped plugin because there is nothing left to optimize
around: manifests defer code load by construction, and the UI paints before
plugins finish loading anyway (the server is async; startup is not a single
blocking script).

## The five proofs

Each of the plugins that defeated the compat layer, rebuilt as a provider. In
every case the *hard* part moves into the server in Rust and the plugin shrinks
to data + small callbacks.

### 1. nvim-cmp → the native completion engine + sources

The engine owns trigger detection (input path), debounce (evloop), source
fan-out with generation tokens, fuzzy ranking (Rust), the menu + matched-char
highlighting + doc float (the pmenu already renders docs), and snippet expansion
on accept. Recall what cmp's compat chain actually consisted of —
`timer:is_active`, the `vim.wait` pump, uv check-handle methods, `hlID`/`syn*`,
decoration providers — none of it was *completion*; all of it was runtime
emulation so cmp could run its own menu. All of it evaporates.

```lua
-- init.lua — completion is built in; this is the whole setup
nx.complete.setup {
  sources = {
    { "lsp",      priority = 100 },              -- built-in (native LSP client)
    { "snippets", priority = 80  },              -- built-in (native snippet engine)
    { "buffer",   priority = 10, min_chars = 3 },-- built-in (rope-side word scan)
    { "emoji" },                                 -- from a plugin, below
  },
  auto = true,                                   -- complete as you type; engine debounces
  keys = { next = "<Tab>", prev = "<S-Tab>", confirm = "<CR>", abort = "<C-e>" },
}
```

A third-party source — this is the *entire* plugin:

```lua
-- plugins/nx-emoji/lua/nx-emoji/init.lua
local emoji = require("nx-emoji.data")           -- { { ":smile:", "😄" }, ... }

nx.complete.source {
  name = "emoji",
  trigger = { chars = { ":" } },                 -- engine wakes us only after ':'
  complete = function(ctx, respond)
    -- ctx = { buf, row, col, line, prefix, token } — a snapshot, never live state
    local items = {}
    for _, e in ipairs(emoji) do
      if e[1]:find(ctx.prefix, 1, true) then
        items[#items + 1] = { label = e[1], insert = e[2], kind = "emoji", doc = e[2] }
      end
    end
    respond { items = items }                    -- may be called async; a stale
  end,                                           -- token is dropped by the engine
  resolve = function(item, respond)              -- optional: lazy docs
    respond(item)
  end,
}
```

### 2. lualine → statusline segments

The server already renders status lines. Segments are functions re-evaluated
**only on declared events** — never per frame (lualine's model is "re-enter Lua
every redraw", which is exactly the forbidden shape). The server caches each
segment's resolved cells and paints natively.

```lua
nx.statusline.setup {
  left  = { "mode", "git", "filename", "diagnostics" },   -- built-ins + plugin segments
  right = { "lsp_progress", "filetype", "location" },
}

-- a custom segment (the lualine "component"):
nx.statusline.segment {
  name = "git",
  events = { "buf:enter", "dir:changed", "user:git" },    -- the invalidation set
  render = function(ctx)                                  -- ctx = { buf, win, focused, width }
    local b = cached_branch[nx.buf.name(ctx.buf)]
    return b and { { text = " " .. b, hl = "StatusGit" } } or nil
  end,
}

-- async data: recompute, then invalidate yourself
nx.spawn { cmd = "git", args = { "branch", "--show-current" },
  on_exit = function(res)
    cached_branch[cwd] = res.stdout:gsub("%s+$", "")
    nx.statusline.invalidate("git")
  end }
```

### 3. telescope → the native picker + sources

The picker UI is server-owned: prompt line, result list, preview pane (floats),
fuzzy matching in Rust (nucleo-class), and **all navigation/typing handled
natively** — Lua sees only "query changed" (dynamic sources) and "confirmed".
Telescope's entire performance story (the prompt-buffer `on_lines` → Lua sorter
loop per keystroke) ceases to exist.

```lua
nx.keymap.set("n", "<leader>ff", function() nx.picker.open("files") end)

-- a static, streaming source:
nx.picker.source {
  name = "files",
  items = function(ctx, push, done)              -- results stream in as found
    nx.spawn { cmd = "rg", args = { "--files" }, cwd = ctx.cwd,
      on_stdout = function(lines)
        for _, l in ipairs(lines) do push { text = l, path = l } end
      end,
      on_exit = done }
  end,
  preview = "file",                              -- declarative: server previews item.path
                                                 -- (rope + native treesitter, zero Lua)
  confirm = function(item) nx.cmd("edit " .. nx.fnameescape(item.path)) end,
}

-- a dynamic source (live grep): re-run per prompt edit, matcher bypassed
nx.picker.source {
  name = "live_grep",
  dynamic = true,
  items = function(ctx, push, done)
    if ctx.query == "" then return done() end
    local p = nx.spawn { cmd = "rg", args = { "--vimgrep", "--", ctx.query },
      on_stdout = function(lines)
        for _, l in ipairs(lines) do push(parse_vimgrep(l)) end
      end,
      on_exit = done }
    ctx.on_cancel(function() p:kill() end)       -- superseded queries are reaped
  end,
  preview = "location",
  confirm = function(item)
    nx.cmd("edit " .. nx.fnameescape(item.path))
    nx.cursor.set(item.row, item.col)
  end,
}
```

### 4. LuaSnip → the native snippet engine

The server owns the LSP snippet grammar (a parser already exists for pmenu
inserts), expansion, the tabstop session (a small input mode — multi-cursor
placement mode is the in-repo precedent for exactly this shape), mirrored
placeholders, and `${1|a,b|}` choices via the native pmenu. Plugins contribute
snippet *data*, with functions for dynamic bodies:

```lua
nx.snippet.setup { jump_next = "<C-j>", jump_prev = "<C-k>" }

nx.snippet.add("rust", {
  { trigger = "fn",   body = "fn ${1:name}(${2}) -> ${3:()} {\n\t$0\n}" },
  { trigger = "test", body = "#[test]\nfn ${1:it_works}() {\n\t${0:assert!(true);}\n}" },
  { trigger = "date", body = function(ctx) return os.date("%Y-%m-%d") end },
  { trigger = "mod",  body = function(ctx)            -- context-aware
      return "mod ${1:" .. nx.fs.stem(nx.buf.name(ctx.buf)) .. "} {\n\t$0\n}"
    end },
})
```

A friendly-snippets loader is a ten-line plugin: `nx.fs.read` the VS Code JSON,
`nx.snippet.add` per filetype. The completion engine's `snippets` source and
LSP completions with snippet bodies expand through the same engine.

### 5. nvim-tree → `nx.tree` dock views

Not a file-explorer built-in but a generic **tree view** surface (file explorer,
symbol outline, git status are all instances). The server owns the dock window
(the panel generalized to a persistent vertical dock), rendering, expand/collapse
state, cursor movement, and key routing; the plugin supplies children and
actions. No buffer puppeteering, no `getcharstr` prompts.

```lua
local view = nx.tree.view {
  name = "files", title = "Files", side = "left", width = 32,
  root = function() return { path = nx.cwd(), dir = true } end,
  children = function(node, respond)
    nx.fs.readdir(node.path, function(err, entries)
      if err then return respond(nil, err) end
      respond(map(entries, function(e)
        return { text = e.name, dir = e.dir, path = e.path,
                 icon = e.dir and "" or icon_for(e.name) }
      end))
    end)
  end,
  actions = {
    ["<CR>"] = function(node, t)
      if node.dir then t:toggle(node) else nx.cmd("edit " .. nx.fnameescape(node.path)) end
    end,
    ["a"] = function(node, t)
      nx.ui.input({ prompt = "New file: " }, function(name)        -- async, not blocking
        if not name then return end
        nx.fs.write(join(dir_of(node), name), "", function() t:refresh(node) end)
      end)
    end,
    ["d"] = function(node, t)
      nx.ui.confirm("Delete " .. node.path .. "?", function(yes)
        if yes then nx.fs.remove(node.path, function() t:refresh(t:parent(node)) end) end
      end)
    end,
  },
}
nx.keymap.set("n", "<leader>e", function() view:toggle() end)
nx.fs.watch(nx.cwd(), function() view:refresh() end)
```

## What dies, what stays

**Dies:** `vim.wait` and the pump, the `vim.uv` emulation (timers/checks/
processes as public handles), decoration providers, the `vim.fn` long tail,
prompt-buffer emulation, the pcall-yield concern, `prelude/compat.lua` growth.
The existing compat code is frozen, not ripped out, until `nx` covers config
needs.

**Stays:** colorscheme compat (`nvim_set_hl` data — catppuccin keeps working);
the runtimepath/`require` machinery; the snapshot/queue/settle machinery itself
(it *is* the `nx` contract, finally documented); the native LSP, treesitter,
extmark, pmenu, float, and panel subsystems — they're the engines the surfaces
above expose.

## Suggested build order

1. **`nx` core** (buf/win/event/spawn/fs/timer/keymap/ui.input — mostly renames
   + contracts over existing machinery) and the manifest loader.
2. **Picker** — highest user value; exercises spawn/stream/cancel/float/preview.
3. **Completion engine** — LSP + buffer + snippets sources built-in.
4. **Statusline segments**, **snippet engine** (shared with 3), **tree docks**.
5. RPC twins of the registries (out-of-process providers) — later, free-ish.
