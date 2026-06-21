# Beyond vim — what nxvim adds

nxvim speaks vim at the keyboard: keystrokes, modes, ex-commands, and options
track [vim/neovim](https://neovim.io)'s observable editing behavior. On top of
that baseline it grows a handful of features vim and neovim don't have natively —
modern editing and UI surfaces that fit the modal grammar rather than fighting
it.

This page is the index. Each feature below has a full guide linked from its name;
the one-liner is the elevator pitch.

## Editing

| Feature | What it is |
| --- | --- |
| [Multi-cursor mode](features/multicursor.md) | Helix/Sublime-style multi-editing: drop N cursors in a dedicated placement mode, then have motions, operators, visual mode, and insert all act on every cursor at once. |

## UI surfaces

| Feature | What it is |
| --- | --- |
| [UI primitives](features/ui-primitives.md) | A layered toolkit for plugin UIs — a Vue-shaped reactive component model (`nx.component`), plugin-owned content surfaces (`nx.view`), ready-made async widgets (`nx.ui` input/confirm/select), and floating windows — all server-owned and sharing one geometry vocabulary. |
| [Permanent docks](features/docks.md) | VSCode-style editable edge panels (file tree, terminals, problem lists) that are global across tabs, toggle independently of windows, and carry their own tabs and options. |
| [Fuzzy picker](features/picker.md) | `nx.picker`: a server-owned fuzzy finder with streaming Lua sources, live (`dynamic`) sources, a file/location preview pane, and fully rebindable keys. |
| [Quickfix & location-list dock tabs](features/quickfix-dock-lists.md) | Quickfix/location lists open as bottom-dock tabs by default (`'qfdock'`) — several searches side by side, entries jumping into the main area — with the `nx.qf.{send,add}_to_{loc,qf}list` sinks and the picker's `<C-q>` / `<Tab>` multi-select. |

## Platform

| Feature | What it is |
| --- | --- |
| [Native `nx.*` plugin API](nx-model.md) | nxvim's own Lua API where the server owns every UI surface and plugins provide data and behavior. |
| [Browser editor](browser-editor.md) | The full editor — core, the Lua VM, and the server tick — compiled to WebAssembly, running entirely client-side with no server. |
| [Edit-host split (remote editing)](edit-host-split.md) | Edit on a remote machine with zero typing lag: the editor and Lua run locally while an `nxvim --daemon` serves the filesystem, processes, and watching over ssh or QUIC. |
| [Lua plugin testing](plugin-testing.md) | `nx.test` (describe/it/expect + async) plus a `nxvim --test-plugin` runner, so pure-Lua plugins test themselves. |
