# The clipboard API

Lua could write the clipboard through the register API (`btv.reg.set("+", …)`) but not
read it: `btv.reg.get("+")` read the Lua register mirror, which the server never fills
for `+` / `*`, so it returned only what the same chunk had just written. A plugin
couldn't paste-transform, preview or inspect what you copied in another app.

## Design

The clipboard gets an API of its own, and it is asynchronous everywhere:

- **`btv.clipboard.read()`** returns a promise of the clipboard's text (`""` when it's
  empty).
- **`btv.clipboard.write(text, opts)`** returns a promise that settles once the text is
  on the clipboard. `opts.linewise` marks it as whole lines for a later `"+p`; it
  defaults to whether the text ends in a newline.
- Both reject, with a message, when there is no clipboard provider (or, in the browser,
  when the page may not read it).

**`btv.reg` no longer accepts `+` and `*`.** `btv.reg.get` / `set` / `gettype` (and their
`vim.fn.getreg` / `setreg` / `getregtype` aliases) raise for them, naming
`btv.clipboard`. In vim they are the clipboard; reading the clipboard can't be
synchronous in the browser, and one API that is the same everywhere beats a register
read that works on some platforms and fails on others. The keys are unchanged: `"+y`
and `"+p` still reach the clipboard.

### Why async everywhere

In the browser, `navigator.clipboard` is async and only the page can call it; the
wasm edit-host only keeps a mirror the page refreshes on focus and paste, which can be
stale or empty. Natively the provider answers synchronously, but a plugin written
against a sync read would break in the browser, and the remote session is tier 1.

### How it runs

`btv.clipboard.read` / `write` register a callback id and queue a clipboard op
(`btv._clipboard_op`). The server drains the ops with the Lua effects:

- **Native**: the op runs against the editor's provider right there (the same call
  `"+p` / `"+y` make) and its callback settles through `btv._run_cb`, so the promise
  settles within the same tick.
- **Browser** (phase 2): the op goes through `HostEffects::clipboard_op` to the wasm
  sink, `eh_take_clipboard_ops` hands it to the Worker, which posts it to the page; the
  page calls `navigator.clipboard.readText` / `writeText` and answers
  `clipboard_op_result`; `eh_clipboard_op_complete` refreshes the mirror and settles
  the callback.

## Phases

1. **The API, native.** `btv.clipboard.read` / `write`, the clipboard op and its
   `ClipboardResult` callback, the drain against the provider, `btv.reg` refusing
   `+` / `*`, and the core's `setreg('+')` branch replaced by clipboard read / write
   methods. Callers moved over: the context-menu example and guide (`Copy Path`), and
   bemtvi-tree's `yank_path`. Tests with `FakeClipboard`: a value copied elsewhere reads
   back, a write lands on the provider (and leaves `""` alone), linewise is carried,
   no provider rejects, and `btv.reg` refuses the clipboard names.
2. **Browser.** The wasm round-trip through the effects, the Worker and the page, the
   mirror refresh, rejection on a denied read, a `verify-clipboard.mjs` browser check,
   and `--no-default-features` builds.

## Status

Both phases are in. In the browser a `btv.clipboard` op reaches `navigator.clipboard`
through the page, so a read sees a copy made in another app without the mirror having
been refreshed, and a settled op leaves the mirror holding what it read or wrote (a
linewise write keeps its flag there, so a `"+p` puts it on its own line). The page's
answer rides a ring type-11 frame under the SAB transport (the run loop may be parked
on `Atomics.wait`) and a `clipboard_op_result` message under postMessage.
`verify-clipboard.mjs` covers both transports and a denied read.

bemtvi-tree's `yank_path` awaits `btv.clipboard.write` (bemtvi-tree `18f5339`), and
its spec has a case that presses `y` and checks the clipboard.
