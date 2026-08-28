# Beyond vim — what bemtvi adds

bemtvi speaks vim at the keyboard: keystrokes, modes, ex-commands, and options
track [vim/neovim](https://neovim.io)'s observable editing behavior. On top of
that baseline it grows a handful of features vim and neovim don't have natively —
modern editing and UI surfaces that fit the modal grammar rather than fighting
it.

This page is the index. Each feature below has a full guide linked from its name;
the one-liner is the elevator pitch.

## Editing

| Feature | What it is |
| --- | --- |
| [Helix mode (selection-first)](features/helix-mode.md) | An opt-in selection-first editing model (`:helix`): noun→verb on a persistent `anchor..head` range, multi-selection as the default. Motions re-select, verbs act now with no operator-pending wait — match mode, surround, regex-select, and per-selection everything. |
| [Multi-cursor mode](features/multicursor.md) | Helix/Sublime-style multi-editing: drop N cursors in a dedicated placement mode, then have motions, operators, visual mode, and insert all act on every cursor at once. |
| [Keyboard macros](features/macros.md) | vim's record-and-replay on bemtvi's keys: `<F2>{reg}` records what you *typed* (mappings included, so a Lua keymap replays), `{count}<F3>{reg}` plays it back, and a failed keystroke ends the run — so `99<F3>a` stops at the end of the buffer. The macro is a plain register holding readable key notation. |
| [Expressions](features/expressions.md) | Hand bemtvi a small Lua expression where a fixed rule is not enough: `:s/…/\=…/` computes each replacement, `'foldexpr'` decides fold levels, and `btv.fold.text` / `btv.indent.expr` / `btv.filetype.detect` / `btv.picker.scorer` answer the questions the core will not guess at. They run in a stateless, deadline-bounded second Lua VM. |
| [Smooth scrolling](features/smooth-scrolling.md) | Viewport scrolls slide instead of teleporting (neoscroll.nvim built in), interpolated client-side so it stays smooth even over a remote link. On by default. |
| [Indent detection](features/indent-detection.md) | A file's own indentation sets its `'expandtab'` / `'shiftwidth'` on every read (vim-sleuth built in), per buffer, on every leg. On by default (`'indentdetect'`). |
| [Image previews](features/image-previews.md) | Open an image file and the picture renders inline — ratatui-image in the terminal, a GPU texture in the GUI, an `<img>` in the browser. |

## UI surfaces

| Feature | What it is |
| --- | --- |
| [UI primitives](features/ui-primitives.md) | A layered toolkit for plugin UIs — a Vue-shaped reactive component model (`btv.component`), plugin-owned content surfaces (`btv.view`), ready-made async widgets (`btv.ui` input/confirm/select), and floating windows — all server-owned and sharing one geometry vocabulary. |
| [Permanent docks](features/docks.md) | VSCode-style editable edge panels (file tree, terminals, problem lists) that are global across tabs, toggle independently of windows, and carry their own tabs and options. |
| [Fuzzy picker](features/picker.md) | `btv.picker`: a server-owned fuzzy finder with streaming Lua sources, live (`dynamic`) sources, a file/location preview pane, and fully rebindable keys. |
| [Quickfix & named-list dock tabs](features/quickfix-dock-lists.md) | The quickfix list and named lists open as bottom-dock tabs by default (`'qfdock'`) — several searches side by side, entries jumping into the main area — with the `btv.qf.*` sinks and the picker's `<C-q>` / `<Tab>` multi-select. Location lists keep vim's split behavior. |

## Platform

| Feature | What it is |
| --- | --- |
| [Native `btv.*` plugin API](btv-model.md) | bemtvi's own Lua API where the server owns every UI surface and plugins provide data and behavior. |
| [Workspaces](features/workspaces.md) | The VSCode "open a folder" model: `--workspace <dir>` opens a directory as a persistent project session, restoring its layout/tabs/buffers and carrying per-workspace option overrides (`btv.wso`). |
| [Browser editor](browser-editor.md) | The full editor — core, the Lua VM, and the server tick — compiled to WebAssembly, running entirely client-side with no server. |
| [Edit-host split (remote editing)](edit-host-split.md) | Edit on a remote machine with zero typing lag: the editor and Lua run locally while an `bemtvi --daemon` serves the filesystem, processes, and watching over ssh or QUIC. |
| [Lua plugin testing](plugin-testing.md) | `btv.test` (describe/it/expect + async) plus a `bemtvi --test-plugin` runner, so pure-Lua plugins test themselves. |
