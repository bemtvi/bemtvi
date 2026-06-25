#!/usr/bin/env bash
# Connect the local nxvim editor to the containerized daemon.
#
# The daemon prints its connect URI — nxvim://HOST:PORT/TOKEN?cert=HASH — to stdout
# at startup; the TOKEN and cert HASH are freshly minted each launch, so we read
# them from the container's logs rather than hard-coding them. The bind HOST in the
# URI is the container-side 0.0.0.0, which isn't dialable from here, so we swap in
# the published loopback host:port (the cert SANs include 127.0.0.1, and the hash is
# pinned TOFU regardless).
#
# Usage:
#     examples/docker-daemon/connect.sh [FILE-ON-DAEMON]
#
# Env:
#     CONTAINER_CLI    podman (default) or docker
#     NXVIM_CONTAINER  container name (default: nxvim-daemon)
#     NXVIM_HOST       reachable host of the published port (default: 127.0.0.1)
#     NXVIM_PORT       published UDP port (default: 8765)
set -euo pipefail

CLI="${CONTAINER_CLI:-podman}"
NAME="${NXVIM_CONTAINER:-nxvim-daemon}"
HOST="${NXVIM_HOST:-127.0.0.1}"
PORT="${NXVIM_PORT:-8765}"
FILE="${1:-/work/sample.txt}"

uri="$("$CLI" logs "$NAME" 2>&1 | grep -oE "nxvim://[^']+" | head -n1 || true)"
if [ -z "$uri" ]; then
  echo "connect.sh: no nxvim:// URI in '$NAME' logs." >&2
  echo "  Is the daemon running?  $CLI ps   /   $CLI logs $NAME" >&2
  exit 1
fi

# Replace the container's bind host:port with the reachable published one.
uri="$(printf '%s' "$uri" | sed -E "s#nxvim://[^/]+#nxvim://${HOST}:${PORT}#")"

echo "connect.sh: dialing $uri" >&2
exec cargo run -p nxvim -- "$uri" "$FILE"
