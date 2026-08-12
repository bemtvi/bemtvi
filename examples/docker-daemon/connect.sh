#!/usr/bin/env bash
# Connect the local bemtvi editor to the containerized daemon.
#
# The daemon prints its connect URI — bemtvi://HOST:PORT/TOKEN?cert=HASH — to stdout
# at startup; the TOKEN and cert HASH are freshly minted each launch, so we read
# them from the container's logs rather than hard-coding them. The bind HOST in the
# URI is the container-side 0.0.0.0, which isn't dialable from here, so we swap in
# the published loopback host:port (the cert SANs include 127.0.0.1, and the hash is
# pinned TOFU regardless).
#
# By default this runs the DAEMON's config (--remote-config), so the config swap below
# is visible. Set REMOTE_CONFIG=0 to run your *local* config over the daemon's
# filesystem instead — a quick way to see the two modes side by side.
#
# Usage:
#     examples/docker-daemon/connect.sh [FILE-ON-DAEMON]
#
# Env:
#     CONTAINER_CLI    podman (default) or docker
#     BEMTVI_CONTAINER  container name (default: bemtvi-daemon)
#     BEMTVI_HOST       reachable host of the published port (default: 127.0.0.1)
#     BEMTVI_PORT       published UDP port (default: 8765)
#     REMOTE_CONFIG    1 (default) runs the daemon's config; 0 runs your local config
set -euo pipefail

CLI="${CONTAINER_CLI:-podman}"
NAME="${BEMTVI_CONTAINER:-bemtvi-daemon}"
HOST="${BEMTVI_HOST:-127.0.0.1}"
PORT="${BEMTVI_PORT:-8765}"
FILE="${1:-/work/sample.txt}"

# Native clients default to the LOCAL config; --remote-config opts into the daemon's.
remote_flag=()
if [ "${REMOTE_CONFIG:-1}" = "1" ]; then
  remote_flag=(--remote-config)
fi

uri="$("$CLI" logs "$NAME" 2>&1 | grep -oE "bemtvi://[^']+" | head -n1 || true)"
if [ -z "$uri" ]; then
  echo "connect.sh: no bemtvi:// URI in '$NAME' logs." >&2
  echo "  Is the daemon running?  $CLI ps   /   $CLI logs $NAME" >&2
  exit 1
fi

# Replace the container's bind host:port with the reachable published one.
uri="$(printf '%s' "$uri" | sed -E "s#bemtvi://[^/]+#bemtvi://${HOST}:${PORT}#")"

echo "connect.sh: dialing $uri" >&2
exec cargo run -p bemtvi -- "${remote_flag[@]}" "$uri" "$FILE"
