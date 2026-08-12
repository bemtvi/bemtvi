# The edit-host split

Editing on another machine the usual way — running the whole editor over there and a
thin client here — means every keystroke round-trips the network before the cursor
moves. That lag is structural, not tunable: the editing state machine lives on the
far side of the wire.

The **edit-host split** moves the network boundary *below* the editor instead. The
editor and Lua run **locally**; only an **`bemtvi --daemon`** serving the filesystem,
processes, and file-watching runs on the remote. So typing, motions, operators, and
undo are all local — **zero round-trips** — and only fs / process / watch / LSP
traffic crosses the wire, the work that was always going to feel like a spinner
anyway. It's the direction VS Code Remote takes: the text model is local, the heavy
I/O is remote.

The same `bemtvi --daemon` serves the [Browser editor](browser-editor.md) too — one
remote, reached natively here or from a browser tab over WebTransport.

## The two halves

```
editor + Lua  (LOCAL, your machine)  ──network──▶  bemtvi --daemon = fs + processes + watch  (REMOTE)
```

- **The local half** is the same embedded editor as a normal `bemtvi`, with its host
  seams — `HostFs`, `HostProc`, the async `HostFsAsync`, the LSP transport, and the
  Lua-facing `LuaFs` — pointed at the daemon instead of the local disk.
- **The remote half** is `bemtvi --daemon`: it reads and writes the remote machine's
  files, spawns its processes, and watches its files. Nothing about editing lives
  there.

## Running it

Two transports reach a daemon — **ssh** (simple) and **QUIC** (multiplexed, and the
same transport the browser uses).

### Over ssh (stdio)

Point a local editor at a daemon spawned over ssh:

```sh
BEMTVI_DAEMON_CMD="ssh user@host bemtvi --daemon" bemtvi --connect-daemon path/to/file
```

`BEMTVI_DAEMON_CMD` is run through `sh -c`, so any command line ending in
`bemtvi --daemon` over stdio works. Left unset, `--connect-daemon` spawns this same
binary locally (`bemtvi --daemon`) — a two-process local split, handy for testing.

In the **GUI**, do it at runtime with `:connect [user@]host[:port][/file]` — it spawns
`ssh … bemtvi --daemon` and routes any password / passphrase prompt to a native dialog
via `SSH_ASKPASS`, so it works from a windowed launch with no tty.

### Over QUIC / WebTransport

Run a standalone listener on the remote — the same `wtransport`-on-`quinn` stack the
browser dials:

```sh
bemtvi --daemon --listen                 # binds 127.0.0.1:8765, prints a connect URI
bemtvi --daemon --listen 0.0.0.0:9000    # accept off-host connections
```

It prints a connect URI — `bemtvi://HOST:PORT/TOKEN?cert=HASH`. Dial it from a local
editor (the `bemtvi://…` scheme selects the QUIC path; `--connect-daemon` is optional):

```sh
bemtvi bemtvi://HOST:PORT/TOKEN?cert=HASH path/to/file
```

In the **GUI**, `:connect bemtvi://…` does the same at runtime.

## What crosses the wire (and what doesn't)

| Stays **local** (zero round-trips) | Goes **remote** (over the wire) |
| --- | --- |
| Every keystroke, motion, operator, undo | Opening / saving files and the file explorer |
| The Lua VM and the redraw | Processes — `btv.run` / `btv.run_stream` / `btv.process` / `:terminal` |
| Your config + shada *(default; see below)* | File-watching — `:checktime` / `'autoread'` / `FileChangedShell` |
| | LSP requests |

Only the things that were always going to feel like a spinner cross the wire.

### Local or remote config (and shada)

By default a daemon session runs your **local** config and keeps shada (marks /
registers / history) local — only I/O crosses the wire. Pass **`--remote-config`** to
run the *daemon's* config + plugins instead: the daemon's config is fetched over the
wire (one `config_bundle` request, materialized into a per-process cache and run
locally, since Lua's synchronous `require` can't await the network), and shada follows
the same choice — a remote-config session keeps its shada **on the daemon**, so a
remote workspace's editor state travels with it. The browser client has no local disk,
so it is **always** remote-config. See [`examples/remote-config`](../examples/remote-config).

### The split-brain filesystem (for Lua)

One subtlety the split forces: *which* filesystem does Lua see? bemtvi splits it on
purpose. **Project-facing** fs APIs route to the daemon — the async `btv.fs` surface
(`btv.fs.read` / `btv.fs.readdir` / `btv.fs.stat` / `btv.fs.watch` / …), root detection,
a picker's previewer, a
VCS-status provider — so they see the **project** on the remote. Config and shada stay
**local** by default, or move to the daemon with `--remote-config` (see above). (This
is the `LuaFs` seam.)

**Plugin management is always local.** The `btv.plugins` manager clones, discovers, and
sources plugins on the **local disk** in every session — even a daemon / `--remote-config`
one — because a plugin loads into the *local* Lua VM (its dir is added to the local
runtimepath and `require`d). So `:PluginSync` downloads into the local
`stdpath('data')/plugins`, and a `dir=` plugin is read locally; a git clone runs the local
`git`, never the daemon's. (A loaded plugin's *own* runtime `btv.fs` / `btv.run` still route
to the daemon — it edits the remote's files.) This is separate from the `pack/*/start`
plugins a `--remote-config` session fetches in the `config_bundle`: those come *from* the
daemon and are materialized into the local cache alongside the config. Either way, plugin
code ends up on the local disk where the VM can load it.

## Staying connected (auto-reconnect)

Because the editor runs **local** and only the seams cross the wire, a dropped
connection — a laptop sleeping past the QUIC idle timeout, an ssh hop dropping, a network
blip — does **not** tear the session down. The link **re-dials underneath the seam
handles the editor already holds**: your buffers, undo, cursor, windows and Lua state are
untouched, and the transport beneath them reconnects. While the link is down, remote ops
(save, read, LSP, watch, terminal) fail *loud* rather than hanging.

- **Automatic.** A drop is retried a few times with backoff (≈0.5 → 8 s). On success the
  seams rebind and editing resumes with no action from you. If the budget is spent the
  link parks **disconnected** and tells you to run `:reconnect`.
- **`:reconnect`** re-dials now (and resets the retry budget); **`:disconnect`** drops the
  link on demand. Both are server-side ex-commands, so they work on the TUI too.
- **Status** is a first-class API: `btv.daemon.status()` returns `"connected"` /
  `"reconnecting"` / `"disconnected"` (or `nil` for a local session), and a
  `User DaemonStatusChanged` autocmd fires on every change — so a statusline component can
  render it (green / yellow / red). See [`examples/daemon-status`](../examples/daemon-status).
- **On reconnect** the editor re-syncs what a *fresh* daemon doesn't know about: it
  re-opens LSP servers, re-arms file watches, and **re-stats** open files — a file changed
  while the link was down is detected (an unmodified buffer autoreloads; an edited one is
  flagged as a conflict). Remote terminals/jobs do not survive a drop (the PTYs die with
  the link) and are surfaced as exited; reopen them with `:terminal`.

All three transports reconnect — ssh/stdio, QUIC, and the browser's WebTransport. (Daemon-
*side* session survival — keeping terminals/jobs alive across a drop — is out of scope.)

## Auth & identity (QUIC)

A daemon executes arbitrary processes, so an open listener is **remote code execution
by design** — the TLS cert buys encryption, not authorization. Two mechanisms, both
minted at `--daemon --listen` launch and presented by the local half on connect:

- **Bearer token** — 32 random bytes, carried on the connect URI's path. The daemon
  compares it constant-time and **drops the session without accepting** on a mismatch,
  so a bad token is a failed connect, never a half-open session.
- **Server identity** — a self-signed cert whose SHA-256 hash the daemon prints; the
  client **pins that hash on first use** (the known-hosts / TOFU model, no CA). The
  browser passes the same hash to its `WebTransport` constructor.

The default loopback bind (`127.0.0.1`) is defense-in-depth — the token is the real
gate. Bind `0.0.0.0` only when the token (and, ideally, a trusted network) is doing
its job.

## How it works (in brief)

- **One code path.** The local half is the *same* embedded editor as the default
  role; the boundary is the same RPC every client speaks. Only its host seams
  (`HostFs` / `HostProc` / `HostFsAsync` / the LSP transport / `LuaFs`) are repointed
  at the daemon — the editing logic is untouched.
- **ssh vs. QUIC.** ssh stdio carries every leg over one ordered stream — simple, but
  a process flood (a fuzzy-finder's `rg`, an `npm install`) can head-of-line-block a
  file save queued behind it. QUIC gives each traffic class its own stream, removing
  that coupling at the protocol level: the legs ride four independently flow-controlled
  bidi streams by latency class — Control (`fs` / `config` / `btv.fs`), Proc, Lsp, Term —
  each prefixed with a one-byte group tag the daemon dispatches on. Because it's the same
  `wtransport`/`quinn` stack the browser's WebTransport uses, native and browser **unify
  on one daemon** (the browser opens the same four streams). See
  [the multi-stream plan](plans/2026-06-26-multi-stream-daemon-transport.md).
- **`:connect` swaps the backend, not the window.** In the GUI, `:connect` builds a
  *fresh local server* whose seams point at the daemon and re-attaches the same window.
  The editor transport is always the in-process duplex, so the window never notices.

## See also

- [Browser editor](browser-editor.md) — the same daemon reached from a browser tab.
- [Architecture → Embedded vs. remote](architecture.md#embedded-vs-remote) — where this
  sits in the client–server model.
- [The edit-host & browser-Lua plan](plans/2026-06-09-edit-host-and-browser-lua.md) —
  the design and its phased implementation.
