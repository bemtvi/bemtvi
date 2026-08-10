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

## Hot-path events are synchronous

Events fall into two classes, and it decides whether your handler may be async.

**Hot-path** events fire while the editor converges a single input tick — that is, on
nearly every keypress:

```
CursorMoved   CursorMovedI   TextChanged   TextChangedI   ModeChanged
InsertEnter   InsertLeave    BufEnter      BufLeave
WinEnter      WinLeave       WinScrolled   WinResized
```

Their handlers **must be synchronous**. Returning a promise from one raises, naming
the event and the file:line you registered it at. The editor will never wait for it,
so returning one means expecting an ordering that cannot happen.

This is not a ban on async *work* — only on handing back the promise. Start it and
return nothing:

```lua
-- WRONG: raises. Nothing awaits this.
nx.autocmd.create("CursorMoved", { callback = function()
  return nx.lsp.buf.hover()
end })

-- Right: fire-and-forget, or defer with nx.schedule / nx.on_next_tick.
nx.autocmd.create("CursorMoved", { callback = function()
  nx.lsp.buf.hover()
end })
```

Every **other** event is async-capable: a `callback` may return a promise
(`nx.promise`, or the result of an `nx.*` async call). A returned promise is never
dropped — a rejection surfaces like any unhandled `nx.promise` rejection, named for the
event that raised it.

The split exists so the machinery below never touches the per-keypress path.

## What happens when a handler is async

Two guarantees, both reproducing for async what vim gets for free by being
synchronous.

**Late subscribers still get the event.** If a handler registers *another* handler for
the same event while the first is still running, the newcomer receives that event too.
This is what makes a lazily-loaded plugin work: `FileType` wakes the plugin, the
plugin's `config` runs (possibly `nx.await`-ing), it registers its own `FileType`
handler — and that handler still fires for the buffer that woke it. Nothing fires
twice: delivery is filtered by registration order, not replayed wholesale.

**Plugins loading late still get the startup file's events.** This is the same
guarantee widened to the plugin-load boundary — see
[Plugins and the startup file](#plugins-and-the-startup-file).

**The read sequence is ordered.** `BufReadPost` → `FileType` → `BufEnter` →
`BufWinEnter` advances one stage at a time, each waiting for the previous stage's async
handlers to finish. So a `BufReadPost` handler that detects the filetype asynchronously
is reflected in the `FileType` that follows — you get one correct `FileType`, not a
wrong one followed by a correction. This is vim's order, and an async handler does not
reorder it: `BufEnter` and `BufWinEnter` stay behind the read, however long it takes.
`BufWritePre` (the write waits — see [Writing](#writing)) and
`QuitPre` / `ExitPre` / `VimLeavePre` (the exit waits — see
[Quitting & exit](#quitting--exit)) are awaited the same way.

**Handlers have a time budget: 500 ms by default.** It bounds how long the editor
*waits*, never whether anything gets delivered — when it expires, the sequence advances
and late subscribers are still served, so one slow handler cannot wedge a buffer
half-initialised. Blowing it warns, naming the handler's file:line; finishing late
warns again with the elapsed time. Raise it for a handler you know is slow:

```lua
nx.autocmd.create("FileType", { timeout = 5000, callback = function() … end })
```

A handler that never settles at all never reports completion, so it stays listed in
`nx.autocmd.pending()` — inspect with
`:lua print(vim.inspect(nx.autocmd.pending()))`. Warnings go to `:messages`.

## Buffer lifecycle

| Event | When it fires | Notes |
| --- | --- | --- |
| `BufAdd` | A buffer is added to the buffer list — before its `BufReadPost` (a file open into a fresh buffer adds it, then reads it). | Fires with the *added* buffer as `<afile>` (`buf` / `file`), so a `:badd` that never enters the buffer still carries it. The startup buffer never fires it (it is the baseline, like `WinNew`/`TabNew` skip the initial window/tab); only buffers created **after** startup do. `BufCreate` is an accepted alias (see [Event aliases](#event-aliases)). |
| `BufReadPost` | A file-backed buffer is first shown after reading an existing file from disk. | Fires **once** per buffer (gated by the "announced" set). Fires for **every** buffer that lands in a window, not only the focused one — so a session/workspace restore announces each restored file rather than leaving background splits uninitialised until you focus them. `buf` / `file` set. `BufRead` is an accepted alias (see [Event aliases](#event-aliases)). A handler registered while the plugins were still loading is **replayed** the reads that happened before it — see [Plugins and the startup file](#plugins-and-the-startup-file). |
| `BufNewFile` | A buffer is opened for a path with **no file on disk** — fires instead of `BufReadPost`. | `buf` / `file` set. |
| `FileType` | A buffer is first announced **and** whenever its filetype changes. | `match` is the filetype (e.g. `"rust"`); `file` is the path. Where ftplugins and `vim.lsp.enable` attach. On the first announce it is ordered behind `BufReadPost`'s handlers, including async ones — see [What happens when a handler is async](#what-happens-when-a-handler-is-async). |
| `BufEnter` / `BufLeave` | A buffer becomes / stops being the current one (including plain switches with no read), and `BufEnter` again on a **re-read of the buffer that is already current** (`:e!`) — a reload re-enters what it re-read. | `buf` / `file` set. Hot-path, so handlers must be synchronous. `BufLeave` fires **before** the incoming buffer is read, `BufEnter` after: a switch is `BufLeave` → `BufReadPost` → `FileType` → `BufEnter`, so the outgoing buffer's teardown always precedes the incoming one's setup. On a buffer's first announce `BufEnter` is ordered last, after `BufReadPost` and `FileType` have settled. A reload fires no `BufLeave` — nothing was left. Restoring a session does **not** fire either for background windows — nothing became current there. |
| `BufWinEnter` | A window *displays* a buffer it wasn't displaying — a switch (`:b`, `:e`), a split onto a file, a window a session/workspace restore fills (including non-current ones the current-buffer events never visit), and a re-read of a displayed buffer (`:e!`). Fires **per window**, so opening a buffer in a second window fires again even though it was already on screen, and again each time you switch back and forth. **Navigation never fires it**: a tab switch, `<C-w>w`, and a no-arg `:split` (which inherits the buffer it was split off) display nothing new. | `buf` / `file` set. **Gated on a registered handler.** Fires **last** on a buffer's first announce — after `BufReadPost`, `FileType` and `BufEnter` have settled, async handlers included. The handler runs **with the window that displayed as the current one** — `nx.win.current()`, `nx.wo`, and the cursor reads all address it, including for a session/workspace restore filling background windows. That context is the mirror one, not a focus change: the editor never moves your cursor to run a handler. So a mutation that binds to "current" only when it drains (`nx.cmd`, feedkeys) **raises** while the two differ, naming the fire — run it outside, or use an explicit-handle API (`nx.wo[win]`, `nx.bo[buf]`, `nx.win.set_cursor`). When the displaying window *is* the focused one — everything you type — nothing is locked. The window context covers the handler's **synchronous** run: past an `await`, an async handler is back in the ordinary context, so capture `local win = nx.win.current()` before the first await and write through `nx.wo[win]`. Every window that displayed the buffer gets its own fire, including ones that displayed it while an async `BufReadPost` was still settling; a window closed before that read completes fires nothing. |
| `BufReadCmd` | A deferred open is about to perform its default read — vim's "replace the default read" hook. A handler that returns `true` **claims** the read: it owns filling the buffer (e.g. the file explorer listing a directory) and the default load is skipped. | `match` / `file` is the path; `args.isdir` says whether it's a directory. **Gated on a registered handler.** |
| `BufDelete` | Just before a buffer is deleted (`:bdelete`), while its state still exists. | `buf` / `file` set. |

Ordering on opening a file is `BufAdd` → `BufReadPost` (or `BufNewFile` for a new path) → `FileType` → `BufEnter` → `BufWinEnter`, with `BufLeave` for the buffer being left ahead of the whole sequence; an async handler does not reorder it — see [What happens when a handler is async](#what-happens-when-a-handler-is-async). Re-reading the current buffer (`:e!`) fires the same sequence minus the `BufLeave` and the `BufAdd` (the bufnr is neither left nor new).

### Plugins and the startup file

Plugins load **asynchronously**. `nx.plugins` awaits a spec's directory before sourcing
it, and a spec's `config` may `nx.await` on its own — so a plugin's `config`, and every
autocmd that config registers, lands several ticks into startup. The file you named on
the command line has already been read by then. Painting before the plugins are up is
deliberate: it is what makes `nxvim file.txt` open instantly.

You do not have to work around it. Every first-announce event fired before the plugins
were ready is **replayed** to the handlers that registered while they were loading, when
`PluginsLoaded` fires:

```
BufReadPost   -> the handlers that exist now (your init.lua's, the built-in
FileType         treesitter / LSP attach) — so the file colours immediately
BufEnter
VimEnter
  <plugin configs run, registering their own BufReadPost / FileType handlers>
PluginsLoaded -> BufReadPost / FileType replayed to exactly those handlers
```

So a plugin registers a plain `BufReadPost` handler and sees the startup file, with the
buffer and the match it was read with. There is no separate event to hook and no sweep
of `nx.buf.list()` to write. Restored session windows are covered the same way.

Three things worth knowing:

- **Nothing fires twice.** Delivery is filtered by registration order, the same
  watermark the async replay above uses. A handler registered *before* the read — one
  from your `init.lua` — receives it on the read and is never replayed to.
- **Only `BufReadPost`, `BufNewFile` and `FileType` are replayed.** They fire once per
  read and carry no pairing semantics, so re-delivering one is unambiguous. `BufEnter`
  and `BufWinEnter` are not: they mean "became current" / "a window displayed it", which may
  no longer be true by the time the plugins land, and a `BufEnter` replayed without its
  `BufLeave` twin would misreport editor state.
- **The window is a startup one.** It closes at `PluginsLoaded` and never reopens, so a
  handler registered later — by a lazy plugin, or by you at the `:` prompt — gets the
  reads that follow it and nothing from before.

## Writing

| Event | When it fires | Notes |
| --- | --- | --- |
| `BufWritePre` | **Before** the buffer is serialized to disk, on `:w` / `:wq` / `:x` / `:wall` / `:wqa`. A handler may mutate the buffer (format-on-save, trim trailing whitespace) and the mutation is what gets written — vim's pre-write contract. A handler may also be **async**: return a promise (e.g. an async LSP format) and the write **waits** for every handler to settle before serializing. | `match` / `file` is the path; glob-matchable (`*.rs`). The buffer is still `modified` here (it is clean by `BufWritePost`). Firing order is `Pre` → *await handlers* → *write* → `Post`, locally and over the daemon/web wire. For `:wall`/`:wqa` each buffer is made current for its own `BufWritePre` (vim's `aucmd_prepbuf`), so a mutating handler targets the right buffer; `:wqa` quits only once every write has committed. A handler whose promise *rejects* does not block the write (a failing formatter still saves). |
| `BufWrite` | Same point as `BufWritePre` — the bare-name spelling. | An alias for `BufWritePre` (see [Event aliases](#event-aliases)); handlers on both spellings fire once. |
| `BufWritePost` | After a successful write. | The hook "reload affected tools" plugins use. |

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
| `ModeChanged` | The reported `mode()` code changes. `match` is the transition `old:new` (e.g. `"n:i"`, `"v:n"`); a handler's `pattern` glob-matches it (`"*:i"`, `"n:*"`, `"*:*"`). A Normal↔MultiCursor swap fires `"n:m"` / `"m:n"` (MultiCursor reports its own `"m"`). **Gated on a registered handler.** |

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
| `LspProgress` | A server reports on long-running work (`$/progress`) — indexing, loading a workspace. | The **pattern is the kind** (`begin` / `report` / `end`), so `pattern = "end"` narrows to completions. `data = { client_id, token, kind, title, message, percentage, cancellable }`, with `nil` for a field the server didn't send. Read the settled state with `nx.lsp.progress()` rather than accumulating updates yourself. |

## Files & environment

| Event | When it fires | Notes |
| --- | --- | --- |
| `FileChangedShell` | A loaded file changed on disk (the watch/`checktime` reconcile). | A handler may set `vim.v.fcs_choice` to `"reload"` / `"edit"` / `"ask"`. |
| `FileChangedShellPost` | After the file-change reconcile completes. | |
| `DirChanged` | The working directory changes (`:cd` / `:lcd` / `:tcd`). | `file` is the new cwd; `match` is the scope. |
| `EncodingChanged` | The current buffer's `'fileencoding'` is changed in place (`:set fileencoding=…`). | `match` is the new encoding label (e.g. `"latin1"`); `buf` / `file` set. **Reading** a file (opening it, or an `:e ++enc=…` reload) only (re)seeds the baseline from the detected encoding — that is not a *change*, so it fires nothing (neovim, whose global `encoding` is fixed, likewise fires nothing on read). `FileEncoding` is an accepted alias (see [Event aliases](#event-aliases)). |
| `ColorScheme` | A colorscheme finishes loading. | `match` is the colorscheme name. This is what colorscheme plugins hook. |

## Startup

| Event | When it fires | Notes |
| --- | --- | --- |
| `VimEnter` | Once, after the editor has finished starting (config sourced, first frame imminent). | `vim.v.vim_did_enter` is `1` from this point on. The built-in plugin manager's first-run prompt hooks it. |
| `UIEnter` | Each time a client attaches its UI — after `VimEnter`, since startup completes before any client connects. | `nx.ui.caps()` describes the client that just attached (`keyboard_protocol` / `truecolor` / `osc52`) and is refreshed immediately before this fires. Hook it for setup that depends on what the terminal can do — notably a `<C-h>` / `<C-i>` / `<C-m>` / `<C-[>` mapping, which is only distinguishable from `<BS>` / `<Tab>` / `<CR>` / `<Esc>` under the kitty keyboard protocol. Fires again on a daemon re-dial (a second attach), so a handler must be idempotent. |

## Quitting & exit

Fired when the editor is really leaving — a committed `:qa` / last-window `:q` / `:wq` /
`:x` / `:wqa` (`!` skips only the modified-buffer `E37` guard, not these events). The
three `*Pre` events are **awaited**: a handler may return a promise and the exit waits
for every handler to settle before advancing, so an async flush/cleanup runs before the
process leaves. They fire in order.

| Event | When it fires | Notes |
| --- | --- | --- |
| `QuitPre` | First, once the quit is committed (the `E37` guard has passed or `!` bypassed it). | **Awaited.** A handler's returned promise holds the exit until it settles. Unlike neovim, `QuitPre` does not fire when `:q` merely closes one of several windows (the editor keeps running) — only on the real editor exit. |
| `ExitPre` | After `QuitPre` settles — "the editor is really leaving". | **Awaited.** The natural place for an async flush before quitting. |
| `VimLeavePre` | After `ExitPre` settles — the last hook before session state (shada) is written. | **Awaited.** Modify anything you want persisted from here. |
| `VimLeave` | After `VimLeavePre` settles, immediately before the editor exits. | **Not** awaited (nothing remains to wait for) — a returned promise is tracked but the exit does not block on it. Post-cleanup only. |

A handler cannot *cancel* the quit (these events are advisory, as in neovim), and a
handler whose promise *rejects* does not block the exit — a failing cleanup still lets
the editor leave.

## Plugins

Fired by the built-in plugin manager (`nx.plugins`). See [Writing nxvim plugins](plugin-authoring.md).

| Event | When it fires | Notes |
| --- | --- | --- |
| `PluginsLoaded` | Once, after **every eager (non-lazy) plugin declared by your config has fully loaded and settled** — its `plugin/` scripts sourced and its `config` run, an async `config` awaited to completion. Gated on `VimEnter`, so it never fires before startup finishes. | The "all my plugins are ready" hook — run setup that depends on several eager plugins here. Fires once; a plugin a later `:PluginSync` installs still emits its own `PluginLoaded` but does not re-fire this. Lazy plugins are **not** waited for. It is also the point the startup announce window closes — see [Plugins and the startup file](#plugins-and-the-startup-file). |
| `PluginLoaded` | Each time **any one plugin** finishes loading — eager at startup, or lazy the moment its `cmd`/`event`/`ft`/`keys` trigger loads it. | `match` (and `data.name`) is the plugin name, so `nx.on("PluginLoaded", { pattern = "my-plugin" }, …)` hooks just that plugin's load. |

To hook one named plugin, prefer **`nx.plugins.on_loaded(name, fn)`** over subscribing to
`PluginLoaded` yourself. The event only reports a load that happens *later*, so the raw
subscription silently never runs if that plugin turns out to be eager and already loaded;
`on_loaded` runs `fn` immediately in that case and waits otherwise, so it fires exactly
once either way:

```lua
nx.plugins.on_loaded("nxvim-lspconfig", function()
  require("nxvim-lspconfig").setup()
end)
```

## User

`User` is the freeform event namespace: any plugin (or nxvim itself) fires one with
`nx.autocmd.exec("User", { pattern = "MyEvent", data = … })`, and a handler
subscribes by `pattern`. nxvim fires `User DaemonStatusChanged` whenever a daemon
session's connection phase changes (read the phase with `nx.daemon.status()`).

## Event aliases

A handful of neovim event names are aliases for another event. nxvim honors these
by **canonicalizing the alias to its real event at registration** — so a config
that does `nx.autocmd.create("BufRead", …)` (muscle memory from neovim) behaves
exactly as if it had used `"BufReadPost"`, and the callback's `event` field reports
the canonical name. Aliases are accepted anywhere an event name is (`create`,
`exec`, `get`, `clear`).

| Alias | Canonical event |
| --- | --- |
| `BufRead` | `BufReadPost` |
| `BufWrite` | `BufWritePre` |
| `BufCreate` | `BufAdd` |
| `FileEncoding` | `EncodingChanged` |

Every alias's target event is one nxvim actually emits — registering on an alias
whose target never fired would be a silent no-op, so we don't add one until the
target exists.
