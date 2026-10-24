#!/usr/bin/env bash
# Clone the recommended first-party plugin set at pinned commits and amalgamate them into
# ONE Lua bundle for the python demo (Phase 6 of docs/plans/2026-06-23-web-python-demo.md).
#
# The browser Lua VM has no filesystem/runtimepath, so a multi-file plugin can't load
# through package.path. amalgamate-plugins.mjs concatenates every plugin's lua/ tree into a
# single chunk registering package.preload[...]; sourcing it (worker.mjs, demo build) makes
# require("bemtvi-line")-class resolve from memory. The demo init.lua then calls each
# plugin's setup() (it also stands in for the plugin/ auto-load scripts, which a
# runtimepath-less browser would not source). The colorscheme needs no plugin: the
# editor's own `bemtvi` scheme is built in.
#
# The clones (.plugins-src) and the bundle (web/vendor/plugins) are gitignored — regenerated,
# not committed. Idempotent: re-running skips when the bundle is present; pass --force to
# rebuild. Override the clone host with BEMTVI_PLUGINS_BASE (default https://github.com) — e.g.
# point it at a local mirror (`file:///path/to/repos`) for an offline build.
set -euo pipefail
cd "$(dirname "$0")"
here="$(pwd)"

BASE="${BEMTVI_PLUGINS_BASE:-https://github.com}"
SRC="$here/.plugins-src"               # gitignored clones
OUT="$here/web/vendor/plugins"         # vendored bundle destination (gitignored; copied by package-site)
BUNDLE="$OUT/plugins-bundle.lua"

# repo<TAB>pinned-commit — the recommended set (first-recommended-plugin-keys-helper), each at
# the commit of the release tag the core's recommended set pins (btv.plugins
# `_default_recommended`), so the demo runs the versions a fresh setup installs. Move a pin
# when its tag moves there.
PLUGINS=(
  "bemtvi/bemtvi-keys-helper	2f9e1060cea16ad0a4c15bcfebc5d15e25cb17ab"
  "bemtvi/bemtvi-tree	20338af5add9cf7e48a80358b80319b59ea5842e"
  "bemtvi/bemtvi-line	ef85139ce85619e249ddfb54337aa218fd335c17"
  "bemtvi/bemtvi-lspconfig	deafc538b71140646acc9b4bf0eaa302f669267c"
  "bemtvi/bemtvi-diff	05bb889a91e21a317efa7dcd089833215284b24d"
  "bemtvi/bemtvi-snippets	ad5eff3ed906efd6057fe9cedab1bd838424b9fd"
  "bemtvi/bemtvi-markdown-preview	f192efbd789e4a8240dbb59ef0dcd1c9e00be627"
)

force=0
[ "${1:-}" = "--force" ] && force=1

if [ "$force" = 0 ] && [ -f "$BUNDLE" ]; then
  echo "plugin bundle already vendored ($BUNDLE) — skip (pass --force to rebuild)"
  exit 0
fi

# 1. Clone each plugin and checkout its pinned commit. A full clone + checkout (not a shallow
#    --branch) so an arbitrary pinned SHA resolves even when it isn't a branch tip. Repos are
#    small; the clones are cached in .plugins-src and reused across runs.
dirs=()
for entry in "${PLUGINS[@]}"; do
  repo="${entry%%	*}"            # owner/name
  sha="${entry##*	}"
  name="${repo##*/}"
  dest="$SRC/$name"
  if [ ! -d "$dest/.git" ]; then
    echo "cloning $repo → $dest"
    rm -rf "$dest"
    git clone --quiet "$BASE/$repo.git" "$dest"
  fi
  git -C "$dest" checkout --quiet "$sha" || {
    # Pin not present in the cached clone — refetch then checkout.
    git -C "$dest" fetch --quiet origin && git -C "$dest" checkout --quiet "$sha"
  }
  got="$(git -C "$dest" rev-parse HEAD)"
  [ "$got" = "$sha" ] || { echo "error: $repo HEAD $got != pinned $sha" >&2; exit 1; }
  dirs+=("$dest")
done

# 2. Amalgamate every plugin's lua/ tree into the one preload bundle.
mkdir -p "$OUT"
node "$here/web/amalgamate-plugins.mjs" -o "$BUNDLE" "${dirs[@]}"
echo "vendored plugin bundle → $BUNDLE ($(du -h "$BUNDLE" | cut -f1), ${#dirs[@]} plugins)"
