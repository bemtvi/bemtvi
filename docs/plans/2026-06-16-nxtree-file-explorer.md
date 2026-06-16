# nxtree — a dockable, extensible file explorer (pure-Lua plugin)

Status: **native phases 1–2 landed** — 2026-06-16. `nx.open` / `nx.layer` (Phase 1)
and `nx.view` (Phase 2) are implemented and tested (`crates/nxvim-server/tests/nx_view.rs`,
`examples/nxview/`); together with the landed `nx.fs` dependency they unblock the
Lua plugin (phases 3–5, still planned). Phase 2 ships dock + split mounts; a `float`
mount fails loud as not-yet-implemented.

A real file tree, built as a **pure-Lua plugin on `nx.*`** per the dogfooding
directive ([ADR 0002](../decisions/0002-native-plugin-system.md)). A small set of
principled native additions are required first; everything else — the tree model,
lazy expand, icons, file actions, search, and the extensibility surface — lives in
Lua. Target use: a permanent **left dock**, where `<CR>` opens the file in the
**main** editing layer, not the dock.

> **Filesystem API (`nx.fs`) is designed in a separate doc:**
> [nx.fs — a promise-always Lua filesystem API](2026-06-16-nx-fs-api.md). The tree
> needs a clean Lua filesystem surface (directory listing *with entry kind*, stat,
> mkdir/rename/remove/copy, and a change-watch) — the full impl already exists in
> Rust (`LuaFs`) but is only partially surfaced to Lua. That API is a **dependency**
> of this plan. Phases below assume its v1 surface: `nx.fs.readdir(path)` →
> `{{name,type}}` (promise), `stat`/`mkdir`/`rename`/`remove`/`copy` (promises), and
> `nx.fs.watch(path,{recursive})` (async-iterator) for auto-refresh.

## Problem

There is today no way to build this as a Lua plugin, for two concrete reasons:

1. **No plugin-controllable content surface that can live in a dock.** Direct
   buffer-text mutation (`nvim_buf_set_lines`, `nvim_create_buf`) is *deliberately
   absent* from the Lua API (`crates/nxvim-lua/src/prelude/api.lua:9`; ADR 0002 —
   the config API is autocmds/keymaps/options, not entity mutation). The *only*
   plugin-owned-lines surface is the bottom **panel** (`vim.panel.set_lines` +
   `on_select` + per-line `set_panel_targets`, `crates/nxvim-core/src/editor/panel.rs`)
   — a single bottom-edge list, not a tree, not mountable in a side dock.

2. **Opening a file from a dock keymap lands it *in the dock*.** `Editor::jump_to`
   / `:edit` open into the **current** window (`crates/nxvim-core/src/editor/buffers.rs:1109`,
   `edit_in_current_window`); there is no `ensure_main_layer` on that path. The
   panel only escapes this because it is an overlay on the Main layer (the focused
   layer stays `Main`). A real dock *is* the focused layer (`Layer::Dock(side)`,
   `crates/nxvim-core/src/editor/dock.rs:217,260`), so a naive `:edit` opens the
   file inside the sidebar.

3. **Filesystem listing is name-only and N-stat-heavy from Lua.** The full
   libuv-shaped fs operation set exists in Rust (`LuaFs`/`StdLuaFs`,
   `crates/nxvim-lua/src/luafs.rs`: `scandir` *with kind*, `stat`, `mkdir`,
   `rename`, `unlink`, `rmdir`, `copyfile`, `realpath`, …) and is already
   daemon/wasm-routed via the host seam — but only a scattered subset is surfaced
   to Lua through `vim.fn.*` (`isdirectory`, `getftime`, `mkdir`, `glob`,
   `filereadable`) plus the sync `nx._readdir` (entry **names only**, no kind). A
   tree listing a directory must therefore `nx._readdir` then `isdirectory` every
   entry — one stat round-trip per file, brutal over the daemon wire. **The Lua
   filesystem surface that fixes this (`nx.fs`) is sketched in a separate doc and
   is a dependency of this plan — see the note at the top.**

## What already exists (reuse)

- **Docks** — `nx.dock.open{side,size,buf}`, `nx.dock.opt(side)` (title /
  showtabline / size / autohide), `toggle/hide/show/focus/close`
  (`crates/nxvim-lua/src/install.rs:303`, `crates/nxvim-core/src/editor/dock.rs`).
  Permanent, global across tabs, splittable. A dock with no `buf` gets a fresh
  scratch buffer; we will instead mount an `nx.view` buffer into it.
- **The panel** — the generalization seed. `open_panel` / `set_panel_lines` /
  `set_panel_on_select` / `set_panel_targets` / `apply_panel_action` /
  `panel_view` (`panel.rs`) already implement *exactly* "a read-only,
  plugin-owned, line-controlled buffer with a `<CR>` handler and per-line jump
  targets routed through `jump_to`". `nx.view` (Phase 3) lifts these off the
  bottom-edge assumption.
- **Layer machine** — `switch_layer` / `ensure_main_layer` / `focus_dock` already
  exist as `pub(crate)` (`dock.rs:217,260,338`). Phase 2 only *exposes* them.
- **Extmarks** — `nx.hl.set(ns, buf, marks)` / `nvim_buf_set_extmark`: hl ranges,
  inline `virt_text`, `sign_text`, `priority`. Work on **any** buffer across
  **any** layer (`api.lua:339`, `api.lua:976`). This is the rendering layer for
  indent / icons / git signs — no new API needed.
- **`LuaFs` trait** — the full fs impl already exists; surfacing it to Lua as
  `nx.fs` is the separately-sketched dependency, not part of these native phases.
- **UI prompts** — `nx.ui.input` / `nx.ui.confirm` / `nx.ui.select` (promise
  form) for the file actions (`crates/nxvim-lua/src/prelude/ui.lua`).
- **Async** — `nx.run` / `nx.run_stream` + promises for the git-signs add-on and
  any shell-backed work (`crates/nxvim-lua/src/prelude/process.lua`).
- **Buffer-local keymaps / autocmds** — `nx.keymap.set(mode,lhs,rhs,{buffer=})`,
  `nx.on(event,…)`.

## Native additions

> **Dependency, sketched separately:** `nx.fs` (the Lua filesystem surface —
> `scandir` with kind, `stat`, `mkdir`, `rename`, `remove`, `copy`). It is the
> prerequisite for the model/actions phases but its design lives in its own doc;
> not numbered as a phase here.

### Phase 1 — `nx.open` / `nx.layer` (expose the Main layer)

Expose the existing `pub(crate)` layer switches. No new core logic.

```lua
nx.open(path, { where = "main" })  -- ensure_main_layer() then edit-in-current-window
nx.layer.main()                    -- focus the Main layer  (wraps ensure_main_layer)
nx.layer.focus("main"|"left"|"right"|"top"|"bottom")  -- wraps switch_layer / focus_dock_named
```

`where` defaults to `"current"` (today's behavior) so nothing regresses; the tree
passes `"main"`. Implemented as an effect/op queued to the core like the other
`nx.dock.*` ops.

Tests: open a dock, focus it, `nx.open(file,{where="main"})` from within → assert
the file's buffer is shown in a **Main**-layer window and the dock still shows its
own buffer; `nx.layer.focus` round-trips focus between dock and main.

### Phase 2 — `nx.view` (generalize the panel into a mountable surface)

The key new primitive: a **read-only, plugin-owned, non-file content surface** —
the same category as the explorer's `dir` buffers and the panel, *not* a new
buffer-mutation API (worth a one-line ADR 0002 amendment to record the widened
surface). Mechanically: the panel, minus the bottom-edge assumption, plus a mount
target and per-line userdata.

```lua
local v = nx.view.create{ name = "nx-tree", filetype = "nxtree" }
v:set_lines(lines)                  -- replace content (generalizes set_panel_lines)
v:set_userdata(list)                -- parallel-to-lines opaque data (node per line)
v:set_decor(ns, marks)              -- extmark batch — same mark shape as nx.hl.set
v:on_select(function(line_idx, ud) … end)   -- <CR> / mouse-confirm
v:mount{ dock = "left", size = 30 } -- or { split = "vsplit" } / { float = {…} }
v:line(); v:cursor(); v:focus(); v:redraw(); v:unmount()
```

The backing buffer is read-only to the editing grammar (reuse the explorer's
inert-editing routing keyed off a buffer flag, e.g. a `view: Some(view_id)` marker
analogous to `dir: Some(path)` in `crates/nxvim-core/src/buffer.rs`). Navigation
keys (`j`/`k`/`/`/`:`) work; text-mutating keys are inert; `<CR>`/click dispatch to
`on_select` via the keymap engine (mirroring `apply_panel_action`). Mounting in a
dock = create the view buffer, then `open_dock(side, size, Some(view_buf))`.

Tests: create + `set_lines` + assert `nvim_buf_get_lines`; mount in left dock +
assert it renders there; `set_decor` + assert extmarks via
`nvim_buf_get_extmarks`; `on_select` fires with the right line + userdata; cursor
nav stays within the view; an `i`/`dd` is inert (content unchanged).

## The Lua plugin (Phases 3–5)

Layout under `examples/nxtree/` (runnable config) backed by a `nxtree/` Lua module
(`lua/nxtree/*.lua`):

```
init.lua       manifest + lazy activation (:NxTree, <leader>e); setup{}
model.lua      node tree, lazy scandir-on-expand, flatten-to-visible-lines
render.lua     lines + extmark decor (indent guides, icons, decorator merge)
actions.lua    open / add / rename / delete / move(cut+paste) / yank-path / refresh
search.lua     "/" live filter over the flattened view
icons.lua      extension→{glyph,hl} registry (seeded + register())
api.lua        register_decorator / register_action / register_icons (extensibility)
```

**Node**: `{ path, name, type, depth, expanded, loaded, children, parent }`. The
dock shows a flattened list of visible nodes; `userdata[i]` is the node for view
line `i`, so `on_select` gets the node directly.

### Phase 3 — model + render + open-in-main (MVP)

- `expand(node)`: lazy `nx.fs.scandir` (one call), sort dirs-first/alpha, build
  child nodes, `loaded=true`, rerender. `collapse` just flips `expanded`.
- `flatten` → `view:set_lines` + `view:set_userdata`; `render` emits extmarks:
  inline icon glyph at `depth*2`, full-line hl group (`NxTreeDir`/`NxTreeFile`).
- `on_select`: dir → toggle expand/collapse; file → `nx.open(path,{where="main"})`.
- `<leader>e` / `:NxTree` toggles the left dock (`nx.dock.toggle("left")`), opening
  + mounting the view on first use, focusing **back to main** after the initial
  mount so the cursor doesn't start in the sidebar.

### Phase 4 — actions, icons, search, extensibility

- **Actions** (buffer-local maps on the view): `a` add (input → `nx.fs.mkdir` for
  trailing `/`, else create empty file), `r` rename (`nx.fs.rename`), `d` delete
  (`nx.ui.confirm` → `nx.fs.remove{recursive}`), `x`+`p` move (cut-path then paste
  via rename), `y` yank absolute path to a register, `R` refresh (re-scandir
  expanded dirs). Each rescans the affected parent and rerenders.
- **Icons**: `icons.lua` seeds common extensions (`.rs .ts .js .lua .md .json .go
  .py .toml .sh …`) + dir-open/closed glyphs; `register(map)` extends it.
- **Search**: `/` enters a live-filter mode — fuzzy-match against the flattened
  node text, narrow `set_lines` to matches (keeping ancestors), `<CR>` opens, `Esc`
  restores. (Picker-delegation is an alternative but in-dock filtering keeps focus
  and tree context.)
- **Extensibility** — three pure-Lua registries:
  - `register_decorator(fn)` — `fn(node) → {sign=, sign_hl=, hl=, virt_text=}`,
    merged per visible line each render.
  - `register_icons(map)` / `register_action(key, fn)`.
- **Ship the example**: `examples/nxtree/` config + a `git-signs` add-on
  (separate, zero-coupling: `nx.run "git status --porcelain"` → `path→status`
  cache → `register_decorator` returning a sign/hl → `nx.on("BufWritePost")`
  invalidate + `view:redraw()`), verified end-to-end in the TUI (the
  agent-verifiable client).

### Phase 5 — polish (optional)

`nx.fs.watch` for auto-refresh (a dir-watch op for the separate `nx.fs` design to
cover); symlink + hidden
toggles; root-cd / `:NxTreeFindFile` reveal-current-buffer; GUI/web paint parity
note if the view surface needs client work beyond what extmarks already cover.

## Out of scope

- A native tree widget (rejected: pushes model/render into Rust, less Lua-
  extensible — see the 2026-06-16 design decision to generalize the panel instead).
- Reusing the transient `dir` explorer buffers (rejected: single-dir netrw-style
  picker, not a persistent expandable docked tree).
- Drag-and-drop reordering, trash/undo of deletes, multi-select operations
  (later, if wanted).
