# Beyond vim — what nxvim adds

nxvim speaks vim at the keyboard: keystrokes, modes, ex-commands, and options
track [vim/neovim](https://neovim.io)'s observable editing behavior. On top of
that baseline it grows a handful of features vim and neovim don't have natively —
modern editing and UI surfaces that fit the modal grammar rather than fighting
it.

This page is the index. Each feature below has (or will have) a full guide
linked from its name; the one-liner is the elevator pitch.

## Editing

| Feature | What it is |
| --- | --- |
| [Multi-cursor mode](features/multicursor.md) | Helix/Sublime-style multi-editing: drop N cursors in a dedicated placement mode, then have motions, operators, visual mode, and insert all act on every cursor at once. |

## UI surfaces

| Feature | What it is |
| --- | --- |
| [Permanent docks](features/docks.md) | VSCode-style editable edge panels (file tree, terminals, problem lists) that are global across tabs, toggle independently of windows, and carry their own tabs and options. |
| [Fuzzy picker](features/picker.md) | `nx.picker`: a server-owned fuzzy finder with streaming Lua sources, live (`dynamic`) sources, a file/location preview pane, and fully rebindable keys. |

These are described in their design specs today; full guides are being written
into this section.

| Feature | What it is | Reference |
| --- | --- | --- |
| Floating windows & UI widgets | First-class floats plus built-in `nx.ui` widgets (input, confirm, select, content floats) any plugin can drive. | [spec](specs/2026-06-14-nx-ui-float-widget.md) |

## Platform

| Feature | What it is | Reference |
| --- | --- | --- |
| Native `nx.*` plugin API | nxvim's own Lua API where the server owns every UI surface and plugins provide data and behavior — not a neovim-compat shim. | [ADR 0002](decisions/0002-native-plugin-system.md) |
| Browser editor | The full editor core compiled to WebAssembly, running entirely client-side with no server. | [spec](plans/2026-06-09-edit-host-and-browser-lua.md) |
| Lua plugin testing | `nx.test` (describe/it/expect + async) plus a `nxvim --test-plugin` runner, so pure-Lua plugins test themselves. | [spec](specs/2026-06-19-lua-plugin-testing.md) |

> **Scope note.** The "spec" links point at design/plan documents written for
> contributors — they describe the implementation, not just the user-facing
> surface. As each feature gets a dedicated user guide it moves up into a full
> entry like multi-cursor's.
