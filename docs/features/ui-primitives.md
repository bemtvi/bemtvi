# UI primitives

nxvim gives plugins a small, layered set of primitives for building rich
interfaces — a **reactive component model**, plugin-owned **content surfaces**,
ready-made async **widgets**, and the **floating windows** underneath them. They
share two properties that make plugin UIs short to write and consistent to use:

- **The server owns every surface.** You never write a render loop, an input
  grab, or frame-timing code — you hand the server data and a description of what
  to show, and react to results. (PUC Lua can't do frame-time work anyway; ADR
  0002.)
- **One geometry vocabulary.** Floats, views, pickers, and the bottom panel all
  place themselves with the same size / align / margin words (see
  [Placing things](#placing-things)).

A file tree, a dashboard, a modal dialog, a which-key popup, a fuzzy finder — each
is a few lines on top of these. Reach for the highest-level one that fits:

| You want to… | Reach for | What it is |
| --- | --- | --- |
| A stateful UI that redraws itself (file tree, dashboard, dialog) | **`nx.view.component`** / **`nx.component`** | A Vue-shaped reactive component: state + a pure render. |
| Plugin-owned content you update by hand (a list, a report) | **`nx.view`** | An inert buffer you set lines on and mount in a dock / split / float. |
| To ask the user something (text, choice, yes/no) | **`nx.ui.input` / `select` / `confirm`** | Async prompt widgets — return a promise. |
| To pop up read-only content (a hint, a tooltip) | **`nx.ui.float`** | A bordered content overlay, dismissed by the next key. |
| A real, editable window placed over the layout | **`nvim_open_win`** (float form, RPC) | The low-level escape hatch — a true floating window (see [Floating windows directly](#floating-windows-directly)). |

## Components — reactive UIs

`nx.component` is a **Vue-shaped** component model: you write reactive **state** and
a **pure render**, and the framework re-runs the render automatically whenever the
state changes (coalesced to one redraw per tick). It owns the whole lifecycle —
waiting for the surface to be ready, batching state writes, tearing down on close —
so there's no tick-dance, no manual re-render, no buffer-number juggling.

A component is a `{ setup, render }` table:

- **`setup(ctx, props)`** runs **once** on mount and owns every side effect —
  reactive state, derived state, event subscriptions, key binds, data fetches — and
  returns the `state` value handed to render. It runs only *after* the surface is
  ready, so everything on `ctx` is valid immediately.
- **`render(state)`** is **pure**: it maps state to what's on screen and returns
  it. The framework re-runs it on every state change.

Both may be **async** — call `nx.await(...)` straight inside them (each runs in its
own `nx.async` coroutine), so a `setup` that loads from disk, or a `render` that
fetches to display, reads top-to-bottom.

```lua
local Counter = nx.view.component({
  setup = function(ctx)
    local s = ctx.reactive({ n = 0 })            -- writing s.n re-renders
    ctx.keymap_set("n", "+", function() s.n = s.n + 1 end)
    ctx.keymap_set("n", "q", ctx.close)
    return s
  end,
  render = function(s)
    return { lines = { "count: " .. s.n, "", "+ to increment · q to quit" } }
  end,
})

Counter.mount({ float = { width = 30, height = 4, grab = true } })
```

`ctx` carries the reactivity and lifecycle:

| `ctx` field | Purpose |
| --- | --- |
| `ctx.reactive(tbl)` | A deep reactive proxy — writing any key schedules a re-render. (Iterate with `ipairs` / `#`, not `pairs` — PUC 5.4 has no `__pairs`.) |
| `ctx.computed(getter)` | A cached derived value (read `c()` / `c.value`); re-evaluates only when a reactive input it read has changed. |
| `ctx.refresh()` | Force a re-render. |
| `ctx.props` | The `opts.props` passed to `mount`. |
| `ctx.on_close(fn)` / `ctx.close()` | Register a teardown hook / close the instance. |

### Two surfaces, one reactive core

*Where* a component renders is a pluggable backend, so the same reactive core drives
two very different surfaces:

- **`"view"`** (the default, and what `nx.view.component` selects) — a
  **focus-taking, navigable** [`nx.view`](#views--the-content-surface) buffer, mounted in a dock, split,
  or grabbing float. `render` returns `{ lines, decor }`. This is the file-tree /
  list / modal-dialog case; `ctx` gains `keymap_set` / `line` / `set_cursor` /
  `bufnr` / `winid` / `bo` / `wo`.
- **`"float"`** — a **non-focus** [popup-content](#popup-content) float (the
  which-key surface). It never steals focus and binds no keys; `render` returns
  `{ lines, title?, relative?, border? }` (rows may be styled chunks), and an
  **empty render hides** the float — so a component shows and hides purely by what
  it returns. Reach it with `nx.component{ surface = "float", … }`.

```lua
-- A self-dismissing toast on the non-focus float surface.
local Toast = nx.component({
  surface = "float",
  setup = function(ctx)
    local s = ctx.reactive({ text = ctx.props.text })
    nx.timer(function() s.text = nil end, 1500)   -- clear -> empty render -> hides
    return s
  end,
  render = function(s)
    if not s.text then return { lines = {} } end  -- an empty render hides the float
    return { lines = { { { s.text, "Comment" } } }, relative = "bottom" }
  end,
})
Toast.mount({ props = { text = "saved ✓" } })
```

`mount(opts)` instantiates the component (returning the instance, with `:close()`);
`opts.props` is passed to `setup`, and the rest configures the surface. Render and
setup errors are caught and surfaced through `nx.notify` rather than crashing the
editor.

## Views — the content surface

`nx.view` is the surface components are built on, and is useful on its own. A view
is a **plugin-owned, read-only buffer**: its lines a plugin sets wholesale, the
editing grammar treats it as inert (navigation works, text-mutating keys don't),
and `<CR>` dispatches to an `on_select` callback. It's the generalization of the
bottom panel — the surface a file tree, a symbol list, or any line-oriented widget
mounts in a dock or a split.

```lua
local v = nx.view.create({ name = "files", filetype = "nxfiles" })
v:set_lines({ "  init.lua", "  README.md" })
v:set_userdata({ { path = "init.lua" }, { path = "README.md" } })  -- parallel to lines
v:on_select(function(line, data) nx.open(data.path, { where = "main" }) end)
v:mount({ dock = "left", size = 30 })
-- later: v:unmount()  (keeps it alive)  /  v:close()  (drops it)
```

| Method | Does |
| --- | --- |
| `:set_lines(lines)` | Replace the content wholesale. |
| `:set_userdata(list)` | Opaque per-line data (1-based, parallel to the lines); the selected line's entry is handed to `on_select`. |
| `:on_select(fn)` | `fn(line, userdata)` on `<CR>` / confirm. |
| `:set_decor(ns, marks)` | Replace namespace `ns`'s extmark decoration — each mark `{ line, col, <extmark opts> }` (0-based), so `hl_group` / `virt_text` / `sign_text` / … all apply. |
| `:mount(opts)` | Show it — `{ dock = … }` / `{ split = "vsplit"\|"split" }` / `{ float = … }` / `{ tab = true }`. |
| `:bufnr()` / `:winid()` | The backing buffer / showing window (live, from the mirror). |
| `:line()` / `:set_cursor(n)` | Read / move the 1-based cursor line. |
| `:place_in(win)` | Adopt a reserved restore slot (used by `nx.view.on_restore`, below). |

### Persisting a view across sessions

A view opts into the workspace session by passing `persist` — a stable, plugin-chosen
string id (instance-unique within your plugin) — to `create`. The editor records only the
`(namespace, id)` pair and the view's slot in the layout, **never its content**: the plugin
owns what's worth saving and stores it in its own
[`nx.shada.plugin()`](../plans/2026-06-26-plugin-shada-namespaces.md) store, keyed
by the same id.

On restart the editor reopens the layout with each persisted view's slot held by an empty
placeholder window, then — once your plugin has loaded — calls the restorer you registered
with `nx.view.on_restore` so you can rebuild the view and drop it into the reserved slot:

```lua
-- Save side: create with a persist id, and stash whatever you need to rebuild it.
local function open_tree(state)
  local v = nx.view.create({ name = "Files", filetype = "nxfiles", persist = "main" })
  v:set_lines(render(state))
  v:on_select(...)
  -- The plugin owns the content; persist just enough to rebuild it.
  nx.shada.plugin():set("view:main", state)
  -- Clean up the stored state if the user closes the view for good.
  v:on_close(function() nx.shada.plugin():delete("view:main") end)
  v:mount({ dock = "left", size = 30 })
  return v
end

-- Restore side: register once at load. `id` is the persist string; `place(view)` drops a
-- freshly-built view into the reserved slot instead of opening a new window.
nx.view.on_restore(function(id, place)
  local state = nx.shada.plugin():get("view:" .. id)
  local v = nx.view.create({ name = "Files", filetype = "nxfiles", persist = id })
  v:set_lines(render(state))
  v:on_select(...)
  place(v)
end)
```

The owning namespace is derived from your plugin's location, exactly like
`nx.shada.plugin()` — so two plugins can both use `persist = "main"` without colliding, and
a persisted view whose plugin is no longer installed has its slot quietly collapsed on
restore. From a context that attributes to no plugin (a bare `:lua`, an RPC, a test), pass
an explicit `namespace = "…"` to both `create` and `on_restore`, the same escape hatch
`nx.shada.plugin(namespace)` takes. **GC:** the editor never deletes your stored state —
delete it yourself when the view is closed for good (the `on_close` line above). A view
created without `persist` is ephemeral: it does not ride the session. Session persistence is
a native-build feature (the web build does not restore layouts yet).

**The high-level way: a persistent component.** The `create` + `on_restore` + fresh-mount
dance above is what `nx.view.component` (see [Components](#components--reactive-uis) above)
automates. Pass `persist = "<id>"` to
`mount` and the framework resolves the namespace once, threads it through the backing view +
a per-component `ctx.store`, and on restart adopts the reserved slot or mounts fresh for you
— no `on_restore` handler, no `VimEnter` fallback:

```lua
local Files = nx.view.component({
  setup = function(ctx)
    local s = ctx.reactive({ tree = ctx.store:get("tree") or load_tree() })
    ctx.on_close(function() ctx.store:delete("tree") end)  -- GC is still yours
    return s
  end,
  render = function(s) return { lines = render_tree(s.tree) } end,
})
Files.mount({ name = "Files", filetype = "nxfiles", persist = "main", dock = "left", size = 30 })
```

`ctx.store` is this component's `nx.shada.plugin()` slice; mutate it on every change and the
sidebar comes back intact. A full runnable example is `examples/view-persist/`. Reach for the
raw `create` / `on_restore` pair only when you need a surface the component doesn't model.

## Widgets — ready-made prompts

The `nx.ui` widgets are the common interactions, prebuilt. The three input ones are
**non-blocking** and **promise-only** (ADR 0002): the call returns at once and you
react with `:next(fn)`, or `await` it inside `nx.async`. (The `vim.ui.*`
muscle-memory aliases keep neovim's callback shape.)

```lua
-- input: a one-line prompt. Resolves to the text, or nil on <Esc>.
nx.ui.input({ prompt = "Rename to: ", default = vim.fn.expand("%:t") })
  :next(function(name) if name and name ~= "" then nx.notify(name) end end)

-- select: a floating chooser. Resolves to the chosen item, or nil on cancel.
nx.ui.select({ "apple", "banana", "cherry" }, { prompt = "Pick:" })
  :next(function(item) if item then nx.notify("picked " .. item) end end)

-- confirm: a yes/no dialog. Resolves to a boolean.
nx.ui.confirm("Quit without saving?", { default = false })
  :next(function(ok) if ok then nx.cmd("qa!") end end)
```

- **`nx.ui.input(opts)`** — `prompt` label + `default` prefill, over the command
  line. `""` on an empty `<CR>`, `nil` on `<Esc>`.
- **`nx.ui.select(items, opts)`** — a floating list. `format_item` maps an item to
  its label (default `tostring`); the original item round-trips back, so an
  arbitrary table survives even though only strings cross the bridge. Its keys are
  rebindable `select`-mode maps (`j`/`k`/`<C-n>`/`<C-p>`/arrows nav, `gg`/`G`
  first/last, `<CR>` confirm, `<Esc>`/`q` cancel):

  ```lua
  nx.keymap.set("select", "<C-j>", nx.ui.select_actions.next)   -- actions: next,
  nx.keymap.set("select", "q", function() end)                  -- prev, first, last,
  ```                                                            -- confirm, cancel

- **`nx.ui.confirm(message, opts)`** — yes/no. `default = true` (the default) makes
  `<CR>` Yes and shows `[Y/n]`; `false` makes it decline and shows `[y/N]`. For a
  multi-choice menu use `select`.
- **`nx.ui.open(uri)`** — hand a path/URL to the OS opener (`open` / `explorer` /
  `xdg-open`), off-tick; resolves to the run result `{ code, stdout, stderr }`
  (like `nx.run`, it *resolves* rather than rejects — a missing opener is
  `code = -1`).

### Popup content

`nx.ui.float(contents, opts)` is the **popup-content** surface — a box of read-only
content (a rounded border by default; `border = "none"` for borderless) with **no
list, no selection, no input grab**. By default it is transient: the *next key*
dismisses it.

```lua
nx.ui.float("A hint.\nPress any key to dismiss.", { title = " info ", relative = "cursor" })
```

`contents` is a string (split on newlines), a list of line strings, or — for a
**styled** popup — a list where a row is a *chunk list* `{ {text, hl_group?}, … }`
(neovim's `virt_text` shape), so a row can colour its key one group and its
description another. With `persist = true` it survives keystrokes and returns a
**handle** (`:update(contents, opts)` / `:close()` / `:is_open()`) — the surface a
key-observer plugin like which-key refreshes as keys arrive. Options: `border`
(`"none"`/`"single"`/`"rounded"`/`"double"`/`"solid"`, default `"rounded"`),
`title`, `relative` (`"cursor"` / `"editor"` / `"bottom"`).

## The float surfaces

When something floats, it's one of four distinct surfaces — worth knowing which,
because they behave differently. The two axes: *is it a real window?* (a real
window holds a buffer, so it can scroll and be focused) and *does it carry a
selectable list?*

| Surface | What it is | Real window? | Grabs input? | Scrolls? |
| --- | --- | --- | --- | --- |
| **Floating window** | A free, editable window over the layout — you manage its lifecycle | ✅ | optional (`grab`) | ✅ |
| **Popup content** | A lightweight display overlay (`nx.ui.float`) — *not* a window | ❌ | never | ❌ |
| **Popup window** | A transient, read-only *real* window over a scratch buffer — auto-dismissed but scrollable | ✅ | never | ✅ |
| **List widget** | The floating selectable list — picker, `nx.ui.select`, completion | server widget | ✅ | ✅ (preview) |

The naming is a two-axis system: **"popup"** marks the transient, read-only family
(*popup content*, *popup window*); **"window"** marks a real backing window
(*floating window*, *popup window*). The **popup window** sits in the intersection —
it auto-dismisses like popup content, but because it's a real window it **scrolls**
(a long hover scrolls past its box) and gets syntax highlighting for free. That's
why LSP hover and signature help use it, not popup content; it has no direct Lua
constructor — `nx.lsp.hover` / `signature_help` open it internally.

## Placing things

Every windowed surface — floating windows, `nx.view`, components, pickers, the
bottom panel — shares one placement vocabulary, so you learn it once:

| Field | Meaning |
| --- | --- |
| `width` / `height` | **Cells** (`40`) or a **viewport fraction** string — `"50vw"` (50% of editor width), `"30vh"`, `"50%"`. A fraction re-resolves on every layout, so it reflows when the terminal resizes. |
| `align` | A 9-grid word — `"top-left"`, `"top"`, `"top-right"`, `"left"`, `"center"`, `"right"`, `"bottom-left"`, `"bottom"`, `"bottom-right"`. |
| `margin` | Inset from the aligned corner. A number (the vertical gap; sides get ~2× to look even), or `{vertical, horizontal}` / `{top, right, bottom, left}`. |
| `relative` | `"editor"`, `"cursor"` (anchored at the cursor, flipping for room), or `"win"`. |
| `border` | `"none"` / `"single"` / `"rounded"` / `"double"` / `"solid"` (plus `"shadow"` for `nx.view` floats). |
| `title` | A string drawn on the top border. |
| `grab` | `true` (default for a view/component float) locks focus to it — the modal-dialog shape; `false` is a non-modal panel focus can leave. |

For low-level parity there's also `anchor` (`"NW"`/`"NE"`/`"SW"`/`"SE"`) + `row`/`col`
offsets, the explicit form `align` + `margin` sugar over.

## Floating windows directly

When you need a **real, editable window** over the layout — not a view or a widget —
the float form of `nvim_open_win` is the low-level primitive. It lives on the **RPC**
surface (the `nvim_*` methods a client, test, or remote tool drives), not in config
Lua: nxvim's Lua API deliberately omits the mutating `vim.api.nvim_*` entity surface
(ADR 0002), so a plugin opens its float through `nx.view:mount{ float = … }` or a
component instead — the same window with the lifecycle managed for you (pass
`grab = false` for a non-modal panel focus can leave).

```lua
-- over RPC (a client or test):
-- win = nvim_open_win(buf, true, { relative = "editor", width = 40, height = 3,
--                                  align = "center", border = "rounded", title = " scratch " })
-- nvim_win_set_config(win, { height = 6 })   -- move / resize live
-- nvim_win_close(win, true)                  -- dismiss
```

From Lua the float *reads* are first-class: `nvim_win_get_config(win)` (alias of
`nx.win.config`) reads a window's float config back (`{ relative = "" }` for a tiled
window), and `nx.win.gettype(win)` returns `"popup"` for a float. A float is a real
window — it holds an editable buffer, splits are disallowed, and `:q` / focus /
`:only` treat it as an overlay rather than part of the tiled tree.

## Try it

Runnable playgrounds ship under `examples/`:

| Example | Shows |
| --- | --- |
| [`nxchecklist`](https://github.com/davidrios/nxvim/tree/main/examples/nxchecklist) | A modal checkbox dialog written with `nx.view.component` (reactive state + pure render). |
| [`nxview`](https://github.com/davidrios/nxvim/tree/main/examples/nxview) | A dockable `nx.view` content surface with `<CR>` → open-in-main. |
| [`which-key`](https://github.com/davidrios/nxvim/tree/main/examples/which-key) | A real which-key as a `surface = "float"` component over the pending-key oracle. |
| [`ui-prompt`](https://github.com/davidrios/nxvim/tree/main/examples/ui-prompt) | `nx.ui.input` and `nx.ui.confirm` prompts. |
| [`ui-select`](https://github.com/davidrios/nxvim/tree/main/examples/ui-select) | The floating chooser, including items that carry data. |
| [`ui-float`](https://github.com/davidrios/nxvim/tree/main/examples/ui-float) | Popup content (`\f` / `\F`), and LSP hover (`K`) through the popup window. |
| [`window-geometry`](https://github.com/davidrios/nxvim/tree/main/examples/window-geometry) | The shared size / align / margin vocabulary across every surface. |

```sh
NXVIM_CONFIG=examples/nxchecklist cargo run -p nxvim
```

## How it works (in brief)

A floating window is a real window carried by the same `WindowOp` queue and `View`
projection as tiled windows — `redraw()` projects each with its rect, `floating`
flag, `border`, and `zindex`, and every client paints the tiled windows first, then
the floats in z-order. The two placement modes map onto existing
`FloatRelative::{Cursor, Editor}` variants with no new positioning code.

The widgets sit on **one server-side float-list component** — completion, the
picker, and `nx.ui.select` are thin engines over the same widget, differing only in
whether they carry a prompt and a preview; for each, only a display label and an
integer key cross the Lua↔Rust bridge. The **component model** is pure Lua: a Vue-3
reactivity core (dependency-tracked `reactive` / `computed`) over the `nx.view`
(and popup-content) surfaces, with the render coalesced to one redraw per tick and
async renders generation-gated so a slow one can't clobber a newer one.

For the full design, see the
[floating-windows plan](../plans/2026-06-06-floating-windows.md), the
[float-list widget spec](../specs/2026-06-14-nx-ui-float-widget.md), and the
[native plugin API spec](../specs/2026-06-11-native-plugin-api.md).
