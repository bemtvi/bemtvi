# Autocommand events

These are the events the nxvim editor currently **emits**. Register a handler for
one with `nx.autocmd.create` (or `vim.api.nvim_create_autocmd`). The event name is
matched exactly, and a handler's `callback` receives the event table
`{ id, event, match, buf, file, data }`.

> **The list is the emitted set, not all of vim's events.** nxvim fires events as
> features come to need them. A handler registered for an event that isn't emitted
> yet (e.g. `OptionSet`) is accepted but simply never
> fires — it does not error. If you need one that's missing, it's a feature gap, not
> a config mistake.

Patterns support shell globs: a `pattern` such as `"*.rs"` (no `/`) matches a file
event by the path tail, one containing a `/` matches the whole path, and `"*"` (or
an omitted pattern) matches everything. A pattern with no glob metacharacter is an
exact compare — so a `FileType rust` autocmd never glob-matches a path.

## Buffer lifecycle

| Event | When it fires | Notes |
| --- | --- | --- |
| `BufReadPost` | A file-backed buffer is first shown after reading an existing file from disk. | Fires **once** per buffer (gated by the "announced" set). `buf` / `file` set. |
| `BufNewFile` | A buffer is opened for a path with **no file on disk** — fires instead of `BufReadPost`. | `buf` / `file` set. |
| `FileType` | A buffer is first announced **and** whenever its filetype changes. | `match` is the filetype (e.g. `"rust"`); `file` is the path. Where ftplugins and `vim.lsp.enable` attach. |
| `BufEnter` / `BufLeave` | A buffer becomes / stops being the current one (including plain switches with no read). | `buf` / `file` set. |
| `BufDelete` | Just before a buffer is deleted (`:bdelete`), while its state still exists. | `buf` / `file` set. |

Ordering on opening a file is `BufReadPost` (or `BufNewFile` for a new path) → `FileType` → `BufEnter`.

## Writing

| Event | When it fires | Notes |
| --- | --- | --- |
| `BufWritePre` | Before a buffer is written to disk (`:w`, `:wall`, and finalized off-tick saves). | `match` / `file` is the path; glob-matchable, e.g. a `*.rs` format-on-save hook. |
| `BufWrite` | Same point as `BufWritePre` — the bare-name spelling. | |
| `BufWritePost` | After a successful write. | The hook format-on-save and "reload affected tools" plugins use. |

## Window & tab

| Event | When it fires |
| --- | --- |
| `WinNew` | A new window is created. |
| `WinEnter` / `WinLeave` | Focus moves to / away from a window. |
| `WinClosed` | A window is closed. |
| `WinResized` | A window's rectangle changes (split/resize/layout change). |
| `WinScrolled` | A window's viewport scrolls — `topline` (vertical) or `leftcol` (horizontal) changes. `match` is the scrolled window's id; fires per-window. **Gated on a registered handler** (high-frequency), so it costs nothing when nothing listens. |
| `TabNew` | A new tab page is created. |
| `TabEnter` / `TabLeave` | The active tab changes. |
| `TabClosed` | A tab page is closed. |

## Mode

| Event | When it fires |
| --- | --- |
| `InsertEnter` / `InsertLeave` | The editor enters / leaves Insert (or Replace) mode. |
| `ModeChanged` | The reported `mode()` code changes. `match` is the transition `old:new` (e.g. `"n:i"`, `"v:n"`); a handler's `pattern` glob-matches it (`"*:i"`, `"n:*"`, `"*:*"`). A Normal↔MultiCursor swap (both report `"n"`) is silent. **Gated on a registered handler.** |

## Editing & cursor

These fire at high frequency, so they are **gated on a registered handler** — when
no autocmd listens for them they cost nothing.

| Event | When it fires |
| --- | --- |
| `TextChanged` | The buffer's text changes in Normal mode (edit, paste, …). |
| `TextChangedI` | The buffer's text changes in Insert mode (per keystroke). |
| `CursorMoved` | The cursor moves in Normal mode. |
| `CursorMovedI` | The cursor moves in Insert mode. |

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
