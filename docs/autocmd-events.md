# Autocommand events

These are the events the nxvim editor currently **emits**. Register a handler for
one with `nx.autocmd.create` (or `vim.api.nvim_create_autocmd`). The event name is
matched exactly, and a handler's `callback` receives the event table
`{ id, event, match, buf, file, data }`.

> **The list is the emitted set, not all of vim's events.** nxvim fires events as
> features come to need them. A handler registered for an event that isn't emitted
> yet (e.g. `BufWritePost`, `TextChanged`, `CursorMoved`) is accepted but simply
> never fires — it does not error. If you need one that's missing, it's a feature
> gap, not a config mistake.

## Buffer lifecycle

| Event | When it fires | Notes |
| --- | --- | --- |
| `BufReadPost` | A file-backed buffer is first shown (read from disk). | Fires **once** per buffer (gated by the "announced" set). `buf` / `file` set. |
| `FileType` | A buffer is first announced **and** whenever its filetype changes. | `match` is the filetype (e.g. `"rust"`); `file` is the path. Where ftplugins and `vim.lsp.enable` attach. |
| `BufEnter` | Every time a buffer becomes current (including plain switches with no read). | `buf` / `file` set. |

Ordering on opening a file is `BufReadPost` → `FileType` → `BufEnter`.

## Window & tab

| Event | When it fires |
| --- | --- |
| `WinNew` | A new window is created. |
| `WinEnter` / `WinLeave` | Focus moves to / away from a window. |
| `WinClosed` | A window is closed. |
| `WinResized` | A window's rectangle changes (split/resize/layout change). |
| `TabNew` | A new tab page is created. |
| `TabEnter` / `TabLeave` | The active tab changes. |
| `TabClosed` | A tab page is closed. |

## Mode

| Event | When it fires |
| --- | --- |
| `InsertEnter` | The editor transitions into Insert (or Replace) mode. |

## LSP

| Event | When it fires | Notes |
| --- | --- | --- |
| `LspAttach` | A language server attaches to a buffer. | `data = { client_id = … }`. |
| `LspDetach` | A language server detaches from a buffer. | `data = { client_id = … }`. |

## Files & environment

| Event | When it fires | Notes |
| --- | --- | --- |
| `FileChangedShell` | A loaded file changed on disk (the watch/`checktime` reconcile). | A handler may set `vim.v.fcs_choice` to `"reload"` / `"edit"` / `"ask"`. |
| `FileChangedShellPost` | After the file-change reconcile completes. | |
| `DirChanged` | The working directory changes (`:cd` / `:lcd` / `:tcd`). | `file` is the new cwd; `match` is the scope. |
| `ColorScheme` | A colorscheme finishes loading. | `match` is the colorscheme name. This is what colorscheme plugins hook. |

## Startup

| Event | When it fires | Notes |
| --- | --- | --- |
| `VimEnter` | Once, after the editor has finished starting (config sourced, first frame imminent). | `vim.v.vim_did_enter` is `1` from this point on. The built-in plugin manager's first-run prompt hooks it. |
