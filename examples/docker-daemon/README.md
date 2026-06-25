# Remote daemon in a container (podman / docker)

nxvim can split itself in two: a thin local **edit-host** (your keystrokes, the UI)
talking to a remote **daemon** that owns the filesystem, processes, file watches and
language servers. This example puts that daemon in a container and connects to it
from your machine over the native transport — **QUIC** (so: UDP).

The point of the example is the **config swap**: a daemon session runs the
*daemon's* config, fetched over the wire, **not** your local one. Two deliberately
different configs make that visible:

| | local launch | daemon (container) session |
|---|---|---|
| config | `local/init.lua` | `daemon/init.lua` (baked into the image) |
| `:WhoAmI` | `LOCAL config …` | `DAEMON config …` |
| `:set tabstop?` | `2` | `8` |
| filesystem | your machine | the container |

> Native mode only for now — this is the QUIC daemon, not the browser/WebTransport
> edit-host. Same wire, different packaging.

## What's here

```
Containerfile        builds the release binary, bakes in daemon/ + workspace/, runs --daemon --listen
compose.yaml         podman-compose / docker compose front-end for the same
connect.sh           reads the daemon's connect URI from the container logs and launches the client
local/init.lua       the config used when you run nxvim normally (no daemon)
daemon/init.lua      the config the container serves over the wire
daemon/lua/whereami.lua   a require-able module, fetched too (proves the whole runtimepath crosses)
workspace/sample.txt the file you edit — it lives on the daemon (in the container)
```

## 1. See the local config first (baseline, no container)

```sh
NXVIM_CONFIG=examples/docker-daemon/local \
  cargo run -p nxvim -- examples/docker-daemon/workspace/sample.txt
```

`:WhoAmI` → *LOCAL config*, `:set tabstop?` → `2`. This is the embedded server: one
process, everything on this machine.

## 2. Start the daemon container

From the **repo root** (the build context is the whole workspace):

```sh
podman build -f examples/docker-daemon/Containerfile -t nxvim-daemon .
podman run --rm -d -p 127.0.0.1:8765:8765/udp --name nxvim-daemon nxvim-daemon
```

or with compose:

```sh
podman compose -f examples/docker-daemon/compose.yaml up --build -d
```

Swap `podman` → `docker` throughout if that's what you have (`CONTAINER_CLI=docker`
for the connect script). **The published port must be `/udp`** — QUIC runs on UDP.

The daemon prints its connect URI to stdout at startup; you can see it with
`podman logs nxvim-daemon`:

```
nxvim daemon listening on 0.0.0.0:8765
  connect with: nxvim --connect-daemon 'nxvim://0.0.0.0:8765/<token>?cert=<hash>'
```

The `<token>` (a 32-byte bearer secret) and `<hash>` (the self-signed cert, pinned
TOFU on first use) are minted fresh on every launch — together they are the auth
gate, which is why the listener can safely bind `0.0.0.0` inside the container.

## 3. Connect from your machine

`connect.sh` reads that URI from the container logs, rewrites the container-side
bind host (`0.0.0.0`) to the reachable published one (`127.0.0.1:8765`), and launches
the client:

```sh
examples/docker-daemon/connect.sh
```

Now compare with step 1:

- `:WhoAmI` → **DAEMON config — served from the container … (tabstop=8)**
- `:set tabstop?` → `8`
- `:pwd` → `/work` (the daemon's cwd, inside the container)
- `:r !hostname` → the **container's** hostname (the process leg runs on the daemon)
- `:lua nx.notify(_G.WHERE)` → proves `require("whereami")` resolved from the
  container's `lua/` tree, fetched and materialized locally

Same client binary, same keystrokes — but the config and the filesystem are the
container's. That's the daemon split.

## How it works

- `nxvim --daemon --listen 0.0.0.0:8765` binds a QUIC listener and serves the
  fs/process/watch/LSP host plus a one-shot `config_bundle` request. No editor, no UI.
- The container sets `NXVIM_CONFIG=/etc/nxvim`, so the daemon resolves `daemon/` as
  its config (the same precedence a local launch uses: `$NXVIM_CONFIG`, then
  `$XDG_CONFIG_HOME/nxvim`, then `$HOME/.config/nxvim`).
- On connect, the client fetches that config over the wire, materializes it into a
  per-process cache, and runs it locally — Lua's synchronous `require`/runtimepath
  can't await the network, so the files must be local, but the source of truth is the
  container. (Native artifacts like `.so` parsers are **not** fetched; the client
  compiles matching ones on demand. See `examples/remote-config` for that detail.)
- Only fs/process/watch/LSP — and that config fetch — cross the wire. The keystroke
  path stays entirely local, so editing feels local even when the daemon is remote.

## Teardown

```sh
podman rm -f nxvim-daemon
# or: podman compose -f examples/docker-daemon/compose.yaml down
```
