# Fuzzy picker

`nx.picker` is nxvim's native fuzzy finder — a centered float with a prompt that
grabs every key, a Rust fuzzy matcher that re-ranks as you type, and an optional
preview pane. The **server** owns the widget: the prompt, the matcher,
navigation, and a generation token that drops a stale response for a query you've
already typed past. No input loop runs in Lua — a source is just a thin driver
that *streams candidates in* and *handles confirm*.

It ships with three sources — `files`, `live_grep`, `buffers` — and registering
your own is a few lines.

## Using a picker

The three built-in sources are bound out of the box:

| Map | Source |
| --- | --- |
| `<leader>ff` | `files` — fuzzy file finder |
| `<leader>fg` | `live_grep` — live grep |
| `<leader>fb` | `buffers` — open buffers (scoped to the focused layer, like `:ls`) |

These are overridable defaults — your own map for the same key wins, and you can
disable one by binding it to an empty function. To open any registered source
from your own keymap, call `nx.picker.open`:

```lua
nx.keymap.set("n", "<leader>o", function() nx.picker.open("files") end)
```

In the open picker (all of these are rebindable — see [Keys](#keys)):

| Key | Action |
| --- | --- |
| *(printable)* | Edit the query — the document is never touched |
| `<C-n>` / `<Down>` | Next item |
| `<C-p>` / `<Up>` | Previous item |
| `<CR>` | Confirm — run the source's action on the highlighted item |
| `<Esc>` | Cancel |
| `<Tab>` / `<S-Tab>` | Multi-select — mark/unmark this row and advance (see [Sending results to a list](#sending-results-to-a-list)) |
| `<C-q>` | Send the results (marked, else all filtered) to a location list |
| `<C-d>` / `<C-u>` | Scroll the preview pane half-page down / up |
| `<C-f>` / `<C-b>` | Scroll the preview pane a page down / up |

## Sending results to a list

`<C-q>` sends the picker's **current results** to a location list — nxvim's take
on telescope's send-to-loclist, and a fast way to turn a search into a working
set you can step through with `:lnext` / `:lprev` (or `<CR>` in the list).

- **Filtered, not everything.** It sends the rows matching your live query — what
  you see — not every candidate the source streamed.
- **Multi-select.** Mark individual rows with `<Tab>` (it marks and advances;
  `<S-Tab>` too). Marks are kept by item, so they survive further typing /
  re-ranking. When any rows are marked, `<C-q>` sends **only the marked** ones (in
  mark order); with none marked it sends the whole filtered list.

Where the list opens is governed by the `'qfdock'` option (**on by default**, the
nxvim way): each send opens as a **tab in the bottom dock**, so several searches
sit side by side, and `<CR>` on an entry jumps into the main editing area. Set
`:set noqfdock` for the classic vim/telescope behavior (a bottom split, replaced
in place). See [Quickfix & location-list dock tabs](quickfix-dock-lists.md) for
the full model and the `nx.qf.{send,add}_to_{loc,qf}list` API the action builds on.

## Writing a source

`nx.picker.source{...}` registers a source. The driver, `items(ctx)`, streams
candidates by calling `ctx.push(item)` per result and signals completion by
**returning**. An item is a table with a `text` display field plus whatever data
`confirm` (or the preview) needs — e.g. `path` / `row` / `col`.

A **static** source pushes a fixed set, fuzzy-matched in Rust as you type:

```lua
nx.picker.source({
  name = "colours",
  items = function(ctx)
    for _, c in ipairs({ "red", "green", "blue", "amber" }) do
      ctx.push({ text = c })
    end
  end,
  confirm = function(item) nx.notify("picked " .. item.text) end,
})
```

A source can be **asynchronous** — wrap `items` in `nx.async` and stream from a
subprocess. nxvim is promise-only, so an async source returns its promise and the
engine awaits it; there is no `done` callback. Reap any spawned job on close via
`ctx.on_cancel`. This is how the built-in `files` source works:

```lua
nx.picker.source({
  name = "files",
  preview = "file",
  items = nx.async(function(ctx)
    local stream = nx.run_stream({ cmd = "rg", args = { "--files" }, cwd = ctx.cwd })
    ctx.on_cancel(function() stream:kill() end)
    for batch in nx.await_each(stream) do
      for _, l in ipairs(batch) do
        if l ~= "" then ctx.push({ text = l, path = l }) end
      end
    end
  end),
  confirm = function(item) nx.picker.edit(item) end,
})
```

`nx.picker.edit(item)` is the common confirm action: it opens `item.path` and, if
the item carries a 1-based `row` (and optional `col`), jumps the cursor there.

The widget windows its rendering and matches incrementally, so a source can
stream **100k+ candidates** and stay fast; `max_results` (default 100000) is only
a runaway-source safety bound.

### Dynamic (live) sources

Set `dynamic = true` and the source re-runs on **every prompt edit** with the
local fuzzy matcher bypassed — the source itself does the filtering. It reads the
live prompt from `ctx.query` and the working directory from `ctx.cwd`. This is how
live grep works (re-spawning `rg` per query):

```lua
nx.picker.source({
  name = "live_grep",
  dynamic = true,
  preview = "location",
  items = nx.async(function(ctx)
    if ctx.query == "" then return end
    local stream = nx.run_stream({
      cmd = "rg", args = { "--vimgrep", "--", ctx.query }, cwd = ctx.cwd,
    })
    ctx.on_cancel(function() stream:kill() end)
    for batch in nx.await_each(stream) do
      for _, l in ipairs(batch) do
        local file, lnum, col = l:match("^(.-):(%d+):(%d+):")
        if file then
          ctx.push({ text = l, path = file, row = tonumber(lnum), col = tonumber(col) })
        end
      end
    end
  end),
  confirm = function(item) nx.picker.edit(item) end,
})
```

A dynamic source is **debounced**: a query edit cancels the in-flight job and
schedules the search `debounce` ms later, so a fast typist spawns one process per
*pause*, not one per keystroke. While the new search runs the previous results
stay on screen — the list never flashes empty; they swap out only when the first
new result arrives (or clear if nothing matched). The delay defaults to
`nx.picker.debounce` (250 ms), overridable per source (`debounce = N`) or per
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

`nx.picker.open(name, opts)` — each `opts` field overrides the matching field on
the source, which in turn overrides the picker default:

| Option | Meaning |
| --- | --- |
| `width` / `height` | A **fixed** box size: a cell count (`100`) or a viewport fraction (`"80vw"` / `"60vh"` / `"50%"`). The picker is never content-sized. |
| `align` + `margin` | Placement, like a float (`"top-left"` … `"center"` … `"bottom-right"`, default centered). |
| `preview` | `"file"` / `"location"` / `nil` (no pane). |
| `prompt_pos` | `"top"` (default) or `"bottom"` (telescope-style, input under the results). |
| `debounce` | Milliseconds before a `dynamic` source re-runs; `0` off. |

```lua
-- a snappier live grep, just for this map:
nx.keymap.set("n", "<leader>fG", function()
  nx.picker.open("live_grep", { debounce = 100 })
end)
```

## Keys

Every picker key is an ordinary `picker`-mode keymap, not a hardcoded grab: while
a picker owns input the server selects the `picker` bucket, so navigation,
confirm, cancel, preview-scroll, and query-editing are all rebindable like any
other mode:

```lua
nx.keymap.set("picker", "<C-j>", nx.picker.actions.next)
nx.keymap.set("picker", "<C-k>", nx.picker.actions.prev)
nx.keymap.set("picker", "<Tab>", nx.picker.actions.confirm)
-- disable a default binding by mapping it to an empty function:
nx.keymap.set("picker", "<C-n>", function() end)
```

The actions are `next`, `prev`, `confirm`, `cancel`, `send_to_loclist`,
`toggle_select`, `clear_select`, `preview_half_down`, `preview_half_up`,
`preview_page_down`, `preview_page_up`, `backspace`, `delete`, `left`, `right`,
`to_start`, `to_end`. The one key that is *not* a map is an
arbitrary printable char — there is no way to enumerate every char, so an unmapped
printable just inserts into the query.

## Try it

A runnable playground ships in [`examples/ui-picker`](https://github.com/davidrios/nxvim/tree/main/examples/ui-picker):

```sh
NXVIM_CONFIG=examples/ui-picker cargo run -p nxvim -- examples/ui-picker/sample.txt
```

It maps the three built-in sources, registers a custom static source, and shows
the box-size, preview, and debounce overrides.

## How it works (in brief)

The full item tables stay Lua-side; only a display label and an integer key cross
the bridge per result (exactly like `nx.ui.select`), so an item's arbitrary
fields never need to serialize. Candidates are batched (~1000 per bridge call)
rather than crossing one at a time, which is what makes streaming 100k results
fast. A generation token stamps every run, so a push from a query you've typed
past — or from a picker that has since closed — is dropped.

For the full design — the unified float-list widget, the Rust matcher, dynamic
forwarding, and the preview cache — see the
[fuzzy-finder plan](../plans/2026-06-14-nx-picker-fuzzy-finder.md), the
[preview-pane plan](../plans/2026-06-14-nx-picker-preview-pane.md), and the
[float-list widget spec](../specs/2026-06-14-nx-ui-float-widget.md).
