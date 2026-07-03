#!/usr/bin/env bash
# Clone the recommended first-party plugin set + the catppuccin colorscheme at pinned
# commits and amalgamate them into ONE Lua bundle for the python demo (Phase 6 of
# docs/plans/2026-06-23-web-python-demo.md).
#
# The browser Lua VM has no filesystem/runtimepath, so a multi-file plugin can't load
# through package.path. amalgamate-plugins.mjs concatenates every plugin's lua/ tree into a
# single chunk registering package.preload[...]; sourcing it (worker.mjs, demo build) makes
# require("nxvim-line")-class resolve from memory. The demo init.lua then calls each
# plugin's setup()/load() (it also stands in for the plugin/ auto-load scripts and the
# colors/ colorscheme file, neither of which a runtimepath-less browser would source).
#
# The clones (.plugins-src) and the bundle (web/vendor/plugins) are gitignored — regenerated,
# not committed. Idempotent: re-running skips when the bundle is present; pass --force to
# rebuild. Override the clone host with NXVIM_PLUGINS_BASE (default https://github.com) — e.g.
# point it at a local mirror (`file:///path/to/repos`) for an offline build.
set -euo pipefail
cd "$(dirname "$0")"
here="$(pwd)"

BASE="${NXVIM_PLUGINS_BASE:-https://github.com}"
SRC="$here/.plugins-src"               # gitignored clones
OUT="$here/web/vendor/plugins"         # vendored bundle destination (gitignored; copied by package-site)
BUNDLE="$OUT/plugins-bundle.lua"

# repo<TAB>pinned-commit — the recommended set (first-recommended-plugin-keys-helper) plus the
# catppuccin colorscheme (the sole neovim-plugin surface). Keep pins in lock-step with the demo.
PLUGINS=(
  "nxvim/nxvim-keys-helper	6a467c80a131d5325d13cc3e60d3eff403a7e13e"
  "nxvim/nxvim-tree	217af933d92b0cbf76bb9712566ffc67aa07a203"
  "nxvim/nxvim-line	b4514e6c56ea75e956f016969d915a4da7c62f3a"
  "nxvim/nxvim-lspconfig	e9d13fff6915faecdccb425ffb0ca881c7b0fb8e"
  "nxvim/nxvim-diff	bc1d9fdebb478aee3de25ede3f0830feff556392"
  "nxvim/catppuccin-nxvim	865da97bb6cd07e6050130fdf757c944e1651d87"
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
