# Permanent docks

A **dock** is a permanent, editable window region pinned to a screen edge — like
VSCode's side bars and bottom panel. It holds normal buffer windows (you can
split inside it), but unlike an ordinary split it is **global** — it shows on
every tab — and the main editing area can never disturb it: splits, window
switches, and tab changes in the main area leave it untouched.

There are four docks, one per edge: `left`, `right`, `top`, `bottom`. The top
dock sits **above** the tabline, owning the very top rows of the screen.

## Layers: main ↔ docks

The screen is split into two **layers**: the main editing area and the docks.
`<C-w>` window commands act within the *focused* layer; the doubled
`<C-w><C-w>` prefix is a **layer switch** that crosses between them.

| Keys | Effect |
| --- | --- |
| `<C-w><C-w>h` / `j` / `k` / `l` | Cross focus from the main area **into** the left / bottom / top / right dock |
| `<C-w><C-w>l` (from a dock) | Cross **back** to the main area |
| `<C-w>v` / `<C-w>s` (in a dock) | Split *within* the focused dock — a single `<C-w>`, as usual |
| `<C-w>l` (in a dock) | Move between windows inside the dock, without leaving it |
| `<C-w><C-w>v` (from main) | Cross to the last-used dock and split it |

So once focus is inside a dock, every plain `<C-w>{cmd}` operates inside that
dock; `<C-w><C-w>` returns you to the main area. Each dock starts on an empty
scratch buffer — cross into one and start typing.

## Opening and closing

Drive docks from Lua or the ex-command wrappers:

```lua
-- side is "left" / "right" / "top" / "bottom"; size is columns (left/right) or
-- rows (top/bottom); buf is an optional existing buffer (default: a scratch).
nx.dock.open({ side = "left", size = 28 })
nx.dock.open({ side = "bottom", size = 6 })

nx.dock.focus("left")   -- move focus to a dock
nx.dock.close("left")   -- drop the dock and its content
```

| Ex-command | Does |
| --- | --- |
| `:DockOpen {side} [size]` | Open (or resize/refocus) a dock |
| `:DockFocus {side}` | Move focus to a dock |
| `:DockClose {side}` | Close a dock, **discarding** its content |
| `:DockToggle {side}` | Hide if shown, show if hidden — **keeping** content |
| `:DockHide {side}` / `:DockShow {side}` | The two halves of toggle, addressed individually |

The ops are queued and applied after the current Lua chunk runs (the editor's
"Lua queues, core mutates" flow), so docks opened in `init.lua` appear on the
first frame.

## Toggle vs. close

These are different on purpose:

- **`:DockClose`** (and `nx.dock.close`) drops the dock and its content.
- **`:DockToggle` / `:DockHide` / `:DockShow`** (and `nx.dock.toggle/hide/show`)
  collapse a dock *from view* while keeping everything — its splits, tabs,
  cursor, and text all come back exactly as they were when you show it again.

A collapsed dock isn't gone: it leaves a `▸LABEL` chip on the command-line row
(bottom-left, when idle). Click the chip to bring that dock back.

Toggling from a keymap uses the same path as the ex-command:

```lua
vim.keymap.set("n", "<leader>e", function()
  nx.dock.toggle("left")
end, { desc = "toggle the left explorer dock" })
```

## Per-dock options

Docks have their own option scope, alongside `nx.bo` / `nx.wo` / `nx.o`. Set
options inline in `nx.dock.open{...}` or after the fact through
`nx.dock.opt(side)`; reads return the cached value (or its default).

```lua
nx.dock.opt("left").title = "EXPLORER"   -- a fixed strip label
nx.dock.opt("left").showtabline = 2      -- always show the dock's own tabline
nx.dock.opt("bottom").autohide = true    -- collapse when focus leaves
nx.dock.opt("left").size = 40            -- resize live
```

| Option | Meaning |
| --- | --- |
| `size` | Width (left/right) or height (top/bottom); settable live to grow/shrink |
| `title` | A fixed strip label, shown ahead of the dock's tab cells |
| `showtabline` | Per-dock override of the global option (`0` never / `1` if >1 tab / `2` always) |
| `laststatus` | Per-dock statusline override (`0`/`1`/`2`/`3`) |
| `autohide` | Collapse the dock the moment focus leaves it; it pops back when you cross in |
| `winhighlight` | Per-window highlight remap (`"Normal:NormalSB,EndOfBuffer:Hidden"`) so a dock paints like a sidebar — see `examples/dock-winhighlight` |

Setting an unknown option warns loudly rather than silently ignoring it.

> **`autohide`** is great for a panel you want out of the way until you need it —
> a terminal tray, say. It collapses as soon as focus leaves and re-appears when
> you cross back in (`<C-w><C-w>j`) or run `:DockShow {side}`.

## Per-dock tabs and tablines

Each dock — and the main area — has its own independent tab stack and tabline.
Focus a dock and `:tabnew` opens a tab *inside that dock*; its strip lights up on
its own, driven by that dock's `showtabline`. Clicking a dock's tabline switches
tabs within that dock. (Design: the
[per-region tablines spec](../plans/2026-06-14-per-region-tablines.md).)

## Try it

A runnable playground ships in [`examples/dock`](https://github.com/davidrios/nxvim/tree/main/examples/dock):

```sh
NXVIM_CONFIG=examples/dock cargo run -p nxvim -- examples/dock/sample.txt
```

It opens a titled left side bar and an `autohide` bottom tray, maps
`<leader>e` to toggle the explorer, and walks through the layer-switch keys
interactively.

## How it works (in brief)

A dock is a parked `WindowTree` swapped onto the live `self.windows` whenever it
gains focus — the same trick tab pages use to keep one tree active. Because every
`split` / `close` / `focus` / edit / redraw reads from `self.windows`, they "just
work" inside the focused dock with no retargeting. Geometry reserves the edge
bands before laying out the main area, and each client maps a window's region
(`Main` / `DockLeft` / `…`) to its absolute screen origin.

For the full design — the layer-swap model, geometry, `<C-w><C-w>` parsing, and
the cross-client rendering — see the
[permanent docks plan](../plans/2026-06-14-permanent-docked-panels.md) and its
[per-region tablines follow-up](../plans/2026-06-14-per-region-tablines.md).
