# Remote config & plugins

In an edit-host (daemon) session, nxvim runs the **daemon's** config and plugins, not
the client's. They are fetched over the wire (one `config_bundle` request),
materialized into a local per-process cache (`$XDG_CACHE_HOME/nxvim/remote/<pid>`), and
run locally — Lua's synchronous `require`/runtimepath can't await the network, so the
files must be local, but the source of truth is the remote.

This directory is a runnable remote config that announces itself loudly so you can see
the mechanism working.

## Run it on one machine (local two-process split)

`--connect-daemon` spawns `nxvim --daemon` as a child over stdio; the child inherits
`NXVIM_CONFIG`, so it serves this directory:

```sh
NXVIM_CONFIG=examples/remote-config \
  cargo run -p nxvim -- --connect-daemon examples/remote-config/sample.txt
```

## Run it across machines (real remote over SSH)

Put this directory at `~/.config/nxvim` on the **remote** host, then from the local one:

```sh
NXVIM_DAEMON_CMD='ssh your-host nxvim --daemon' \
  cargo run -p nxvim -- --connect-daemon
```

Only fs/process/LSP — and this config fetch — cross the wire; the keystroke path stays
local.

## Verify the remote config is live

- A startup notification: *"loaded init.lua + plugins fetched from the daemon"*.
- `:RemoteHello` — a command from the remote `init.lua`.
- `:RemotePlugin` — a command from a plugin the daemon served (`pack/demo/start/…`).
- `:set tabstop?` → `7` (set by the remote `init.lua`).
- `:lua nx.notify(_G.REMOTE_GREETING)` — proves `require` resolved a module from the
  remote `lua/` tree.

## What's here

```
init.lua                                  the remote config (option, command, autocmd)
lua/remote_mod.lua                        a require-able module (fetched too)
pack/demo/start/remote-demo/plugin/…      a packaged plugin (fetched + sourced)
sample.txt                                a buffer with the checklist above
```

Native artifacts (`.so`/`.dylib`/`.dll`) are deliberately **not** fetched — tree-sitter
parsers and the like are compiled locally on the client, where they match its arch.
Instead, the client learns which parser languages the remote had installed and
**compiles them locally on demand** — the first time you open a buffer of a given type,
its parser is built in the background (via the same path as `:TSInstall`), if the remote
had it and you don't already. So a remote session gets highlighting for the same
languages, lazily, without copying incompatible binaries.
