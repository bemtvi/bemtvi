#!/usr/bin/env bash
# Print (and optionally open) the browser URL that connects the web client to the
# containerized daemon.
#
# The web container only serves static files; the WebTransport connection is made by
# YOUR browser, running on the host. So the daemon URI we hand the page must point at
# the daemon's *host-published* address (127.0.0.1:8765), not a container hostname. We
# read the freshly-minted URI (token + cert hash) from the daemon container's logs,
# rewrite its bind host, URL-encode it, and hang it off the page as ?daemon=<uri>.
#
# Usage:
#     examples/docker-daemon/connect-web.sh           # print the URL
#     OPEN=1 examples/docker-daemon/connect-web.sh     # also xdg-open it
#
# Env:
#     CONTAINER_CLI    podman (default) or docker
#     BEMTVI_CONTAINER  daemon container name (default: bemtvi-daemon)
#     BEMTVI_HOST       reachable daemon host (default: 127.0.0.1)
#     BEMTVI_PORT       published daemon UDP port (default: 8765)
#     WEB_HOST         web host (default: localhost — keep it localhost: a secure context)
#     WEB_PORT         published web port (default: 8088)
set -euo pipefail

CLI="${CONTAINER_CLI:-podman}"
NAME="${BEMTVI_CONTAINER:-bemtvi-daemon}"
DHOST="${BEMTVI_HOST:-127.0.0.1}"
DPORT="${BEMTVI_PORT:-8765}"
WHOST="${WEB_HOST:-localhost}"
WPORT="${WEB_PORT:-8088}"

uri="$("$CLI" logs "$NAME" 2>&1 | grep -oE "bemtvi://[^']+" | head -n1 || true)"
if [ -z "$uri" ]; then
  echo "connect-web.sh: no bemtvi:// URI in '$NAME' logs — is the daemon running?" >&2
  exit 1
fi

# Point the browser at the host-published daemon address.
uri="$(printf '%s' "$uri" | sed -E "s#bemtvi://[^/]+#bemtvi://${DHOST}:${DPORT}#")"
# URL-encode the whole URI so it survives as a single query value (node is the web
# bundle's own toolchain, so it's a safe dependency to assume here).
enc="$(node -e 'process.stdout.write(encodeURIComponent(process.argv[1]))' "$uri")"

url="http://${WHOST}:${WPORT}/web/?daemon=${enc}"
echo "Open this in a WebTransport-capable browser (Chrome/Edge):"
echo
echo "    $url"
echo

if [ "${OPEN:-0}" = "1" ]; then
  if command -v xdg-open >/dev/null 2>&1; then
    xdg-open "$url" >/dev/null 2>&1 &
  else
    echo "(OPEN=1 set but xdg-open not found — open the URL manually)" >&2
  fi
fi
