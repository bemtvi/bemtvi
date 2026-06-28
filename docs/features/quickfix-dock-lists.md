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

The `nx.qf.*` family populates a list and shows it. Each takes an array of entry
dicts (`{ filename =, lnum =, col =, text = }`, the `setloclist` shape) and an
optional `{ title = }`:

| Function | Effect |
| --- | --- |
| `nx.qf.send_to_loclist(list, opts)` | Replace the **current window's** location list and open it in a bottom split (vim behavior — a loclist is window-scoped and never docks). |
| `nx.qf.add_to_loclist(list, opts)` | **Append** to the current window's location list. |
| `nx.qf.send_to_qflist(list, opts)` | Replace the global quickfix list and show it (one reused dock tab under `'qfdock'`, else a split). |
| `nx.qf.add_to_qflist(list, opts)` | **Append** to the global quickfix list and show it. |

To save several searches as side-by-side dock tabs, use a **named list**
(`nx.qf.list` / `show`, below) — that, not the loclist, is the dock-tab surface.

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
results to a **named list** keyed `<picker>:<query>` — the marked rows if any are
marked, else the whole filtered set. Each distinct search is its own persistent
dock tab (re-running the same search updates it in place), independent of the
quickfix and of any window.

## Named lists (window-independent, addressed by name)

A *named list* is like the global quickfix list — structured entries, its own
bottom-dock tab, `<CR>` jumps to the entry in the main editing layer — but there can
be **many**, each addressed by a stable **name**. Storage lives on the editor (not a
window), so a named list survives closing any window and never collides with the
single quickfix list (or with `:grep` / `:make`). That makes it the right home for a
persistent plugin panel (e.g. a debugger's "All Breakpoints").

You push items with `nx.qf.list(name, items)` whenever your data changes — no
datasource/refresh indirection — then `nx.qf.show(name)` opens or focuses the tab.

```lua
local function todo_items()           -- re-scan the live buffer on demand
  local items = {}
  for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    if line:find("TODO") then
      items[#items + 1] = { filename = nx.buf.name(), lnum = i, col = 1, text = line }
    end
  end
  return items
end

nx.keymap.set("n", "<leader>tl", function()
  nx.qf.list("todos", todo_items(), { title = "TODO / FIXME" })
  nx.qf.show("todos")                 -- repaints the tab if already open
end)
```

| Call | Purpose |
| --- | --- |
| `nx.qf.list(name, items[, opts]) -> name` | create / replace the list `name`; repaints its tab if open |
| `nx.qf.show(name) -> name` | open or focus the list's dock tab |
| `nx.qf.drop(name) -> name` | close its tab and forget the list |

`opts.title` sets the dock-tab label (defaults to `name`); `opts.action` is `"r"`
(default — replace in place), `" "` (push a new list onto the stack), or `"a"`
(append). `nx.qf.list` only writes the list — it never opens or focuses the tab, so
call `nx.qf.show` to surface it. Showing a name with no items yet opens an empty tab.

Each name is an independent list shown as its own dock tab, so several sit side by
side and a `:grep` later clobbers none of them. Closing the tab — or any window —
never destroys a named list: re-show it by name and it re-renders.

## Try it

A runnable playground ships in
[`examples/picker-to-named-list`](https://github.com/davidrios/nxvim/tree/main/examples/picker-to-named-list):

```sh
NXVIM_CONFIG=examples/picker-to-named-list \
  cargo run -p nxvim -- examples/picker-to-named-list/sample.txt
```

Named lists have their own playground in
[`examples/named-lists`](https://github.com/davidrios/nxvim/tree/main/examples/named-lists)
(`\tl` collects a TODO/FIXME list, `\ll` a long-lines list beside it, `\td` drops the
first):

```sh
NXVIM_CONFIG=examples/named-lists \
  cargo run -p nxvim -- examples/named-lists/sample.txt
```

## How it works (in brief)

Three list flavors share one rendering / navigation engine, differing only in where
they're stored and shown:

- The **quickfix** list is global. Under `'qfdock'` (default) it shows as the single
  bottom-dock tab; otherwise a bottom split.
- A **location list** is owned by a window (vim's model). `:lopen` / `send_to_loclist`
  open it in a bottom split of that window; closing the owner closes the list. It
  never docks — `'qfdock'` does not apply to it.
- A **named list** is the dock-tab "save searches" surface. Its list lives in an
  editor-side registry keyed by name (not on a window), so it is addressed and
  re-shown by name, sits beside other named lists as its own dock tab, and outlives
  any window close.

All three jump the same way: a jump excludes the display window as its target, so it
falls back to the main layer (always enumerated first), landing the file in the
editing area rather than inside the dock. The picker's `<C-q>` captures the matched
item keys **and the live query** server-side, then (in Lua) builds a named list keyed
`<picker>:<query>` via `nx.qf.list` + `nx.qf.show`.
