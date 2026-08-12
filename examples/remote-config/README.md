# Remote config & plugins (and shada)

In an edit-host (daemon) session, bemtvi can run **either** your local config or the
**daemon's** config and plugins — you choose with `--remote-config`. Native clients
default to **local**; `--remote-config` opts into the daemon's config. (The web client,
which has no local disk, is always remote.) Shada (marks/registers/history) follows the
same choice:

| | config | shada |
| --- | --- | --- |
| *(default)* | your **local** config | **local** (`stdpath('state')/shada`) |
| `--remote-config` | the **daemon's** config + plugins | on the **daemon** (`stdpath('state')/shada/remote-session.shada` there) |

With `--remote-config`, the daemon's config is fetched over the wire (one `config_bundle`
request), materialized into a local per-process cache (`$XDG_CACHE_HOME/bemtvi/remote/<pid>`),
and run locally — Lua's synchronous `require`/runtimepath can't await the network, so the
files must be local, but the source of truth is the remote. The remote shada is staged to
a local redb store that syncs back to the daemon after each flush.

This directory is a runnable remote config that announces itself loudly so you can see
the mechanism working.

## Run it on one machine (local two-process split)

`--connect-daemon` spawns `bemtvi --daemon` as a child over stdio; the child inherits
`BEMTVI_CONFIG`, so it serves this directory. Add `--remote-config` to actually run it
(without the flag you get your *local* config instead):

```sh
BEMTVI_CONFIG=examples/remote-config \
  cargo run -p bemtvi -- --connect-daemon --remote-config examples/remote-config/sample.txt
```

Drop `--remote-config` and the same command runs your own `~/.config/bemtvi` over the
daemon's filesystem — a quick way to see the two modes side by side.

## Run it across machines (real remote over SSH)

Put this directory at `~/.config/bemtvi` on the **remote** host, then from the local one:

```sh
BEMTVI_DAEMON_CMD='ssh your-host bemtvi --daemon' \
  cargo run -p bemtvi -- --connect-daemon --remote-config
```

Only fs/process/LSP — this config fetch, and (with `--remote-config`) the shada sync —
cross the wire; the keystroke path stays local. Set a mark or yank a register, quit, and
reconnect with `--remote-config`: the state comes back from the *remote's* shada, while a
plain (local) session keeps that state on your own machine.

Add `--shada-namespace <project>` to keep each project's remote shada separate on the
daemon (under `stdpath('state')/shada/remote/ns/<project>/` there) — so marks/registers
from one remote workspace don't bleed into another. Without it, a single global remote
store is shared across projects (matching neovim's global-shada semantics).

## Run it from the browser (web build)

The wasm edit-host is born remote the same way (the web client is always remote-config).
Launch a listening daemon serving this directory, then open the web client pointed at it
with `?daemon=<uri>`:

```sh
BEMTVI_CONFIG=examples/remote-config cargo run -p bemtvi -- --daemon --listen 127.0.0.1:0
# copy the printed bemtvi://… URI, then open the web build with ?daemon=<uri>
```

The browser fetches the same `config_bundle` over WebTransport, stages it into its
in-memory FS, and sources the daemon's `init.lua` + plugins — so `require`, the remote
command, and the remote option all work in the browser too (the local OPFS `init.lua` is
skipped: in daemon mode the config surface is entirely the daemon's). See
`crates/bemtvi-edithost/web/verify-remote-config.mjs` for an end-to-end check.

## Verify the remote config is live

- A startup notification: *"loaded init.lua + plugins fetched from the daemon"*.
- `:RemoteHello` — a command from the remote `init.lua`.
- `:RemotePlugin` — a command from a plugin the daemon served (`pack/demo/start/…`).
- `:set tabstop?` → `7` (set by the remote `init.lua`).
- `:lua btv.notify(_G.REMOTE_GREETING)` — proves `require` resolved a module from the
  remote `lua/` tree.

## What's here

```
init.lua                                  the remote config (option, command, autocmd)
lua/remote_mod.lua                        a require-able module (fetched too)
pack/demo/start/remote-demo/plugin/…      a packaged plugin (fetched + sourced)
sample.txt                                a buffer with the checklist above
```

### Two kinds of plugin, two paths

This example uses a `pack/*/start` plugin, which rides the `config_bundle`: it comes
*from* the daemon and is materialized into the local cache alongside the config.

A plugin declared with **`btv.plugins`** (the git-clone manager) is different — it is
**always managed on the local disk**, in every session, even `--remote-config`. That is
deliberate: a plugin loads into the *local* Lua VM (its dir is added to the local
runtimepath and `require`d), so `:PluginSync` clones into your local
`stdpath('data')/plugins` with the local `git`, never onto the daemon. (A loaded plugin's
own runtime `btv.fs` / `btv.run` still route to the daemon — it edits the remote's files.)
So both plugin styles end up on the local disk where the VM can load them; they just
arrive by different routes.

Native artifacts (`.so`/`.dylib`/`.dll`) are deliberately **not** fetched — tree-sitter
parsers and the like are compiled locally on the client, where they match its arch.
Instead, the client learns which parser languages the remote had installed and
**compiles them locally on demand** — the first time you open a buffer of a given type,
its parser is built in the background (via the same path as `:TSInstall`), if the remote
had it and you don't already. So a remote session gets highlighting for the same
languages, lazily, without copying incompatible binaries.
