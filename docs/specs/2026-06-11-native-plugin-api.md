# The native plugin API (`nx.*`) — design sketch

> **Status: PARTIALLY IMPLEMENTED (proposed 2026-06-11).** The design for
> nxvim's plugin system — the extensibility half of
> [ADR 0002](../decisions/0002-native-plugin-system.md): the server owns every
> UI surface and the frame, plugins are async, declarative *providers*, and
> the only `vim.*` surface nxvim ships is a closed whitelist of muscle-memory
> aliases over `nx.*`.
>
> **Landed (2026-06-12,
> [foundation plan](../plans/2026-06-12-nx-foundation-and-treesitter.md)):** the
> `nx.*` namespace as the canonical config surface with `vim.*` re-expressed as
> the bounded alias whitelist — `nx.g`/`b`/`w`, `nx.o`/`opt`/`bo`/`wo`/`go`,
> `nx.cmd`, `nx.keymap`, `nx.on`, `nx.command`, `nx.notify`/`schedule` — plus
> `nx.treesitter` (highlight control as the `nx.bo.filetype` / `nx.bo.ts_highlight`
> two nouns, and `nx.treesitter.set_query`). The UI-orchestration registries below
> (`nx.complete` / `picker` / `statusline` / `snippet` / `tree`) and the async
> primitives (`nx.run` / `nx.run_stream` / `timer` / `fs` / `ui`) remain proposed.

## Why not neovim's plugin model

neovim plugins are **imperative programs written against neovim's runtime
model** — synchronous re-entrant editor access, blocking reads (`getcharstr`,
`vim.wait`), libuv as a public API (`vim.uv` timers / check handles /
processes), frame-time rendering hooks (decoration providers), and the
unbounded `vim.fn` inventory. nxvim's model is snapshot reads + queued effects
on a pure synchronous core, behind a client-server boundary. Hosting the
former on the latter would mean reimplementing neovim's event loop and
renderer contract underneath someone else's API — surrendering the properties
that are the point of the design (a pure core, a frame no script can stall,
identical behavior across front ends including the serverless wasm builds).

The plugins that define the ecosystem's UX — completion menus, fuzzy pickers,
statuslines, popups, tree sidebars — are precisely **UI-orchestration
programs**: they want to own frame time and input loops. That observation
dictates the design.

## The model in one sentence

**The server owns every UI surface and the frame; plugins are async,
declarative *providers* of data and behavior.** Where neovim says "here are
buffer primitives and hooks, draw your own completion menu", nxvim says "here
is a completion engine; give it items."

Five rules — each one a property the architecture already enforces
internally; the API makes them the documented contract:

1. **Reads are snapshots.** `nx.buf.lines(b)` etc. read the state pushed at
   Lua entry. Documented, not disguised.
2. **Writes are queued effects.** Applied at the settle point
   (`apply_lua_effects → run_pending → redraw`). Async writers guard with a
   changedtick: `nx.buf.edit{tick = t, ...}` fails loud if stale.
3. **Nothing blocks, ever.** No wait-pumps, no blocking reads, no uv handles.
   Anything that waits returns a **promise** (`nx.run`, `nx.fs.*`, `nx.timer`'s
   `nx.promise.delay`) you `nx.await` inside `nx.async`, or — for multi-value
   streaming — an async-iterator (`nx.run_stream` + `nx.await_each`); `nx` is
   promise-only (no callback-shaped async). Event subscriptions (`nx.autocmd`,
   keymap rhs) and emit sinks (a picker source's `ctx.push`, a `nx.decor` provider's
   `publish`) stay handler-based — they fire repeatedly, so a promise is the wrong
   shape. (See [the promise-only migration](../plans/2026-06-16-nx-promise-only-async.md).)
4. **No frame-time Lua.** Plugins publish decorations / segments / items
   whenever they like; the server folds them into the next frame. A plugin
   cannot make redraw slow. (ADR 0001's bridge pattern, generalized into *the*
   extension contract.)
5. **Registrations are data.** Providers register with a name + schema and get
   called with a context carrying a generation token; they emit through the
   context's sink (`ctx.push`) and signal completion by returning (a promise, or
   nothing for a synchronous provider). Stale responses are dropped by the engine.

Because Lua influences the editor only through the same queues RPC clients
use, every `nx.*` registration gets an RPC twin in principle
(`nx_complete_register`, …) — out-of-process plugins in any language are the
same surface, later. The in-process Lua host is v1.

## The surface

| Namespace | What it is | Backed by (exists today) |
| --- | --- | --- |
| `nx.buf` / `nx.win` / `nx.cursor` | snapshot reads, queued edits, `on_change` byte-delta subscription | mirrors + effect queues + the edit journal |
| `nx.regex` / `nx.buf.search` | real-regex matching for Lua strings (a `string`-library-shaped object) / native buffer text search | the core regex engines — the Rust `regex` crate (`pcre`) + the vendored vim engine (`vim`) |
| `nx.on(event, opts, fn)` | structured event subscriptions | the lifecycle/autocmd diff |
| `nx.run` / `nx.run_stream` / `nx.timer` / `nx.fs.*` | async process (promise / async-iterator) / timer / fs | evloop actor + HostFs seams |
| `nx.hl.set(ns, buf, marks)` | batch-published decorations (known up front) | the extmark layer + priorities |
| `nx.decor.provider` | viewport-scoped decoration publisher — lazy, recomputed on scroll, off the frame | the decoration-provider drive (`decor_on_win`), debounced off `redraw`; folds into the extmark layer |
| `nx.keymap` / `nx.command` / `nx.cmd` | maps, user commands, ex dispatch | existing |
| `nx.ui.input` / `select` / `confirm` / `float` | small async UI primitives (input/select/confirm are promise-only; float is fire-and-forget) | cmdline + floats + pmenu |
| `nx.complete` | **native completion engine**; plugins = sources | pmenu + docs float, native LSP, evloop debounce; Rust fuzzy matcher (new) |
| `nx.statusline` | segment registry + layout; event-keyed invalidation | server-side statusline render |
| `nx.picker` | **native fuzzy picker** (prompt + list + preview); plugins = sources | floats + the panel's input-grab pattern; matcher shared with completion |
| `nx.snippet` | **native snippet engine** (LSP grammar, tabstop mode, choices) | the existing LSP-snippet parse; tabstop session modeled like multi-cursor placement mode |
| `nx.tree` | generic dock/tree views (file explorer, symbols, git) | the panel generalized to a persistent vertical dock |
| `nx.shada.plugin()` | opt-in, **isolated** cross-session key/value storage; the namespace is assigned from the calling plugin's location, not chosen | a dedicated table in the shada store + the existing flush cadence |

The same `nx.*` namespace is the **config** API: `init.lua` is written against
it (`nx.o`/`nx.opt` for options, `nx.keymap`, `nx.on`, `nx.command`, `nx.lsp`
for server setup, `nx.treesitter` for tree scripting). The only `vim.*` Lua is
a closed set of muscle-memory aliases (see *The `vim.*` boundary* below).

### Plugins, manifests, and activation (why there is no plugin-manager plugin)

A plugin is a directory with a cheap data-only manifest; code loads on first
contribution hit (VS Code-style activation — the thing a plugin manager approximates
from the outside, because neovim plugins cannot *declare* their triggers):

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

`init.lua` declares the set; the built-in manager syncs it over the async
runtime (real `git clone` via `nx.run`):

```lua
nx.plugins {
  { "nxvim/nx-files" },
  { "nxvim/nx-emoji" },
}
-- :PluginSync clones/updates; :PluginList shows state
```

There is no third-party plugin-manager layer because there is nothing for one
to optimize: manifests defer code load by construction, and the UI paints
before plugins finish loading anyway (the server is async; startup is not a
single blocking script).

## Six worked examples

The ecosystem staples a daily driver needs, designed as providers. In every
case the *hard* part is the server's job, in Rust, built once — and the
plugin shrinks to data + small callbacks.

### 1. Completion (the completion-menu shape) — native engine + sources

The engine owns trigger detection (input path), debounce (evloop), source
fan-out with generation tokens, fuzzy ranking (Rust), the menu +
matched-char highlighting + doc float (the pmenu already renders docs), and
snippet expansion on accept. Under neovim, a completion plugin must build all
of that itself — its own menu windows, debounce on libuv check handles,
frame-time decoration providers for matched-char highlights. Here none of it
is plugin surface.

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

### 2. Statusline (the lualine shape) — segments

The server already renders status lines. Segments are functions re-evaluated
**only on declared events** — never per frame (the "re-enter Lua every
redraw" model is exactly what rule 4 forbids). The server caches each
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

-- async data: recompute, then invalidate yourself (nx.run is a promise — await it)
local refresh = nx.async(function()
  local res = nx.await(nx.run { cmd = "git", args = { "branch", "--show-current" } })
  cached_branch[cwd] = res.stdout:gsub("%s+$", "")
  nx.statusline.invalidate("git")
end)
refresh()
```

### 3. Fuzzy finding (the fuzzy-finder shape) — native picker + sources

The picker UI is server-owned: prompt line, result list, preview pane
(floats), fuzzy matching in Rust (nucleo-class), and **all navigation/typing
handled natively**. Under neovim a picker's hot loop is Lua — a prompt-buffer
callback re-sorting results on every keystroke; here Lua sees only "query
changed" (dynamic sources) and "confirmed".

```lua
nx.keymap.set("n", "<leader>ff", function() nx.picker.open("files") end)

-- a static, streaming source: an nx.async iterator over nx.run_stream's batches.
-- The source emits via ctx.push and completes by returning (no `done` callback).
nx.picker.source {
  name = "files",
  items = nx.async(function(ctx)               -- results stream in as found
    for batch in nx.await_each(nx.run_stream { cmd = "rg", args = { "--files" }, cwd = ctx.cwd }) do
      for _, l in ipairs(batch) do ctx.push { text = l, path = l } end
    end
  end),
  preview = "file",                              -- declarative: server previews item.path
                                                 -- (rope + native treesitter, zero Lua)
  confirm = function(item) nx.cmd("edit " .. nx.fnameescape(item.path)) end,
}

-- a dynamic source (live grep): re-run per prompt edit, matcher bypassed
nx.picker.source {
  name = "live_grep",
  dynamic = true,
  items = nx.async(function(ctx)
    if ctx.query == "" then return end
    local stream = nx.run_stream { cmd = "rg", args = { "--vimgrep", "--", ctx.query } }
    ctx.on_cancel(function() stream:kill() end)  -- superseded queries are reaped
    for batch in nx.await_each(stream) do
      for _, l in ipairs(batch) do ctx.push(parse_vimgrep(l)) end
    end
  end),
  preview = "location",
  confirm = function(item)
    nx.cmd("edit " .. nx.fnameescape(item.path))
    nx.cursor.set(item.row, item.col)
  end,
}
```

### 4. Snippets (the LuaSnip shape) — native engine

The server owns the LSP snippet grammar (a parser already exists for pmenu
inserts), expansion, the tabstop session (a small input mode — multi-cursor
placement mode is the in-repo precedent for exactly this shape), mirrored
placeholders, and `${1|a,b|}` choices via the native pmenu. Plugins
contribute snippet *data*, with functions for dynamic bodies:

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

A friendly-snippets loader is a ten-line plugin: `nx.fs.read` the VS Code
JSON, `nx.snippet.add` per filetype. The completion engine's `snippets`
source and LSP completions with snippet bodies expand through the same
engine.

### 5. File explorer (the nvim-tree shape) — `nx.tree` dock views

Not a file-explorer built-in but a generic **tree view** surface (file
explorer, symbol outline, git status are all instances). The server owns the
dock window (the panel generalized to a persistent vertical dock), rendering,
expand/collapse state, cursor movement, and key routing; the plugin supplies
children and actions. No buffer puppeteering, no blocking prompts.

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
      nx.ui.input({ prompt = "New file: " }):next(function(name)   -- promise, not blocking
        if not name then return end
        nx.fs.write(join(dir_of(node), name), "", function() t:refresh(node) end)
      end)
    end,
    ["d"] = function(node, t)
      nx.ui.confirm("Delete " .. node.path .. "?"):next(function(yes)
        if yes then nx.fs.remove(node.path, function() t:refresh(t:parent(node)) end) end
      end)
    end,
  },
}
nx.keymap.set("n", "<leader>e", function() view:toggle() end)
nx.fs.watch(nx.cwd(), function() view:refresh() end)
```

### 6. Viewport decorations (the decoration-provider shape) — `nx.decor`

Some decorations are expensive *and* depend on what's on screen: rainbow
parens, indent guides, inline blame, semantic tokens on a huge file. neovim
serves these with a **decoration provider** — `on_win`/`on_line` callbacks the
renderer invokes per visible row, every frame. That is precisely the
re-enter-Lua-every-redraw model rule 4 forbids: a slow provider stalls the
frame, and the PUC 5.1 backend cannot host the per-row hot loop at all.

`nx.decor` keeps the *useful kernel* — only decorate what is visible;
recompute when the viewport moves — and drops the frame coupling. The engine
wakes the provider **once per visible-range change** (scroll, resize, edit
reflow), debounced off the frame path, hands it a snapshot of the visible
slice, and the provider **publishes** marks carrying a generation token; a
publish from a viewport the user already scrolled past is dropped. There is no
`on_line`, no per-frame call, and no single-frame "ephemeral" mark — a
published range stands until the next publish supersedes it or the viewport
invalidates its generation.

```lua
-- a rainbow-delimiters-shaped plugin — the whole thing
nx.decor.provider {
  name = "rainbow",
  bufs = { filetype = { "lua", "rust", "json" } },   -- engine skips non-matching windows

  -- Called off the frame, once per range change, never during redraw.
  on_range = function(ctx, publish)
    -- ctx is a snapshot, never live state:
    --   { win, buf, top, bot, lines, tick, gen }
    --   top/bot = 0-based inclusive visible rows; lines = exactly that slice
    local marks, depth = {}, 0
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      for col = 1, #line do
        local c = line:sub(col, col)
        if c:match("[%(%[{]") then
          marks[#marks+1] = { row, col-1, end_col = col, hl = RAINBOW[depth % 6 + 1] }
          depth = depth + 1
        elseif c:match("[%)%]}]") then
          depth = math.max(0, depth - 1)
          marks[#marks+1] = { row, col-1, end_col = col, hl = RAINBOW[depth % 6 + 1] }
        end
      end
    end
    publish(marks)          -- carries ctx.gen; engine folds it into the next frame,
  end,                      -- or drops it if the window already scrolled past `gen`
}
```

Marks are the **same shape as `nx.hl.set`** — decorations are one data type
whether static or viewport-driven:
`{ row, col, end_row?, end_col?, hl?, virt_text?, virt_lines?, sign?, conceal?, priority? }`.
Async is fine: an indent-guide or blame provider can `nx.run`/`nx.lsp`
inside `on_range` and call `publish` from the callback — the generation token
makes a late response safe to fold or safe to drop. A provider that errors is
reported loud (`E5108`) and disabled after repeated failures, matching the
"no silent stubs" convention and neovim's own `CB_MAX_ERROR`.

Decorations you already know — diagnostics from an LSP response, signs from a
diff — need no provider; they are a plain `nx.hl.set(ns, buf, marks)`. Reach
for `nx.decor` only when the work is worth scoping to the viewport.

## Plugin persistence — assigned, isolated namespaces (`nx.shada.plugin`)

A plugin that wants to remember something across sessions opts in:

```lua
local store = nx.shada.plugin()           -- no argument
store:set("recent", { "a.txt", "b.txt" }) -- any JSON-able Lua value
local recent = store:get("recent")        -- a fresh copy, or nil
store:delete("recent"); store:keys(); store:clear()
```

The data lives in the **current** shada store — global, a `--shada-namespace`
workspace, or a remote daemon, whichever this session uses — in a dedicated
table keyed apart from the core editor state, so a plugin's blob can never reach
the registers / marks / history (and persistence rides the ordinary debounced
checkpoint + clean-exit flush; with shada off it is in-memory only, like
registers).

The point is the *namespace*. It is **assigned, not chosen**: `nx.shada.plugin()`
takes no name and derives one from where the calling code lives — it walks the
stack to the caller's source file and attributes it to the runtimepath / plugin
directory that contains it. The namespace is then, in order: the canonical **name
the package manager registered** for that directory, when the plugin was loaded
through `nx.plugins` (tightest identity — a `name = …` spec can differ from the
install dir's basename); the reserved `user` for the config root; otherwise the
directory's **basename** (the fallback for a plugin loaded outside the manager,
e.g. a `pack/*/start/*` directory). So a plugin gets its own slice and *cannot
name* — and so cannot read or clobber — another plugin's. This is stronger than a
self-chosen string, where any code could claim any namespace. (A context that
attributes to no plugin — a bare `:lua` / RPC / test, or a deferred/async callback
whose stack no longer carries the plugin chunk — may pass an explicit namespace as
an escape hatch; a sourced file may pass one only if it **equals** its assigned
namespace (a harmless self-statement a framework can rely on when it resolves the
namespace once and threads it explicitly) — claiming a *different* one is a loud
error.) Worked end to end in `examples/plugin-shada/`.

Each namespace is capped at **1 MiB** of serialized key+value bytes, so one
plugin can't bloat the shared store and slow every launch's recency-merge. A
`set` that would cross the cap fails loud (no silent truncation; the prior value
is left intact), while a shrink is always allowed so a plugin can recover. It is
for small structured state — settings, a recent list — not bulk data.

The store is lifecycle-managed. `nx.shada.namespaces()` lists every namespace
currently stored (an audit of what plugins have stowed), and
`nx.shada.forget(name)` prunes one. `:PluginClean` uses them: when it removes an
uninstalled plugin's directory it also forgets that plugin's namespace, so the
data doesn't outlive the plugin.

It works everywhere the editor runs: the native redb store and the serverless web
build's OPFS blob round-trip plugin namespaces identically (verified by
`verify-shada.mjs` across a page reload).

## Treesitter highlighting is buffer state, not a verb

A small case that sharpens rule 5 ("registrations are data") into a working
rule of thumb: **prefer the noun.** neovim toggles treesitter highlighting
with `vim.treesitter.start(buf, lang)` / `stop(buf)` — *commands*. A command
leaves no readable state ("is TS on for this buffer?" has no answer you can
point at), isn't idempotent, and doesn't survive a session/shada round-trip.
`nx` models the same capability as **derived buffer state**, two declarative
nouns the engine reads:

| `nx.bo` state | Default | Decides |
| --- | --- | --- |
| `filetype` | from the path's extension | *which* language (`filetype` → lang) |
| `ts_highlight` | on when a language resolves | *whether* the native engine highlights |
| `commentstring` | the filetype's built-in template (`// %s`, `# %s`, …) | how the `gc`/`gcc` comment operator wraps lines |

Two nouns, not one, because "off" is orthogonal to "which": a giant `.rs`
buffer can keep `filetype = "rust"` (so LSP, indent, and comments still key off
it) with `ts_highlight = false`. Both are plain buffer options — set in
`init.lua`, in a `nx.on("filetype", …)` handler, or by a plugin — and both
write the per-buffer override the engine already derives its highlight language
from (`Editor::ts_override`). Nothing new in the engine: the writer moves from
a command to an option, and the state becomes introspectable and serializable.

The neovim verbs survive only as **aliases that desugar to these option
writes** (see the whitelist below): `vim.treesitter.start(buf, lang?)` sets
`filetype`/lang and `ts_highlight = true`; `vim.treesitter.stop(buf)` sets
`ts_highlight = false` and leaves `filetype` alone. They pass the alias
admission test precisely because there *is* a 1:1 declarative target to desugar
onto — the noun is what makes the alias admissible.

## The `vim.*` boundary

Per [ADR 0002](../decisions/0002-native-plugin-system.md) the break is clean:
**every editor API lives in `nx.*`**, config included. The only `vim.*` Lua is
a **closed whitelist of muscle-memory aliases** mapping 1:1 onto the `nx.*`
equivalents, so config can be written in familiar spellings. Colorschemes are
nxvim's own: a colorscheme is Lua that fills the highlight registry through the
`nx` highlight API (its `nvim_set_hl` alias), which is part of that whitelist —
not a separate surface.

The admission test for an alias: frequent in real
configs, declarative or callback-shaped (never blocking, never frame-time),
1:1 onto an `nx` primitive. The set (the canonical list lives in
[ADR 0002](../decisions/0002-native-plugin-system.md)): variables / options /
env (`vim.g`/`vim.b`/`vim.w`, `vim.o`/`vim.opt`/`vim.opt_local`/`vim.bo`/
`vim.wo`, `vim.env`); `vim.cmd` and `vim.keymap.set`/`del`; the pure helpers
(`vim.tbl_*`, `vim.split`, `vim.trim`, `vim.startswith`/`endswith`,
`vim.list_extend`, `vim.deepcopy`, `vim.inspect`, `vim.json`); a partial
`vim.api` of exactly the declarative registrations
(`nvim_create_autocmd`/`augroup`/`del`/`clear`, `nvim_create_user_command`,
`nvim_set_hl` — any other `vim.api` access fails loud) plus
`vim.filetype.add`; and the callback-shaped async (`vim.notify`,
`vim.schedule`, `vim.defer_fn`, `vim.ui.input`/`select`, and `vim.system` in
its callback form only — `:wait()` fails loud); and `vim.treesitter.start` /
`stop`, the one carve-out from the no-`vim.treesitter`-surface rule, desugaring
to the `filetype` / `ts_highlight` buffer-state writes above (*Treesitter
highlighting is buffer state*). Aliases, not an API: the same objects, `nx`
semantics, no growth beyond the list.

There is no `vim.treesitter` or `vim.lsp` *surface*: of that machinery, what
serves nxvim's objectives is refactored into `nx.treesitter` / `nx.lsp` (the
highlight toggle becomes buffer state, above), and the rest is deleted. The neovim runtime-model surfaces — wait-pumps, public uv
handles, frame-time decoration providers, the `vim.fn` long tail,
prompt-buffer emulation — exist on neither side of the API: plugins and config
get `nx.run` / `nx.timer` / `nx.fs` / `nx.ui.*` and the off-frame
`nx.decor` instead.

The native subsystems the surfaces above expose — LSP, treesitter, extmarks,
the pmenu, floats, the panel, the evloop, the settle contract — are the
engines this API is a thin contract over.

## Suggested build order

1. **`nx` core** (buf/win/options/event/spawn/fs/timer/keymap/command/ui.input
   + `nx.lsp` setup — contracts over existing machinery; `init.lua` targets it
   from day one) and the manifest loader + package manager. *(package manager
   landed — `nx.plugins`: declarative specs, async `git` install/update over
   `nx.run`, eager + lazy (`cmd`/`event`/`ft`/`keys`) loading, `:PluginSync` /
   `:PluginInstall` / `:PluginUpdate` / `:PluginClean` / `:PluginList`. A loaded
   plugin is put on the live runtimepath via the `nx._add_rtp` bridge — so its
   modules `require` and its `colors/`/`queries/`/`lsp/` resolve without a
   restart — then its `plugin/` scripts source and its `config` runs. `config`/
   `init` accept a plain or async function. A built-in **first-run** flow
   (`nx.plugins.recommend{…}` + the `VimEnter` autocmd) offers a curated set on a
   fresh setup and, on accept, writes it to a managed `lua/plugins.lua` the user's
   `init.lua` requires. See `crates/nxvim-lua/src/prelude/plugins.lua` and
   `examples/plugins/`.)*
2. **Picker** — highest daily-driver value; exercises spawn / streaming /
   cancellation / floats / preview end to end.
3. **Completion engine** — LSP + buffer + snippets sources built-in.
4. **Statusline segments** *(landed — `nx.statusline`, the lualine-shaped
   registry: built-ins resolved natively, custom segments re-rendered on declared
   events / `invalidate`; see
   [the plan](../plans/2026-06-15-nx-statusline-segments.md))*, **snippet engine**
   *(landed, shared with 3)*, **tree docks**.
5. **`nx.decor`** — the decoration-provider drive already exists; the new
   piece is the debounced viewport-changed signal off the scroll/resize path
   (not `redraw`) and the generation-keyed publish into the extmark layer.
   Lower daily-driver priority (rainbow / indent guides / inline blame are
   polish), and it shares the off-frame event-keyed-publish mechanism with the
   statusline (4), so it slots in naturally after it.
6. RPC twins of the registries (out-of-process providers) — later.
