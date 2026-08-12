# Fuzzy picker

`btv.picker` is bemtvi's native fuzzy finder — a centered float with a prompt that
grabs every key, a Rust fuzzy matcher that re-ranks as you type, and an optional
preview pane. The **server** owns the widget: the prompt, the matcher,
navigation, and a generation token that drops a stale response for a query you've
already typed past. No input loop runs in Lua — a source is just a thin driver
that *streams candidates in* and *handles confirm*.

It ships with a set of built-in sources — `files`, `live_grep`, `buffers`,
`curbuf`, `diagnostics`, `keymaps`, `marks`, `jumplist`, `pickers` — and
registering your own is a few lines.

`files` and `live_grep` search **unrestricted** by default — the equivalent of
`rg -uu` (`--no-ignore --hidden`), so a `.gitignore`d build artifact or a dotfile
like `.github/workflows/ci.yml` is still findable. The one exclusion is `.git`
itself. Narrowing a search is the [filter boxes](#include--exclude-filters)' job
(`<C-g>`) — per-search, so hiding `target/` this time never makes a file
unfindable the next.

## Using a picker

The built-in sources are bound out of the box, plus a **resume** map:

| Map | Source |
| --- | --- |
| `<leader>ff` | `files` — fuzzy file finder |
| `<leader>fg` | `live_grep` — live grep |
| `<leader>fb` | `buffers` — open buffers (scoped to the focused layer, like `:ls`) |
| `<leader>f/` | `curbuf` — fuzzy find in the current buffer |
| `<leader>fd` | `diagnostics` — diagnostics |
| `<leader>fk` | `keymaps` — keymaps |
| `<leader>fm` | `marks` — marks |
| `<leader>fj` | `jumplist` — the jumplist |
| `<leader>fi` | `pickers` — the registered pickers themselves |
| `<leader>fr` | `resume` — reopen the last picker where you left off |

These are overridable defaults — your own map for the same key wins, and you can
disable one by binding it to an empty function. To open any registered source
from your own keymap, call `btv.picker.open`:

```lua
btv.keymap.set("n", "<leader>o", function() btv.picker.open("files") end)
```

## Resume — `<leader>fr`

`<leader>fr` (telescope's `resume`) reopens the most-recently-closed picker
restored to **exactly** where you left off — the same displayed rows, prompt
text, highlighted row, and multi-select marks. The server replays a frozen
snapshot it captured at close, so a `live_grep` picker comes back with its
*actual* previous results rather than a fresh, differently-ordered search;
editing the query from there re-runs the source as usual. It's a no-op (with a
gentle notice) before any picker has closed. Call it from your own map with
`btv.picker.resume()`.

Transient internal pickers (the command-line completion overlay, for instance)
opt out by setting `resumable = false` on their source, so resume always points
at the last *real* picker.

In the open picker (all of these are rebindable — see [Keys](#keys)):

| Key | Action |
| --- | --- |
| *(printable)* | Edit the query — the document is never touched |
| `<C-n>` / `<Down>` | Next item |
| `<C-p>` / `<Up>` | Previous item |
| `<CR>` | Confirm — run the source's action on the highlighted item |
| `<C-t>` | Confirm in a **new tab** — open the highlighted item in a fresh tab |
| `<C-x>` | Confirm in a **horizontal split** |
| `<C-v>` | Confirm in a **vertical split** |
| `<Esc>` | Cancel |
| `<Tab>` / `<S-Tab>` | Multi-select — mark/unmark this row and advance (see [Sending results to a list](#sending-results-to-a-list)) |
| `<C-q>` | Send the results (marked, else all filtered) to a named list `<picker>:<query>` |
| `<C-d>` / `<C-u>` | Scroll the preview pane half-page down / up |
| `<C-f>` / `<C-b>` | Scroll the preview pane a page down / up |

## Sending results to a list

`<C-q>` sends the picker's **current results** to a **named list** keyed
`<picker>:<query>` — bemtvi's take on telescope's send-to-loclist, and a fast way to
turn a search into a working set you step through with `<CR>` in the list. Each
distinct search is its own persistent dock tab (re-running the same search updates it
in place); a named list never collides with the quickfix and survives closing the
window you sent it from. See [named lists](quickfix-dock-lists.md).

- **Filtered, not everything.** It sends the rows matching your live query — what
  you see — not every candidate the source streamed.
- **Multi-select.** Mark individual rows with `<Tab>` (it marks and advances;
  `<S-Tab>` too). Marks are kept by item, so they survive further typing /
  re-ranking. When any rows are marked, `<C-q>` sends **only the marked** ones (in
  mark order); with none marked it sends the whole filtered list.

Where the list opens is governed by the `'qfdock'` option (**on by default**, the
bemtvi way): each send opens as a **tab in the bottom dock**, so several searches
sit side by side, and `<CR>` on an entry jumps into the main editing area. Set
`:set noqfdock` for a bottom split instead. See [Quickfix & named dock
lists](quickfix-dock-lists.md) for the full model and the `btv.qf.list` / `show` API
the action builds on.

## Writing a source

`btv.picker.source{...}` registers a source. The driver, `items(ctx)`, streams
candidates by calling `ctx.push(item)` per result and signals completion by
**returning**. An item is a table with a `text` display field plus whatever data
`confirm` (or the preview) needs — e.g. `path` / `row` / `col`.

A **static** source pushes a fixed set, fuzzy-matched in Rust as you type:

```lua
btv.picker.source({
  name = "colours",
  items = function(ctx)
    for _, c in ipairs({ "red", "green", "blue", "amber" }) do
      ctx.push({ text = c })
    end
  end,
  confirm = function(item) btv.notify("picked " .. item.text) end,
})
```

A source can be **asynchronous** — wrap `items` in `btv.async` and stream from a
subprocess. bemtvi is promise-only, so an async source returns its promise and the
engine awaits it; there is no `done` callback. Reap any spawned job on close via
`ctx.on_cancel`. This is essentially how the built-in `files` source works (the
shipped one adds a fallback chain — `find`, then an `btv.fs` walk — for when `rg`
is missing):

```lua
btv.picker.source({
  name = "files",
  preview = "file",
  items = btv.async(function(ctx)
    local stream = btv.run_stream({ cmd = "rg", args = { "--files" }, cwd = ctx.cwd })
    ctx.on_cancel(function() stream:kill() end)
    for batch in btv.await_each(stream) do
      for _, l in ipairs(batch) do
        if l ~= "" then ctx.push({ text = l, path = l }) end
      end
    end
  end),
  confirm = function(item) btv.picker.edit(item) end,
})
```

Declaring `filter = true` gives the source the
[include/exclude boxes](#include--exclude-filters). Every candidate carrying a
`path` is then tested against them at the single point they all cross, so a source
gets the filter for free however it enumerates. A source that shells out to a
ripgrep-compatible tool should *also* splice `ctx.rg_globs` (ready-made `-g`
argument pairs) into its argv, so the tool prunes the tree instead of streaming
paths that are about to be dropped — on a `node_modules`-sized directory that is
the difference between the results arriving and the cap filling with noise. The
patterns themselves reach the source as `ctx.include` / `ctx.exclude`.

```lua
items = btv.async(function(ctx)
  local args = { "--files", "--color=never" }
  for _, a in ipairs(ctx.rg_globs) do args[#args + 1] = a end
  local stream = btv.run_stream({ cmd = "rg", args = args, cwd = ctx.cwd })
  …
end),
```

`btv.picker.edit(item, mode)` is the common confirm action: it opens `item.path`
and, if the item carries a 1-based `row` (and optional `col`), jumps the cursor
there. The `mode` is the confirm gesture (the picker passes it to
`confirm(item, mode)`): `"current"` opens in the focused window honoring
[`'switchbuf'`](#switching-to-an-open-tab); `"tab"` / `"split"` / `"vsplit"` (the
defaults `<C-t>` / `<C-x>` / `<C-v>`) open in a new tab / horizontal split /
vertical split. Forward it from a custom source's `confirm` to support those keys:
`confirm = function(item, mode) btv.picker.edit(item, mode) end`.

### Switching to an open tab

Where a confirmed pick (and every jump — LSP go-to, quickfix, marks) **lands** is
governed by `'switchbuf'`. bemtvi defaults it to `usetab`: opening a buffer already
shown in another tab focuses that tab instead of re-opening it in the current
window. Set it like vim — `btv.o.switchbuf = "useopen"` (reuse a window in the
current tab only) or `btv.o.switchbuf = ""` (classic: always open in the current
window). `<C-t>` always makes a new tab regardless (an explicit tab gesture).

The widget windows its rendering and matches incrementally, so a source can
stream **100k+ candidates** and stay fast; `max_results` (default 100000) is only
a runaway-source safety bound.

### Dynamic (live) sources

Set `dynamic = true` and the source re-runs on **every prompt edit** with the
local fuzzy matcher bypassed — the source itself does the filtering. It reads the
live prompt from `ctx.query` and the working directory from `ctx.cwd`. This is
essentially how live grep works (re-spawning `rg` per query; the shipped source
falls back to `grep`, then an `btv.fs` match, when `rg` is missing):

```lua
btv.picker.source({
  name = "live_grep",
  dynamic = true,
  preview = "location",
  items = btv.async(function(ctx)
    if ctx.query == "" then return end
    local stream = btv.run_stream({
      cmd = "rg", args = { "--vimgrep", "--", ctx.query }, cwd = ctx.cwd,
    })
    ctx.on_cancel(function() stream:kill() end)
    for batch in btv.await_each(stream) do
      for _, l in ipairs(batch) do
        local file, lnum, col = l:match("^(.-):(%d+):(%d+):")
        if file then
          ctx.push({ text = l, path = file, row = tonumber(lnum), col = tonumber(col) })
        end
      end
    end
  end),
  confirm = function(item) btv.picker.edit(item) end,
})
```

A dynamic source is **debounced**: a query edit cancels the in-flight job and
schedules the search `debounce` ms later, so a fast typist spawns one process per
*pause*, not one per keystroke. While the new search runs the previous results
stay on screen — the list never flashes empty; they swap out only when the first
new result arrives (or clear if nothing matched). The delay defaults to
`btv.picker.debounce` (250 ms), overridable per source (`debounce = N`) or per
open; `0` disables it.

## Preview pane

Add `preview` to show a side pane for the highlighted item:

- `"file"` — shows the head of `item.path`.
- `"location"` — shows `item.path` scrolled to `item.row` / `item.col` (1-based)
  with the match range highlighted.

Omitted means no preview pane. Preview content is tree-sitter-highlighted by the
server, and works across the terminal, GUI, and web clients. Scroll it with the
`<C-d>` / `<C-u>` / `<C-f>` / `<C-b>` keys above.

## Open-time options

`btv.picker.open(name, opts)` — each `opts` field overrides the matching field on
the source, which in turn overrides the picker default:

| Option | Meaning |
| --- | --- |
| `width` / `height` | A **fixed** box size: a cell count (`100`) or a viewport fraction (`"80vw"` / `"60vh"` / `"50%"`). The picker is never content-sized. |
| `align` + `margin` | Placement, like a float (`"top-left"` … `"center"` … `"bottom-right"`, default centered). |
| `preview` | `"file"` / `"location"` / `nil` (no pane). |
| `prompt_pos` | `"top"` (default) or `"bottom"` (telescope-style, input under the results). |
| `query` | Initial prompt text — the picker opens already filtered against it, caret at its end. Default `""`. |
| `title` | A title centered on the box's top border (the shipped sources set their own); `nil` for none. |
| `multiselect` | Whether `<Tab>` marks rows for a batch action (default `true`); `false` is a single-choice picker. |
| `layer` | Where a confirmed item opens: `"main"` crosses back to the main editor area first; `"active"` (the default) opens in the focused layer. The shipped `files` / `live_grep` set `"main"`. |
| `debounce` | Milliseconds before a `dynamic` source re-runs; `0` off. |
| `include` / `exclude` | Pre-fill the [filter boxes](#include--exclude-filters) — a comma-separated string or a list of globs. A seed, not a lock: the boxes stay editable. |
| `filters` | `"open"` reveals the filter rows immediately; `"collapsed"` (the default) keeps the picker's single-line shape with a badge. |

```lua
-- a snappier live grep, just for this map:
btv.keymap.set("n", "<leader>fG", function()
  btv.picker.open("live_grep", { debounce = 100 })
end)
```

## Keys

Every picker key is an ordinary `picker`-mode keymap, not a hardcoded grab: while
a picker owns input the server selects the `picker` bucket, so navigation,
confirm, cancel, preview-scroll, and query-editing are all rebindable like any
other mode:

```lua
btv.keymap.set("picker", "<C-j>", btv.picker.actions.next)
btv.keymap.set("picker", "<C-k>", btv.picker.actions.prev)
btv.keymap.set("picker", "<Tab>", btv.picker.actions.confirm)
-- disable a default binding by mapping it to an empty function:
btv.keymap.set("picker", "<C-n>", function() end)
```

The actions are `next`, `prev`, `confirm`, `confirm_tab`, `confirm_split`,
`confirm_vsplit`, `cancel`, `send_to_list`,
`toggle_select`, `clear_select`, `preview_half_down`, `preview_half_up`,
`preview_page_down`, `preview_page_up`, `backspace`, `delete`, `left`, `right`,
`to_start`, `to_end`, `next_field`, `toggle_filters`. The one key that is *not* a
map is an arbitrary printable char — there is no way to enumerate every char, so an
unmapped printable just inserts into whichever line has focus.

## Include / exclude filters

`files` and `live_grep` carry two glob boxes, the way VSCode's search panel does —
**files to include** and **files to exclude**. `<C-g>` reveals them and steps
through the three editable lines (query → include → exclude); typing goes to
whichever has focus.

```
┌─ Find Files ─────────────────────┐      ┌─ Find Files ─────────────────────┐
│ > handler               [+1 -2]  │      │ > handler                        │
├──────────────────────────────────┤ <C-g>│ include  src/**                  │
│ src/net/handler.rs               │ ───► │ exclude  target/, *.lock         │ ← focus
│ src/ui/handler.rs                │      ├──────────────────────────────────┤
└──────────────────────────────────┘      │ src/net/handler.rs               │
        collapsed, filters active         └──────────────────────────────────┘
```

Each box holds a **comma-separated** list of globs. A comma inside `{…}` belongs to
the pattern, so `**/{node_modules,target}/**` is one glob, not two. Collapsing the
boxes does not turn the filter off — the badge (`[+1 -2]`: one include, two exclude
patterns) keeps an active filter visible, so a search that is quietly hiding files
never looks like one that isn't.

### Defaults

`btv.picker.setup` sets the line every filterable picker opens with — the "stop
showing me build output" knob:

```lua
btv.picker.setup({
  exclude = { "target/", "node_modules/", "*.min.js" },
  history = 20,  -- past lines kept per box for <C-Up>/<C-Down>; 0 disables
})
```

### History — `<C-Up>` / `<C-Down>`

Each box remembers the lines you have used, most recent first, and **persists them
across restarts** (via `btv.shada.plugin`, on the ordinary shada cadence). With a box
focused, `<C-Up>` walks back into older lines and `<C-Down>` forward again, returning
to the line you were composing — cmdline history. The two boxes keep separate lists,
so an include pattern never surfaces in the exclude box where it would mean the
opposite.

A filterable picker opens pre-filled with the most recent line for each box, so the
filter you worked out yesterday is already applied rather than retyped.

```lua
btv.picker.history("exclude")   -- the stored lines, most recent first
btv.picker.forget_history()     -- drop them all
```

Precedence, lowest to highest: the source's own default → `btv.picker.setup` → the
most recent line used → an explicit `btv.picker.open{ include = … }`. History outranks
the configured default because a line you actually typed is a stronger statement than
one configured months ago; an explicit open option outranks everything, so a picker
asked for a scope gets exactly that scope.

Two conveniences make a typed pattern mean what you meant:

| You type | It matches |
| --- | --- |
| `*.lock` | any `.lock` file, at any depth |
| `target` | the `target` entry **and** everything under it, at any depth |
| `vendor/` | same — a trailing `/` just says "the directory" |
| `src/**` | taken as written; a pattern with a `/` is anchored at the search root |

A picker can also open already scoped, which is what a dedicated map usually wants:

```lua
-- "find in sources"
btv.keymap.set("n", "<leader>fs", function()
  btv.picker.open("files", { include = { "src/**", "crates/**" } })
end)

-- grep everything except the vendored trees, with the boxes already showing
btv.picker.open("live_grep", { exclude = "vendor/, **/*.min.js", filters = "open" })
```

Any source whose items carry a `path` can have the boxes by declaring
`filter = true` — see [Writing a source](#writing-a-source). On a source without
it, `<C-g>` says so rather than presenting boxes that would filter nothing.

## Try it

A runnable playground ships in [`examples/ui-picker`](https://github.com/davidrios/bemtvi/tree/main/examples/ui-picker):

```sh
BEMTVI_CONFIG=examples/ui-picker cargo run -p bemtvi -- examples/ui-picker/sample.txt
```

It maps the three built-in sources, registers a custom static source, and shows
the box-size, preview, and debounce overrides.

For the filter boxes there is a second playground,
[`examples/picker-filters`](https://github.com/davidrios/bemtvi/tree/main/examples/picker-filters):

```sh
BEMTVI_CONFIG=examples/picker-filters \
  cargo run -p bemtvi -- examples/picker-filters/sample.txt
```

It ships the mess a real project has — a `target/` artifact, a `vendor/` lock file,
a dotfile — and walks through defaults, editing a box, what each pattern shape
means, the `<C-Up>` history, pre-scoped pickers, and a custom filterable source.

## How it works (in brief)

The full item tables stay Lua-side; only a display label and an integer key cross
the bridge per result (exactly like `btv.ui.select`), so an item's arbitrary
fields never need to serialize. Candidates are batched (~1000 per bridge call)
rather than crossing one at a time, which is what makes streaming 100k results
fast. A generation token stamps every run, so a push from a query you've typed
past — or from a picker that has since closed — is dropped.

For the full design — the unified float-list widget, the Rust matcher, dynamic
forwarding, and the preview cache — see the
[fuzzy-finder plan](../plans/2026-06-14-btv-picker-fuzzy-finder.md), the
[preview-pane plan](../plans/2026-06-14-btv-picker-preview-pane.md), and the
[float-list widget spec](../specs/2026-06-14-btv-ui-float-widget.md).
