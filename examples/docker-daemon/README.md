# Remote daemon in a container (podman / docker)

bemtvi can split itself in two: a thin **edit-host** (your keystrokes, the UI) talking
to a remote **daemon** that owns the filesystem, processes, file watches and language
servers. This example puts that daemon in a container and connects to it two ways over
the same transport — **QUIC** (so: UDP):

- a **native** TUI client running on your machine (`connect.sh`), and
- the **browser** client served from a second container (`connect-web.sh`), which
  dials the same daemon over **WebTransport**.

The point of the example is the **config swap**: a daemon session *can* run the
*daemon's* config, fetched over the wire, instead of your local one — you choose with
`--remote-config`. Native clients default to your **local** config (only I/O crosses
the wire); the **browser** client is always remote. Two deliberately different configs
make the swap visible:

| | local launch | daemon session (`--remote-config`) | daemon session (default) |
|---|---|---|---|
| config | `local/init.lua` | `daemon/init.lua` (baked into the image) | your `local/init.lua` |
| `:WhoAmI` | `LOCAL config …` | `DAEMON config …` | `LOCAL config …` |
| `:set tabstop?` | `2` | `8` | `2` |
| filesystem | your machine | the container | the container |

The `connect.sh` script below passes `--remote-config` by default so the swap is
visible; the web client (step 4) is remote either way.

> Native daemon only for now — both clients reach the same `--daemon --listen`
> listener; the daemon side is not itself containerized-anything-special.

## What's here

```
Containerfile        daemon image: release binary, bakes in daemon/ + workspace/, runs --daemon --listen
Containerfile.web    web image: a slim node server for the prebuilt wasm bundle (static files)
compose.yaml         podman-compose / docker compose front-end for both containers
connect.sh           reads the daemon's connect URI from the logs and launches the native TUI client
connect-web.sh       prints the browser URL (?daemon=…) pointing the web client at the daemon
local/init.lua       the config used when you run bemtvi normally (no daemon)
daemon/init.lua      the config the container serves over the wire
daemon/lua/whereami.lua   a require-able module, fetched too (proves the whole runtimepath crosses)
workspace/sample.txt the file you edit — it lives on the daemon (in the container)
```

## 1. See the local config first (baseline, no container)

```sh
BEMTVI_CONFIG=examples/docker-daemon/local \
  cargo run -p bemtvi -- examples/docker-daemon/workspace/sample.txt
```

`:WhoAmI` → *LOCAL config*, `:set tabstop?` → `2`. This is the embedded server: one
process, everything on this machine.

## 2. Start the daemon container

From the **repo root** (the build context is the whole workspace):

```sh
podman build -f examples/docker-daemon/Containerfile -t bemtvi-daemon .
podman run --rm -d -p 127.0.0.1:8765:8765/udp --name bemtvi-daemon bemtvi-daemon
```

or with compose:

```sh
podman compose -f examples/docker-daemon/compose.yaml up --build -d
```

Swap `podman` → `docker` throughout if that's what you have (`CONTAINER_CLI=docker`
for the connect script). **The published port must be `/udp`** — QUIC runs on UDP.

The daemon prints its connect URI to stdout at startup; you can see it with
`podman logs bemtvi-daemon`:

```
bemtvi daemon listening on 0.0.0.0:8765
  connect with: bemtvi --connect-daemon 'bemtvi://0.0.0.0:8765/<token>?cert=<hash>'
```

The `<token>` (a 32-byte bearer secret) and `<hash>` (the self-signed cert, pinned
TOFU on first use) are minted fresh on every launch — together they are the auth
gate, which is why the listener can safely bind `0.0.0.0` inside the container.

## 3. Connect from your machine

`connect.sh` reads that URI from the container logs, rewrites the container-side
bind host (`0.0.0.0`) to the reachable published one (`127.0.0.1:8765`), and launches
the client with `--remote-config` (so it runs the daemon's config):

```sh
examples/docker-daemon/connect.sh
```

Now compare with step 1:

- `:WhoAmI` → **DAEMON config — served from the container … (tabstop=8)**
- `:set tabstop?` → `8`
- `:pwd` → `/work` (the daemon's cwd, inside the container)
- `:r !hostname` → the **container's** hostname (the process leg runs on the daemon)
- `:lua btv.notify(_G.WHERE)` → proves `require("whereami")` resolved from the
  container's `lua/` tree, fetched and materialized locally

Same client binary, same keystrokes — but the config and the filesystem are the
container's. That's the daemon split with the config swap.

Drop the config swap and you keep your own config over the daemon's filesystem — the
native default:

```sh
REMOTE_CONFIG=0 examples/docker-daemon/connect.sh
```

Now `:WhoAmI` → **LOCAL config** and `:set tabstop?` → `2` (your `local/init.lua`),
but `:pwd` is still `/work` and `:r !hostname` is still the container's — only the
config came back local; the filesystem and processes stay on the daemon.

## 4. Or connect from the browser (second container)

The browser build of bemtvi is arch-independent wasm served as static files. Build the
bundle once on the host (needs `emcc` + `node`), then serve it from its own container:

```sh
crates/bemtvi-edithost/build.sh        # → crates/bemtvi-edithost/dist/ + web/vendor/

podman build -f examples/docker-daemon/Containerfile.web -t bemtvi-web crates/bemtvi-edithost
podman run --rm -d -p 127.0.0.1:8088:8088 --name bemtvi-web bemtvi-web
```

(or `podman compose … up --build -d`, which brings up both containers at once.)

Now get the page URL — `connect-web.sh` reads the daemon's URI from its container
logs, points it at the daemon's host-published address, and hangs it off the page as
`?daemon=…`:

```sh
examples/docker-daemon/connect-web.sh        # prints http://localhost:8088/web/?daemon=…
# OPEN=1 examples/docker-daemon/connect-web.sh   # also xdg-opens it
```

Open that URL in a WebTransport-capable browser (Chrome/Edge). The page boots, dials
the daemon over WebTransport, and you get the **same daemon session** as the native
client: `:WhoAmI` → DAEMON, `:e /work/sample.txt` reads the file off the container's
disk over the wire.

**Why two containers but only one connection:** the web container is a *static file
server only*. The WebTransport connection is made by your browser, running on the
host — so it dials the daemon's host-published port (`127.0.0.1:8765`), exactly like
the native client does. Container-to-container networking isn't on the data path;
that's why the `?daemon=` URI uses the host address, not a compose service name.

Notes:
- `http://localhost` is a secure context, so WebTransport is allowed without TLS on
  the page; `serve.mjs` sends the COOP/COEP/CORP headers the worker's SharedArrayBuffer
  needs.
- The daemon's self-signed cert is pinned by hash (`serverCertificateHashes`), so no CA
  is involved — the same TOFU model the native client uses.

## How it works

- `bemtvi --daemon --listen 0.0.0.0:8765` binds a QUIC listener and serves the
  fs/process/watch/LSP host plus a one-shot `config_bundle` request. No editor, no UI.
- The container sets `BEMTVI_CONFIG=/etc/bemtvi`, so the daemon resolves `daemon/` as
  its config (the same precedence a local launch uses: `$BEMTVI_CONFIG`, then
  `$XDG_CONFIG_HOME/bemtvi`, then `$HOME/.config/bemtvi`).
- With `--remote-config` (or any web session), the client fetches that config over the
  wire, materializes it into a per-process cache, and runs it locally — Lua's
  synchronous `require`/runtimepath can't await the network, so the files must be local,
  but the source of truth is the container. (Native artifacts like `.so` parsers are
  **not** fetched; the client compiles matching ones on demand. See
  `examples/remote-config` for that detail.) Without the flag, a native client runs your
  *local* config and does only a lite fetch — the daemon's cwd + parser set, no config
  files.
- Only fs/process/watch/LSP — and that config fetch — cross the wire. The keystroke
  path stays entirely local, so editing feels local even when the daemon is remote.

## Teardown

```sh
podman rm -f bemtvi-daemon bemtvi-web
# or: podman compose -f examples/docker-daemon/compose.yaml down
```
