# Quickfix & location-list dock tabs

nxvim shows quickfix and location lists as **tabs in the bottom dock** by default,
not as a split window — so you can keep several searches open side by side and
flip between them, while activating an entry opens the file in the main editing
area. The classic vim behavior (a bottom split, one list, replaced in place) is one
option away.

This is the surface the [fuzzy picker](picker.md)'s `<C-q>` builds on, but it is a
general list facility — `:copen`, `:lopen`, `:make`, `:vimgrep`, diagnostics, and
the `nx.qf.*` API all flow through it.

## The `'qfdock'` option

| `'qfdock'` | Behavior |
| --- | --- |
| **on** (default — the nxvim way) | A list's display opens as a tab in the **bottom dock**. Location lists each get their own tab (so searches stack up); the single global quickfix list is one reused tab. `<CR>` / `:cc` / `:cnext` jump into the **main** layer, leaving the dock in place. |
| off (`:set noqfdock`) | The classic vim/telescope behavior: a full-width **bottom split** of the current window, the single global quickfix list / per-window location list, replaced in place. |

```lua
nx.o.qfdock = false      -- prefer the classic split behavior
-- or, transiently:  :set noqfdock  /  :set qfdock
```

The option governs `:copen` / `:lopen` and the `nx.qf.{send,add}_to_*` actions
uniformly, so one switch flips the whole behavior.

## Closing a list

`:cclose` / `:lclose` close the list. In dock mode that closes its dock tab (and
the bottom dock itself when it was the last tab); in split mode it closes the
split — same command, either way.

Dock tabs are ordinary dock tabs: cycle between saved searches with `gt` / `gT`
while the dock is focused, cross in and out of the dock with `<C-w><C-w>`. (See
[Permanent docks](docks.md).)

## Sending results to a list

The `nx.qf.*` family populates a list and shows it, honoring `'qfdock'`. Each takes
an array of entry dicts (`{ filename =, lnum =, col =, text = }`, the `setloclist`
shape) and an optional `{ title = }`:

| Function | Effect |
| --- | --- |
| `nx.qf.send_to_loclist(list, opts)` | A **new** location list. In dock mode → a new bottom-dock tab beside the others; in split mode → replaces the current window's loclist + opens a split. |
| `nx.qf.add_to_loclist(list, opts)` | **Append** to the focused dock loclist tab (or the current window's loclist in split mode). |
| `nx.qf.send_to_qflist(list, opts)` | Replace the global quickfix list and show it (one reused tab / split). |
| `nx.qf.add_to_qflist(list, opts)` | **Append** to the global quickfix list and show it. |

(Bare `nx.send_to_loclist` etc. aliases exist too.) Example — send the current
buffer's TODO lines to a saved location list:

```lua
nx.keymap.set("n", "<leader>lt", function()
  local items = {}
  for i, line in ipairs(nx.api.nvim_buf_get_lines(0, 0, -1, false)) do
    if line:match("TODO") then
      items[#items + 1] = { filename = nx.buf.name(), lnum = i, text = line }
    end
  end
  nx.qf.send_to_loclist(items, { title = "TODOs" })
end)
```

## From the picker

In any [picker](picker.md), `<Tab>` marks rows (multi-select) and `<C-q>` sends the
results to a location list — the marked rows if any are marked, else the whole
filtered set. With `'qfdock'` on (default) each `<C-q>` saves the search as its own
bottom-dock tab.

## Dynamic (named, function-sourced) lists

A *dynamic list* binds a **name** to a **datasource function**. `nx.qf.refresh(name)`
re-runs that function and rewrites the bound list in place (action `"r"`), so an open
quickfix / location window repaints with the fresh results — no stale snapshot. The
source returns an items array (the same entry-dict shape as `setqflist`) **or a
promise** resolving to one, so a slow producer (LSP, ripgrep) never blocks the editor.

```lua
nx.qf.dynamic({
  name = "todos",
  title = "TODO / FIXME",
  source = function()                 -- re-scans the live buffer each refresh
    local items = {}
    for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
      if line:find("TODO") then
        items[#items + 1] = { filename = nx.buf.name(), lnum = i, col = 1, text = line }
      end
    end
    return items                      -- or: return some_promise_of_items
  end,
})

nx.keymap.set("n", "<leader>tr", function()
  nx.qf.refresh("todos"):next(function() nx.qf.open() end)
end)
```

| Call | Purpose |
| --- | --- |
| `nx.qf.dynamic{ name=, source=, loclist=, win=, title= }` | register (or replace) a named list |
| `nx.qf.refresh(name) -> promise` | re-run the source, rewrite the list in place, repaint |
| `nx.qf.drop(name) -> bool` | forget the registration (leaves the list contents) |

Pass `loclist = true` for a per-window **location** list; `nx.qf.dynamic` captures the
current window then (override with `win = <id>`), so refreshing from anywhere still
targets the right one. A first registration neither populates nor opens the list —
call `refresh`, then `:copen` / `:lopen` (`nx.qf.open` / `nx.qf.lopen`) to show it.

**Redefining is in place.** Calling `nx.qf.dynamic` again with an existing `name`
replaces that registration rather than spawning a second list: a location list keeps
the window a prior call bound (unless you pass an explicit `win`), so redefining a
*visible* list updates it where it already shows — it never opens a new window or dock
tab against whatever window happens to be focused now. A redefinition also refreshes
immediately, so the new source takes effect (repainting an open window) without a
manual `refresh`.

## Try it

A runnable playground ships in
[`examples/picker-to-loclist`](https://github.com/davidrios/nxvim/tree/main/examples/picker-to-loclist):

```sh
NXVIM_CONFIG=examples/picker-to-loclist \
  cargo run -p nxvim -- examples/picker-to-loclist/sample.txt
```

Dynamic lists have their own playground in
[`examples/dynamic-lists`](https://github.com/davidrios/nxvim/tree/main/examples/dynamic-lists)
(`\tr` refreshes a TODO quickfix list, `\lr` a long-lines location list, `\ar` an
async-sourced one):

```sh
NXVIM_CONFIG=examples/dynamic-lists \
  cargo run -p nxvim -- examples/dynamic-lists/sample.txt
```

## How it works (in brief)

A location list is owned by a window. In dock mode each saved search is a dock-tab
window that both owns *and* displays its own location list — so N searches are N
independent lists for free, with no new "named list" concept. A jump excludes the
display window as its target, so it falls back to the main layer (which is always
enumerated first), landing the file in the editing area rather than inside the
dock. The picker's `<C-q>` captures the matched item keys server-side (the filtered
set), then builds the list through the same `nx.qf.send_to_loclist` path.
